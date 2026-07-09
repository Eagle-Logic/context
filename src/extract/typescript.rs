use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};

use crate::model::{Binding, FileFacts, Item, RawCall, Receiver};

/// Extract a TypeScript (or TSX) source file. Module resolution treats import
/// specifiers as paths (`./x`, `../y`), so bindings store `specifier/symbol`
/// and the resolver's TS branch turns them into module edges.
pub fn extract(src: &str, tsx: bool) -> Result<FileFacts> {
    let mut parser = Parser::new();
    let lang = if tsx {
        tree_sitter_typescript::LANGUAGE_TSX
    } else {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT
    };
    parser
        .set_language(&lang.into())
        .context("loading typescript grammar")?;
    let tree = parser.parse(src, None).context("tree-sitter parse failed")?;

    let mut facts = FileFacts::default();
    let mut items = Vec::new();
    visit(tree.root_node(), src, &mut items, &mut facts);
    facts.items = items;
    Ok(facts)
}

const DECL_KINDS: &[&str] = &[
    "function_declaration",
    "class_declaration",
    "abstract_class_declaration",
    "interface_declaration",
    "type_alias_declaration",
    "enum_declaration",
];

fn visit(node: Node, src: &str, items: &mut Vec<Item>, facts: &mut FileFacts) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "import_statement" => parse_import(child, src, facts),
            "export_statement" => parse_export(child, src, items, facts),
            k if DECL_KINDS.contains(&k) => {
                definition(child, child, src, items, facts, false)
            }
            "lexical_declaration" | "variable_declaration" => {
                lexical(child, child, src, items, facts, false)
            }
            _ => {}
        }
    }
}

/// `export ...` — either a declaration (`export function f`), a re-export
/// (`export { x } from './y'`, `export * from './y'`), or a local re-export
/// (`export { x }`, which is already a defined name — skipped).
fn parse_export(node: Node, src: &str, items: &mut Vec<Item>, facts: &mut FileFacts) {
    let mut cursor = node.walk();
    let children: Vec<Node> = node.named_children(&mut cursor).collect();

    if let Some(decl) = children.iter().find(|c| DECL_KINDS.contains(&c.kind())) {
        definition(*decl, node, src, items, facts, true);
        return;
    }
    if let Some(lex) = children
        .iter()
        .find(|c| matches!(c.kind(), "lexical_declaration" | "variable_declaration"))
    {
        lexical(*lex, node, src, items, facts, true);
        return;
    }

    // Re-export forms carry a `string` source.
    let Some(source) = children.iter().find(|c| c.kind() == "string") else {
        return;
    };
    let spec = string_text(*source, src);
    if let Some(clause) = children.iter().find(|c| c.kind() == "export_clause") {
        let mut c2 = clause.walk();
        for es in clause.named_children(&mut c2) {
            if es.kind() == "export_specifier" {
                let (name, alias) = spec_names(es, src);
                let bind = alias.unwrap_or_else(|| name.clone());
                facts.imports.push(format!("{spec}/{name}"));
                facts.reexports.push(Binding {
                    name: bind,
                    path: format!("{spec}/{name}"),
                    public: true,
                });
            }
        }
    } else {
        // `export * from './x'`
        facts.imports.push(spec.clone());
        facts.reexports.push(Binding {
            name: "*".to_string(),
            path: spec,
            public: true,
        });
    }
}

