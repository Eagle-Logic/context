mod extract;
mod git;
mod mcp;
mod model;
mod query;
mod render;
mod view;

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand, ValueEnum};

use model::{Graph, Module};
use view::View;

#[derive(Parser)]
#[command(
    name = "ctx",
    version,
    about = "Deterministic AST skeleton maps of a codebase for agent context injection"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Md,
    Json,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate the full structural map (Strategy A: map-first boot sequence)
    Map {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_enum, default_value_t = Format::Md)]
        format: Format,
        /// Detail level: skeleton (architecture only), interface (public API),
        /// full (private items + call edges)
        #[arg(long, value_enum, default_value_t = View::Full)]
        view: View,
        /// Fit the output to a token budget: emit the richest view at or below
        /// `--view` whose ~token count fits, reporting the choice on stderr.
        /// Never truncates. Omit for the exact `--view`.
        #[arg(long)]
        max_tokens: Option<usize>,
        /// Write to a file instead of stdout (e.g. CODEBASE_MAP.md)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// One module plus its immediate upstream/downstream neighbors (Strategy B: subtree pruning)
    Subtree {
        /// Module name or suffix, e.g. "core::inference" or "inference"
        module: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_enum, default_value_t = Format::Md)]
        format: Format,
        /// Detail level: skeleton, interface, or full
        #[arg(long, value_enum, default_value_t = View::Full)]
        view: View,
    },
    /// Top-level module list with dependency edges (the global high-level map)
    Modules {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Locate where a symbol is defined (module, kind, line, signature)
    Def {
        /// Symbol name or qualified name, e.g. "SteerConfig" or "Type::method"
        name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_enum, default_value_t = Format::Md)]
        format: Format,
    },
    /// Reverse call edges: every function that calls the given function/method
    Callers {
        /// Callee name or qualified name, e.g. "basename" or "Type::method"
        name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_enum, default_value_t = Format::Md)]
        format: Format,
    },
    /// Everything needed to edit a symbol: definition, signature types, callees, callers
    Context {
        /// Symbol name or qualified name, e.g. "SteerConfig" or "Type::method"
        name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Approximate token budget for the assembled context
        #[arg(long, default_value_t = 4000)]
        max_tokens: usize,
        #[arg(long, value_enum, default_value_t = Format::Md)]
        format: Format,
    },
    /// Rank modules by dependency centrality — the heart of the codebase
    Core {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// How many modules to show
        #[arg(long, default_value_t = 30)]
        limit: usize,
        /// Weight centrality by git churn: rank hotspots (central AND volatile)
        #[arg(long)]
        churn: bool,
        #[arg(long, value_enum, default_value_t = Format::Md)]
        format: Format,
    },
    /// Coverage report: how much of the call graph resolved, and blind spots
    Doctor {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_enum, default_value_t = Format::Md)]
        format: Format,
    },
    /// Impact map of a diff: changed modules + upstream deps + downstream callers
    Changed {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Diff against this git ref instead of the working tree vs HEAD
        #[arg(long)]
        since: Option<String>,
        /// Report public-API changes (removed/changed signatures + who breaks)
        #[arg(long)]
        api: bool,
        #[arg(long, value_enum, default_value_t = Format::Md)]
        format: Format,
        #[arg(long, value_enum, default_value_t = View::Full)]
        view: View,
    },
    /// Structural diff between two git refs: `A..B` (B defaults to the working
    /// tree). Changed modules + who they break; `--api` for breaking changes.
    Diff {
        /// Ref range `A..B`, or a single ref `A` (diffs A against the working tree)
        range: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Report public-API changes between the refs (removed/changed + who breaks)
        #[arg(long)]
        api: bool,
        #[arg(long, value_enum, default_value_t = Format::Md)]
        format: Format,
        #[arg(long, value_enum, default_value_t = View::Full)]
        view: View,
    },
    /// Run as an MCP server over stdio (exposes the read-only commands as tools)
    Mcp,
    /// Print the recommended CLAUDE.md discovery-protocol block
    Snippet,
}

