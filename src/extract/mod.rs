pub mod markdown;
pub mod python;
pub mod rust;
pub mod typescript;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::WalkBuilder;

use crate::model::{Binding, Call, Diagnostics, Graph, Item, Lang, Module, RawCall, Receiver};

const SKIP_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    "dist",
    "build",
    ".git",
];

const CHASE_DEPTH: usize = 8;

/// What to scan. Empty means "everything ctx supports".
///
/// Vendored trees, archived docs and dead code are the bulk of a large repo's
/// map, and `.gitignore` will not exclude them because they are legitimately
/// tracked. Without a lever the only options were "whole repo" or nothing.
#[derive(Default, Clone)]
pub struct Filter {
    /// Gitignore-style globs to skip (e.g. `docs/archive/**`).
    pub exclude: Vec<String>,
    /// Languages to include; empty means all supported.
    pub langs: Vec<Lang>,
}

/// Set once from the CLI before any graph is built, then read-only.
///
/// A process-wide filter rather than a parameter on 17 call sites: `ctx` is a
/// one-shot CLI where the scan scope is fixed by argv at startup, so threading it
/// through every command (and the MCP dispatcher, and parity) would be churn with
/// no added expressiveness.
static FILTER: std::sync::OnceLock<Filter> = std::sync::OnceLock::new();

pub fn set_filter(f: Filter) {
    let _ = FILTER.set(f);
}

fn filter() -> &'static Filter {
    FILTER.get_or_init(Filter::default)
}

/// One-line description of the active scan filter, or None when unfiltered.
///
/// A filtered graph is a different graph: deps vanish, modules disappear, and
/// `doctor` would otherwise report a clean bill of health for a tree it never
/// looked at. Reports that claim coverage must disclose their scope.
pub fn filter_note() -> Option<String> {
    let f = filter();
    if f.exclude.is_empty() && f.langs.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    if !f.langs.is_empty() {
        let names: Vec<&str> = f
            .langs
            .iter()
            .map(|l| match l {
                Lang::Rust => "rust",
                Lang::Python => "python",
                Lang::TypeScript => "ts",
                Lang::Markdown => "md",
            })
            .collect();
        parts.push(format!("--lang {}", names.join(",")));
    }
    for e in &f.exclude {
        parts.push(format!("--exclude '{e}'"));
    }
    Some(parts.join(" "))
}

/// Build the walker for `root`, applying any `--exclude` globs.
fn walker(root: &Path) -> WalkBuilder {
    let mut wb = WalkBuilder::new(root);
    let excludes = &filter().exclude;
    if !excludes.is_empty() {
        let mut ob = ignore::overrides::OverrideBuilder::new(root);
        for pat in excludes {
            // Only negations, so everything not matched still passes through.
            if let Err(e) = ob.add(&format!("!{pat}")) {
                eprintln!("ctx: ignoring bad --exclude glob '{pat}': {e}");
            }
        }
        match ob.build() {
            Ok(ov) => {
                wb.overrides(ov);
            }
            Err(e) => eprintln!("ctx: --exclude globs unusable, scanning everything: {e}"),
        }
    }
    wb
}

/// Whether this language is in scope for the current filter.
fn lang_selected(lang: Lang) -> bool {
    let langs = &filter().langs;
    langs.is_empty() || langs.contains(&lang)
}

pub fn build_graph(root: &Path) -> Result<Graph> {
    let root = root
        .canonicalize()
        .with_context(|| format!("cannot resolve path {}", root.display()))?;

    let mut files: Vec<PathBuf> = Vec::new();
    for entry in walker(&root).build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if path
            .components()
            .any(|c| SKIP_DIRS.contains(&c.as_os_str().to_str().unwrap_or("")))
        {
            continue;
        }
        let take = match path.extension().and_then(|e| e.to_str()) {
            Some("rs") => lang_selected(Lang::Rust),
            Some("py") => lang_selected(Lang::Python),
            Some("tsx") => lang_selected(Lang::TypeScript),
            Some("md") | Some("markdown") => lang_selected(Lang::Markdown),
            // Skip `.d.ts` — ambient type declarations carry no topology.
            Some("ts") => {
                lang_selected(Lang::TypeScript) && !path.to_str().is_some_and(|s| s.ends_with(".d.ts"))
            }
            _ => false,
        };
        if take {
            files.push(path.to_path_buf());
        }
    }
    files.sort();

    let mut modules = Vec::new();
    for path in &files {
        let src = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let rel = path.strip_prefix(&root).unwrap_or(path);
        let ext = path.extension().and_then(|e| e.to_str());
        let lang = match ext {
            Some("rs") => Lang::Rust,
            Some("py") => Lang::Python,
            Some("ts") | Some("tsx") => Lang::TypeScript,
            Some("md") | Some("markdown") => Lang::Markdown,
            _ => continue,
        };
        let mut facts = match lang {
            Lang::Rust => {
                rust::extract(&src).with_context(|| format!("parsing {}", rel.display()))?
            }
            Lang::Python => {
                python::extract(&src).with_context(|| format!("parsing {}", rel.display()))?
            }
            Lang::TypeScript => typescript::extract(&src, ext == Some("tsx"))
                .with_context(|| format!("parsing {}", rel.display()))?,
            Lang::Markdown => {
                markdown::extract(&src).with_context(|| format!("parsing {}", rel.display()))?
            }
        };
        let file_name = rel.file_name().and_then(|f| f.to_str());
        let is_package = match lang {
            Lang::Python => file_name == Some("__init__.py"),
            Lang::TypeScript => matches!(file_name, Some("index.ts") | Some("index.tsx")),
            // README / index collapse to their directory, like an index file.
            Lang::Markdown => file_name.is_some_and(|f| {
                let stem = f.rsplit_once('.').map_or(f, |(s, _)| s);
                stem.eq_ignore_ascii_case("readme") || stem.eq_ignore_ascii_case("index")
            }),
            _ => false,
        };
        // Any Python module can be imported through, but only __init__.py
        // bindings read as intentional re-exports worth rendering.
        if lang == Lang::Python && !is_package {
            for b in &mut facts.reexports {
                b.public = false;
            }
        }
        let (name, crate_prefix) = module_name(rel, lang);
        modules.push(Module {
            name,
            resolve_name: String::new(),
            heuristic_deps: Vec::new(),
            file: rel.display().to_string(),
            lang,
            deps: Vec::new(),
            extern_deps: Vec::new(),
            reexports: Vec::new(),
            items: facts.items,
            raw_imports: facts.imports,
            raw_reexports: facts.reexports,
            defined_names: facts.defined,
            crate_prefix,
            is_package,
            diag: Diagnostics::default(),
        });
    }

    disambiguate_module_names(&mut modules);
    resolve_deps(&mut modules, &root);
    modules.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Graph {
        root: root.display().to_string(),
        file_count: files.len(),
        modules,
    })
}

