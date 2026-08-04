use std::collections::{BTreeMap, BTreeSet};

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
    pub file: String,
    pub lang: Lang,
    /// Internal modules this module imports from, with re-export facades
    /// chased through to the defining module.
    pub deps: Vec<String>,
    /// External crates / packages referenced (top-level names, deduped).
    pub extern_deps: Vec<String>,
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
/// coverage report.
///
/// Every call site lands in exactly one of three buckets, so
/// `call_sites == resolved + external + unresolved`. The split between
/// `external` and `unresolved` is the whole point: `external` is *provably*
/// not an internal edge (the callee name is defined nowhere under this root),
/// while `unresolved` is a genuine miss — a name that does exist here but that
/// ctx could not pin to a definition. Internal recall is
/// `resolved / (resolved + unresolved)`; the old "share of all call sites"
/// figure is dominated by std/extern traffic and understates the graph badly.
#[derive(Default, Clone)]
pub struct Diagnostics {
    /// Call sites seen (post-extract).
    pub call_sites: usize,
    /// Call sites that produced at least one edge.
    pub resolved: usize,
    /// Of the resolved, how many are heuristic (receiver-inferred).
    pub heuristic: usize,
    /// Of the resolved, how many fanned out over trait/interface impls.
    pub dispatch: usize,
    /// Provably external: the callee name is defined nowhere under this root,
    /// so no internal edge could exist. Never a blind spot.
    pub external: usize,
    /// The real misses: the callee name IS defined somewhere under this root,
    /// but ctx could not pin which definition.
    pub unresolved: usize,
    /// Census of the callee names behind `external`, for `doctor --explain`.
    pub extern_names: BTreeMap<String, usize>,
    /// Census of the callee names behind `unresolved`, for `doctor --explain`
    /// and for the completeness warning on `callers`/`context`.
    pub unresolved_names: BTreeMap<String, usize>,
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
    /// The abstraction this container implements: the trait of a Rust
    /// `impl Trait for Type`, a TypeScript `implements`/`extends` clause, or a
    /// Python base class. Drives dispatch fan-out.
    #[serde(skip)]
    pub implements: Vec<String>,
    /// Declared field/property types by field name (struct fields, class
    /// properties). Lets `self.field.method()` resolve.
    #[serde(skip)]
    pub field_types: BTreeMap<String, String>,
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
    /// True when the edge is one branch of a dynamic-dispatch fan-out: the
    /// call goes through a trait object / interface / bounded generic, and
    /// this is one of the implementations it may land in. Over-approximate by
    /// construction — exactly one sibling `dispatch` edge runs at a time.
    #[serde(skip_serializing_if = "is_false")]
    pub dispatch: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// How a callee was referenced — governs how confidently a receiver method
/// call can be attributed to a container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Receiver {
    /// A free function or fully-pathed call: `foo()`, `a::b::foo()`.
    Free,
    /// An explicit self/Self receiver (`self.f()`, `Self::f()`): the
    /// enclosing impl/class is the correct container.
    SelfType,
    /// `self.field.method()` — the receiver is a field of the enclosing type,
    /// resolved against that type's declared field types.
    SelfField(String),
    /// A receiver whose concrete type is known from a local binding, a
    /// parameter annotation, or a field declaration: `let e: Engine`, then
    /// `e.step()`. The attribution is backed by a type written in the source.
    Typed(String),
    /// A receiver that is a trait object, `impl Trait`, a bounded generic, or
    /// an interface-typed value: the call dispatches over every implementation.
    Dyn(String),
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
    pub fn name_segs(&self) -> Vec<String> {
        let sep = match self.lang {
            Lang::Rust => "::",
            Lang::Python | Lang::TypeScript | Lang::Markdown => ".",
        };
        self.name.split(sep).map(|s| s.to_string()).collect()
    }

    pub fn item_count(&self) -> usize {
        fn count(items: &[Item]) -> usize {
            items.iter().map(|i| 1 + count(&i.children)).sum()
        }
        count(&self.items)
    }
}
