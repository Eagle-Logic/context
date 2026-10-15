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
    /// The path segments resolution keys on, carried rather than re-derived.
    ///
    /// A module name is these segments joined by the language separator, and
    /// resolution needs them back. Splitting the name to recover them is what
    /// made any component containing the separator ambiguous: `app.config.ts`
    /// reads as two segments, so `./app.config` resolved one level too deep.
    /// Keeping the Vec removes the ambiguity instead of encoding around it,
    /// which is why names may contain a dot again and still match their file.
    ///
    /// For Go this is the package *directory*, not the file: a package is the
    /// unit of import, and every `.go` file in it is a peer.
    #[serde(skip)]
    pub path_segs: Vec<String>,
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
    /// Go: the `module` path from the nearest enclosing go.mod, split into
    /// segments. An import starting with these segments is internal and its
    /// remainder is a directory path under `crate_prefix`; anything else is
    /// another module's. Empty when no go.mod was found, which drops Go import
    /// resolution back to suffix matching.
    #[serde(skip)]
    pub go_module: Vec<String>,
    /// Declared return types of this module's callables; see
    /// [`FileFacts::returns`]. Consumed during resolution.
    #[serde(skip)]
    pub returns: BTreeMap<String, String>,
    /// Source regions the extractor did not enter, by kind — carried from
    /// [`FileFacts::undescended`].
    ///
    /// Deliberately NOT on [`Diagnostics`], which `resolve_deps` replaces
    /// wholesale after extraction; a value seeded at extract time would be
    /// silently dropped there.
    #[serde(skip)]
    pub undescended: BTreeMap<String, usize>,
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
    /// Declared return types, for resolving a [`Receiver::Returned`].
    ///
    /// Keyed by the callee as a call site writes it: a bare name for a free
    /// function, `Type.Method` for a method. The value is the base type of the
    /// callee's FIRST result, which is what `x, err := New()` binds `x` to —
    /// position 0 of a result tuple, not a guess about which result matters.
    pub returns: BTreeMap<String, String>,
    /// Source regions this extractor did not enter, by human-readable kind.
    ///
    /// The third quantity. A call site ctx resolved is an edge; one it saw and
    /// could not pin is `Diagnostics::unresolved`; one inside a region never
    /// entered is neither — it is absent, and absence cannot be a miss. That
    /// is the single path where a gap is invisible to the machinery whose
    /// whole job is making gaps enumerable, so the regions get counted even
    /// though their contents cannot be.
    ///
    /// Rust populates this with `macro_invocation`. The analogues are known
    /// and unmeasured: Python `exec`/`eval`, and TypeScript or JavaScript
    /// calls built from template strings.
    pub undescended: BTreeMap<String, usize>,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    Rust,
    Python,
    TypeScript,
    Go,
    Markdown,
}

