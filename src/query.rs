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
    let sep = Lang::sep(m.lang);
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
    let sep = Lang::sep(m.lang);
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


/// Kinds of every definition of this name (type-like or not).
fn definition_kinds(g: &Graph, q: &[&str]) -> Vec<String> {
    let Some(&last) = q.last() else { return Vec::new() };
    let mut hits = Vec::new();
    for m in &g.modules {
        collect_defs(&m.items, m, None, last, None, &mut hits);
    }
    hits.into_iter().map(|h| h.kind).collect()
}

fn is_type_kind(kind: &str) -> bool {
    matches!(
        kind,
        "struct" | "enum" | "trait" | "interface" | "type" | "class"
    )
}

/// Every item whose SIGNATURE mentions `type_name` — the reverse of the forward
/// index `context` already builds.
///
/// A type has no call edges, so `callers SteerConfig` used to answer "no callers
/// found" for a struct referenced by a dozen files. That reads as "safe to
/// change" for the single most common breaking change there is: altering a type.
/// Signatures are where a type change actually breaks callers — fields,
/// parameters, return types — so scanning them is both cheap and the right scope.
fn type_references<'a>(g: &'a Graph, type_name: &str) -> Vec<(&'a Module, &'a Item)> {
    fn walk<'a>(
        items: &'a [Item],
        m: &'a Module,
        type_name: &str,
        out: &mut Vec<(&'a Module, &'a Item)>,
    ) {
        for it in items {
            // Skip the definition itself.
            let is_self = it.name.as_deref() == Some(type_name) && is_type_kind(&it.kind);
            if !is_self && identifiers(&it.signature).iter().any(|n| n == type_name) {
                out.push((m, it));
            }
            walk(&it.children, m, type_name, out);
        }
    }
    let mut out = Vec::new();
    for m in &g.modules {
        walk(&m.items, m, type_name, &mut out);
    }
    out
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
    // A type is never "called", so call edges alone answer the wrong question.
    let kinds = definition_kinds(g, &q);
    let is_type = !kinds.is_empty() && kinds.iter().all(|k| is_type_kind(k));
    let refs = if is_type {
        type_references(g, bare_name)
    } else {
        Vec::new()
    };

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
        let (unresolved, external, zones) = name_diagnostics(g, q.last().copied().unwrap_or(""));
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
            // Measured, per-name: how many call sites bearing this name ctx
            // could not pin, and how many it proved external. `complete` is
            // scoped to that question — it is not a claim about the two flags
            // above, which cover losses this count cannot see.
            "completeness": {
                "unresolved_call_sites_with_this_name": unresolved,
                "external_call_sites_with_this_name": external,
                "unresolved_in_modules": zones.iter().map(|(m, c)| json!({"module": m, "count": c})).collect::<Vec<_>>(),
                "complete": unresolved == 0 && external == 0,
            },
            "references": refs
                .iter()
                .map(|(m, it)| json!({
                    "module": m.name,
                    "file": m.file,
                    "line": it.line,
                    "kind": it.kind,
                    "signature": it.signature,
                }))
                .collect::<Vec<_>>(),
        }))
        .unwrap_or_default()
            + "\n";
    }

    let last = *q.last().unwrap();
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

    if !refs.is_empty() {
        let mut out = format!(
            "'{query}' is a type, so it has no call edges. {} item(s) reference it \
             in a signature — these are what a change to it can break:\n\n",
            refs.len()
        );
        for (m, it) in &refs {
            out.push_str(&format!(
                "{}  ({}:{})\n    {}\n",
                m.name, m.file, it.line, it.signature
            ));
        }
        if !found.is_empty() {
            out.push_str(&format!("\nplus {} resolved call edge(s).\n", found.len()));
        }
        out.push_str(
            "\nSignature references only: uses inside function BODIES are not indexed.\n",
        );
        return out + &recall_note;
    }

    if found.is_empty() {
        let type_note = if is_type {
            "\n(this name is a type: types have no call edges, and nothing references it \
             in a signature)\n"
        } else {
            ""
        };
        return format!(
            "no callers found for '{query}'\n\
             (only resolved call edges are indexed; ambiguous calls and \
             ubiquitous std-named methods are intentionally dropped)\n{type_note}{recall_note}"
        ) + &completeness_note(g, last, "result");    }
    let mut out = format!("{} caller(s) of '{}':\n\n", found.len(), query);
    for c in &found {
        let edges: Vec<String> = c.edges.iter().map(edge_str).collect();
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
    if found.iter().any(|c| c.edges.iter().any(|e| e.dispatch)) {
        out.push_str("(* = dispatch edge: one branch of a trait/interface fan-out)\n");
    }
    out.push_str(&recall_note);
    out.push_str(&completeness_note(g, last, "blast radius"));
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

// ---- call graph (trace / path) ---------------------------------------------

/// One callable in the call graph: a function, method, or `let`-bound closure.
pub struct FnNode {
    pub qualname: String,
    pub file: String,
    pub line: usize,
    pub module: String,
    segs: Vec<String>,
}

/// How a caller reaches a callee. Carried through traversals so a trace can
/// say which hops are worth double-checking.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// Backed by an import, a path, or a declared receiver type.
    Proven,
    /// Attributed by receiver inference — verify before relying on it.
    Heuristic,
    /// One branch of a dynamic-dispatch fan-out: reachable, but only one
    /// sibling branch actually runs on any given call.
    Dispatch,
}

impl Confidence {
    fn mark(self) -> &'static str {
        match self {
            Confidence::Proven => "",
            Confidence::Heuristic => "~",
            Confidence::Dispatch => "*",
        }
    }
}

