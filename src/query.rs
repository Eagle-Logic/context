//! Graph queries over the resolved topology: `def` (symbol -> definition
//! site) and `callers` (reverse call edges). Both are pure traversals over
//! the graph `build_graph` already produces — no extra extraction.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::json;

use crate::model::{Call, Graph, Item, Lang, Module};

/// Split a set of target modules' neighborhood into upstream (modules the
/// targets depend on) and downstream (modules that depend on a target).
/// Shared by `subtree` and `changed`.
pub fn neighbors<'a>(
    g: &'a Graph,
    target_names: &BTreeSet<&str>,
) -> (Vec<&'a Module>, Vec<&'a Module>) {
    let upstream_names: BTreeSet<&str> = g
        .modules
        .iter()
        .filter(|m| target_names.contains(m.name.as_str()))
        .flat_map(|m| m.deps.iter().map(|d| d.as_str()))
        .filter(|d| !target_names.contains(d))
        .collect();
    let upstream = g
        .modules
        .iter()
        .filter(|m| upstream_names.contains(m.name.as_str()))
        .collect();
    let downstream = g
        .modules
        .iter()
        .filter(|m| {
            !target_names.contains(m.name.as_str())
                && m.deps.iter().any(|d| target_names.contains(d.as_str()))
        })
        .collect();
    (upstream, downstream)
}

fn sep_of(m: &Module) -> &'static str {
    match m.lang {
        Lang::Rust => "::",
        Lang::Python | Lang::TypeScript | Lang::Markdown => ".",
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
    edges: Vec<Call>,
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
        let edges: Vec<Call> = it
            .calls
            .iter()
            .filter(|c| edge_matches(&c.to, q))
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

/// How many definitions the graph holds for this query.
///
/// This is the number that decides whether reverse-edge resolution could have
/// been ambiguous: a method name with several definitions cannot be attributed
/// from an opaque receiver, so those call sites are dropped rather than guessed.
/// `callers` reports it, because "1 caller" is a very different claim when the
/// graph knows the name three times over.
fn definition_count(g: &Graph, q: &[&str]) -> usize {
    let Some(&last) = q.last() else { return 0 };
    // Deliberately ignores any qualifier. Ambiguity is a property of the NAME as
    // written at the call site: `x.run()` cannot be pinned to `A::run` or
    // `B::run` no matter how the query is spelled, so counting only `A::run`
    // would silence the warning exactly when a user follows the documented
    // advice to qualify.
    let mut hits = Vec::new();
    for m in &g.modules {
        collect_defs(&m.items, m, None, last, None, &mut hits);
    }
    hits.len()
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

    // Reverse edges exist only for calls that resolved, so `callers` must never
    // present its count as an answer without the known loss channels alongside
    // it. Two are knowable from here.
    let defs = definition_count(g, &q);
    let bare_name = q.last().copied().unwrap_or(query);
    let ambiguous = defs > 1;
    // A suppressed ubiquitous name (`get`, `open`, `push`, ...) has NO reverse
    // edges at all, by design — including for a project-defined method that
    // merely shares the name. An empty result there is silence, not absence.
    let suppressed = crate::extract::is_suppressed_method_name(bare_name);
    let lower_bound = ambiguous || suppressed;

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
        return serde_json::to_string_pretty(&json!({
            "query": query,
            "callers": arr,
            "definitions": defs,
            // Never claim completeness: resolution can drop a call site for
            // reasons not visible from here (module-level calls, function-local
            // imports). These flag the two knowable causes; absent flags mean
            // "no known reason to distrust this", not "guaranteed complete".
            "lower_bound": lower_bound,
            "ambiguous_name": ambiguous,
            "suppressed_common_name": suppressed,
        }))
        .unwrap_or_default()
            + "\n";
    }

    // Spelled out the same way whether or not anything was found: the risk of
    // acting on an incomplete blast radius does not depend on the count.
    let recall_note = if suppressed {
        format!(
            "\nNOT INDEXED: '{bare_name}' is on the suppressed list of ubiquitous method names, \
             so ctx indexes NO reverse edges for it — including for a project-defined method of \
             that name. The result above carries no information about whether callers exist.\n\
             Use grep for this one:  rg -n '\\b{bare_name}\\s*\\('\n"
        )
    } else if ambiguous {
        format!(
            "\nINCOMPLETE: '{bare_name}' has {defs} definitions in this graph. Calls through a \
             receiver that cannot be pinned to one of them are dropped, never guessed, so the \
             list above is a LOWER BOUND.\n\
             Before changing this signature, confirm with:  rg -n '\\b{bare_name}\\s*\\('\n"
        )
    } else {
        String::new()
    };

    if found.is_empty() {
        return format!(
            "no callers found for '{query}'\n\
             (only resolved call edges are indexed; ambiguous calls and \
             ubiquitous std-named methods are intentionally dropped)\n{recall_note}"
        );
    }
    let mut out = format!("{} caller(s) of '{}':\n\n", found.len(), query);
    for c in &found {
        let edges: Vec<String> = c
            .edges
            .iter()
            .map(|e| {
                if e.heuristic {
                    format!("{}~", e.to)
                } else {
                    e.to.clone()
                }
            })
            .collect();
        out.push_str(&format!(
            "{}  ({}:{})  → {}\n",
            c.qualname,
            c.file,
            c.line,
            edges.join(", ")
        ));
    }
    if found.iter().any(|c| c.edges.iter().any(|e| e.heuristic)) {
        out.push_str("\n(~ = heuristic edge: attributed by receiver inference, not import/path)\n");
    }
    out.push_str(&recall_note);
    out
}

fn callers_of(g: &Graph, q: &[&str]) -> Vec<Caller> {
    let mut found = Vec::new();
    for m in &g.modules {
        let mut path = Vec::new();
        collect_callers(&m.items, m, &mut path, q, &mut found);
    }
    found.sort_by(|a, b| (&a.qualname, a.line).cmp(&(&b.qualname, b.line)));
    found
}

// ---- context ---------------------------------------------------------------

