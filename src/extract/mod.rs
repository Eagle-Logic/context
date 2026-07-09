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

pub fn build_graph(root: &Path) -> Result<Graph> {
    let root = root
        .canonicalize()
        .with_context(|| format!("cannot resolve path {}", root.display()))?;

    let mut files: Vec<PathBuf> = Vec::new();
    for entry in WalkBuilder::new(&root).build() {
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
            Some("rs") | Some("py") | Some("tsx") | Some("md") | Some("markdown") => true,
            // Skip `.d.ts` — ambient type declarations carry no topology.
            Some("ts") => !path.to_str().is_some_and(|s| s.ends_with(".d.ts")),
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

    resolve_deps(&mut modules, &root);
    modules.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Graph {
        root: root.display().to_string(),
        file_count: files.len(),
        modules,
    })
}

/// Source-language extensions `ctx` does not (yet) model — used by the
/// coverage report to name its blind spots honestly.
const UNSUPPORTED_SOURCE_EXTS: &[&str] = &[
    "js", "jsx", "mjs", "cjs", "go", "java", "kt", "kts", "rb", "c", "cc", "cpp", "cxx", "h",
    "hpp", "cs", "swift", "php", "scala", "clj", "ex", "exs", "lua", "vue", "svelte", "sql",
];

/// Count unmodeled source files by extension under `root`, honoring the same
/// directory-skip rules as the graph walk. Returns (ext, count), descending.
pub fn unsupported_census(root: &Path) -> Vec<(String, usize)> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for entry in WalkBuilder::new(root).build().flatten() {
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
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if UNSUPPORTED_SOURCE_EXTS.contains(&ext) {
                *counts.entry(ext.to_string()).or_default() += 1;
            }
        }
    }
    let mut v: Vec<(String, usize)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
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
        .map(|m| (m.name_segs(), m.name.clone()))
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
                let (calls_per_item, call_deps, diag) = compute_calls(m, &ctx, &method_index);
                deps.extend(call_deps.into_iter().filter(|d| d != &m.name));
                (deps, ext, reex, calls_per_item, diag)
            })
            .collect()
    };

    for (m, (deps, ext, reex, calls, diag)) in modules.iter_mut().zip(results) {
        m.deps = deps.into_iter().collect();
        m.extern_deps = ext.into_iter().collect();
        m.reexports = reex;
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
            let cur = m.name_segs();
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
                let cur = m.name_segs();
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
                let cur = m.name_segs();
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
                    heuristic: false,
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
) -> (Vec<Vec<Call>>, BTreeSet<String>, Diagnostics) {
    #[allow(clippy::too_many_arguments)]
    fn rec(
        items: &[Item],
        container: Option<&str>,
        m: &Module,
        ctx: &Ctx,
        method_index: &MethodIndex,
        per_item: &mut Vec<Vec<Call>>,
        deps: &mut BTreeSet<String>,
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
            rec(&it.children, next, m, ctx, method_index, per_item, deps, diag);
        }
    }
    let mut per_item = Vec::new();
    let mut deps = BTreeSet::new();
    let mut diag = Diagnostics::default();
    rec(&m.items, None, m, ctx, method_index, &mut per_item, &mut deps, &mut diag);
    (per_item, deps, diag)
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
