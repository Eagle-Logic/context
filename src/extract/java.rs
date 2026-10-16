//! Java extraction.
//!
//! Java is the first language here whose module identity is declared *inside*
//! the file. Rust, Python and TypeScript derive a module name from the path;
//! Go derives it from the directory plus `go.mod`. Java states it: `package
//! com.foo;` plus the type name is the fully-qualified name every import
//! spells, and the path only mirrors that by Maven convention. So
//! [`FileFacts::declared_module`] carries it and the path is not trusted —
//! a multi-module build (`service-a/src/main/java/com/foo/Bar.java`) would
//! otherwise produce a module name no import can match.
//!
//! Receivers are unusually easy here, which is where the recall comes from:
//! Java requires a declared type on every field, parameter and local, so
//! `Typed` is available almost everywhere `Unknown` would be in Python.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};

use super::span_of;
use crate::model::{Binding, FileFacts, Item, RawCall, Receiver};

/// Types whose methods tell you nothing about topology, so a receiver of this
/// type is not worth an edge. Mirrors the `TRANSPARENT` list in `rust.rs`.
const TRANSPARENT: &[&str] = &[
    "String",
    "CharSequence",
    "Object",
    "Integer",
    "Long",
    "Double",
    "Float",
    "Boolean",
    "Byte",
    "Short",
    "Character",
    "List",
    "ArrayList",
    "Map",
    "HashMap",
    "Set",
    "HashSet",
    "Optional",
    "Stream",
    "Collection",
    "Iterable",
    "Iterator",
    "StringBuilder",
];

pub fn extract(src: &str) -> Result<FileFacts> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .context("loading java grammar")?;
    let tree = parser
        .parse(src, None)
        .context("tree-sitter parse failed")?;

    let mut facts = FileFacts::default();
    let mut items = Vec::new();
    let root = tree.root_node();

    let mut package: Option<Vec<String>> = None;
    for i in 0..root.named_child_count() as u32 {
        let Some(child) = root.named_child(i) else {
            continue;
        };
        match child.kind() {
            "package_declaration" => {
                package = scoped_name(child, src).map(|p| {
                    p.split('.')
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                });
            }
            // `import a.b.C;` binds `C`. `import static a.b.C.m;` binds the
            // member, and `import a.b.*;` binds nothing nameable — it is kept
            // as an import so the dependency is visible, with "*" as the bound
            // name the way the other extractors spell a glob.
            "import_declaration" => {
                if let Some(path) = scoped_name(child, src) {
                    let wildcard = text(child, src).contains('*');
                    let bound = if wildcard {
                        "*".to_string()
                    } else {
                        path.rsplit('.').next().unwrap_or(&path).to_string()
                    };
                    facts.imports.push(path.clone());
                    facts.reexports.push(Binding {
                        name: bound,
                        path,
                        // Java has no re-export: an import is private to the
                        // compilation unit. Recorded so local calls resolve,
                        // never rendered as a facade.
                        public: false,
                    });
                }
            }
            _ => {
                if let Some(it) = type_declaration(child, src, &mut facts) {
                    items.push(it);
                }
            }
        }
    }
    facts.items = items;
    facts.declared_module = package;
    Ok(facts)
}

/// The dotted name a `package`/`import` declaration carries.
fn scoped_name(node: Node, src: &str) -> Option<String> {
    for i in 0..node.named_child_count() as u32 {
        let ch = node.named_child(i)?;
        if matches!(ch.kind(), "scoped_identifier" | "identifier") {
            return Some(text(ch, src).to_string());
        }
    }
    None
}