/// Type/keyword names never worth resolving as a signature reference.
const NOISE: &[&str] = &[
    "self", "Self", "str", "String", "bool", "char", "usize", "isize", "u8", "u16", "u32", "u64",
    "u128", "i8", "i16", "i32", "i64", "i128", "f32", "f64", "Vec", "Option", "Result", "Box",
    "Rc", "Arc", "HashMap", "BTreeMap", "HashSet", "BTreeSet", "string", "number", "boolean",
    "void", "unknown", "any", "Promise", "Record", "Array", "object", "null", "undefined",
    "never", "export", "function", "const", "class", "interface", "type", "enum", "pub", "fn",
    "async", "await", "impl", "def", "struct", "trait", "mut", "dyn", "where", "return", "true",
    "false", "None", "Some", "Ok", "Err",
];

struct DefLite {
    qualname: String,
    kind: String,
    file: String,
    line: usize,
    doc: Option<String>,
}

/// Index every named item by bare name (first definition wins) for resolving
/// the types that appear in a signature.
fn def_index(g: &Graph) -> HashMap<String, DefLite> {
    fn rec<'a>(
        items: &'a [Item],
        m: &'a Module,
        container: Option<&str>,
        sep: &str,
        idx: &mut HashMap<String, DefLite>,
    ) {
        for it in items {
            if let Some(name) = &it.name {
                idx.entry(name.clone()).or_insert_with(|| {
                    let qualname = match container {
                        Some(c) => format!("{}{sep}{c}{sep}{name}", m.name),
                        None => format!("{}{sep}{name}", m.name),
                    };
                    DefLite {
                        qualname,
                        kind: it.kind.clone(),
                        file: m.file.clone(),
                        line: it.line,
                        doc: it.doc.clone(),
                    }
                });
            }
            let next = if matches!(it.kind.as_str(), "impl" | "trait" | "class" | "mod") {
                it.name.as_deref().or(container)
            } else {
                container
            };
            rec(&it.children, m, next, sep, idx);
        }
    }
    let mut idx = HashMap::new();
    for m in &g.modules {
        rec(&m.items, m, None, sep_of(m), &mut idx);
    }
    idx
}

/// Locate the target items (with their enclosing container) for a query.
#[allow(clippy::type_complexity)]
fn find_targets<'a>(
    g: &'a Graph,
    last: &str,
    parent: Option<&str>,
) -> Vec<(String, Option<String>, &'a Item, &'a Module)> {
    fn rec<'a>(
        items: &'a [Item],
        m: &'a Module,
        container: Option<&str>,
        sep: &str,
        last: &str,
        parent: Option<&str>,
        out: &mut Vec<(String, Option<String>, &'a Item, &'a Module)>,
    ) {
        for it in items {
            if it.name.as_deref() == Some(last) && parent.is_none_or(|p| container == Some(p)) {
                let qualname = match container {
                    Some(c) => format!("{}{sep}{c}{sep}{last}", m.name),
                    None => format!("{}{sep}{last}", m.name),
                };
                out.push((qualname, container.map(str::to_string), it, m));
            }
            let next = if matches!(it.kind.as_str(), "impl" | "trait" | "class" | "mod") {
                it.name.as_deref().or(container)
            } else {
                container
            };
            rec(&it.children, m, next, sep, last, parent, out);
        }
    }
    let mut out = Vec::new();
    for m in &g.modules {
        rec(&m.items, m, None, sep_of(m), last, parent, &mut out);
    }
    out
}

/// Distinct identifiers appearing in a signature, in order.
fn identifiers(sig: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in sig.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else if !cur.is_empty() {
            if !out.contains(&cur) {
                out.push(cur.clone());
            }
            cur.clear();
        }
    }
    if !cur.is_empty() && !out.contains(&cur) {
        out.push(cur);
    }
    out
}

/// The type-like definitions referenced by a signature: PascalCase
/// identifiers that resolve to a type/struct/enum/etc. (not the symbol
/// itself, param names, or noise).
fn signature_types<'a>(
    sig: &str,
    self_name: &str,
    idx: &'a HashMap<String, DefLite>,
) -> Vec<&'a DefLite> {
    identifiers(sig)
        .into_iter()
        .filter(|n| n != self_name)
        .filter(|n| n.chars().next().is_some_and(char::is_uppercase))
        .filter(|n| !NOISE.contains(&n.as_str()))
        .filter_map(|n| idx.get(&n))
        .filter(|d| {
            matches!(
                d.kind.as_str(),
                "struct" | "enum" | "trait" | "interface" | "type" | "class"
            )
        })
        .take(8)
        .collect()
}

fn edge_str(c: &Call) -> String {
    if c.heuristic {
        format!("{}~", c.to)
    } else {
        c.to.clone()
    }
}

