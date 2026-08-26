//! Move planning: the blast radius of relocating a module, and nothing more.
//!
//! This is deliberately an **oracle, not an actuator**. ctx never writes source
//! files; it emits the exact set of sites to change and then verifies the result.
//! That split is the point:
//!
//! - An agent can already perform edits cheaply and precisely. What it cannot do
//!   is know it found every site, or prove afterwards that nothing was orphaned.
//!   The scarce thing is ground truth, not typing.
//! - Staying read-only keeps ctx's safety profile: no partial application, no
//!   undo semantics, no permission friction, and the agent remains accountable
//!   for its own edits.
//!
//! Scope is bounded by what ctx can actually prove. Module moves ride on import
//! and link resolution — path arithmetic, deterministic, non-heuristic — which is
//! ctx's strongest signal. Renaming a *method* would instead ride on receiver
//! inference, which resolves a minority of call sites and deliberately drops
//! ambiguous ones; a plan built on that would silently miss sites, so it is not
//! offered here.

use serde_json::json;

use crate::model::{Graph, Lang, Module};

/// One import statement that must be rewritten.
pub struct ImportSite<'a> {
    pub module: &'a Module,
    /// Import text exactly as written in the source, to search for.
    pub written: String,
    /// The same text with the module path substituted.
    pub rewritten: String,
}

/// Path form of a module name in `lang`'s import syntax.
fn as_path(name: &str, lang: Lang) -> String {
    let from_sep = if name.contains("::") { "::" } else { "." };
    name.split(from_sep).collect::<Vec<_>>().join(Lang::sep(lang))
}

/// Suggested on-disk destination for a module's file.
///
/// The module's own name segments are stripped off the tail of its path, and the
/// destination's segments are appended in their place — so the prefix (`src/`,
/// `crates/foo/src/`) is preserved and an index file keeps its index role:
/// `src/query.rs` + `query -> query::graph` becomes `src/query/graph.rs`, while
/// `src/native/mod.rs` + `native -> core::native` becomes `src/core/native/mod.rs`.
fn suggested_file(m: &Module, from: &str, to: &str) -> Option<String> {
    let segs = |n: &str| -> Vec<String> {
        n.split(['.', ':'])
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    };
    let from_segs = segs(from);
    let to_segs = segs(to);
    if from_segs.is_empty() || to_segs.is_empty() {
        return None;
    }

    let path = std::path::Path::new(&m.file);
    let file_name = path.file_name()?.to_str()?;
    let stem = file_name.rsplit_once('.').map_or(file_name, |(s, _)| s);
    let ext = file_name.strip_prefix(stem).unwrap_or("");
    let comps: Vec<&str> = path
        .parent()
        .map(|p| p.iter().filter_map(|c| c.to_str()).collect())
        .unwrap_or_default();

    // An index-like file names its directory, so the module occupies the parent
    // directories; otherwise the file itself is the last segment.
    let is_index = m.is_package || matches!(stem, "mod" | "lib" | "main");
    let dir_segs_owned = if is_index {
        from_segs.len()
    } else {
        from_segs.len().saturating_sub(1)
    };
    if dir_segs_owned > comps.len() {
        return None;
    }
    let prefix = &comps[..comps.len() - dir_segs_owned];

    let mut parts: Vec<String> = prefix.iter().map(|s| s.to_string()).collect();
    if is_index {
        parts.extend(to_segs.iter().cloned());
        parts.push(file_name.to_string());
    } else {
        parts.extend(to_segs[..to_segs.len() - 1].iter().cloned());
        parts.push(format!("{}{ext}", to_segs.last()?));
    }
    Some(parts.join("/"))
}

/// Every import that resolves to `target`, with the rewrite applied.
fn import_sites<'a>(g: &'a Graph, target: &str, to: &str) -> Vec<ImportSite<'a>> {
    let mut out = Vec::new();
    for m in &g.modules {
        for (written, resolved) in &m.import_sites {
            if resolved != target {
                continue;
            }
            let from_path = as_path(target, m.lang);
            let to_path = as_path(to, m.lang);
            let rewritten = written.replace(&from_path, &to_path);
            out.push(ImportSite {
                module: m,
                written: written.clone(),
                rewritten,
            });
        }
    }
    out
}

