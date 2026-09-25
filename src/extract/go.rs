use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};

use super::span_of;
use crate::model::{Binding, FileFacts, Item, RawCall, Receiver};

pub fn extract(src: &str) -> Result<FileFacts> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_go::LANGUAGE.into())
        .context("loading go grammar")?;
    let tree = parser
        .parse(src, None)
        .context("tree-sitter parse failed")?;

    let root = tree.root_node();
    let mut facts = FileFacts::default();
    // Interfaces declared in THIS file, collected before the main pass so a
    // receiver typed by one is classified as dispatch even when the interface
    // is declared further down. Same-file syntax, so this is evidence rather
    // than inference; an interface from another file in the package is not
    // visible here and its receivers stay `Typed`.
    let ifaces = interfaces_in(root, src);
    // Imports first: a call's receiver can only be told apart from a package
    // qualifier by knowing which names the file imported, and every body walk
    // below needs that set.
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "import_declaration" {
            imports(child, src, &mut facts);
        }
    }
    let pkgs: BTreeSet<String> = facts.reexports.iter().map(|b| b.name.clone()).collect();
    // Return types, before any body is walked: `r := NewRouter()` needs the
    // signature of `NewRouter`, which may sit below the use.
    facts.returns = returns_in(root, src);
    let mut items = Vec::new();
    // `func (r *T) M()` is not lexically inside `T`, so methods are bucketed by
    // receiver type and emitted as one synthetic `impl T` container per type.
    // That is the shape `build_universe` indexes methods from, and it makes a
    // Go method reachable by the same `T.M` path as a Rust inherent method.
    let mut methods: BTreeMap<String, Vec<Item>> = BTreeMap::new();

    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        match child.kind() {
            "function_declaration" => {
                let it = function(child, src, "fn", &ifaces, &pkgs, None);
                if let Some(n) = &it.name {
                    facts.defined.insert(n.clone());
                }
                items.push(it);
            }
            "method_declaration" => {
                let Some((var, ty)) = receiver_of_method(child, src) else {
                    continue;
                };
                methods.entry(ty).or_default().push(function(
                    child,
                    src,
                    "fn",
                    &ifaces,
                    &pkgs,
                    Some(&var),
                ));
            }
            "type_declaration" => types(child, src, &mut items, &mut facts.defined),
            "var_declaration" | "const_declaration" => {
                values(child, src, &mut items, &mut facts.defined)
            }
            _ => {}
        }
    }

    for (ty, children) in methods {
        let line = children.iter().map(|c| c.line).min().unwrap_or(1);
        let end_line = children.iter().map(|c| c.end_line).max().unwrap_or(line);
        // Hashed over the method set, not a source span: the methods of one
        // type need not be contiguous in the file, so there is no span to hash.
        let joined: String = children
            .iter()
            .map(|c| format!("{}{}", c.signature, c.hash))
            .collect();
        items.push(Item {
            kind: "impl".to_string(),
            signature: format!("impl {ty}"),
            line,
            end_line,
            hash: crate::model::content_hash(&joined),
            doc: None,
            calls: Vec::new(),
            children,
            arity: None,
            name: Some(ty),
            raw_calls: Vec::new(),
            implements: Vec::new(),
            field_types: BTreeMap::new(),
        });
    }

    if let Some(it) = package_level_item(root, src, &ifaces, &pkgs) {
        items.insert(0, it);
    }
    facts.items = items;
    Ok(facts)
}

/// The declared result type of every callable in this file, keyed as a call
/// site writes the callee: `NewRouter` for a function, `Router.Path` for a
/// method.
///
/// Only the first result is recorded — see [`FileFacts::returns`].
fn returns_in(root: Node, src: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        let key = match child.kind() {
            "function_declaration" => match def_name(child, src) {
                Some(n) => n,
                None => continue,
            },
            "method_declaration" => {
                let (Some((_, ty)), Some(n)) =
                    (receiver_of_method(child, src), def_name(child, src))
                else {
                    continue;
                };
                format!("{ty}.{n}")
            }
            _ => continue,
        };
        if let Some(t) = first_result_type(child, src) {
            out.insert(key, t);
        }
    }
    out
}

/// The base type of a callable's first result.
///
/// `func New() *Store` yields `Store`; `func New() (*Store, error)` also yields
/// `Store`, because a result list's first entry is its first entry. A result
/// whose type names no single owner (`map[..]`, `error`, a builtin) yields None.
fn first_result_type(node: Node, src: &str) -> Option<String> {
    let result = node.child_by_field_name("result")?;
    let ty = if result.kind() == "parameter_list" {
        let mut c = result.walk();
        let first = result.named_children(&mut c).find(|n| {
            matches!(
                n.kind(),
                "parameter_declaration" | "variadic_parameter_declaration"
            )
        })?;
        collapse(text(first.child_by_field_name("type")?, src))
    } else {
        collapse(text(result, src))
    };
    let base = base_type(&ty);
    if base.is_empty() || BUILTIN_TYPES.contains(&base.as_str()) {
        return None;
    }
    Some(base)
}

/// Names of the interface types declared at the top level of this file.
fn interfaces_in(root: Node, src: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "type_declaration" {
            continue;
        }
        let mut c = child.walk();
        for spec in child.named_children(&mut c) {
            let (Some(n), Some(t)) = (
                spec.child_by_field_name("name"),
                spec.child_by_field_name("type"),
            ) else {
                continue;
            };
            if t.kind() == "interface_type" {
                out.insert(text(n, src).to_string());
            }
        }
    }
    out
}