const SNIPPET: &str = r#"## Codebase Discovery Tools

`ctx` is a deterministic static-analysis CLI for this repo (Rust, Python, TypeScript, Markdown). Use it to
orient yourself instead of grepping raw source; only read raw files when you need
implementation bodies.

- `ctx map --view skeleton` — bird's-eye architecture: modules, deps, type names (cheapest)
- `ctx map --view interface` — + public signatures, struct fields, enum variants (API surface)
- `ctx map` — + private items and per-function call edges (`→ callee`) for tracing execution
  (a trailing `~` on an edge means it was inferred from an opaque receiver, not an import/path —
  trust it less)
- `ctx modules` — one line per module with dependency edges
- `ctx subtree <module> [--view ...]` — one module plus its immediate upstream dependencies
  and downstream dependents
- `ctx def <name>` — where a symbol is defined: module, kind, line, and signature (jump-to-def
  without knowing the module; accepts bare `Foo` or qualified `Type::method`)
- `ctx callers <name>` — every function that calls the given function/method (resolved reverse
  call edges — the blast radius before you change a signature; more precise than grep)
- `ctx context <name>` — everything needed to edit a symbol in one shot: its definition, the
  types in its signature, what it calls, and what calls it (token-budgeted via `--max-tokens`)
- `ctx changed [--since <ref>]` — impact map of your diff: the changed modules plus their
  dependencies and the callers they may break (defaults to the working tree vs HEAD)
- `ctx changed --api [--since <ref>]` — public API changes in your diff: removed or
  signature-changed public items and who breaks (a pre-merge breaking-change check)
- `ctx diff <A>..<B>` — structural diff between two git refs (B defaults to the working
  tree): changed modules + who they break; add `--api` for breaking changes across the range
- `ctx core` — the modules that matter most, ranked by dependency centrality (where to look
  first in an unfamiliar codebase; add `--churn` to weight by how often they change)
- `ctx doctor` — coverage report: what fraction of the call graph resolved and which modules/
  edges to distrust (run once to calibrate how much to lean on ctx for this repo)
- add `--format json` to any of the above for machine-readable output