/// Plan the relocation of `from` to `to`: every site that must change.
pub fn move_plan(g: &Graph, from: &str, to: &str, json_out: bool) -> String {
    let matches = |name: &str| {
        name == from
            || name.ends_with(&format!("::{from}"))
            || name.ends_with(&format!(".{from}"))
    };
    let Some(target) = g.modules.iter().find(|m| matches(&m.name)) else {
        return format!("no module matching '{from}'\n");
    };
    if g.modules.iter().any(|m| m.name == to) {
        return format!("'{to}' already exists — pick a destination that is free\n");
    }

    let sites = import_sites(g, &target.name, to);
    let file_move = suggested_file(target, &target.name, to);
    // Dependents that ctx knows about but that produced no import site: these are
    // edges inferred from call receivers, not imports, so they are NOT reliable
    // rewrite targets and must be listed separately rather than silently mixed in.
    let importers: std::collections::BTreeSet<&str> =
        sites.iter().map(|s| s.module.name.as_str()).collect();
    let soft_dependents: Vec<&Module> = g
        .modules
        .iter()
        .filter(|m| m.deps.iter().any(|d| d == &target.name))
        .filter(|m| !importers.contains(m.name.as_str()))
        .collect();

    if json_out {
        return serde_json::to_string_pretty(&json!({
            "from": target.name,
            "to": to,
            "file": target.file,
            "suggested_file": file_move,
            "import_sites": sites.iter().map(|s| json!({
                "module": s.module.name,
                "file": s.module.file,
                "written": s.written,
                "rewritten": s.rewritten,
            })).collect::<Vec<_>>(),
            "unverified_dependents": soft_dependents
                .iter()
                .map(|m| json!({"module": m.name, "file": m.file}))
                .collect::<Vec<_>>(),
            "verify": format!("ctx move-verify {to}"),
        }))
        .unwrap_or_default()
            + "\n";
    }

    let mut out = format!("# Move plan: {} → {to}\n\n", target.name);
    match &file_move {
        Some(f) => out.push_str(&format!("## 1. Move the file\n\n  {}  →  {f}\n\n", target.file)),
        None => out.push_str(&format!(
            "## 1. Move the file\n\n  {}  →  (destination path could not be derived)\n\n",
            target.file
        )),
    }

    out.push_str(&format!(
        "## 2. Rewrite {} import site(s)\n\n",
        sites.len()
    ));
    if sites.is_empty() {
        out.push_str("  none — nothing imports this module\n\n");
    } else {
        let mut last = "";
        for s in &sites {
            if s.module.name != last {
                out.push_str(&format!("{}  ({})\n", s.module.name, s.module.file));
                last = &s.module.name;
            }
            out.push_str(&format!("    -  {}\n    +  {}\n", s.written, s.rewritten));
        }
        out.push('\n');
    }

    if !soft_dependents.is_empty() {
        out.push_str(&format!(
            "## 3. Unverified dependents ({})\n\n\
             These reference the module through inferred call edges, not imports, so ctx \
             cannot name a literal string to rewrite. Check them by hand:\n\n",
            soft_dependents.len()
        ));
        for m in &soft_dependents {
            out.push_str(&format!("  {}  ({})\n", m.name, m.file));
        }
        out.push('\n');
    }

    // Rust module declarations are not imports, so they never appear as sites —
    // but the move is broken without them. Say so explicitly rather than letting
    // a complete-looking plan omit a required edit.
    if target.lang == Lang::Rust {
        let tail = target.name.rsplit("::").next().unwrap_or(&target.name);
        let to_tail = to.rsplit("::").next().unwrap_or(to);
        out.push_str(&format!(
            "## Also required (Rust)\n\n  \
             Remove `mod {tail};` from the old parent module and add `mod {to_tail};` to the \
             new one. Module declarations are not imports, so they do not appear above.\n\n"
        ));
    }

    out.push_str(
        "## Confidence\n\n\
         Every site in section 2 comes from import/link resolution — path arithmetic, not \
         receiver inference — so the list is exact for the languages ctx parses. It does NOT \
         cover: Rust `mod` declarations (see above), dynamic imports, string-built paths, \
         unparsed languages, or references in build files and CI config. Grep for the old \
         path once before deleting it.\n\n\
         ## Verify after applying\n\n",
    );
    out.push_str(&format!(
        "  ctx move-verify {to}\n\n\
         which re-derives the graph and checks the module landed, the old name is gone, and \
         no imports or links were orphaned.\n"
    ));
    out
}