/// A top-level (or nested) type declaration, with its members as children.
fn type_declaration(node: Node, src: &str, facts: &mut FileFacts) -> Option<Item> {
    let kind = match node.kind() {
        "class_declaration" => "class",
        "interface_declaration" => "interface",
        "enum_declaration" => "enum",
        // A record is a class whose shape is its constructor. `struct` is the
        // closest existing spelling, and the kind strings are matched
        // literally across view/query, so inventing `record` would make it
        // invisible to `--view skeleton`.
        "record_declaration" => "struct",
        "annotation_type_declaration" => "interface",
        _ => return None,
    };
    let name = node
        .child_by_field_name("name")
        .map(|n| text(n, src).to_string())?;
    facts.defined.insert(name.clone());

    let implements = heritage(node, src);
    let fields = field_types(node, src);
    let mut children = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        for i in 0..body.named_child_count() as u32 {
            let Some(m) = body.named_child(i) else {
                continue;
            };
            match m.kind() {
                "method_declaration"
                | "constructor_declaration"
                | "compact_constructor_declaration" => {
                    children.push(method(m, src, &name, &fields));
                }
                "field_declaration" => {
                    if let Some(it) = field(m, src) {
                        children.push(it);
                    }
                }
                _ => {
                    if let Some(it) = type_declaration(m, src, facts) {
                        children.push(it);
                    }
                }
            }
        }
    }

    let mut it = item(kind, head(node, src), node, src, children, Some(name));
    it.implements = implements;
    it.field_types = fields;
    Some(it)
}

/// `extends` / `implements`, which is what `implements` means in the model:
/// the names a dispatch fan-out over this type should consider.
fn heritage(node: Node, src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for i in 0..node.named_child_count() as u32 {
        let Some(ch) = node.named_child(i) else {
            continue;
        };
        match ch.kind() {
            "superclass" | "super_interfaces" | "extends_interfaces" => {
                for j in 0..ch.named_child_count() as u32 {
                    if let Some(t) = ch.named_child(j) {
                        collect_type_names(t, src, &mut out);
                    }
                }
            }
            _ => {}
        }
    }
    out.sort();
    out.dedup();
    out
}

fn collect_type_names(node: Node, src: &str, out: &mut Vec<String>) {
    match node.kind() {
        "type_identifier" => out.push(text(node, src).to_string()),
        "generic_type" | "scoped_type_identifier" => {
            if let Some(n) = node.named_child(0) {
                collect_type_names(n, src, out);
            }
        }
        "type_list" => {
            for i in 0..node.named_child_count() as u32 {
                if let Some(ch) = node.named_child(i) {
                    collect_type_names(ch, src, out);
                }
            }
        }
        _ => {}
    }
}

/// Declared field types, which is what makes `SelfField` resolvable.
fn field_types(node: Node, src: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(body) = node.child_by_field_name("body") else {
        return out;
    };
    for i in 0..body.named_child_count() as u32 {
        let Some(f) = body.named_child(i) else {
            continue;
        };
        if f.kind() != "field_declaration" {
            continue;
        }
        let Some(ty) = f.child_by_field_name("type").map(|t| base_type(t, src)) else {
            continue;
        };
        for j in 0..f.named_child_count() as u32 {
            if let Some(d) = f.named_child(j) {
                if d.kind() == "variable_declarator" {
                    if let Some(n) = d.child_by_field_name("name") {
                        out.insert(text(n, src).to_string(), ty.clone());
                    }
                }
            }
        }
    }
    out
}

/// The outermost named type, with generics and arrays stripped: `List<Foo>` is
/// a `List`, and a receiver typed `List` tells you nothing worth an edge.
fn base_type(node: Node, src: &str) -> String {
    let mut names = Vec::new();
    collect_type_names(node, src, &mut names);
    names
        .into_iter()
        .next()
        .unwrap_or_else(|| text(node, src).trim().to_string())
}

fn field(node: Node, src: &str) -> Option<Item> {
    let name = (0..node.named_child_count() as u32)
        .filter_map(|i| node.named_child(i))
        .find(|c| c.kind() == "variable_declarator")
        .and_then(|d| d.child_by_field_name("name"))
        .map(|n| text(n, src).to_string())?;
    Some(item(
        "var",
        collapse(text(node, src).trim_end_matches(';')),
        node,
        src,
        Vec::new(),
        Some(name),
    ))
}