/// Give every module a unique name, renaming the losers of a collision.
///
/// Module names come from paths, so distinct files can land on the same name:
/// `src/lib.rs` and `src/main.rs` are both `crate`; `native/README.md` and
/// `src/native/mod.rs` are both `native`. Resolution indexes modules by name, so
/// a duplicate used to make one of them unreachable — its reverse call edges
/// silently vanished, which is the worst failure this tool has, because
/// `ctx callers` returning nothing reads as "safe to change".
///
/// Code keeps the bare name and prose is the one renamed — otherwise a
/// `native/README.md` can take the name of the `src/native/mod.rs` it documents,
/// which both misreports the top of `ctx core` and breaks `use crate::native::…`
/// resolution. Within a tie, the first file in sorted order wins, so the result
/// is deterministic. Losers get `name@stem`, falling back to `name@stem@N` if
/// stems collide too. Renames are reported on stderr — a visibly renamed module
/// is far better than a silently ambiguous graph.
fn disambiguate_module_names(modules: &mut [Module]) {
    // Visit code before prose so code claims the bare name; `modules` is already
    // in sorted-file order, which breaks ties.
    let mut order: Vec<usize> = (0..modules.len()).collect();
    order.sort_by_key(|&i| (modules[i].lang == Lang::Markdown, i));

    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut renames: Vec<(String, String, String)> = Vec::new();
    for i in order {
        let name = modules[i].name.clone();
        let count = seen.entry(name.clone()).or_insert(0);
        *count += 1;
        if *count == 1 {
            continue;
        }
        let stem = std::path::Path::new(&modules[i].file)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("dup")
            .to_string();
        let mut candidate = format!("{name}@{stem}");
        let mut n = 2;
        while seen.contains_key(&candidate) {
            candidate = format!("{name}@{stem}@{n}");
            n += 1;
        }
        seen.insert(candidate.clone(), 1);
        renames.push((name.clone(), candidate.clone(), modules[i].file.clone()));
        // Display name changes; resolution identity must not.
        modules[i].resolve_name = name;
        modules[i].name = candidate;
    }
    for (from, to, file) in &renames {
        eprintln!("ctx: module name collision — {file} renamed '{from}' -> '{to}'");
    }
}

/// Assets, data and lockfiles: absent from a code topology by design, so they
/// are not blind spots and must not pad the coverage report. Everything NOT
/// listed here that ctx cannot parse IS reported, so a new language shows up
/// as a blind spot automatically instead of waiting to be allowlisted.
const NON_SOURCE_EXTS: &[&str] = &[
    // images / fonts / media
    "png", "jpg", "jpeg", "gif", "svg", "ico", "webp", "bmp", "pdf", "mp4", "mp3", "wav", "ttf",
    "otf", "woff", "woff2",
    // archives / binaries / model weights
    "zip", "gz", "tgz", "bz2", "xz", "zst", "tar", "bin", "so", "dylib", "dll", "exe", "a", "o",
    "pyc", "pyd", "whl", "onnx", "pt", "pth", "safetensors", "gguf", "ggml", "npy", "npz", "pkl",
    // data / config / text
    "json", "jsonl", "csv", "tsv", "parquet", "db", "sqlite", "lock", "log", "txt", "toml", "yaml",
    "yml", "ini", "cfg", "env", "gbnf", "gitignore", "gitattributes",
    // backups / artifacts / keys: present in a tree, but not language blind spots
    "bak", "backup", "archive", "orig", "rej", "tmp", "swp", "pub", "pem", "key",
    "pin", "patch", "manifest", "tpl", "lockb",
];

