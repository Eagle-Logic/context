//! Graph queries over the resolved topology: `def` (symbol -> definition
//! site) and `callers` (reverse call edges). Both are pure traversals over
//! the graph `build_graph` already produces — no extra extraction.

use serde_json::json;

use crate::model::{Graph, Item, Lang, Module};

fn sep_of(m: &Module) -> &'static str {
    match m.lang {
        Lang::Rust => "::",
        Lang::Python => ".",
    }
}

/// Split a possibly-qualified name into segments, accepting either `::` or
/// `.` so `SteerOverride::to_config`, `pkg.mod.fn`, and bare `to_config`
/// all work regardless of language.
fn segments(s: &str) -> Vec<&str> {
    s.split([':', '.']).filter(|x| !x.is_empty()).collect()
}

// ---- def -------------------------------------------------------------------

struct Def {
    qualname: String,
    kind: String,
    file: String,
    line: usize,
    signature: String,
    doc: Option<String>,
}

fn collect_defs(
    items: &[Item],
    m: &Module,
    container: Option<&str>,
    last: &str,
    parent: Option<&str>,
    out: &mut Vec<Def>,
) {
    let sep = sep_of(m);
    for it in items {
        if it.name.as_deref() == Some(last) && parent.is_none_or(|p| container == Some(p)) {
            let qualname = match container {
                Some(c) => format!("{}{sep}{c}{sep}{last}", m.name),
                None => format!("{}{sep}{last}", m.name),
            };
            out.push(Def {
                qualname,
                kind: it.kind.clone(),
                file: m.file.clone(),
                line: it.line,
                signature: it.signature.clone(),
                doc: it.doc.clone(),
            });
        }
        let next = if matches!(it.kind.as_str(), "impl" | "trait" | "class" | "mod") {
            it.name.as_deref().or(container)
        } else {
            container
        };
        collect_defs(&it.children, m, next, last, parent, out);
    }
}

pub fn def(g: &Graph, query: &str, json_out: bool) -> String {
    let q = segments(query);
    let Some(&last) = q.last() else {
        return "empty query\n".to_string();
    };
    let parent = if q.len() >= 2 { Some(q[q.len() - 2]) } else { None };

    let mut hits = Vec::new();
    for m in &g.modules {
        collect_defs(&m.items, m, None, last, parent, &mut hits);
    }
    hits.sort_by(|a, b| a.qualname.cmp(&b.qualname));

    if json_out {
        let arr: Vec<_> = hits
            .iter()
            .map(|h| {
                json!({
                    "qualname": h.qualname,
                    "kind": h.kind,
                    "file": h.file,
                    "line": h.line,
                    "signature": h.signature,
                    "doc": h.doc,
                })
            })
            .collect();
        return serde_json::to_string_pretty(&json!({ "query": query, "definitions": arr }))
            .unwrap_or_default()
            + "\n";
    }

    if hits.is_empty() {
        return format!("no definition found for '{query}'\n");
    }
    let mut out = format!("{} definition(s) of '{}':\n", hits.len(), query);
    for h in &hits {
        out.push_str(&format!(
            "\n{}   [{}]   {}:{}\n",
            h.qualname, h.kind, h.file, h.line
        ));
        let doc = h
            .doc
            .as_deref()
            .map(|d| format!("  — {d}"))
            .unwrap_or_default();
        out.push_str(&format!("    {}{}\n", h.signature, doc));
    }
    out
}

// ---- callers ---------------------------------------------------------------

/// An edge (a resolved call display string) targets the query when its
/// trailing segments equal the query's segments. Bare `to_config` matches
/// any `X::to_config`; qualified `SteerOverride::to_config` matches only that.
fn edge_matches(edge: &str, q: &[&str]) -> bool {
    let e = segments(edge);
    e.len() >= q.len() && e[e.len() - q.len()..] == *q
}

struct Caller {
    qualname: String,
    file: String,
    line: usize,
    edges: Vec<String>,
}

fn collect_callers(
    items: &[Item],
    m: &Module,
    path: &mut Vec<String>,
    q: &[&str],
    out: &mut Vec<Caller>,
) {
    let sep = sep_of(m);
    for it in items {
        let edges: Vec<String> = it
            .calls
            .iter()
            .filter(|e| edge_matches(e, q))
            .cloned()
            .collect();
        if !edges.is_empty() {
            let mut qualname = m.name.clone();
            for p in path.iter() {
                qualname.push_str(sep);
                qualname.push_str(p);
            }
            if let Some(n) = &it.name {
                qualname.push_str(sep);
                qualname.push_str(n);
            }
            out.push(Caller {
                qualname,
                file: m.file.clone(),
                line: it.line,
                edges,
            });
        }
        let container = matches!(it.kind.as_str(), "impl" | "trait" | "class" | "mod");
        match (container, &it.name) {
            (true, Some(n)) => {
                path.push(n.clone());
                collect_callers(&it.children, m, path, q, out);
                path.pop();
            }
            _ => collect_callers(&it.children, m, path, q, out),
        }
    }
}

