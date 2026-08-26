mod extract;
mod git;
mod mcp;
mod model;
mod parity;
mod query;
mod refactor;
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
    about = "Queryable code graph for coding agents: call graphs, blast radius, API breakage, cross-language parity"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// Skip paths matching a gitignore-style glob (repeatable), e.g.
    /// --exclude 'docs/archive/**' --exclude 'vendor/**'
    #[arg(long, global = true)]
    exclude: Vec<String>,
    /// Restrict the scan to these languages (repeatable). `code` means every
    /// supported language except Markdown — useful when prose dominates a map.
    #[arg(long, value_enum, global = true)]
    lang: Vec<LangArg>,
}

/// Language selector for `--lang`.
#[derive(Clone, Copy, ValueEnum)]
enum LangArg {
    Rust,
    Python,
    Ts,
    Md,
    /// Every supported language except Markdown.
    Code,
}

impl LangArg {
    fn expand(self) -> Vec<model::Lang> {
        use model::Lang;
        match self {
            LangArg::Rust => vec![Lang::Rust],
            LangArg::Python => vec![Lang::Python],
            LangArg::Ts => vec![Lang::TypeScript],
            LangArg::Md => vec![Lang::Markdown],
            LangArg::Code => vec![Lang::Rust, Lang::Python, Lang::TypeScript],
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Md,
    Json,
}

/// A systematic cross-language rename table for `ctx parity`.
#[derive(Clone, Copy, ValueEnum)]
enum AliasSet {
    /// Python → Rust conventions (e.g. `__init__` → `new`)
    PyRust,
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
        /// full (private items + call edges). Defaults to skeleton: an
        /// unbudgeted `full` map is ~258k tokens on a large repo, and the
        /// expensive view should be asked for, not stumbled into.
        #[arg(long, value_enum, default_value_t = View::Skeleton)]
        view: View,
        /// Fit the output to a token budget: emit the richest view at or below
        /// `--view` whose ~token count fits, reporting the choice on stderr. If
        /// even `skeleton` is over budget, the least-central modules are pruned
        /// until it fits, so the budget is a hard cap. Items inside a kept
        /// module are never truncated. Omit for the exact `--view`.
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
        /// Fit the output to a token budget: reduces detail, then drops the
        /// least-central neighbors. The target module is always kept.
        #[arg(long)]
        max_tokens: Option<usize>,
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
    /// Forward call tree from a symbol — what actually runs underneath it
    /// (add --reverse for the inbound tree: what reaches it)
    Trace {
        /// Function/method name, bare or qualified (Type::method)
        name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// How many call hops to expand
        #[arg(long, default_value_t = 3)]
        depth: usize,
        /// Walk callers instead of callees
        #[arg(long)]
        reverse: bool,
        #[arg(long, value_enum, default_value_t = Format::Md)]
        format: Format,
    },
    /// Shortest call path between two symbols — how execution gets from A to B
    Path {
        /// Starting function/method
        from: String,
        /// Destination function/method
        to: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_enum, default_value_t = Format::Md)]
        format: Format,
    },
    /// Coverage report: how much of the call graph resolved, and blind spots
    Doctor {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Print the full per-name census behind the numbers: every callee
        /// ctx could not pin, and every name it classified as external
        #[arg(long)]
        explain: bool,
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
        /// Fit the output to a token budget: sheds least-central neighbors.
        /// Changed modules are always kept.
        #[arg(long)]
        max_tokens: Option<usize>,
        /// With --api: exit non-zero if a public item was REMOVED. Signature
        /// changes are reported but do not fail, since ctx cannot tell an added
        /// optional parameter from an incompatible one. For CI gates.
        #[arg(long)]
        strict: bool,
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
        /// Fit the output to a token budget: sheds least-central neighbors.
        /// Changed modules are always kept.
        #[arg(long)]
        max_tokens: Option<usize>,
        /// With --api: exit non-zero on a breaking change. For CI gates.
        #[arg(long)]
        strict: bool,
        #[arg(long, value_enum, default_value_t = Format::Md)]
        format: Format,
        #[arg(long, value_enum, default_value_t = View::Full)]
        view: View,
    },
    /// Cross-language structural parity: is a port a faithful copy of its
    /// source? Reports members missing from the port, arity drift, dropped
    /// internal calls, moves, and additions. Structure only — never semantics.
    Parity {
        /// The original module (a file or directory)
        source: PathBuf,
        /// The port — one or more files/directories (compared as a union)
        #[arg(required = true)]
        target: Vec<PathBuf>,
        /// Exit non-zero if any source member is missing from the port
        #[arg(long)]
        strict: bool,
        /// Map a renamed container/type explicitly, e.g.
        /// --alias TriStateRouter=IntentGate (repeatable). Overrides inference.
        #[arg(long, value_parser = parse_alias)]
        alias: Vec<(String, String)>,
        /// Apply a systematic cross-language rename table (e.g. py-rust maps
        /// `__init__` → `new`). Alias matches are reported, never silent.
        #[arg(long, value_enum)]
        aliases: Option<AliasSet>,
        #[arg(long, value_enum, default_value_t = Format::Md)]
        format: Format,
    },
    /// Plan a module move: every import site that must be rewritten
    MovePlan {
        /// Module to move, e.g. "native::gate"
        from: String,
        /// Destination module path, e.g. "native::routing::gate"
        to: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_enum, default_value_t = Format::Md)]
        format: Format,
    },
    /// Check a completed move: module landed, old name gone, nothing orphaned
    MoveVerify {
        /// The module path that was moved away from
        from: String,
        /// The destination it should now live at
        to: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_enum, default_value_t = Format::Md)]
        format: Format,
    },
    /// Run as an MCP server over stdio (exposes the read-only commands as tools)
    Mcp {
        /// Directory the server is allowed to read. Every `path` argument
        /// resolves against it, and one that escapes it is refused. Defaults
        /// to the working directory — a model has no legitimate reason to
        /// leave the project it was pointed at.
        #[arg(long)]
        root: Option<PathBuf>,
        /// Append one JSON line per tool call (tool, arguments, output tokens,
        /// duration) to this file, plus a session total on exit. `-` for
        /// stderr. Off by default; never affects a tool's response.
        #[arg(long)]
        metrics: Option<PathBuf>,
    },
    /// Print the CLAUDE.md discovery block, measured for this repo
    Snippet {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

/// Build the CLAUDE.md discovery block for a specific repo.
///
/// This is the one artifact ctx emits that gets *copied* somewhere else, so it is
/// the one that can go stale. The previous block was generic prose telling every
/// agent to "run `ctx map --view skeleton` to load the topology" — advice that is
/// fine at 20 modules and catastrophic at 720, and which sat wrong in a repo's
/// CLAUDE.md long after the tool had learned better.
///
/// Two changes make staleness self-correcting rather than inevitable:
///
/// 1. **It measures.** Module count, prose share, real map cost and the
///    call-resolution rate are read off THIS repo, so the guidance is derived
///    rather than asserted, and the numbers tell an agent what to trust.
/// 2. **It is delimited.** The block is fenced by markers so regenerating
///    replaces it in place instead of appending a second, contradictory copy.
///
/// It is also short. A CLAUDE.md block is resident in every session forever, so
/// anything an agent can get from `--help` on demand does not belong here — only
/// what it cannot derive: this repo's scale, and where ctx's output is not
/// trustworthy.
fn snippet_for(g: &Graph) -> String {
    let modules = g.modules.len();

    let render_at = |v: View| -> usize {
        let mut gg = g.clone();
        view::apply(&mut gg, v);
        est_tokens(&render::markdown(&gg))
    };
    let skeleton_tok = render_at(View::Skeleton);
    let full_tok = render_at(View::Full);

    // Measured on the SKELETON view, because that is the view the advice is
    // about. At full view code carries bodies and call edges and dwarfs prose,
    // which understates how much of an *orientation* map is documentation.
    let mut skel = g.clone();
    view::apply(&mut skel, View::Skeleton);
    let prose_bytes: usize = skel
        .modules
        .iter()
        .filter(|m| m.lang == model::Lang::Markdown)
        .map(|m| {
            let mut s = String::new();
            render::module_md(m, &mut s);
            s.len()
        })
        .sum();
    let all_bytes: usize = skel
        .modules
        .iter()
        .map(|m| {
            let mut s = String::new();
            render::module_md(m, &mut s);
            s.len()
        })
        .sum();
    let prose_pct = (prose_bytes * 100).checked_div(all_bytes).unwrap_or(0);

    let (sites, resolved): (usize, usize) = g.modules.iter().fold((0, 0), |(s, r), m| {
        (s + m.diag.call_sites, r + m.diag.resolved)
    });
    let resolve_pct = (resolved * 100).checked_div(sites).unwrap_or(0);

    let mut out = String::from("<!-- ctx:begin — regenerate with `ctx snippet` -->\n");
    out.push_str("## Codebase Discovery\n\n");
    out.push_str(
        "`ctx` is a deterministic structural index (Rust/Python/TS/Markdown). Use it to locate \
         and orient; read raw files for implementation bodies. `ctx <cmd> --help` for detail.\n\n",
    );

    out.push_str(&format!(
        "**This repo:** {modules} modules · unbudgeted map ~{full_tok} tok (skeleton \
         ~{skeleton_tok}) · {prose_pct}% of the map is prose · {resolve_pct}% of call sites \
         resolve internally.\n\n"
    ));

    // Targeted lookups lead. An orientation-first protocol reads well and does not
    // survive contact with an agent that has a concrete task: measured over one
    // long working session, the "start with `ctx core`" step was never taken once,
    // while the value that did land came from jumping straight to a symbol.
    out.push_str("Reach for these first — they answer a question you actually have:\n\n");
    out.push_str(
        "- `ctx def <name>` — where a symbol is defined, across languages, without guessing \
         the file.\n",
    );
    out.push_str(
        "- `ctx callers <name>` — who calls it, before you change a signature. Also answers \
         `<Type>`: what references a type, which is the most common breaking change.\n",
    );
    out.push_str(
        "- `ctx context <symbol>` — definition + signature types + callees + callers in one \
         call. Usually enough to edit without opening the file.\n",
    );
    out.push_str("- `ctx changed --api` before pushing — public items removed or changed, and who breaks.\n\n");
    out.push_str("When you genuinely do not know where you are:\n\n");
    out.push_str("- `ctx core` — the modules that matter most, by dependency centrality.\n");
    // Gate on the cost of what an agent would actually run — the default map —
    // not on the cheapest view. Calling an 18k-token map "affordable" because its
    // skeleton fits is the same category of wrong advice this block replaced.
    if full_tok > 15_000 {
        out.push_str(&format!(
            "- `ctx map --max-tokens 10000{}` — whole-topology view. **Never run an unbudgeted \
             map here** (~{full_tok} tok); `--max-tokens` is a hard cap.\n",
            if prose_pct >= 25 { " --lang code" } else { "" }
        ));
    } else {
        out.push_str(&format!(
            "- `ctx map` — the whole map is ~{full_tok} tok here, small enough to read \
             directly. Pass `--max-tokens` if it grows.\n"
        ));
    }
    out.push('\n');

    out.push_str(
        "**Trust:** `ctx callers` is a floor, never a complete answer. It prints `INCOMPLETE` \
         when a name has several definitions and `NOT INDEXED` for suppressed common names \
         (`get`, `open`, `push`, …) where it indexes nothing at all. No warning means no *known* \
         reason to distrust it, not a guarantee — run the `rg` it prints before changing a \
         signature. A trailing `~` on an edge means it was inferred from an opaque receiver.\n",
    );
    // Only warn when there is a real measurement to warn about: a repo with no
    // call sites scores 0% and would otherwise be told its call graph is
    // untrustworthy, which is noise, not calibration.
    if sites >= 50 && resolve_pct < 30 {
        out.push_str(&format!(
            "Only {resolve_pct}% of call sites resolve internally here, so calibrate with \
             `ctx doctor` before leaning on call edges.\n"
        ));
    }
    out.push_str(
        "\n`CODEBASE_MAP.md` is generated, not committed: gitignore it and rebuild on demand.\n",
    );
    out.push_str("<!-- ctx:end -->\n");
    out
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Scan scope is fixed by argv, so install it before any graph is built.
    extract::set_filter(extract::Filter {
        exclude: cli.exclude.clone(),
        langs: cli.lang.iter().flat_map(|l| l.expand()).collect(),
    });
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
            // What actually landed in the output: pruning can drop modules, and
            // the "wrote" line must report emitted modules, not parsed ones.
            let mut emitted_count = module_count;
            let rendered = match max_tokens {
                Some(budget) => {
                    let b = render_budgeted(&g, view, format, budget);
                    emitted_count = module_count - b.omitted;
                    // Match on `fit` first: an over-budget render that pruned
                    // nothing still has to say so.
                    let detail = match (b.fit, b.omitted) {
                        (true, 0) => String::new(),
                        (true, n) => {
                            format!(" — pruned {n} of {module_count} least-central modules to fit")
                        }
                        (false, n) => {
                            // The map's header and footer are a fixed floor that
                            // no amount of pruning removes, so at very small
                            // budgets they, not the modules, are what overflows.
                            let floor = est_tokens(&b.text);
                            format!(
                                " — BUDGET NOT MET: emitted ~{floor} tok with {emitted_count} of \
                                 {module_count} module(s) ({n} omitted); the map header and \
                                 footer alone exceed {budget} tok"
                            )
                        }
                    };
                    eprintln!(
                        "budget {budget} tok → {} view (~{} tok){detail}",
                        b.view.name(),
                        est_tokens(&b.text),
                    );
                    // stdout is what a pipe, an `-o` file and an MCP caller see;
                    // a verdict that only reaches stderr is a verdict nobody
                    // downstream can act on.
                    annotate_budget(b.text, b.view, budget, b.fit, format)
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
                        "wrote {} ({} modules{}, {} bytes)",
                        file.display(),
                        emitted_count,
                        if emitted_count == module_count {
                            String::new()
                        } else {
                            format!(" of {module_count}")
                        },
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
            max_tokens,
        } => {
            let g = extract::build_graph(&path)?;
            let text = subtree_text(&g, &module, view, format, max_tokens)?;
            if let Some(budget) = max_tokens {
                eprintln!("budget {budget} tok → subtree (~{} tok)", est_tokens(&text));
            }
            print!("{text}");
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
                query::core(
                    &g,
                    limit,
                    churn_map.as_ref(),
                    matches!(format, Format::Json)
                )
            );
        }
        Cmd::Trace {
            name,
            path,
            depth,
            reverse,
            format,
        } => {
            let g = extract::build_graph(&path)?;
            print!(
                "{}",
                query::trace(&g, &name, depth, reverse, matches!(format, Format::Json))
            );
        }
        Cmd::Path {
            from,
            to,
            path,
            format,
        } => {
            let g = extract::build_graph(&path)?;
            print!(
                "{}",
                query::path(&g, &from, &to, matches!(format, Format::Json))
            );
        }
        Cmd::Doctor {
            path,
            explain,
            format,
        } => {
            let g = extract::build_graph(&path)?;
            let unsupported = extract::unsupported_census(&path);
            print!(
                "{}",
                query::coverage_report(&g, &unsupported, explain, matches!(format, Format::Json))
            );
        }
        Cmd::Changed {
            path,
            since,
            api,
            max_tokens: _,
            strict,
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
            let (report, removed_n, changed_n) =
                query::api_report(&base, &current, label, matches!(format, Format::Json));
            print!("{report}");
            if strict && removed_n > 0 {
                eprintln!(
                    "ctx: {removed_n} public item(s) REMOVED vs {label} — failing (--strict)"
                );
                std::process::exit(1);
            }
            if strict && changed_n > 0 {
                eprintln!(
                    "ctx: {changed_n} signature change(s) vs {label} — reported, not failing \
                     (ctx cannot distinguish an added optional parameter from an incompatible one)"
                );
            }
        }
        Cmd::Changed {
            path,
            since,
            api: _,
            max_tokens,
            strict: _,
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
            print!(
                "{}",
                impact_text(
                    &g,
                    label,
                    &targets,
                    &upstream,
                    &downstream,
                    format,
                    max_tokens
                )
            );
        }
        Cmd::Diff {
            range,
            path,
            api,
            max_tokens,
            strict,
            format,
            view,
        } => run_diff(&range, &path, api, format, view, max_tokens, strict)?,
        Cmd::Parity {
            source,
            target,
            strict,
            alias,
            aliases,
            format,
        } => {
            let src = members_for(&source)?;
            let mut tgt = Vec::new();
            for t in &target {
                tgt.extend(members_for(t)?);
            }
            let amap = match aliases {
                Some(AliasSet::PyRust) => parity::py_rust_aliases(),
                None => parity::AliasMap::new(),
            };
            let containers: std::collections::BTreeMap<String, String> =
                alias.into_iter().collect();
            let (out, missing) = parity::report(
                &src,
                &tgt,
                &amap,
                &containers,
                matches!(format, Format::Json),
            );
            print!("{out}");
            if strict && missing > 0 {
                std::process::exit(1);
            }
        }
        Cmd::MovePlan {
            from,
            to,
            path,
            format,
        } => {
            let g = extract::build_graph(&path)?;
            print!(
                "{}",
                refactor::move_plan(&g, &from, &to, matches!(format, Format::Json))
            );
        }
        Cmd::MoveVerify {
            from,
            to,
            path,
            format,
        } => {
            let g = extract::build_graph(&path)?;
            let (out, ok) = refactor::move_verify(&g, &from, &to, matches!(format, Format::Json));
            print!("{out}");
            if !ok {
                std::process::exit(1);
            }
        }
        Cmd::Mcp { root, metrics } => mcp::run(root, metrics)?,
        Cmd::Snippet { path } => {
            let g = extract::build_graph(&path)?;
            print!("{}", snippet_for(&g));
        }
    }
    Ok(())
}