/// Check a completed move: did the module land, and did anything break?
pub fn move_verify(g: &Graph, from: &str, to: &str, json_out: bool) -> (String, bool) {
    let landed = g.modules.iter().any(|m| m.name == to);
    let old_gone = !g.modules.iter().any(|m| m.name == from);
    let orphans: Vec<(&str, &str)> = g
        .modules
        .iter()
        .flat_map(|m| {
            m.import_sites
                .iter()
                .filter(|(_, resolved)| resolved == from)
                .map(move |(written, _)| (m.file.as_str(), written.as_str()))
        })
        .collect();
    let broken: usize = g.modules.iter().map(|m| m.diag.broken_links.len()).sum();
    let ok = landed && old_gone && orphans.is_empty();

    if json_out {
        let s = serde_json::to_string_pretty(&json!({
            "ok": ok,
            "landed": landed,
            "old_name_gone": old_gone,
            "orphaned_imports": orphans
                .iter()
                .map(|(f, w)| json!({"file": f, "import": w}))
                .collect::<Vec<_>>(),
            "broken_links_total": broken,
        }))
        .unwrap_or_default();
        return (s + "\n", ok);
    }

    let mut out = format!("# Move verify: {from} → {to}\n\n");
    out.push_str(&format!(
        "  module '{to}' present:    {}\n  old name '{from}' gone:  {}\n  orphaned imports:      {}\n",
        if landed { "yes" } else { "NO" },
        if old_gone { "yes" } else { "NO" },
        orphans.len()
    ));
    for (f, w) in &orphans {
        out.push_str(&format!("    {f}:  {w}\n"));
    }
    out.push_str(&format!("  broken links (repo-wide): {broken}\n\n"));
    out.push_str(if ok {
        "PASS — the move is structurally complete.\n"
    } else {
        "FAIL — see above. Broken-link count is repo-wide; compare it against the value \
         before the move.\n"
    });
    (out, ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::build_graph;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn graph(files: &[(&str, &str)]) -> (Graph, std::path::PathBuf) {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ctx_mv_{}_{}", std::process::id(), id));
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
    fn plan_lists_every_import_site_with_its_rewrite() {
        let (g, dir) = graph(&[
            ("pkg/__init__.py", ""),
            ("pkg/gate.py", "def check():\n    pass\n"),
            ("app.py", "from pkg.gate import check\n\ndef go():\n    check()\n"),
        ]);
        let out = move_plan(&g, "pkg.gate", "pkg.routing.gate", false);
        let _ = fs::remove_dir_all(&dir);
        assert!(out.contains("pkg/gate.py"), "must name the file to move: {out}");
        assert!(
            out.contains("pkg/routing/gate.py"),
            "destination path must be derived: {out}"
        );
        assert!(out.contains("pkg.routing.gate"), "rewrite must be shown: {out}");
        assert!(out.contains("1 import site"), "site count: {out}");
    }

    #[test]
    fn plan_refuses_an_occupied_destination() {
        let (g, dir) = graph(&[
            ("a.py", "def x():\n    pass\n"),
            ("b.py", "def y():\n    pass\n"),
        ]);
        let out = move_plan(&g, "a", "b", false);
        let _ = fs::remove_dir_all(&dir);
        assert!(out.contains("already exists"), "{out}");
    }

    #[test]
    fn plan_separates_inferred_dependents_from_import_sites() {
        // A dependent reached only by receiver inference has no literal string to
        // rewrite, so it must be listed apart rather than presented as a site.
        let (g, dir) = graph(&[
            ("lib.py", "class Thing:\n    def unique_method_name(self):\n        pass\n"),
            ("user.py", "def go(t):\n    t.unique_method_name()\n"),
        ]);
        let out = move_plan(&g, "lib", "core.lib", false);
        let _ = fs::remove_dir_all(&dir);
        assert!(
            out.contains("0 import site") || out.contains("none — nothing imports"),
            "an inferred edge is not an import site: {out}"
        );
        assert!(out.contains("Unverified dependents"), "{out}");
    }

    #[test]
    fn verify_fails_while_the_old_name_still_resolves() {
        let (g, dir) = graph(&[
            ("pkg/__init__.py", ""),
            ("pkg/gate.py", "def check():\n    pass\n"),
            ("app.py", "from pkg.gate import check\n"),
        ]);
        // Nothing has moved yet, so verification must fail.
        let (out, ok) = move_verify(&g, "pkg.gate", "pkg.routing.gate", false);
        let _ = fs::remove_dir_all(&dir);
        assert!(!ok, "unmoved tree must not verify: {out}");
        assert!(out.contains("orphaned imports:      1"), "{out}");
        assert!(out.contains("FAIL"), "{out}");
    }
}
