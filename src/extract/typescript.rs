use std::collections::{BTreeMap, HashMap};

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
    let tree = parser
        .parse(src, None)
        .context("tree-sitter parse failed")?;

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
/// Only declaration bodies were scanned, so top-level code — the norm in TS
/// entry points, `index.ts` wiring and component modules — was invisible to
/// `callers`. Being unnamed, it reports the module itself as the caller and
/// never pollutes `def` lookups.
fn module_level_item(root: Node, src: &str) -> Option<Item> {
    let mut cursor = root.walk();
    let mut raw_calls = Vec::new();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "import_statement"
            || child.kind() == "export_statement"
            || DECL_KINDS.contains(&child.kind())
        {
            continue;
        }
        raw_calls.extend(collect_calls(child, src, &TypeEnv::default()));
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
            k if DECL_KINDS.contains(&k) => definition(child, child, src, items, facts, false),
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
            let env = fn_env(decl, src);
            let calls = body
                .map(|b| collect_calls(b, src, &env))
                .unwrap_or_default();
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
            (
                "interface",
                interface_sig(decl, body, src),
                Vec::new(),
                Vec::new(),
            )
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
    if matches!(kind, "class" | "interface") {
        it.implements = heritage(decl, src);
        if let Some(b) = decl.child_by_field_name("body") {
            it.field_types = property_types(b, src);
        }
    }
    items.push(it);
}