fn definition(
    decl: Node,
    outer: Node,
    src: &str,
    items: &mut Vec<Item>,
    facts: &mut FileFacts,
    exported: bool,
) {
    let Some(name) = field_text(decl, "name", src) else {
        return;
    };
    facts.defined.insert(name.clone());

    let (kind, signature, children, raw_calls) = match decl.kind() {
        "function_declaration" => {
            let body = decl.child_by_field_name("body");
            let sig = head(decl, body, src);
            let calls = body.map(|b| collect_calls(b, src)).unwrap_or_default();
            ("fn", sig, Vec::new(), calls)
        }
        "class_declaration" | "abstract_class_declaration" => {
            let body = decl.child_by_field_name("body");
            let sig = head(decl, body, src);
            let members = body.map(|b| class_members(b, src)).unwrap_or_default();
            ("class", sig, members, Vec::new())
        }
        "interface_declaration" => {
            let body = decl.child_by_field_name("body");
            ("interface", interface_sig(decl, body, src), Vec::new(), Vec::new())
        }
        "enum_declaration" => {
            let body = decl.child_by_field_name("body");
            ("enum", enum_sig(decl, body, src), Vec::new(), Vec::new())
        }
        "type_alias_declaration" => (
            "type",
            clip(collapse(text(decl, src)).trim_end_matches(';')),
            Vec::new(),
            Vec::new(),
        ),
        _ => return,
    };

    let signature = if exported {
        format!("export {signature}")
    } else {
        signature
    };
    let mut it = mk(kind, signature, outer, src, children, Some(name));
    it.raw_calls = raw_calls;
    if kind == "fn" {
        it.arity = arity_from(decl);
    }
    items.push(it);
}

/// `const Name = (…) => {…}` / `export const X = …`. Arrow/function values
/// become callable `fn` items; other exported consts become `const` items.
fn lexical(
    lex: Node,
    outer: Node,
    src: &str,
    items: &mut Vec<Item>,
    facts: &mut FileFacts,
    exported: bool,
) {
    let mut cursor = lex.walk();
    for decl in lex.named_children(&mut cursor) {
        if decl.kind() != "variable_declarator" {
            continue;
        }
        let Some(name) = field_text(decl, "name", src) else {
            continue;
        };
        let value = decl.child_by_field_name("value");
        let is_fn = value.is_some_and(|v| matches!(v.kind(), "arrow_function" | "function_expression" | "function"));

        // Skip local non-callable consts — low architectural signal.
        if !exported && !is_fn {
            continue;
        }
        facts.defined.insert(name.clone());

        let (kind, sig, calls) = if is_fn {
            let v = value.unwrap();
            let body = v.child_by_field_name("body");
            let params = v
                .child_by_field_name("parameters")
                .map(|p| collapse(text(p, src)))
                .unwrap_or_else(|| "()".to_string());
            let calls = body.map(|b| collect_calls(b, src)).unwrap_or_default();
            ("fn", format!("const {name} = {params} =>"), calls)
        } else {
            ("const", format!("const {name}"), Vec::new())
        };
        let sig = if exported { format!("export {sig}") } else { sig };
        let mut it = mk(kind, clip(&sig), outer, src, Vec::new(), Some(name));
        it.raw_calls = calls;
        if is_fn {
            it.arity = value.and_then(arity_from);
        }
        items.push(it);
    }
}

/// Public methods of a class body (private/protected/`#` members dropped).
fn class_members(body: Node, src: &str) -> Vec<Item> {
    let mut out = Vec::new();
    let mut cursor = body.walk();
    for m in body.named_children(&mut cursor) {
        if m.kind() != "method_definition" {
            continue;
        }
        let head_txt = head(m, m.child_by_field_name("body"), src);
        if head_txt.starts_with("private") || head_txt.starts_with("protected") {
            continue;
        }
        let Some(name) = field_text(m, "name", src) else {
            continue;
        };
        if name.starts_with('#') {
            continue;
        }
        let calls = m
            .child_by_field_name("body")
            .map(|b| collect_calls(b, src))
            .unwrap_or_default();
        let mut it = mk("fn", head_txt, m, src, Vec::new(), Some(name));
        it.raw_calls = calls;
        it.arity = arity_from(m);
        out.push(it);
    }
    out
}

fn interface_sig(decl: Node, body: Option<Node>, src: &str) -> String {
    let head = head(decl, body, src);
    let Some(body) = body else { return head };
    let mut cursor = body.walk();
    let fields: Vec<String> = body
        .named_children(&mut cursor)
        .filter(|c| c.kind() == "property_signature")
        .map(|c| collapse(text(c, src)).trim_end_matches([';', ',']).to_string())
        .collect();
    if fields.is_empty() {
        head
    } else {
        clip(&format!("{head} {{ {} }}", fields.join(", ")))
    }
}

