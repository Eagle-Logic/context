//! A minimal, dependency-free MCP server over stdio: newline-delimited
//! JSON-RPC 2.0 exposing ctx's read-only commands as tools. No async, no
//! framework — a `read_line` loop and a dispatch table.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::model::Graph;
use crate::view::View;
use crate::{extract, query, render, view};

/// Graphs already built in this process, newest first.
///
/// Every tool call rebuilds the graph from source, which is what keeps answers
/// current and is cheap on a small repo. On a large one it dominates: measured
/// on a 14,073-file tree, a five-call session spent 3,676 ms, and a `def` for a
/// name that does not exist — a no-op query — still cost 680 ms. Essentially
/// all of it was re-parsing an unchanged tree.
///
/// So the graph is kept and reused while the source it was built from is
/// unchanged, verified by re-walking for mtime and length. That check is a stat
/// per file with no reads, so a hit costs a walk instead of a parse.
///
/// Bounded, because a graph for a large repo is not small and an MCP server is
/// long-lived. Sessions overwhelmingly query one root, so a handful of entries
/// covers the real access pattern; beyond that the oldest is dropped.
const GRAPH_CACHE_ENTRIES: usize = 4;

struct CachedGraph {
    root: PathBuf,
    fingerprint: u64,
    graph: Arc<Graph>,
}

static GRAPH_CACHE: OnceLock<Mutex<Vec<CachedGraph>>> = OnceLock::new();

/// Build the graph for `root`, reusing the cached one if the tree is unchanged.
///
/// Correctness rule: a hit requires the fingerprint to match, and the
/// fingerprint covers exactly the file set `build_graph` parses. Returning a
/// stale graph would make ctx quietly wrong about current source, which is
/// worse than being slow.
fn graph_for(root: &Path) -> Result<Arc<Graph>> {
    let fingerprint = extract::source_fingerprint(root);
    let cache = GRAPH_CACHE.get_or_init(|| Mutex::new(Vec::new()));

    if let Ok(mut c) = cache.lock() {
        if let Some(i) = c
            .iter()
            .position(|e| e.root == root && e.fingerprint == fingerprint)
        {
            let hit = c.remove(i);
            let g = Arc::clone(&hit.graph);
            c.insert(0, hit);
            return Ok(g);
        }
    }

    let g = Arc::new(extract::build_graph(root)?);
    if let Ok(mut c) = cache.lock() {
        // Drop any stale entry for this root before inserting the fresh one,
        // so an edited tree does not keep a dead graph alive in the cache.
        c.retain(|e| e.root != root);
        c.insert(
            0,
            CachedGraph {
                root: root.to_path_buf(),
                fingerprint,
                graph: Arc::clone(&g),
            },
        );
        c.truncate(GRAPH_CACHE_ENTRIES);
    }
    Ok(g)
}

/// Opt-in per-call instrumentation: what each tool actually cost.
///
/// The case for measuring at all: "an agent should query rather than load a
/// map" is an argument until someone counts the tokens. This is the counter.
///
/// It never touches a tool's response — metrics go to a separate sink, so
/// enabling them cannot change what the model sees. That keeps the output a
/// pure function of the source tree, which is the property the whole tool
/// rests on. Durations are monotonic `Instant` deltas rather than wall-clock
/// readings, so nothing here introduces a dependency on the time of day.
struct Metrics {
    out: Box<dyn Write + Send>,
    seq: u64,
    total_tokens: usize,
    total_ms: u128,
}

static METRICS: OnceLock<Mutex<Metrics>> = OnceLock::new();

/// The JSON line for one tool call. Pure, so the shape is testable without
/// standing up a server or touching the process-global sink.
fn metric_line(seq: u64, tool: &str, args: &Value, text: &str, is_error: bool, ms: u128) -> Value {
    json!({
        "seq": seq,
        "tool": tool,
        "args": args,
        "output_chars": text.len(),
        "output_tokens": crate::est_tokens(text),
        // Surfaced per call so a budget that keeps biting is visible in the
        // log rather than only in the response the model already consumed.
        "truncated": text.contains("[ctx] TRUNCATED"),
        "is_error": is_error,
        "ms": ms,
    })
}

