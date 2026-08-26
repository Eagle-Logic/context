use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};

use crate::model::{Binding, FileFacts, Item, RawCall, Receiver};

pub fn extract(src: &str) -> Result<FileFacts> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .context("loading python grammar")?;
    let tree = parser.parse(src, None).context("tree-sitter parse failed")?;

    let mut facts = FileFacts::default();
    let mut items = Vec::new();
    let root = tree.root_node();
    visit(root, src, &mut items, &mut facts);
    if let Some(it) = module_level_item(root, src) {
        items.insert(0, it);
    }
    facts.items = items;
    Ok(facts)
}

/// Calls made at module level, as a synthetic unnamed item.
///
/// Only function bodies were scanned, so a call in top-level code — extremely
/// common in scripts, test modules and `__init__.py` wiring — was invisible to
/// `callers`. Being unnamed, it reports the module itself as the caller and
/// never pollutes `def` lookups.
fn module_level_item(root: Node, src: &str) -> Option<Item> {
    let mut cursor = root.walk();
    let mut raw_calls = Vec::new();
    for child in root.named_children(&mut cursor) {
        if matches!(
            child.kind(),
            "import_statement"
                | "import_from_statement"
                | "future_import_statement"
                | "decorated_definition"
                | "class_definition"
                | "function_definition"
        ) {
            continue;
        }
        let (calls, _) = body_facts(child, src, &TypeEnv::default(), &mut BTreeSet::new());
        raw_calls.extend(calls);
    }
    if raw_calls.is_empty() {
        return None;
    }
    Some(Item {
        kind: "def".to_string(),
        signature: "<module level>".to_string(),
        line: 1,
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

fn visit(node: Node, src: &str, items: &mut Vec<Item>, facts: &mut FileFacts) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "import_statement" => parse_import(text(child, src), facts),
            "import_from_statement" => parse_from_import(text(child, src), facts),
            "future_import_statement" => {}
            "decorated_definition" => {
                if let Some(def) = child.child_by_field_name("definition") {
                    definition(def, src, items, &mut facts.defined, true);
                }
            }
            "class_definition" | "function_definition" => {
                definition(child, src, items, &mut facts.defined, true)
            }
            _ => {}
        }
    }
}

/// Receiver types ctx can name inside one body: annotated parameters,
/// annotated assignments, and `x = Foo()` constructor calls.
#[derive(Default, Clone)]
struct TypeEnv {
    vars: HashMap<String, String>,
}

/// The bare class name a Python type annotation points at, or None when the
/// annotation says nothing useful (`list[str]`, `int`, a string forward ref
/// that is not a plain name).
fn annotation_type(raw: &str) -> Option<String> {
    let t = collapse(raw);
    let t = t.trim().trim_matches(['"', '\'']).trim();
    // Optional[Foo] / list[Foo] — only unwrap the Optional-shaped ones.
    let t = match t.strip_prefix("Optional[").and_then(|r| r.strip_suffix(']')) {
        Some(inner) => inner.trim(),
        None => t,
    };
    let t = t.split('|').next().unwrap_or(t).trim();
    if t.contains('[') || t.is_empty() {
        return None;
    }
    let base = t.rsplit('.').next().unwrap_or(t).trim();
    base.chars()
        .next()
        .is_some_and(char::is_uppercase)
        .then(|| base.to_string())
}

fn fn_env(node: Node, src: &str) -> TypeEnv {
    let mut env = TypeEnv::default();
    let Some(params) = node.child_by_field_name("parameters") else {
        return env;
    };
    let mut c = params.walk();
    for p in params.named_children(&mut c) {
        // `x: Foo` and `x: Foo = None`
        if !matches!(p.kind(), "typed_parameter" | "typed_default_parameter") {
            continue;
        }
        let Some(ty) = p.child_by_field_name("type") else { continue };
        let name = match p.kind() {
            "typed_default_parameter" => p.child_by_field_name("name").map(|n| text(n, src)),
            _ => p.named_children(&mut p.walk()).next().map(|n| text(n, src)),
        };
        let (Some(name), Some(t)) = (name, annotation_type(text(ty, src))) else {
            continue;
        };
        env.vars.insert(name.to_string(), t);
    }
    env
}

