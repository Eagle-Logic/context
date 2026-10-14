use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};

use super::span_of;
use crate::model::{Binding, FileFacts, Item, RawCall, Receiver};

pub fn extract(src: &str) -> Result<FileFacts> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .context("loading rust grammar")?;
    let tree = parser
        .parse(src, None)
        .context("tree-sitter parse failed")?;

    let mut facts = FileFacts::default();
    let mut items = Vec::new();
    visit(tree.root_node(), src, &mut items, &mut facts, true);
    facts.items = items;
    let macros = count_macro_invocations(tree.root_node());
    if macros > 0 {
        facts
            .undescended
            .insert("macro invocation".to_string(), macros);
    }
    Ok(facts)
}

/// Count `macro_invocation` nodes, whose contents are never read as code.
///
/// tree-sitter parses a macro's arguments as a `token_tree` of raw tokens
/// rather than expressions, so `foo()` inside `assert_eq!(..)` never becomes a
/// `call_expression` and can never reach the resolver. It is not an unresolved
/// call site; it is not a call site at all, as far as the rest of ctx is
/// concerned. That is the one path where a missing edge is invisible to the
/// machinery built to make missing edges enumerable, so the regions are
/// counted even though their contents cannot be.
///
/// Every macro invocation counts, including those whose token tree holds no
/// calls. Filtering to the ones that "look like they contain a call" would put
/// a guess inside the single mechanism that exists to avoid guessing, and the
/// filter would itself be unverifiable. The cost is a larger number than
/// strictly necessary; the benefit is that it is a fact.
///
/// On ripgrep this counts 3549 regions, which is the scale behind
/// `ctx callers arg` reporting 2 callers where ~357 real call sites exist.
fn count_macro_invocations(root: Node) -> usize {
    let mut n = 0;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "macro_invocation" {
            n += 1;
        }
        let mut c = node.walk();
        for ch in node.named_children(&mut c) {
            stack.push(ch);
        }
    }
    n
}

/// Whether this subtree contains a call expression.
///
/// Exact, not heuristic: the node is ordinary parsed Rust, so this is a
/// question the tree can simply answer. That is the difference from
/// [`count_macro_invocations`], which must count every macro because a token
/// tree cannot be asked anything — here only the initializers that really do
/// hide a call are counted, so the number means what it says.
fn contains_call(node: Node) -> bool {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == "call_expression" {
            return true;
        }
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            stack.push(ch);
        }
    }
    false
}

/// Walk a file, collecting items and the calls inside function bodies.
///
/// One asymmetry worth knowing, because nothing else records it. Python
/// (`python.rs`), TypeScript (`typescript.rs`) and Go (`go.rs`) each
/// synthesize a module-level container so that calls written outside any
/// function are still call sites. Rust has none, so the initializer of a
/// `static` or `const` is parsed as an item but never walked for calls:
///
///     pub fn make() -> u32 { 7 }
///     pub static X: u32 = make();   // not a call site
///     pub const Y: u32 = make();    // not a call site
///     pub fn seen() { make(); }     // a call site
///
/// `ctx doctor` reports `call sites: 1` there. All three calls are real. The
/// two it misses are absent rather than unresolved, so they cannot lower
/// recall — the same shape as the macro-body gap, by a different route.
///
/// They are therefore *disclosed* rather than resolved: each initializer that
/// contains a call is counted into [`FileFacts::undescended`], so `callers`
/// qualifies its completeness line instead of claiming the tree is covered.
/// Before that, `ctx callers make` reported one caller and closed with "no
/// call site named `make` went unresolved anywhere in this tree" — true as
/// written, and false as read.
///
/// README.md names "a call made at module level" as a loss channel; this is
/// the live instance of it, and the only language here that still has one.
fn visit(node: Node, src: &str, items: &mut Vec<Item>, facts: &mut FileFacts, module_level: bool) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if module_level {
            if let "function_item"
            | "function_signature_item"
            | "struct_item"
            | "union_item"
            | "enum_item"
            | "trait_item"
            | "mod_item"
            | "type_item"
            | "const_item"
            | "static_item"
            | "macro_definition" = child.kind()
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
                // The initializer is parsed and then never walked for calls,
                // so a call written here is absent from the graph rather than
                // unresolved — see this function's doc comment. Counting it
                // keeps `callers` from claiming completeness over a gap it
                // cannot see, which is the whole point of the field.
                //
                // A macro in the initializer (`static X: Vec<u32> =
                // vec![make()]`) is already counted as a macro invocation, so
                // it is disclosed either way and never counted twice here.
                if matches!(child.kind(), "const_item" | "static_item") && contains_call(child) {
                    *facts
                        .undescended
                        .entry("module-level initializer".to_string())
                        .or_default() += 1;
                }
                let sig = clip(collapse(text(child, src)).trim_end_matches(';'));
                items.push(item(
                    child.kind(),
                    sig,
                    child,
                    src,
                    Vec::new(),
                    def_name(child, src),
                ));
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
            let (Some(pat), Some(ty)) = (
                p.child_by_field_name("pattern"),
                p.child_by_field_name("type"),
            ) else {
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
        "Send",
        "Sync",
        "Sized",
        "Copy",
        "Clone",
        "Debug",
        "Display",
        "Default",
        "Eq",
        "PartialEq",
        "Ord",
        "PartialOrd",
        "Hash",
        "Unpin",
        "'static",
    ];
    bounds
        .trim_start_matches(':')
        .split('+')
        .map(|b| strip_generics(b.trim()))
        .map(|b| last_seg(&b))
        .find(|b| !b.is_empty() && !b.starts_with('\'') && !MARKERS.contains(&b.as_str()))
}