/// Record one tool call. No-op unless `--metrics` was passed.
fn record(tool: &str, args: &Value, text: &str, is_error: bool, elapsed_ms: u128) {
    let Some(m) = METRICS.get() else { return };
    let Ok(mut m) = m.lock() else { return };
    m.seq += 1;
    m.total_tokens += crate::est_tokens(text);
    m.total_ms += elapsed_ms;
    let line = metric_line(m.seq, tool, args, text, is_error, elapsed_ms);
    let _ = writeln!(m.out, "{line}");
    let _ = m.out.flush();
}

/// Totals for the session, written when the client closes the pipe.
///
/// The per-call lines answer "what did this cost"; this line answers "what did
/// the whole session cost", which is the number worth quoting.
fn record_summary() {
    let Some(m) = METRICS.get() else { return };
    let Ok(mut m) = m.lock() else { return };
    let line = json!({
        "summary": true,
        "calls": m.seq,
        "total_output_tokens": m.total_tokens,
        "total_ms": m.total_ms,
    });
    let _ = writeln!(m.out, "{line}");
    let _ = m.out.flush();
}

/// The directory this server may read.
///
/// An MCP tool is invoked by a model, not a person. A model has no legitimate
/// reason to leave the project it was pointed at, and it cannot see the cost of
/// wandering until the tokens are already spent — so containment is
/// default-deny with an explicit opt-out, not a documented caveat.
///
/// Set once at startup and never from a request, so no argument can widen it.
static ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Resolve a caller-supplied `path` against [`ROOT`], refusing anything that
/// escapes it.
///
/// Both sides are canonicalized first, so `..` traversal and symlinks out of
/// the tree are caught rather than merely discouraged.
fn resolve_in_root(path: &str) -> std::result::Result<PathBuf, String> {
    // Falls back to the working directory, which is what `run` would have set
    // anyway — so a direct `dispatch` call (tests, embedding) is contained too
    // rather than silently unrestricted.
    let root = ROOT.get_or_init(|| {
        std::env::current_dir()
            .and_then(|d| d.canonicalize())
            .unwrap_or_default()
    });
    let joined = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        root.join(path)
    };
    let real = joined
        .canonicalize()
        .map_err(|e| format!("cannot resolve path {path:?}: {e}"))?;
    if !real.starts_with(root) {
        return Err(format!(
            "refused: {path:?} resolves to {} which is outside this server's root ({}). \
             The MCP server only reads the project it was started in. Start it with \
             `ctx mcp --root <dir>` to point it somewhere else.",
            real.display(),
            root.display()
        ));
    }
    Ok(real)
}

pub fn run(root: Option<PathBuf>, metrics: Option<PathBuf>) -> Result<()> {
    if let Some(p) = metrics {
        // `-` means stderr: stdout carries the JSON-RPC stream, so it is the
        // one sink that must stay clean.
        let out: Box<dyn Write + Send> = if p.as_os_str() == "-" {
            Box::new(io::stderr())
        } else {
            Box::new(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&p)
                    .with_context(|| format!("cannot open metrics file {}", p.display()))?,
            )
        };
        METRICS
            .set(Mutex::new(Metrics {
                out,
                seq: 0,
                total_tokens: 0,
                total_ms: 0,
            }))
            .ok();
    }
    let root = root.unwrap_or(std::env::current_dir()?);
    let root = root
        .canonicalize()
        .with_context(|| format!("cannot resolve MCP root {}", root.display()))?;
    ROOT.set(root).ok();
    let stdin = io::stdin();
    let mut handle = stdin.lock();
    let mut out = io::stdout();
    let mut buf = String::new();
    loop {
        buf.clear();
        if handle.read_line(&mut buf)? == 0 {
            record_summary();
            break; // EOF: client closed the pipe
        }
        let line = buf.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let response = handle_method(method, &req);
        // A request carries an id and MUST get a reply; a notification gets
        // nothing. An unhandled method still needs an error object — staying
        // silent leaves clients that probe `resources/list` or `prompts/list`
        // blocking on an id that never arrives.
        if let Some(id) = id {
            let msg = match response {
                Some(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                None => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": format!("Method not found: {method}")},
                }),
            };
            writeln!(out, "{}", serde_json::to_string(&msg)?)?;
            out.flush()?;
        }
    }
    Ok(())
}

