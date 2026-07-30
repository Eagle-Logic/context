use std::collections::BTreeSet;

use serde::Serialize;

#[derive(Serialize, Clone)]
pub struct Graph {
    pub root: String,
    pub file_count: usize,
    pub modules: Vec<Module>,
}

#[derive(Serialize, Clone)]
pub struct Module {
    pub name: String,
    /// Original path-derived name, kept when `name` is renamed to break a
    /// collision. Empty means `name` was never renamed.
    #[serde(skip)]
    pub resolve_name: String,
    pub file: String,
    pub lang: Lang,
    /// Internal modules this module imports from, with re-export facades
    /// chased through to the defining module.
    pub deps: Vec<String>,
    /// External crates / packages referenced (top-level names, deduped).
    pub extern_deps: Vec<String>,
    /// Subset of `deps` that exists ONLY because of receiver-inferred (`~`)
    /// call edges. Rendered with a `~` so the "a `~`-free edge is reliable" rule
    /// holds at module level too, not just per call site.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub heuristic_deps: Vec<String>,
    /// (import text as written, module it resolved to) for every internal
    /// import. Resolution discards this mapping, but it is exactly what a
    /// rename/move plan needs: the literal string to rewrite, per site.
    #[serde(skip)]
    pub import_sites: Vec<(String, String)>,
    /// Symbols this module re-exports (Rust `pub use`, Python `__init__`
    /// imports), resolved to their source path where possible.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reexports: Vec<String>,
    pub items: Vec<Item>,
    /// Raw import strings as written in source; consumed during resolution.
    #[serde(skip)]
    pub raw_imports: Vec<String>,
    /// Name bindings introduced by use/import that downstream modules can
    /// import through; consumed during resolution.
    #[serde(skip)]
    pub raw_reexports: Vec<Binding>,
    /// Top-level symbol names defined in this module (chase termination).
    #[serde(skip)]
    pub defined_names: BTreeSet<String>,
    /// Path segments that prefix this module's name because of workspace
    /// layout (e.g. ["crates", "foo"]). Used to resolve `crate::` paths.
    #[serde(skip)]
    pub crate_prefix: Vec<String>,
    /// Python __init__.py: relative imports resolve against the module's own
    /// name rather than its parent.
    #[serde(skip)]
    pub is_package: bool,
    /// Call-edge resolution stats for this module (for `ctx doctor`).
    #[serde(skip)]
    pub diag: Diagnostics,
}

/// How completely a module's call sites resolved — the raw material for the
/// coverage report. `dropped = call_sites - resolved`.
#[derive(Default, Clone)]
pub struct Diagnostics {
    /// Call sites seen (post-extract, so `<T as Trait>::f` and other
    /// extract-time drops are not counted here).
    pub call_sites: usize,
    /// Call sites that produced an edge.
    pub resolved: usize,
    /// Of the resolved, how many are heuristic (receiver-inferred).
    pub heuristic: usize,
    /// Ubiquitous std/builtin method names (`push`, `iter`, `map`, …) that are
    /// intentionally never edged — not a blind spot.
    pub std_builtin: usize,
    /// Markdown only: (line, target) of links that resolve to no existing
    /// file or heading — i.e. genuinely broken links.
    pub broken_links: Vec<(usize, String)>,
}

/// A name made importable from a module via use/import: `name` is the bound
/// (possibly aliased) name, `path` the source path as written; "*" is a glob.
#[derive(Clone)]
pub struct Binding {
    pub name: String,
    pub path: String,
    pub public: bool,
}

/// Everything an extractor pulls out of one source file.
#[derive(Default)]
pub struct FileFacts {
    pub items: Vec<Item>,
    pub imports: Vec<String>,
    pub reexports: Vec<Binding>,
    pub defined: BTreeSet<String>,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    Rust,
    Python,
    TypeScript,
    Markdown,
}

impl Lang {
    pub fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::TypeScript => "typescript",
            Lang::Markdown => "markdown",
        }
    }
}

#[derive(Serialize, Clone)]
pub struct Item {
    pub kind: String,
    pub signature: String,
    pub line: usize,
    /// First line of the item's doc comment (Rust `///`) or docstring
    /// (Python), if present. A cheap semantic label for the signature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    /// Resolved call edges out of this function (empty for non-functions
    /// and for calls that could not be resolved unambiguously).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<Call>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Item>,
    /// Number of value parameters (receiver excluded) for functions/methods;
    /// None for non-callables. Used by `ctx parity` for cross-language arity
    /// comparison, where signature text is not comparable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arity: Option<usize>,
    /// Bare symbol name (impl blocks: the type name); used for resolution.
    #[serde(skip)]
    pub name: Option<String>,
    /// Call sites as written in source; consumed during resolution.
    #[serde(skip)]
    pub raw_calls: Vec<RawCall>,
}

/// One resolved outgoing call edge.
#[derive(Serialize, Clone)]
pub struct Call {
    pub to: String,
    /// True when the edge was inferred by a receiver-type heuristic
    /// (enclosing-impl attribution of an opaque receiver, or unique
    /// method-name lookup) rather than a resolved import/path/definition.
    #[serde(skip_serializing_if = "is_false")]
    pub heuristic: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// How a callee was referenced — governs how confidently a receiver method
/// call can be attributed to a container.
#[derive(Clone)]
pub enum Receiver {
    /// A free function or fully-pathed call: `foo()`, `a::b::foo()`.
    Free,
    /// An explicit self/Self receiver (`self.f()`, `Self::f()`): the
    /// enclosing impl/class is the correct container.
    SelfType,
    /// An opaque receiver (`expr.f()`): the type is unknown, so any
    /// attribution is a heuristic guess.
    Unknown,
}

/// One call site: `path` is the callee as written (`build`, `helpers::go`,
/// `a.b.f`); `recv` records how the receiver was expressed.
#[derive(Clone)]
pub struct RawCall {
    pub path: String,
    pub recv: Receiver,
}

impl Module {
    /// Path-derived segments used for RESOLUTION, which must survive
    /// collision renaming.
    ///
    /// `name` can be rewritten to keep display names unique (`native` ->
    /// `native@README`), but import and link resolution does path arithmetic
    /// against a module's own location — for an index-like module the name *is*
    /// its directory — so resolving against a renamed name silently loses every
    /// edge. Lookups therefore key on this, while edges and labels use `name`.
    pub fn resolve_segs(&self) -> Vec<String> {
        let base = if self.resolve_name.is_empty() {
            &self.name
        } else {
            &self.resolve_name
        };
        let sep = match self.lang {
            Lang::Rust => "::",
            Lang::Python | Lang::TypeScript | Lang::Markdown => ".",
        };
        base.split(sep).map(|s| s.to_string()).collect()
    }

    pub fn item_count(&self) -> usize {
        fn count(items: &[Item]) -> usize {
            items.iter().map(|i| 1 + count(&i.children)).sum()
        }
        count(&self.items)
    }
}