/// Calls made in package-level `var`/`const` initializers, as a synthetic
/// unnamed item.
///
/// Go has no top-level statements, but `var db = mustOpen(dsn)` is ordinary
/// wiring and its call is real. Being unnamed it reports the module itself as
/// the caller and never pollutes `def` lookups.
fn package_level_item(
    root: Node,
    src: &str,
    ifaces: &BTreeSet<String>,
    pkgs: &BTreeSet<String>,
) -> Option<Item> {
    let mut cursor = root.walk();
    let mut raw_calls = Vec::new();
    for child in root.named_children(&mut cursor) {
        if !matches!(child.kind(), "var_declaration" | "const_declaration") {
            continue;
        }
        let (calls, _) = body_facts(child, src, &TypeEnv::default(), ifaces, pkgs);
        raw_calls.extend(calls);
    }
    if raw_calls.is_empty() {
        return None;
    }
    Some(Item {
        kind: "fn".to_string(),
        signature: "<package level>".to_string(),
        line: 1,
        end_line: src.lines().count().max(1),
        hash: crate::model::content_hash(src),
        doc: None,
        calls: Vec::new(),
        children: Vec::new(),
        arity: None,
        name: None,
        raw_calls,
        implements: Vec::new(),
        field_types: BTreeMap::new(),
    })
}

/// `import "a/b"`, `import ( x "c" ; _ "d" ; . "e" )`.
///
/// The binding name is what call sites write (`b.Func`), which is the path's
/// last segment unless renamed. It is the only reason a package-qualified call
/// can be resolved, so an import with no usable name still contributes its
/// path as a dependency.
fn imports(node: Node, src: &str, facts: &mut FileFacts) {
    let mut queue = std::collections::VecDeque::from([node]);
    while let Some(n) = queue.pop_front() {
        if n.kind() == "import_spec" {
            let Some(p) = n.child_by_field_name("path") else {
                continue;
            };
            let path = unquote(text(p, src));
            if path.is_empty() {
                continue;
            }
            facts.imports.push(path.clone());
            let alias = n.child_by_field_name("name").map(|a| text(a, src));
            let name = match alias {
                // `_ "net/http/pprof"` — imported for side effects only, so
                // nothing in this file can name it.
                Some("_") => continue,
                // `. "x"` splices the package's names into this file's scope.
                Some(".") => "*".to_string(),
                Some(a) => a.to_string(),
                None => default_pkg_name(&path),
            };
            facts.reexports.push(Binding {
                name,
                path,
                // A Go import is never re-exported: another package cannot
                // reach `a.b` by importing the file that imports it.
                public: false,
            });
            continue;
        }
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            queue.push_back(ch);
        }
    }
}

/// The name an unaliased import binds, which is what call sites write.
///
/// The path's last element, except for the two version conventions, where it is
/// not: a module at major version 2 or above carries a `/vN` suffix that is part
/// of the module path and not of the package name, and gopkg.in spells the same
/// thing `.vN`. So `github.com/go-chi/chi/v5` binds `chi` and
/// `gopkg.in/yaml.v3` binds `yaml`.
///
/// This is worth getting right rather than approximating: taking the last
/// element literally bound `v5`, so every `chi.NewRouter()` in a repo on a v2+
/// module failed to resolve — and because the name is how a qualified call finds
/// its package, that one character cost a fifth of go-chi's call graph.
///
/// A package whose `package` clause disagrees with its path in any other way
/// still has to be imported under an explicit alias to be readable, and an alias
/// is used verbatim.
fn default_pkg_name(path: &str) -> String {
    let last = path.rsplit('/').next().unwrap_or(path);
    // `yaml.v3` -> `yaml`
    if let Some((stem, ver)) = last.rsplit_once('.') {
        if is_major_version(ver) && !stem.is_empty() {
            return stem.to_string();
        }
    }
    // `.../chi/v5` -> `chi`
    if is_major_version(last) {
        let mut segs = path.rsplit('/');
        segs.next();
        if let Some(prev) = segs.next().filter(|p| !p.is_empty()) {
            return prev.to_string();
        }
    }
    last.to_string()
}

/// `v2`, `v5`, `v11` — a module major-version element. `v1` is never written as
/// a suffix, but accepting it costs nothing and rejecting it would be a special
/// case with no purpose.
fn is_major_version(s: &str) -> bool {
    s.strip_prefix('v')
        .is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
}

/// `type T struct {...}` / `type I interface {...}` / `type A = B` / `type N int`.
fn types(node: Node, src: &str, items: &mut Vec<Item>, defined: &mut BTreeSet<String>) {
    let mut cursor = node.walk();
    for spec in node.named_children(&mut cursor) {
        if !matches!(spec.kind(), "type_spec" | "type_alias") {
            continue;
        }
        let Some(n) = spec.child_by_field_name("name") else {
            continue;
        };
        let name = text(n, src).to_string();
        defined.insert(name.clone());
        let ty = spec.child_by_field_name("type");
        let kind = match ty.map(|t| t.kind()) {
            Some("struct_type") => "struct",
            Some("interface_type") => "interface",
            _ => "type",
        };

        let mut field_types = BTreeMap::new();
        let mut implements = Vec::new();
        let mut children = Vec::new();
        let sig = match (kind, ty) {
            ("struct", Some(t)) => {
                let (fields, embeds) = struct_fields(t, src);
                // Embedding promotes the embedded type's methods onto this one,
                // so an embedder is a dispatch target for the embedded type.
                implements = embeds;
                let rendered: Vec<String> = fields
                    .iter()
                    .map(|(n, t)| format!("{n} {t}"))
                    .chain(implements.iter().cloned())
                    .collect();
                field_types = fields;
                if rendered.is_empty() {
                    format!("type {name} struct {{}}")
                } else {
                    format!("type {name} struct {{ {} }}", rendered.join("; "))
                }
            }
            ("interface", Some(t)) => {
                let (methods, embeds) = interface_members(t, src);
                implements = embeds;
                let names: Vec<String> = methods
                    .iter()
                    .filter_map(|m| m.name.clone())
                    .chain(implements.iter().cloned())
                    .collect();
                children = methods;
                format!("type {name} interface {{ {} }}", names.join("; "))
            }
            // `type Celsius float64`, `type Alias = Other`: the declaration is
            // short enough to be its own signature.
            _ => {
                let raw = collapse(text(spec, src));
                if raw.starts_with("type") {
                    raw
                } else {
                    format!("type {raw}")
                }
            }
        };

        let mut it = item(kind, clip(&sig), spec, src, children, Some(name));
        it.field_types = field_types;
        it.implements = implements;
        items.push(it);
    }
}

