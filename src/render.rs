use crate::model::{Call, Graph, Item, Module};

/// An edge with its confidence marker: `~` receiver-inferred, `*` one branch
/// of a dispatch fan-out.
pub fn edge_str(c: &Call) -> String {
    match (c.heuristic, c.dispatch) {
        (_, true) => format!("{}*", c.to),
        (true, _) => format!("{}~", c.to),
        _ => c.to.clone(),
    }
}

pub fn markdown(g: &Graph) -> String {
    let mut out = String::new();
    out.push_str("# Codebase Topology Map\n");
    out.push_str(
        "Deterministic AST skeleton: module topology and signatures only, no implementation bodies.\n",
    );
    out.push_str(
        "Call edges: `→ name` resolved via import/path or declared receiver type; `name~`\n\
         inferred by receiver heuristic (trust less); `name*` one branch of a dynamic-dispatch\n\
         fan-out (trait object / interface / bounded generic — exactly one branch runs).\n",
    );
    out.push_str(&format!(
        "root: {} | files: {} | modules: {}\n",
        g.root,
        g.file_count,
        g.modules.len()
    ));
    // A filtered map is a partial map; saying so costs one line.
    if let Some(f) = crate::extract::filter_note() {
        out.push_str(&format!("scope: FILTERED ({f}) — modules outside it are absent\n"));
    }
    out.push('\n');
    for m in &g.modules {
        module_md(m, &mut out);
    }
    out
}

pub fn module_md(m: &Module, out: &mut String) {
    out.push_str(&format!("## {}  ({})\n", m.name, m.file));
    if !m.deps.is_empty() {
        // A dep that exists only via receiver inference is marked, so the
        // "`~`-free means resolved by import/path" rule holds at module level.
        let deps: Vec<String> = m
            .deps
            .iter()
            .map(|d| {
                if m.heuristic_deps.contains(d) {
                    format!("{d}~")
                } else {
                    d.clone()
                }
            })
            .collect();
        out.push_str(&format!("deps: {}\n", deps.join(", ")));
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
        let edges: Vec<String> = i
            .calls
            .iter()
            .map(edge_str)
            .collect();
        out.push_str(&format!(" → {}", edges.join(", ")));
    }
    if let Some(d) = &i.doc {
        out.push_str(&format!("  — {d}"));
    }
    out.push('\n');
    for c in &i.children {
        item_md(c, depth + 1, out);
    }
}

/// Render titled sections of modules, skipping any that are empty.
fn sections_md(out: &mut String, sections: &[(&str, &[&Module])]) {
    for (title, set) in sections {
        if set.is_empty() {
            continue;
        }
        out.push_str(&format!("---\n### {title}\n\n"));
        for m in *set {
            module_md(m, out);
        }
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
    sections_md(
        &mut out,
        &[
            ("Target", targets),
            ("Upstream (dependencies)", upstream),
            ("Downstream (dependents)", downstream),
        ],
    );
    out
}

/// Impact map for a diff: the changed modules, what they depend on, and the
/// callers they may break.
pub fn changed_md(
    label: &str,
    changed: &[&Module],
    upstream: &[&Module],
    downstream: &[&Module],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Changed vs {label}\n"));
    out.push_str(&format!(
        "{} changed module(s), their dependencies, and the callers they impact.\n\n",
        changed.len()
    ));
    sections_md(
        &mut out,
        &[
            ("Changed", changed),
            ("Depends on (upstream)", upstream),
            ("Impacted callers (downstream)", downstream),
        ],
    );
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
            let ds: Vec<String> = m
                .deps
                .iter()
                .map(|d| {
                    if m.heuristic_deps.contains(d) {
                        format!("{d}~")
                    } else {
                        d.clone()
                    }
                })
                .collect();
            format!("  -> {}", ds.join(", "))
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