/// Resolve a parity path argument to a flattened member bag. A directory is
/// flattened whole; a single file builds the graph for its parent directory
/// and flattens only the module that file produced.
fn members_for(path: &std::path::Path) -> Result<Vec<parity::MemberView>> {
    let meta =
        fs::metadata(path).map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
    if meta.is_dir() {
        let g = extract::build_graph(path)?;
        return Ok(g.modules.iter().flat_map(parity::flatten).collect());
    }
    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    let g = extract::build_graph(dir)?;
    let want = path.canonicalize()?;
    let mut out = Vec::new();
    for m in &g.modules {
        if dir.join(&m.file).canonicalize().ok().as_deref() == Some(&want) {
            out.extend(parity::flatten(m));
        }
    }
    if out.is_empty() {
        bail!(
            "no module for {} — unsupported language, empty file, or no definitions",
            path.display()
        );
    }
    Ok(out)
}

/// `ctx diff A..B` — map the file changes between two refs onto B's module
/// graph (B defaults to the working tree). With `--api`, diff the public API
/// surface between the two refs instead.
fn run_diff(
    range: &str,
    path: &std::path::Path,
    api: bool,
    format: Format,
    view: View,
    max_tokens: Option<usize>,
    strict: bool,
) -> Result<()> {
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
        let (report, removed_n, changed_n) = out;
        print!("{report}");
        if strict && removed_n > 0 {
            eprintln!("ctx: {removed_n} public item(s) REMOVED in {a} — failing (--strict)");
            std::process::exit(1);
        }
        if strict && changed_n > 0 {
            eprintln!("ctx: {changed_n} signature change(s) in {a} — reported, not failing");
        }
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
        print!(
            "{}",
            impact_text(
                &head,
                &label,
                &targets,
                &upstream,
                &downstream,
                format,
                max_tokens
            )
        );
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

/// Render a change-impact report, optionally fitted to a token budget.
///
/// Shared by `changed` and `diff`. Changed modules are always kept — they are the
/// subject of the report — so only neighbors are shed, least-central first. A
/// single commit on a 14-module repo rendered ~10k tokens unbudgeted, which is
/// the wrong shape for something meant to be read during review.
fn impact_text(
    g: &Graph,
    label: &str,
    targets: &[&Module],
    upstream: &[&Module],
    downstream: &[&Module],
    format: Format,
    budget: Option<usize>,
) -> String {
    let rank_of: std::collections::HashMap<&str, usize> = query::centrality_order(g)
        .into_iter()
        .enumerate()
        .map(|(pos, i)| (g.modules[i].name.as_str(), pos))
        .collect();

    let render = |cap: Option<usize>| -> String {
        let mut up: Vec<&Module> = upstream.to_vec();
        let mut down: Vec<&Module> = downstream.to_vec();
        let mut dropped = 0usize;
        if let Some(cap) = cap {
            let by_rank = |set: &mut Vec<&Module>| {
                set.sort_by_key(|m| rank_of.get(m.name.as_str()).copied().unwrap_or(usize::MAX));
            };
            by_rank(&mut up);
            by_rank(&mut down);
            let half = cap.div_ceil(2);
            let up_keep = half.min(up.len());
            let down_keep = (cap - up_keep).min(down.len());
            dropped = (up.len() - up_keep) + (down.len() - down_keep);
            up.truncate(up_keep);
            down.truncate(down_keep);
            up.sort_by(|a, b| a.name.cmp(&b.name));
            down.sort_by(|a, b| a.name.cmp(&b.name));
        }
        match format {
            Format::Md => {
                let mut s = render::changed_md(label, targets, &up, &down);
                if dropped > 0 {
                    s.push_str(&format!(
                        "\n---\n{dropped} less-central neighbor(s) omitted to fit the token \
                         budget. The changed modules themselves are always kept.\n"
                    ));
                }
                s
            }
            Format::Json => {
                let mut v = serde_json::json!({
                    "since": label,
                    "changed": targets,
                    "upstream": up,
                    "downstream": down,
                });
                if dropped > 0 {
                    if let Some(o) = v.as_object_mut() {
                        o.insert("neighbors_omitted".into(), serde_json::json!(dropped));
                    }
                }
                serde_json::to_string_pretty(&v).unwrap_or_default() + "\n"
            }
        }
    };

    let Some(budget) = budget else {
        return render(None);
    };
    let whole = render(None);
    if est_tokens(&whole) <= budget {
        return whole;
    }
    let max_neighbors = upstream.len() + downstream.len();
    let bare = render(Some(0));
    if est_tokens(&bare) > budget {
        // The changed modules alone are over budget. They are the subject of the
        // report and are never dropped, so say the cap was not met rather than
        // implying this output fits.
        let mut text = bare;
        if matches!(format, Format::Md) {
            let n = est_tokens(&text);
            text.push_str(&format!(
                "\n---\n[ctx] BUDGET NOT MET: the changed modules alone are ~{n} tokens, over \
                 the {budget}-token budget. All neighbors were dropped. Narrow the diff, or use \
                 `--view interface`.\n"
            ));
        }
        return text;
    }
    let (mut lo, mut hi) = (0usize, max_neighbors);
    let mut best = bare;
    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        let text = render(Some(mid));
        if est_tokens(&text) <= budget {
            best = text;
            lo = mid + 1;
        } else if mid == 0 {
            break;
        } else {
            hi = mid - 1;
        }
    }
    best
}

