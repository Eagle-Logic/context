use std::collections::BTreeSet;

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
        raw_calls.extend(collect_calls(child, src));
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
    if is_class {
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
        }
    } else if let Some(b) = body {
        raw_calls = collect_calls(b, src);
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
    });
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

/// Walk a function body collecting every call site.
fn collect_calls(body: Node, src: &str) -> Vec<RawCall> {
    let mut out = Vec::new();
    let mut stack = vec![body];
    while let Some(n) = stack.pop() {
        if n.kind() == "call" {
            if let Some(f) = n.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => out.push(RawCall {
                        path: text(f, src).to_string(),
                        recv: Receiver::Free,
                    }),
                    "attribute" => {
                        let t = collapse(text(f, src));
                        if let Some(rest) =
                            t.strip_prefix("self.").or_else(|| t.strip_prefix("cls."))
                        {
                            // Only direct self.method(); self.obj.method() has
                            // an unknowable receiver type.
                            if !rest.contains('.') {
                                out.push(RawCall {
                                    path: rest.to_string(),
                                    recv: Receiver::SelfType,
                                });
                            }
                        } else if !t.contains('(') {
                            // Dotted path `pkg.mod.func` (module-qualified) or
                            // `obj.method` (opaque receiver, sorted out at
                            // resolution by whether the path resolves).
                            out.push(RawCall {
                                path: t,
                                recv: Receiver::Free,
                            });
                        }
                    }
                    _ => {}
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