/// Count unmodeled source files by extension under `root`, honoring the same
/// directory-skip rules as the graph walk. Returns (ext, count), descending.
pub fn unsupported_census(root: &Path) -> Vec<(String, usize)> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    // Count hidden-but-supported files too: the walker skips dotted paths by
    // default, so `.hooks/deploy.py` is real source that silently never enters
    // the graph. For a report whose entire job is honest disclosure, an
    // undisclosed omission is the one unacceptable answer.
    let mut hidden_supported = 0usize;
    for entry in walker(root).hidden(false).build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if path
            .components()
            .any(|c| SKIP_DIRS.contains(&c.as_os_str().to_str().unwrap_or("")))
        {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(path);
        let is_hidden = rel
            .components()
            .any(|c| c.as_os_str().to_str().is_some_and(|s| s.starts_with('.')));
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        let supported = matches!(ext, "rs" | "py" | "ts" | "tsx" | "md" | "markdown");
        if supported {
            if is_hidden {
                hidden_supported += 1;
            }
            continue;
        }
        if is_hidden {
            // Hidden non-source (lockfiles, CI config) is not a blind spot.
            continue;
        }
        // Report every unparsed extension EXCEPT known non-source, rather than
        // an allowlist of known source: the old allowlist had no `.ipynb`,
        // `.sh`, `.pyi`, `.proto` or `.tf`, so a repo full of notebooks
        // reported "none". Inverting it keeps new languages honest by default;
        // the denylist only suppresses assets, data and lockfiles, which are
        // not blind spots in a code topology.
        if NON_SOURCE_EXTS.contains(&ext) {
            continue;
        }
        *counts.entry(ext.to_string()).or_default() += 1;
    }
    let mut v: Vec<(String, usize)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    if hidden_supported > 0 {
        v.push((
            format!("(hidden paths, {hidden_supported} supported file(s) not scanned)"),
            hidden_supported,
        ));
    }
    v
}

/// Derive a stable module name from the file path. "src" components are
/// dropped; components before the first "src" become the crate prefix
/// (workspace layouts like crates/foo/src/bar.rs -> crates::foo::bar).
fn module_name(rel: &Path, lang: Lang) -> (String, Vec<String>) {
    let comps: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
    let mut segs: Vec<String> = Vec::new();
    let mut crate_prefix: Vec<String> = Vec::new();
    let mut seen_src = false;

    for (i, c) in comps.iter().enumerate() {
        let last = i == comps.len() - 1;
        if !last {
            if *c == "src" {
                if !seen_src {
                    crate_prefix = segs.clone();
                    seen_src = true;
                }
                continue;
            }
            segs.push((*c).to_string());
            continue;
        }
        let stem = c.rsplit_once('.').map(|(s, _)| s).unwrap_or(c);
        let drop = match lang {
            Lang::Rust => matches!(stem, "mod" | "lib" | "main"),
            Lang::Python => stem == "__init__",
            Lang::TypeScript => stem == "index",
            Lang::Markdown => stem.eq_ignore_ascii_case("readme") || stem.eq_ignore_ascii_case("index"),
        };
        if !drop {
            segs.push(stem.to_string());
        }
    }

    let name = if segs.is_empty() {
        match lang {
            Lang::Rust => "crate".to_string(),
            Lang::Python | Lang::TypeScript | Lang::Markdown => "root".to_string(),
        }
    } else {
        segs.join(sep(lang))
    };
    (name, crate_prefix)
}

fn sep(lang: Lang) -> &'static str {
    match lang {
        Lang::Rust => "::",
        Lang::Python | Lang::TypeScript | Lang::Markdown => ".",
    }
}

struct Ctx<'a> {
    modules: &'a [Module],
    /// Module name segments sorted longest-first so the most specific
    /// prefix wins.
    index: Vec<(Vec<String>, String)>,
    by_name: HashMap<String, usize>,
    /// Canonical scan root, for on-disk checks (markdown link targets).
    root: &'a Path,
}

enum Resolved {
    Internal(String),
    External(String),
    Ignore,
}

fn resolve_deps(modules: &mut [Module], root: &Path) {
    let mut index: Vec<(Vec<String>, String)> = modules
        .iter()
        .map(|m| (m.resolve_segs(), m.name.clone()))
        .collect();
    index.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.1.cmp(&b.1)));
    let by_name: HashMap<String, usize> = modules
        .iter()
        .enumerate()
        .map(|(i, m)| (m.name.clone(), i))
        .collect();

    type ModuleResult = (
        BTreeSet<String>,
        BTreeSet<String>,
        Vec<String>,
        Vec<Vec<Call>>,
        Vec<String>,
        Diagnostics,
    );
    let results: Vec<ModuleResult> = {
        let ctx = Ctx {
            modules: &*modules,
            index,
            by_name,
            root,
        };
        let method_index = build_method_index(ctx.modules);
        ctx.modules
            .iter()
            .map(|m| {
                let mut deps = BTreeSet::new();
                let mut ext = BTreeSet::new();
                for imp in &m.raw_imports {
                    match resolve_from(imp, m, &ctx) {
                        Resolved::Internal(n) if n != m.name => {
                            deps.insert(n);
                        }
                        Resolved::External(n) => {
                            ext.insert(n);
                        }
                        _ => {}
                    }
                }
                let reex = m
                    .raw_reexports
                    .iter()
                    .filter(|b| b.public)
                    .map(|b| display_reexport(b, m, &ctx))
                    .collect();
                let (calls_per_item, call_deps, soft_deps, diag) =
                    compute_calls(m, &ctx, &method_index);
                // An import-derived dep is hard evidence; only a dep that exists
                // SOLELY via receiver inference stays marked soft.
                let soft: Vec<String> = soft_deps
                    .into_iter()
                    .filter(|d| d != &m.name && !deps.contains(d))
                    .collect();
                deps.extend(call_deps.into_iter().filter(|d| d != &m.name));
                (deps, ext, reex, calls_per_item, soft, diag)
            })
            .collect()
    };

    for (m, (deps, ext, reex, calls, soft, diag)) in modules.iter_mut().zip(results) {
        m.deps = deps.into_iter().collect();
        m.extern_deps = ext.into_iter().collect();
        m.reexports = reex;
        m.heuristic_deps = soft;
        m.diag = diag;
        let mut it = calls.into_iter();
        apply_calls(&mut m.items, &mut it);
    }
}