/// Stamp the emitted view and budget verdict into the output itself.
///
/// An agent that asked for `full` and silently received `interface` will look for
/// call edges that were deleted; one handed an over-budget map needs to know the
/// cap was not met. Both facts are free to include and impossible to recover.
fn annotate_budget(text: String, view: View, budget: usize, fit: bool, format: Format) -> String {
    let verdict = if fit {
        format!("fitted to {budget} tokens")
    } else {
        format!("BUDGET NOT MET: exceeds {budget} tokens")
    };
    match format {
        Format::Md => text.replacen(
            "# Codebase Topology Map\n",
            &format!(
                "# Codebase Topology Map\nview: {} ({verdict})\n",
                view.name()
            ),
            1,
        ),
        Format::Json => text,
    }
}

/// Render a subtree, optionally fitted to a token budget.
///
/// Shared by the CLI and the MCP server so both honor the same budget. Fitting
/// reduces detail first (full → interface → skeleton); if that is not enough it
/// drops the least-central *neighbors*, never the target — the target is the
/// thing you asked about, so it is the one module that must survive.
fn subtree_text(
    g: &Graph,
    module: &str,
    view: View,
    format: Format,
    budget: Option<usize>,
) -> Result<String> {
    let matches_query = |name: &str| {
        name == module
            || name.ends_with(&format!("::{module}"))
            || name.ends_with(&format!(".{module}"))
    };
    if !g.modules.iter().any(|m| matches_query(&m.name)) {
        bail!("{}", not_found_message(g, module));
    }
    // Centrality of the whole graph, so neighbor ranking does not shift with the
    // view or the cap.
    let rank_of: std::collections::HashMap<&str, usize> = query::centrality_order(g)
        .into_iter()
        .enumerate()
        .map(|(pos, i)| (g.modules[i].name.as_str(), pos))
        .collect();

    let render_at = |v: View, neighbor_cap: Option<usize>| -> String {
        let mut gg = g.clone();
        view::apply(&mut gg, v);
        let targets: Vec<&Module> = gg
            .modules
            .iter()
            .filter(|m| matches_query(&m.name))
            .collect();
        let target_names: BTreeSet<&str> = targets.iter().map(|m| m.name.as_str()).collect();
        let (mut upstream, mut downstream) = query::neighbors(&gg, &target_names);
        let mut dropped = 0usize;
        if let Some(cap) = neighbor_cap {
            let by_rank = |set: &mut Vec<&Module>| {
                set.sort_by_key(|m| rank_of.get(m.name.as_str()).copied().unwrap_or(usize::MAX));
            };
            by_rank(&mut upstream);
            by_rank(&mut downstream);
            // Split the cap between the two sides so neither starves the other.
            let half = cap.div_ceil(2);
            let up_keep = half.min(upstream.len());
            let down_keep = (cap - up_keep).min(downstream.len());
            dropped = (upstream.len() - up_keep) + (downstream.len() - down_keep);
            upstream.truncate(up_keep);
            downstream.truncate(down_keep);
            // Restore name order for a stable, readable rendering.
            upstream.sort_by(|a, b| a.name.cmp(&b.name));
            downstream.sort_by(|a, b| a.name.cmp(&b.name));
        }
        match format {
            Format::Md => {
                let mut s = render::subtree_md(module, &targets, &upstream, &downstream);
                if dropped > 0 {
                    s.push_str(&format!(
                        "\n---\n{dropped} less-central neighbor(s) omitted to fit the token \
                         budget. Ask for one by name with `ctx subtree <module>`.\n"
                    ));
                }
                s
            }
            Format::Json => {
                let mut v = serde_json::json!({
                    "query": module,
                    "target": targets,
                    "upstream": upstream,
                    "downstream": downstream,
                });
                if dropped > 0 {
                    if let Some(o) = v.as_object_mut() {
                        o.insert("neighbors_omitted".into(), serde_json::json!(dropped));
                    }
                }
                serde_json::to_string_pretty(&v).unwrap_or_default() + "\n"
            }
        }
    };

    let Some(budget) = budget else {
        return Ok(render_at(view, None));
    };

    let max_neighbors = {
        let names: BTreeSet<&str> = g
            .modules
            .iter()
            .filter(|m| matches_query(&m.name))
            .map(|m| m.name.as_str())
            .collect();
        let (u, d) = query::neighbors(g, &names);
        u.len() + d.len()
    };

    // Richest view whose PRUNED form fits, not the first view that fits whole.
    // Returning early at the coarsest view left most of the budget unspent —
    // `--max-tokens 4000` and `--max-tokens 12000` produced identical output —
    // when the budget could have bought `interface` detail on fewer neighbors.
    let ladder = [View::Full, View::Interface, View::Skeleton];
    let start = ladder.iter().position(|&v| v == view).unwrap_or(0);
    let mut best: Option<String> = None;
    for &v in &ladder[start..] {
        let whole = render_at(v, None);
        if est_tokens(&whole) <= budget {
            return Ok(whole);
        }
        // Largest neighbor set that fits at this detail level.
        let (mut lo, mut hi) = (0usize, max_neighbors);
        let mut fit_here: Option<String> = None;
        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            let text = render_at(v, Some(mid));
            if est_tokens(&text) <= budget {
                fit_here = Some(text);
                lo = mid + 1;
            } else if mid == 0 {
                break;
            } else {
                hi = mid - 1;
            }
        }
        if let Some(t) = fit_here {
            // Richest rung wins: stop at the first view that fits when pruned.
            return Ok(t);
        }
        best = Some(render_at(v, Some(0)));
    }

    // Not satisfiable at any rung: the target module alone is over budget. Emit
    // the coarsest form and say so in band rather than implying a fit.
    let mut text = best.unwrap_or_else(|| render_at(View::Skeleton, Some(0)));
    if matches!(format, Format::Md) {
        text.push_str(&format!(
            "\n---\n[ctx] BUDGET NOT MET: the target module alone is ~{} tokens, over the \
             {budget}-token budget. Use `ctx def`/`ctx context <symbol>` for a smaller slice.\n",
            est_tokens(&text)
        ));
    }
    Ok(text)
}