fn definition(
    node: Node,
    src: &str,
    items: &mut Vec<Item>,
    defined: &mut BTreeSet<String>,
    top: bool,
) {
    let name = node
        .child_by_field_name("name")
        .map(|n| text(n, src).to_string());
    if top {
        if let Some(n) = &name {
            defined.insert(n.clone());
        }
    }
    let body = node.child_by_field_name("body");
    let head = match body {
        Some(b) => collapse(&src[node.start_byte()..b.start_byte()]),
        None => collapse(text(node, src)),
    };
    let head = head.trim_end_matches(':').trim().to_string();
    let is_class = node.kind() == "class_definition";

    let mut children = Vec::new();
    let mut raw_calls = Vec::new();
    let mut implements = Vec::new();
    let mut field_types = BTreeMap::new();
    if is_class {
        // Base classes are Python's interface: a call on a value typed as the
        // base may land in any subclass.
        if let Some(args) = node.child_by_field_name("superclasses") {
            let mut c = args.walk();
            for a in args.named_children(&mut c) {
                if let Some(t) = annotation_type(text(a, src)) {
                    implements.push(t);
                }
            }
        }
        if let Some(b) = body {
            let mut cursor = b.walk();
            for c in b.named_children(&mut cursor) {
                match c.kind() {
                    "decorated_definition" => {
                        if let Some(def) = c.child_by_field_name("definition") {
                            definition(def, src, &mut children, defined, false);
                        }
                    }
                    "class_definition" | "function_definition" => {
                        definition(c, src, &mut children, defined, false)
                    }
                    _ => {}
                }
            }
            field_types = class_fields(b, src);
        }
    } else if let Some(b) = body {
        let env = fn_env(node, src);
        let (calls, nested) = body_facts(b, src, &env, defined);
        raw_calls = calls;
        children = nested;
    }

    items.push(Item {
        kind: if is_class { "class" } else { "def" }.to_string(),
        signature: clip(&head),
        line: node.start_position().row + 1,
        doc: body.and_then(|b| docstring(b, src)),
        calls: Vec::new(),
        children,
        arity: if is_class { None } else { arity(node, src) },
        name,
        raw_calls,
        implements,
        field_types,
    });
}

/// Declared attribute types of a class: `x: Foo` at class level and
/// `self.x: Foo = ...` inside any method.
fn class_fields(body: Node, src: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![body];
    while let Some(n) = stack.pop() {
        if n.kind() == "typed_parameter" || n.kind() == "typed_default_parameter" {
            continue;
        }
        if let ("assignment", Some(l), Some(ty)) = (
            n.kind(),
            n.child_by_field_name("left"),
            n.child_by_field_name("type"),
        ) {
            let name = text(l, src).trim();
            let name = name.strip_prefix("self.").unwrap_or(name);
            if !name.contains('.') && !name.is_empty() {
                if let Some(t) = annotation_type(text(ty, src)) {
                    out.insert(name.to_string(), t);
                }
            }
        }
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            stack.push(ch);
        }
    }
    out
}

/// Value-parameter count for a `def`, excluding a leading `self`/`cls`.
fn arity(node: Node, src: &str) -> Option<usize> {
    let params = node.child_by_field_name("parameters")?;
    let mut cursor = params.walk();
    let ps: Vec<Node> = params.named_children(&mut cursor).collect();
    let mut count = 0;
    for (i, p) in ps.iter().enumerate() {
        if i == 0 {
            let first = text(*p, src)
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .next()
                .unwrap_or("");
            if first == "self" || first == "cls" {
                continue;
            }
        }
        count += 1;
    }
    Some(count)
}

