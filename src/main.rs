mod extract;
mod git;
mod model;
mod query;
mod render;
mod view;

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand, ValueEnum};

use model::Module;
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
    /// Impact map of a diff: changed modules + upstream deps + downstream callers
    Changed {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Diff against this git ref instead of the working tree vs HEAD
        #[arg(long)]
        since: Option<String>,
        #[arg(long, value_enum, default_value_t = Format::Md)]
        format: Format,
        #[arg(long, value_enum, default_value_t = View::Full)]
        view: View,
    },
    /// Print the recommended CLAUDE.md discovery-protocol block
    Snippet,
}

const SNIPPET: &str = r#"## Codebase Discovery Tools

`ctx` is a deterministic static-analysis CLI for this repo (Rust, Python, TypeScript). Use it to
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
- `ctx changed [--since <ref>]` — impact map of your diff: the changed modules plus their
  dependencies and the callers they may break (defaults to the working tree vs HEAD)
- add `--format json` to any of the above for machine-readable output

Protocol: before modifying or analyzing code, run `ctx map --view skeleton` to load the
topology. To work on a module, run `ctx subtree <module>` first and follow its call edges.
If you encounter an unfamiliar type or function, run `ctx def <name>` (or `ctx subtree` on its
defining module) instead of reading the file. Before changing a function's signature or
behavior, run `ctx callers <name>` to see everything that depends on it. Line anchors (`[L42]`)
give exact positions for surgical reads.
"#;

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Map {
            path,
            format,
            view,
            output,
        } => {
            let mut g = extract::build_graph(&path)?;
            view::apply(&mut g, view);
            let rendered = match format {
                Format::Md => render::markdown(&g),
                Format::Json => serde_json::to_string_pretty(&g)?,
            };
            match output {
                Some(file) => {
                    fs::write(&file, &rendered)?;
                    eprintln!(
                        "wrote {} ({} modules, {} bytes)",
                        file.display(),
                        g.modules.len(),
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
        Cmd::Changed {
            path,
            since,
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
        Cmd::Snippet => print!("{SNIPPET}"),
    }
    Ok(())
}