/// The resolved call graph: callables as nodes, resolved call edges as arcs.
/// `trace` and `path` are walks over this; nothing here re-parses source.
pub struct CallGraph {
    pub nodes: Vec<FnNode>,
    out: Vec<Vec<(usize, Confidence)>>,
    incoming: Vec<Vec<(usize, Confidence)>>,
    /// Per node, call edges that matched no node in the graph — the honest
    /// edge of the map, reported rather than silently dropped.
    dangling: Vec<Vec<String>>,
}

/// An edge display may carry a fan-out annotation (`Trait::m [7 impls]`);
/// strip it before matching against node names.
fn edge_target(to: &str) -> &str {
    to.split(" [").next().unwrap_or(to).trim()
}

fn collect_nodes<'a>(
    items: &'a [Item],
    m: &Module,
    path: &mut Vec<String>,
    out: &mut Vec<(FnNode, &'a Item)>,
) {
    let sep = Lang::sep(m.lang);
    for it in items {
        let named = it.name.clone();
        if matches!(it.kind.as_str(), "fn" | "def") {
            if let Some(n) = &named {
                let mut qual = m.name.clone();
                for p in path.iter() {
                    qual.push_str(sep);
                    qual.push_str(p);
                }
                qual.push_str(sep);
                qual.push_str(n);
                out.push((
                    FnNode {
                        segs: segments(&qual).into_iter().map(str::to_string).collect(),
                        qualname: qual,
                        file: m.file.clone(),
                        line: it.line,
                        module: m.name.clone(),
                    },
                    it,
                ));
            }
        }
        // Containers and enclosing functions both qualify what is nested in
        // them, so a nested helper reads as `outer::helper`.
        match (
            matches!(it.kind.as_str(), "impl" | "trait" | "class" | "interface" | "mod" | "fn" | "def"),
            &named,
        ) {
            (true, Some(n)) => {
                path.push(n.clone());
                collect_nodes(&it.children, m, path, out);
                path.pop();
            }
            _ => collect_nodes(&it.children, m, path, out),
        }
    }
}

pub fn call_graph(g: &Graph) -> CallGraph {
    let mut raw: Vec<(FnNode, &Item)> = Vec::new();
    for m in &g.modules {
        let mut path = Vec::new();
        collect_nodes(&m.items, m, &mut path, &mut raw);
    }
    // Index by bare name so a call edge can be matched by suffix cheaply.
    let mut by_last: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, (n, _)) in raw.iter().enumerate() {
        if let Some(last) = n.segs.last() {
            by_last.entry(last.as_str()).or_default().push(i);
        }
    }

    let n = raw.len();
    let mut out: Vec<Vec<(usize, Confidence)>> = vec![Vec::new(); n];
    let mut incoming: Vec<Vec<(usize, Confidence)>> = vec![Vec::new(); n];
    let mut dangling: Vec<Vec<String>> = vec![Vec::new(); n];

    for (i, (node, it)) in raw.iter().enumerate() {
        for c in &it.calls {
            let q: Vec<&str> = segments(edge_target(&c.to));
            let Some(&last) = q.last() else { continue };
            let cands = by_last.get(last).cloned().unwrap_or_default();
            let mut hits: Vec<usize> = cands
                .into_iter()
                .filter(|&j| {
                    let s = &raw[j].0.segs;
                    s.len() >= q.len() && s[s.len() - q.len()..] == *q
                })
                .collect();
            if hits.is_empty() {
                dangling[i].push(c.to.clone());
                continue;
            }
            // An unqualified edge that matches several definitions almost
            // always means the one in the same module.
            if hits.len() > 1 {
                let same: Vec<usize> = hits
                    .iter()
                    .copied()
                    .filter(|&j| raw[j].0.module == node.module)
                    .collect();
                if !same.is_empty() {
                    hits = same;
                }
            }
            let conf = if c.dispatch {
                Confidence::Dispatch
            } else if c.heuristic || hits.len() > 1 {
                Confidence::Heuristic
            } else {
                Confidence::Proven
            };
            for j in hits {
                if !out[i].iter().any(|&(k, _)| k == j) {
                    out[i].push((j, conf));
                    incoming[j].push((i, conf));
                }
            }
        }
    }

    CallGraph {
        nodes: raw.into_iter().map(|(n, _)| n).collect(),
        out,
        incoming,
        dangling,
    }
}

impl CallGraph {
    fn arcs(&self, i: usize, reverse: bool) -> &[(usize, Confidence)] {
        if reverse { &self.incoming[i] } else { &self.out[i] }
    }

    /// Nodes whose qualified name ends with the query's segments.
    fn find(&self, query: &str) -> Vec<usize> {
        let q = segments(query);
        if q.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<usize> = (0..self.nodes.len())
            .filter(|&i| {
                let s = &self.nodes[i].segs;
                s.len() >= q.len() && s[s.len() - q.len()..] == *q
            })
            .collect();
        hits.sort_by(|&a, &b| self.nodes[a].qualname.cmp(&self.nodes[b].qualname));
        hits
    }
}

struct TraceLine {
    depth: usize,
    node: usize,
    conf: Confidence,
    /// Why this branch stopped: "seen" (already expanded), "depth", or none.
    stop: Option<&'static str>,
    /// Number of call edges out of this node that left the graph.
    dangling: usize,
    last_of_parent: bool,
}

