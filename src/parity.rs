//! `ctx parity`: cross-language structural equivalence.
//!
//! A source module and its port in another language are two renderings of one
//! structural skeleton. We flatten each into a language-neutral bag of
//! *members* keyed by `(container, canonical-name, role)`, align by exact key,
//! and report where they diverge: members missing from the port, arity drift,
//! dropped internal calls, moves, and additions. Structure only — never
//! semantics.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::json;

use crate::model::{Item, Module};

/// Kind equivalence class — alignment only ever happens within a role.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Role {
    Type,
    Function,
    Const,
    Module,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Role::Type => "type",
            Role::Function => "fn",
            Role::Const => "const",
            Role::Module => "mod",
        }
    }
}

/// Fold a language-specific item kind into a neutral role. `impl` returns None
/// because it is a *transparent container*, not a member of its own.
fn role_of(kind: &str) -> Option<Role> {
    match kind {
        "struct" | "enum" | "trait" | "type" | "class" | "interface" => Some(Role::Type),
        "fn" | "def" => Some(Role::Function),
        "const" | "static" => Some(Role::Const),
        "mod" | "section" => Some(Role::Module),
        _ => None,
    }
}

/// Collapse naming-convention differences: `computeGate`, `compute_gate`, and
/// `ComputeGate` all canonicalise to `computegate`.
fn canon(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// The bare final segment of a resolved callee path (`a::b::f` / `a.b.f` → `f`).
fn last_seg(s: &str) -> &str {
    s.rsplit([':', '.']).next().unwrap_or(s)
}

/// Systematic Python→Rust renames, as `canon(python) -> [canon(rust)…]`. Only
/// renames `canon()` does *not* already bridge appear here: it strips the
/// dunder underscores, so `__len__`↔`len`, `__eq__`↔`eq`, `__hash__`↔`hash`,
/// `__next__`↔`next`, `__contains__`↔`contains` already align with no entry.
/// A source name aligns to a target if the target matches any candidate.
pub fn py_rust_aliases() -> AliasMap {
    [
        ("init", vec!["new"]),
        ("str", vec!["tostring", "fmt", "display"]),
        ("repr", vec!["fmt", "debug"]),
        ("getitem", vec!["index", "get"]),
        ("setitem", vec!["indexmut", "insert", "set"]),
        ("delitem", vec!["remove"]),
        ("iter", vec!["iter", "intoiter"]),
        ("contains", vec!["contains"]),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.into_iter().map(String::from).collect()))
    .collect()
}

#[derive(Clone)]
struct Member {
    /// Original (display) container name; canonicalised at key time.
    container: Option<String>,
    name: String,
    role: Role,
    arity: Option<usize>,
    callees: BTreeSet<String>,
    kind: String,
    disp: String,
    file: String,
    line: usize,
}

type Key = (Option<String>, String, Role);

/// `canon(source-name) -> [canon(target-name) candidates]` for systematic
/// cross-language renames. Empty = exact-match only.
pub type AliasMap = BTreeMap<String, Vec<String>>;

impl Member {
    fn key(&self) -> Key {
        (self.container.as_deref().map(canon), self.name.clone(), self.role)
    }
    /// Container-agnostic key, for detecting moves and additions.
    fn nr(&self) -> (String, Role) {
        (self.name.clone(), self.role)
    }
    fn qual(&self) -> String {
        match &self.container {
            Some(c) => format!("{c}.{}", self.disp),
            None => self.disp.clone(),
        }
    }
}

/// Flatten a module's item tree into member records. `impl` blocks are
/// transparent (their type name becomes the container); type/module items are
/// both members *and* containers we descend into.
pub fn flatten(m: &Module) -> Vec<MemberView> {
    let mut out = Vec::new();
    walk(&m.items, None, &m.file, &mut out);
    out
}

