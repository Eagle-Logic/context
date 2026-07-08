use std::collections::BTreeSet;

use serde::Serialize;

#[derive(Serialize)]
pub struct Graph {
    pub root: String,
    pub file_count: usize,
    pub modules: Vec<Module>,
}

#[derive(Serialize)]
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
}

/// A name made importable from a module via use/import: `name` is the bound
/// (possibly aliased) name, `path` the source path as written; "*" is a glob.
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
}

#[derive(Serialize)]
pub struct Item {
    pub kind: String,
    pub signature: String,
    pub line: usize,
    /// Resolved call edges out of this function (empty for non-functions
    /// and for calls that could not be resolved unambiguously).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Item>,
    /// Bare symbol name (impl blocks: the type name); used for resolution.
    #[serde(skip)]
    pub name: Option<String>,
    /// Call sites as written in source; consumed during resolution.
    #[serde(skip)]
    pub raw_calls: Vec<RawCall>,
}

/// One call site: `path` is the callee as written (`build`, `helpers::go`,
/// `a.b.f`); `method` marks receiver-based calls (`.steer()`, `self.helper()`)
/// whose receiver type is unknown.
pub struct RawCall {
    pub path: String,
    pub method: bool,
}

impl Module {
    pub fn name_segs(&self) -> Vec<String> {
        let sep = match self.lang {
            Lang::Rust => "::",
            Lang::Python => ".",
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