/// Assemble everything needed to edit a symbol — definition, the types in its
/// signature, what it calls, and what calls it — trimmed to a token budget.
pub fn context(g: &Graph, query: &str, max_tokens: usize, json_out: bool) -> String {
    let q = segments(query);
    let Some(&last) = q.last() else {
        return "empty query\n".to_string();
    };
    let parent = if q.len() >= 2 { Some(q[q.len() - 2]) } else { None };
    let targets = find_targets(g, last, parent);
    if targets.is_empty() {
        return format!("no definition found for '{query}'\n");
    }
    let idx = def_index(g);
    let budget = max_tokens.saturating_mul(4).max(400);

    if json_out {
        let blocks: Vec<_> = targets
            .iter()
            .take(3)
            .map(|(qual, container, it, m)| {
                let q_caller: Vec<&str> = match container {
                    Some(c) => vec![c.as_str(), last],
                    None => vec![last],
                };
                let callers = callers_of(g, &q_caller);
                let types: Vec<_> = signature_types(&it.signature, last, &idx)
                    .iter()
                    .map(|d| {
                        json!({"name": d.qualname, "kind": d.kind, "file": d.file, "line": d.line})
                    })
                    .collect();
                json!({
                    "definition": {"qualname": qual, "kind": it.kind, "file": m.file, "line": it.line, "signature": it.signature, "doc": it.doc},
                    "signature_types": types,
                    "calls": it.calls.iter().map(|c| json!({"to": c.to, "heuristic": c.heuristic})).collect::<Vec<_>>(),
                    "callers": callers.iter().map(|c| json!({"caller": c.qualname, "file": c.file, "line": c.line})).collect::<Vec<_>>(),
                })
            })
            .collect();
        return serde_json::to_string_pretty(&json!({"query": query, "context": blocks}))
            .unwrap_or_default()
            + "\n";
    }

    let mut out = String::new();
    if targets.len() > 1 {
        out.push_str(&format!(
            "{} definitions match '{}' (showing up to 3; qualify to narrow):\n",
            targets.len(),
            query
        ));
    }
    for (qual, container, it, m) in targets.iter().take(3) {
        out.push_str(&format!("\n# Context: {qual}\n\n"));
        out.push_str(&format!("## Definition\n{qual}   [{}]   {}:{}\n", it.kind, m.file, it.line));
        let doc = it.doc.as_deref().map(|d| format!("  — {d}")).unwrap_or_default();
        out.push_str(&format!("    {}{}\n", it.signature, doc));

        // Signature types.
        let types = signature_types(&it.signature, last, &idx);
        if !types.is_empty() {
            out.push_str("\n## Signature types\n");
            for d in types {
                let doc = d.doc.as_deref().map(|s| format!("  — {s}")).unwrap_or_default();
                out.push_str(&format!("- {} [{}]  {}:{}{}\n", d.qualname, d.kind, d.file, d.line, doc));
            }
        }

        // Callees.
        if !it.calls.is_empty() {
            out.push_str(&format!("\n## Calls ({})\n", it.calls.len()));
            append_capped(&mut out, it.calls.iter().map(|c| format!("- {}", edge_str(c))), 30, budget);
        }

        // Callers (blast radius).
        let q_caller: Vec<&str> = match container {
            Some(c) => vec![c.as_str(), last],
            None => vec![last],
        };
        let callers = callers_of(g, &q_caller);
        if !callers.is_empty() {
            out.push_str(&format!("\n## Callers ({})\n", callers.len()));
            append_capped(
                &mut out,
                callers.iter().map(|c| format!("- {}  ({}:{})", c.qualname, c.file, c.line)),
                30,
                budget,
            );
        }
    }
    out
}

/// Push up to `cap` lines, stopping early if the byte budget is hit; notes
/// how many were elided.
fn append_capped(out: &mut String, lines: impl Iterator<Item = String>, cap: usize, budget: usize) {
    let mut shown = 0;
    let mut total = 0;
    for line in lines {
        total += 1;
        if shown < cap && out.len() + line.len() < budget {
            out.push_str(&line);
            out.push('\n');
            shown += 1;
        }
    }
    if shown < total {
        out.push_str(&format!("- … (+{} more)\n", total - shown));
    }
}

// ---- core (centrality) -----------------------------------------------------

/// Out-edges as module indices: `m -> d` for each dep `d` of `m` that resolves
/// to a module in this graph.
fn out_edges(g: &Graph) -> Vec<Vec<usize>> {
    let index: HashMap<&str, usize> = g
        .modules
        .iter()
        .enumerate()
        .map(|(i, m)| (m.name.as_str(), i))
        .collect();
    g.modules
        .iter()
        .map(|m| {
            m.deps
                .iter()
                .filter_map(|d| index.get(d.as_str()).copied())
                .collect()
        })
        .collect()
}

/// PageRank over the module graph (edge `m -> d` for each dep `d` of `m`), so
/// heavily-depended-upon modules score highest. Deterministic: fixed damping
/// and iteration count, so the same graph always yields the same ranks.
///
/// Shared by `core` and by budget-driven pruning, so "central" means the same
/// thing whether you are ranking modules or deciding which to drop.
pub fn pagerank(g: &Graph) -> Vec<f64> {
    let n = g.modules.len();
    if n == 0 {
        return Vec::new();
    }
    let out = out_edges(g);
    let outdeg: Vec<usize> = out.iter().map(Vec::len).collect();

    let damping = 0.85;
    let base = (1.0 - damping) / n as f64;
    let mut rank = vec![1.0 / n as f64; n];
    for _ in 0..50 {
        let dangling: f64 = (0..n).filter(|&i| outdeg[i] == 0).map(|i| rank[i]).sum();
        let mut next = vec![base + damping * dangling / n as f64; n];
        for i in 0..n {
            if outdeg[i] > 0 {
                let share = damping * rank[i] / outdeg[i] as f64;
                for &j in &out[i] {
                    next[j] += share;
                }
            }
        }
        rank = next;
    }
    rank
}