/// `class A extends B implements C, D` / `interface A extends B` — the
/// abstractions a call may dispatch through.
fn heritage(decl: Node, src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut c = decl.walk();
    for ch in decl.named_children(&mut c) {
        if !matches!(
            ch.kind(),
            "class_heritage" | "extends_type_clause" | "implements_clause" | "extends_clause"
        ) {
            continue;
        }
        let mut h = ch.walk();
        let mut nodes: Vec<Node> = ch.named_children(&mut h).collect();
        if nodes.is_empty() {
            nodes = vec![ch];
        }
        for n in nodes {
            let mut hh = n.walk();
            let inner: Vec<Node> = n.named_children(&mut hh).collect();
            let targets = if inner.is_empty() { vec![n] } else { inner };
            for t in targets {
                if let Some(name) = type_name(&collapse(text(t, src))) {
                    out.push(name);
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Declared property types of a class body: `private engine: Engine`.
fn property_types(body: Node, src: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut c = body.walk();
    for m in body.named_children(&mut c) {
        if !matches!(m.kind(), "public_field_definition" | "property_signature") {
            continue;
        }
        let (Some(n), Some(t)) = (m.child_by_field_name("name"), m.child_by_field_name("type"))
        else {
            continue;
        };
        if let Some(ty) = type_name(&collapse(text(t, src))) {
            out.insert(text(n, src).to_string(), ty);
        }
    }
    out
}

/// The bare class/interface name a TypeScript type annotation points at, or
/// None when it carries no dispatch information (`string`, `Foo[]`, unions).
fn type_name(raw: &str) -> Option<String> {
    let t = raw.trim().trim_start_matches(':').trim();
    let t = t.strip_prefix("readonly ").unwrap_or(t).trim();
    // `Promise<Foo>` / `Array<Foo>` wrap a value; the receiver is the wrapper.
    let base = t
        .split(['<', '|', '&', '[', '('])
        .next()
        .unwrap_or(t)
        .trim();
    let base = base.rsplit('.').next().unwrap_or(base).trim();
    if base.is_empty() || !base.chars().next().is_some_and(char::is_uppercase) {
        return None;
    }
    Some(base.to_string())
}

/// Receiver types ctx can name inside one TypeScript body.
#[derive(Default, Clone)]
struct TypeEnv {
    vars: HashMap<String, String>,
}

fn fn_env(node: Node, src: &str) -> TypeEnv {
    let mut env = TypeEnv::default();
    let Some(params) = node.child_by_field_name("parameters") else {
        return env;
    };
    let mut c = params.walk();
    for p in params.named_children(&mut c) {
        let (Some(pat), Some(ty)) = (
            p.child_by_field_name("pattern"),
            p.child_by_field_name("type"),
        ) else {
            continue;
        };
        let name = text(pat, src).trim().to_string();
        if name.is_empty() || name.contains(['{', '[']) {
            continue;
        }
        if let Some(t) = type_name(&collapse(text(ty, src))) {
            env.vars.insert(name, t);
        }
    }
    env
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
        let is_fn = value.is_some_and(|v| {
            matches!(
                v.kind(),
                "arrow_function" | "function_expression" | "function"
            )
        });

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
            let env = fn_env(v, src);
            let calls = body
                .map(|b| collect_calls(b, src, &env))
                .unwrap_or_default();
            ("fn", format!("const {name} = {params} =>"), calls)
        } else {
            ("const", format!("const {name}"), Vec::new())
        };
        let sig = if exported {
            format!("export {sig}")
        } else {
            sig
        };
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
        let env = fn_env(m, src);
        let calls = m
            .child_by_field_name("body")
            .map(|b| collect_calls(b, src, &env))
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
        .map(|c| {
            collapse(text(c, src))
                .trim_end_matches([';', ','])
                .to_string()
        })
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

/// Collect call sites in a body: `f()` (free), `this.m()` (self),
/// `this.field.m()` (property type), and `obj.m()` (typed if a parameter
/// annotation, `let x: Foo`, or `new Foo()` says what `obj` is).
fn collect_calls(body: Node, src: &str, env: &TypeEnv) -> Vec<RawCall> {
    let mut env = env.clone();
    let mut call_nodes: Vec<Node> = Vec::new();
    let mut stack = vec![body];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "variable_declarator" => {
                if let Some((name, ty)) = declarator_binding(n, src) {
                    env.vars.insert(name, ty);
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
    call_nodes.reverse();
    let mut out = Vec::new();
    for n in call_nodes {
        let Some(f) = n.child_by_field_name("function") else {
            continue;
        };
        match f.kind() {
            "identifier" => out.push(RawCall {
                path: text(f, src).to_string(),
                recv: Receiver::Free,
            }),
            "member_expression" => {
                let Some(prop) = f.child_by_field_name("property") else {
                    continue;
                };
                let recv = match f.child_by_field_name("object") {
                    Some(o) if o.kind() == "this" => Receiver::SelfType,
                    // `this.field.m()`
                    Some(o) if o.kind() == "member_expression" => {
                        match (
                            o.child_by_field_name("object"),
                            o.child_by_field_name("property"),
                        ) {
                            (Some(inner), Some(fld)) if inner.kind() == "this" => {
                                Receiver::SelfField(text(fld, src).to_string())
                            }
                            _ => Receiver::Unknown,
                        }
                    }
                    Some(o) if o.kind() == "identifier" => env
                        .vars
                        .get(text(o, src))
                        .cloned()
                        .map_or(Receiver::Unknown, Receiver::Typed),
                    _ => Receiver::Unknown,
                };
                out.push(RawCall {
                    path: text(prop, src).to_string(),
                    recv,
                });
            }
            _ => {}
        }
    }
    out
}

/// `const x: Foo = ...` / `const x = new Foo()` — the declarator forms that
/// name a type.
fn declarator_binding(n: Node, src: &str) -> Option<(String, String)> {
    let name = field_text(n, "name", src)?;
    if let Some(ty) = n.child_by_field_name("type") {
        if let Some(t) = type_name(&collapse(text(ty, src))) {
            return Some((name, t));
        }
    }
    let value = n.child_by_field_name("value")?;
    if value.kind() != "new_expression" {
        return None;
    }
    let ctor = value.child_by_field_name("constructor")?;
    type_name(&collapse(text(ctor, src))).map(|t| (name, t))
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
            let cleaned = rest.trim().trim_matches(|c: char| {
                matches!(c, '-' | '=' | '*' | '#' | '/') || c.is_whitespace()
            });
            if !cleaned.is_empty() {
                lines.push(cleaned.to_string());
            }
        } else {
            break; // plain /* */ block: not a doc comment
        }
        sib = s.prev_sibling();
    }
    lines.reverse();
    lines
        .into_iter()
        .find(|l| !l.is_empty())
        .map(|l| clip_doc(&l))
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
        implements: Vec::new(),
        field_types: BTreeMap::new(),
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
        assert!(
            it[0].signature.starts_with("export function foo"),
            "{}",
            it[0].signature
        );
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
        assert!(
            it[0].signature.contains("name: string"),
            "{}",
            it[0].signature
        );
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