fn enum_sig(decl: Node, body: Option<Node>, src: &str) -> String {
    let head = head(decl, body, src);
    let Some(body) = body else { return head };
    let mut cursor = body.walk();
    let variants: Vec<String> = body
        .named_children(&mut cursor)
        .filter_map(|c| match c.kind() {
            "property_identifier" => Some(text(c, src).to_string()),
            "enum_assignment" => field_text(c, "name", src),
            _ => None,
        })
        .collect();
    if variants.is_empty() {
        head
    } else {
        clip(&format!("{head} {{ {} }}", variants.join(" | ")))
    }
}

/// Collect call sites in a body: `f()` (free), `this.m()` (self), and
/// `obj.m()` (opaque receiver).
fn collect_calls(body: Node, src: &str) -> Vec<RawCall> {
    let mut out = Vec::new();
    let mut stack = vec![body];
    while let Some(n) = stack.pop() {
        if n.kind() == "call_expression" {
            if let Some(f) = n.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => out.push(RawCall {
                        path: text(f, src).to_string(),
                        recv: Receiver::Free,
                    }),
                    "member_expression" => {
                        if let Some(prop) = f.child_by_field_name("property") {
                            let is_self = f
                                .child_by_field_name("object")
                                .is_some_and(|o| o.kind() == "this");
                            out.push(RawCall {
                                path: text(prop, src).to_string(),
                                recv: if is_self {
                                    Receiver::SelfType
                                } else {
                                    Receiver::Unknown
                                },
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

/// `import D, { a, b as c } from './x'` / `import * as ns from './x'` /
/// `import './x'`. Each imported name is pushed as a `specifier/name` path
/// (for a module dep edge) plus a binding (for call resolution).
fn parse_import(node: Node, src: &str, facts: &mut FileFacts) {
    let mut cursor = node.walk();
    let children: Vec<Node> = node.named_children(&mut cursor).collect();
    let Some(source) = children.iter().find(|c| c.kind() == "string") else {
        return;
    };
    let spec = string_text(*source, src);

    let Some(clause) = children.iter().find(|c| c.kind() == "import_clause") else {
        // Side-effect import `import './x'`.
        facts.imports.push(spec);
        return;
    };

    let mut cc = clause.walk();
    let mut named_any = false;
    for part in clause.named_children(&mut cc) {
        match part.kind() {
            // Default import: `import D from './x'`.
            "identifier" => {
                facts.imports.push(spec.clone());
                facts.reexports.push(Binding {
                    name: text(part, src).to_string(),
                    path: spec.clone(),
                    public: false,
                });
            }
            // Namespace: `import * as ns from './x'`.
            "namespace_import" => {
                if let Some(id) = part.named_children(&mut part.walk()).next() {
                    facts.imports.push(spec.clone());
                    facts.reexports.push(Binding {
                        name: text(id, src).to_string(),
                        path: spec.clone(),
                        public: false,
                    });
                }
            }
            "named_imports" => {
                named_any = true;
                let mut ni = part.walk();
                for isp in part.named_children(&mut ni) {
                    if isp.kind() == "import_specifier" {
                        let (name, alias) = spec_names(isp, src);
                        let bind = alias.unwrap_or_else(|| name.clone());
                        let path = format!("{spec}/{name}");
                        facts.imports.push(path.clone());
                        facts.reexports.push(Binding {
                            name: bind,
                            path,
                            public: false,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    if !named_any && children.len() == 1 {
        facts.imports.push(spec);
    }
}

// ---- doc comments ----------------------------------------------------------

/// First line of the JSDoc (`/** */`) or `//` comment run preceding an item.
fn doc_comment(node: Node, src: &str) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut sib = node.prev_sibling();
    while let Some(s) = sib {
        if s.kind() != "comment" {
            break;
        }
        let t = text(s, src).trim();
        if let Some(rest) = t.strip_prefix("/**") {
            // Self-contained block doc: take its first meaningful line, stop.
            let body = rest.trim_end_matches("*/");
            lines.push(
                body.lines()
                    .map(|l| l.trim().trim_start_matches('*').trim().to_string())
                    .find(|l| !l.is_empty())
                    .unwrap_or_default(),
            );
            break;
        } else if let Some(rest) = t.strip_prefix("//") {
            // Strip decorative banners (`// ----- Foo`, `//////`); a line that
            // is only separators contributes nothing.
            let cleaned = rest
                .trim()
                .trim_matches(|c: char| matches!(c, '-' | '=' | '*' | '#' | '/') || c.is_whitespace());
            if !cleaned.is_empty() {
                lines.push(cleaned.to_string());
            }
        } else {
            break; // plain /* */ block: not a doc comment
        }
        sib = s.prev_sibling();
    }
    lines.reverse();
    lines.into_iter().find(|l| !l.is_empty()).map(|l| clip_doc(&l))
}

// ---- helpers ---------------------------------------------------------------

/// (imported name, optional alias) for an import/export specifier node.
fn spec_names(node: Node, src: &str) -> (String, Option<String>) {
    let mut cursor = node.walk();
    let ids: Vec<Node> = node
        .named_children(&mut cursor)
        .filter(|c| matches!(c.kind(), "identifier" | "type_identifier"))
        .collect();
    match ids.as_slice() {
        [name] => (text(*name, src).to_string(), None),
        [name, alias, ..] => (
            text(*name, src).to_string(),
            Some(text(*alias, src).to_string()),
        ),
        [] => (String::new(), None),
    }
}

fn field_text(node: Node, field: &str, src: &str) -> Option<String> {
    node.child_by_field_name(field)
        .map(|n| text(n, src).to_string())
}

/// Text of a `string` node with its surrounding quotes stripped.
fn string_text(node: Node, src: &str) -> String {
    let mut cursor = node.walk();
    if let Some(frag) = node
        .named_children(&mut cursor)
        .find(|c| c.kind() == "string_fragment")
    {
        return text(frag, src).to_string();
    }
    text(node, src)
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .to_string()
}

fn head(node: Node, body: Option<Node>, src: &str) -> String {
    match body {
        Some(b) => collapse(&src[node.start_byte()..b.start_byte()]),
        None => clip(collapse(text(node, src)).trim_end_matches(';')),
    }
}

fn mk(
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

/// Count of parameters in a `formal_parameters` / `parameters` node reachable
/// from `n` (TS has no `self` receiver). None if no param list is found.
fn arity_from(n: Node) -> Option<usize> {
    let params = n
        .child_by_field_name("parameters")
        .or_else(|| child_of_kind(n, "formal_parameters"))?;
    let mut cursor = params.walk();
    Some(
        params
            .named_children(&mut cursor)
            .filter(|c| c.kind() != "comment")
            .count(),
    )
}

fn child_of_kind<'a>(n: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = n.walk();
    let kids: Vec<Node<'a>> = n.named_children(&mut cursor).collect();
    kids.into_iter().find(|c| c.kind() == kind)
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

    fn items(src: &str) -> Vec<Item> {
        extract(src, true).unwrap().items
    }

    #[test]
    fn exported_function_with_jsdoc() {
        let it = items("/** Does a thing. */\nexport function foo(a: number): void {}\n");
        assert_eq!(it.len(), 1);
        assert_eq!(it[0].kind, "fn");
        assert!(it[0].signature.starts_with("export function foo"), "{}", it[0].signature);
        assert_eq!(it[0].doc.as_deref(), Some("Does a thing."));
    }

    #[test]
    fn exported_arrow_const_is_a_fn() {
        let it = items("export const Widget = (p: P) => { return 1; };\n");
        assert_eq!(it[0].kind, "fn");
        assert!(it[0].signature.contains("const Widget"));
    }

    #[test]
    fn interface_inlines_fields() {
        let it = items("export interface Props { name: string; count: number }\n");
        assert_eq!(it[0].kind, "interface");
        assert!(it[0].signature.contains("name: string"), "{}", it[0].signature);
    }

    #[test]
    fn non_exported_top_level_const_is_skipped() {
        // A plain non-exported value carries no architectural signal.
        assert!(items("const internal = 3;\n").is_empty());
    }

    #[test]
    fn pure_banner_comment_is_not_a_doc() {
        let it = items("// --------------------\nexport function foo() {}\n");
        assert_eq!(it[0].doc, None);
    }

    #[test]
    fn defined_names_collected() {
        let facts = extract("export function foo() {}\nexport class Bar {}\n", true).unwrap();
        assert!(facts.defined.contains("foo"));
        assert!(facts.defined.contains("Bar"));
    }
}
