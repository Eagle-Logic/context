//! A minimal, dependency-free MCP server over stdio: newline-delimited
//! JSON-RPC 2.0 exposing ctx's read-only commands as tools. No async, no
//! framework — a `read_line` loop and a dispatch table.

use std::io::{self, BufRead, Write};
use std::path::Path;

use anyhow::Result;
use serde_json::{json, Value};

use crate::view::View;
use crate::{extract, query, render, view};

pub fn run() -> Result<()> {
    let stdin = io::stdin();
    let mut handle = stdin.lock();
    let mut out = io::stdout();
    let mut buf = String::new();
    loop {
        buf.clear();
        if handle.read_line(&mut buf)? == 0 {
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
        // Requests carry an id and get a reply; notifications get nothing.
        if let (Some(id), Some(result)) = (id, response) {
            let msg = json!({"jsonrpc": "2.0", "id": id, "result": result});
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

fn path_prop() -> Value {
    json!({ "type": "string", "description": "Path to the repo (default \".\")" })
}

fn tools() -> Vec<Value> {
    let name_prop =
        json!({ "type": "string", "description": "Symbol name, bare or qualified (Type::method)" });
    vec![
        tool(
            "map",
            "Deterministic topology map of the codebase (modules, deps, signatures, call edges).",
            json!({ "path": path_prop(), "view": {"type": "string", "enum": ["skeleton", "interface", "full"], "description": "Detail level (default skeleton)"} }),
            &[],
        ),
        tool("modules", "One line per module with dependency edges.", json!({ "path": path_prop() }), &[]),
        tool(
            "subtree",
            "A module plus its immediate upstream dependencies and downstream dependents.",
            json!({ "path": path_prop(), "module": {"type": "string", "description": "Module name or suffix"} }),
            &["module"],
        ),
        tool(
            "def",
            "Where a symbol is defined: module, kind, line, signature, doc.",
            json!({ "path": path_prop(), "name": name_prop }),
            &["name"],
        ),
        tool(
            "callers",
            "Every function that calls the given function/method (resolved reverse call edges).",
            json!({ "path": path_prop(), "name": name_prop }),
            &["name"],
        ),
        tool(
            "context",
            "Everything needed to edit a symbol: definition, signature types, callees, callers.",
            json!({ "path": path_prop(), "name": name_prop, "max_tokens": {"type": "integer", "description": "Approx token budget (default 4000)"} }),
            &["name"],
        ),
        tool(
            "core",
            "Modules ranked by dependency centrality — the heart of the codebase.",
            json!({ "path": path_prop(), "limit": {"type": "integer", "description": "How many to show (default 30)"} }),
            &[],
        ),
        tool(
            "trace",
            "Transitive call tree from a symbol: what actually runs underneath it. Set reverse=true for the inbound tree (what reaches it).",
            json!({ "path": path_prop(), "name": name_prop, "depth": {"type": "integer", "description": "Call hops to expand (default 3)"}, "reverse": {"type": "boolean", "description": "Walk callers instead of callees"} }),
            &["name"],
        ),
        tool(
            "path",
            "Shortest call path between two symbols — how execution gets from one to the other.",
            json!({ "path": path_prop(), "from": {"type": "string", "description": "Starting function/method"}, "to": {"type": "string", "description": "Destination function/method"} }),
            &["from", "to"],
        ),
        tool(
            "doctor",
            "Coverage report: internal call-graph recall, the callee names ctx could not pin, and what it cannot model. Set explain=true for the full per-name census.",
            json!({ "path": path_prop(), "explain": {"type": "boolean", "description": "Print the full per-name census"} }),
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
    let (text, is_error) = dispatch(name, &args);
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn dispatch(name: &str, args: &Value) -> (String, bool) {
    let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
    let g = match extract::build_graph(Path::new(path)) {
        Ok(g) => g,
        Err(e) => return (format!("error building graph for {path}: {e}"), true),
    };
    let sarg = |k: &str| args.get(k).and_then(Value::as_str);
    let uarg = |k: &str, d: u64| args.get(k).and_then(Value::as_u64).unwrap_or(d) as usize;
    let barg = |k: &str| args.get(k).and_then(Value::as_bool).unwrap_or(false);

    match name {
        "map" => {
            let mut g = g;
            let v = match sarg("view") {
                Some("interface") => View::Interface,
                Some("full") => View::Full,
                _ => View::Skeleton,
            };
            view::apply(&mut g, v);
            (render::markdown(&g), false)
        }
        "modules" => (render::module_list(&g), false),
        "subtree" => match sarg("module") {
            Some(m) => (query::subtree(&g, m, false), false),
            None => ("missing required argument 'module'".into(), true),
        },
        "def" => match sarg("name") {
            Some(n) => (query::def(&g, n, false), false),
            None => ("missing required argument 'name'".into(), true),
        },
        "callers" => match sarg("name") {
            Some(n) => (query::callers(&g, n, false), false),
            None => ("missing required argument 'name'".into(), true),
        },
        "context" => match sarg("name") {
            Some(n) => (query::context(&g, n, uarg("max_tokens", 4000), false), false),
            None => ("missing required argument 'name'".into(), true),
        },
        "core" => (query::core(&g, uarg("limit", 30), None, false), false),
        "trace" => match sarg("name") {
            Some(n) => (
                query::trace(&g, n, uarg("depth", 3), barg("reverse"), false),
                false,
            ),
            None => ("missing required argument 'name'".into(), true),
        },
        "path" => match (sarg("from"), sarg("to")) {
            (Some(f), Some(t)) => (query::path(&g, f, t, false), false),
            _ => ("missing required argument 'from' or 'to'".into(), true),
        },
        "doctor" => {
            let unsupported = extract::unsupported_census(Path::new(path));
            (
                query::coverage_report(&g, &unsupported, barg("explain"), false),
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
        for expected in ["map", "def", "callers", "context", "core", "doctor", "trace", "path"] {
            assert!(names.contains(&expected), "missing tool {expected}: {names:?}");
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
}