fn walk(items: &[Item], container: Option<&str>, file: &str, out: &mut Vec<MemberView>) {
    for it in items {
        let Some(name) = it.name.as_deref() else {
            continue;
        };
        // Transparent container: an `impl Type { … }` contributes its methods
        // under `Type`, adding no nesting level of its own.
        if it.kind == "impl" {
            walk(&it.children, Some(name), file, out);
            continue;
        }
        let Some(role) = role_of(&it.kind) else {
            // Unknown kind: keep descending under the same container.
            walk(&it.children, container, file, out);
            continue;
        };
        let callees: BTreeSet<String> = it
            .calls
            .iter()
            .map(|c| canon(last_seg(&c.to)))
            .filter(|s| !s.is_empty())
            .collect();
        out.push(MemberView(Member {
            container: container.map(str::to_string),
            name: canon(name),
            role,
            arity: it.arity,
            callees,
            kind: it.kind.clone(),
            disp: name.to_string(),
            file: file.to_string(),
            line: it.line,
        }));
        // Descend into type / module containers, keyed by this item's name.
        if matches!(role, Role::Type | Role::Module) {
            walk(&it.children, Some(name), file, out);
        }
    }
}

/// Opaque wrapper so the `Member` internals stay private to this module while
/// callers can still hold and pass the flattened result.
pub struct MemberView(Member);

