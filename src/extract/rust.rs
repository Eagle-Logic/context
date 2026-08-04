use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};

use crate::model::{Binding, FileFacts, Item, RawCall, Receiver};

pub fn extract(src: &str) -> Result<FileFacts> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .context("loading rust grammar")?;
    let tree = parser.parse(src, None).context("tree-sitter parse failed")?;

    let mut facts = FileFacts::default();
    let mut items = Vec::new();
    visit(tree.root_node(), src, &mut items, &mut facts, true);
    facts.items = items;
    Ok(facts)
}

fn visit(
    node: Node,
    src: &str,
    items: &mut Vec<Item>,
    facts: &mut FileFacts,
    module_level: bool,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if module_level {
            if let "function_item" | "function_signature_item" | "struct_item" | "union_item"
            | "enum_item" | "trait_item" | "mod_item" | "type_item" | "const_item"
            | "static_item" | "macro_definition" = child.kind()
            {
                if let Some(n) = child.child_by_field_name("name") {
                    facts.defined.insert(text(n, src).to_string());
                }
            }
        }
        match child.kind() {
            "use_declaration" => {
                let mut c = child.walk();
                let is_pub = child
                    .named_children(&mut c)
                    .any(|n| n.kind() == "visibility_modifier");
                for (path, name) in parse_use(text(child, src)) {
                    facts.imports.push(path.clone());
                    if module_level {
                        // Private bindings resolve local calls; only pub ones
                        // are chase-able / rendered as re-exports.
                        facts.reexports.push(Binding {
                            name,
                            path,
                            public: is_pub,
                        });
                    }
                }
            }
            "function_item" | "function_signature_item" => {
                items.push(function(child, src, &TypeEnv::default()))
            }
            "struct_item" | "union_item" => items.push(structure(child, src)),
            "enum_item" => items.push(enumeration(child, src)),
            "trait_item" | "impl_item" => items.push(container(child, src, facts)),
            "mod_item" => {
                // Inline `mod name { ... }`; file-based `mod name;` declarations
                // are covered by the file walk itself.
                if let Some(body) = child.child_by_field_name("body") {
                    let mut sub = Vec::new();
                    visit(body, src, &mut sub, facts, true);
                    items.push(item(
                        "mod",
                        head_before(child, body, src),
                        child,
                        src,
                        sub,
                        def_name(child, src),
                    ));
                }
            }
            "type_item" | "associated_type" | "const_item" | "static_item" => {
                let sig = clip(collapse(text(child, src)).trim_end_matches(';'));
                items.push(item(child.kind(), sig, child, src, Vec::new(), def_name(child, src)));
            }
            "macro_definition" => {
                let name = def_name(child, src);
                let sig = format!("macro_rules! {}", name.as_deref().unwrap_or("?"));
                items.push(item("macro", sig, child, src, Vec::new(), name));
            }
            _ => {}
        }
    }
}

fn function(node: Node, src: &str, outer: &TypeEnv) -> Item {
    let env = fn_env(node, src, outer);
    let (sig, raw_calls, nested) = match node.child_by_field_name("body") {
        Some(body) => {
            let (calls, nested) = body_facts(body, src, &env);
            (head_before(node, body, src), calls, nested)
        }
        None => (
            collapse(text(node, src)).trim_end_matches(';').to_string(),
            Vec::new(),
            Vec::new(),
        ),
    };
    let mut it = item("fn", sig, node, src, nested, def_name(node, src));
    it.raw_calls = raw_calls;
    it.arity = arity(node);
    it
}

/// The receiver types ctx can name inside one function body: parameters with
/// declared types, generic parameters standing for a trait bound, and `let`
/// bindings whose type is annotated or obvious from the initializer. An outer
/// function's environment is inherited so a closure or nested `fn` still sees
/// the enclosing bindings.
#[derive(Default, Clone)]
struct TypeEnv {
    vars: HashMap<String, Receiver>,
}