/// Parse `--alias Old=New` into a container rename pair.
fn parse_alias(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((a, b)) if !a.trim().is_empty() && !b.trim().is_empty() => {
            Ok((a.trim().to_string(), b.trim().to_string()))
        }
        _ => Err(format!("expected Old=New, got '{s}'")),
    }
}

/// A module-not-found error with near misses instead of the whole index.
///
/// Dumping every module name cost 24 KB on a 720-module repo — a typo should not
/// spend thousands of tokens of an agent's context. Candidates are ranked by
/// substring containment, then by shared prefix length with the query.
fn not_found_message(g: &Graph, query: &str) -> String {
    const MAX_SUGGESTIONS: usize = 12;
    let q = query.to_lowercase();
    let shared_prefix = |name: &str| -> usize {
        name.to_lowercase()
            .chars()
            .zip(q.chars())
            .take_while(|(a, b)| a == b)
            .count()
    };
    let mut ranked: Vec<(&str, bool, usize)> = g
        .modules
        .iter()
        .map(|m| {
            let lower = m.name.to_lowercase();
            let last = lower
                .rsplit(['.', ':'])
                .next()
                .unwrap_or(&lower)
                .to_string();
            (
                m.name.as_str(),
                lower.contains(&q) || last.contains(&q),
                shared_prefix(&m.name),
            )
        })
        .collect();
    // Contains-match first, then longest shared prefix, then name for stability.
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)).then(a.0.cmp(b.0)));
    let suggestions: Vec<&str> = ranked
        .iter()
        .filter(|(_, contains, prefix)| *contains || *prefix > 0)
        .take(MAX_SUGGESTIONS)
        .map(|(n, _, _)| *n)
        .collect();

    let total = g.modules.len();
    if suggestions.is_empty() {
        return format!(
            "no module matching '{query}' among {total} modules.\n\
             Run `ctx modules` for the full list, or `ctx core` for the ones that matter most."
        );
    }
    format!(
        "no module matching '{query}' among {total} modules. Closest:\n  {}\n\
         Run `ctx modules` for the full list, or `ctx core` for the ones that matter most.",
        suggestions.join("\n  ")
    )
}