/// A struct's named fields (for `x.field.M()` resolution) and its embedded
/// types (which promote their methods onto it).
fn struct_fields(t: Node, src: &str) -> (BTreeMap<String, String>, Vec<String>) {
    let mut fields = BTreeMap::new();
    let mut embeds = Vec::new();
    let Some(list) = t
        .named_children(&mut t.walk())
        .find(|c| c.kind() == "field_declaration_list")
    else {
        return (fields, embeds);
    };
    let mut c = list.walk();
    for d in list.named_children(&mut c) {
        if d.kind() != "field_declaration" {
            continue;
        }
        let Some(ty) = d.child_by_field_name("type") else {
            // No `type` field means an embedded field: the type IS the name.
            let raw = collapse(text(d, src));
            let base = base_type(&raw);
            if !base.is_empty() {
                embeds.push(base.clone());
                fields.insert(base.clone(), raw);
            }
            continue;
        };
        let names: Vec<Node> = d.children_by_field_name("name", &mut d.walk()).collect();
        if names.is_empty() {
            let raw = collapse(text(ty, src));
            let base = base_type(&raw);
            if !base.is_empty() {
                embeds.push(base.clone());
                fields.insert(base, raw);
            }
            continue;
        }
        for n in names {
            fields.insert(text(n, src).to_string(), collapse(text(ty, src)));
        }
    }
    (fields, embeds)
}

/// An interface's declared methods, plus the interfaces it embeds.
fn interface_members(t: Node, src: &str) -> (Vec<Item>, Vec<String>) {
    let mut methods = Vec::new();
    let mut embeds = Vec::new();
    let mut c = t.walk();
    for e in t.named_children(&mut c) {
        match e.kind() {
            "method_elem" => {
                let name = e
                    .child_by_field_name("name")
                    .map(|n| text(n, src).to_string());
                let mut it = item(
                    "fn",
                    clip(&collapse(text(e, src))),
                    e,
                    src,
                    Vec::new(),
                    name,
                );
                it.arity = arity(e);
                methods.push(it);
            }
            // `interface { Reader; Writer }` and constraint elements.
            "type_elem" => {
                let base = base_type(&collapse(text(e, src)));
                if !base.is_empty() && base.chars().next().is_some_and(char::is_alphabetic) {
                    embeds.push(base);
                }
            }
            _ => {}
        }
    }
    (methods, embeds)
}

/// Package-level `var`/`const` declarations, one item per declared name.
fn values(node: Node, src: &str, items: &mut Vec<Item>, defined: &mut BTreeSet<String>) {
    let kind = if node.kind() == "const_declaration" {
        "const"
    } else {
        "var"
    };
    let mut cursor = node.walk();
    for spec in node.named_children(&mut cursor) {
        if !matches!(spec.kind(), "var_spec" | "const_spec") {
            continue;
        }
        let sig = collapse(text(spec, src));
        for n in spec.children_by_field_name("name", &mut spec.walk()) {
            let name = text(n, src).to_string();
            if name == "_" {
                continue;
            }
            defined.insert(name.clone());
            items.push(item(
                kind,
                clip(&format!("{kind} {sig}")),
                spec,
                src,
                Vec::new(),
                Some(name),
            ));
        }
    }
}

/// `func (r *T) M(...)` -> (receiver variable, receiver type).
fn receiver_of_method(node: Node, src: &str) -> Option<(String, String)> {
    let recv = node.child_by_field_name("receiver")?;
    let mut c = recv.walk();
    let decl = recv
        .named_children(&mut c)
        .find(|n| n.kind() == "parameter_declaration")?;
    let ty = base_type(&collapse(text(decl.child_by_field_name("type")?, src)));
    if ty.is_empty() {
        return None;
    }
    // An anonymous receiver (`func (T) M()`) still names its type.
    let var = decl
        .child_by_field_name("name")
        .map(|n| text(n, src).to_string())
        .unwrap_or_default();
    Some((var, ty))
}

/// Receiver types ctx can name inside one body: declared parameters, the
/// method receiver, and `:=` / `var` bindings whose type the source states.
#[derive(Default, Clone)]
struct TypeEnv {
    vars: HashMap<String, Receiver>,
}

/// Turn a declared Go type into the receiver kind it implies, or None when the
/// type says nothing about which method body runs (`map[string]int`, `[]byte`,
/// `int`, a channel).
fn classify_type(raw: &str, ifaces: &BTreeSet<String>) -> Option<Receiver> {
    let base = base_type(raw);
    if base.is_empty() {
        return None;
    }
    // A builtin cannot own a method defined in this tree.
    if BUILTIN_TYPES.contains(&base.as_str()) {
        return None;
    }
    if ifaces.contains(&base) {
        return Some(Receiver::Dyn(base));
    }
    Some(Receiver::Typed(base))
}

const BUILTIN_TYPES: &[&str] = &[
    "bool",
    "string",
    "int",
    "int8",
    "int16",
    "int32",
    "int64",
    "uint",
    "uint8",
    "uint16",
    "uint32",
    "uint64",
    "uintptr",
    "byte",
    "rune",
    "float32",
    "float64",
    "complex64",
    "complex128",
    "error",
    "any",
];