impl Lang {
    pub fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::TypeScript => "typescript",
            Lang::Go => "go",
            Lang::Markdown => "markdown",
        }
    }

    /// The language's module-path separator. Every qualified name — module
    /// names, item qualnames, call paths — is joined and split with this, so
    /// a new language getting it wrong here is wrong everywhere at once.
    /// Deliberately the single definition: it used to be copied into three
    /// files, where two could agree and the third silently disagree.
    pub fn sep(self) -> &'static str {
        match self {
            Lang::Rust => "::",
            Lang::Python | Lang::TypeScript | Lang::Go | Lang::Markdown => ".",
        }
    }

    /// Every language, in the order [`Lang::all_exts`] emits extensions.
    ///
    /// That order reaches exactly two places: the `(supported: ...)` footer on
    /// `ctx doctor` and the `Searched:` line in BENCHMARK.md. It is NOT the
    /// order `doctor` lists languages — `coverage_report` keys a `BTreeMap` on
    /// [`Lang::name`], so that is alphabetical (`markdown 4, rust 15`) — and
    /// not the order `--lang` reports either, which echoes what the caller
    /// typed.
    pub const ALL: [Lang; 5] = [
        Lang::Rust,
        Lang::Python,
        Lang::TypeScript,
        Lang::Go,
        Lang::Markdown,
    ];

    /// Every file extension this language parses.
    ///
    /// Deliberately the single definition, for exactly the reason given on
    /// [`Lang::sep`] — and this list learned the lesson the hard way. It was
    /// copied into six places: `source_files`, the extension->`Lang` match in
    /// `build_graph`, `unsupported_census`, the `doctor` footer, and twice into
    /// `bench/run.sh`. Five agreed and the sixth silently disagreed: the
    /// benchmark harness never gained `.js` when JavaScript shipped, so every
    /// published preact row compared ctx over 232 files against a grep that
    /// opened 40 of them.
    ///
    /// JavaScript lives under `TypeScript` because one extractor parses both
    /// and they form a single interoperating module graph; `source_files`
    /// explains why splitting them into separate `Lang`s would break real
    /// edges.
    pub fn exts(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &["rs"],
            Lang::Python => &["py"],
            Lang::TypeScript => &["ts", "tsx", "js", "jsx", "mjs", "cjs"],
            Lang::Go => &["go"],
            Lang::Markdown => &["md", "markdown"],
        }
    }

    /// Compile-time proof that [`Lang::ALL`] lists every variant.
    ///
    /// `name`, `sep` and `exts` are exhaustive matches, so a new variant fails
    /// to compile in each. `ALL` is a hand-written array, so it does not — and
    /// everything routes through it: `from_ext`, `all_exts`, `--lang code`.
    ///
    /// A variant missing from `ALL` fails silently and completely. `from_ext`
    /// returns `None` for its extensions, so `source_files` never walks those
    /// files, `build_graph` skips them, `unsupported_census` reports the
    /// language as a *blind spot*, `--lang code` excludes it, and `--lang <it>`
    /// yields an empty graph. No error anywhere.
    ///
    /// Tests cannot catch this, which is the point of doing it here: every
    /// test iterates `ALL`, so an entry that was never added is invisible to
    /// all of them. This match is exhaustive, so adding a variant forces an
    /// arm; the arm indexes `ALL`, so if `ALL` was not grown the index is out
    /// of bounds on a fixed-size array and rustc rejects it outright. Both
    /// halves of the mistake become compile errors.
    //
    // Verified by adding a sixth variant: omitting the arm gives E0004, and an
    // arm pointing past the end of `ALL` gives "index out of bounds: the
    // length is 5 but the index is 5" under the deny-by-default
    // `unconditional_panic` lint. Never called — the guarantee is a property
    // of compiling the body, not of running it.
    #[allow(dead_code)]
    const fn all_is_exhaustive(self) -> &'static Lang {
        match self {
            Lang::Rust => &Lang::ALL[0],
            Lang::Python => &Lang::ALL[1],
            Lang::TypeScript => &Lang::ALL[2],
            Lang::Go => &Lang::ALL[3],
            Lang::Markdown => &Lang::ALL[4],
        }
    }

    /// The language that parses `ext`, or `None` if none does.
    ///
    /// `ext` is the bare extension, without a leading dot.
    pub fn from_ext(ext: &str) -> Option<Lang> {
        Lang::ALL.iter().copied().find(|l| l.exts().contains(&ext))
    }

    /// Every supported extension, grouped by language in [`Lang::ALL`] order.
    ///
    /// This is what `ctx doctor` prints and what `bench/run.sh` builds its
    /// `grep --include` list from, so neither restates the set.
    pub fn all_exts() -> impl Iterator<Item = &'static str> {
        Lang::ALL.iter().flat_map(|l| l.exts().iter().copied())
    }
}

/// Short content hash used for [`Item::hash`].
///
/// Truncated to 12 hex chars: long enough that a collision inside one file is
/// not a practical concern, short enough to sit on a result line without
/// crowding out the thing the reader came for.
pub fn content_hash(text: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    format!("{:012x}", h.finish() & 0xffff_ffff_ffff)
}

