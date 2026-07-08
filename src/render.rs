use crate::model::{Graph, Item, Module};

pub fn markdown(g: &Graph) -> String {
    let mut out = String::new();
    out.push_str("# Codebase Topology Map\n");
    out.push_str(
        "Deterministic AST skeleton: module topology and signatures only, no implementation bodies.\n",
    );
    out.push_str(&format!(
        "root: {} | files: {} | modules: {}\n\n",
        g.root,
        g.file_count,
        g.modules.len()
    ));
    for m in &g.modules {
        module_md(m, &mut out);
    }
    out
}

pub fn module_md(m: &Module, out: &mut String) {
    out.push_str(&format!("## {}  ({})\n", m.name, m.file));
    if !m.deps.is_empty() {
        out.push_str(&format!("deps: {}\n", m.deps.join(", ")));
    }
    if !m.extern_deps.is_empty() {
        out.push_str(&format!("extern: {}\n", m.extern_deps.join(", ")));
    }
    if !m.reexports.is_empty() {
        out.push_str(&format!("reexports: {}\n", m.reexports.join(", ")));
    }
    for i in &m.items {
        item_md(i, 0, out);
    }
    out.push('\n');
}

fn item_md(i: &Item, depth: usize, out: &mut String) {
    out.push_str(&"  ".repeat(depth));
    out.push_str(&format!("- {}  [L{}]", i.signature, i.line));
    if !i.calls.is_empty() {
        out.push_str(&format!(" → {}", i.calls.join(", ")));
    }
    if let Some(d) = &i.doc {
        out.push_str(&format!("  — {d}"));
    }
    out.push('\n');
    for c in &i.children {
        item_md(c, depth + 1, out);
    }
}

pub fn subtree_md(
    query: &str,
    targets: &[&Module],
    upstream: &[&Module],
    downstream: &[&Module],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Subtree: {query}\n"));
    out.push_str(
        "Target module(s) plus immediate upstream (dependencies) and downstream (dependents).\n\n",
    );
    for (title, set) in [
        ("Target", targets),
        ("Upstream (dependencies)", upstream),
        ("Downstream (dependents)", downstream),
    ] {
        if set.is_empty() {
            continue;
        }
        out.push_str(&format!("---\n### {title}\n\n"));
        for m in set {
            module_md(m, &mut out);
        }
    }
    out
}

pub fn module_list(g: &Graph) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "root: {} | modules: {}\n",
        g.root,
        g.modules.len()
    ));
    for m in &g.modules {
        let deps = if m.deps.is_empty() {
            String::new()
        } else {
            format!("  -> {}", m.deps.join(", "))
        };
        out.push_str(&format!(
            "{}  ({})  [{} items]{}\n",
            m.name,
            m.file,
            m.item_count(),
            deps
        ));
    }
    out
}