/// Module indices ordered most- to least-central.
///
/// Modules with no resolved dependency edges all sit at the same PageRank
/// baseline, and on a large repo that tie group is most of the tree. Breaking
/// those ties by name alone would fill a budget with whatever sorts first, so
/// code outranks prose within a tie: a Markdown heading map is orientation, but
/// a `CHANGELOG` should not displace a module something actually imports.
/// Ties then fall back to name, keeping the order stable across runs.
pub fn centrality_order(g: &Graph) -> Vec<usize> {
    let rank = pagerank(g);
    let is_code = |i: usize| g.modules[i].lang != Lang::Markdown;
    let mut order: Vec<usize> = (0..g.modules.len()).collect();
    order.sort_by(|&a, &b| {
        rank[b]
            .partial_cmp(&rank[a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(is_code(b).cmp(&is_code(a)))
            .then(g.modules[a].name.cmp(&g.modules[b].name))
    });
    order
}

/// Rank modules by dependency centrality, optionally weighted by git churn.
pub fn core(
    g: &Graph,
    limit: usize,
    churn: Option<&HashMap<String, usize>>,
    json_out: bool,
) -> String {
    let n = g.modules.len();
    if n == 0 {
        return "no modules\n".to_string();
    }
    let out = out_edges(g);
    let outdeg: Vec<usize> = out.iter().map(Vec::len).collect();
    let rank = pagerank(g);

    let mut indeg = vec![0usize; n];
    for edges in &out {
        for &j in edges {
            indeg[j] += 1;
        }
    }

    // Per-module churn and the combined "hotspot" score: central AND volatile.
    let churn_of: Vec<usize> = g
        .modules
        .iter()
        .map(|m| churn.and_then(|c| c.get(&m.name)).copied().unwrap_or(0))
        .collect();
    let max_rank = rank.iter().cloned().fold(0.0_f64, f64::max).max(f64::MIN_POSITIVE);
    let max_churn = *churn_of.iter().max().unwrap_or(&0);
    let hotspot: Vec<f64> = (0..n)
        .map(|i| {
            if max_churn == 0 {
                rank[i]
            } else {
                (rank[i] / max_rank) * (churn_of[i] as f64 / max_churn as f64)
            }
        })
        .collect();

    let key = |i: usize| if churn.is_some() { hotspot[i] } else { rank[i] };
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| {
        key(b)
            .partial_cmp(&key(a))
            .unwrap()
            .then(g.modules[a].name.cmp(&g.modules[b].name))
    });
    order.truncate(limit);

    if json_out {
        let arr: Vec<_> = order
            .iter()
            .map(|&i| {
                json!({
                    "module": g.modules[i].name,
                    "file": g.modules[i].file,
                    "score": rank[i],
                    "dependents": indeg[i],
                    "dependencies": outdeg[i],
                    "churn": churn_of[i],
                    "hotspot": hotspot[i],
                    "items": g.modules[i].item_count(),
                })
            })
            .collect();
        return serde_json::to_string_pretty(&json!({"root": g.root, "core": arr}))
            .unwrap_or_default()
            + "\n";
    }

    let mut s = format!("# Core modules — {}\n", g.root);
    if churn.is_some() {
        s.push_str("Ranked by hotspot = centrality × churn (central AND frequently changed).\n\n");
        s.push_str("  score    in  out  churn  module\n");
        for &i in &order {
            s.push_str(&format!(
                "  {:.4}  {:>4} {:>4}  {:>5}  {}  [{} items]\n",
                rank[i],
                indeg[i],
                outdeg[i],
                churn_of[i],
                g.modules[i].name,
                g.modules[i].item_count()
            ));
        }
    } else {
        s.push_str("Ranked by dependency centrality (PageRank); higher = more depended-upon.\n\n");
        s.push_str("  score    in  out  module\n");
        for &i in &order {
            s.push_str(&format!(
                "  {:.4}  {:>4} {:>4}  {}  [{} items]\n",
                rank[i],
                indeg[i],
                outdeg[i],
                g.modules[i].name,
                g.modules[i].item_count()
            ));
        }
    }
    s
}

// ---- coverage --------------------------------------------------------------

/// A completeness/blind-spot report: how much of the call graph resolved,
/// where confidence is low, and which source files are not modeled at all.
pub fn coverage_report(g: &Graph, unsupported: &[(String, usize)], json_out: bool) -> String {
    let (mut sites, mut resolved, mut heuristic, mut std_builtin) = (0usize, 0usize, 0usize, 0usize);
    let mut by_lang: BTreeMap<&str, usize> = BTreeMap::new();
    for m in &g.modules {
        sites += m.diag.call_sites;
        resolved += m.diag.resolved;
        heuristic += m.diag.heuristic;
        std_builtin += m.diag.std_builtin;
        *by_lang.entry(m.lang.name()).or_default() += 1;
    }
    // Genuine misses: neither an internal edge nor a known std/builtin call.
    let dropped = sites.saturating_sub(resolved).saturating_sub(std_builtin);
    let pct = |n: usize, d: usize| if d == 0 { 0.0 } else { 100.0 * n as f64 / d as f64 };
    let miss = |m: &Module| m.diag.call_sites - m.diag.resolved - m.diag.std_builtin;

    // Low-confidence zones: high heuristic ratio (min 10 resolved), and most
    // genuinely-unresolved call sites.
    let mut heur_zones: Vec<&Module> = g
        .modules
        .iter()
        .filter(|m| m.diag.resolved >= 10)
        .collect();
    heur_zones.sort_by(|a, b| {
        pct(b.diag.heuristic, b.diag.resolved)
            .partial_cmp(&pct(a.diag.heuristic, a.diag.resolved))
            .unwrap()
    });
    let mut drop_zones: Vec<&Module> = g.modules.iter().filter(|m| miss(m) > 0).collect();
    drop_zones.sort_by_key(|&m| std::cmp::Reverse(miss(m)));

    // Markdown broken links: targets that resolve to no file or heading. These
    // are true dead links (out-of-scope `../` links that exist on disk are not
    // counted), so they warrant a dedicated report rather than the generic
    // "external/unpinned" bucket that suits code.
    let broken: Vec<(&str, usize, &str)> = g
        .modules
        .iter()
        .flat_map(|m| {
            m.diag
                .broken_links
                .iter()
                .map(move |(line, target)| (m.file.as_str(), *line, target.as_str()))
        })
        .collect();

    if json_out {
        return serde_json::to_string_pretty(&json!({
            "root": g.root,
            "modules": g.modules.len(),
            "modules_by_lang": by_lang,
            "call_sites": sites,
            "resolved": resolved,
            "heuristic": heuristic,
            "std_builtin": std_builtin,
            "dropped": dropped,
            "broken_links": broken.iter().map(|(f, l, t)| json!({"file": f, "line": l, "target": t})).collect::<Vec<_>>(),
            "unsupported_files": unsupported.iter().map(|(e, n)| json!({"ext": e, "count": n})).collect::<Vec<_>>(),
        }))
        .unwrap_or_default()
            + "\n";
    }

    let mut out = format!("# ctx coverage report — {}\n\n", g.root);
    let langs: Vec<String> = by_lang.iter().map(|(l, n)| format!("{l} {n}")).collect();
    out.push_str(&format!(
        "Modules: {}  ({})\n\n",
        g.modules.len(),
        langs.join(", ")
    ));
    out.push_str("## Call-edge resolution\n");
    out.push_str("ctx only draws edges it can prove; std/builtin and external-crate calls are\n");
    out.push_str("intentionally not edged, so a low internal-edge fraction is expected, not a defect.\n\n");
    out.push_str(&format!("call sites:          {sites}\n"));
    out.push_str(&format!(
        "  internal edges:    {resolved} ({:.1}%)   [{heuristic} heuristic (~), {:.1}% of edges]\n",
        pct(resolved, sites),
        pct(heuristic, resolved)
    ));
    out.push_str(&format!(
        "  std / builtin:     {std_builtin} ({:.1}%)   [push/iter/map/… — never edged by design]\n",
        pct(std_builtin, sites)
    ));
    out.push_str(&format!(
        "  external / unpinned: {dropped} ({:.1}%)   [external-crate, dynamic, or ambiguous]\n",
        pct(dropped, sites)
    ));

    if !heur_zones.is_empty() {
        out.push_str("\n## Low-confidence zones (edges to distrust — grep to confirm)\n");
        for m in heur_zones.iter().take(8) {
            out.push_str(&format!(
                "  {:<32} {:.0}% heuristic ({}/{} edges)\n",
                m.name,
                pct(m.diag.heuristic, m.diag.resolved),
                m.diag.heuristic,
                m.diag.resolved
            ));
        }
    }
    if !drop_zones.is_empty() {
        out.push_str("\n## Most external/unpinned calls (mapped least completely — relative signal)\n");
        for m in drop_zones.iter().take(8) {
            out.push_str(&format!(
                "  {:<32} {} unpinned of {} sites\n",
                m.name,
                miss(m),
                m.diag.call_sites
            ));
        }
    }

    if !broken.is_empty() {
        out.push_str(&format!("\n## Broken links ({}, Markdown)\n", broken.len()));
        out.push_str("targets pointing at no file or heading (existing out-of-scope links excluded):\n");
        for (file, line, target) in broken.iter().take(20) {
            out.push_str(&format!("  {file}:{line}  →  {target}\n"));
        }
        if broken.len() > 20 {
            out.push_str(&format!("  … and {} more\n", broken.len() - 20));
        }
    }

    out.push_str("\n## Not modeled (blind spots)\n");
    if unsupported.is_empty() {
        out.push_str("  none — every source file under this root is a supported language\n");
    } else {
        out.push_str("  source files present that ctx does not parse:\n");
        for (ext, n) in unsupported {
            out.push_str(&format!("  .{ext:<8} {n}\n"));
        }
    }
    out.push_str("  (supported: .rs .py .ts .tsx .md)\n");
    out
}

// ---- api diff (breaking-change detector) -----------------------------------

struct Surface {
    kind: String,
    signature: String,
    file: String,
    line: usize,
    /// Segments to match call edges against (container + name, or just name).
    match_q: Vec<String>,
}

fn is_surface_kind(k: &str) -> bool {
    matches!(
        k,
        "fn" | "def" | "struct" | "enum" | "trait" | "interface" | "type" | "class" | "const"
            | "macro"
    )
}

/// The public API surface of a graph: every exported/`pub` item (and public
/// methods of public types) keyed by qualified name, with its signature.
fn public_surface(g: &Graph) -> BTreeMap<String, Surface> {
    fn rec(
        items: &[Item],
        m: &Module,
        container: Option<&str>,
        sep: &str,
        force_public: bool,
        map: &mut BTreeMap<String, Surface>,
    ) {
        for it in items {
            let public = force_public || crate::view::is_public(it, m.lang);
            if let (Some(name), true) = (&it.name, public) {
                if is_surface_kind(&it.kind) {
                    let qual = match container {
                        Some(c) => format!("{}{sep}{c}{sep}{name}", m.name),
                        None => format!("{}{sep}{name}", m.name),
                    };
                    let match_q = match container {
                        Some(c) => vec![c.to_string(), name.clone()],
                        None => vec![name.clone()],
                    };
                    map.insert(
                        qual,
                        Surface {
                            kind: it.kind.clone(),
                            signature: it.signature.clone(),
                            file: m.file.clone(),
                            line: it.line,
                            match_q,
                        },
                    );
                }
            }
            if matches!(it.kind.as_str(), "impl" | "trait" | "class" | "mod") {
                let next = it.name.as_deref().or(container);
                // Trait methods have no `pub` but are part of the trait's API.
                let child_force = it.kind == "trait" && crate::view::is_public(it, m.lang);
                rec(&it.children, m, next, sep, child_force, map);
            }
        }
    }
    let mut map = BTreeMap::new();
    for m in &g.modules {
        rec(&m.items, m, None, sep_of(m), false, &mut map);
    }
    map
}

fn caller_names(g: &Graph, q: &[String]) -> Vec<String> {
    let refs: Vec<&str> = q.iter().map(String::as_str).collect();
    callers_of(g, &refs).into_iter().map(|c| c.qualname).collect()
}

fn fmt_callers(names: &[String]) -> String {
    if names.is_empty() {
        "    callers: none found in this tree\n".to_string()
    } else {
        let shown: Vec<&str> = names.iter().take(8).map(String::as_str).collect();
        let more = names.len().saturating_sub(shown.len());
        let suffix = if more > 0 {
            format!(", +{more} more")
        } else {
            String::new()
        };
        format!("    callers ({}): {}{}\n", names.len(), shown.join(", "), suffix)
    }
}

/// Diff the public API surface between `base` and `current`: removed and
/// signature-changed public items are (potentially) breaking; each is listed
/// with the callers it affects (removed → who used it in `base`; changed →
/// who uses it in `current`).
pub fn api_report(base: &Graph, current: &Graph, label: &str, json_out: bool) -> String {
    let bs = public_surface(base);
    let cs = public_surface(current);

    let mut removed: Vec<(&String, &Surface)> = Vec::new();
    let mut changed: Vec<(&String, &Surface, &Surface)> = Vec::new();
    for (q, b) in &bs {
        match cs.get(q) {
            None => removed.push((q, b)),
            Some(c) if c.signature != b.signature => changed.push((q, b, c)),
            _ => {}
        }
    }
    let added: Vec<(&String, &Surface)> = cs.iter().filter(|(q, _)| !bs.contains_key(*q)).collect();

    if json_out {
        return serde_json::to_string_pretty(&json!({
            "since": label,
            "removed": removed.iter().map(|(q, b)| json!({"name": q, "kind": b.kind, "signature": b.signature, "callers": caller_names(base, &b.match_q)})).collect::<Vec<_>>(),
            "changed": changed.iter().map(|(q, b, c)| json!({"name": q, "kind": c.kind, "was": b.signature, "now": c.signature, "callers": caller_names(current, &c.match_q)})).collect::<Vec<_>>(),
            "added": added.iter().map(|(q, c)| json!({"name": q, "kind": c.kind, "signature": c.signature})).collect::<Vec<_>>(),
        }))
        .unwrap_or_default()
            + "\n";
    }

    if removed.is_empty() && changed.is_empty() && added.is_empty() {
        return format!("no public API changes vs {label}\n");
    }

    let mut out = format!("# API changes vs {label}\n");
    out.push_str(&format!(
        "{} removed, {} changed, {} added.\n",
        removed.len(),
        changed.len(),
        added.len()
    ));

    if !removed.is_empty() {
        out.push_str("\n## Removed — breaking\n");
        for (q, b) in &removed {
            out.push_str(&format!("- {q}  [{}]  ({}:{})\n", b.kind, b.file, b.line));
            out.push_str(&format!("    was: {}\n", b.signature));
            out.push_str(&fmt_callers(&caller_names(base, &b.match_q)));
        }
    }
    if !changed.is_empty() {
        out.push_str("\n## Changed signature — potentially breaking\n");
        for (q, b, c) in &changed {
            out.push_str(&format!("- {q}  [{}]  ({}:{})\n", c.kind, c.file, c.line));
            out.push_str(&format!("    was: {}\n", b.signature));
            out.push_str(&format!("    now: {}\n", c.signature));
            out.push_str(&fmt_callers(&caller_names(current, &c.match_q)));
        }
    }
    if !added.is_empty() {
        out.push_str("\n## Added — non-breaking\n");
        for (q, c) in &added {
            out.push_str(&format!("- {q}  [{}]  {}\n", c.kind, c.signature));
        }
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
    fn centrality_ranks_depended_upon_modules_first() {
        // b and c both import a; a imports nothing. a is the most depended-upon.
        let (g, dir) = graph(&[
            ("a.py", "def core():\n    pass\n"),
            ("b.py", "from a import core\n\ndef useb():\n    core()\n"),
            ("c.py", "from a import core\n\ndef usec():\n    core()\n"),
        ]);
        let order = centrality_order(&g);
        let top = g.modules[order[0]].name.clone();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(top, "a", "most depended-upon module should rank first");
    }

    #[test]
    fn centrality_prefers_code_over_prose_on_ties() {
        // Neither has dependency edges, so both sit at the PageRank baseline and
        // the tie-break decides. Name order alone would put the doc first.
        let (g, dir) = graph(&[
            ("aaa_doc.md", "# Doc\n\n## A section\n"),
            ("zzz_code.py", "def solo():\n    pass\n"),
        ]);
        let order = centrality_order(&g);
        let first = g.modules[order[0]].name.clone();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(first, "zzz_code", "code should outrank prose at equal rank");
    }

    #[test]
    fn prune_keeps_the_most_central_and_counts_the_rest() {
        let (mut g, dir) = graph(&[
            ("a.py", "def core():\n    pass\n"),
            ("b.py", "from a import core\n\ndef useb():\n    core()\n"),
            ("c.py", "from a import core\n\ndef usec():\n    core()\n"),
        ]);
        let total = g.modules.len();
        let stats = crate::view::prune_to_central(&mut g, 1, 0.10);
        let kept: Vec<String> = g.modules.iter().map(|m| m.name.clone()).collect();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(stats.omitted, total - 1);
        assert_eq!(kept, ["a"], "the single kept module should be the central one");
    }

    /// A repo full of cross-linked docs and code that imports only external
    /// packages: which side "wins" a naive ranking is luck, so a codebase map
    /// must never come back as pure prose.
    #[test]
    fn pruned_map_always_retains_code_when_code_exists() {
        let mut files: Vec<(String, String)> = Vec::new();
        // Docs that link each other, so they earn real (non-baseline) PageRank.
        let index: String = (0..12)
            .map(|i| format!("- [Guide {i}](guide_{i}.md)\n"))
            .collect();
        files.push(("README.md".into(), format!("# Index\n\n{index}")));
        for i in 0..12 {
            let body: String = (0..30)
                .map(|j| format!("## Guide {i} section {j} with a long heading\n\n"))
                .collect();
            files.push((
                format!("guide_{i}.md"),
                format!("# Guide {i}\n\n{body}\nSee [index](README.md)\n"),
            ));
        }
        for i in 0..6 {
            files.push((
                format!("mod_{i}.py"),
                "import os, json\n\nclass Worker:\n    def run(self):\n        pass\n".into(),
            ));
        }
        let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let (mut g, dir) = graph(&refs);

        // Every nontrivial keep count must retain at least one code module.
        for keep in [1usize, 2, 5, 10] {
            let mut gg = g.clone();
            crate::view::prune_to_central(&mut gg, keep, 0.10);
            let code = gg.modules.iter().filter(|m| m.lang != Lang::Markdown).count();
            assert!(
                code > 0,
                "keep={keep} produced a map with no code modules at all"
            );
        }
        let stats = crate::view::prune_to_central(&mut g, 4, 0.10);
        let _ = fs::remove_dir_all(&dir);
        assert!(stats.omitted > 0);
    }

    #[test]
    fn prose_is_held_to_its_share_of_a_pruned_map() {
        // One small code module against several large docs: uncapped, prose would
        // dominate the map.
        let mut files: Vec<(String, String)> = vec![(
            "code.py".to_string(),
            "class TheOnlyClassHere:\n    pass\n".to_string(),
        )];
        for i in 0..8 {
            let body: String = (0..30)
                .map(|j| format!("## Heading number {i}_{j} with a fairly long title\n\n"))
                .collect();
            files.push((format!("doc_{i}.md"), format!("# Doc {i}\n\n{body}")));
        }
        let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let (mut g, dir) = graph(&refs);
        // keep must be < module count, or pruning is not required and the
        // ceiling deliberately does not engage.
        let stats = crate::view::prune_to_central(&mut g, 8, 0.10);
        let prose_left = g.modules.iter().filter(|m| m.lang == Lang::Markdown).count();
        let code_left = g.modules.iter().filter(|m| m.lang != Lang::Markdown).count();
        let _ = fs::remove_dir_all(&dir);
        assert!(stats.prose_capped > 0, "oversized prose should be capped");
        assert_eq!(code_left, 1, "code must survive the prose ceiling");
        assert!(prose_left < 8, "some docs should have been dropped");
    }

    #[test]
    fn docs_only_repo_is_exempt_from_the_prose_ceiling() {
        // No code to balance against: emptying the map would be worse than an
        // unbalanced one.
        let (mut g, dir) = graph(&[
            ("a.md", "# A\n\n## One\n\n## Two\n"),
            ("b.md", "# B\n\n## Three\n"),
        ]);
        let stats = crate::view::prune_to_central(&mut g, 2, 0.10);
        let left = g.modules.len();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(stats.prose_capped, 0, "a docs-only repo keeps its docs");
        assert_eq!(left, 2);
    }

    #[test]
    fn small_docs_survive_alongside_dominant_code() {
        // The allowance is 10% of the *code* size, so code has to genuinely
        // dominate for a doc to fit.
        let big: String = (0..60)
            .map(|j| format!("class ClassWithAGenerouslyLongName_{j}:\n    pass\n\n"))
            .collect();
        let (mut g, dir) = graph(&[("code.py", big.as_str()), ("tiny.md", "# T\n")]);
        let stats = crate::view::prune_to_central(&mut g, 2, 0.10);
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(stats.prose_capped, 0, "a doc well under 10% must be kept");
        assert_eq!(g.modules.len(), 2);
    }

    #[test]
    fn prune_is_a_noop_when_keep_covers_everything() {
        let (mut g, dir) = graph(&[("a.py", "def f():\n    pass\n")]);
        let before = g.modules.len();
        let stats = crate::view::prune_to_central(&mut g, before + 10, 0.10);
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(stats.omitted, 0);
        assert_eq!(g.modules.len(), before);
    }

    #[test]
    fn callers_discloses_ambiguity_and_stays_quiet_otherwise() {
        // Two `run` methods: an opaque receiver cannot be pinned to either, so
        // edges are dropped and the count is a lower bound the caller must know
        // about. `solo` has one definition, so no warning belongs on it.
        let (g, dir) = graph(&[(
            "a.py",
            "class A:\n    def run(self):\n        pass\n\n\nclass B:\n    def run(self):\n        pass\n\n\ndef solo():\n    pass\n",
        )]);
        let ambiguous = callers(&g, "run", false);
        let unique = callers(&g, "solo", false);
        let json = callers(&g, "run", true);
        let _ = fs::remove_dir_all(&dir);
        assert!(ambiguous.contains("INCOMPLETE"), "ambiguity must be disclosed");
        assert!(ambiguous.contains("2 definitions"));
        assert!(ambiguous.contains("rg -n"), "must offer the confirming command");
        assert!(
            !unique.contains("INCOMPLETE"),
            "a uniquely-named symbol must not raise a false alarm"
        );
        // Qualifying the query must not silence the warning: ambiguity is a
        // property of the name at the call site, not of how it was spelled.
        let qualified = callers(&g, "A::run", false);
        assert!(
            qualified.contains("INCOMPLETE"),
            "qualifying must not hide the ambiguity"
        );
        assert!(json.contains("\"lower_bound\": true"), "machine callers need the flag");
    }

    #[test]
    fn suppressed_common_names_say_they_are_not_indexed() {
        // `open` is on the suppressed ubiquitous-method list, so NO reverse edges
        // exist for it — an empty result must not read as "no callers".
        let (g, dir) = graph(&[(
            "m.py",
            "class Clock:\n    def open(self):\n        pass\n\n\ndef driver(c):\n    c.open()\n",
        )]);
        let text = callers(&g, "open", false);
        let json = callers(&g, "open", true);
        let _ = fs::remove_dir_all(&dir);
        assert!(text.contains("NOT INDEXED"), "suppression must be disclosed: {text}");
        assert!(json.contains("\"suppressed_common_name\": true"));
        assert!(json.contains("\"lower_bound\": true"));
        assert!(!json.contains("\"complete\": true"), "must never claim completeness");
    }

    #[test]
    fn cross_language_method_names_do_not_create_edges() {
        // A Python call to `apply_template` must not be attributed to a Rust
        // method of the same name — that invents a Python -> Rust dependency.
            let (g, dir) = graph(&[
            (
                "user.py",
                "import os\n\ndef go(tok):\n    tok.apply_template('x')\n",
            ),
            (
                "src/engine.rs",
                "pub struct Engine;\nimpl Engine {\n    pub fn apply_template(&self, s: &str) {}\n}\n",
            ),
        ]);
        let text = callers(&g, "apply_template", false);
        let py = g.modules.iter().find(|m| m.name == "user").unwrap();
        let deps = py.deps.clone();
        let _ = fs::remove_dir_all(&dir);
        assert!(
            deps.is_empty(),
            "a Python module must not gain a Rust dep from a shared method name: {deps:?}"
        );
        assert!(
            !text.contains("user.go"),
            "the Python call must not be reported as a caller of the Rust method: {text}"
        );
    }

    #[test]
    fn colliding_module_names_are_made_unique() {
        // `foo.py` and `foo.ts` both want to be module `foo`; indexing by name
        // used to make one unreachable, silently dropping its reverse edges.
        let (g, dir) = graph(&[
            ("foo.py", "def build_model():\n    pass\n"),
            ("foo.ts", "export function buildModel() {}\n"),
        ]);
        let names: Vec<&str> = g.modules.iter().map(|m| m.name.as_str()).collect();
        let unique: BTreeSet<&&str> = names.iter().collect();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(names.len(), unique.len(), "module names must be unique: {names:?}");
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

    #[test]
    fn opaque_receiver_edge_is_flagged_heuristic() {
        // `w.frobnicate()` — receiver type unknown; resolved only because the
        // method name is unique codebase-wide, so it must be marked `~`.
        let (g, dir) = graph(&[
            ("src/a.rs", "pub struct Widget;\nimpl Widget { pub fn frobnicate(&self) {} }\n"),
            ("src/b.rs", "use crate::a::Widget;\npub fn run(w: Widget) { w.frobnicate(); }\n"),
        ]);
        let out = callers(&g, "frobnicate", false);
        assert!(out.contains("b::run"), "{out}");
        assert!(out.contains("frobnicate~"), "{out}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn neighbors_split_upstream_and_downstream() {
        let (g, dir) = graph(&[
            ("src/a.rs", "pub fn base() {}\n"),
            ("src/b.rs", "use crate::a::base;\npub fn mid() { base(); }\n"),
            ("src/c.rs", "use crate::b::mid;\npub fn top() { mid(); }\n"),
        ]);
        let targets: BTreeSet<&str> = ["b"].into_iter().collect();
        let (up, down) = neighbors(&g, &targets);
        assert!(up.iter().any(|m| m.name == "a"), "a is upstream of b");
        assert!(down.iter().any(|m| m.name == "c"), "c is downstream of b");
        assert!(!up.iter().any(|m| m.name == "c"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn typescript_relative_import_resolves_module_and_callers() {
        let (g, dir) = graph(&[
            ("src/api/client.ts", "export function apiGet() {}\n"),
            (
                "src/App.tsx",
                "import { apiGet } from './api/client';\nexport function App() { apiGet(); }\n",
            ),
        ]);
        let app = g.modules.iter().find(|m| m.name == "App").unwrap();
        assert!(app.deps.iter().any(|d| d == "api.client"), "{:?}", app.deps);
        let out = callers(&g, "apiGet", false);
        assert!(out.contains("App.App"), "{out}");
        assert!(out.contains("api.client.apiGet"), "{out}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn self_receiver_edge_is_trusted() {
        // `self.helper()` is reliably the enclosing impl — never heuristic.
        let src = "pub struct A;\n\
                   impl A { pub fn helper(&self) {} pub fn run(&self) { self.helper(); } }\n";
        let (g, dir) = graph(&[("src/a.rs", src)]);
        let out = callers(&g, "helper", false);
        assert!(out.contains("A::run"), "{out}");
        assert!(!out.contains('~'), "{out}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn context_bundles_def_callees_and_callers() {
        let (g, dir) = graph(&[
            ("src/a.rs", "pub fn helper() {}\n"),
            ("src/b.rs", "use crate::a::helper;\npub fn run() { helper(); }\n"),
        ]);
        let out = context(&g, "run", 4000, false);
        assert!(out.contains("# Context: b::run"), "{out}");
        assert!(out.contains("## Calls"), "{out}");
        assert!(out.contains("a::helper"), "{out}");
        let out2 = context(&g, "helper", 4000, false);
        assert!(out2.contains("## Callers"), "{out2}");
        assert!(out2.contains("b::run"), "{out2}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn context_signature_types_are_pascal_case_types_only() {
        let src = "pub struct Widget { n: i32 }\n\
                   pub fn make(count: i32, w: Widget) -> Widget { w }\n";
        let (g, dir) = graph(&[("src/a.rs", src)]);
        let out = context(&g, "make", 4000, false);
        // `Widget` is a type ref; `count`/`w`/`i32` must not appear as types.
        assert!(out.contains("a::Widget [struct]"), "{out}");
        assert!(!out.contains("## Signature types\n- a::count"), "{out}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn core_ranks_hub_above_leaves() {
        let (g, dir) = graph(&[
            ("src/hub.rs", "pub fn h() {}\n"),
            ("src/a.rs", "use crate::hub::h;\npub fn a() { h(); }\n"),
            ("src/b.rs", "use crate::hub::h;\npub fn b() { h(); }\n"),
        ]);
        let out = core(&g, 10, None, false);
        let hub = out.find("  hub  [").unwrap();
        let a = out.find("  a  [").unwrap();
        assert!(hub < a, "hub (2 dependents) should outrank a:\n{out}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn api_report_flags_removed_changed_added() {
        let base = graph(&[(
            "src/api.rs",
            "pub fn stable() {}\npub fn gone() {}\npub fn morph(a: i32) {}\n",
        )]);
        let cur = graph(&[(
            "src/api.rs",
            "pub fn stable() {}\npub fn morph(a: i32, b: i32) {}\npub fn fresh() {}\n",
        )]);
        let out = api_report(&base.0, &cur.0, "HEAD", false);
        assert!(out.contains("## Removed") && out.contains("api::gone"), "{out}");
        assert!(out.contains("## Changed") && out.contains("api::morph"), "{out}");
        assert!(out.contains("## Added") && out.contains("api::fresh"), "{out}");
        assert!(!out.contains("stable"), "unchanged item must not appear:\n{out}");
        let _ = fs::remove_dir_all(base.1);
        let _ = fs::remove_dir_all(cur.1);
    }

    #[test]
    fn markdown_links_resolve_headings_and_flag_broken() {
        let (g, dir) = graph(&[
            (
                "guide.md",
                "# Guide\n\nSee [setup](./setup.md#install) and [ghost](./ghost.md).\n",
            ),
            ("setup.md", "# Setup\n\n## Install\n\ndo it\n"),
        ]);
        // Resolved cross-doc link forms a dep edge; the missing file does not.
        let guide = g.modules.iter().find(|m| m.name == "guide").unwrap();
        assert!(guide.deps.iter().any(|d| d == "setup"), "{:?}", guide.deps);
        assert!(!guide.deps.iter().any(|d| d == "ghost"), "{:?}", guide.deps);
        // The heading anchor is a backlink target.
        let out = callers(&g, "install", false);
        assert!(out.contains("guide"), "{out}");
        // The dead link surfaces in the coverage report's broken-links section.
        let cov = coverage_report(&g, &[], false);
        assert!(cov.contains("## Broken links"), "{cov}");
        assert!(cov.contains("./ghost.md"), "{cov}");
        assert!(!cov.contains("./setup.md"), "resolved link must not be broken:\n{cov}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn coverage_separates_internal_std_and_blind_spots() {
        let src = "pub fn helper() {}\n\
                   pub fn run() { helper(); let v: Vec<i32> = Vec::new(); v.len(); }\n";
        let (g, dir) = graph(&[("src/a.rs", src)]);
        let out = coverage_report(&g, &[("cpp".to_string(), 2)], false);
        assert!(out.contains("internal edges:"), "{out}");
        assert!(out.contains("std / builtin:"), "{out}"); // v.len()
        assert!(out.contains(".cpp"), "{out}"); // blind spot listed
        let _ = fs::remove_dir_all(dir);
    }
}
