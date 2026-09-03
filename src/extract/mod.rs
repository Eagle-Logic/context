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

/// The 1-based inclusive line span of a tree-sitter node.
///
/// Shared by the three tree-sitter extractors so a call site and an item span
/// are measured the same way — a call is a range, not a point, because a call
/// spanning four lines is four lines an agent has to read.
pub(crate) fn span_of(n: tree_sitter::Node) -> (usize, usize) {
    (n.start_position().row + 1, n.end_position().row + 1)
}

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

/// The source files under `root` that the graph is built from, sorted.
///
/// Split out so a cache can fingerprint exactly the set that gets parsed. A
/// staleness check that walked even slightly differently would be a
/// correctness bug — it would miss the file it did not look at — so there is
/// deliberately one definition of "what counts as a source file here".
pub fn source_files(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in walker(root).build() {
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
                lang_selected(Lang::TypeScript)
                    && !path.to_str().is_some_and(|s| s.ends_with(".d.ts"))
            }
            _ => false,
        };
        if take {
            files.push(path.to_path_buf());
        }
    }
    files.sort();
    files
}

/// A fingerprint of the source tree: path, mtime and length of every file the
/// graph would be built from.
///
/// Cheap next to parsing — a stat per file, no reads — which is what makes a
/// cache worthwhile: re-walking to check is far less work than re-parsing.
///
/// Covers modification, addition and deletion. A file whose mtime and length
/// are both unchanged is treated as unchanged; that is the same assumption
/// every build system makes, and the alternative is hashing contents, which
/// costs the reads the cache exists to avoid.
pub fn source_fingerprint(root: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for f in source_files(root) {
        f.hash(&mut h);
        if let Ok(m) = fs::metadata(&f) {
            m.len().hash(&mut h);
            if let Ok(t) = m.modified() {
                if let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) {
                    d.as_nanos().hash(&mut h);
                }
            }
        }
    }
    h.finish()
}

