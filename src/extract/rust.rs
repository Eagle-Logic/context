use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};

use crate::model::{Binding, FileFacts, Item, RawCall};

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
            "function_item" | "function_signature_item" => items.push(function(child, src)),
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
                        sub,
                        def_name(child, src),
                    ));
                }
            }
            "type_item" | "associated_type" | "const_item" | "static_item" => {
                let sig = clip(collapse(text(child, src)).trim_end_matches(';'));
                items.push(item(child.kind(), sig, child, Vec::new(), def_name(child, src)));
            }
            "macro_definition" => {
                let name = def_name(child, src);
                let sig = format!("macro_rules! {}", name.as_deref().unwrap_or("?"));
                items.push(item("macro", sig, child, Vec::new(), name));
            }
            _ => {}
        }
    }
}

fn function(node: Node, src: &str) -> Item {
    let (sig, raw_calls) = match node.child_by_field_name("body") {
        Some(body) => (head_before(node, body, src), collect_calls(body, src)),
        None => (
            collapse(text(node, src)).trim_end_matches(';').to_string(),
            Vec::new(),
        ),
    };
    let mut it = item("fn", sig, node, Vec::new(), def_name(node, src));
    it.raw_calls = raw_calls;
    it
}

fn structure(node: Node, src: &str) -> Item {
    let sig = match node.child_by_field_name("body") {
        Some(body) if body.kind() == "field_declaration_list" => {
            let head = head_before(node, body, src);
            let mut cursor = body.walk();
            let fields: Vec<String> = body
                .named_children(&mut cursor)
                .filter(|c| c.kind() == "field_declaration")
                .map(|c| collapse(text(c, src)))
                .collect();
            if fields.is_empty() {
                head
            } else {
                clip(&format!("{} {{ {} }}", head, fields.join(", ")))
            }
        }
        // Tuple structs / unit structs: the whole declaration is the signature.
        _ => clip(collapse(text(node, src)).trim_end_matches(';')),
    };
    item("struct", sig, node, Vec::new(), def_name(node, src))
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
    item("enum", sig, node, Vec::new(), def_name(node, src))
}

fn container(node: Node, src: &str, facts: &mut FileFacts) -> Item {
    let (kind, name) = if node.kind() == "trait_item" {
        ("trait", def_name(node, src))
    } else {
        // impl blocks: the implementing type is the container name.
        let name = node
            .child_by_field_name("type")
            .map(|t| strip_generics(&collapse(text(t, src))));
        ("impl", name)
    };
    match node.child_by_field_name("body") {
        Some(body) => {
            let mut sub = Vec::new();
            visit(body, src, &mut sub, facts, false);
            item(kind, head_before(node, body, src), node, sub, name)
        }
        None => item(kind, collapse(text(node, src)), node, Vec::new(), name),
    }
}

/// Walk a function body collecting every call site.
fn collect_calls(body: Node, src: &str) -> Vec<RawCall> {
    let mut out = Vec::new();
    let mut stack = vec![body];
    while let Some(n) = stack.pop() {
        if n.kind() == "call_expression" {
            if let Some(f) = n.child_by_field_name("function") {
                push_callee(f, src, &mut out);
            }
        }
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            stack.push(ch);
        }
    }
    out
}

fn push_callee(f: Node, src: &str, out: &mut Vec<RawCall>) {
    match f.kind() {
        "identifier" => out.push(RawCall {
            path: text(f, src).to_string(),
            method: false,
        }),
        "scoped_identifier" => {
            let t = strip_turbofish(&collapse(text(f, src)));
            if t.starts_with('<') {
                return; // <T as Trait>::f — unresolvable without type info
            }
            if let Some(rest) = t.strip_prefix("Self::") {
                out.push(RawCall {
                    path: rest.to_string(),
                    method: true,
                });
            } else {
                out.push(RawCall {
                    path: t,
                    method: false,
                });
            }
        }
        "generic_function" => {
            if let Some(inner) = f.child_by_field_name("function") {
                push_callee(inner, src, out);
            }
        }
        "field_expression" => {
            if let Some(field) = f.child_by_field_name("field") {
                out.push(RawCall {
                    path: text(field, src).to_string(),
                    method: true,
                });
            }
        }
        _ => {}
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

fn item(kind: &str, signature: String, node: Node, children: Vec<Item>, name: Option<String>) -> Item {
    Item {
        kind: kind.to_string(),
        signature,
        line: node.start_position().row + 1,
        calls: Vec::new(),
        children,
        name,
        raw_calls: Vec::new(),
    }
}