/// Normalize an import path written inside module `m` into absolute segment
/// candidates. `true` means the path is explicitly internal (crate-relative
/// or dot-relative) and must never fall back to an external edge.
fn candidates(imp: &str, m: &Module) -> (Vec<Vec<String>>, bool) {
    match m.lang {
        Lang::Rust => {
            let raw: Vec<String> = imp
                .split("::")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if raw.is_empty() {
                return (Vec::new(), true);
            }
            let cur = m.resolve_segs();
            match raw[0].as_str() {
                "crate" => (
                    vec![[m.crate_prefix.clone(), raw[1..].to_vec()].concat()],
                    true,
                ),
                "self" => (vec![[cur, raw[1..].to_vec()].concat()], true),
                "super" => {
                    let n = raw.iter().take_while(|s| s.as_str() == "super").count();
                    let base = cur[..cur.len().saturating_sub(n)].to_vec();
                    (vec![[base, raw[n..].to_vec()].concat()], true)
                }
                _ => (vec![raw], false),
            }
        }
        Lang::TypeScript | Lang::Markdown => {
            if imp.starts_with('.') {
                // Relative specifier `./a/b`, `../a`: resolve against the
                // importing module's directory, one climb per `..`.
                let parts: Vec<&str> = imp.split('/').collect();
                let mut climb = 0usize;
                let mut i = 0;
                while i < parts.len() && matches!(parts[i], "." | "..") {
                    if parts[i] == ".." {
                        climb += 1;
                    }
                    i += 1;
                }
                let rest: Vec<String> = parts[i..]
                    .iter()
                    .flat_map(|p| p.split('.'))
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                let cur = m.resolve_segs();
                let dir_len = if m.is_package {
                    cur.len()
                } else {
                    cur.len().saturating_sub(1)
                };
                let base = cur[..dir_len.saturating_sub(climb)].to_vec();
                (vec![[base, rest].concat()], true)
            } else {
                // Bare/alias/member path (`react`, `a.b.c`): may be external.
                let segs: Vec<String> = imp
                    .split(['/', '.'])
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if segs.is_empty() {
                    return (Vec::new(), false);
                }
                let mut cands = vec![segs.clone()];
                if segs.len() > 1 {
                    cands.push(segs[1..].to_vec());
                }
                (cands, false)
            }
        }
        Lang::Python => {
            let dots = imp.chars().take_while(|c| *c == '.').count();
            let rest: Vec<String> = imp[dots..]
                .split('.')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if dots > 0 {
                // One dot means the enclosing package: the module itself for
                // an __init__.py, its parent otherwise. Each extra dot climbs
                // one more level.
                let cur = m.resolve_segs();
                let pkg_len = if m.is_package {
                    cur.len()
                } else {
                    cur.len().saturating_sub(1)
                };
                let base = cur[..pkg_len.saturating_sub(dots - 1)].to_vec();
                (vec![[base, rest].concat()], true)
            } else {
                let mut cands = vec![rest.clone()];
                // Scanning inside the package root: absolute "pkg.core.utils"
                // still resolves once the leading package name is dropped.
                if rest.len() > 1 {
                    cands.push(rest[1..].to_vec());
                }
                (cands, false)
            }
        }
    }
}

fn match_prefix(abs: &[String], index: &[(Vec<String>, String)]) -> Option<(String, usize)> {
    for (segs, name) in index {
        if !segs.is_empty() && segs.len() <= abs.len() && abs[..segs.len()] == segs[..] {
            return Some((name.clone(), segs.len()));
        }
    }
    None
}

/// Resolve a path written inside `m` to (module, remaining symbol segments),
/// without chasing or external fallback.
fn resolve_path(path: &str, m: &Module, ctx: &Ctx) -> Option<(String, Vec<String>)> {
    let (cands, _) = candidates(path, m);
    for c in &cands {
        if c.is_empty() {
            continue;
        }
        if let Some((name, len)) = match_prefix(c, &ctx.index) {
            return Some((name, c[len..].to_vec()));
        }
    }
    None
}

fn resolve_from(imp: &str, m: &Module, ctx: &Ctx) -> Resolved {
    let (cands, explicit_internal) = candidates(imp, m);
    for c in &cands {
        if c.is_empty() {
            continue;
        }
        if let Some((name, len)) = match_prefix(c, &ctx.index) {
            return Resolved::Internal(chase(&name, &c[len..], ctx, CHASE_DEPTH).0);
        }
    }
    if explicit_internal {
        return Resolved::Ignore;
    }
    match cands.first().and_then(|c| c.first()) {
        None => Resolved::Ignore,
        Some(f) if m.lang == Lang::Rust && matches!(f.as_str(), "std" | "core" | "alloc") => {
            Resolved::Ignore
        }
        Some(f) => Resolved::External(f.clone()),
    }
}

/// Can `sym` be imported through module `m` by an outside module?
/// Rust: only `pub use` bindings re-export. Python: any module-level
/// binding is importable through the module.
fn chaseable<'x>(m: &'x Module, sym: &str) -> Option<&'x Binding> {
    m.raw_reexports
        .iter()
        .find(|b| b.name == sym && (b.public || m.lang == Lang::Python))
}

