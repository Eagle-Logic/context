pub mod python;
pub mod rust;

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::WalkBuilder;

use crate::model::{Binding, Graph, Item, Lang, Module, RawCall};

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
        if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("rs") | Some("py")
        ) {
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
        let lang = match path.extension().and_then(|e| e.to_str()) {
            Some("rs") => Lang::Rust,
            Some("py") => Lang::Python,
            _ => continue,
        };
        let mut facts = match lang {
            Lang::Rust => {
                rust::extract(&src).with_context(|| format!("parsing {}", rel.display()))?
            }
            Lang::Python => {
                python::extract(&src).with_context(|| format!("parsing {}", rel.display()))?
            }
        };
        let is_package =
            lang == Lang::Python && rel.file_name().and_then(|f| f.to_str()) == Some("__init__.py");
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
        });
    }

    resolve_deps(&mut modules);
    modules.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Graph {
        root: root.display().to_string(),
        file_count: files.len(),
        modules,
    })
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
        };
        if !drop {
            segs.push(stem.to_string());
        }
    }

    let name = if segs.is_empty() {
        match lang {
            Lang::Rust => "crate".to_string(),
            Lang::Python => "root".to_string(),
        }
    } else {
        segs.join(sep(lang))
    };
    (name, crate_prefix)
}

fn sep(lang: Lang) -> &'static str {
    match lang {
        Lang::Rust => "::",
        Lang::Python => ".",
    }
}

struct Ctx<'a> {
    modules: &'a [Module],
    /// Module name segments sorted longest-first so the most specific
    /// prefix wins.
    index: Vec<(Vec<String>, String)>,
    by_name: HashMap<String, usize>,
}

enum Resolved {
    Internal(String),
    External(String),
    Ignore,
}

fn resolve_deps(modules: &mut [Module]) {
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

    type ModuleResult = (BTreeSet<String>, BTreeSet<String>, Vec<String>, Vec<Vec<String>>);
    let results: Vec<ModuleResult> = {
        let ctx = Ctx {
            modules: &*modules,
            index,
            by_name,
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
                let (calls_per_item, call_deps) = compute_calls(m, &ctx, &method_index);
                deps.extend(call_deps.into_iter().filter(|d| d != &m.name));
                (deps, ext, reex, calls_per_item)
            })
            .collect()
    };

    for (m, (deps, ext, reex, calls)) in modules.iter_mut().zip(results) {
        m.deps = deps.into_iter().collect();
        m.extern_deps = ext.into_iter().collect();
        m.reexports = reex;
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
fn resolve_call(
    rc: &RawCall,
    container: Option<&str>,
    m: &Module,
    ctx: &Ctx,
    method_index: &MethodIndex,
) -> Option<(String, Option<String>)> {
    let s = sep(m.lang);

    if rc.method {
        return method_edge(&rc.path, container, m, ctx, method_index);
    }

    let segs: Vec<&str> = rc.path.split(s).map(str::trim).filter(|x| !x.is_empty()).collect();
    match segs.len() {
        0 => None,
        1 => {
            let name = segs[0];
            if m.defined_names.contains(name) {
                return Some((name.to_string(), None));
            }
            // `use crate::core::build; build()` / `from x import f; f()`
            let b = m.raw_reexports.iter().find(|b| b.name == name)?;
            let (n2, r2) = resolve_path(&b.path, m, ctx)?;
            let (fm, fr) = chase(&n2, &r2, ctx, CHASE_DEPTH);
            finish_call(&fm, &fr, m, ctx)
        }
        _ => {
            // Type-qualified local call: Engine::new() with Engine defined here.
            if m.defined_names.contains(segs[0]) {
                return Some((segs.join(s), None));
            }
            // First segment bound by use/import: helpers::go(), np-style aliases.
            if let Some(b) = m.raw_reexports.iter().find(|b| b.name == segs[0]) {
                let full = format!("{}{s}{}", b.path, segs[1..].join(s));
                let (n2, r2) = resolve_path(&full, m, ctx)?;
                let (fm, fr) = chase(&n2, &r2, ctx, CHASE_DEPTH);
                return finish_call(&fm, &fr, m, ctx);
            }
            // Direct path: crate::core::build(), pkg.utils.helper().
            if let Some((n2, r2)) = resolve_path(&rc.path, m, ctx) {
                let (fm, fr) = chase(&n2, &r2, ctx, CHASE_DEPTH);
                return finish_call(&fm, &fr, m, ctx);
            }
            // Python `obj.method()`: the receiver is opaque, but the trailing
            // name may still resolve like a method call.
            if m.lang == Lang::Python {
                if let Some(last) = segs.last() {
                    return method_edge(last, container, m, ctx, method_index);
                }
            }
            None
        }
    }
}

/// Resolve a receiver-less method name: the enclosing impl/class first,
/// then the global method index if the name is unique and not a std method.
fn method_edge(
    name: &str,
    container: Option<&str>,
    m: &Module,
    ctx: &Ctx,
    method_index: &MethodIndex,
) -> Option<(String, Option<String>)> {
    let s = sep(m.lang);
    if let Some(c) = container {
        if container_has_method(m, c, name) {
            return Some((format!("{c}{s}{name}"), None));
        }
    }
    if STD_METHODS.contains(&name) {
        return None;
    }
    let owners = method_index.get(name)?;
    if owners.len() != 1 {
        return None;
    }
    let (om, oc) = owners.iter().next().unwrap();
    let os = sep(ctx.modules[*ctx.by_name.get(om)?].lang);
    if om == &m.name {
        Some((format!("{oc}{os}{name}"), None))
    } else {
        Some((format!("{om}{os}{oc}{os}{name}"), Some(om.clone())))
    }
}

fn finish_call(
    fm: &str,
    fr: &[String],
    m: &Module,
    ctx: &Ctx,
) -> Option<(String, Option<String>)> {
    if fr.is_empty() {
        return None;
    }
    let target = &ctx.modules[*ctx.by_name.get(fm)?];
    if !target.defined_names.contains(&fr[0]) {
        return None;
    }
    let s = sep(target.lang);
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
    method_index: &MethodIndex,
) -> (Vec<Vec<String>>, BTreeSet<String>) {
    fn rec(
        items: &[Item],
        container: Option<&str>,
        m: &Module,
        ctx: &Ctx,
        method_index: &MethodIndex,
        per_item: &mut Vec<Vec<String>>,
        deps: &mut BTreeSet<String>,
    ) {
        for it in items {
            let mut calls = BTreeSet::new();
            for rc in &it.raw_calls {
                if let Some((disp, dep)) = resolve_call(rc, container, m, ctx, method_index) {
                    calls.insert(disp);
                    if let Some(d) = dep {
                        deps.insert(d);
                    }
                }
            }
            per_item.push(calls.into_iter().collect());
            let next = if matches!(it.kind.as_str(), "impl" | "trait" | "class") {
                it.name.as_deref()
            } else {
                container
            };
            rec(&it.children, next, m, ctx, method_index, per_item, deps);
        }
    }
    let mut per_item = Vec::new();
    let mut deps = BTreeSet::new();
    rec(&m.items, None, m, ctx, method_index, &mut per_item, &mut deps);
    (per_item, deps)
}

fn apply_calls(items: &mut [Item], resolved: &mut std::vec::IntoIter<Vec<String>>) {
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