fn method(node: Node, src: &str, owner: &str, fields: &BTreeMap<String, String>) -> Item {
    let name = node
        .child_by_field_name("name")
        .map(|n| text(n, src).to_string())
        .unwrap_or_else(|| owner.to_string());
    let mut env = TypeEnv::default();
    if let Some(params) = node.child_by_field_name("parameters") {
        for i in 0..params.named_child_count() as u32 {
            let Some(p) = params.named_child(i) else {
                continue;
            };
            let ty = p.child_by_field_name("type").map(|t| base_type(t, src));
            let nm = p
                .child_by_field_name("name")
                .map(|n| text(n, src).to_string());
            if let (Some(ty), Some(nm)) = (ty, nm) {
                env.vars.insert(nm, classify(&ty));
            }
        }
    }
    let mut calls = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        body_facts(body, src, &mut env, owner, fields, &mut calls);
    }
    let arity = node
        .child_by_field_name("parameters")
        .map(|p| p.named_child_count());
    let mut it = item("fn", head(node, src), node, src, Vec::new(), Some(name));
    it.raw_calls = calls;
    it.arity = arity;
    it
}

#[derive(Default, Clone)]
struct TypeEnv {
    vars: HashMap<String, Receiver>,
}

/// A declared type becomes `Typed` unless it is transparent or an obvious
/// abstraction. `Dyn` is reserved for interface-shaped names, which Java spells
/// no differently from classes — so this cannot tell them apart and does not
/// pretend to: `typed_edge` promotes to a dispatch fan-out when the universe
/// knows the type is subtyped, which is evidence rather than a guess.
fn classify(ty: &str) -> Receiver {
    if ty.is_empty() || TRANSPARENT.contains(&ty) {
        Receiver::Unknown
    } else {
        Receiver::Typed(ty.to_string())
    }
}

fn body_facts(
    body: Node,
    src: &str,
    env: &mut TypeEnv,
    owner: &str,
    fields: &BTreeMap<String, String>,
    out: &mut Vec<RawCall>,
) {
    let mut stack = vec![body];
    let mut calls: Vec<Node> = Vec::new();
    // Pass 1: locals, so a declaration below a call still types it.
    while let Some(n) = stack.pop() {
        match n.kind() {
            "local_variable_declaration" => {
                if let Some(ty) = n.child_by_field_name("type").map(|t| base_type(t, src)) {
                    for i in 0..n.named_child_count() as u32 {
                        if let Some(d) = n.named_child(i) {
                            if d.kind() == "variable_declarator" {
                                if let Some(nm) = d.child_by_field_name("name") {
                                    env.vars.insert(text(nm, src).to_string(), classify(&ty));
                                }
                            }
                        }
                    }
                }
            }
            "method_invocation" | "object_creation_expression" => calls.push(n),
            _ => {}
        }
        for i in 0..n.named_child_count() as u32 {
            if let Some(ch) = n.named_child(i) {
                stack.push(ch);
            }
        }
    }
    // Pass 2: resolve receivers against the completed environment.
    for n in calls {
        push_callee(n, src, env, owner, fields, out);
    }
}