/// Follow re-export facades: an import that lands on module `name` with
/// leftover symbol segments is chased through that module's bindings until
/// it reaches the module that actually defines the symbol. Returns the final
/// module plus whatever symbol segments remain relative to it.
fn chase(name: &str, rest: &[String], ctx: &Ctx, depth: usize) -> (String, Vec<String>) {
    if depth == 0 || rest.is_empty() {
        return (name.to_string(), rest.to_vec());
    }
    let Some(&mi) = ctx.by_name.get(name) else {
        return (name.to_string(), rest.to_vec());
    };
    let m = &ctx.modules[mi];
    let sym = &rest[0];

    // The leftover segment may itself be a submodule file (routed through
    // a directory module like foo/mod.rs or a package __init__.py).
    let child = format!("{}{}{}", name, sep(m.lang), sym);
    if ctx.by_name.contains_key(&child) {
        return chase(&child, &rest[1..], ctx, depth - 1);
    }
    if m.defined_names.contains(sym.as_str()) {
        return (name.to_string(), rest.to_vec());
    }
    if let Some(b) = chaseable(m, sym) {
        if let Some((n2, mut r2)) = resolve_path(&b.path, m, ctx) {
            r2.extend_from_slice(&rest[1..]);
            return chase(&n2, &r2, ctx, depth - 1);
        }
        return (name.to_string(), rest.to_vec());
    }
    for g in m
        .raw_reexports
        .iter()
        .filter(|b| b.name == "*" && (b.public || m.lang == Lang::Python))
    {
        if let Some((n2, r2)) = resolve_path(&g.path, m, ctx) {
            if !r2.is_empty() {
                continue;
            }
            if let Some(&ni) = ctx.by_name.get(&n2) {
                if ni != mi && provides(ni, sym, ctx, depth - 1) {
                    return chase(&n2, rest, ctx, depth - 1);
                }
            }
        }
    }
    (name.to_string(), rest.to_vec())
}

