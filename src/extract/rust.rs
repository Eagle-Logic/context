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

fn function(node: Node, src: &str) -> Item {
    let (sig, raw_calls) = match node.child_by_field_name("body") {
        Some(body) => (head_before(node, body, src), collect_calls(body, src)),
        None => (
            collapse(text(node, src)).trim_end_matches(';').to_string(),
            Vec::new(),
        ),
    };
    let mut it = item("fn", sig, node, src, Vec::new(), def_name(node, src));
    it.raw_calls = raw_calls;
    it.arity = arity(node);
    it
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
                clip_structure(&head, &fields)
            }
        }
        // Tuple structs / unit structs: the whole declaration is the signature.
        _ => clip(collapse(text(node, src)).trim_end_matches(';')),
    };
    item("struct", sig, node, src, Vec::new(), def_name(node, src))
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
            .map(|t| strip_generics(&collapse(text(t, src))));
        ("impl", name)
    };
    match node.child_by_field_name("body") {
        Some(body) => {
            let mut sub = Vec::new();
            visit(body, src, &mut sub, facts, false);
            item(kind, head_before(node, body, src), node, src, sub, name)
        }
        None => item(kind, collapse(text(node, src)), node, src, Vec::new(), name),
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
            recv: Receiver::Free,
        }),
        "scoped_identifier" => {
            let t = strip_turbofish(&collapse(text(f, src)));
            if t.starts_with('<') {
                return; // <T as Trait>::f — unresolvable without type info
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
                push_callee(inner, src, out);
            }
        }
        "field_expression" => {
            if let Some(field) = f.child_by_field_name("field") {
                // `self.method()` is reliably the enclosing impl; any other
                // receiver (`expr.method()`) has an unknown type.
                let recv = match f.child_by_field_name("value") {
                    Some(v) if text(v, src) == "self" => Receiver::SelfType,
                    _ => Receiver::Unknown,
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
        doc: doc_comment(node, src),
        calls: Vec::new(),
        children,
        arity: None,
        name,
        raw_calls: Vec::new(),
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

    #[test]
    fn nested_brace_use_expands_without_inventing_paths() {
        let paths: Vec<String> = expand_use_tree("crate::{a::{alpha, gamma}, b::beta}");
        assert_eq!(paths, ["crate::a::alpha", "crate::a::gamma", "crate::b::beta"]);
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
        assert!(s.contains("more }"), "a clipped member list must say what is missing: {s}");
        assert!(!s.ends_with('…'), "a bare ellipsis reads as a complete list");
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