fn handle_method(method: &str, req: &Value) -> Option<Value> {
    match method {
        "initialize" => Some(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "ctx", "version": env!("CARGO_PKG_VERSION") },
        })),
        "tools/list" => Some(json!({ "tools": tools() })),
        "tools/call" => Some(tools_call(req)),
        "ping" => Some(json!({})),
        _ => None,
    }
}

fn tool(name: &str, desc: &str, props: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": desc,
        "inputSchema": { "type": "object", "properties": props, "required": required },
    })
}

/// Budget applied when an MCP caller does not set `max_tokens`.
///
/// Generous enough to be useful, small enough that a zero-argument call cannot
/// dump 130k tokens into an agent's context.
const DEFAULT_MCP_BUDGET: usize = 25_000;

/// Hold `text` to `budget` tokens, cutting on a line boundary and saying so.
///
/// `map` and `subtree` degrade gracefully — they drop detail, then whole
/// modules — so they never truncate. The flat reports have no such ladder, so
/// the honest option is to cut and be loud about it: a silent cut would be a
/// report that lies by omission, which is the one thing this tool must not do.
fn clamp(text: String, budget: Option<usize>, what: &str) -> String {
    let Some(budget) = budget else { return text };
    if crate::est_tokens(&text) <= budget {
        return text;
    }
    let cap = budget * crate::BYTES_PER_TOKEN;
    let cut = text[..cap.min(text.len())]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let kept = &text[..cut];
    let dropped_lines = text[cut..].lines().count();
    format!(
        "{kept}\n[ctx] TRUNCATED at {budget} tokens: {dropped_lines} more line(s) withheld \
         (~{} tokens total). This is a cut, not the whole answer. Raise max_tokens, or \
         narrow the query — `{what}` on a subdirectory, or a more specific name.\n",
        crate::est_tokens(&text)
    )
}

/// Repeated in every tool's schema, so each word here is paid ten times over
/// in standing context. Keep it to what disambiguates the argument.
fn path_prop() -> Value {
    json!({ "type": "string", "description": "Repo path (default \".\")" })
}

fn budget_prop(desc: &str) -> Value {
    json!({ "type": "integer", "description": desc })
}

/// One budget description, shared. The truncation behaviour is announced in
/// the output itself when it happens, so paying to describe it up front — in
/// every turn, whether or not a budget is ever hit — is the wrong trade.
const BUDGET_DESC: &str = "Max output tokens (default 25000)";