Protocol: before modifying or analyzing code, run `ctx map --view skeleton` to load the
topology. When you're about to work on a specific symbol, run `ctx context <name>` — it bundles
the definition, signature types, callees, and callers in one call, so you rarely need to open the
file until you're editing its body. For broader orientation use `ctx subtree <module>`; before
changing a signature run `ctx callers <name>` to see the blast radius. Line anchors (`[L42]`)
give exact positions for surgical reads.
"#;

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Map {
            path,
            format,
            view,
            max_tokens,
            output,
        } => {
            let mut g = extract::build_graph(&path)?;
            let module_count = g.modules.len();
            let rendered = match max_tokens {
                Some(budget) => {
                    let (used, text, fit) = render_budgeted(&g, view, format, budget);
                    eprintln!(
                        "budget {budget} tok → {} view (~{} tok){}",
                        used.name(),
                        text.len() / 4,
                        if fit { "" } else { " — still over; coarsest view emitted" }
                    );
                    text
                }
                None => {
                    view::apply(&mut g, view);
                    match format {
                        Format::Md => render::markdown(&g),
                        Format::Json => serde_json::to_string_pretty(&g)?,
                    }
                }
            };
            match output {
                Some(file) => {
                    fs::write(&file, &rendered)?;
                    eprintln!(
                        "wrote {} ({} modules, {} bytes)",
                        file.display(),
                        module_count,
                        rendered.len()
                    );
                }
                None => print!("{rendered}"),
            }
        }
        Cmd::Subtree {
            module,
            path,
            format,
            view,
        } => {
            let mut g = extract::build_graph(&path)?;
            view::apply(&mut g, view);
            let targets: Vec<&Module> = g
                .modules
                .iter()
                .filter(|m| {
                    m.name == module
                        || m.name.ends_with(&format!("::{module}"))
                        || m.name.ends_with(&format!(".{module}"))
                })
                .collect();
            if targets.is_empty() {
                let names: Vec<&str> = g.modules.iter().map(|m| m.name.as_str()).collect();
                bail!(
                    "no module matching '{}'. Available modules:\n  {}",
                    module,
                    names.join("\n  ")
                );
            }

            let target_names: BTreeSet<&str> =
                targets.iter().map(|m| m.name.as_str()).collect();
            let (upstream, downstream) = query::neighbors(&g, &target_names);

            match format {
                Format::Md => print!(
                    "{}",
                    render::subtree_md(&module, &targets, &upstream, &downstream)
                ),
                Format::Json => {
                    let json = serde_json::json!({
                        "query": module,
                        "target": targets,
                        "upstream": upstream,
                        "downstream": downstream,
                    });
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
            }
        }
        Cmd::Modules { path } => {
            let g = extract::build_graph(&path)?;
            print!("{}", render::module_list(&g));
        }
        Cmd::Def { name, path, format } => {
            let g = extract::build_graph(&path)?;
            print!("{}", query::def(&g, &name, matches!(format, Format::Json)));
        }
        Cmd::Callers { name, path, format } => {
            let g = extract::build_graph(&path)?;
            print!(
                "{}",
                query::callers(&g, &name, matches!(format, Format::Json))
            );
        }
        Cmd::Context {
            name,
            path,
            max_tokens,
            format,
        } => {
            let g = extract::build_graph(&path)?;
            print!(
                "{}",
                query::context(&g, &name, max_tokens, matches!(format, Format::Json))
            );
        }
        Cmd::Core {
            path,
            limit,
            churn,
            format,
        } => {
            let g = extract::build_graph(&path)?;
            // Map git's repo-relative churn counts onto module names.
            let churn_map = if churn {
                let raw = git::churn(&path)?;
                let repo = git::repo_root(&path)?;
                let scan = path.canonicalize()?;
                let prefix = scan.strip_prefix(&repo).unwrap_or(std::path::Path::new(""));
                let mut mm = std::collections::HashMap::new();
                for m in &g.modules {
                    let rel = prefix.join(&m.file);
                    if let Some(&c) = raw.get(rel.to_string_lossy().as_ref()) {
                        mm.insert(m.name.clone(), c);
                    }
                }
                Some(mm)
            } else {
                None
            };
            print!(
                "{}",
                query::core(&g, limit, churn_map.as_ref(), matches!(format, Format::Json))
            );
        }
        Cmd::Doctor { path, format } => {
            let g = extract::build_graph(&path)?;
            let unsupported = extract::unsupported_census(&path);
            print!(
                "{}",
                query::coverage_report(&g, &unsupported, matches!(format, Format::Json))
            );
        }
        Cmd::Changed {
            path,
            since,
            api,
            format,
            view: _,
        } if api => {
            // Compare the public API surface now vs a base ref, building the
            // base tree in a throwaway detached worktree.
            let current = extract::build_graph(&path)?;
            let label = since.as_deref().unwrap_or("HEAD");
            let repo = git::repo_root(&path)?;
            let rel = path
                .canonicalize()?
                .strip_prefix(&repo)
                .map(|p| p.to_path_buf())
                .unwrap_or_default();
            let wt = std::env::temp_dir().join(format!("ctx-api-{}", std::process::id()));
            git::add_worktree(&repo, &wt, label)?;
            let base = extract::build_graph(&wt.join(&rel));
            git::remove_worktree(&repo, &wt);
            let base = base?;
            print!(
                "{}",
                query::api_report(&base, &current, label, matches!(format, Format::Json))
            );
        }
        Cmd::Changed {
            path,
            since,
            api: _,
            format,
            view,
        } => {
            let mut g = extract::build_graph(&path)?;
            view::apply(&mut g, view);
            let changed_paths = git::changed_files(&path, since.as_deref())?;
            let changed_set: std::collections::HashSet<PathBuf> = changed_paths
                .iter()
                .filter_map(|p| p.canonicalize().ok())
                .collect();
            let root = PathBuf::from(&g.root);
            let targets: Vec<&Module> = g
                .modules
                .iter()
                .filter(|m| {
                    root.join(&m.file)
                        .canonicalize()
                        .map(|p| changed_set.contains(&p))
                        .unwrap_or(false)
                })
                .collect();
            let label = since.as_deref().unwrap_or("HEAD");
            if targets.is_empty() {
                println!(
                    "no changed Rust/Python modules vs {} ({} changed path(s) total)",
                    label,
                    changed_paths.len()
                );
                return Ok(());
            }
            let target_names: BTreeSet<&str> = targets.iter().map(|m| m.name.as_str()).collect();
            let (upstream, downstream) = query::neighbors(&g, &target_names);
            match format {
                Format::Md => print!(
                    "{}",
                    render::changed_md(label, &targets, &upstream, &downstream)
                ),
                Format::Json => {
                    let json = serde_json::json!({
                        "since": label,
                        "changed": targets,
                        "upstream": upstream,
                        "downstream": downstream,
                    });
                    println!("{}", serde_json::to_string_pretty(&json)?);
                }
            }
        }
        Cmd::Diff {
            range,
            path,
            api,
            format,
            view,
        } => run_diff(&range, &path, api, format, view)?,
        Cmd::Mcp => mcp::run()?,
        Cmd::Snippet => print!("{SNIPPET}"),
    }
    Ok(())
}