fn fn_env(node: Node, src: &str, outer: &TypeEnv) -> TypeEnv {
    let mut env = outer.clone();
    // Generic parameters bounded by a trait dispatch over its implementors:
    // `fn run<S: Sampler>(s: S)` makes `s` a dispatch receiver.
    let mut generics: HashMap<String, String> = HashMap::new();
    let mut c = node.walk();
    for ch in node.named_children(&mut c) {
        match ch.kind() {
            "type_parameters" => {
                let mut tc = ch.walk();
                for p in ch.named_children(&mut tc) {
                    if let Some((n, b)) = bounded_param(p, src) {
                        generics.insert(n, b);
                    }
                }
            }
            "where_clause" => {
                let mut wc = ch.walk();
                for pred in ch.named_children(&mut wc) {
                    if let Some((n, b)) = bounded_param(pred, src) {
                        generics.entry(n).or_insert(b);
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(params) = node.child_by_field_name("parameters") {
        let mut c = params.walk();
        for p in params.named_children(&mut c) {
            if p.kind() != "parameter" {
                continue;
            }
            let (Some(pat), Some(ty)) =
                (p.child_by_field_name("pattern"), p.child_by_field_name("type"))
            else {
                continue;
            };
            let name = text(pat, src).trim_start_matches("mut ").trim().to_string();
            if name.is_empty() || name.contains(['(', '[', '{']) {
                continue; // destructuring pattern — no single receiver name
            }
            if let Some(r) = classify_type(text(ty, src), &generics) {
                env.vars.insert(name, r);
            }
        }
    }
    env
}

/// `S: Sampler` in either a `<..>` list or a `where` clause -> (S, Sampler).
/// The grammar exposes these as an unnamed (type_identifier, trait_bounds)
/// pair rather than named fields, so both are located positionally.
fn bounded_param(p: Node, src: &str) -> Option<(String, String)> {
    let mut c = p.walk();
    let kids: Vec<Node> = p.named_children(&mut c).collect();
    let name = kids
        .iter()
        .find(|k| matches!(k.kind(), "type_identifier" | "constrained_type_parameter"))?;
    let bounds = kids.iter().find(|k| k.kind() == "trait_bounds")?;
    let n = text(*name, src).trim().to_string();
    first_bound(text(*bounds, src)).map(|b| (n, b))
}

/// The first non-lifetime, non-marker trait in a bound list (`: Sampler + Send`).
fn first_bound(bounds: &str) -> Option<String> {
    const MARKERS: &[&str] = &[
        "Send", "Sync", "Sized", "Copy", "Clone", "Debug", "Display", "Default", "Eq", "PartialEq",
        "Ord", "PartialOrd", "Hash", "Unpin", "'static",
    ];
    bounds
        .trim_start_matches(':')
        .split('+')
        .map(|b| strip_generics(b.trim()))
        .map(|b| last_seg(&b))
        .find(|b| {
            !b.is_empty() && !b.starts_with('\'') && !MARKERS.contains(&b.as_str())
        })
}

/// Smart-pointer wrappers that are transparent for method dispatch.
const TRANSPARENT: &[&str] = &["Box", "Arc", "Rc", "RefCell", "Cell", "Mutex", "RwLock", "Cow"];

/// Turn a declared type into the receiver kind it implies, or None when the
/// type says nothing useful about which method body runs (`Vec<T>`, `Option<T>`,
/// primitives).
fn classify_type(raw: &str, generics: &HashMap<String, String>) -> Option<Receiver> {
    let t = collapse(raw);
    let t = t
        .trim()
        .trim_start_matches('&')
        .trim()
        .trim_start_matches("'a")
        .trim()
        .trim_start_matches("mut ")
        .trim();
    if let Some(rest) = t.strip_prefix("dyn ") {
        return Some(Receiver::Dyn(last_seg(&strip_generics(rest))));
    }
    if let Some(rest) = t.strip_prefix("impl ") {
        return first_bound(rest).map(Receiver::Dyn);
    }
    let base = last_seg(&strip_generics(t));
    if TRANSPARENT.contains(&base.as_str()) {
        // Look through the wrapper: Box<dyn Sampler> dispatches like `dyn Sampler`.
        let inner = t.find('<').map(|i| &t[i + 1..t.rfind('>').unwrap_or(t.len())]);
        return inner.and_then(|i| classify_type(i, generics));
    }
    if let Some(bound) = generics.get(&base) {
        return Some(Receiver::Dyn(bound.clone()));
    }
    if base.is_empty() || !base.chars().next().is_some_and(char::is_uppercase) {
        return None;
    }
    Some(Receiver::Typed(base))
}

fn last_seg(s: &str) -> String {
    s.rsplit("::").next().unwrap_or(s).trim().to_string()
}

/// Value-parameter count for a `fn`, excluding a `self` receiver.
fn arity(node: Node) -> Option<usize> {
    let params = node.child_by_field_name("parameters")?;
    let mut cursor = params.walk();
    Some(
        params
            .named_children(&mut cursor)
            .filter(|c| c.kind() != "self_parameter")
            .count(),
    )
}

fn structure(node: Node, src: &str) -> Item {
    let mut field_types = BTreeMap::new();
    let sig = match node.child_by_field_name("body") {
        Some(body) if body.kind() == "field_declaration_list" => {
            let head = head_before(node, body, src);
            let mut cursor = body.walk();
            let decls: Vec<Node> = body
                .named_children(&mut cursor)
                .filter(|c| c.kind() == "field_declaration")
                .collect();
            for d in &decls {
                // Field types make `self.field.method()` resolvable.
                if let (Some(n), Some(t)) =
                    (d.child_by_field_name("name"), d.child_by_field_name("type"))
                {
                    field_types.insert(text(n, src).to_string(), collapse(text(t, src)));
                }
            }
            let fields: Vec<String> = decls.iter().map(|c| collapse(text(*c, src))).collect();
            if fields.is_empty() {
                head
            } else {
                clip(&format!("{} {{ {} }}", head, fields.join(", ")))
            }
        }
        // Tuple structs / unit structs: the whole declaration is the signature.
        _ => clip(collapse(text(node, src)).trim_end_matches(';')),
    };
    let mut it = item("struct", sig, node, src, Vec::new(), def_name(node, src));
    it.field_types = field_types;
    it
}

fn enumeration(node: Node, src: &str) -> Item {
    let sig = match node.child_by_field_name("body") {
        Some(body) => {
            let head = head_before(node, body, src);
            let mut cursor = body.walk();
            let variants: Vec<&str> = body
                .named_children(&mut cursor)
                .filter(|c| c.kind() == "enum_variant")
                .filter_map(|c| c.child_by_field_name("name").map(|n| text(n, src)))
                .collect();
            if variants.is_empty() {
                head
            } else {
                clip(&format!("{} {{ {} }}", head, variants.join(" | ")))
            }
        }
        None => collapse(text(node, src)),
    };
    item("enum", sig, node, src, Vec::new(), def_name(node, src))
}

fn container(node: Node, src: &str, facts: &mut FileFacts) -> Item {
    let (kind, name) = if node.kind() == "trait_item" {
        ("trait", def_name(node, src))
    } else {
        // impl blocks: the implementing type is the container name.
        let name = node
            .child_by_field_name("type")
            .map(|t| last_seg(&strip_generics(&collapse(text(t, src)))));
        ("impl", name)
    };
    // `impl Trait for Type` — the trait is what callers dispatch through.
    let implements: Vec<String> = node
        .child_by_field_name("trait")
        .map(|t| vec![last_seg(&strip_generics(&collapse(text(t, src))))])
        .unwrap_or_default();
    let mut it = match node.child_by_field_name("body") {
        Some(body) => {
            let mut sub = Vec::new();
            visit(body, src, &mut sub, facts, false);
            item(kind, head_before(node, body, src), node, src, sub, name)
        }
        None => item(kind, collapse(text(node, src)), node, src, Vec::new(), name),
    };
    it.implements = implements;
    it
}

/// Walk a function body for everything resolution needs: the call sites (with
/// receivers typed wherever the source says what they are) and any nested
/// `fn` items, which become children so their calls are attributed to them
/// rather than smeared onto the enclosing function.
fn body_facts(body: Node, src: &str, env: &TypeEnv) -> (Vec<RawCall>, Vec<Item>) {
    // Pass 1: local bindings and nested functions. Call sites are noted but
    // not resolved yet, so a binding declared below a call still types it.
    let mut env = env.clone();
    let mut call_nodes: Vec<Node> = Vec::new();
    let mut nested: Vec<Item> = Vec::new();
    let mut stack = vec![body];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "function_item" => {
                nested.push(function(n, src, &env));
                continue; // its body belongs to it, not to us
            }
            // Types declared inside a function body are real definitions: skip
            // them and `let w = Walk {..}; w.step()` has no type to resolve
            // against, silently breaking the call chain at that hop.
            "struct_item" | "union_item" | "enum_item" | "impl_item" | "trait_item" => {
                nested.push(local_type(n, src));
                continue;
            }
            "let_declaration" => {
                if let Some((name, r)) = let_binding(n, src, &env) {
                    env.vars.insert(name, r);
                }
                // A `let`-bound closure is a callable the enclosing function
                // owns; calls to it are internal edges, not mystery names.
                if let Some(c) = let_closure(n, src, &env) {
                    nested.push(c);
                }
            }
            "call_expression" => call_nodes.push(n),
            _ => {}
        }
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            stack.push(ch);
        }
    }

    // Pass 2: resolve receivers against the completed environment. Reverse
    // restores source order, which the stack walk inverted.
    call_nodes.reverse();
    let mut out = Vec::new();
    for n in call_nodes {
        if let Some(f) = n.child_by_field_name("function") {
            push_callee(f, src, &env, &mut out);
        }
    }
    (out, nested)
}

/// A struct/enum/impl/trait declared inside a function body, lifted to a child
/// item so its methods are indexed like any other type's.
fn local_type(n: Node, src: &str) -> Item {
    let mut scratch = FileFacts::default();
    match n.kind() {
        "struct_item" | "union_item" => structure(n, src),
        "enum_item" => enumeration(n, src),
        _ => container(n, src, &mut scratch),
    }
}

/// `let pct = |n, d| ...` — a named closure, lifted to a child `fn` item so it
/// is findable by `def`, traceable, and resolvable as a callee.
fn let_closure(n: Node, src: &str, env: &TypeEnv) -> Option<Item> {
    let pat = n.child_by_field_name("pattern")?;
    let name = text(pat, src).trim_start_matches("mut ").trim().to_string();
    if name.is_empty() || name.contains(['(', '[', '{', ':']) {
        return None;
    }
    let value = n.child_by_field_name("value")?;
    if value.kind() != "closure_expression" {
        return None;
    }
    let params = value.child_by_field_name("parameters");
    let body = value.child_by_field_name("body");
    let sig = match body {
        Some(b) => collapse(&src[n.start_byte()..b.start_byte()])
            .trim_end_matches('{')
            .trim()
            .to_string(),
        None => collapse(text(n, src)),
    };
    let (raw_calls, inner) = match body {
        Some(b) => body_facts(b, src, env),
        None => (Vec::new(), Vec::new()),
    };
    let mut it = item("fn", clip(&sig), n, src, inner, Some(name));
    it.raw_calls = raw_calls;
    it.arity = params.map(|p| {
        let mut c = p.walk();
        p.named_children(&mut c).count()
    });
    Some(it)
}

/// The type a `let` introduces: an explicit annotation, or a constructor call
/// / struct literal on the right-hand side.
fn let_binding(n: Node, src: &str, env: &TypeEnv) -> Option<(String, Receiver)> {
    let pat = n.child_by_field_name("pattern")?;
    let name = text(pat, src).trim_start_matches("mut ").trim().to_string();
    if name.is_empty() || name.contains(['(', '[', '{', ':']) {
        return None; // destructuring — no single receiver name
    }
    let generics = HashMap::new();
    if let Some(ty) = n.child_by_field_name("type") {
        if let Some(r) = classify_type(text(ty, src), &generics) {
            return Some((name, r));
        }
    }
    let value = n.child_by_field_name("value")?;
    let r = match value.kind() {
        // `let e = Engine::new(..)` — an associated function names its type.
        "call_expression" => {
            let f = value.child_by_field_name("function")?;
            let path = strip_turbofish(&collapse(text(f, src)));
            let (ty, _) = path.rsplit_once("::")?;
            classify_type(ty, &generics)?
        }
        // `let c = Config { .. }`
        "struct_expression" => {
            let nm = value.child_by_field_name("name")?;
            classify_type(text(nm, src), &generics)?
        }
        // `let x = y;` propagates y's type.
        "identifier" => env.vars.get(text(value, src))?.clone(),
        _ => return None,
    };
    Some((name, r))
}

fn push_callee(f: Node, src: &str, env: &TypeEnv, out: &mut Vec<RawCall>) {
    match f.kind() {
        "identifier" => out.push(RawCall {
            path: text(f, src).to_string(),
            recv: Receiver::Free,
        }),
        "scoped_identifier" => {
            let t = strip_turbofish(&collapse(text(f, src)));
            // `<T as Trait>::f(..)` — qualified trait dispatch. The trait is
            // named right there, so this resolves to its implementors.
            if let Some(rest) = t.strip_prefix('<') {
                if let Some((qual, method)) = rest.split_once(">::") {
                    if let Some((_, tr)) = qual.split_once(" as ") {
                        out.push(RawCall {
                            path: method.trim().to_string(),
                            recv: Receiver::Dyn(last_seg(&strip_generics(tr.trim()))),
                        });
                    }
                }
                return;
            }
            if let Some(rest) = t.strip_prefix("Self::") {
                out.push(RawCall {
                    path: rest.to_string(),
                    recv: Receiver::SelfType,
                });
            } else {
                out.push(RawCall {
                    path: t,
                    recv: Receiver::Free,
                });
            }
        }
        "generic_function" => {
            if let Some(inner) = f.child_by_field_name("function") {
                push_callee(inner, src, env, out);
            }
        }
        "field_expression" => {
            if let Some(field) = f.child_by_field_name("field") {
                let recv = match f.child_by_field_name("value") {
                    Some(v) => receiver_of(v, src, env),
                    None => Receiver::Unknown,
                };
                out.push(RawCall {
                    path: text(field, src).to_string(),
                    recv,
                });
            }
        }
        _ => {}
    }
}

/// Classify the receiver expression of a method call.
fn receiver_of(v: Node, src: &str, env: &TypeEnv) -> Receiver {
    match v.kind() {
        "self" => Receiver::SelfType,
        "identifier" => {
            let name = text(v, src);
            if name == "self" {
                return Receiver::SelfType;
            }
            env.vars.get(name).cloned().unwrap_or(Receiver::Unknown)
        }
        // `self.field.method()` — resolved later against the enclosing type's
        // declared field types.
        "field_expression" => match (v.child_by_field_name("value"), v.child_by_field_name("field")) {
            (Some(o), Some(fld)) if text(o, src) == "self" => {
                Receiver::SelfField(text(fld, src).to_string())
            }
            _ => Receiver::Unknown,
        },
        // `Engine::new().step()` / `self.build().step()`: the constructor names
        // the type it returns.
        "call_expression" => match v.child_by_field_name("function") {
            Some(f) if f.kind() == "scoped_identifier" => {
                let path = strip_turbofish(&collapse(text(f, src)));
                match path.rsplit_once("::") {
                    Some((ty, _)) => classify_type(ty, &HashMap::new()).unwrap_or(Receiver::Unknown),
                    None => Receiver::Unknown,
                }
            }
            _ => Receiver::Unknown,
        },
        _ => Receiver::Unknown,
    }
}

/// Remove `::<...>` turbofish segments from a call path.
fn strip_turbofish(s: &str) -> String {
    let b: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut depth = 0usize;
    let mut i = 0;
    while i < b.len() {
        if depth == 0 && i + 2 < b.len() && b[i] == ':' && b[i + 1] == ':' && b[i + 2] == '<' {
            depth = 1;
            i += 3;
            continue;
        }
        if depth > 0 {
            if b[i] == '<' {
                depth += 1;
            } else if b[i] == '>' {
                depth -= 1;
            }
            i += 1;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn strip_generics(s: &str) -> String {
    match s.find('<') {
        Some(i) => s[..i].trim().to_string(),
        None => s.to_string(),
    }
}

fn def_name(node: Node, src: &str) -> Option<String> {
    node.child_by_field_name("name")
        .map(|n| text(n, src).to_string())
}

/// Expand a `use` declaration into (path, bound name) per imported name.
fn parse_use(t: &str) -> Vec<(String, String)> {
    let t = match t.find("use ") {
        Some(i) => &t[i + 4..],
        None => t,
    };
    let t = t.trim().trim_end_matches(';').trim();

    let full_paths: Vec<String> = if let Some(brace) = t.find('{') {
        let base = t[..brace].trim_end_matches(':').trim().to_string();
        t[brace + 1..]
            .trim_end_matches('}')
            .split(',')
            .map(|p| p.replace(['{', '}'], "").trim().to_string())
            .filter(|p| !p.is_empty())
            .map(|p| {
                if base.is_empty() {
                    p
                } else {
                    format!("{base}::{p}")
                }
            })
            .collect()
    } else {
        vec![t.to_string()]
    };

    full_paths.iter().filter_map(|s| entry(s)).collect()
}

fn entry(s: &str) -> Option<(String, String)> {
    let (path_part, alias) = match s.split_once(" as ") {
        Some((p, a)) => (p.trim(), Some(a.trim().to_string())),
        None => (s.trim(), None),
    };
    let mut path = path_part.to_string();
    let mut name = String::new();
    if let Some(p) = path.strip_suffix("::*") {
        path = p.to_string();
        name = "*".to_string();
    } else if path == "*" {
        return None;
    } else if let Some(p) = path.strip_suffix("::self") {
        // `use foo::{self}` binds the module itself under its own name.
        path = p.to_string();
    }
    if path.is_empty() {
        return None;
    }
    if name.is_empty() {
        name = path.rsplit("::").next().unwrap_or(&path).to_string();
    }
    Some((path, alias.unwrap_or(name)))
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

fn head_before(node: Node, body: Node, src: &str) -> String {
    collapse(&src[node.start_byte()..body.start_byte()])
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

/// First line of the `///` (or `/** */`) doc block immediately preceding an
/// item. Attributes between the comment and the item are skipped; inner docs
/// (`//!`), regular comments (`//`, `////`), and blank runs stop the search.
fn doc_comment(node: Node, src: &str) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut sib = node.prev_sibling();
    while let Some(s) = sib {
        match s.kind() {
            "attribute_item" => {}
            "line_comment" => match line_doc(text(s, src).trim()) {
                Some(d) => lines.push(d),
                None => break,
            },
            "block_comment" => {
                if let Some(d) = block_doc(text(s, src).trim()) {
                    lines.push(d);
                }
                break;
            }
            _ => break,
        }
        sib = s.prev_sibling();
    }
    lines.reverse();
    lines
        .into_iter()
        .find(|l| !l.is_empty())
        .map(|l| clip_doc(&l))
}

/// Outer line doc (`/// text`). Returns None for regular (`//`, `////`) or
/// inner (`//!`) comments so they terminate the run.
fn line_doc(t: &str) -> Option<String> {
    let rest = t.strip_prefix("///")?;
    if rest.starts_with('/') {
        return None; // //// ... is a normal comment
    }
    Some(rest.trim().to_string())
}

/// First non-empty line of an outer block doc (`/** ... */`).
fn block_doc(t: &str) -> Option<String> {
    let rest = t.strip_prefix("/**")?;
    if rest.starts_with('*') {
        return None; // /*** or /**/ is not a doc comment
    }
    let body = rest.trim_end_matches("*/");
    Some(
        body.lines()
            .map(|l| l.trim().trim_start_matches('*').trim().to_string())
            .find(|l| !l.is_empty())
            .unwrap_or_default(),
    )
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

    fn items(src: &str) -> Vec<Item> {
        extract(src).unwrap().items
    }

    fn doc_of(src: &str) -> Option<String> {
        items(src).into_iter().next().and_then(|i| i.doc)
    }

    #[test]
    fn line_doc_takes_first_line() {
        assert_eq!(
            doc_of("/// First line.\n/// Second line.\npub fn foo() {}\n").as_deref(),
            Some("First line.")
        );
    }

    #[test]
    fn doc_skips_intervening_attributes() {
        assert_eq!(
            doc_of("/// Docs.\n#[inline]\npub fn foo() {}\n").as_deref(),
            Some("Docs.")
        );
    }

    #[test]
    fn regular_inner_and_quad_comments_are_not_docs() {
        // `//`, `//!`, and `////` must not be attributed as item docs.
        for src in [
            "// plain\npub fn a() {}\n",
            "//! inner\npub fn a() {}\n",
            "//// quad\npub fn a() {}\n",
        ] {
            assert_eq!(doc_of(src), None, "{src:?}");
        }
    }

    #[test]
    fn block_doc_first_line() {
        assert_eq!(
            doc_of("/** Block summary.\n * more */\npub struct S;\n").as_deref(),
            Some("Block summary.")
        );
    }

    #[test]
    fn absent_doc_is_none() {
        assert_eq!(doc_of("pub fn foo() {}\n"), None);
    }

    #[test]
    fn struct_and_fn_are_extracted() {
        let it = items("pub struct S { a: i32 }\npub fn f() {}\n");
        assert_eq!(it.len(), 2);
        assert_eq!(it[0].kind, "struct");
        assert_eq!(it[1].kind, "fn");
    }
}