fn push_callee(
    node: Node,
    src: &str,
    env: &TypeEnv,
    owner: &str,
    fields: &BTreeMap<String, String>,
    out: &mut Vec<RawCall>,
) {
    let (line, end_line) = span_of(node);
    if node.kind() == "object_creation_expression" {
        // `new Foo()` is a call to Foo's constructor, which this models as a
        // free call on the type name — the same shape `rust.rs` uses for
        // `Foo::new`.
        if let Some(t) = node.child_by_field_name("type") {
            let ty = base_type(t, src);
            if !ty.is_empty() {
                out.push(RawCall {
                    path: ty,
                    recv: Receiver::Free,
                    line,
                    end_line,
                });
            }
        }
        return;
    }
    let Some(name) = node.child_by_field_name("name").map(|n| text(n, src)) else {
        return;
    };
    let recv = match node.child_by_field_name("object") {
        None => Receiver::SelfType,
        Some(obj) => match obj.kind() {
            "this" => Receiver::SelfType,
            "identifier" => {
                let t = text(obj, src);
                if let Some(r) = env.vars.get(t) {
                    r.clone()
                } else if let Some(ty) = fields.get(t) {
                    // A bare name that is a field of the enclosing type.
                    Receiver::SelfField(ty.clone())
                } else if t.chars().next().is_some_and(char::is_uppercase) {
                    // `Foo.bar()` — a static call, which is as pinned as a
                    // free function: the type is written at the call site.
                    Receiver::Typed(t.to_string())
                } else {
                    Receiver::Unknown
                }
            }
            "field_access" => match obj.child_by_field_name("field") {
                Some(f) => fields
                    .get(text(f, src))
                    .map(|ty| Receiver::SelfField(ty.clone()))
                    .unwrap_or(Receiver::Unknown),
                None => Receiver::Unknown,
            },
            // A chained call (`a.b().c()`) or anything else: the type comes
            // from an expression this does not evaluate, so say so rather than
            // guess. `Returned` is Go-specific plumbing and claiming it here
            // would route into a resolver that expects a Go signature.
            _ => Receiver::Unknown,
        },
    };
    let path = match (&recv, node.child_by_field_name("object")) {
        (Receiver::Typed(t), Some(obj)) if obj.kind() == "identifier" && text(obj, src) == t => {
            format!("{t}.{name}")
        }
        (Receiver::SelfType, _) => format!("{owner}.{name}"),
        _ => name.to_string(),
    };
    out.push(RawCall {
        path,
        recv,
        line,
        end_line,
    });
}

/// The declaration line, without the body: what a skeleton view shows.
fn head(node: Node, src: &str) -> String {
    let full = text(node, src);
    let cut = node
        .child_by_field_name("body")
        .and_then(|b| full.find(text(b, src)).or(Some(full.len())))
        .unwrap_or(full.len());
    clip(&collapse(full[..cut.min(full.len())].trim()))
}