/// The bare type name a Go type expression points at: pointers, slices,
/// arrays, variadics, channels and generic arguments come off; a composite
/// with no single owner (map, func, chan of composite) yields "".
fn base_type(raw: &str) -> String {
    let mut t = collapse(raw).trim().to_string();
    loop {
        let before = t.clone();
        t = t.trim().to_string();
        for p in ["*", "&", "...", "[]", "<-chan ", "chan<- ", "chan ", "..."] {
            if let Some(rest) = t.strip_prefix(p) {
                t = rest.trim().to_string();
            }
        }
        // `[N]T`
        if t.starts_with('[') {
            if let Some(i) = t.find(']') {
                t = t[i + 1..].trim().to_string();
            }
        }
        if t == before {
            break;
        }
    }
    // Composites own no methods reachable by a bare name.
    if t.starts_with("map[")
        || t.starts_with("func(")
        || t.starts_with("interface{")
        || t.starts_with("struct{")
    {
        return String::new();
    }
    // `Pair[K, V]` -> `Pair`
    if let Some(i) = t.find('[') {
        t = t[..i].to_string();
    }
    // `pkg.Type` -> `Type`: the bare name is what the method index is keyed on,
    // and a type from another package resolves or not on its own evidence.
    let t = t.rsplit('.').next().unwrap_or(&t).trim().to_string();
    if t.is_empty()
        || !t
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
    {
        return String::new();
    }
    t
}

/// The environment a function body starts with: its parameters, results and
/// (for a method) its receiver.
fn fn_env(node: Node, src: &str, ifaces: &BTreeSet<String>, recv: Option<&str>) -> TypeEnv {
    let mut env = TypeEnv::default();
    if let Some(r) = recv {
        if !r.is_empty() && r != "_" {
            // The receiver variable is Go's `self`: the enclosing container is
            // the right owner, with no inference involved.
            env.vars.insert(r.to_string(), Receiver::SelfType);
        }
    }
    for (name, r) in param_types(node, src, ifaces) {
        env.vars.insert(name, r);
    }
    env
}

/// Every named parameter and result of a callable, with the receiver kind its
/// declared type implies. Named results count: `func f() (s *Store, err error)`
/// makes `s` usable in the body.
fn param_types(node: Node, src: &str, ifaces: &BTreeSet<String>) -> Vec<(String, Receiver)> {
    let mut out = Vec::new();
    for field in ["parameters", "result"] {
        let Some(params) = node.child_by_field_name(field) else {
            continue;
        };
        if params.kind() != "parameter_list" {
            continue;
        }
        let mut c = params.walk();
        for p in params.named_children(&mut c) {
            if !matches!(
                p.kind(),
                "parameter_declaration" | "variadic_parameter_declaration"
            ) {
                continue;
            }
            let Some(ty) = p.child_by_field_name("type") else {
                continue;
            };
            let Some(r) = classify_type(text(ty, src), ifaces) else {
                continue;
            };
            for n in p.children_by_field_name("name", &mut p.walk()) {
                let name = text(n, src).trim().to_string();
                if !name.is_empty() && name != "_" {
                    out.push((name, r.clone()));
                }
            }
        }
    }
    out
}

fn function(
    node: Node,
    src: &str,
    kind: &str,
    ifaces: &BTreeSet<String>,
    pkgs: &BTreeSet<String>,
    recv: Option<&str>,
) -> Item {
    let env = fn_env(node, src, ifaces, recv);
    let (sig, raw_calls) = match node.child_by_field_name("body") {
        Some(body) => {
            let (calls, _) = body_facts(body, src, &env, ifaces, pkgs);
            (head_before(node, body, src), calls)
        }
        None => (collapse(text(node, src)), Vec::new()),
    };
    let mut it = item(kind, clip(&sig), node, src, Vec::new(), def_name(node, src));
    it.raw_calls = raw_calls;
    it.arity = arity(node);
    it
}

/// Value-parameter count, receiver excluded. A grouped declaration
/// (`func f(a, b int)`) declares one node per group and one `name` per
/// parameter, so names are counted rather than nodes.
fn arity(node: Node) -> Option<usize> {
    let params = node.child_by_field_name("parameters")?;
    let mut cursor = params.walk();
    let mut n = 0usize;
    for p in params.named_children(&mut cursor) {
        if !matches!(
            p.kind(),
            "parameter_declaration" | "variadic_parameter_declaration"
        ) {
            continue;
        }
        // An unnamed parameter (`func(int, string)`) is still a parameter.
        n += p
            .children_by_field_name("name", &mut p.walk())
            .count()
            .max(1);
    }
    Some(n)
}

/// Walk a function body for its call sites, typing receivers wherever a
/// parameter, a receiver or a `:=` / `var` binding states what they are.
///
/// Calls inside a `func` literal are attributed to the enclosing function: a
/// goroutine body or a handler closure has no name of its own to hang them on,
/// and the enclosing function is what a reader is looking for.
fn body_facts(
    body: Node,
    src: &str,
    env: &TypeEnv,
    ifaces: &BTreeSet<String>,
    pkgs: &BTreeSet<String>,
) -> (Vec<RawCall>, Vec<Item>) {
    // Pass 1: bindings. Call sites are noted but not classified yet, so a
    // variable declared below a call still types it.
    let mut env = env.clone();
    let mut call_nodes: Vec<Node> = Vec::new();
    let mut literals: Vec<Node> = Vec::new();
    let mut stack = vec![body];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "short_var_declaration" => {
                for (name, r) in short_var_bindings(n, src, &env, ifaces) {
                    env.vars.insert(name, r);
                }
            }
            "var_declaration" | "const_declaration" => {
                for (name, r) in var_bindings(n, src, &env, ifaces) {
                    env.vars.insert(name, r);
                }
            }
            // A `func` literal is its own scope, walked after this one so it
            // inherits the finished environment and can shadow it. Flattening the
            // two was actively wrong, not merely imprecise: `r.Get("/", func(w
            // http.ResponseWriter, r *http.Request){..})` rebinds `r`, so every
            // call on the OUTER `r` was being attributed to the parameter type.
            "func_literal" => {
                literals.push(n);
                continue;
            }
            "call_expression" => call_nodes.push(n),
            _ => {}
        }
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            stack.push(ch);
        }
    }

    // Pass 2: classify receivers against the completed environment.
    let mut out = Vec::new();
    for n in call_nodes {
        if let Some(f) = n.child_by_field_name("function") {
            push_callee(f, src, &env, ifaces, pkgs, span_of(n), &mut out);
        }
    }

    // Pass 3: nested scopes, each starting from the completed outer environment
    // so a literal declared above a binding still sees it. Their calls are
    // attributed to the enclosing function, which is the name a reader has.
    for lit in literals {
        let mut inner = env.clone();
        for (name, r) in param_types(lit, src, ifaces) {
            inner.vars.insert(name, r);
        }
        if let Some(b) = lit.child_by_field_name("body") {
            let (calls, _) = body_facts(b, src, &inner, ifaces, pkgs);
            out.extend(calls);
        }
    }

    // One deterministic order for calls gathered from several scopes.
    out.sort_by_key(|c| (c.line, c.end_line));
    (out, Vec::new())
}