/// Depth-first walk producing a printable call tree. Cycles and repeated
/// subtrees are cut with a marker rather than expanded again, so recursion is
/// safe and the output stays finite.
fn walk_tree(cg: &CallGraph, root: usize, depth: usize, reverse: bool) -> Vec<TraceLine> {
    let mut out = Vec::new();
    let mut expanded: BTreeSet<usize> = BTreeSet::new();

    /// One step of the walk: which node, how it was reached, and where in the
    /// tree it sits.
    struct Step {
        node: usize,
        conf: Confidence,
        depth: usize,
        last: bool,
    }

    fn rec(
        cg: &CallGraph,
        st: Step,
        max: usize,
        reverse: bool,
        expanded: &mut BTreeSet<usize>,
        out: &mut Vec<TraceLine>,
    ) {
        let Step { node: i, conf, depth: d, last } = st;
        let kids = cg.arcs(i, reverse);
        let seen = expanded.contains(&i);
        let stop = if seen && !kids.is_empty() {
            Some("seen above")
        } else if d >= max && !kids.is_empty() {
            Some("depth limit")
        } else {
            None
        };
        out.push(TraceLine {
            depth: d,
            node: i,
            conf,
            stop,
            dangling: cg.dangling[i].len(),
            last_of_parent: last,
        });
        if stop.is_some() {
            return;
        }
        expanded.insert(i);
        let kids = kids.to_vec();
        for (k, (j, c)) in kids.iter().enumerate() {
            let step = Step {
                node: *j,
                conf: *c,
                depth: d + 1,
                last: k + 1 == kids.len(),
            };
            rec(cg, step, max, reverse, expanded, out);
        }
    }
    let root_step = Step {
        node: root,
        conf: Confidence::Proven,
        depth: 0,
        last: true,
    };
    rec(cg, root_step, depth, reverse, &mut expanded, &mut out);
    out
}

/// Draw the tree with box-drawing prefixes. `open[d]` tracks whether an
/// ancestor at depth d still has siblings below it.
fn render_tree(cg: &CallGraph, lines: &[TraceLine], out: &mut String) {
    // open[d] == "the ancestor at depth d+1 still has siblings below it", so a
    // continuation bar is drawn under it.
    let mut open: Vec<bool> = Vec::new();
    for l in lines {
        open.truncate(l.depth.saturating_sub(1));
        let mut prefix = String::new();
        for d in 0..l.depth {
            if d + 1 == l.depth {
                prefix.push_str(if l.last_of_parent { "└─ " } else { "├─ " });
            } else {
                prefix.push_str(if open.get(d).copied().unwrap_or(false) { "│  " } else { "   " });
            }
        }
        if l.depth > 0 {
            open.push(!l.last_of_parent);
        }
        let n = &cg.nodes[l.node];
        let mut suffix = String::new();
        if let Some(s) = l.stop {
            suffix.push_str(&format!("  ({s})"));
        }
        if l.dangling > 0 && l.stop.is_none() {
            suffix.push_str(&format!("  [+{} outside graph]", l.dangling));
        }
        out.push_str(&format!(
            "{prefix}{}{}  [{}:{}]{}\n",
            n.qualname,
            l.conf.mark(),
            n.file,
            n.line,
            suffix
        ));
    }
}