fn tools() -> Vec<Value> {
    let name_prop = json!({ "type": "string", "description": "Symbol name, bare or qualified" });
    vec![
        tool(
            "map",
            "Whole-repo structural map. Expensive and rarely the right first move — \
             prefer context/callers/subtree for a specific question.",
            json!({
                "path": path_prop(),
                "view": {"type": "string", "enum": ["skeleton", "interface", "full"], "description": "Default skeleton"},
                "max_tokens": budget_prop(BUDGET_DESC),
            }),
            &[],
        ),
        tool(
            "modules",
            "One line per module with dependency edges.",
            json!({
                "path": path_prop(),
                "max_tokens": budget_prop(BUDGET_DESC),
            }),
            &[],
        ),
        tool(
            "subtree",
            "One module plus its immediate dependencies and dependents.",
            json!({
                "path": path_prop(),
                "module": {"type": "string", "description": "Module name or suffix"},
                "view": {"type": "string", "enum": ["skeleton", "interface", "full"], "description": "Default full"},
                "max_tokens": budget_prop(BUDGET_DESC),
            }),
            &["module"],
        ),
        tool(
            "def",
            "Where a symbol is defined: file, span, signature.",
            json!({ "path": path_prop(), "name": name_prop }),
            &["name"],
        ),
        tool(
            "callers",
            "Every function that calls this — the blast radius before a signature change.",
            json!({
                "path": path_prop(),
                "name": name_prop,
                "max_tokens": budget_prop(BUDGET_DESC),
            }),
            &["name"],
        ),
        tool(
            "context",
            "Definition, signature types, callees and callers for one symbol. \
             Usually enough to edit without opening the file.",
            json!({ "path": path_prop(), "name": name_prop, "max_tokens": {"type": "integer", "description": "Max output tokens (default 4000)"},
                    "include_source": {"type": "boolean", "description": "Inline the definition and its signature types' source"} }),
            &["name"],
        ),
        tool(
            "core",
            "Modules ranked by dependency centrality.",
            json!({ "path": path_prop(), "limit": {"type": "integer", "description": "Default 30"} }),
            &[],
        ),
        tool(
            "trace",
            "Transitive call tree from a symbol; reverse=true for what reaches it.",
            json!({ "path": path_prop(), "name": name_prop, "depth": {"type": "integer", "description": "Default 3"}, "reverse": {"type": "boolean", "description": "Inbound tree"} }),
            &["name"],
        ),
        tool(
            "path",
            "Shortest call path between two symbols.",
            json!({ "path": path_prop(), "from": {"type": "string", "description": "From symbol"}, "to": {"type": "string", "description": "To symbol"} }),
            &["from", "to"],
        ),
        tool(
            "doctor",
            "Coverage report: what resolved, and the callee names it could not pin.",
            json!({
                "path": path_prop(),
                "explain": {"type": "boolean", "description": "Full per-name census"},
                "max_tokens": budget_prop(BUDGET_DESC),
            }),
            &[],
        ),
    ]
}