/// `ctx diff A..B` — map the file changes between two refs onto B's module
/// graph (B defaults to the working tree). With `--api`, diff the public API
/// surface between the two refs instead.
fn run_diff(range: &str, path: &std::path::Path, api: bool, format: Format, view: View) -> Result<()> {
    let (a, b) = parse_range(range);
    let repo = git::repo_root(path)?;
    let rel = path
        .canonicalize()?
        .strip_prefix(&repo)
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    let json = matches!(format, Format::Json);
    let label = match &b {
        Some(b) => format!("{a}..{b}"),
        None => format!("{a}..<worktree>"),
    };

    if api {
        // Public-API diff needs both trees fully built.
        let (base, wt_a) = graph_at(&repo, &rel, Some(&a), path, "diff-a")?;
        let (head, wt_b) = match graph_at(&repo, &rel, b.as_deref(), path, "diff-b") {
            Ok(v) => v,
            Err(e) => {
                cleanup(&repo, wt_a);
                return Err(e);
            }
        };
        let out = query::api_report(&base, &head, &a, json);
        cleanup(&repo, wt_a);
        cleanup(&repo, wt_b);
        print!("{out}");
        return Ok(());
    }

    // Structural diff: build B's graph, map the changed files onto it.
    let (mut head, wt_b) = graph_at(&repo, &rel, b.as_deref(), path, "diff-b")?;
    view::apply(&mut head, view);
    let diffs = match git::diff_files(path, &a, b.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            cleanup(&repo, wt_b);
            return Err(e);
        }
    };
    // Diff paths are repo-relative; resolve them against the tree they belong
    // to (B's worktree, or the main repo root for the working tree).
    let diff_base = wt_b.clone().unwrap_or_else(|| repo.clone());
    let changed_set: std::collections::HashSet<PathBuf> = diffs
        .iter()
        .filter_map(|d| diff_base.join(d).canonicalize().ok())
        .collect();
    let root = PathBuf::from(&head.root);
    let targets: Vec<&Module> = head
        .modules
        .iter()
        .filter(|m| {
            root.join(&m.file)
                .canonicalize()
                .map(|p| changed_set.contains(&p))
                .unwrap_or(false)
        })
        .collect();

    if targets.is_empty() {
        println!(
            "no changed Rust/Python/TS/Markdown modules in {label} ({} changed path(s) total)",
            diffs.len()
        );
    } else {
        let target_names: BTreeSet<&str> = targets.iter().map(|m| m.name.as_str()).collect();
        let (upstream, downstream) = query::neighbors(&head, &target_names);
        match format {
            Format::Md => print!(
                "{}",
                render::changed_md(&label, &targets, &upstream, &downstream)
            ),
            Format::Json => {
                let json = serde_json::json!({
                    "range": label,
                    "changed": targets,
                    "upstream": upstream,
                    "downstream": downstream,
                });
                println!("{}", serde_json::to_string_pretty(&json)?);
            }
        }
    }
    cleanup(&repo, wt_b);
    Ok(())
}