/// `ctx trace <sym>` — the call tree rooted at a symbol: what it reaches
/// (forward) or what reaches it (`--reverse`), to a bounded depth.
pub fn trace(g: &Graph, query: &str, depth: usize, reverse: bool, json_out: bool) -> String {
    let cg = call_graph(g);
    let roots = cg.find(query);
    if roots.is_empty() {
        return format!(
            "no callable named '{query}' in the call graph\n\
             (ctx trace walks functions, methods, and named closures; try `ctx def {query}`)\n"
        );
    }

    if json_out {
        let blocks: Vec<_> = roots
            .iter()
            .take(3)
            .map(|&r| {
                let lines = walk_tree(&cg, r, depth, reverse);
                json!({
                    "root": cg.nodes[r].qualname,
                    "file": cg.nodes[r].file,
                    "line": cg.nodes[r].line,
                    "tree": lines.iter().map(|l| json!({
                        "depth": l.depth,
                        "qualname": cg.nodes[l.node].qualname,
                        "file": cg.nodes[l.node].file,
                        "line": cg.nodes[l.node].line,
                        "confidence": match l.conf {
                            Confidence::Proven => "proven",
                            Confidence::Heuristic => "heuristic",
                            Confidence::Dispatch => "dispatch",
                        },
                        "stopped": l.stop,
                        "edges_outside_graph": l.dangling,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        return serde_json::to_string_pretty(&json!({
            "query": query,
            "direction": if reverse { "callers" } else { "callees" },
            "depth": depth,
            "traces": blocks,
        }))
        .unwrap_or_default()
            + "\n";
    }

    let dir = if reverse { "callers of" } else { "call tree from" };
    let mut out = format!("# {dir} '{query}'  (depth {depth})\n");
    out.push_str("~ heuristic edge (verify) · * one branch of a dispatch fan-out\n\n");
    for &r in roots.iter().take(3) {
        let lines = walk_tree(&cg, r, depth, reverse);
        render_tree(&cg, &lines, &mut out);
        out.push('\n');
    }
    if roots.len() > 3 {
        out.push_str(&format!("({} more definitions matched '{query}')\n", roots.len() - 3));
    }
    out
}

/// `ctx path <from> <to>` — the shortest call path between two symbols, or an
/// honest statement that the resolved graph contains none.
pub fn path(g: &Graph, from: &str, to: &str, json_out: bool) -> String {
    let cg = call_graph(g);
    let starts = cg.find(from);
    let goals: BTreeSet<usize> = cg.find(to).into_iter().collect();
    if starts.is_empty() || goals.is_empty() {
        let missing = if starts.is_empty() { from } else { to };
        return format!("no callable named '{missing}' in the call graph\n");
    }

    // BFS from every match of `from`; the first goal reached is a shortest path.
    let mut prev: HashMap<usize, (usize, Confidence)> = HashMap::new();
    let mut seen: BTreeSet<usize> = starts.iter().copied().collect();
    let mut queue: std::collections::VecDeque<usize> = starts.iter().copied().collect();
    let mut found: Option<usize> = None;
    'bfs: while let Some(i) = queue.pop_front() {
        if goals.contains(&i) && !starts.contains(&i) {
            found = Some(i);
            break 'bfs;
        }
        for &(j, c) in &cg.out[i] {
            if seen.insert(j) {
                prev.insert(j, (i, c));
                queue.push_back(j);
            }
        }
    }
    // A start that is itself the goal is a zero-hop answer.
    let found = found.or_else(|| starts.iter().copied().find(|i| goals.contains(i)));

    let Some(end) = found else {
        if json_out {
            return serde_json::to_string_pretty(&json!({
                "from": from, "to": to, "found": false, "hops": null, "path": [],
            }))
            .unwrap_or_default()
                + "\n";
        }
        return format!(
            "no call path from '{from}' to '{to}' in the resolved graph\n\
             (the graph only contains edges ctx could prove; run `ctx doctor` to see\n\
             how much of this repo resolved, and which callee names went unpinned)\n"
        );
    };

    // Rebuild the route back to whichever start it came from. Each entry
    // carries the confidence of the hop *into* that node, so the marker lands
    // on the callee rather than the caller.
    let mut route: Vec<(usize, Confidence)> = Vec::new();
    let mut cur = end;
    loop {
        match prev.get(&cur) {
            Some(&(p, c)) => {
                route.push((cur, c));
                cur = p;
            }
            // The start node has no inbound hop.
            None => {
                route.push((cur, Confidence::Proven));
                break;
            }
        }
    }
    route.reverse();
    let hops = route.len().saturating_sub(1);

    if json_out {
        return serde_json::to_string_pretty(&json!({
            "from": from,
            "to": to,
            "found": true,
            "hops": hops,
            "path": route.iter().enumerate().map(|(k, (i, c))| json!({
                "qualname": cg.nodes[*i].qualname,
                "file": cg.nodes[*i].file,
                "line": cg.nodes[*i].line,
                "confidence": if k == 0 { "proven" } else { match c {
                    Confidence::Proven => "proven",
                    Confidence::Heuristic => "heuristic",
                    Confidence::Dispatch => "dispatch",
                }},
            })).collect::<Vec<_>>(),
        }))
        .unwrap_or_default()
            + "\n";
    }

    let mut out = format!("# path: {from} → {to}  ({hops} hop(s))\n");
    out.push_str("~ heuristic edge (verify) · * one branch of a dispatch fan-out\n\n");
    for (k, (i, c)) in route.iter().enumerate() {
        let n = &cg.nodes[*i];
        let arrow = if k == 0 { String::new() } else { format!("{}→ ", "  ".repeat(k)) };
        let mark = if k == 0 { "" } else { c.mark() };
        out.push_str(&format!("{arrow}{}{}  [{}:{}]\n", n.qualname, mark, n.file, n.line));
    }
    out
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
        rec(&m.items, m, None, Lang::sep(m.lang), &mut idx);
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
        rec(&m.items, m, None, Lang::sep(m.lang), last, parent, &mut out);
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
    crate::render::edge_str(c)
}

/// What the diagnostics say about call sites bearing a given callee name:
/// how many went unresolved (and where), and how many were classified as
/// external. The raw material for telling a caller whether an answer is
/// complete or merely everything ctx could prove.
fn name_diagnostics<'a>(g: &'a Graph, name: &str) -> (usize, usize, Vec<(&'a str, usize)>) {
    let mut unresolved = 0usize;
    let mut external = 0usize;
    let mut zones: Vec<(&str, usize)> = Vec::new();
    for m in &g.modules {
        if let Some(&c) = m.diag.unresolved_names.get(name) {
            unresolved += c;
            zones.push((m.name.as_str(), c));
        }
        if let Some(&c) = m.diag.extern_names.get(name) {
            external += c;
        }
    }
    zones.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    (unresolved, external, zones)
}

/// A one-line verdict on whether a name-keyed answer can be trusted as
/// complete. An agent should not have to run `ctx doctor` to find out that the
/// specific symbol it asked about is one of the unresolved ones.
fn completeness_note(g: &Graph, name: &str, subject: &str) -> String {
    let (unresolved, external, zones) = name_diagnostics(g, name);
    if unresolved > 0 {
        let where_ = zones
            .iter()
            .take(4)
            .map(|(m, c)| format!("{m} {c}"))
            .collect::<Vec<_>>()
            .join(", ");
        return format!(
            "\ncompleteness: {unresolved} call site(s) named `{name}` could not be pinned \
             ({where_}) —\nthis {subject} may be incomplete; grep `{name}` to confirm.\n"
        );
    }
    if external > 0 {
        return format!(
            "\ncompleteness: every internal call site named `{name}` resolved, but {external} \
             call(s)\nto that name were classified external (a std/third-party name collision). \
             If `{name}`\nis also invoked on a value ctx could not type, those calls are not listed.\n"
        );
    }
    format!(
        "\ncompleteness: no call site named `{name}` went unresolved anywhere in this tree —\n\
         this {subject} is complete to the limit of what ctx parses.\n"
    )
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
                    "calls": it.calls.iter().map(|c| json!({"to": c.to, "heuristic": c.heuristic, "dispatch": c.dispatch})).collect::<Vec<_>>(),
                    "callers": callers.iter().map(|c| json!({"caller": c.qualname, "file": c.file, "line": c.line})).collect::<Vec<_>>(),
                })
            })
            .collect();
        let (unresolved, external, zones) = name_diagnostics(g, last);
        return serde_json::to_string_pretty(&json!({
            "query": query,
            "context": blocks,
            "completeness": {
                "unresolved_call_sites_with_this_name": unresolved,
                "external_call_sites_with_this_name": external,
                "unresolved_in_modules": zones.iter().map(|(m, c)| json!({"module": m, "count": c})).collect::<Vec<_>>(),
                "complete": unresolved == 0 && external == 0,
            },
        }))
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
    out.push_str(&completeness_note(g, last, "caller list"));
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
///
/// The headline is *internal recall* — resolved edges over call sites that
/// could have been internal at all. The older "internal edges / all call
/// sites" figure counted `vec.push()` and `serde_json::to_string()` against
/// ctx, so it read like a failure rate when it was mostly a measure of how
/// much std and third-party code the repo calls.
pub fn coverage_report(
    g: &Graph,
    unsupported: &[(String, usize)],
    explain: bool,
    json_out: bool,
) -> String {
    let mut t = Totals::default();
    let mut by_lang: BTreeMap<&str, usize> = BTreeMap::new();
    for m in &g.modules {
        t.add(&m.diag);
        *by_lang.entry(m.lang.name()).or_default() += 1;
    }
    let pct = |n: usize, d: usize| if d == 0 { 0.0 } else { 100.0 * n as f64 / d as f64 };
    let internal = t.resolved + t.unresolved;

    // Low-confidence zones: high heuristic ratio (min 10 resolved), and the
    // modules holding the most genuine misses.
    let mut heur_zones: Vec<&Module> = g
        .modules
        .iter()
        .filter(|m| m.diag.resolved >= 10 && m.diag.heuristic > 0)
        .collect();
    heur_zones.sort_by(|a, b| {
        pct(b.diag.heuristic, b.diag.resolved)
            .partial_cmp(&pct(a.diag.heuristic, a.diag.resolved))
            .unwrap()
            .then(b.diag.heuristic.cmp(&a.diag.heuristic))
    });
    let mut miss_zones: Vec<&Module> = g
        .modules
        .iter()
        .filter(|m| m.diag.unresolved > 0)
        .collect();
    miss_zones.sort_by_key(|m| std::cmp::Reverse(m.diag.unresolved));

    // The names behind the misses — the actionable half of the report.
    let mut miss_census: BTreeMap<&str, usize> = BTreeMap::new();
    let mut extern_census: BTreeMap<&str, usize> = BTreeMap::new();
    for m in &g.modules {
        for (n, c) in &m.diag.unresolved_names {
            *miss_census.entry(n.as_str()).or_default() += c;
        }
        for (n, c) in &m.diag.extern_names {
            *extern_census.entry(n.as_str()).or_default() += c;
        }
    }
    let ranked = |c: &BTreeMap<&str, usize>| {
        let mut v: Vec<(String, usize)> =
            c.iter().map(|(k, v)| ((*k).to_string(), *v)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v
    };
    let miss_ranked = ranked(&miss_census);
    let extern_ranked = ranked(&extern_census);

    // Markdown broken links: targets that resolve to no file or heading. These
    // are true dead links (out-of-scope `../` links that exist on disk are not
    // counted), so they warrant a dedicated report.
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
            "call_sites": t.sites,
            "resolved": t.resolved,
            "heuristic": t.heuristic,
            "dispatch": t.dispatch,
            "external": t.external,
            "unresolved": t.unresolved,
            "internal_call_sites": internal,
            "internal_recall_pct": (pct(t.resolved, internal) * 10.0).round() / 10.0,
            "unresolved_names": miss_ranked.iter().map(|(n, c)| json!({"name": n, "count": c})).collect::<Vec<_>>(),
            "external_names": if explain {
                extern_ranked.iter().map(|(n, c)| json!({"name": n, "count": c})).collect::<Vec<_>>()
            } else { Vec::new() },
            "broken_links": broken.iter().map(|(f, l, tg)| json!({"file": f, "line": l, "target": tg})).collect::<Vec<_>>(),
            "unsupported_files": unsupported.iter().map(|(e, n)| json!({"ext": e, "count": n})).collect::<Vec<_>>(),
        }))
        .unwrap_or_default()
            + "\n";
    }

    let mut out = format!("# ctx coverage report — {}\n\n", g.root);
    // This report exists to be honest about scope; a filter that removed part of
    // the tree must be stated, or "no blind spots" means nothing.
    if let Some(f) = crate::extract::filter_note() {
        out.push_str(&format!(
            "SCOPE: FILTERED ({f}). Files outside this filter were never scanned and are \
             NOT counted as blind spots below. Re-run without filters for whole-repo coverage.\n\n"
        ));
    }
    let langs: Vec<String> = by_lang.iter().map(|(l, n)| format!("{l} {n}")).collect();
    out.push_str(&format!(
        "Modules: {}  ({})\n\n",
        g.modules.len(),
        langs.join(", ")
    ));

    out.push_str("## Internal recall — the number to trust\n");
    out.push_str(&format!(
        "  {}/{} = {:.1}%   of call sites that could be internal, ctx pinned this many.\n\n",
        t.resolved,
        internal,
        pct(t.resolved, internal)
    ));
    out.push_str("A call site is \"could be internal\" when the callee name is defined somewhere\n");
    out.push_str("under this root. Calls into std or a third-party crate are excluded, because no\n");
    out.push_str("internal edge could exist for them however good the resolver gets.\n\n");

    out.push_str("## Every call site, bucketed\n");
    out.push_str(&format!("call sites:            {}\n", t.sites));
    out.push_str(&format!(
        "  internal edges:      {:<6} [{} heuristic (~), {} dispatch fan-out (*)]\n",
        t.resolved, t.heuristic, t.dispatch
    ));
    out.push_str(&format!(
        "  external (provable): {:<6} ({:.1}%)  callee defined nowhere here — std/extern\n",
        t.external,
        pct(t.external, t.sites)
    ));
    out.push_str(&format!(
        "  unresolved internal: {:<6} ({:.1}%)  the real misses — see below\n",
        t.unresolved,
        pct(t.unresolved, t.sites)
    ));

    if !miss_ranked.is_empty() {
        let shown = if explain { miss_ranked.len() } else { 15 };
        out.push_str("\n## What ctx missed (callee names that exist here but went unpinned)\n");
        out.push_str("grep these; every other edge in the map is one ctx could prove.\n");
        for (n, c) in miss_ranked.iter().take(shown) {
            out.push_str(&format!("  {c:>5}  {n}\n"));
        }
        if miss_ranked.len() > shown {
            out.push_str(&format!(
                "  … and {} more distinct name(s) — rerun with --explain for the full census\n",
                miss_ranked.len() - shown
            ));
        }
    }

    if !miss_zones.is_empty() {
        out.push_str("\n## Where the misses are\n");
        for m in miss_zones.iter().take(8) {
            let mi = m.diag.resolved + m.diag.unresolved;
            out.push_str(&format!(
                "  {:<32} {:>4} unresolved   (module recall {:.0}%)\n",
                m.name,
                m.diag.unresolved,
                pct(m.diag.resolved, mi)
            ));
        }
    }

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

    if explain && !extern_ranked.is_empty() {
        out.push_str(&format!(
            "\n## External census ({} distinct names, provably not internal)\n",
            extern_ranked.len()
        ));
        for (n, c) in extern_ranked.iter().take(40) {
            out.push_str(&format!("  {c:>5}  {n}\n"));
        }
        if extern_ranked.len() > 40 {
            out.push_str(&format!("  … and {} more\n", extern_ranked.len() - 40));
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

#[derive(Default)]
struct Totals {
    sites: usize,
    resolved: usize,
    heuristic: usize,
    dispatch: usize,
    external: usize,
    unresolved: usize,
}

impl Totals {
    fn add(&mut self, d: &crate::model::Diagnostics) {
        self.sites += d.call_sites;
        self.resolved += d.resolved;
        self.heuristic += d.heuristic;
        self.dispatch += d.dispatch;
        self.external += d.external;
        self.unresolved += d.unresolved;
    }
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
        rec(&m.items, m, None, Lang::sep(m.lang), false, &mut map);
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
/// Public-API diff. Returns the rendered report, the count of **removed** items,
/// and the count of **signature-changed** items.
///
/// The two are separate because only one of them is unambiguous. A removal always
/// breaks callers. A signature change might be adding an optional parameter or a
/// struct field — routine and compatible — and ctx has no type system, so it
/// cannot tell that from changing a parameter's type. Collapsing both into one
/// "breaking" number makes a CI gate fail on ordinary additive commits, and a
/// gate that cries wolf gets switched off, which is worse than no gate.
///
/// Counts are returned rather than left for callers to parse out of the prose.
pub fn api_report(
    base: &Graph,
    current: &Graph,
    label: &str,
    json_out: bool,
) -> (String, usize, usize) {
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
        return (serde_json::to_string_pretty(&json!({
            "since": label,
            "removed": removed.iter().map(|(q, b)| json!({"name": q, "kind": b.kind, "signature": b.signature, "callers": caller_names(base, &b.match_q)})).collect::<Vec<_>>(),
            "changed": changed.iter().map(|(q, b, c)| json!({"name": q, "kind": c.kind, "was": b.signature, "now": c.signature, "callers": caller_names(current, &c.match_q)})).collect::<Vec<_>>(),
            "added": added.iter().map(|(q, c)| json!({"name": q, "kind": c.kind, "signature": c.signature})).collect::<Vec<_>>(),
        }))
        .unwrap_or_default()
            + "\n", removed.len(), changed.len());
    }

    if removed.is_empty() && changed.is_empty() && added.is_empty() {
        return (format!("no public API changes vs {label}\n"), 0, 0);
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
    (out, removed.len(), changed.len())
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

    /// A three-language dispatch fixture: an abstraction, two implementations
    /// that override it, a concrete field, a local binding, and a nested
    /// helper. Reused by the dispatch/typing/trace tests below.
    const RUST_DISPATCH: &str = r#"
pub trait Sampler { fn step(&self, x: f32) -> f32; fn name(&self) -> &'static str { tag() } }
pub struct Greedy;
impl Sampler for Greedy { fn step(&self, x: f32) -> f32 { pick_max(x) } }
pub struct TopK { pub k: usize }
impl Sampler for TopK { fn step(&self, x: f32) -> f32 { self.trim(x) } }
impl TopK { fn trim(&self, x: f32) -> f32 { x } }
fn pick_max(x: f32) -> f32 { x }
fn tag() -> &'static str { "s" }

pub struct Engine { sampler: Box<dyn Sampler>, top: TopK }
impl Engine {
    pub fn run(&self, x: f32) -> f32 { self.sampler.step(x) }
    pub fn run_with(&self, s: &dyn Sampler, x: f32) -> f32 { s.step(x) }
    pub fn run_generic<S: Sampler>(&self, s: S, x: f32) -> f32 { s.step(x) }
    pub fn run_concrete(&self, x: f32) -> f32 { self.top.trim(x) }
    pub fn run_local(&self, x: f32) -> f32 { let k = TopK { k: 2 }; k.trim(x) }
    pub fn label(&self, s: &dyn Sampler) -> &'static str { s.name() }
    pub fn nested(&self, x: f32) -> f32 { fn dbl(v: f32) -> f32 { v } let half = |v: f32| v; half(dbl(x)) }
}
"#;

    /// Every call edge out of the named function, markers included.
    fn edges_of(g: &Graph, name: &str) -> Vec<String> {
        fn rec(items: &[Item], name: &str, out: &mut Vec<String>) {
            for it in items {
                if it.name.as_deref() == Some(name) {
                    out.extend(it.calls.iter().map(edge_str));
                }
                rec(&it.children, name, out);
            }
        }
        let mut out = Vec::new();
        for m in &g.modules {
            rec(&m.items, name, &mut out);
        }
        out.sort();
        out
    }

    #[test]
    fn dyn_field_param_and_generic_all_fan_out_over_implementations() {
        let (g, dir) = graph(&[("src/lib.rs", RUST_DISPATCH)]);
        // A boxed trait-object field, a `&dyn` parameter, and a bounded
        // generic are three spellings of the same dispatch: all reach both
        // implementations, and every branch is marked `*`.
        for f in ["run", "run_with", "run_generic"] {
            let e = edges_of(&g, f);
            assert_eq!(e, vec!["Greedy::step*", "TopK::step*"], "{f}: {e:?}");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn concrete_receiver_types_resolve_without_fanning_out() {
        let (g, dir) = graph(&[("src/lib.rs", RUST_DISPATCH)]);
        // A field declared `TopK` and a `let` bound to a `TopK` literal both
        // pin the one real target — no fan-out, no heuristic marker.
        assert_eq!(edges_of(&g, "run_concrete"), vec!["TopK::trim"]);
        assert_eq!(edges_of(&g, "run_local"), vec!["TopK::trim"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn default_trait_method_is_the_target_when_nothing_overrides_it() {
        let (g, dir) = graph(&[("src/lib.rs", RUST_DISPATCH)]);
        // `step` is overridden by both impls, so the trait's own declaration
        // is not a target. `name` is not overridden, so its default body is.
        assert_eq!(edges_of(&g, "label"), vec!["Sampler::name*"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn nested_fns_and_named_closures_become_callable_children() {
        let (g, dir) = graph(&[("src/lib.rs", RUST_DISPATCH)]);
        let e = edges_of(&g, "nested");
        assert_eq!(e, vec!["nested::dbl", "nested::half"], "{e:?}");
        // …and are findable as definitions in their own right.
        assert!(def(&g, "dbl", false).contains("1 definition(s)"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn trace_walks_transitively_through_a_dispatch_fan_out() {
        let (g, dir) = graph(&[("src/lib.rs", RUST_DISPATCH)]);
        let out = trace(&g, "run", 3, false, false);
        // One hop reaches both impls; the second hop reaches what each impl
        // calls — the thing `callers` cannot do.
        assert!(out.contains("Greedy::step"), "{out}");
        assert!(out.contains("TopK::step"), "{out}");
        assert!(out.contains("pick_max"), "{out}");
        assert!(out.contains("TopK::trim"), "{out}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn trace_reverse_walks_callers_and_terminates_on_cycles() {
        let src = "pub fn a() { b() }
pub fn b() { c() }
pub fn c() { a() }
";
        let (g, dir) = graph(&[("src/lib.rs", src)]);
        let out = trace(&g, "c", 5, true, false);
        assert!(out.contains("crate::b"), "{out}");
        // A cycle must be cut, not expanded forever.
        assert!(out.contains("seen above"), "{out}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn trace_depth_bounds_the_tree() {
        let src = "pub fn a() { b() }
pub fn b() { c() }
pub fn c() {}
";
        let (g, dir) = graph(&[("src/lib.rs", src)]);
        let shallow = trace(&g, "a", 1, false, false);
        assert!(shallow.contains("crate::b"), "{shallow}");
        assert!(!shallow.contains("crate::c"), "{shallow}");
        assert!(trace(&g, "a", 2, false, false).contains("crate::c"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn path_finds_a_multi_hop_route_and_reports_hop_count() {
        let src = "pub fn a() { b() }
pub fn b() { c() }
pub fn c() { d() }
pub fn d() {}
";
        let (g, dir) = graph(&[("src/lib.rs", src)]);
        let out = path(&g, "a", "d", false);
        assert!(out.contains("3 hop(s)"), "{out}");
        for step in ["crate::a", "crate::b", "crate::c", "crate::d"] {
            assert!(out.contains(step), "{out}");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn path_crosses_modules_and_a_dispatch_boundary() {
        let (g, dir) = graph(&[("src/lib.rs", RUST_DISPATCH)]);
        let out = path(&g, "run", "pick_max", false);
        assert!(out.contains("2 hop(s)"), "{out}");
        // The dispatch hop is marked, so the route is not mistaken for proof
        // that this specific branch runs.
        assert!(out.contains("Greedy::step*"), "{out}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn path_says_so_when_no_route_exists() {
        let src = "pub fn a() {}
pub fn z() {}
";
        let (g, dir) = graph(&[("src/lib.rs", src)]);
        let out = path(&g, "a", "z", false);
        assert!(out.contains("no call path"), "{out}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn doctor_recall_excludes_provably_external_calls() {
        // One internal call, one call into std. The old denominator counted
        // both, reporting 50%; recall counts only the call that could have
        // been an internal edge.
        let src = "pub fn helper() {}
pub fn go(v: Vec<u8>) { helper(); v.len(); }
";
        let (g, dir) = graph(&[("src/lib.rs", src)]);
        let out = coverage_report(&g, &[], false, false);
        assert!(out.contains("100.0%"), "{out}");
        assert!(out.contains("external (provable)"), "{out}");
        let _ = fs::remove_dir_all(dir);
    }

    /// Two types share a method name and the receiver carries no declared
    /// type, so ctx cannot say which one runs — the canonical genuine miss.
    const AMBIGUOUS: &str = r#"
pub struct A;
pub struct B;
impl A { pub fn render(&self) {} }
impl B { pub fn render(&self) {} }
pub fn go(items: Vec<A>) { for i in items { i.render(); } }
"#;

    #[test]
    fn doctor_names_what_it_could_not_pin() {
        let (g, dir) = graph(&[("src/lib.rs", AMBIGUOUS)]);
        let out = coverage_report(&g, &[], true, false);
        assert!(out.contains("unresolved internal: 1"), "{out}");
        // The name is spelled out, so the fix is one grep away rather than a
        // percentage with nothing behind it.
        assert!(out.contains("What ctx missed"), "{out}");
        assert!(out.contains("render"), "{out}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn callers_states_whether_the_blast_radius_is_complete() {
        let src = "pub fn target() {}\npub fn caller() { target(); }\n";
        let (g, dir) = graph(&[("src/lib.rs", src)]);
        let out = callers(&g, "target", false);
        assert!(out.contains("1 caller(s)"), "{out}");
        assert!(out.contains("this blast radius is complete"), "{out}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn callers_warns_when_call_sites_with_that_name_went_unresolved() {
        let (g, dir) = graph(&[("src/lib.rs", AMBIGUOUS)]);
        let out = callers(&g, "render", false);
        // The unpinnable call site is disclosed rather than silently missing,
        // so an agent knows this one answer needs a confirming grep.
        assert!(out.contains("may be incomplete"), "{out}");
        assert!(out.contains("crate 1"), "{out}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn python_base_class_calls_fan_out_over_subclasses() {
        let src = r#"
class Base:
    def step(self): ...

class A(Base):
    def step(self): return 1

class B(Base):
    def step(self): return 2

def run(b: Base):
    return b.step()
"#;
        let (g, dir) = graph(&[("pkg/m.py", src)]);
        let e = edges_of(&g, "run");
        assert_eq!(e, vec!["A.step*", "B.step*"], "{e:?}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn typescript_interface_calls_fan_out_over_implementations() {
        let src = r#"
export interface S { step(): number; }
export class A implements S { step(): number { return 1; } }
export class B implements S { step(): number { return 2; } }
export function run(s: S): number { return s.step(); }
"#;
        let (g, dir) = graph(&[("src/m.ts", src)]);
        let e = edges_of(&g, "run");
        assert_eq!(e, vec!["A.step*", "B.step*"], "{e:?}");
        let _ = fs::remove_dir_all(dir);
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
    fn module_level_calls_are_attributed_to_the_module() {
        // Top-level code is invisible if only function bodies are scanned, which
        // silently loses callers in scripts, tests and __init__ wiring.
        let (g, dir) = graph(&[
            ("lib.py", "def frequencies(x):\n    return x\n"),
            ("top.py", "from lib import frequencies\n\nFR = frequencies(3)\n"),
        ]);
        let text = callers(&g, "frequencies", false);
        let _ = fs::remove_dir_all(&dir);
        assert!(
            text.contains("top"),
            "a module-level call must be reported: {text}"
        );
    }

    #[test]
    fn type_queries_report_signature_references() {
        // A type has no call edges; "no callers found" reads as "safe to change"
        // for the most common breaking change there is.
        let (g, dir) = graph(&[
            ("a.py", "class Config:\n    pass\n"),
            ("b.py", "from a import Config\n\ndef build(c: Config) -> Config:\n    return c\n"),
        ]);
        let text = callers(&g, "Config", false);
        let json = callers(&g, "Config", true);
        let _ = fs::remove_dir_all(&dir);
        assert!(text.contains("is a type"), "type queries need the type path: {text}");
        assert!(text.contains("build"), "the referencing signature must be listed");
        assert!(json.contains("\"references\""));
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
    fn declared_param_type_resolves_the_receiver_as_proven() {
        // `w: Widget` is written in the source, so `w.frobnicate()` is not a
        // guess — no `~`. (Before receiver typing this same call resolved only
        // because the method name happened to be unique codebase-wide.)
        let (g, dir) = graph(&[
            ("src/a.rs", "pub struct Widget;\nimpl Widget { pub fn frobnicate(&self) {} }\n"),
            ("src/b.rs", "use crate::a::Widget;\npub fn run(w: Widget) { w.frobnicate(); }\n"),
        ]);
        let out = callers(&g, "frobnicate", false);
        assert!(out.contains("b::run"), "{out}");
        assert!(!out.contains("frobnicate~"), "{out}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn opaque_receiver_edge_is_flagged_heuristic() {
        // A `for` binding carries no declared type, so the only reason this
        // resolves is that the method name is unique codebase-wide: `~`.
        let (g, dir) = graph(&[
            ("src/a.rs", "pub struct Widget;\nimpl Widget { pub fn frobnicate(&self) {} }\n"),
            (
                "src/b.rs",
                "use crate::a::Widget;\npub fn run(ws: Vec<Widget>) { for w in ws { w.frobnicate(); } }\n",
            ),
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
        let (out, removed_n, changed_n) = api_report(&base.0, &cur.0, "HEAD", false);
        assert!(out.contains("## Removed") && out.contains("api::gone"), "{out}");
        assert!(out.contains("## Changed") && out.contains("api::morph"), "{out}");
        assert!(out.contains("## Added") && out.contains("api::fresh"), "{out}");
        assert!(!out.contains("stable"), "unchanged item must not appear:\n{out}");
        // Counted separately on purpose: a removal always breaks callers, while a
        // signature change may be an added optional parameter. Only the first is
        // safe to gate CI on.
        assert_eq!(removed_n, 1, "removals drive --strict");
        assert_eq!(changed_n, 1, "signature changes are reported, not gated");
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
        let cov = coverage_report(&g, &[], false, false);
        assert!(cov.contains("## Broken links"), "{cov}");
        assert!(cov.contains("./ghost.md"), "{cov}");
        assert!(!cov.contains("./setup.md"), "resolved link must not be broken:\n{cov}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn coverage_separates_internal_external_and_blind_spots() {
        let src = "pub fn helper() {}\n\
                   pub fn run() { helper(); let v: Vec<i32> = Vec::new(); v.len(); }\n";
        let (g, dir) = graph(&[("src/a.rs", src)]);
        let out = coverage_report(&g, &[("cpp".to_string(), 2)], false, false);
        assert!(out.contains("internal edges:"), "{out}");
        // `Vec::new()` and `v.len()` are provably external, not misses.
        assert!(out.contains("external (provable): 2"), "{out}");
        assert!(out.contains("unresolved internal: 0"), "{out}");
        assert!(out.contains(".cpp"), "{out}"); // blind spot listed
        let _ = fs::remove_dir_all(dir);
    }
}