/// Bytes per token for budget arithmetic.
///
/// `len/4` is the familiar rule of thumb, but measured against a real tokenizer
/// on this tool's own output it under-counts by 5-23% — code and Markdown are
/// denser than prose English. A cap that overshoots is not a cap, so budgets use
/// a conservative divisor: better to under-fill than to blow the window.
pub const BYTES_PER_TOKEN: usize = 3;

/// Estimated tokens for rendered output.
pub fn est_tokens(text: &str) -> usize {
    text.len() / BYTES_PER_TOKEN
}

/// Most a budget-pruned map will spend on prose (Markdown) modules, as a share
/// of its rendered size. Docs link to each other and earn genuine centrality, so
/// without a ceiling they can take a third of a small orientation budget in a
/// codebase map. Applies only when pruning is required — an unbudgeted map, and
/// a budget met by detail reduction alone, are never rebalanced.
const PROSE_BUDGET_SHARE: f64 = 0.10;

/// The outcome of fitting a render to a token budget.
struct Budgeted {
    view: View,
    text: String,
    /// Whether the budget was actually met.
    fit: bool,
    /// Modules dropped to make it fit (0 when detail reduction sufficed).
    omitted: usize,
}

/// Render `g` at the richest view at or below `start` whose output fits
/// `max_tokens` (~len/4).
///
/// Detail is reduced first (full → interface → skeleton). If even skeleton is
/// over budget, detail is exhausted and the only remaining lever is dropping
/// modules, so the least-central ones are pruned until the map fits — the
/// budget is a real cap, not a suggestion. Items within a kept module are never
/// truncated: you get fewer modules, each still whole.
///
/// `fit = false` only when a single module already exceeds the budget.
fn render_budgeted(g: &Graph, start: View, format: Format, max_tokens: usize) -> Budgeted {
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
    for &v in &ladder[start_idx..] {
        let text = render(v);
        if est_tokens(&text) <= max_tokens {
            return Budgeted {
                view: v,
                text,
                fit: true,
                omitted: 0,
            };
        }
    }

    // Detail exhausted: prune modules. Skeleton is applied once up front so the
    // search clones a much smaller graph on each probe.
    let mut skel = g.clone();
    view::apply(&mut skel, View::Skeleton);
    let total = skel.modules.len();

    // Largest `keep` that fits. Output grows monotonically with `keep`, so
    // binary search costs ~log2(total) renders instead of a linear scan.
    let (mut lo, mut hi) = (1usize, total);
    let mut best: Option<(usize, String, usize)> = None;
    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        let (text, omitted) = render_pruned(&skel, format, max_tokens, mid, total);
        if est_tokens(&text) <= max_tokens {
            best = Some((mid, text, omitted));
            lo = mid + 1;
        } else {
            if mid == 1 {
                // Even one module is over budget — emit it and say so.
                return Budgeted {
                    view: View::Skeleton,
                    text,
                    fit: false,
                    omitted,
                };
            }
            hi = mid - 1;
        }
    }

    match best {
        Some((_, text, omitted)) => Budgeted {
            view: View::Skeleton,
            text,
            fit: true,
            omitted,
        },
        // Unreachable in practice: mid == 1 returns above.
        None => {
            let (text, omitted) = render_pruned(&skel, format, max_tokens, 1, total);
            Budgeted {
                view: View::Skeleton,
                text,
                fit: false,
                omitted,
            }
        }
    }
}