/// Split a `A..B` range into (A, Some(B)); a bare `A` (or `A..`) yields
/// (A, None), meaning "diff against the working tree". Tolerates `A...B`.
fn parse_range(range: &str) -> (String, Option<String>) {
    match range.split_once("..") {
        Some((a, rest)) => {
            let b = rest.trim_start_matches('.').trim();
            (a.trim().to_string(), (!b.is_empty()).then(|| b.to_string()))
        }
        None => (range.trim().to_string(), None),
    }
}

/// Build the module graph as of `reference` (via a throwaway detached
/// worktree) or, when `reference` is None, from the live working tree at
/// `live`. Returns the graph and the worktree dir to clean up, if any.
fn graph_at(
    repo: &std::path::Path,
    rel: &std::path::Path,
    reference: Option<&str>,
    live: &std::path::Path,
    tag: &str,
) -> Result<(Graph, Option<PathBuf>)> {
    match reference {
        Some(r) => {
            let wt = std::env::temp_dir().join(format!("ctx-{tag}-{}", std::process::id()));
            git::add_worktree(repo, &wt, r)?;
            match extract::build_graph(&wt.join(rel)) {
                Ok(g) => Ok((g, Some(wt))),
                Err(e) => {
                    git::remove_worktree(repo, &wt);
                    Err(e)
                }
            }
        }
        None => Ok((extract::build_graph(live)?, None)),
    }
}

fn cleanup(repo: &std::path::Path, wt: Option<PathBuf>) {
    if let Some(wt) = wt {
        git::remove_worktree(repo, &wt);
    }
}

/// Render `g` at the richest view at or below `start` whose output fits
/// `max_tokens` (~len/4). Never truncates; if even skeleton is over budget it
/// returns skeleton with `fit = false`. Returns (view_used, text, fit).
fn render_budgeted(g: &Graph, start: View, format: Format, max_tokens: usize) -> (View, String, bool) {
    let ladder = [View::Full, View::Interface, View::Skeleton];
    let start_idx = ladder.iter().position(|&v| v == start).unwrap_or(0);
    let render = |v: View| -> String {
        let mut gg = g.clone();
        view::apply(&mut gg, v);
        match format {
            Format::Md => render::markdown(&gg),
            Format::Json => serde_json::to_string_pretty(&gg).unwrap_or_default(),
        }
    };
    let mut coarsest = (View::Skeleton, String::new());
    for &v in &ladder[start_idx..] {
        let text = render(v);
        if text.len() / 4 <= max_tokens {
            return (v, text, true);
        }
        coarsest = (v, text);
    }
    (coarsest.0, coarsest.1, false)
}

#[cfg(test)]
mod tests {
    use super::parse_range;

    #[test]
    fn parse_range_forms() {
        assert_eq!(parse_range("a..b"), ("a".into(), Some("b".into())));
        assert_eq!(parse_range("main..feature"), ("main".into(), Some("feature".into())));
        // Three-dot range tolerated; leading dots on B stripped.
        assert_eq!(parse_range("a...b"), ("a".into(), Some("b".into())));
        // A bare ref or an open-ended `A..` means "vs the working tree".
        assert_eq!(parse_range("HEAD~1"), ("HEAD~1".into(), None));
        assert_eq!(parse_range("main.."), ("main".into(), None));
        // Whitespace is trimmed.
        assert_eq!(parse_range(" a .. b "), ("a".into(), Some("b".into())));
    }
}