/// Smart-pointer wrappers that are transparent for method dispatch.
const TRANSPARENT: &[&str] = &[
    "Box", "Arc", "Rc", "RefCell", "Cell", "Mutex", "RwLock", "Cow",
];

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
        let inner = t
            .find('<')
            .map(|i| &t[i + 1..t.rfind('>').unwrap_or(t.len())]);
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
                clip_structure(&head, &fields)
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
                clip_structure_sep(&head, &variants, " | ")
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
            push_callee(f, src, &env, span_of(n), &mut out);
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

fn push_callee(f: Node, src: &str, env: &TypeEnv, at: (usize, usize), out: &mut Vec<RawCall>) {
    match f.kind() {
        "identifier" => out.push(RawCall {
            path: text(f, src).to_string(),
            recv: Receiver::Free,
            line: at.0,
            end_line: at.1,
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
                            line: at.0,
                            end_line: at.1,
                        });
                    }
                }
                return;
            }
            if let Some(rest) = t.strip_prefix("Self::") {
                out.push(RawCall {
                    path: rest.to_string(),
                    recv: Receiver::SelfType,
                    line: at.0,
                    end_line: at.1,
                });
            } else {
                out.push(RawCall {
                    path: t,
                    recv: Receiver::Free,
                    line: at.0,
                    end_line: at.1,
                });
            }
        }
        "generic_function" => {
            if let Some(inner) = f.child_by_field_name("function") {
                push_callee(inner, src, env, at, out);
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
                    line: at.0,
                    end_line: at.1,
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
        "field_expression" => match (
            v.child_by_field_name("value"),
            v.child_by_field_name("field"),
        ) {
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
                    Some((ty, _)) => {
                        classify_type(ty, &HashMap::new()).unwrap_or(Receiver::Unknown)
                    }
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

/// Split on commas that sit outside any brace group.
fn split_top_level(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut depth = 0usize;
    let mut cur = String::new();
    for ch in s.chars() {
        match ch {
            '{' => {
                depth += 1;
                cur.push(ch);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                cur.push(ch);
            }
            ',' if depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    out.push(cur.trim().to_string());
    out.retain(|p| !p.is_empty());
    out
}

/// Contents of the brace group `s` opens with, ignoring anything past its match.
fn brace_inner(s: &str) -> &str {
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, ch) in s.char_indices() {
        match ch {
            '{' => {
                depth += 1;
                if depth == 1 {
                    start = i + 1;
                }
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &s[start..i];
                }
            }
            _ => {}
        }
    }
    // Unbalanced (truncated source): take whatever is there.
    &s[start.min(s.len())..]
}

/// Expand a use tree into full paths, honoring nested brace groups.
///
/// Nesting must be handled recursively. Splitting only the first group on commas
/// turns `use crate::{a::{alpha, gamma}, b::beta}` into `crate::gamma` — both a
/// lost edge to `a::gamma` and a fabricated one to `crate::gamma`. If a sibling
/// module happens to be named `gamma`, that invented dependency *resolves*, and
/// then feeds PageRank, `core`, `subtree`, and pruning order.
fn expand_use_tree(t: &str) -> Vec<String> {
    let t = t.trim();
    let Some(i) = t.find('{') else {
        return if t.is_empty() {
            Vec::new()
        } else {
            vec![t.to_string()]
        };
    };
    let base = t[..i].trim().trim_end_matches(':').trim().to_string();
    let inner = brace_inner(&t[i..]).to_string();
    let mut out = Vec::new();
    for part in split_top_level(&inner) {
        // `use a::{self, b}` imports `a` itself.
        if part == "self" {
            if !base.is_empty() {
                out.push(base.clone());
            }
            continue;
        }
        for p in expand_use_tree(&part) {
            if base.is_empty() {
                out.push(p);
            } else {
                out.push(format!("{base}::{p}"));
            }
        }
    }
    out
}

/// Expand a `use` declaration into (path, bound name) per imported name.
fn parse_use(t: &str) -> Vec<(String, String)> {
    let t = match t.find("use ") {
        Some(i) => &t[i + 4..],
        None => t,
    };
    let t = t.trim().trim_end_matches(';').trim();
    expand_use_tree(t).iter().filter_map(|s| entry(s)).collect()
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

/// Field and variant lists ARE the architectural signal, so they get a much
/// larger allowance than a plain signature — and when a clip does happen it says
/// how many members were dropped, rather than a bare `…` that reads as "that's
/// the whole type".
const SIG_CLIP: usize = 200;
const STRUCTURE_CLIP: usize = 600;

fn clip(s: &str) -> String {
    clip_to(s, SIG_CLIP)
}

fn clip_to(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let mut out: String = s.chars().take(max.saturating_sub(3)).collect();
        out.push('…');
        out
    } else {
        s.to_string()
    }
}

/// Clip a `head { a, b, c }` structure, disclosing how many members were cut.
fn clip_structure(head: &str, members: &[String]) -> String {
    clip_structure_sep(head, members, ", ")
}

fn clip_structure_sep<S: AsRef<str>>(head: &str, members: &[S], sep: &str) -> String {
    let all: Vec<&str> = members.iter().map(|m| m.as_ref()).collect();
    let full = format!("{} {{ {} }}", head, all.join(sep));
    if full.chars().count() <= STRUCTURE_CLIP {
        return full;
    }
    // Keep whole members, then say how many are missing.
    let mut kept: Vec<&str> = Vec::new();
    let mut len = head.chars().count() + 4;
    for m in &all {
        let cost = m.chars().count() + sep.len();
        if len + cost > STRUCTURE_CLIP && !kept.is_empty() {
            break;
        }
        len += cost;
        kept.push(m);
    }
    let dropped = all.len() - kept.len();
    let body = kept.join(sep);
    if dropped == 0 {
        clip_to(&format!("{head} {{ {body} }}"), STRUCTURE_CLIP)
    } else {
        format!("{head} {{ {body}{sep}… +{dropped} more }}")
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

    /// Pins the asymmetry documented on `visit`. This asserts current
    /// behaviour, not desired behaviour: if Rust ever gains module-level call
    /// sites, this fails and whoever did it updates that doc comment and the
    /// matching paragraph in README.md. A documented gap that quietly closes
    /// leaves prose that lies in the other direction.
    #[test]
    fn module_level_initializers_contribute_no_call_sites() {
        let f = extract(
            "pub fn make() -> u32 { 7 }\n\
             pub static X: u32 = make();\n\
             pub const Y: u32 = make();\n\
             pub fn seen() { make(); }\n",
        )
        .unwrap();
        fn count(items: &[Item]) -> usize {
            items
                .iter()
                .map(|i| i.raw_calls.len() + count(&i.children))
                .sum()
        }
        assert_eq!(
            count(&f.items),
            1,
            "three calls to `make` are written; only the one inside `seen` is seen"
        );
        // Unseen is acceptable; unseen and undisclosed is not. The two it
        // missed must show up here so `callers` cannot claim completeness.
        assert_eq!(
            f.undescended.get("module-level initializer"),
            Some(&2),
            "both skipped initializers must be disclosed: {:?}",
            f.undescended
        );
    }

    /// The exactness that separates this from the macro count. A `const` with
    /// no call in it hides nothing, so counting it would inflate the
    /// disclosure with noise — and unlike a token tree, the initializer is
    /// real AST, so "does this hide a call" is answerable rather than guessed.
    #[test]
    fn an_initializer_with_no_call_is_not_disclosed() {
        let f = extract("pub const N: u32 = 7;\npub static S: &str = \"x\";\n").unwrap();
        assert!(
            f.undescended.is_empty(),
            "nothing is hidden here: {:?}",
            f.undescended
        );
    }

    #[test]
    fn a_macro_invocation_is_counted_as_undescended() {
        let f = extract("fn run() { assert_eq!(a(), b()); }\n").unwrap();
        assert_eq!(f.undescended.get("macro invocation"), Some(&1));
    }

    #[test]
    fn a_file_with_no_macros_discloses_nothing() {
        let f = extract("fn run() { helper(); }\nfn helper() {}\n").unwrap();
        assert!(f.undescended.is_empty(), "{:?}", f.undescended);
    }

    /// The reason this disclosure exists. Calls inside a macro are not
    /// unresolved call sites — they are not call sites at all, because
    /// tree-sitter hands back a token tree rather than expressions. So they
    /// cannot lower recall, and nothing but this count reveals them.
    #[test]
    fn calls_inside_a_macro_are_absent_not_unresolved() {
        let f = extract("fn run() { assert_eq!(ghost(), other()); }\n").unwrap();
        let run = f
            .items
            .iter()
            .find(|i| i.name.as_deref() == Some("run"))
            .expect("run should be extracted");
        let seen: Vec<&String> = run.raw_calls.iter().map(|c| &c.path).collect();
        assert!(
            seen.is_empty(),
            "macro body should contribute no call sites, got {seen:?}"
        );
        assert_eq!(f.undescended.get("macro invocation"), Some(&1));
    }

    #[test]
    fn macro_definitions_are_not_counted_as_invocations() {
        let f = extract("macro_rules! m { () => { a(); } }\n").unwrap();
        assert!(
            !f.undescended.contains_key("macro invocation"),
            "a definition is not an invocation: {:?}",
            f.undescended
        );
    }

    #[test]
    fn nested_brace_use_expands_without_inventing_paths() {
        let paths: Vec<String> = expand_use_tree("crate::{a::{alpha, gamma}, b::beta}");
        assert_eq!(
            paths,
            ["crate::a::alpha", "crate::a::gamma", "crate::b::beta"]
        );
    }

    #[test]
    fn use_self_in_group_imports_the_module_itself() {
        let paths: Vec<String> = expand_use_tree("crate::a::{self, alpha}");
        assert_eq!(paths, ["crate::a", "crate::a::alpha"]);
    }

    #[test]
    fn deeply_nested_use_groups_expand() {
        let paths: Vec<String> = expand_use_tree("x::{y::{z::{deep, deeper}}, flat}");
        assert_eq!(paths, ["x::y::z::deep", "x::y::z::deeper", "x::flat"]);
    }

    #[test]
    fn clipped_structures_disclose_how_many_members_were_dropped() {
        let members: Vec<String> = (0..80)
            .map(|i| format!("pub field_with_a_long_name_{i}: SomeLongTypeName<Inner>"))
            .collect();
        let s = clip_structure("pub struct Big", &members);
        assert!(
            s.contains("more }"),
            "a clipped member list must say what is missing: {s}"
        );
        assert!(
            !s.ends_with('…'),
            "a bare ellipsis reads as a complete list"
        );
    }

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