/// Render a skeleton graph pruned to its `keep` most central modules, annotated
/// with what was dropped so the omission is visible in the output itself.
/// `skel` must already have the skeleton view applied.
fn render_pruned(
    skel: &Graph,
    format: Format,
    budget: usize,
    keep: usize,
    total: usize,
) -> (String, usize) {
    let mut gg = skel.clone();
    // Scarce budget goes to code first: prose keeps its rank but not its space.
    let stats = view::prune_to_central(&mut gg, keep, PROSE_BUDGET_SHARE);
    let shown = gg.modules.len();
    let omitted = stats.omitted;
    let text = match format {
        Format::Md => {
            let mut s = render::markdown(&gg);
            if omitted > 0 {
                let prose_note = if stats.prose_capped > 0 {
                    format!(
                        "{} of the omitted modules are docs that were central enough to keep \
                         but would have pushed prose past {}% of the map.\n",
                        stats.prose_capped,
                        (PROSE_BUDGET_SHARE * 100.0).round() as u32
                    )
                } else {
                    String::new()
                };
                s.push_str(&format!(
                    "\n---\n\
                     {omitted} of {total} modules omitted: kept the {shown} most central, \
                     targeting the {budget}-token budget.\n\
                     {prose_note}\
                     Dependency edges above may name an omitted module — that name is still \
                     what to ask for.\n\
                     Use `ctx subtree <module>` or `ctx context <symbol>` for anything not \
                     listed; `ctx core` ranks the full set.\n"
                ));
            }
            s
        }
        Format::Json => {
            let mut v = serde_json::to_value(&gg).unwrap_or_default();
            if let Some(obj) = v.as_object_mut() {
                obj.insert("modules_shown".into(), serde_json::json!(shown));
                obj.insert("modules_total".into(), serde_json::json!(total));
                obj.insert("modules_omitted".into(), serde_json::json!(omitted));
                obj.insert("pruned_to_budget".into(), serde_json::json!(budget));
            }
            serde_json::to_string_pretty(&v).unwrap_or_default()
        }
    };
    (text, omitted)
}