/// First non-empty line of a def/class docstring: the leading string literal
/// of the body, if any.
fn docstring(body: Node, src: &str) -> Option<String> {
    let mut cursor = body.walk();
    let first = body.named_children(&mut cursor).next()?;
    if first.kind() != "expression_statement" {
        return None;
    }
    let mut c2 = first.walk();
    let s = first.named_children(&mut c2).next()?;
    if s.kind() != "string" {
        return None;
    }
    strip_py_string(text(s, src))
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(clip_doc)
}

/// Strip string prefixes (r/b/f/u) and surrounding quotes from a Python
/// string literal, returning the raw content.
fn strip_py_string(raw: &str) -> &str {
    let s = raw.trim_start_matches(['r', 'R', 'b', 'B', 'f', 'F', 'u', 'U']);
    for q in ["\"\"\"", "'''", "\"", "'"] {
        if let Some(rest) = s.strip_prefix(q) {
            return rest.strip_suffix(q).unwrap_or(rest);
        }
    }
    s
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

/// Walk a function body for its call sites (receivers typed where an
/// annotation or constructor says what they are) and its nested `def`s, which
/// become children so their calls are attributed to them.
fn body_facts(
    body: Node,
    src: &str,
    env: &TypeEnv,
    defined: &mut BTreeSet<String>,
) -> (Vec<RawCall>, Vec<Item>) {
    let mut env = env.clone();
    let mut call_nodes: Vec<Node> = Vec::new();
    let mut nested: Vec<Item> = Vec::new();
    let mut stack = vec![body];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "function_definition" => {
                definition(n, src, &mut nested, defined, false);
                continue; // its body belongs to it
            }
            "assignment" => {
                if let Some((name, ty)) = assign_binding(n, src) {
                    env.vars.insert(name, ty);
                }
            }
            "call" => call_nodes.push(n),
            _ => {}
        }
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            stack.push(ch);
        }
    }
    call_nodes.reverse();
    let mut out = Vec::new();
    for n in call_nodes {
        if let Some(f) = n.child_by_field_name("function") {
            push_callee(f, src, &env, &mut out);
        }
    }
    (out, nested)
}

/// `x: Foo = ...` or `x = Foo(...)` — the two assignment forms that name a type.
fn assign_binding(n: Node, src: &str) -> Option<(String, String)> {
    let left = n.child_by_field_name("left")?;
    let name = text(left, src).trim().to_string();
    if name.contains(['.', ',', '[']) || name.is_empty() {
        return None;
    }
    if let Some(ty) = n.child_by_field_name("type") {
        if let Some(t) = annotation_type(text(ty, src)) {
            return Some((name, t));
        }
    }
    let value = n.child_by_field_name("right")?;
    if value.kind() != "call" {
        return None;
    }
    let f = value.child_by_field_name("function")?;
    // A constructor call is an identifier (or dotted path) starting uppercase.
    let t = annotation_type(text(f, src))?;
    Some((name, t))
}

fn push_callee(f: Node, src: &str, env: &TypeEnv, out: &mut Vec<RawCall>) {
    match f.kind() {
        "identifier" => out.push(RawCall {
            path: text(f, src).to_string(),
            recv: Receiver::Free,
        }),
        "attribute" => {
            let t = collapse(text(f, src));
            if t.contains('(') {
                return;
            }
            if let Some(rest) = t.strip_prefix("self.").or_else(|| t.strip_prefix("cls.")) {
                if !rest.contains('.') {
                    out.push(RawCall {
                        path: rest.to_string(),
                        recv: Receiver::SelfType,
                    });
                } else if let Some((field, method)) = rest.split_once('.') {
                    // `self.engine.step()` — resolved against the attribute's
                    // declared type.
                    if !method.contains('.') {
                        out.push(RawCall {
                            path: method.to_string(),
                            recv: Receiver::SelfField(field.to_string()),
                        });
                    }
                }
                return;
            }
            // `obj.method()` where `obj`'s type is known from an annotation or
            // a constructor call.
            if let Some((recv_name, method)) = t.rsplit_once('.') {
                if let Some(ty) = env.vars.get(recv_name) {
                    out.push(RawCall {
                        path: method.to_string(),
                        recv: Receiver::Typed(ty.clone()),
                    });
                    return;
                }
            }
            // Dotted path `pkg.mod.func` (module-qualified) or `obj.method`
            // (opaque receiver, sorted out at resolution by whether it resolves).
            out.push(RawCall {
                path: t,
                recv: Receiver::Free,
            });
        }
        _ => {}
    }
}