fn text<'a>(node: Node, src: &'a str) -> &'a str {
    &src[node.byte_range()]
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(s: &str) -> String {
    if s.chars().count() <= 200 {
        return s.to_string();
    }
    let keep: String = s.chars().take(197).collect();
    format!("{keep}...")
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
        doc: None,
        calls: Vec::new(),
        children,
        arity: None,
        name,
        raw_calls: Vec::new(),
        implements: Vec::new(),
        field_types: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(src: &str) -> FileFacts {
        extract(src).unwrap()
    }

    /// The package declaration is the module path, and it is the reason
    /// `declared_module` exists: the path only mirrors it by convention, and a
    /// multi-module build would produce a name no import can match.
    #[test]
    fn the_package_declaration_becomes_the_module_path() {
        let f = facts("package com.foo.bar;\npublic class Thing {}\n");
        assert_eq!(
            f.declared_module,
            Some(vec!["com".into(), "foo".into(), "bar".into()])
        );
    }

    #[test]
    fn a_file_with_no_package_declares_nothing() {
        let f = facts("public class Thing {}\n");
        assert_eq!(f.declared_module, None);
    }

    #[test]
    fn imports_bind_the_simple_name() {
        let f = facts("package a;\nimport b.c.Thing;\npublic class X {}\n");
        assert_eq!(f.imports, vec!["b.c.Thing"]);
        let b = f.reexports.iter().find(|b| b.name == "Thing").unwrap();
        assert_eq!(b.path, "b.c.Thing");
        // Java has no re-export: an import is private to the compilation unit.
        assert!(!b.public);
    }

    /// A wildcard names a package, and a package is not a module here — every
    /// file is its own module, named for the type it declares. So it binds "*"
    /// the way every other extractor spells a glob, rather than inventing an
    /// edge to a type the import never named.
    #[test]
    fn a_wildcard_import_binds_a_glob_not_a_type() {
        let f = facts("package a;\nimport b.c.*;\npublic class X {}\n");
        assert!(f.reexports.iter().any(|b| b.name == "*"));
    }

    #[test]
    fn types_and_members_are_extracted_with_the_shared_kind_spellings() {
        let f = facts(
            "package a;\n\
             public class Outer {\n\
               private final Helper helper;\n\
               public int run(String s) { return helper.go(s); }\n\
             }\n\
             interface Marker {}\n\
             enum Color { RED }\n",
        );
        let kinds: Vec<&str> = f.items.iter().map(|i| i.kind.as_str()).collect();
        // The kind strings are matched literally by view/query, so a novel
        // spelling would make the item invisible to `--view skeleton`.
        assert!(kinds.contains(&"class"), "{kinds:?}");
        assert!(kinds.contains(&"interface"), "{kinds:?}");
        assert!(kinds.contains(&"enum"), "{kinds:?}");
        let outer = f
            .items
            .iter()
            .find(|i| i.name.as_deref() == Some("Outer"))
            .unwrap();
        assert!(outer
            .children
            .iter()
            .any(|c| c.name.as_deref() == Some("run")));
        assert_eq!(
            outer.field_types.get("helper").map(String::as_str),
            Some("Helper")
        );
    }

    /// A declared field type is what makes `SelfField` resolvable, and Java
    /// declares one everywhere — which is where its recall comes from.
    #[test]
    fn a_field_receiver_carries_its_declared_type() {
        let f = facts(
            "package a;\n\
             public class X {\n\
               private final Helper helper;\n\
               void run() { helper.go(); }\n\
             }\n",
        );
        let x = f
            .items
            .iter()
            .find(|i| i.name.as_deref() == Some("X"))
            .unwrap();
        let run = x
            .children
            .iter()
            .find(|c| c.name.as_deref() == Some("run"))
            .unwrap();
        let call = run
            .raw_calls
            .iter()
            .find(|c| c.path.ends_with("go"))
            .unwrap();
        assert!(
            matches!(&call.recv, Receiver::SelfField(t) if t == "Helper"),
            "{:?}",
            call.recv
        );
    }

    /// `new Foo()` is a call to Foo's constructor, modelled as a free call on
    /// the type name — the shape `rust.rs` uses for `Foo::new`.
    #[test]
    fn object_creation_is_a_free_call_on_the_type() {
        let f = facts("package a;\npublic class X { void run() { new Helper(); } }\n");
        let x = f
            .items
            .iter()
            .find(|i| i.name.as_deref() == Some("X"))
            .unwrap();
        let run = x
            .children
            .iter()
            .find(|c| c.name.as_deref() == Some("run"))
            .unwrap();
        assert!(run
            .raw_calls
            .iter()
            .any(|c| c.path == "Helper" && matches!(c.recv, Receiver::Free)));
    }

    /// Generics and arrays are stripped: a receiver typed `List` tells you
    /// nothing worth an edge, which is why `List` is in TRANSPARENT.
    #[test]
    fn a_transparent_receiver_type_is_not_claimed() {
        let f = facts(
            "package a;\n\
             public class X {\n\
               void run() { List<Thing> xs = make(); xs.add(1); }\n\
             }\n",
        );
        let x = f
            .items
            .iter()
            .find(|i| i.name.as_deref() == Some("X"))
            .unwrap();
        let run = x
            .children
            .iter()
            .find(|c| c.name.as_deref() == Some("run"))
            .unwrap();
        let add = run
            .raw_calls
            .iter()
            .find(|c| c.path.ends_with("add"))
            .unwrap();
        assert!(matches!(add.recv, Receiver::Unknown), "{:?}", add.recv);
    }

    #[test]
    fn heritage_populates_implements_for_dispatch() {
        let f = facts("package a;\npublic class X extends Base implements Marker {}\n");
        let x = f
            .items
            .iter()
            .find(|i| i.name.as_deref() == Some("X"))
            .unwrap();
        assert!(
            x.implements.contains(&"Base".to_string()),
            "{:?}",
            x.implements
        );
        assert!(
            x.implements.contains(&"Marker".to_string()),
            "{:?}",
            x.implements
        );
    }
}