#[cfg(test)]
mod tests {
    use super::{parse_range, render_budgeted, snippet_for, Format, View};
    use crate::extract::build_graph;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A repo with enough modules that a small budget cannot be met by detail
    /// reduction alone, forcing the pruning rung.
    ///
    /// Classes, not functions: skeleton view drops bare `def`s, so a repo of
    /// plain functions collapses to almost nothing and never needs pruning.
    fn wide_repo() -> std::path::PathBuf {
        // Unique per call: these tests run in parallel and each removes its dir,
        // so a shared path would have them deleting each other's fixture.
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ctx_budget_{}_{}", std::process::id(), id));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for i in 0..40 {
            let body: String = (0..20)
                .map(|j| format!("class ClassWithADeliberatelyLongName_{i}_{j}:\n    pass\n\n"))
                .collect();
            fs::write(dir.join(format!("mod_{i}.py")), body).unwrap();
        }
        dir
    }

    #[test]
    fn budget_is_a_hard_cap_once_detail_is_exhausted() {
        let dir = wide_repo();
        let g = build_graph(&dir).unwrap();
        let total = g.modules.len();
        let budget = 2_000;
        let b = render_budgeted(&g, View::Full, Format::Md, budget);
        let _ = fs::remove_dir_all(&dir);

        assert!(
            b.fit,
            "a 2000-token budget should be satisfiable by pruning"
        );
        assert!(
            b.text.len() / 4 <= budget,
            "emitted ~{} tok exceeds the {budget} tok budget",
            b.text.len() / 4
        );
        assert!(b.omitted > 0, "pruning should have dropped modules");
        assert!(b.omitted < total, "pruning should not drop everything");
        assert!(
            b.text.contains("modules omitted"),
            "a pruned map must disclose the omission in its output"
        );
    }

    /// The prose cap must never touch an unbudgeted map — that path is what
    /// writes a committed CODEBASE_MAP.md, which has to stay complete.
    #[test]
    fn unbudgeted_map_keeps_every_doc() {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ctx_docs_{}_{}", std::process::id(), id));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("code.py"), "class Solo:\n    pass\n").unwrap();
        for i in 0..6 {
            let body: String = (0..30)
                .map(|j| format!("## Section {i}_{j} with a reasonably long heading\n\n"))
                .collect();
            fs::write(dir.join(format!("doc_{i}.md")), format!("# D{i}\n\n{body}")).unwrap();
        }
        let mut g = build_graph(&dir).unwrap();
        let total = g.modules.len();
        // The no-budget path: apply the view and render, no pruning of any kind.
        crate::view::apply(&mut g, View::Full);
        let text = crate::render::markdown(&g);
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(g.modules.len(), total, "no module may be dropped");
        for i in 0..6 {
            assert!(
                text.contains(&format!("doc_{i}")),
                "doc_{i} must survive an unbudgeted render"
            );
        }
    }

    #[test]
    fn generous_budget_prunes_nothing() {
        let dir = wide_repo();
        let g = build_graph(&dir).unwrap();
        let b = render_budgeted(&g, View::Full, Format::Md, 10_000_000);
        let _ = fs::remove_dir_all(&dir);
        assert!(b.fit);
        assert_eq!(
            b.omitted, 0,
            "nothing should be dropped when everything fits"
        );
        assert!(!b.text.contains("modules omitted"));
    }

    #[test]
    fn unmeetable_budget_reports_no_fit_without_hanging() {
        let dir = wide_repo();
        let g = build_graph(&dir).unwrap();
        // Not satisfiable: a single module already exceeds one token.
        let b = render_budgeted(&g, View::Full, Format::Md, 1);
        let _ = fs::remove_dir_all(&dir);
        assert!(!b.fit, "an unmeetable budget must report fit = false");
        assert!(
            !b.text.is_empty(),
            "something usable should still be emitted"
        );
    }

    #[test]
    fn budget_render_is_deterministic() {
        let dir = wide_repo();
        let g = build_graph(&dir).unwrap();
        let a = render_budgeted(&g, View::Full, Format::Md, 2_000);
        let b = render_budgeted(&g, View::Full, Format::Md, 2_000);
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(
            a.text, b.text,
            "same graph and budget must render identically"
        );
        assert_eq!(a.omitted, b.omitted);
    }

    #[test]
    fn snippet_is_measured_and_scales_its_advice() {
        // Small repo: reading the whole map is fine, and the block must not
        // manufacture a low-resolution warning out of a repo with no call sites.
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ctx_snip_{}_{}", std::process::id(), id));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.py"), "class One:\n    pass\n").unwrap();
        let g = build_graph(&dir).unwrap();
        let small = snippet_for(&g);
        let _ = fs::remove_dir_all(&dir);

        assert!(
            small.contains("<!-- ctx:begin"),
            "must be delimited for in-place regen"
        );
        assert!(small.contains("<!-- ctx:end -->"));
        assert!(small.contains("1 modules") || small.contains("1 module"));
        assert!(
            small.contains("small enough to read directly"),
            "a tiny repo should not be told to budget: {small}"
        );
        assert!(
            !small.contains("calibrate with"),
            "no call sites means no resolution verdict to give: {small}"
        );
        assert!(
            small.contains("INCOMPLETE") && small.contains("NOT INDEXED"),
            "the callers caveat is safety-critical and must always be present"
        );
    }

    #[test]
    fn snippet_warns_when_an_unbudgeted_map_is_expensive() {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ctx_snipbig_{}_{}", std::process::id(), id));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for i in 0..40 {
            let body: String = (0..25)
                .map(|j| format!("class ClassWithAQuiteLongName_{i}_{j}:\n    pass\n\n"))
                .collect();
            fs::write(dir.join(format!("m_{i}.py")), body).unwrap();
        }
        let g = build_graph(&dir).unwrap();
        let big = snippet_for(&g);
        let _ = fs::remove_dir_all(&dir);
        assert!(
            big.contains("Never run an unbudgeted map here"),
            "an expensive map must be called out: {big}"
        );
        assert!(big.contains("--max-tokens"), "{big}");
    }

    #[test]
    fn parse_range_forms() {
        assert_eq!(parse_range("a..b"), ("a".into(), Some("b".into())));
        assert_eq!(
            parse_range("main..feature"),
            ("main".into(), Some("feature".into()))
        );
        // Three-dot range tolerated; leading dots on B stripped.
        assert_eq!(parse_range("a...b"), ("a".into(), Some("b".into())));
        // A bare ref or an open-ended `A..` means "vs the working tree".
        assert_eq!(parse_range("HEAD~1"), ("HEAD~1".into(), None));
        assert_eq!(parse_range("main.."), ("main".into(), None));
        // Whitespace is trimmed.
        assert_eq!(parse_range(" a .. b "), ("a".into(), Some("b".into())));
    }
}