/// Produce the parity report comparing a source member bag against the union
/// of target member bags. Returns the rendered text and the count of members
/// missing from the port (for `--strict`).
pub fn report(
    source: &[MemberView],
    target: &[MemberView],
    aliases: &AliasMap,
    json_out: bool,
) -> (String, usize) {
    let source: Vec<&Member> = source.iter().map(|m| &m.0).collect();
    let target: Vec<&Member> = target.iter().map(|m| &m.0).collect();

    let t_by_key: BTreeMap<Key, &Member> = target.iter().map(|m| (m.key(), *m)).collect();
    let mut t_by_nr: BTreeMap<(String, Role), Vec<&Member>> = BTreeMap::new();
    for m in &target {
        t_by_nr.entry(m.nr()).or_default().push(m);
    }
    let s_nr: BTreeSet<(String, Role)> = source.iter().map(|m| m.nr()).collect();

    // Names that align by *exact* key on both sides — the only callees precise
    // enough to reason about for call drift (alias-aligned names excluded, so
    // a renamed callee is never mis-flagged as dropped).
    let aligned_names: BTreeSet<String> = source
        .iter()
        .filter(|m| t_by_key.contains_key(&m.key()))
        .map(|m| m.name.clone())
        .collect();

    let mut missing: Vec<&Member> = Vec::new();
    let mut ambiguous: Vec<(&Member, &Member)> = Vec::new();
    let mut arity: Vec<(&Member, &Member)> = Vec::new();
    let mut calls: Vec<(&Member, Vec<String>)> = Vec::new();
    let mut via_alias: Vec<(&Member, &Member, String)> = Vec::new();
    let mut consumed: BTreeSet<Key> = BTreeSet::new();
    let mut aligned = 0usize;

    for s in &source {
        // Resolve the target: exact key first, then any aliased key.
        let mut resolved: Option<(&Member, Option<String>)> = None;
        if let Some(t) = t_by_key.get(&s.key()) {
            resolved = Some((t, None));
        } else if let Some(alts) = aliases.get(&s.name) {
            for alt in alts {
                let mut k = s.key();
                k.1 = alt.clone();
                if let Some(t) = t_by_key.get(&k) {
                    resolved = Some((t, Some(alt.clone())));
                    break;
                }
            }
        }

        match resolved {
            Some((t, alias_used)) => {
                aligned += 1;
                consumed.insert(t.key());
                if let (Some(a), Some(b)) = (s.arity, t.arity) {
                    if a != b {
                        arity.push((s, t));
                    }
                }
                match alias_used {
                    None => {
                        let dropped: Vec<String> = s
                            .callees
                            .iter()
                            .filter(|c| aligned_names.contains(*c) && !t.callees.contains(*c))
                            .cloned()
                            .collect();
                        if !dropped.is_empty() {
                            calls.push((s, dropped));
                        }
                    }
                    Some(alt) => via_alias.push((s, t, alt)),
                }
            }
            None => {
                if let Some(cands) = t_by_nr.get(&s.nr()) {
                    ambiguous.push((s, cands[0]));
                } else {
                    missing.push(s);
                }
            }
        }
    }

    // Additions: target members not consumed by any alignment and sharing no
    // (name, role) with the source (so moves aren't counted twice).
    let added: Vec<&Member> = target
        .iter()
        .filter(|t| !consumed.contains(&t.key()) && !s_nr.contains(&t.nr()))
        .copied()
        .collect();

    if json_out {
        let out = json!({
            "source_members": source.len(),
            "target_members": target.len(),
            "aligned": aligned,
            "missing": missing.iter().map(mj).collect::<Vec<_>>(),
            "arity_drift": arity.iter().map(|(s, t)| json!({
                "member": s.qual(), "role": s.role.name(),
                "source_arity": s.arity, "target_arity": t.arity,
                "file": s.file, "line": s.line,
            })).collect::<Vec<_>>(),
            "call_drift": calls.iter().map(|(s, d)| json!({
                "member": s.qual(), "dropped_calls": d, "file": s.file, "line": s.line,
            })).collect::<Vec<_>>(),
            "ambiguous": ambiguous.iter().map(|(s, t)| json!({
                "source": s.qual(), "target": t.qual(), "role": s.role.name(),
            })).collect::<Vec<_>>(),
            "aligned_via_alias": via_alias.iter().map(|(s, t, alt)| json!({
                "source": s.qual(), "target": t.qual(), "alias": alt,
            })).collect::<Vec<_>>(),
            "added": added.iter().map(mj).collect::<Vec<_>>(),
        });
        return (
            serde_json::to_string_pretty(&out).unwrap_or_default() + "\n",
            missing.len(),
        );
    }

    let mut o = String::new();
    o.push_str("# ctx parity — source → port\n");
    o.push_str(&format!(
        "source {} members · target {} · aligned {} ({}% of source){}\n",
        source.len(),
        target.len(),
        aligned,
        pct(aligned, source.len()),
        if aliases.is_empty() {
            String::new()
        } else {
            format!(" · {} via alias", via_alias.len())
        },
    ));

    if !missing.is_empty() {
        o.push_str(&format!(
            "\n## Missing in port ({}) — in source, no counterpart in target\n",
            missing.len()
        ));
        for m in &missing {
            o.push_str(&format!(
                "  {:<4} {:<28} {}:{}\n",
                m.role.name(),
                m.qual(),
                m.file,
                m.line
            ));
        }
    }
    if !arity.is_empty() {
        o.push_str(&format!("\n## Arity drift ({})\n", arity.len()));
        for (s, t) in &arity {
            o.push_str(&format!(
                "  {:<4} {:<24} source={} → port={}   {}:{}\n",
                s.role.name(),
                s.qual(),
                s.arity.map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
                t.arity.map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
                s.file,
                s.line
            ));
        }
    }
    if !calls.is_empty() {
        o.push_str(&format!(
            "\n## Call drift ({}, soft) — internal calls in source, absent in port\n",
            calls.len()
        ));
        for (s, dropped) in &calls {
            o.push_str(&format!(
                "  {:<4} {:<24} dropped → {}\n",
                s.role.name(),
                s.qual(),
                dropped.join(", ")
            ));
        }
    }
    if !ambiguous.is_empty() {
        o.push_str(&format!(
            "\n## Ambiguous ({}) — name matches, container differs (moved?)\n",
            ambiguous.len()
        ));
        for (s, t) in &ambiguous {
            o.push_str(&format!(
                "  {:<4} source: {}  →  port: {}\n",
                s.role.name(),
                s.qual(),
                t.qual()
            ));
        }
    }
    if !via_alias.is_empty() {
        o.push_str(&format!(
            "\n## Aligned via alias ({}) — matched through a rename rule, not exactly\n",
            via_alias.len()
        ));
        for (s, t, alt) in &via_alias {
            o.push_str(&format!(
                "  {:<4} {}  →  {}   (via {} → {})\n",
                s.role.name(),
                s.qual(),
                t.qual(),
                s.name,
                alt
            ));
        }
    }
    if !added.is_empty() {
        o.push_str(&format!(
            "\n## Added in port ({}) — target only (often legitimate)\n",
            added.len()
        ));
        for m in &added {
            o.push_str(&format!("  {:<4} {}\n", m.role.name(), m.qual()));
        }
    }

    o.push_str(&format!(
        "\nparity: {}/{} source aligned · {} missing · {} arity · {} call · {} moved\n",
        aligned,
        source.len(),
        missing.len(),
        arity.len(),
        calls.len(),
        ambiguous.len()
    ));
    (o, missing.len())
}