pub fn callers(g: &Graph, query: &str, json_out: bool) -> String {
    let q = segments(query);
    if q.is_empty() {
        return "empty query\n".to_string();
    }
    let mut found = Vec::new();
    for m in &g.modules {
        let mut path = Vec::new();
        collect_callers(&m.items, m, &mut path, &q, &mut found);
    }
    found.sort_by(|a, b| (&a.qualname, a.line).cmp(&(&b.qualname, b.line)));

    if json_out {
        let arr: Vec<_> = found
            .iter()
            .map(|c| {
                json!({
                    "caller": c.qualname,
                    "file": c.file,
                    "line": c.line,
                    "edges": c.edges,
                })
            })
            .collect();
        return serde_json::to_string_pretty(&json!({ "query": query, "callers": arr }))
            .unwrap_or_default()
            + "\n";
    }

    if found.is_empty() {
        return format!(
            "no callers found for '{query}'\n\
             (only resolved call edges are indexed; ambiguous calls and \
             ubiquitous std-named methods are intentionally dropped)\n"
        );
    }
    let mut out = format!("{} caller(s) of '{}':\n\n", found.len(), query);
    for c in &found {
        out.push_str(&format!(
            "{}  ({}:{})  → {}\n",
            c.qualname,
            c.file,
            c.line,
            c.edges.join(", ")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::build_graph;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// Materialize `files` under a unique temp dir and build a graph from it.
    /// Returns the graph and the dir (caller removes it).
    fn graph(files: &[(&str, &str)]) -> (Graph, PathBuf) {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ctx_q_{}_{}", std::process::id(), id));
        let _ = fs::remove_dir_all(&dir);
        for (rel, content) in files {
            let p = dir.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, content).unwrap();
        }
        let g = build_graph(&dir).unwrap();
        (g, dir)
    }

    #[test]
    fn segments_splits_either_separator() {
        assert_eq!(segments("a::b::c"), ["a", "b", "c"]);
        assert_eq!(segments("a.b.c"), ["a", "b", "c"]);
        assert_eq!(segments("foo"), ["foo"]);
    }

    #[test]
    fn edge_matches_on_trailing_segments() {
        assert!(edge_matches("a::b::foo", &["foo"]));
        assert!(edge_matches("a::b::foo", &["b", "foo"]));
        assert!(edge_matches("foo", &["foo"]));
        assert!(!edge_matches("a::b::foo", &["x", "foo"]));
        assert!(!edge_matches("foo", &["bar"]));
        // A bare query must not match a longer suffix it isn't the tail of.
        assert!(!edge_matches("foobar", &["foo"]));
    }

    #[test]
    fn def_reports_kind_signature_and_doc() {
        let (g, dir) = graph(&[("src/lib.rs", "/// A widget.\npub struct Widget { pub n: i32 }\n")]);
        let out = def(&g, "Widget", false);
        assert!(out.contains("1 definition(s)"));
        assert!(out.contains("Widget"));
        assert!(out.contains("[struct]"));
        assert!(out.contains("A widget."));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn def_qualified_name_disambiguates_overloads() {
        let src = "pub struct A;\npub struct B;\n\
                   impl A { pub fn go(&self) {} }\nimpl B { pub fn go(&self) {} }\n";
        let (g, dir) = graph(&[("src/lib.rs", src)]);
        assert!(def(&g, "go", false).contains("2 definition(s)"));
        let one = def(&g, "A::go", false);
        assert!(one.contains("1 definition(s)"));
        assert!(one.contains("A::go") && !one.contains("B::go"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn def_missing_symbol() {
        let (g, dir) = graph(&[("src/lib.rs", "pub fn a() {}\n")]);
        assert!(def(&g, "nope", false).contains("no definition"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn callers_resolves_cross_module_call_edge() {
        let (g, dir) = graph(&[
            ("src/a.rs", "pub fn helper() {}\n"),
            ("src/b.rs", "use crate::a::helper;\npub fn run() { helper(); }\n"),
        ]);
        let out = callers(&g, "helper", false);
        assert!(out.contains("1 caller(s)"), "{out}");
        assert!(out.contains("b::run"), "{out}");
        assert!(out.contains("a::helper"), "{out}"); // the resolved edge
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn callers_none_for_uncalled_symbol() {
        let (g, dir) = graph(&[("src/a.rs", "pub fn lonely() {}\n")]);
        assert!(callers(&g, "lonely", false).contains("no callers"));
        let _ = fs::remove_dir_all(dir);
    }
}