/// `x := Foo{}` / `x := NewFoo()` / `x, err := pkg.Open(p)`.
fn short_var_bindings(
    n: Node,
    src: &str,
    env: &TypeEnv,
    ifaces: &BTreeSet<String>,
) -> Vec<(String, Receiver)> {
    let (Some(left), Some(right)) = (
        n.child_by_field_name("left"),
        n.child_by_field_name("right"),
    ) else {
        return Vec::new();
    };
    let lefts: Vec<Node> = left.named_children(&mut left.walk()).collect();
    let rights: Vec<Node> = right.named_children(&mut right.walk()).collect();
    let mut out = Vec::new();
    // `x, err := New()` — one expression spread over several names. Only the
    // first name is bound, from the callee's first result: position 0 is
    // position 0, so this is the result list being read, not a guess about
    // which result matters. The remaining names stay untyped.
    if rights.len() == 1 && lefts.len() > 1 {
        let name = text(lefts[0], src).trim().to_string();
        if name.is_empty() || name == "_" || name.contains(['.', '[']) {
            return out;
        }
        if let Some(recv) = value_type(rights[0], src, env, ifaces) {
            out.push((name, recv));
        }
        return out;
    }
    if lefts.len() != rights.len() {
        return out;
    }
    for (l, r) in lefts.iter().zip(rights.iter()) {
        let name = text(*l, src).trim().to_string();
        if name.is_empty() || name == "_" || name.contains(['.', '[']) {
            continue;
        }
        if let Some(recv) = value_type(*r, src, env, ifaces) {
            out.push((name, recv));
        }
    }
    out
}

/// `var x T` / `var x = Foo{}` inside a body.
fn var_bindings(
    n: Node,
    src: &str,
    env: &TypeEnv,
    ifaces: &BTreeSet<String>,
) -> Vec<(String, Receiver)> {
    let mut out = Vec::new();
    let mut c = n.walk();
    for spec in n.named_children(&mut c) {
        if !matches!(spec.kind(), "var_spec" | "const_spec") {
            continue;
        }
        let names: Vec<String> = spec
            .children_by_field_name("name", &mut spec.walk())
            .map(|x| text(x, src).trim().to_string())
            .filter(|s| !s.is_empty() && s != "_")
            .collect();
        let declared = spec
            .child_by_field_name("type")
            .and_then(|t| classify_type(text(t, src), ifaces));
        let inferred = spec
            .child_by_field_name("value")
            .and_then(|v| v.named_children(&mut v.walk()).next())
            .and_then(|v| value_type(v, src, env, ifaces));
        let Some(r) = declared.or(inferred) else {
            continue;
        };
        // `var a, b T` gives both names the same declared type; an inferred
        // one only applies when a single name was declared.
        if names.len() == 1 || spec.child_by_field_name("type").is_some() {
            for name in names {
                out.push((name, r.clone()));
            }
        }
    }
    out
}

/// The type an initializer expression names, where the source states it.
fn value_type(v: Node, src: &str, env: &TypeEnv, ifaces: &BTreeSet<String>) -> Option<Receiver> {
    match v.kind() {
        // `Config{...}` / `pkg.Config{...}` / `&Config{...}`
        "composite_literal" => classify_type(text(v.child_by_field_name("type")?, src), ifaces),
        "unary_expression" | "parenthesized_expression" => {
            let inner = v.named_children(&mut v.walk()).next()?;
            value_type(inner, src, env, ifaces)
        }
        "call_expression" => {
            // `new(T)` returns `*T` by the language spec, and T is written right
            // here — so this is a declared type, not a return-type lookup.
            if let Some(t) = new_arg_type(v, src, ifaces) {
                return Some(t);
            }
            // `NewEngine()` / `pkg.New()` / `r.Sub()` — the type is declared on
            // the callee, which may be in another file or another package, so
            // the call site records the callee and resolution reads the return
            // type off it.
            callee_key(v, src, env, ifaces).map(Receiver::Returned)
        }
        "identifier" => env.vars.get(text(v, src)).cloned(),
        _ => None,
    }
}