pub fn build_graph(root: &Path) -> Result<Graph> {
    let root = root
        .canonicalize()
        .with_context(|| format!("cannot resolve path {}", root.display()))?;

    let files = source_files(&root);

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
            import_sites: Vec::new(),
            file: slash_path(rel),
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
    "png",
    "jpg",
    "jpeg",
    "gif",
    "svg",
    "ico",
    "webp",
    "bmp",
    "pdf",
    "mp4",
    "mp3",
    "wav",
    "ttf",
    "otf",
    "woff",
    "woff2",
    // archives / binaries / model weights
    "zip",
    "gz",
    "tgz",
    "bz2",
    "xz",
    "zst",
    "tar",
    "bin",
    "so",
    "dylib",
    "dll",
    "exe",
    "a",
    "o",
    "pyc",
    "pyd",
    "whl",
    "onnx",
    "pt",
    "pth",
    "safetensors",
    "gguf",
    "ggml",
    "npy",
    "npz",
    "pkl",
    // data / config / text
    "json",
    "jsonl",
    "csv",
    "tsv",
    "parquet",
    "db",
    "sqlite",
    "lock",
    "log",
    "txt",
    "toml",
    "yaml",
    "yml",
    "ini",
    "cfg",
    "env",
    "gbnf",
    "gitignore",
    "gitattributes",
    // backups / artifacts / keys: present in a tree, but not language blind spots
    "bak",
    "backup",
    "archive",
    "orig",
    "rej",
    "tmp",
    "swp",
    "pub",
    "pem",
    "key",
    "pin",
    "patch",
    "manifest",
    "tpl",
    "lockb",
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

/// Render a relative path with `/` separators on every platform.
///
/// `Path::display()` uses the OS separator, so the same source tree would
/// print `src/model.rs` on Unix and `src\model.rs` on Windows — and
/// `move-plan` would emit a single line mixing both, because destination
/// paths are built by joining with `/`. ctx's contract is that the output is
/// a pure function of the source tree, which has to hold across platforms
/// too, so paths are normalized here at the one place they enter the graph.
///
/// Joins components rather than replacing `\`, because a backslash is a legal
/// character in a Unix filename and replacing it would corrupt the path.
fn slash_path(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
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
            Lang::Markdown => {
                stem.eq_ignore_ascii_case("readme") || stem.eq_ignore_ascii_case("index")
            }
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
        segs.join(lang.sep())
    };
    (name, crate_prefix)
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
        Vec<(String, String)>,
        Diagnostics,
    );
    let results: Vec<ModuleResult> = {
        let ctx = Ctx {
            modules: &*modules,
            index,
            by_name,
            root,
        };
        let uni = build_universe(ctx.modules);
        ctx.modules
            .iter()
            .map(|m| {
                let mut deps = BTreeSet::new();
                let mut ext = BTreeSet::new();
                let mut sites: Vec<(String, String)> = Vec::new();
                for imp in &m.raw_imports {
                    match resolve_from(imp, m, &ctx) {
                        Resolved::Internal(n) if n != m.name => {
                            sites.push((imp.clone(), n.clone()));
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
                let (calls_per_item, call_deps, soft_deps, diag) = compute_calls(m, &ctx, &uni);
                // An import-derived dep is hard evidence; only a dep that exists
                // SOLELY via receiver inference stays marked soft.
                let soft: Vec<String> = soft_deps
                    .into_iter()
                    .filter(|d| d != &m.name && !deps.contains(d))
                    .collect();
                deps.extend(call_deps.into_iter().filter(|d| d != &m.name));
                (deps, ext, reex, calls_per_item, soft, sites, diag)
            })
            .collect()
    };

    for (m, (deps, ext, reex, calls, soft, sites, diag)) in modules.iter_mut().zip(results) {
        m.deps = deps.into_iter().collect();
        m.extern_deps = ext.into_iter().collect();
        m.reexports = reex;
        m.heuristic_deps = soft;
        m.import_sites = sites;
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
    let child = format!("{}{}{}", name, Lang::sep(m.lang), sym);
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
        .contains_key(&format!("{}{}{}", m.name, Lang::sep(m.lang), sym))
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
///
/// This list suppresses *edges* only. It plays no part in classifying a call
/// site as external, which is decided by evidence (see `Universe::all_names`).
/// Is this a method name whose reverse edges are deliberately not indexed?
///
/// `callers` needs this to answer honestly: a suppressed name has NO reverse
/// edges by design, so an empty result says nothing about whether callers exist.
pub fn is_suppressed_method_name(name: &str) -> bool {
    STD_METHODS.contains(&name)
}

const STD_METHODS: &[&str] = &[
    "new",
    "default",
    "clone",
    "into",
    "from",
    "as_ref",
    "as_mut",
    "as_str",
    "to_string",
    "to_owned",
    "to_vec",
    "into_iter",
    "iter",
    "iter_mut",
    "next",
    "collect",
    "map",
    "filter",
    "fold",
    "for_each",
    "any",
    "all",
    "find",
    "position",
    "push",
    "pop",
    "insert",
    "remove",
    "get",
    "get_mut",
    "contains",
    "contains_key",
    "len",
    "is_empty",
    "clear",
    "extend",
    "append",
    "join",
    "split",
    "trim",
    "parse",
    "unwrap",
    "unwrap_or",
    "unwrap_or_else",
    "unwrap_or_default",
    "expect",
    "ok",
    "err",
    "is_some",
    "is_none",
    "is_ok",
    "is_err",
    "and_then",
    "or_else",
    "take",
    "replace",
    "send",
    "recv",
    "lock",
    "read",
    "write",
    "flush",
    "close",
    "open",
    "format",
    "items",
    "keys",
    "values",
    "update",
    "add",
    "copy",
    "sort",
    "sort_by",
    "reverse",
    "index",
    "count",
    "startswith",
    "endswith",
    "strip",
    "lstrip",
    "rstrip",
    "lower",
    "upper",
    "encode",
    "decode",
    "min",
    "max",
    "sum",
    "abs",
    "enumerate",
    "zip",
    "first",
    "last",
    "entry",
    "or_insert",
    "or_default",
    "chars",
    "bytes",
    "lines",
    "clamp",
    "floor",
    "ceil",
    "round",
    "sqrt",
    "powi",
    "powf",
    "exp",
    "ln",
    "cos",
    "sin",
    "tan",
    "get_or_insert_with",
    "retain",
    "drain",
    "chunks",
    "windows",
];

/// Free-call names that always belong to the language, not to the repo:
/// enum constructors and macro-ish builtins. Like `STD_METHODS` these are
/// classified external on a resolution failure, so the miss census stays
/// readable — an `Err(..)` is never a call into your code.
const STD_FREE: &[&str] = &[
    "Ok",
    "Err",
    "Some",
    "None",
    "format",
    "vec",
    "print",
    "println",
    "eprint",
    "eprintln",
    "panic",
    "assert",
    "assert_eq",
    "assert_ne",
    "debug_assert",
    "matches",
    "todo",
    "unimplemented",
    "unreachable",
    "dbg",
    "int",
    "str",
    "list",
    "dict",
    "set",
    "tuple",
    "bool",
    "float",
    "range",
    "super",
    "isinstance",
    "hasattr",
    "getattr",
    "setattr",
    "String",
    "Vec",
    "Box",
    "Rc",
    "Arc",
];

/// method name -> set of (module, container type/class) defining it.
type MethodIndex = HashMap<String, BTreeSet<(String, String)>>;

/// A dynamic-dispatch call fans out over implementations. Past this many the
/// edge list stops being information and starts being noise, so it collapses
/// to the abstraction itself.
const DISPATCH_FANOUT: usize = 12;

/// Whole-tree symbol evidence, built once and shared by every module's
/// resolution pass.
struct Universe {
    /// method name -> the (module, container) pairs defining it.
    methods: MethodIndex,
    /// Every name defined anywhere under this root — module-level symbols,
    /// types, methods, nested functions. A callee name absent from this set
    /// is *provably* external: no internal edge could exist, whatever ctx
    /// does. This is what separates "not our code" from "we missed it".
    all_names: BTreeSet<String>,
    /// Every segment appearing in a module name, so a leading path segment can
    /// be recognized as an internal module reference.
    module_segs: BTreeSet<String>,
    /// trait / interface / base-class name -> the (module, type) pairs
    /// implementing it. The raw material for dispatch fan-out.
    implementors: HashMap<String, BTreeSet<(String, String)>>,
    /// type name -> its declared field/property types, so `self.field.m()`
    /// can be resolved against the field's type.
    fields: HashMap<String, BTreeMap<String, String>>,
}

fn build_universe(modules: &[Module]) -> Universe {
    fn rec(
        items: &[Item],
        module: &str,
        methods: &mut MethodIndex,
        all: &mut BTreeSet<String>,
        implementors: &mut HashMap<String, BTreeSet<(String, String)>>,
        fields: &mut HashMap<String, BTreeMap<String, String>>,
    ) {
        for it in items {
            if let Some(n) = &it.name {
                all.insert(n.clone());
                if !it.field_types.is_empty() {
                    fields
                        .entry(n.clone())
                        .or_default()
                        .extend(it.field_types.clone());
                }
            }
            if matches!(it.kind.as_str(), "impl" | "trait" | "class" | "interface") {
                if let Some(cname) = &it.name {
                    for ch in &it.children {
                        if matches!(ch.kind.as_str(), "fn" | "def") {
                            if let Some(n) = &ch.name {
                                methods
                                    .entry(n.clone())
                                    .or_default()
                                    .insert((module.to_string(), cname.clone()));
                            }
                        }
                    }
                    for t in &it.implements {
                        implementors
                            .entry(t.clone())
                            .or_default()
                            .insert((module.to_string(), cname.clone()));
                    }
                    // A trait/interface is its own fallback implementor, so a
                    // default method body is reachable when no impl overrides it.
                    if matches!(it.kind.as_str(), "trait" | "interface") {
                        implementors
                            .entry(cname.clone())
                            .or_default()
                            .insert((module.to_string(), cname.clone()));
                    }
                }
            }
            rec(&it.children, module, methods, all, implementors, fields);
        }
    }
    let mut u = Universe {
        methods: MethodIndex::new(),
        all_names: BTreeSet::new(),
        module_segs: BTreeSet::new(),
        implementors: HashMap::new(),
        fields: HashMap::new(),
    };
    for m in modules {
        u.all_names.extend(m.defined_names.iter().cloned());
        u.module_segs.extend(m.resolve_segs());
        rec(
            &m.items,
            &m.name,
            &mut u.methods,
            &mut u.all_names,
            &mut u.implementors,
            &mut u.fields,
        );
    }
    u
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

/// One outgoing edge produced by a call site. Same-module edges display
/// without the module prefix and carry no dep.
struct EdgeOut {
    display: String,
    dep: Option<String>,
    heuristic: bool,
    dispatch: bool,
}

impl EdgeOut {
    fn plain(display: String, dep: Option<String>) -> Self {
        EdgeOut {
            display,
            dep,
            heuristic: false,
            dispatch: false,
        }
    }
    fn guess(display: String, dep: Option<String>) -> Self {
        EdgeOut {
            display,
            dep,
            heuristic: true,
            dispatch: false,
        }
    }
}

/// The outcome of resolving one call site. The two non-edge outcomes are kept
/// apart deliberately: `External` is proof that no internal edge exists,
/// `Unresolved` is an admission that one might and ctx could not find it.
/// Collapsing them is what made the old coverage number unreadable.
enum Resolution {
    /// One or more edges. More than one means dynamic dispatch fan-out.
    Edges(Vec<EdgeOut>),
    /// The callee name is defined nowhere under this root: std, a builtin, or
    /// an external crate/package. Never a blind spot.
    External(String),
    /// The callee name IS defined under this root, but ctx could not pin which
    /// definition. A genuine miss — the number worth driving down.
    Unresolved(String),
}

impl Resolution {
    fn one(e: EdgeOut) -> Self {
        Resolution::Edges(vec![e])
    }
}

/// Classify a call site ctx could not edge, using whole-tree evidence rather
/// than a hardcoded list: if the name is defined nowhere here, the miss is
/// provably external.
fn miss(name: &str, uni: &Universe) -> Resolution {
    // A ubiquitous std name that failed to resolve is std, not a miss —
    // whatever internal symbol happens to share the spelling. Classifying
    // these as misses buries the real ones under `.push()` and `Err(..)`.
    if STD_METHODS.contains(&name) || STD_FREE.contains(&name) {
        return Resolution::External(name.to_string());
    }
    if uni.all_names.contains(name) {
        Resolution::Unresolved(name.to_string())
    } else {
        Resolution::External(name.to_string())
    }
}

/// Wrap `finish_call`'s Option into a Resolution, classifying a failure by the
/// symbol the path was trying to reach.
fn finished(fm: &str, fr: &[String], m: &Module, ctx: &Ctx, uni: &Universe) -> Resolution {
    match finish_call(fm, fr, m, ctx) {
        Some((display, dep)) => Resolution::one(EdgeOut::plain(display, dep)),
        None => miss(fr.last().map(String::as_str).unwrap_or(""), uni),
    }
}

fn resolve_call(
    rc: &RawCall,
    container: Option<&str>,
    m: &Module,
    ctx: &Ctx,
    uni: &Universe,
    locals: &BTreeMap<String, String>,
) -> Resolution {
    let s = Lang::sep(m.lang);

    // Markdown "calls" are links; resolve them as doc/heading references.
    if m.lang == Lang::Markdown {
        return resolve_md_link(&rc.path, m, ctx);
    }

    match &rc.recv {
        // `self.f()` / `Self::f()`: the enclosing container is the correct
        // owner, so an enclosing-impl hit is trustworthy.
        Receiver::SelfType => {
            return method_edge(&rc.path, container, m, ctx, uni, false);
        }
        // `self.field.f()`: look the field's declared type up on the
        // enclosing type, then resolve as if it were written out.
        Receiver::SelfField(field) => {
            return match container
                .and_then(|c| uni.fields.get(c))
                .and_then(|f| f.get(field))
            {
                Some(ty) => match field_receiver(ty, uni) {
                    Receiver::Typed(t) => typed_edge(&rc.path, &t, m, ctx, uni),
                    Receiver::Dyn(t) => dispatch_edge(&rc.path, &t, m, uni),
                    _ => method_edge(&rc.path, container, m, ctx, uni, true),
                },
                None => method_edge(&rc.path, container, m, ctx, uni, true),
            };
        }
        // The receiver's type is written in the source (parameter annotation,
        // `let` binding, or field declaration) — resolve against that type.
        Receiver::Typed(t) => return typed_edge(&rc.path, t, m, ctx, uni),
        // The receiver is a trait object / interface / bounded generic: the
        // call reaches every implementation.
        Receiver::Dyn(t) => return dispatch_edge(&rc.path, t, m, uni),
        // `expr.f()`: receiver type unknown — any attribution is a guess.
        Receiver::Unknown => {
            return method_edge(&rc.path, container, m, ctx, uni, true);
        }
        Receiver::Free => {}
    }

    let segs: Vec<&str> = rc
        .path
        .split(s)
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .collect();
    match segs.len() {
        0 => Resolution::External(String::new()),
        1 => {
            let name = segs[0];
            if m.defined_names.contains(name) {
                return Resolution::one(EdgeOut::plain(name.to_string(), None));
            }
            // A function nested in the enclosing function body: lexically
            // scoped, so this is the only thing the name can mean.
            if let Some(display) = locals.get(name) {
                return Resolution::one(EdgeOut::plain(display.clone(), None));
            }
            // `use crate::core::build; build()` / `from x import f; f()`
            let Some(b) = m.raw_reexports.iter().find(|b| b.name == name) else {
                return miss(name, uni);
            };
            let Some((n2, r2)) = resolve_path(&b.path, m, ctx) else {
                return miss(name, uni);
            };
            let (fm, fr) = chase(&n2, &r2, ctx, CHASE_DEPTH);
            finished(&fm, &fr, m, ctx, uni)
        }
        _ => {
            let last = *segs.last().unwrap();
            // Type-qualified local call: Engine::new() with Engine defined here.
            if m.defined_names.contains(segs[0]) {
                // In Rust `Engine::new()` really is type-qualified. In
                // Python/TS the same shape is `receiver.method()`, and
                // `defined_names` holds functions and imports too — so a
                // local variable that happens to share a module-level name
                // (a pytest fixture called `router`) produced a confident,
                // `~`-free edge naming nothing in the graph. Keep the signal,
                // but stop asserting it.
                let d = segs.join(s);
                return Resolution::one(if m.lang == Lang::Rust {
                    EdgeOut::plain(d, None)
                } else {
                    EdgeOut::guess(d, None)
                });
            }
            // First segment bound by use/import: helpers::go(), np-style aliases.
            if let Some(b) = m.raw_reexports.iter().find(|b| b.name == segs[0]) {
                let full = format!("{}{s}{}", b.path, segs[1..].join(s));
                let Some((n2, r2)) = resolve_path(&full, m, ctx) else {
                    return miss(last, uni);
                };
                let (fm, fr) = chase(&n2, &r2, ctx, CHASE_DEPTH);
                return finished(&fm, &fr, m, ctx, uni);
            }
            // Direct path: crate::core::build(), pkg.utils.helper().
            if let Some((n2, r2)) = resolve_path(&rc.path, m, ctx) {
                let (fm, fr) = chase(&n2, &r2, ctx, CHASE_DEPTH);
                return finished(&fm, &fr, m, ctx, uni);
            }
            // Python `obj.method()`: the receiver is opaque, but the trailing
            // name may still resolve like a method call (heuristic).
            if m.lang == Lang::Python {
                return method_edge(last, container, m, ctx, uni, true);
            }
            // A qualified path whose root is neither a local symbol, an
            // imported binding, nor an internal module is another crate's.
            if is_external_root(segs[0], m, uni) {
                return Resolution::External(segs[0].to_string());
            }
            miss(last, uni)
        }
    }
}

/// Reduce a declared field type to a receiver kind. Field types are stored as
/// written (`Box<dyn Sampler>`, `&'a Engine`), so the wrappers come off here.
fn field_receiver(ty: &str, uni: &Universe) -> Receiver {
    let t = ty
        .trim()
        .trim_start_matches('&')
        .trim()
        .trim_start_matches("mut ")
        .trim();
    if let Some(rest) = t.strip_prefix("dyn ") {
        return Receiver::Dyn(base_type(rest));
    }
    let base = base_type(t);
    const TRANSPARENT: &[&str] = &[
        "Box", "Arc", "Rc", "RefCell", "Cell", "Mutex", "RwLock", "Cow", "Option",
    ];
    if TRANSPARENT.contains(&base.as_str()) {
        if let (Some(i), Some(j)) = (t.find('<'), t.rfind('>')) {
            return field_receiver(&t[i + 1..j], uni);
        }
    }
    if uni.implementors.contains_key(&base) {
        return Receiver::Dyn(base);
    }
    Receiver::Typed(base)
}

/// The bare type name: generics stripped, path segments dropped.
fn base_type(t: &str) -> String {
    let t = t.split('<').next().unwrap_or(t).trim();
    t.rsplit("::").next().unwrap_or(t).trim().to_string()
}

/// Is the leading segment of a qualified call path provably not ours? A root
/// that names no internal symbol, no imported binding, and no module segment
/// belongs to an external crate/package.
fn is_external_root(root: &str, m: &Module, uni: &Universe) -> bool {
    // Explicitly internal roots, whatever else they look like.
    if matches!(root, "crate" | "self" | "super") || root.starts_with('.') {
        return false;
    }
    !uni.all_names.contains(root)
        && !uni.module_segs.contains(root)
        && !m.raw_reexports.iter().any(|b| b.name == root)
}

/// Resolve a markdown link to a doc/heading edge, or flag it broken. An
/// external URL or a link to a non-doc asset is provably external (correctly
/// not an internal edge); a relative doc link whose file or heading doesn't
/// exist is Unresolved — i.e. a broken link.
fn resolve_md_link(link: &str, m: &Module, ctx: &Ctx) -> Resolution {
    if let Some(inner) = link.strip_prefix("[[").and_then(|s| s.strip_suffix("]]")) {
        return resolve_md_wiki(inner, m, ctx);
    }
    if is_external_url(link) {
        return Resolution::External(link.to_string());
    }
    let (file_part, frag) = match link.split_once('#') {
        Some((f, g)) => (f, Some(g)),
        None => (link, None),
    };
    // Pure anchor `#section` — a heading in this same doc.
    if file_part.is_empty() {
        return match frag {
            Some(g) => md_anchor_edge(&m.name, g, m, ctx),
            None => Resolution::Unresolved(link.to_string()),
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
                Resolution::Unresolved(link.to_string())
            } else {
                Resolution::External(link.to_string())
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
        None => Resolution::Unresolved(inner.to_string()),
    }
}

/// Edge to a whole document (a link with no `#fragment`).
fn md_module_edge(name: &str, m: &Module) -> Resolution {
    let dep = (name != m.name).then(|| name.to_string());
    Resolution::one(EdgeOut::plain(name.to_string(), dep))
}

/// Edge to a specific heading; broken (Drop) if that heading slug is absent.
fn md_anchor_edge(name: &str, frag: &str, m: &Module, ctx: &Ctx) -> Resolution {
    let s = markdown::slug(frag);
    let Some(&ni) = ctx.by_name.get(name) else {
        return Resolution::Unresolved(format!("{name}#{frag}"));
    };
    if !ctx.modules[ni].defined_names.contains(&s) {
        // file exists, heading doesn't
        return Resolution::Unresolved(format!("{name}#{frag}"));
    }
    if name == m.name {
        Resolution::one(EdgeOut::plain(s, None))
    } else {
        Resolution::one(EdgeOut::plain(
            format!("{name}.{s}"),
            Some(name.to_string()),
        ))
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
    uni: &Universe,
    receiver_unknown: bool,
) -> Resolution {
    let s = Lang::sep(m.lang);
    if let Some(c) = container {
        if container_has_method(m, c, name) {
            // Reliable for a self receiver; a guess for an opaque one.
            let display = format!("{c}{s}{name}");
            return Resolution::one(if receiver_unknown {
                EdgeOut::guess(display, None)
            } else {
                EdgeOut::plain(display, None)
            });
        }
    }
    // A ubiquitous std name on a receiver ctx could not type: overwhelmingly
    // `vec.push()`, not a local `fn push`. Reporting these as misses would
    // bury the real ones under hundreds of `.collect()` calls. A receiver with
    // a known type never reaches here — `typed_edge` resolves it first, which
    // is the escape hatch for a genuine local `push`.
    if STD_METHODS.contains(&name) {
        return Resolution::External(name.to_string());
    }
    let Some(owners) = uni.methods.get(name) else {
        return miss(name, uni);
    };
    if owners.len() != 1 {
        return miss(name, uni);
    }
    // Resolved purely because the method name is unique codebase-wide — a
    // heuristic, since the receiver type was never confirmed.
    let (om, oc) = owners.iter().next().unwrap();
    match owner_edge(om, oc, name, m, ctx) {
        Some((display, dep)) => Resolution::one(EdgeOut::guess(display, dep)),
        None => miss(name, uni),
    }
}

/// Build the display/dep pair for an edge onto method `name` of container
/// `oc` defined in module `om`, as seen from module `m`.
fn owner_edge(
    om: &str,
    oc: &str,
    name: &str,
    m: &Module,
    ctx: &Ctx,
) -> Option<(String, Option<String>)> {
    let &omi = ctx.by_name.get(om)?;
    // A unique method name is evidence only WITHIN one language. Across
    // languages it is coincidence: `tok.apply_chat_template(...)` in Python is
    // the HuggingFace tokenizer, not the Rust `NativeEngine` method that happens
    // to share the name — and attributing it invents a Python -> Rust dependency
    // that cannot exist. Those fabricated edges then drive `deps:`, `subtree`
    // upstream and `core`'s ranking, so this is not a cosmetic miss. Measured on
    // a polyglot repo: 45 of 50 `apply_chat_template` "callers", and ~18% of all
    // module dep edges, were cross-language artifacts.
    if ctx.modules[omi].lang != m.lang {
        return None;
    }
    let os = Lang::sep(ctx.modules[omi].lang);
    if om == m.name {
        Some((format!("{oc}{os}{name}"), None))
    } else {
        Some((format!("{om}{os}{oc}{os}{name}"), Some(om.to_string())))
    }
}

/// `x.f()` where `x`'s type is written in the source. Unlike the unique-name
/// heuristic this consults the declared type, so a method name shared by a
/// dozen types still lands on the right one.
fn typed_edge(name: &str, ty: &str, m: &Module, ctx: &Ctx, uni: &Universe) -> Resolution {
    // The receiver's type is not defined under this root, so neither is the
    // method: `let m: BTreeMap<_,_>` then `m.entry(..)` is provably external,
    // even though some unrelated internal symbol may share the name.
    if !uni.all_names.contains(ty) {
        return Resolution::External(format!("{ty}::{name}"));
    }
    // The declared type is an abstraction others implement or subclass — a
    // Python base class, a TypeScript interface, a Rust trait named directly.
    // Which body runs depends on the value, so fan out rather than pinning the
    // base's own definition and calling it proven.
    let subtyped = uni
        .implementors
        .get(ty)
        .is_some_and(|s| s.iter().any(|(_, t)| t != ty));
    if subtyped {
        return dispatch_edge(name, ty, m, uni);
    }
    let Some(owners) = uni.methods.get(name) else {
        return miss(name, uni);
    };
    let matching: Vec<&(String, String)> = owners.iter().filter(|(_, c)| c == ty).collect();
    // Prefer an owner in this module when the type name is not unique.
    let pick = matching
        .iter()
        .find(|(om, _)| om == &m.name)
        .or_else(|| matching.first());
    let Some((om, oc)) = pick else {
        return miss(name, uni);
    };
    match owner_edge(om, oc, name, m, ctx) {
        // The receiver type is declared in source, so this is not a guess —
        // unless several distinct types share the name, which is.
        Some((display, dep)) => Resolution::one(EdgeOut {
            display,
            dep,
            heuristic: matching.len() > 1,
            dispatch: false,
        }),
        None => miss(name, uni),
    }
}

/// A call through a trait object, `impl Trait`, a bounded generic, or an
/// interface-typed value. Exactly one implementation runs, but which one is
/// not knowable statically — so ctx emits the whole possible set, marked as
/// dispatch. An over-approximation you can see beats a dropped edge.
fn dispatch_edge(name: &str, abstraction: &str, m: &Module, uni: &Universe) -> Resolution {
    if !uni.all_names.contains(abstraction) {
        return Resolution::External(format!("{abstraction}::{name}"));
    }
    let Some(impls) = uni.implementors.get(abstraction) else {
        return miss(name, uni);
    };
    let Some(owners) = uni.methods.get(name) else {
        return miss(name, uni);
    };
    let os = Lang::sep(m.lang);
    let edge_to = |om: &String, ty: &String| {
        let (display, dep) = if om == &m.name {
            (format!("{ty}{os}{name}"), None)
        } else {
            (format!("{om}{os}{ty}{os}{name}"), Some(om.clone()))
        };
        EdgeOut {
            display,
            dep,
            heuristic: false,
            dispatch: true,
        }
    };
    // Concrete implementations first. The declaring trait/interface is only a
    // target when nothing overrides the method — i.e. it is a default body,
    // the thing that actually runs, rather than a bare signature.
    let mut edges: Vec<EdgeOut> = impls
        .iter()
        .filter(|(om, ty)| ty != abstraction && owners.contains(&(om.clone(), ty.clone())))
        .map(|(om, ty)| edge_to(om, ty))
        .collect();
    if edges.is_empty() {
        edges = impls
            .iter()
            .filter(|(om, ty)| ty == abstraction && owners.contains(&(om.clone(), ty.clone())))
            .map(|(om, ty)| edge_to(om, ty))
            .collect();
    }
    if edges.is_empty() {
        return miss(name, uni);
    }
    if edges.len() > DISPATCH_FANOUT {
        // Too many branches to be readable: name the abstraction instead, and
        // say how wide the fan-out is so nobody mistakes it for a single call.
        return Resolution::one(EdgeOut {
            display: format!("{abstraction}{os}{name} [{} impls]", edges.len()),
            dep: None,
            heuristic: false,
            dispatch: true,
        });
    }
    Resolution::Edges(edges)
}

fn finish_call(fm: &str, fr: &[String], m: &Module, ctx: &Ctx) -> Option<(String, Option<String>)> {
    if fr.is_empty() {
        return None;
    }
    let target = &ctx.modules[*ctx.by_name.get(fm)?];
    if !target.defined_names.contains(&fr[0]) {
        return None;
    }
    // Backed by a resolved import/path landing on a defined symbol: trusted.
    let s = Lang::sep(target.lang);
    if fm == m.name {
        Some((fr.join(s), None))
    } else {
        Some((format!("{fm}{s}{}", fr.join(s)), Some(fm.to_string())))
    }
}

/// Resolve every call site in a module. Returns resolved call lists in
/// item DFS pre-order (applied back by apply_calls in the same order) plus
/// the modules those calls reach.
fn compute_calls(
    m: &Module,
    ctx: &Ctx,
    uni: &Universe,
) -> (
    Vec<Vec<Call>>,
    BTreeSet<String>,
    BTreeSet<String>,
    Diagnostics,
) {
    struct Walk<'a> {
        m: &'a Module,
        ctx: &'a Ctx<'a>,
        uni: &'a Universe,
        per_item: Vec<Vec<Call>>,
        deps: BTreeSet<String>,
        /// Deps that exist SOLELY via receiver inference. An import-derived dep
        /// is hard evidence; this set is what stays marked soft.
        soft_deps: BTreeSet<String>,
        diag: Diagnostics,
    }

    impl Walk<'_> {
        fn rec(
            &mut self,
            items: &[Item],
            container: Option<&str>,
            locals: &BTreeMap<String, String>,
            fn_path: Option<&str>,
        ) {
            for it in items {
                // Functions nested in this one are in scope for its body (and
                // for each other), and are the only thing their bare name can
                // refer to.
                let scope = extend_locals(locals, it, fn_path, self.m);
                // Dedup by target; if the same edge resolves both ways, the
                // trusted (non-heuristic) resolution wins. The third slot
                // accumulates every call site behind the edge, because
                // collapsing three calls to `foo()` into one edge would
                // otherwise throw away two of the three places to look.
                let mut calls: BTreeMap<String, (bool, bool, Vec<(usize, usize)>)> = BTreeMap::new();
                for rc in &it.raw_calls {
                    self.diag.call_sites += 1;
                    match resolve_call(rc, container, self.m, self.ctx, self.uni, &scope) {
                        Resolution::Edges(edges) => {
                            self.diag.resolved += 1;
                            if edges.iter().any(|e| e.heuristic) {
                                self.diag.heuristic += 1;
                            }
                            if edges.iter().any(|e| e.dispatch) {
                                self.diag.dispatch += 1;
                            }
                            for e in edges {
                                calls
                                    .entry(e.display)
                                    .and_modify(|v| {
                                        v.0 = v.0 && e.heuristic;
                                        v.1 = v.1 && e.dispatch;
                                        v.2.push((rc.line, rc.end_line));
                                    })
                                    .or_insert_with(|| {
                                        (e.heuristic, e.dispatch, vec![(rc.line, rc.end_line)])
                                    });
                                if let Some(d) = e.dep {
                                    if e.heuristic {
                                        self.soft_deps.insert(d.clone());
                                    }
                                    self.deps.insert(d);
                                }
                            }
                        }
                        Resolution::External(name) => {
                            self.diag.external += 1;
                            *self.diag.extern_names.entry(name).or_default() += 1;
                        }
                        Resolution::Unresolved(name) => {
                            self.diag.unresolved += 1;
                            *self.diag.unresolved_names.entry(name).or_default() += 1;
                            // For markdown, an unresolved link is a dead link.
                            if self.m.lang == Lang::Markdown {
                                self.diag.broken_links.push((rc.line, rc.path.clone()));
                            }
                        }
                    }
                }
                self.per_item.push(
                    calls
                        .into_iter()
                        .map(|(to, (heuristic, dispatch, mut sites))| {
                            // Determinism is load-bearing: the same tree must
                            // render the same bytes, and a dispatch fan-out can
                            // reach one target from one site more than once.
                            sites.sort_unstable();
                            sites.dedup();
                            Call {
                                to,
                                heuristic,
                                dispatch,
                                sites,
                            }
                        })
                        .collect(),
                );
                let next = if matches!(it.kind.as_str(), "impl" | "trait" | "class" | "interface") {
                    it.name.as_deref()
                } else {
                    container
                };
                let inner_fn = if matches!(it.kind.as_str(), "fn" | "def") {
                    it.name.as_deref().or(fn_path)
                } else {
                    fn_path
                };
                self.rec(&it.children, next, &scope, inner_fn);
            }
        }
    }

    let mut w = Walk {
        m,
        ctx,
        uni,
        per_item: Vec::new(),
        deps: BTreeSet::new(),
        soft_deps: BTreeSet::new(),
        diag: Diagnostics::default(),
    };
    w.rec(&m.items, None, &BTreeMap::new(), None);
    (w.per_item, w.deps, w.soft_deps, w.diag)
}

/// The lexical scope of function-local helpers visible inside `it`: whatever
/// was already in scope, plus `it`'s own nested functions, keyed by bare name
/// and mapped to a qualified display (`outer::helper`) so the edge is
/// distinguishable from a module-level function of the same name.
fn extend_locals(
    locals: &BTreeMap<String, String>,
    it: &Item,
    fn_path: Option<&str>,
    m: &Module,
) -> BTreeMap<String, String> {
    if !matches!(it.kind.as_str(), "fn" | "def") {
        return locals.clone();
    }
    let s = Lang::sep(m.lang);
    let owner = it.name.as_deref().or(fn_path);
    let mut out = locals.clone();
    for ch in &it.children {
        if !matches!(ch.kind.as_str(), "fn" | "def") {
            continue;
        }
        let Some(n) = &ch.name else { continue };
        let display = match owner {
            Some(o) => format!("{o}{s}{n}"),
            None => n.clone(),
        };
        out.insert(n.clone(), display);
    }
    out
}

fn apply_calls(items: &mut [Item], resolved: &mut std::vec::IntoIter<Vec<Call>>) {
    for it in items {
        it.calls = resolved.next().unwrap_or_default();
        apply_calls(&mut it.children, resolved);
    }
}

fn display_reexport(b: &Binding, m: &Module, ctx: &Ctx) -> String {
    let s = Lang::sep(m.lang);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Paths must render with `/` on every platform.
    ///
    /// Regression: on Windows `Path::display()` yields `pkg\gate.py`, while
    /// `move-plan` builds its destination by joining with `/` — so a single
    /// line of output read `pkg\gate.py  →  pkg/routing/gate.py`. Beyond being
    /// ugly, it broke ctx's determinism contract: identical source produced
    /// different output per platform. Caught by CI's Windows job.
    #[test]
    fn paths_render_with_forward_slashes_on_every_platform() {
        let p: PathBuf = ["pkg", "sub", "gate.py"].iter().collect();
        assert_eq!(slash_path(&p), "pkg/sub/gate.py");
        assert_eq!(slash_path(Path::new("gate.py")), "gate.py");
        assert_eq!(slash_path(Path::new("")), "");
    }

    /// A fingerprint that misses an edit is worse than no cache at all: it
    /// makes ctx confidently wrong about current source.
    #[test]
    fn the_fingerprint_moves_when_the_tree_does() {
        let dir = std::env::temp_dir().join(format!("ctx_fp_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        let f = dir.join("src/a.rs");
        fs::write(&f, "fn a() {}\n").unwrap();

        let base = source_fingerprint(&dir);
        assert_eq!(
            base,
            source_fingerprint(&dir),
            "must be stable when nothing changes"
        );

        // Content change. Length differs here, so this holds regardless of
        // filesystem mtime granularity.
        fs::write(&f, "fn a() {}\nfn b() {}\n").unwrap();
        let edited = source_fingerprint(&dir);
        assert_ne!(base, edited, "an edit must change the fingerprint");

        // Addition.
        fs::write(dir.join("src/c.rs"), "fn c() {}\n").unwrap();
        let added = source_fingerprint(&dir);
        assert_ne!(edited, added, "a new file must change the fingerprint");

        // Deletion.
        fs::remove_file(dir.join("src/c.rs")).unwrap();
        assert!(
            added != source_fingerprint(&dir),
            "a deletion must change it"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// The fingerprint must cover exactly what gets parsed. If it walked a
    /// different set, the cache would miss changes in the difference.
    #[test]
    fn the_fingerprint_covers_exactly_the_parsed_file_set() {
        let dir = std::env::temp_dir().join(format!("ctx_fpset_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/a.rs"), "fn a() {}\n").unwrap();
        // Not a source file ctx models, and inside a skipped directory.
        fs::write(dir.join("src/notes.txt"), "hello").unwrap();
        fs::create_dir_all(dir.join("target")).unwrap();
        fs::write(dir.join("target/b.rs"), "fn b() {}\n").unwrap();

        let files = source_files(&dir);
        assert_eq!(files.len(), 1, "only src/a.rs should count: {files:?}");
        assert!(files[0].ends_with("a.rs"));

        // Touching an ignored file must NOT invalidate — otherwise the cache
        // would never hit on a repo with a build directory.
        let before = source_fingerprint(&dir);
        fs::write(dir.join("target/b.rs"), "fn b() { /* changed */ }\n").unwrap();
        fs::write(dir.join("src/notes.txt"), "changed").unwrap();
        assert_eq!(
            before,
            source_fingerprint(&dir),
            "ignored files must not invalidate"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