/// Does module `mi` make `sym` importable (defined locally, a submodule,
/// a direct binding, or through a glob re-export)?
fn provides(mi: usize, sym: &str, ctx: &Ctx, depth: usize) -> bool {
    let m = &ctx.modules[mi];
    if m.defined_names.contains(sym) {
        return true;
    }
    if ctx
        .by_name
        .contains_key(&format!("{}{}{}", m.name, sep(m.lang), sym))
    {
        return true;
    }
    if depth == 0 {
        return false;
    }
    for b in m
        .raw_reexports
        .iter()
        .filter(|b| b.public || m.lang == Lang::Python)
    {
        if b.name == sym {
            return true;
        }
        if b.name == "*" {
            if let Some((n2, r2)) = resolve_path(&b.path, m, ctx) {
                if r2.is_empty() {
                    if let Some(&ni) = ctx.by_name.get(&n2) {
                        if ni != mi && provides(ni, sym, ctx, depth - 1) {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// Receiver-based method calls are resolved via a global method index only
/// when the name is unique codebase-wide AND not a ubiquitous std method —
/// otherwise a single local `fn push` would swallow every `vec.push()`.
/// Is this a method name whose reverse edges are deliberately not indexed?
///
/// `callers` needs this to answer honestly: a suppressed name has NO reverse
/// edges by design, so an empty result says nothing about whether callers exist.
pub fn is_suppressed_method_name(name: &str) -> bool {
    STD_METHODS.contains(&name)
}

const STD_METHODS: &[&str] = &[
    "new", "default", "clone", "into", "from", "as_ref", "as_mut", "as_str", "to_string",
    "to_owned", "to_vec", "into_iter", "iter", "iter_mut", "next", "collect", "map", "filter",
    "fold", "for_each", "any", "all", "find", "position", "push", "pop", "insert", "remove",
    "get", "get_mut", "contains", "contains_key", "len", "is_empty", "clear", "extend",
    "append", "join", "split", "trim", "parse", "unwrap", "unwrap_or", "unwrap_or_else",
    "unwrap_or_default", "expect", "ok", "err", "is_some", "is_none", "is_ok", "is_err",
    "and_then", "or_else", "take", "replace", "send", "recv", "lock", "read", "write",
    "flush", "close", "open", "format", "items", "keys", "values", "update", "add", "copy",
    "sort", "sort_by", "reverse", "index", "count", "startswith", "endswith", "strip",
    "lstrip", "rstrip", "lower", "upper", "encode", "decode", "min", "max", "sum", "abs",
    "enumerate", "zip", "first", "last", "entry", "or_insert", "or_default", "chars",
    "bytes", "lines", "clamp", "floor", "ceil", "round", "sqrt", "powi", "powf", "exp",
    "ln", "cos", "sin", "tan", "get_or_insert_with", "retain", "drain", "chunks", "windows",
];

/// method name -> set of (module, container type/class) defining it.
type MethodIndex = HashMap<String, BTreeSet<(String, String)>>;

fn build_method_index(modules: &[Module]) -> MethodIndex {
    fn rec(items: &[Item], module: &str, idx: &mut MethodIndex) {
        for it in items {
            if matches!(it.kind.as_str(), "impl" | "trait" | "class") {
                if let Some(cname) = &it.name {
                    for ch in &it.children {
                        if matches!(ch.kind.as_str(), "fn" | "def") {
                            if let Some(n) = &ch.name {
                                idx.entry(n.clone())
                                    .or_default()
                                    .insert((module.to_string(), cname.clone()));
                            }
                        }
                    }
                }
            }
            rec(&it.children, module, idx);
        }
    }
    let mut idx = MethodIndex::new();
    for m in modules {
        rec(&m.items, &m.name, &mut idx);
    }
    idx
}

fn container_has_method(m: &Module, container: &str, name: &str) -> bool {
    fn rec(items: &[Item], container: &str, name: &str) -> bool {
        items.iter().any(|it| {
            (matches!(it.kind.as_str(), "impl" | "trait" | "class")
                && it.name.as_deref() == Some(container)
                && it
                    .children
                    .iter()
                    .any(|ch| ch.name.as_deref() == Some(name)))
                || rec(&it.children, container, name)
        })
    }
    rec(&m.items, container, name)
}

/// Resolve one call site to (display string, dep module). Same-module edges
/// display without the module prefix and carry no dep. Unresolvable or
/// ambiguous calls return None — never guessed.
/// The outcome of resolving one call site — richer than an Option so the
/// coverage report can separate std/builtin calls from genuine misses.
enum Resolution {
    Edge {
        display: String,
        dep: Option<String>,
        heuristic: bool,
    },
    /// A ubiquitous std/builtin method — intentionally not edged.
    StdBuiltin,
    /// Could not be resolved to an internal edge (ambiguous, external, or
    /// unknown receiver).
    Drop,
}

/// Wrap `finish_call`'s Option into a Resolution.
fn finished(fm: &str, fr: &[String], m: &Module, ctx: &Ctx) -> Resolution {
    match finish_call(fm, fr, m, ctx) {
        Some((display, dep, heuristic)) => Resolution::Edge {
            display,
            dep,
            heuristic,
        },
        None => Resolution::Drop,
    }
}

fn resolve_call(
    rc: &RawCall,
    container: Option<&str>,
    m: &Module,
    ctx: &Ctx,
    method_index: &MethodIndex,
) -> Resolution {
    let s = sep(m.lang);

    // Markdown "calls" are links; resolve them as doc/heading references.
    if m.lang == Lang::Markdown {
        return resolve_md_link(&rc.path, m, ctx);
    }

    match rc.recv {
        // `self.f()` / `Self::f()`: the enclosing container is the correct
        // owner, so an enclosing-impl hit is trustworthy.
        Receiver::SelfType => {
            return method_edge(&rc.path, container, m, ctx, method_index, false)
        }
        // `expr.f()`: receiver type unknown — any attribution is a guess.
        Receiver::Unknown => {
            return method_edge(&rc.path, container, m, ctx, method_index, true)
        }
        Receiver::Free => {}
    }

    let segs: Vec<&str> = rc.path.split(s).map(str::trim).filter(|x| !x.is_empty()).collect();
    match segs.len() {
        0 => Resolution::Drop,
        1 => {
            let name = segs[0];
            if m.defined_names.contains(name) {
                return Resolution::Edge {
                    display: name.to_string(),
                    dep: None,
                    heuristic: false,
                };
            }
            // `use crate::core::build; build()` / `from x import f; f()`
            let Some(b) = m.raw_reexports.iter().find(|b| b.name == name) else {
                return Resolution::Drop;
            };
            let Some((n2, r2)) = resolve_path(&b.path, m, ctx) else {
                return Resolution::Drop;
            };
            let (fm, fr) = chase(&n2, &r2, ctx, CHASE_DEPTH);
            finished(&fm, &fr, m, ctx)
        }
        _ => {
            // Type-qualified local call: Engine::new() with Engine defined here.
            if m.defined_names.contains(segs[0]) {
                return Resolution::Edge {
                    display: segs.join(s),
                    dep: None,
                    // In Rust `Engine::new()` really is type-qualified. In
                    // Python/TS the same shape is `receiver.method()`, and
                    // `defined_names` holds functions and imports too — so a
                    // local variable that happens to share a module-level name
                    // (a pytest fixture called `router`) produced a confident,
                    // `~`-free edge naming nothing in the graph. Keep the signal,
                    // but stop asserting it.
                    heuristic: m.lang != Lang::Rust,
                };
            }
            // First segment bound by use/import: helpers::go(), np-style aliases.
            if let Some(b) = m.raw_reexports.iter().find(|b| b.name == segs[0]) {
                let full = format!("{}{s}{}", b.path, segs[1..].join(s));
                let Some((n2, r2)) = resolve_path(&full, m, ctx) else {
                    return Resolution::Drop;
                };
                let (fm, fr) = chase(&n2, &r2, ctx, CHASE_DEPTH);
                return finished(&fm, &fr, m, ctx);
            }
            // Direct path: crate::core::build(), pkg.utils.helper().
            if let Some((n2, r2)) = resolve_path(&rc.path, m, ctx) {
                let (fm, fr) = chase(&n2, &r2, ctx, CHASE_DEPTH);
                return finished(&fm, &fr, m, ctx);
            }
            // Python `obj.method()`: the receiver is opaque, but the trailing
            // name may still resolve like a method call (heuristic).
            if m.lang == Lang::Python {
                if let Some(last) = segs.last() {
                    return method_edge(last, container, m, ctx, method_index, true);
                }
            }
            Resolution::Drop
        }
    }
}

/// Resolve a markdown link to a doc/heading edge, or flag it broken. An
/// external URL or a link to a non-doc asset is treated like a std/builtin
/// call (correctly not an internal edge); a relative doc link whose file or
/// heading doesn't exist becomes a Drop — i.e. a broken link.
fn resolve_md_link(link: &str, m: &Module, ctx: &Ctx) -> Resolution {
    if let Some(inner) = link.strip_prefix("[[").and_then(|s| s.strip_suffix("]]")) {
        return resolve_md_wiki(inner, m, ctx);
    }
    if is_external_url(link) {
        return Resolution::StdBuiltin;
    }
    let (file_part, frag) = match link.split_once('#') {
        Some((f, g)) => (f, Some(g)),
        None => (link, None),
    };
    // Pure anchor `#section` — a heading in this same doc.
    if file_part.is_empty() {
        return match frag {
            Some(g) => md_anchor_edge(&m.name, g, m, ctx),
            None => Resolution::Drop,
        };
    }
    let (stem, is_doc) = strip_md_ext(file_part.trim_end_matches('/'));
    match resolve_path(stem, m, ctx) {
        Some((name, rest)) if rest.is_empty() => match frag {
            None => md_module_edge(&name, m),
            Some(g) => md_anchor_edge(&name, g, m, ctx),
        },
        // A doc link that resolves to no module is broken — unless the file
        // exists on disk but simply wasn't scanned (e.g. linking out of a
        // subdir); then it's out-of-scope, not broken. Non-doc assets
        // (`.png`, `.rs`) are never doc nodes.
        _ => {
            if is_doc && !md_target_on_disk(ctx.root, &m.file, file_part) {
                Resolution::Drop
            } else {
                Resolution::StdBuiltin
            }
        }
    }
}

/// Does a relative markdown link target exist on disk, relative to the
/// linking file? Tries the path as-is, with an `.md`/`.markdown` extension,
/// and as a directory with a README/index.
fn md_target_on_disk(root: &Path, m_file: &str, file_part: &str) -> bool {
    let dir = root.join(m_file);
    let dir = dir.parent().unwrap_or(root);
    let target = dir.join(file_part);
    if target.exists() {
        return true;
    }
    for ext in ["md", "markdown"] {
        if dir.join(format!("{file_part}.{ext}")).exists() {
            return true;
        }
    }
    target.join("README.md").exists() || target.join("index.md").exists()
}

fn resolve_md_wiki(inner: &str, m: &Module, ctx: &Ctx) -> Resolution {
    let (page, frag) = inner
        .split_once('#')
        .map_or((inner, None), |(p, g)| (p, Some(g)));
    let target = markdown::slug(page);
    let hit = ctx.modules.iter().find(|mm| {
        mm.name
            .rsplit('.')
            .next()
            .is_some_and(|last| markdown::slug(last) == target)
    });
    match hit {
        Some(mm) => match frag {
            None => md_module_edge(&mm.name, m),
            Some(g) => md_anchor_edge(&mm.name, g, m, ctx),
        },
        None => Resolution::Drop,
    }
}

/// Edge to a whole document (a link with no `#fragment`).
fn md_module_edge(name: &str, m: &Module) -> Resolution {
    if name == m.name {
        Resolution::Edge {
            display: name.to_string(),
            dep: None,
            heuristic: false,
        }
    } else {
        Resolution::Edge {
            display: name.to_string(),
            dep: Some(name.to_string()),
            heuristic: false,
        }
    }
}

/// Edge to a specific heading; broken (Drop) if that heading slug is absent.
fn md_anchor_edge(name: &str, frag: &str, m: &Module, ctx: &Ctx) -> Resolution {
    let s = markdown::slug(frag);
    let Some(&ni) = ctx.by_name.get(name) else {
        return Resolution::Drop;
    };
    if !ctx.modules[ni].defined_names.contains(&s) {
        return Resolution::Drop; // file exists, heading doesn't
    }
    if name == m.name {
        Resolution::Edge {
            display: s,
            dep: None,
            heuristic: false,
        }
    } else {
        Resolution::Edge {
            display: format!("{name}.{s}"),
            dep: Some(name.to_string()),
            heuristic: false,
        }
    }
}

fn is_external_url(link: &str) -> bool {
    link.starts_with("//")
        || link.starts_with("mailto:")
        || link.starts_with("tel:")
        || link.contains("://")
}

/// Strip a markdown extension; returns (stem, looks_like_a_doc). An
/// extensionless target is treated as a doc reference (a dir/README).
fn strip_md_ext(fp: &str) -> (&str, bool) {
    for ext in [".md", ".markdown", ".mdx"] {
        if let Some(stem) = fp.strip_suffix(ext) {
            return (stem, true);
        }
    }
    let has_other_ext = fp.rsplit('/').next().is_some_and(|f| f.contains('.'));
    (fp, !has_other_ext)
}

/// Resolve a receiver-less method name: the enclosing impl/class first,
/// then the global method index if the name is unique and not a std method.
fn method_edge(
    name: &str,
    container: Option<&str>,
    m: &Module,
    ctx: &Ctx,
    method_index: &MethodIndex,
    receiver_unknown: bool,
) -> Resolution {
    let s = sep(m.lang);
    if let Some(c) = container {
        if container_has_method(m, c, name) {
            // Reliable for a self receiver; a guess for an opaque one.
            return Resolution::Edge {
                display: format!("{c}{s}{name}"),
                dep: None,
                heuristic: receiver_unknown,
            };
        }
    }
    if STD_METHODS.contains(&name) {
        return Resolution::StdBuiltin;
    }
    let Some(owners) = method_index.get(name) else {
        return Resolution::Drop;
    };
    if owners.len() != 1 {
        return Resolution::Drop;
    }
    // Resolved purely because the method name is unique codebase-wide — a
    // heuristic, since the receiver type was never confirmed.
    let (om, oc) = owners.iter().next().unwrap();
    let Some(&omi) = ctx.by_name.get(om) else {
        return Resolution::Drop;
    };
    // A unique method name is evidence only WITHIN one language. Across
    // languages it is coincidence: `tok.apply_chat_template(...)` in Python is
    // the HuggingFace tokenizer, not the Rust `NativeEngine` method that happens
    // to share the name — and attributing it invents a Python -> Rust dependency
    // that cannot exist. Those fabricated edges then drive `deps:`, `subtree`
    // upstream and `core`'s ranking, so this is not a cosmetic miss. Measured on
    // a polyglot repo: 45 of 50 `apply_chat_template` "callers", and ~18% of all
    // module dep edges, were cross-language artifacts.
    if ctx.modules[omi].lang != m.lang {
        return Resolution::Drop;
    }
    let os = sep(ctx.modules[omi].lang);
    if om == &m.name {
        Resolution::Edge {
            display: format!("{oc}{os}{name}"),
            dep: None,
            heuristic: true,
        }
    } else {
        Resolution::Edge {
            display: format!("{om}{os}{oc}{os}{name}"),
            dep: Some(om.clone()),
            heuristic: true,
        }
    }
}

fn finish_call(
    fm: &str,
    fr: &[String],
    m: &Module,
    ctx: &Ctx,
) -> Option<(String, Option<String>, bool)> {
    if fr.is_empty() {
        return None;
    }
    let target = &ctx.modules[*ctx.by_name.get(fm)?];
    if !target.defined_names.contains(&fr[0]) {
        return None;
    }
    // Backed by a resolved import/path landing on a defined symbol: trusted.
    let s = sep(target.lang);
    if fm == m.name {
        Some((fr.join(s), None, false))
    } else {
        Some((format!("{fm}{s}{}", fr.join(s)), Some(fm.to_string()), false))
    }
}

/// Resolve every call site in a module. Returns resolved call lists in
/// item DFS pre-order (applied back by apply_calls in the same order) plus
/// the modules those calls reach.
fn compute_calls(
    m: &Module,
    ctx: &Ctx,
    method_index: &MethodIndex,
) -> (Vec<Vec<Call>>, BTreeSet<String>, BTreeSet<String>, Diagnostics) {
    #[allow(clippy::too_many_arguments)]
    fn rec(
        items: &[Item],
        container: Option<&str>,
        m: &Module,
        ctx: &Ctx,
        method_index: &MethodIndex,
        per_item: &mut Vec<Vec<Call>>,
        deps: &mut BTreeSet<String>,
        soft_deps: &mut BTreeSet<String>,
        diag: &mut Diagnostics,
    ) {
        for it in items {
            // Dedup by target; if the same edge resolves both ways, the
            // trusted (non-heuristic) resolution wins.
            let mut calls: BTreeMap<String, bool> = BTreeMap::new();
            for rc in &it.raw_calls {
                diag.call_sites += 1;
                match resolve_call(rc, container, m, ctx, method_index) {
                    Resolution::Edge {
                        display,
                        dep,
                        heuristic,
                    } => {
                        diag.resolved += 1;
                        if heuristic {
                            diag.heuristic += 1;
                        }
                        calls
                            .entry(display)
                            .and_modify(|h| *h = *h && heuristic)
                            .or_insert(heuristic);
                        if let Some(d) = dep {
                            if heuristic {
                                soft_deps.insert(d.clone());
                            }
                            deps.insert(d);
                        }
                    }
                    Resolution::StdBuiltin => diag.std_builtin += 1,
                    Resolution::Drop => {
                        // For markdown, a drop is a genuine broken link.
                        if m.lang == Lang::Markdown {
                            diag.broken_links.push((it.line, rc.path.clone()));
                        }
                    }
                }
            }
            per_item.push(
                calls
                    .into_iter()
                    .map(|(to, heuristic)| Call { to, heuristic })
                    .collect(),
            );
            let next = if matches!(it.kind.as_str(), "impl" | "trait" | "class") {
                it.name.as_deref()
            } else {
                container
            };
            rec(&it.children, next, m, ctx, method_index, per_item, deps, soft_deps, diag);
        }
    }
    let mut per_item = Vec::new();
    let mut deps = BTreeSet::new();
    let mut soft_deps = BTreeSet::new();
    let mut diag = Diagnostics::default();
    rec(&m.items, None, m, ctx, method_index, &mut per_item, &mut deps, &mut soft_deps, &mut diag);
    (per_item, deps, soft_deps, diag)
}

fn apply_calls(items: &mut [Item], resolved: &mut std::vec::IntoIter<Vec<Call>>) {
    for it in items {
        it.calls = resolved.next().unwrap_or_default();
        apply_calls(&mut it.children, resolved);
    }
}

fn display_reexport(b: &Binding, m: &Module, ctx: &Ctx) -> String {
    let s = sep(m.lang);
    let target = match resolve_path(&b.path, m, ctx) {
        Some((name, rest)) if rest.is_empty() => name,
        Some((name, rest)) => format!("{name}{s}{}", rest.join(s)),
        None => b.path.clone(),
    };
    if b.name == "*" {
        format!("{target}{s}*")
    } else if target == b.name || target.ends_with(&format!("{s}{}", b.name)) {
        target
    } else {
        format!("{target} as {}", b.name)
    }
}