/// The `returns` key for what a call expression calls, when the call site names
/// it unambiguously: `NewRouter`, `mux.NewRouter`, or `Router.Path` when the
/// receiver's own type is already known.
fn callee_key(v: Node, src: &str, env: &TypeEnv, ifaces: &BTreeSet<String>) -> Option<String> {
    let f = v.child_by_field_name("function")?;
    match f.kind() {
        "identifier" => Some(text(f, src).to_string()),
        "selector_expression" => {
            let operand = f.child_by_field_name("operand")?;
            let field = text(f.child_by_field_name("field")?, src);
            match operand.kind() {
                "identifier" => {
                    let recv = text(operand, src);
                    match env.vars.get(recv) {
                        // A receiver whose type the source states.
                        Some(Receiver::Typed(t)) | Some(Receiver::Dyn(t)) => {
                            Some(format!("{t}.{field}"))
                        }
                        // Already a chain head: keep building on it.
                        Some(Receiver::Returned(k)) => Some(format!("{k}.{field}")),
                        // A package qualifier; the import binding sorts it out.
                        None => Some(format!("{recv}.{field}")),
                        _ => None,
                    }
                }
                // A method on the result of another call —
                // `new(Route).Host("x").Path("/a")`. Each link is appended, and
                // resolution walks the chain left to right. Builder APIs are
                // written this way and are unreadable to a one-link rule.
                "call_expression" => {
                    if let Some(Receiver::Typed(t)) | Some(Receiver::Dyn(t)) =
                        new_arg_type(operand, src, ifaces)
                    {
                        return Some(format!("{t}.{field}"));
                    }
                    let inner = callee_key(operand, src, env, ifaces)?;
                    Some(format!("{inner}.{field}"))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// The receiver kind implied by `new(T)`, whose result is `*T`.
fn new_arg_type(v: Node, src: &str, ifaces: &BTreeSet<String>) -> Option<Receiver> {
    let f = v.child_by_field_name("function")?;
    if f.kind() != "identifier" || text(f, src) != "new" {
        return None;
    }
    let args = v.child_by_field_name("arguments")?;
    let arg = args.named_children(&mut args.walk()).next()?;
    classify_type(text(arg, src), ifaces)
}

fn push_callee(
    f: Node,
    src: &str,
    env: &TypeEnv,
    ifaces: &BTreeSet<String>,
    pkgs: &BTreeSet<String>,
    at: (usize, usize),
    out: &mut Vec<RawCall>,
) {
    let push = |path: String, recv: Receiver, out: &mut Vec<RawCall>| {
        if path.is_empty() {
            return;
        }
        out.push(RawCall {
            path,
            recv,
            line: at.0,
            end_line: at.1,
        });
    };
    match f.kind() {
        "identifier" => push(text(f, src).to_string(), Receiver::Free, out),
        // `f[int](x)` — generic instantiation; the callee is the operand.
        "index_expression" => {
            if let Some(inner) = f.child_by_field_name("operand") {
                push_callee(inner, src, env, ifaces, pkgs, at, out);
            }
        }
        "parenthesized_expression" => {
            if let Some(inner) = f.named_children(&mut f.walk()).next() {
                push_callee(inner, src, env, ifaces, pkgs, at, out);
            }
        }
        "selector_expression" => {
            let (Some(operand), Some(field)) = (
                f.child_by_field_name("operand"),
                f.child_by_field_name("field"),
            ) else {
                return;
            };
            let name = text(field, src).to_string();
            match operand.kind() {
                "identifier" => {
                    let recv = text(operand, src);
                    match env.vars.get(recv) {
                        // The method receiver: `s.helper()` inside a method of S.
                        Some(Receiver::SelfType) => push(name, Receiver::SelfType, out),
                        Some(r) => push(name, r.clone(), out),
                        // An imported package: `http.ListenAndServe()`. Keep it
                        // qualified so resolution can walk the import binding.
                        None if pkgs.contains(recv) => {
                            push(format!("{recv}.{name}"), Receiver::Free, out)
                        }
                        // A variable whose type the source never states. Calling
                        // it a qualified path would make it look provably
                        // external; it is an opaque receiver, and saying so is
                        // what earns the `~`.
                        None => push(name, Receiver::Unknown, out),
                    }
                }
                // `s.store.Get()` — a field of the receiver, resolved against
                // the enclosing type's declared field types.
                "selector_expression" => {
                    let (Some(base), Some(fld)) = (
                        operand.child_by_field_name("operand"),
                        operand.child_by_field_name("field"),
                    ) else {
                        return push(name, Receiver::Unknown, out);
                    };
                    if base.kind() == "identifier"
                        && matches!(env.vars.get(text(base, src)), Some(Receiver::SelfType))
                    {
                        push(name, Receiver::SelfField(text(fld, src).to_string()), out)
                    } else {
                        push(name, Receiver::Unknown, out)
                    }
                }
                // `new(Route).Path(..)` / `NewRouter().Use(..)` — the receiver is
                // itself a call, so the same rules apply to it.
                "call_expression" => {
                    let recv = new_arg_type(operand, src, ifaces)
                        .or_else(|| callee_key(operand, src, env, ifaces).map(Receiver::Returned))
                        .unwrap_or(Receiver::Unknown);
                    push(name, recv, out)
                }
                _ => push(name, Receiver::Unknown, out),
            }
        }
        _ => {}
    }
}

fn item(
    kind: &str,
    signature: String,
    node: Node,
    src: &str,
    children: Vec<Item>,
    name: Option<String>,
) -> Item {
    Item {
        kind: kind.to_string(),
        signature,
        line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        hash: crate::model::content_hash(&src[node.start_byte()..node.end_byte()]),
        doc: doc_comment(node, src),
        calls: Vec::new(),
        children,
        arity: None,
        name,
        raw_calls: Vec::new(),
        implements: Vec::new(),
        field_types: BTreeMap::new(),
    }
}

/// First line of the `//` comment block immediately above an item.
///
/// Unlike Rust, Go has no separate doc-comment syntax — a plain `//` run
/// touching the declaration *is* the doc, which is why this accepts what the
/// Rust extractor deliberately rejects. A blank line ends the block.
fn doc_comment(node: Node, src: &str) -> Option<String> {
    let mut sib = node.prev_sibling();
    let mut first: Option<String> = None;
    let mut last_start = node.start_position().row;
    while let Some(s) = sib {
        if s.kind() != "comment" {
            break;
        }
        let end = s.end_position().row;
        // A blank line between the comment and what follows detaches it.
        if last_start.saturating_sub(end) > 1 {
            break;
        }
        let t = text(s, src);
        let body = t.strip_prefix("//").map(|r| r.trim()).or_else(|| {
            t.strip_prefix("/*")
                .and_then(|r| r.strip_suffix("*/"))
                .map(|r| r.trim())
        })?;
        if !body.is_empty() {
            first = Some(body.to_string());
        }
        last_start = s.start_position().row;
        sib = s.prev_sibling();
    }
    first.map(|s| clip_doc(&s))
}

fn def_name(node: Node, src: &str) -> Option<String> {
    node.child_by_field_name("name")
        .map(|n| text(n, src).to_string())
}

/// The declaration head: everything from the item's start up to its body.
fn head_before(node: Node, body: Node, src: &str) -> String {
    collapse(&src[node.start_byte()..body.start_byte()])
        .trim_end_matches('{')
        .trim()
        .to_string()
}

fn unquote(s: &str) -> String {
    s.trim()
        .trim_start_matches(['"', '`'])
        .trim_end_matches(['"', '`'])
        .to_string()
}

fn text<'a>(node: Node, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(s: &str) -> String {
    if s.chars().count() > 200 {
        let mut out: String = s.chars().take(197).collect();
        out.push('…');
        out
    } else {
        s.to_string()
    }
}

fn clip_doc(s: &str) -> String {
    if s.chars().count() > 100 {
        let mut out: String = s.chars().take(97).collect();
        out.push('…');
        out
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(src: &str) -> FileFacts {
        extract(src).unwrap()
    }

    fn find<'a>(items: &'a [Item], name: &str) -> Option<&'a Item> {
        for it in items {
            if it.name.as_deref() == Some(name) {
                return Some(it);
            }
            if let Some(f) = find(&it.children, name) {
                return Some(f);
            }
        }
        None
    }

    #[test]
    fn functions_types_and_values_are_defined_names() {
        let f = facts(
            "package p\n\
             func Run(a int, b string) error { return nil }\n\
             type Engine struct { name string }\n\
             type Runner interface { Step() error }\n\
             const Limit = 10\n\
             var Global = 1\n",
        );
        for n in ["Run", "Engine", "Runner", "Limit", "Global"] {
            assert!(f.defined.contains(n), "missing {n}");
        }
        assert_eq!(find(&f.items, "Run").unwrap().arity, Some(2));
        assert_eq!(find(&f.items, "Engine").unwrap().kind, "struct");
        assert_eq!(find(&f.items, "Runner").unwrap().kind, "interface");
    }

    #[test]
    fn methods_become_one_impl_container_per_receiver_type() {
        let f = facts(
            "package p\n\
             type E struct{}\n\
             func (e *E) Start() {}\n\
             func (e E) Stop() {}\n\
             func (o *Other) Go() {}\n",
        );
        let impls: Vec<&Item> = f.items.iter().filter(|i| i.kind == "impl").collect();
        assert_eq!(impls.len(), 2, "one impl per receiver type");
        let ei = impls
            .iter()
            .find(|i| i.name.as_deref() == Some("E"))
            .unwrap();
        let names: Vec<&str> = ei
            .children
            .iter()
            .filter_map(|c| c.name.as_deref())
            .collect();
        assert_eq!(names, vec!["Start", "Stop"]);
        // A method is not importable by its bare name.
        assert!(!f.defined.contains("Start"));
    }

    #[test]
    fn imports_bind_the_name_call_sites_write() {
        let f = facts(
            "package p\n\
             import (\n\
               \"net/http\"\n\
               sq \"database/sql\"\n\
               _ \"github.com/lib/pq\"\n\
             )\n",
        );
        assert_eq!(
            f.imports,
            vec!["net/http", "database/sql", "github.com/lib/pq"]
        );
        let names: Vec<(&str, &str)> = f
            .reexports
            .iter()
            .map(|b| (b.name.as_str(), b.path.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![("http", "net/http"), ("sq", "database/sql")],
            "a blank import binds no name"
        );
    }

    #[test]
    fn a_versioned_module_path_binds_the_package_not_the_version() {
        assert_eq!(default_pkg_name("github.com/go-chi/chi/v5"), "chi");
        assert_eq!(default_pkg_name("gopkg.in/yaml.v3"), "yaml");
        assert_eq!(default_pkg_name("net/http"), "http");
        assert_eq!(default_pkg_name("fmt"), "fmt");
        assert_eq!(default_pkg_name("example.com/x/v2/inner"), "inner");
        // Not a version element, so it stays as written.
        assert_eq!(default_pkg_name("example.com/vendor"), "vendor");

        let f = facts(
            "package p\n\nimport \"github.com/go-chi/chi/v5\"\n\n             func f() { chi.NewRouter() }\n",
        );
        assert_eq!(f.reexports[0].name, "chi");
        assert_eq!(f.reexports[0].path, "github.com/go-chi/chi/v5");
        let c = &find(&f.items, "f").unwrap().raw_calls[0];
        assert_eq!(c.path, "chi.NewRouter");
    }

    #[test]
    fn receiver_calls_are_self_typed() {
        let f = facts(
            "package p\n\
             type E struct{}\n\
             func (e *E) Start() { e.boot() }\n\
             func (e *E) boot() {}\n",
        );
        let start = find(&f.items, "Start").unwrap();
        let c = &start.raw_calls[0];
        assert_eq!(c.path, "boot");
        assert_eq!(c.recv, Receiver::SelfType);
    }

    #[test]
    fn a_declared_parameter_types_its_receiver() {
        let f = facts(
            "package p\n\
             type E struct{}\n\
             func drive(e *E) { e.Start() }\n",
        );
        let c = &find(&f.items, "drive").unwrap().raw_calls[0];
        assert_eq!(c.path, "Start");
        assert_eq!(c.recv, Receiver::Typed("E".to_string()));
    }

    #[test]
    fn an_interface_declared_here_dispatches() {
        let f = facts(
            "package p\n\
             type Runner interface { Step() error }\n\
             func drive(r Runner) { r.Step() }\n",
        );
        let c = &find(&f.items, "drive").unwrap().raw_calls[0];
        assert_eq!(c.recv, Receiver::Dyn("Runner".to_string()));
    }

    #[test]
    fn composite_literal_binding_types_later_calls() {
        let f = facts(
            "package p\n\
             type E struct{}\n\
             func run() { e := &E{}; e.Start() }\n",
        );
        let c = &find(&f.items, "run").unwrap().raw_calls[0];
        assert_eq!(c.recv, Receiver::Typed("E".to_string()));
    }

    #[test]
    fn a_builder_chain_accumulates_a_resolvable_key() {
        let f = facts(
            "package p\n\n             type Route struct{}\n\n             func (r *Route) Host(h string) *Route { return r }\n\n             func run() { new(Route).Host(\"h\").Path(\"/a\") }\n",
        );
        let calls = &find(&f.items, "run").unwrap().raw_calls;
        let c = calls
            .iter()
            .find(|c| c.path == "Path")
            .expect("the outer link of the chain");
        assert_eq!(
            c.recv,
            Receiver::Returned("Route.Host".to_string()),
            "each link of the chain is appended so resolution can walk it"
        );
        // And the inner link still names its own receiver type directly.
        let inner = calls.iter().find(|c| c.path == "Host").unwrap();
        assert_eq!(inner.recv, Receiver::Typed("Route".to_string()));
    }

    #[test]
    fn new_names_the_type_it_allocates() {
        let f = facts("package p\ntype R struct{}\nfunc f() { new(R).Go() }\n");
        let c = &find(&f.items, "f").unwrap().raw_calls[0];
        assert_eq!(c.recv, Receiver::Typed("R".to_string()));
    }

    #[test]
    fn a_package_qualified_call_stays_qualified() {
        let f = facts(
            "package p\n\
             import \"net/http\"\n\
             func serve() { http.ListenAndServe(\"\", nil) }\n",
        );
        let c = &find(&f.items, "serve").unwrap().raw_calls[0];
        assert_eq!(c.path, "http.ListenAndServe");
        assert_eq!(c.recv, Receiver::Free);
    }

    #[test]
    fn a_receiver_field_call_names_the_field() {
        let f = facts(
            "package p\n\
             type S struct { store *Store }\n\
             func (s *S) Get() { s.store.Fetch() }\n",
        );
        let c = &find(&f.items, "Get").unwrap().raw_calls[0];
        assert_eq!(c.path, "Fetch");
        assert_eq!(c.recv, Receiver::SelfField("store".to_string()));
    }

    #[test]
    fn struct_fields_and_embeds_are_recorded() {
        let f = facts(
            "package p\n\
             type S struct {\n\
               sync.Mutex\n\
               name string\n\
               size, cap int\n\
             }\n",
        );
        let s = find(&f.items, "S").unwrap();
        assert_eq!(
            s.field_types.get("name").map(String::as_str),
            Some("string")
        );
        assert_eq!(s.field_types.get("size").map(String::as_str), Some("int"));
        assert_eq!(s.field_types.get("cap").map(String::as_str), Some("int"));
        assert_eq!(s.implements, vec!["Mutex".to_string()]);
    }

    #[test]
    fn interface_embedding_is_an_implements_edge() {
        let f = facts("package p\ntype RW interface { Reader; Writer }\n");
        let rw = find(&f.items, "RW").unwrap();
        assert_eq!(
            rw.implements,
            vec!["Reader".to_string(), "Writer".to_string()]
        );
    }

    #[test]
    fn doc_comment_is_the_line_run_above() {
        let f = facts(
            "package p\n\
             // Run drives the engine.\n\
             // More detail here.\n\
             func Run() {}\n",
        );
        assert_eq!(
            find(&f.items, "Run").unwrap().doc.as_deref(),
            Some("Run drives the engine.")
        );
    }

    #[test]
    fn a_detached_comment_is_not_a_doc() {
        let f = facts("package p\n// unrelated\n\nfunc Run() {}\n");
        assert_eq!(find(&f.items, "Run").unwrap().doc, None);
    }

    #[test]
    fn package_level_initializer_calls_are_attributed_to_the_module() {
        let f = facts("package p\nvar db = mustOpen(\"dsn\")\n");
        let ml = &f.items[0];
        assert_eq!(ml.signature, "<package level>");
        assert!(ml.name.is_none());
        assert_eq!(ml.raw_calls[0].path, "mustOpen");
    }

    #[test]
    fn calls_in_a_func_literal_belong_to_the_enclosing_function() {
        let f = facts(
            "package p\n\
             func run() { go func() { work() }() }\n",
        );
        let paths: Vec<&str> = find(&f.items, "run")
            .unwrap()
            .raw_calls
            .iter()
            .map(|c| c.path.as_str())
            .collect();
        assert!(paths.contains(&"work"), "got {paths:?}");
    }

    #[test]
    fn builtin_typed_receivers_are_not_attributed() {
        let f = facts("package p\nfunc f(s string) { s.Trim() }\n");
        let c = &find(&f.items, "f").unwrap().raw_calls[0];
        assert_eq!(c.recv, Receiver::Unknown, "a string owns no local method");
    }

    #[test]
    fn base_type_strips_wrappers_and_rejects_composites() {
        assert_eq!(base_type("*Engine"), "Engine");
        assert_eq!(base_type("[]*pkg.Engine"), "Engine");
        assert_eq!(base_type("...Opt"), "Opt");
        assert_eq!(base_type("Pair[K, V]"), "Pair");
        assert_eq!(base_type("<-chan Msg"), "Msg");
        assert_eq!(base_type("map[string]int"), "");
        assert_eq!(base_type("func(int) error"), "");
    }
}