/// "import a.b, c as d" -> imports ["a.b", "c"] plus name bindings for
/// call resolution ("a" -> a, "d" -> c). Never rendered as re-exports.
fn parse_import(t: &str, facts: &mut FileFacts) {
    let t = collapse(t);
    let Some(rest) = t.strip_prefix("import ") else { return };
    for part in rest.split(',') {
        let (real, alias) = match part.split_once(" as ") {
            Some((r, a)) => (r.trim(), Some(a.trim())),
            None => (part.trim(), None),
        };
        if real.is_empty() {
            continue;
        }
        facts.imports.push(real.to_string());
        let (name, path) = match alias {
            Some(a) => (a.to_string(), real.to_string()),
            None => {
                let first = real.split('.').next().unwrap_or(real);
                (first.to_string(), first.to_string())
            }
        };
        facts.reexports.push(Binding {
            name,
            path,
            public: false,
        });
    }
}

/// "from ..core.utils import helper, X as Y" -> imports + name bindings.
/// Every module-level from-import binds a name others can import through;
/// public visibility (rendered as a re-export) is decided by the caller
/// based on whether the file is an __init__.py.
fn parse_from_import(t: &str, facts: &mut FileFacts) {
    let t = collapse(t).replace(['(', ')'], "");
    let Some(rest) = t.strip_prefix("from ") else { return };
    let Some((module, names)) = rest.split_once(" import ") else { return };
    let module = module.trim();
    for name in names.split(',') {
        let (real, alias) = match name.split_once(" as ") {
            Some((r, a)) => (r.trim(), Some(a.trim())),
            None => (name.trim(), None),
        };
        if real.is_empty() {
            continue;
        }
        if real == "*" {
            facts.imports.push(module.to_string());
            facts.reexports.push(Binding {
                name: "*".to_string(),
                path: module.to_string(),
                public: true,
            });
            continue;
        }
        let full = if module.ends_with('.') {
            format!("{module}{real}")
        } else {
            format!("{module}.{real}")
        };
        facts.imports.push(full.clone());
        facts.reexports.push(Binding {
            name: alias.unwrap_or(real).to_string(),
            path: full,
            public: true,
        });
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_of(src: &str) -> Option<String> {
        extract(src).unwrap().items.into_iter().next().and_then(|i| i.doc)
    }

    #[test]
    fn function_docstring_first_line() {
        let src = "def foo():\n    \"\"\"Summary line.\n\n    More detail.\n    \"\"\"\n    return 1\n";
        assert_eq!(doc_of(src).as_deref(), Some("Summary line."));
    }

    #[test]
    fn class_docstring_single_quotes() {
        assert_eq!(
            doc_of("class C:\n    'One liner.'\n    x = 1\n").as_deref(),
            Some("One liner.")
        );
    }

    #[test]
    fn raw_prefixed_docstring() {
        assert_eq!(
            doc_of("def f():\n    r\"\"\"Raw doc.\"\"\"\n    pass\n").as_deref(),
            Some("Raw doc.")
        );
    }

    #[test]
    fn no_docstring_is_none() {
        assert_eq!(doc_of("def f():\n    return 1\n"), None);
    }
}