fn tools_call(req: &Value) -> Value {
    let params = req.get("params");
    let name = params
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let args = params
        .and_then(|p| p.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let started = Instant::now();
    let (text, is_error) = dispatch(name, &args);
    record(name, &args, &text, is_error, started.elapsed().as_millis());
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn dispatch(name: &str, args: &Value) -> (String, bool) {
    let requested = args.get("path").and_then(Value::as_str).unwrap_or(".");
    let root_path = match resolve_in_root(requested) {
        Ok(p) => p,
        Err(msg) => return (msg, true),
    };
    let path = root_path.as_path();
    let g = match graph_for(path) {
        Ok(g) => g,
        Err(e) => return (format!("error building graph for {requested}: {e}"), true),
    };
    let g = &*g;
    let sarg = |k: &str| args.get(k).and_then(Value::as_str);
    let uarg = |k: &str, d: u64| args.get(k).and_then(Value::as_u64).unwrap_or(d) as usize;
    let barg = |k: &str| args.get(k).and_then(Value::as_bool).unwrap_or(false);

    let view_arg = |d: View| match sarg("view") {
        Some("skeleton") => View::Skeleton,
        Some("interface") => View::Interface,
        Some("full") => View::Full,
        _ => d,
    };
    // An MCP result lands straight in the caller's context with no shell to pipe
    // it through, and a tool an LLM can invoke with zero arguments must be safe
    // by default — so absent means DEFAULT_MCP_BUDGET, not unbudgeted.
    let raw_budget = args.get("max_tokens");
    let budget = match raw_budget {
        None | Some(Value::Null) => Some(DEFAULT_MCP_BUDGET),
        Some(v) => match v.as_u64() {
            Some(n) if n > 0 => Some(n as usize),
            // A malformed budget silently meaning "unbudgeted" is the worst
            // possible reading of a cap.
            _ => return ("max_tokens must be a positive integer".into(), true),
        },
    };

    match name {
        "map" => {
            // Defaults to skeleton, not the CLI's `full`: an unbudgeted full map
            // is ~258k tokens on a large repo, and an MCP result lands straight
            // in the caller's context with no chance to pipe it anywhere.
            let v = view_arg(View::Skeleton);
            match budget {
                Some(b) => {
                    let r = crate::render_budgeted(g, v, crate::Format::Md, b);
                    // The CLI reports the view chosen and whether it fit on
                    // stderr; over MCP there is no stderr, so it goes in band or
                    // it is lost.
                    let mut text = r.text;
                    if !r.fit {
                        text.push_str(&format!(
                            "\n[ctx] BUDGET NOT MET: emitted view '{}' still exceeds {b} tokens.\n",
                            r.view.name()
                        ));
                    } else if r.view != v || r.omitted > 0 {
                        text.push_str(&format!(
                            "\n[ctx] fitted to {b} tokens: '{}' view, {} module(s) omitted.\n",
                            r.view.name(),
                            r.omitted
                        ));
                    }
                    (text, false)
                }
                None => {
                    // `view::apply` mutates, and the graph is shared with the
                    // cache, so this branch works on a copy. Only reachable
                    // when a caller explicitly opts out of the budget.
                    let mut g = g.clone();
                    view::apply(&mut g, v);
                    (render::markdown(&g), false)
                }
            }
        }
        "modules" => (clamp(render::module_list(g), budget, "ctx modules"), false),
        "subtree" => match sarg("module") {
            // Same code path as the CLI, so the two cannot drift again.
            Some(m) => {
                match crate::subtree_text(g, m, view_arg(View::Full), crate::Format::Md, budget) {
                    Ok(t) => (t, false),
                    Err(e) => (format!("{e}"), true),
                }
            }
            None => ("missing required argument 'module'".into(), true),
        },
        "def" => match sarg("name") {
            Some(n) => (query::def(g, n, false), false),
            None => ("missing required argument 'name'".into(), true),
        },
        "callers" => match sarg("name") {
            Some(n) => (
                clamp(query::callers(g, n, false), budget, "ctx callers"),
                false,
            ),
            None => ("missing required argument 'name'".into(), true),
        },
        "context" => match sarg("name") {
            // The only budget-advertising tool that never clamped: its internal
            // budget caps two lists, not the whole bundle, and `include_source`
            // puts real bulk outside those lists. A ceiling that leaks is not one.
            Some(n) => {
                let cap = uarg("max_tokens", 4000);
                (
                    clamp(
                        query::context(g, n, cap, barg("include_source"), false),
                        Some(cap),
                        "ctx context",
                    ),
                    false,
                )
            }
            None => ("missing required argument 'name'".into(), true),
        },
        "core" => (query::core(g, uarg("limit", 30), None, false), false),
        "trace" => match sarg("name") {
            Some(n) => (
                query::trace(g, n, uarg("depth", 3), barg("reverse"), false),
                false,
            ),
            None => ("missing required argument 'name'".into(), true),
        },
        "path" => match (sarg("from"), sarg("to")) {
            (Some(f), Some(t)) => (query::path(g, f, t, false), false),
            _ => ("missing required argument 'from' or 'to'".into(), true),
        },
        "doctor" => {
            let unsupported = extract::unsupported_census(path);
            (
                clamp(
                    query::coverage_report(g, &unsupported, barg("explain"), false),
                    budget,
                    "ctx doctor",
                ),
                false,
            )
        }
        other => (format!("unknown tool: {other}"), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_reports_server_info() {
        let r = handle_method("initialize", &json!({"id": 1, "method": "initialize"})).unwrap();
        assert_eq!(r["serverInfo"]["name"], "ctx");
        assert!(r["capabilities"]["tools"].is_object());
    }

    #[test]
    fn tools_list_includes_the_core_commands() {
        let r = handle_method("tools/list", &json!({"id": 1})).unwrap();
        let names: Vec<&str> = r["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        for expected in [
            "map", "def", "callers", "context", "core", "doctor", "trace", "path",
        ] {
            assert!(
                names.contains(&expected),
                "missing tool {expected}: {names:?}"
            );
        }
    }

    #[test]
    fn dispatch_missing_required_arg_is_error() {
        let (_text, is_error) = dispatch("def", &json!({"path": "."}));
        assert!(is_error);
    }

    #[test]
    fn notification_without_id_gets_no_response() {
        // A method we handle, but no id → run() would not reply. handle_method
        // still returns a value; the id-gating lives in run(). Here we just
        // confirm an unknown method yields None.
        assert!(handle_method("notifications/initialized", &json!({})).is_none());
    }

    #[test]
    fn path_outside_the_root_is_refused_with_a_reason() {
        // The system temp dir: an absolute path that exists on every platform
        // and is never inside the crate the test process runs in. Hardcoding
        // /tmp here failed on Windows for the wrong reason — it does not exist,
        // so it was rejected as unresolvable rather than as out-of-root.
        let outside = std::env::temp_dir();
        let (text, is_error) = dispatch("modules", &json!({"path": outside.to_string_lossy()}));
        assert!(is_error, "escaping the root must be an error, got: {text}");
        // The refusal has to say WHY and how to override it, or the caller —
        // a model — just retries the same thing.
        assert!(text.contains("refused"), "{text}");
        assert!(text.contains("outside this server's root"), "{text}");
        assert!(text.contains("--root"), "must name the override: {text}");
    }

    #[test]
    fn dot_dot_traversal_cannot_escape_the_root() {
        let (text, is_error) = dispatch("modules", &json!({"path": "../.."}));
        assert!(is_error, "`../..` must not escape the root, got: {text}");
        assert!(text.contains("refused"), "{text}");
    }

    #[test]
    fn a_path_inside_the_root_still_works() {
        let (text, is_error) = dispatch("modules", &json!({"path": "src"}));
        assert!(!is_error, "an in-root path must be allowed: {text}");
        assert!(text.contains("modules:"), "{text}");
    }

    #[test]
    fn uncapped_tools_now_respect_a_budget_and_say_when_they_cut() {
        // A budget this small must bite on any real repo.
        let (text, is_error) = dispatch("modules", &json!({"path": ".", "max_tokens": 20}));
        assert!(!is_error, "{text}");
        assert!(
            text.contains("[ctx] TRUNCATED"),
            "must announce the cut: {text}"
        );
        assert!(
            text.contains("This is a cut, not the whole answer"),
            "a truncated report must not read as complete: {text}"
        );
        assert!(
            crate::est_tokens(&text) < 400,
            "cut should be near the budget: {text}"
        );
    }

    #[test]
    fn callers_and_doctor_are_budgeted_too() {
        for tool in ["callers", "doctor"] {
            let mut args = json!({"path": ".", "max_tokens": 20});
            if tool == "callers" {
                args["name"] = json!("new");
            }
            let (text, is_error) = dispatch(tool, &args);
            assert!(!is_error, "{tool}: {text}");
            assert!(
                text.contains("[ctx] TRUNCATED"),
                "{tool} must be budgeted: {text}"
            );
        }
    }

    #[test]
    fn context_is_clamped_like_every_other_budgeted_tool() {
        // It advertised `max_tokens` and never enforced a ceiling: its internal
        // budget caps two of its lists, not the bundle, and `include_source`
        // puts real bulk outside those lists.
        let args =
            json!({"path": ".", "name": "build_graph", "max_tokens": 20, "include_source": true});
        let (text, is_error) = dispatch("context", &args);
        assert!(!is_error, "{text}");
        assert!(text.contains("[ctx] TRUNCATED"), "{text}");
        assert!(
            crate::est_tokens(&text) < 400,
            "cut, not merely noted: {text}"
        );
    }

    #[test]
    fn every_tool_that_can_grow_advertises_max_tokens() {
        // Regression: `modules` shipped uncapped and returned ~2.3M tokens when
        // pointed at a filesystem root. Anything whose output scales with repo
        // size must expose the knob.
        for t in tools() {
            let name = t["name"].as_str().unwrap();
            if !matches!(
                name,
                "modules" | "callers" | "doctor" | "map" | "subtree" | "context"
            ) {
                continue;
            }
            assert!(
                t["inputSchema"]["properties"].get("max_tokens").is_some(),
                "{name} must advertise max_tokens"
            );
        }
    }

    #[test]
    fn metrics_are_off_unless_asked_for() {
        // The default path must not panic, allocate a sink, or write anywhere.
        record("map", &json!({}), "some output", false, 7);
        record_summary();
        assert!(
            METRICS.get().is_none(),
            "metrics must stay unset until --metrics is passed"
        );
    }

    #[test]
    fn a_metric_line_reports_what_the_call_cost() {
        let l = metric_line(
            3,
            "callers",
            &json!({"name": "new"}),
            "abcdefghijkl",
            false,
            42,
        );
        assert_eq!(l["seq"], 3);
        assert_eq!(l["tool"], "callers");
        assert_eq!(l["args"]["name"], "new");
        assert_eq!(l["output_chars"], 12);
        assert_eq!(l["output_tokens"], crate::est_tokens("abcdefghijkl"));
        assert_eq!(l["ms"], 42);
        assert_eq!(l["truncated"], false);
        assert_eq!(l["is_error"], false);
    }

    #[test]
    fn a_truncated_response_is_flagged_in_the_log() {
        // Otherwise a budget that keeps biting is invisible to whoever is
        // reading the log rather than the responses.
        let cut = "some rows\n[ctx] TRUNCATED at 25000 tokens: 900 more line(s) withheld";
        let l = metric_line(1, "modules", &json!({}), cut, false, 5);
        assert_eq!(l["truncated"], true);
    }

    #[test]
    fn an_unchanged_tree_is_served_from_cache() {
        let dir = std::env::temp_dir().join(format!("ctx_cache_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn a() {}\n").unwrap();
        let dir = dir.canonicalize().unwrap();

        let first = graph_for(&dir).unwrap();
        let second = graph_for(&dir).unwrap();
        // Same allocation, not merely an equal graph: proves no rebuild.
        assert!(
            Arc::ptr_eq(&first, &second),
            "an unchanged tree must be served from cache, not rebuilt"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_edited_tree_is_rebuilt_not_served_stale() {
        let dir = std::env::temp_dir().join(format!("ctx_cache_edit_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn a() {}\n").unwrap();
        let dir = dir.canonicalize().unwrap();

        let first = graph_for(&dir).unwrap();
        let before = first.modules[0].items.len();

        std::fs::write(dir.join("src/a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        let second = graph_for(&dir).unwrap();

        assert!(
            !Arc::ptr_eq(&first, &second),
            "an edited tree must not be served from cache"
        );
        assert!(
            second.modules[0].items.len() > before,
            "the rebuilt graph must reflect the edit"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cache_is_bounded() {
        // A long-lived server must not accumulate graphs for every path it is
        // ever asked about.
        let base = std::env::temp_dir().join(format!("ctx_cache_bound_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mut dirs = Vec::new();
        for i in 0..(GRAPH_CACHE_ENTRIES + 3) {
            let d = base.join(format!("r{i}/src"));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("a.rs"), format!("fn f{i}() {{}}\n")).unwrap();
            dirs.push(base.join(format!("r{i}")).canonicalize().unwrap());
        }
        for d in &dirs {
            graph_for(d).unwrap();
        }
        let len = GRAPH_CACHE.get().unwrap().lock().unwrap().len();
        assert!(
            len <= GRAPH_CACHE_ENTRIES,
            "cache grew to {len}, cap is {GRAPH_CACHE_ENTRIES}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