fn mj(m: &&Member) -> serde_json::Value {
    json!({
        "member": m.qual(), "role": m.role.name(), "kind": m.kind,
        "file": m.file, "line": m.line,
    })
}

fn pct(n: usize, d: usize) -> u64 {
    if d == 0 {
        100
    } else {
        (100 * n as u64) / d as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    fn members(rel: &str, content: &str) -> Vec<MemberView> {
        let id = N.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("ctx_parity_{}_{}", std::process::id(), id));
        let _ = fs::remove_dir_all(&dir);
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, content).unwrap();
        let g = extract::build_graph(&dir).unwrap();
        let out = g.modules.iter().flat_map(flatten).collect();
        let _ = fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn canon_folds_naming_conventions() {
        assert_eq!(canon("computeGate"), "computegate");
        assert_eq!(canon("compute_gate"), "computegate");
        assert_eq!(canon("ComputeGate"), "computegate");
    }

    #[test]
    fn python_method_and_rust_impl_method_align() {
        // Python class method and Rust impl method flatten to the same key.
        let py = members(
            "gate.py",
            "class Gate:\n    def run(self, x):\n        return x\n",
        );
        let rs = members(
            "gate.rs",
            "pub struct Gate;\nimpl Gate {\n    pub fn run(&self, x: i32) -> i32 { x }\n}\n",
        );
        let (out, missing) = report(&py, &rs, &AliasMap::new(), false);
        assert_eq!(missing, 0, "{out}");
        assert!(out.contains("aligned 2"), "{out}"); // Gate + Gate.run
    }

    #[test]
    fn missing_and_arity_and_added_are_reported() {
        let py = members(
            "m.py",
            "def run(x, y):\n    return x\n\ndef gone():\n    return 1\n",
        );
        let rs = members(
            "m.rs",
            "pub fn run(x: i32) -> i32 { x }\npub fn extra() {}\n",
        );
        let (out, missing) = report(&py, &rs, &AliasMap::new(), false);
        assert_eq!(missing, 1, "{out}"); // `gone` missing
        assert!(out.contains("Missing in port") && out.contains("gone"), "{out}");
        assert!(out.contains("Arity drift") && out.contains("run"), "{out}"); // 2 → 1
        assert!(out.contains("Added in port") && out.contains("extra"), "{out}");
    }

    #[test]
    fn call_drift_flags_dropped_internal_call() {
        // Both sides have run + normalize aligned; the port's run drops the
        // call to normalize.
        let py = members(
            "c.py",
            "def normalize(x):\n    return x\n\ndef run(x):\n    return normalize(x)\n",
        );
        let rs = members(
            "c.rs",
            "pub fn normalize(x: i32) -> i32 { x }\npub fn run(x: i32) -> i32 { x }\n",
        );
        let (out, _) = report(&py, &rs, &AliasMap::new(), false);
        assert!(out.contains("Call drift") && out.contains("normalize"), "{out}");
    }

    #[test]
    fn py_rust_alias_maps_init_to_new() {
        let py = members("a.py", "class Gate:\n    def __init__(self, cfg):\n        pass\n");
        let rs = members(
            "a.rs",
            "pub struct Gate;\nimpl Gate {\n    pub fn new(cfg: Cfg) -> Self { Gate }\n}\n",
        );
        // Without aliases, __init__ has no counterpart → missing.
        let plain = report(&py, &rs, &AliasMap::new(), false);
        assert_eq!(plain.1, 1, "{}", plain.0);
        assert!(plain.0.contains("__init__"), "{}", plain.0);
        // With the py→rust table, __init__ aligns to new and is shown as such.
        let aliased = report(&py, &rs, &py_rust_aliases(), false);
        assert_eq!(aliased.1, 0, "{}", aliased.0);
        assert!(aliased.0.contains("Aligned via alias"), "{}", aliased.0);
        assert!(!aliased.0.contains("## Added"), "new must not read as added:\n{}", aliased.0);
    }
}