#[derive(Serialize, Clone)]
pub struct Item {
    pub kind: String,
    pub signature: String,
    pub line: usize,
    /// Last line of the definition, inclusive. With `line` this is a span, so
    /// an agent can read exactly the item instead of guessing how far to go —
    /// which is the difference between reading a function and reading the file
    /// it happens to sit in.
    pub end_line: usize,
    /// Short content hash of the item's full source span, body included.
    ///
    /// Two uses, both about not re-reading: an agent that already has this
    /// item can tell it is unchanged without opening the file, and two
    /// identical spans (a duplicated helper, a generated impl) are visibly the
    /// same without comparing text.
    pub hash: String,
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
    /// Every site in the caller's body that produced this edge, as
    /// `(line, end_line)` of the whole call expression.
    ///
    /// Plural because edges dedup by callee: three calls to `foo()` in one
    /// function are one edge with three sites. A multi-line call reports its
    /// full span, so the pair is a range to read, not a point to guess from.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sites: Vec<(usize, usize)>,
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
    /// A receiver bound to the result of calling something else: `r :=
    /// NewRouter()`, then `r.Path(..)`. The payload is the callee as written
    /// (`NewRouter`, `mux.NewRouter`, `Router.Path`).
    ///
    /// Go's dominant idiom, and the one case an extractor cannot settle on its
    /// own: the type is stated in the *callee's* signature, which may be in
    /// another file or another package. So the call site records what it was
    /// given and resolution looks the return type up, which makes it as backed
    /// by declared source as `Typed` — just resolved a step later.
    Returned(String),
    /// An opaque receiver (`expr.f()`): the type is unknown, so any
    /// attribution is a heuristic guess.
    Unknown,
}

/// One call site: `path` is the callee as written (`build`, `helpers::go`,
/// `a.b.f`); `recv` records how the receiver was expressed; `line`/`end_line`
/// span the whole call expression, so a multi-line call is a range.
#[derive(Clone)]
pub struct RawCall {
    pub path: String,
    pub recv: Receiver,
    /// First line of the call expression (1-based).
    pub line: usize,
    /// Last line of the call expression, inclusive.
    pub end_line: usize,
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
        if !self.path_segs.is_empty() {
            return self.path_segs.clone();
        }
        // Only reachable for a module deserialized from JSON, where `path_segs`
        // is skipped. Splitting the name is the old behaviour and the old bug,
        // but it is strictly better than resolving against nothing.
        self.name
            .split(self.lang.sep())
            .map(|s| s.to_string())
            .collect()
    }

    pub fn item_count(&self) -> usize {
        fn count(items: &[Item]) -> usize {
            items.iter().map(|i| 1 + count(&i.children)).sum()
        }
        count(&self.items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every extension must map back to the language that claims it. A typo or
    /// a duplicate across two `exts()` arms would otherwise route files to the
    /// wrong extractor, and `from_ext` picks the first match silently.
    #[test]
    fn every_extension_round_trips_to_its_own_language() {
        for lang in Lang::ALL {
            for ext in lang.exts() {
                assert_eq!(
                    Lang::from_ext(ext).map(Lang::name),
                    Some(lang.name()),
                    ".{ext} is claimed by {} but resolves elsewhere",
                    lang.name()
                );
            }
        }
    }

    /// The compile-time check guarantees `ALL` has an entry per variant; this
    /// guarantees each arm points at its OWN entry. A transposed index would
    /// otherwise satisfy the compiler and silently mis-order the array that
    /// `all_exts` and the `doctor` footer are built from.
    #[test]
    fn each_language_maps_to_its_own_slot_in_all() {
        for l in Lang::ALL {
            assert_eq!(
                l.all_is_exhaustive().name(),
                l.name(),
                "the ALL arm for {} points at the wrong entry",
                l.name()
            );
        }
    }

    #[test]
    fn no_extension_is_claimed_twice() {
        let mut seen: Vec<&str> = Lang::all_exts().collect();
        let before = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(before, seen.len(), "duplicate extension across languages");
    }

    #[test]
    fn unknown_extensions_resolve_to_nothing() {
        for ext in ["cpp", "java", "cs", "rb", "", "RS"] {
            assert!(Lang::from_ext(ext).is_none(), ".{ext} must not be claimed");
        }
    }

    /// The regression this single-sourcing exists for: these three were parsed
    /// by the walk while `doctor` reported them unsupported, because the footer
    /// restated the list instead of deriving it.
    #[test]
    fn javascript_module_and_long_markdown_extensions_are_supported() {
        for ext in ["mjs", "cjs", "markdown"] {
            assert!(Lang::from_ext(ext).is_some(), ".{ext} must be supported");
        }
    }
}
