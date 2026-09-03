# `ctx` by example

Every command below was run against **this repository**, and every block is
verbatim output — nothing edited for effect. Everything except the three
git-ref-dependent commands (`changed --api`, `move-plan`, `parity`) is
regenerated from the current tree; those three are from `0.20.0` and say so
where they appear.

No version is stamped here on purpose. These blocks carry line numbers from the
tree they were run against, so a version string dates them without making them
current — this file had drifted a whole minor version before anyone noticed.
Regenerate it in the release commit instead.

The repo under analysis: **14 Rust files, 3 Markdown files, 17 modules, 5,101
call sites.** Small enough to read in a sitting, which makes it a fair place to
check whether the answers are actually right.

Timings are best-of-five on a warm page cache. The graph is rebuilt from source
on *every* command — there is no index, no daemon, no cache to invalidate:

```
ctx map --view skeleton    0.06s
ctx doctor                 0.06s
ctx callers <symbol>       0.06s
```

---

## 1. Who calls this? — `ctx callers`

The blast radius before you change a signature. Only *resolved* call sites, so
it is precise where a text grep floods.

```
$ ctx callers coverage_report
6 caller(s) of 'coverage_report':

mcp::dispatch  (src/mcp.rs:434-571  #94ff6095218b)  → query::coverage_report
query::tests::a_broken_link_reports_its_own_line_not_its_headings  (src/query.rs:3317-3329  #f94b64f4da35)  → coverage_report
query::tests::coverage_separates_internal_external_and_blind_spots  (src/query.rs:3332-3343  #adf073f295c1)  → coverage_report
query::tests::doctor_names_what_it_could_not_pin  (src/query.rs:2458-2467  #6cd4660b675e)  → coverage_report
query::tests::doctor_recall_excludes_provably_external_calls  (src/query.rs:2433-2445  #6c3b7200040d)  → coverage_report
query::tests::markdown_links_resolve_headings_and_flag_broken  (src/query.rs:3290-3314  #839e24537089)  → coverage_report

completeness: no call site named `coverage_report` went unresolved anywhere in this tree —
this blast radius is complete to the limit of what ctx parses.
```

That last line is the point. The answer states its own limits, so you know
whether the confirming grep is needed. Here it isn't.

---

## 2. How does execution get here? — `ctx path`

The shortest call path between two symbols, hop by hop. One command instead of
a grep chain.

```
$ ctx path main coverage_report
# path: main → coverage_report  (5 hop(s))
~ heuristic edge (verify) · * one branch of a dispatch fan-out

crate::main  [src/main.rs:462]
  → mcp::run  [src/mcp.rs:191]
    → mcp::handle_method  [src/mcp.rs:260]
      → mcp::tools_call  [src/mcp.rs:418]
        → mcp::dispatch  [src/mcp.rs:434]
          → query::coverage_report  [src/query.rs:1812]
```

---

## 3. What runs underneath this? — `ctx trace`

Transitive, not one hop. Cycles and repeated subtrees are cut with a marker
rather than expanded forever.

```
$ ctx trace build_graph --depth 2
# call tree from 'build_graph'  (depth 2)
~ heuristic edge (verify) · * one branch of a dispatch fan-out

extract::build_graph  [src/extract/mod.rs:204]  [+1 outside graph]
├─ extract::disambiguate_module_names  [src/extract/mod.rs:305]
├─ extract::markdown::extract  [src/extract/markdown.rs:45]  [+1 outside graph]
│  ├─ extract::markdown::assign_lines  [src/extract/markdown.rs:139]  (depth limit)
│  ├─ extract::markdown::assign_spans  [src/extract/markdown.rs:184]  (depth limit)
│  ├─ extract::markdown::atx_heading  [src/extract/markdown.rs:227]
│  ├─ extract::markdown::clip  [src/extract/markdown.rs:464]
│  ├─ extract::markdown::collect_link_defs  [src/extract/markdown.rs:410]  (depth limit)
│  ├─ extract::markdown::extract_links  [src/extract/markdown.rs:339]  (depth limit)
│  ├─ extract::markdown::fence_open  [src/extract/markdown.rs:315]
│  ├─ extract::markdown::frontmatter_lines  [src/extract/markdown.rs:247]
│  ├─ extract::markdown::is_structural  [src/extract/markdown.rs:326]
│  ├─ extract::markdown::link_def  [src/extract/markdown.rs:422]
│  ├─ extract::markdown::nest  [src/extract/markdown.rs:206]  (depth limit)
│  ├─ extract::markdown::setext_heading  [src/extract/markdown.rs:296]
│  ├─ extract::markdown::slug  [src/extract/markdown.rs:13]
│  └─ extract::markdown::strip_inline  [src/extract/markdown.rs:460]
├─ extract::module_name  [src/extract/mod.rs:510]
│  └─ model::Lang::sep  [src/model.rs:143]
├─ extract::python::extract  [src/extract/python.rs:9]  [+1 outside graph]
│  ├─ extract::python::module_level_item  [src/extract/python.rs:35]  (depth limit)
│  └─ extract::python::visit  [src/extract/python.rs:73]  (depth limit)
├─ extract::resolve_deps  [src/extract/mod.rs:570]
│  ├─ extract::apply_calls  [src/extract/mod.rs:1923]  (depth limit)
│  ├─ extract::build_universe  [src/extract/mod.rs:1104]  (depth limit)
│  ├─ extract::compute_calls  [src/extract/mod.rs:1760]  (depth limit)
│  ├─ extract::display_reexport  [src/extract/mod.rs:1930]  (depth limit)
│  ├─ model::Module::resolve_segs~  [src/model.rs:287]  (depth limit)
│  └─ extract::resolve_from  [src/extract/mod.rs:777]  (depth limit)
├─ extract::rust::extract  [src/extract/rust.rs:9]  [+1 outside graph]
│  └─ extract::rust::visit  [src/extract/rust.rs:25]  (depth limit)
├─ extract::slash_path  [src/extract/mod.rs:500]
├─ extract::source_files  [src/extract/mod.rs:140]
│  ├─ extract::lang_selected  [src/extract/mod.rs:129]  (depth limit)
│  └─ extract::walker  [src/extract/mod.rs:107]  (depth limit)
└─ extract::typescript::extract  [src/extract/typescript.rs:12]  [+1 outside graph]
   ├─ extract::typescript::module_level_item  [src/extract/typescript.rs:43]  (depth limit)
   └─ extract::typescript::visit  [src/extract/typescript.rs:84]  (depth limit)
```

Three annotations carry the honesty:

- `~` on `resolve_segs` — inferred from a receiver whose type isn't written in
  the source. Verify that one.
- `[+1 outside graph]` — a branch left the resolved edges here.
- `(depth limit)` — cut by `--depth 2`, not a dead end.

---

## 4. Did I break the API? — `ctx changed --api`
> Captured at `0.20.0`; it depends on a git ref, so the line numbers below
> are from that commit, not the current tree.


A pre-merge gate that names the callers a change breaks. This run is against
real history from the session that shipped 0.18.0 — it caught a removal the
author had made a few commits earlier:

```
$ ctx changed --api --since 4224d9c
# API changes vs 4224d9c
2 removed, 3 changed, 12 added.

## Removed — breaking
- model::Module::name_segs  [fn]  (src/model.rs:228)
    was: pub fn name_segs(&self) -> Vec<String>
    callers (3): extract::build_universe, extract::candidates, extract::resolve_deps
- query::subtree  [fn]  (src/query.rs:1070)
    was: pub fn subtree(g: &Graph, module: &str, json_out: bool) -> String
    callers (1): mcp::dispatch

## Changed signature — potentially breaking
- model::Module  [struct]  (src/model.rs:13)
```

`--strict` makes it a CI gate. It fails on **removals only** — a signature
change might be an added optional parameter, and a gate that cries wolf gets
switched off.

---

## 5. What must a move touch? — `ctx move-plan`
> Captured at `0.20.0`; it depends on a git ref, so the line numbers below
> are from that commit, not the current tree.


An oracle, not an actuator. `ctx` never writes source files: an agent can
already edit precisely, what it *can't* do is know it found every site.

```
$ ctx move-plan parity check::parity
# Move plan: parity → check::parity

## 1. Move the file

  src/parity.rs  →  src/check/parity.rs

## 2. Rewrite 0 import site(s)

  none — nothing imports this module

## Also required (Rust)

  Remove `mod parity;` from the old parent module and add `mod parity;` to the
  new one. Module declarations are not imports, so they do not appear above.

## Confidence

Every site in section 2 comes from import/link resolution — path arithmetic,
not receiver inference — so the list is exact for the languages ctx parses. It
does NOT cover: Rust `mod` declarations (see above), dynamic imports,
string-built paths, unparsed languages, or references in build files and CI
config. Grep for the old path once before deleting it.
```

Note what it refuses to hide: `mod parity;` is *not* an import, so rather than
silently omitting it, the plan names it as a manual step — and then spells out
its own scope limits.

---

## 6. Is the port faithful? — `ctx parity`
> Captured at `0.20.0`; it depends on a git ref, so the line numbers below
> are from that commit, not the current tree.


Cross-language structural comparison. A Python module and its Rust port are two
renderings of one skeleton, so they can be diffed directly.

```
$ ctx parity gate.py gate.rs --aliases py-rust
# ctx parity — source → port
source 6 members · target 5 · aligned 5 (83% of source) · 1 via alias

## Missing in port (1) — in source, no counterpart in target
  fn   Gate.record                  gate.py:16

## Arity drift (1)
  fn   Gate.score               source=2 → port=1   gate.py:7

## Aligned via alias (1) — matched through a rename rule, not exactly
  fn   Gate.__init__  →  Gate.new   (via new)

parity: 5/6 source aligned · 1 missing · 1 arity · 0 call · 0 moved
```

A dropped method, a dropped parameter, and the `__init__`→`new` rename — in one
command. Alias matches get their own section so the fuzz you opted into stays
visible. `--strict` exits non-zero for CI.

---

## 7. How much should I trust any of this? — `ctx doctor`

The differentiator. Every tool guesses; this one tells you where.

```
$ ctx doctor
# ctx coverage report — /home/steve/projects/context

Modules: 17  (markdown 3, rust 14)

## Internal recall — the number to trust
  1098/1140 = 96.3%   of call sites that could be internal, ctx pinned this many.

A call site is "could be internal" when the callee name is defined somewhere
under this root. Calls into std or a third-party crate are excluded, because no
internal edge could exist for them however good the resolver gets.

## Every call site, bucketed
call sites:            5101
  internal edges:      1098   [15 heuristic (~), 0 dispatch fan-out (*)]
  external (provable): 3961   (77.7%)  callee defined nowhere here — std/extern
  unresolved internal: 42     (0.8%)  the real misses — see below

## What ctx missed (callee names that exist here but went unpinned)
grep these; every other edge in the map is one ctx could prove.
     26  walk
      7  context
      2  est_tokens
      2  path
      1  as_path
      1  flatten
      1  name
      1  render_budgeted
      1  subtree_text

## Where the misses are
  extract::rust                      11 unresolved   (module recall 93%)
  extract::typescript                11 unresolved   (module recall 90%)
  extract::python                    10 unresolved   (module recall 85%)
  mcp                                 5 unresolved   (module recall 92%)
  extract                             3 unresolved   (module recall 98%)
  git                                 1 unresolved   (module recall 97%)
  query                               1 unresolved   (module recall 100%)

## Low-confidence zones (edges to distrust — grep to confirm)
  parity                           18% heuristic (11/61 edges)
  extract                          1% heuristic (2/172 edges)
  crate                            1% heuristic (1/148 edges)
  query                            0% heuristic (1/236 edges)

## Not modeled (blind spots)
  none — every source file under this root is a supported language
  (supported: .rs .py .ts .tsx .md)
```

96.2% recall comes with **the exact grep list for the other 3.8%** — eight
names, with counts and the modules they live in.

The denominator is honest too. 3,858 of 4,958 call sites go into `std` or a
third-party crate, where no internal edge could ever exist, so they're excluded
rather than quietly inflating the percentage. That classification is by evidence
— *is this name defined anywhere under the root?* — not a hardcoded list.

The honest bit isn't that coverage is high. It's that the gaps are enumerable.

> `ctx` treats Markdown as part of the graph, which is why this file counts
> toward the 16 modules above — a document about the tool is a node in the
> graph the tool builds.

---

## 8. Where's the heart of this codebase? — `ctx core`

PageRank over the module graph. This is the one command that overlaps what
other repo-map tools do, and it's deliberately a small part of the surface.

```
$ ctx core --limit 8
# Core modules — /home/steve/projects/context
Ranked by dependency centrality (PageRank); higher = more depended-upon.

  score    in  out  module
  0.3002    12    0  model  [19 items]
  0.1544    10    1  extract  [76 items]
  0.0657     4    2  render  [13 items]
  0.0546     1    0  EXAMPLES  [13 items]
  0.0512     3    3  view  [11 items]
  0.0491     2    4  query  [141 items]
  0.0295     0    0  SECURITY  [6 items]
  0.0295     0    4  crate  [46 items]
```

`model` on top with 11 inbound and 0 outbound is the right answer: it's the
shared data model every other module depends on and which depends on nothing.

`EXAMPLES` ranking fourth is this file. Markdown is part of the graph, so the
README's link to it is a real edge — which is also why `ctx doctor` above counts
16 modules and not 14. A document about the tool is a node in the graph the tool
builds.

---

## 9. Where is this defined? — `ctx def`

Jump-to-def without knowing the file, across languages.

```
$ ctx def Universe
1 definition(s) of 'Universe':

extract::Universe   [struct]   src/extract/mod.rs:1085-1102  #85eadbaee90b
    struct Universe { methods: MethodIndex, all_names: BTreeSet<String>, module_segs: BTreeSet<String>, implementors: HashMap<String, BTreeSet<(String, String)>>, fields: HashMap<String, BTreeMap<String, String>> }  — Whole-tree symbol evidence, built once and shared by every module's
```

The trailing `—` is the first line of the doc comment, so a signature listing
doubles as a labelled one at no extra token cost.

---

## 10. Everything needed to edit this — `ctx context`

The flagship. One call for the definition, the types in its signature, what it
calls, and what calls it — instead of a map→def→callers→subtree dance.

```
$ ctx context signature_types

# Context: query::signature_types

## Definition
query::signature_types   [fn]   src/query.rs:1179-1198  #4ac8c94b6d61
    fn signature_types<'a>( sig: &str, self_name: &str, idx: &'a HashMap<String, DefLite>, ) -> Vec<&'a DefLite>  — The type-like definitions referenced by a signature: PascalCase

## Signature types
- query::DefLite [struct]  src/query.rs:1056-1070  #c81b24d6be3b
    struct DefLite { qualname: String, kind: String, file: String, line: usize, end_line: usize, hash: String, doc: Option<String>, signature: String, lang: Lang }

## Calls — dependencies (1, sites in src/query.rs)
- identifiers  @ 1184

## Callers — dependents (1)
- query::context  (src/query.rs:1302-1587  #b0710a0017d0)  @ 1347, 1423

completeness: no call site named `signature_types` went unresolved anywhere in this tree —
this caller list is complete to the limit of what ctx parses.
```

Two things to notice. The callee list says **where** each call happens
(`@ 1184`), and the caller list says where it calls *back* (`@ 1363, 1438` —
`context` calls this from two places). Those are spans, not lines: a call spread
over five lines reports `1345-1349`. The section headings name the direction,
because outgoing dependencies and incoming dependents answer different
questions — "what does this need" versus "what breaks if I change it".

### `--include-source` — when the answer should not need a follow-up read

```
$ ctx context Receiver --include-source

# Context: model::Receiver

## Definition
model::Receiver   [enum]   src/model.rs:244-263  #fe75ad1b13f7
    pub enum Receiver { Free | SelfType | SelfField | Typed | Dyn | Unknown }  — How a callee was referenced — governs how confidently a receiver method

```rust
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
```

## Referenced by — dependents (6 signature(s))
- extract  (src/extract/mod.rs:1392)
    fn field_receiver(ty: &str, uni: &Universe) -> Receiver
- extract::rust  (src/extract/rust.rs:133)
    struct TypeEnv { vars: HashMap<String, Receiver> }
- extract::rust  (src/extract/rust.rs:237)
    fn classify_type(raw: &str, generics: &HashMap<String, String>) -> Option<Receiver>
- extract::rust  (src/extract/rust.rs:469)
    fn let_binding(n: Node, src: &str, env: &TypeEnv) -> Option<(String, Receiver)>
- extract::rust  (src/extract/rust.rs:567)
    fn receiver_of(v: Node, src: &str, env: &TypeEnv) -> Receiver
- model  (src/model.rs:269)
    pub struct RawCall { pub path: String, pub recv: Receiver, pub line: usize, pub end_line: usize }
Signature references only: uses inside function BODIES are not indexed.

completeness: no call site named `Receiver` went unresolved anywhere in this tree —
this caller list is complete to the limit of what ctx parses.
```rust
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
```

## Referenced by — dependents (6 signature(s))
- extract  (src/extract/mod.rs:1392)
    fn field_receiver(ty: &str, uni: &Universe) -> Receiver
- extract::rust  (src/extract/rust.rs:133)
    struct TypeEnv { vars: HashMap<String, Receiver> }
- extract::rust  (src/extract/rust.rs:237)
    fn classify_type(raw: &str, generics: &HashMap<String, String>) -> Option<Receiver>
- extract::rust  (src/extract/rust.rs:469)
    fn let_binding(n: Node, src: &str, env: &TypeEnv) -> Option<(String, Receiver)>
- extract::rust  (src/extract/rust.rs:567)
    fn receiver_of(v: Node, src: &str, env: &TypeEnv) -> Receiver
- model  (src/model.rs:269)
    pub struct RawCall { pub path: String, pub recv: Receiver, pub line: usize, pub end_line: usize }
Signature references only: uses inside function BODIES are not indexed.

completeness: no call site named `Receiver` went unresolved anywhere in this tree —
this caller list is complete to the limit of what ctx parses.
```rust
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
```

## Referenced by — dependents (6 signature(s))
- extract  (src/extract/mod.rs:1392)
    fn field_receiver(ty: &str, uni: &Universe) -> Receiver
- extract::rust  (src/extract/rust.rs:133)
    struct TypeEnv { vars: HashMap<String, Receiver> }
- extract::rust  (src/extract/rust.rs:237)
    fn classify_type(raw: &str, generics: &HashMap<String, String>) -> Option<Receiver>
- extract::rust  (src/extract/rust.rs:469)
    fn let_binding(n: Node, src: &str, env: &TypeEnv) -> Option<(String, Receiver)>
- extract::rust  (src/extract/rust.rs:567)
    fn receiver_of(v: Node, src: &str, env: &TypeEnv) -> Receiver
- model  (src/model.rs:269)
    pub struct RawCall { pub path: String, pub recv: Receiver, pub line: usize, pub end_line: usize }
Signature references only: uses inside function BODIES are not indexed.

completeness: no call site named `Receiver` went unresolved anywhere in this tree —
this caller list is complete to the limit of what ctx parses.
```rust
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
```

## Referenced by — dependents (6 signature(s))
- extract  (src/extract/mod.rs:1392)
    fn field_receiver(ty: &str, uni: &Universe) -> Receiver
- extract::rust  (src/extract/rust.rs:133)
    struct TypeEnv { vars: HashMap<String, Receiver> }
- extract::rust  (src/extract/rust.rs:237)
    fn classify_type(raw: &str, generics: &HashMap<String, String>) -> Option<Receiver>
- extract::rust  (src/extract/rust.rs:469)
    fn let_binding(n: Node, src: &str, env: &TypeEnv) -> Option<(String, Receiver)>
- extract::rust  (src/extract/rust.rs:567)
    fn receiver_of(v: Node, src: &str, env: &TypeEnv) -> Receiver
- model  (src/model.rs:269)
    pub struct RawCall { pub path: String, pub recv: Receiver, pub line: usize, pub end_line: usize }
Signature references only: uses inside function BODIES are not indexed.

completeness: no call site named `Receiver` went unresolved anywhere in this tree —
this caller list is complete to the limit of what ctx parses.
```

The enum is the case that earns this flag. Shown `Receiver` with no variants, a
model will confidently invent a seventh one; shown all six with their payloads,
it cannot. Note that the rendered signature above the fence collapses
`SelfField(String)` to `SelfField` — the payload types survive only in the
materialized source, which is precisely the gap the flag closes.

Note too that a type has **no call edges at all**, so there is no `## Calls`
section here. What replaces it is `## Referenced by` — every signature that
mentions the type, which is where changing a type actually breaks callers.

---

## Install

```sh
brew install eagle-logic/tap/ctx
# or
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/Eagle-Logic/context/releases/latest/download/code-context-installer.sh | sh
# or, with a Rust toolchain
cargo install code-context
```

The crate is `code-context`; the binary is `ctx`. Prebuilt for macOS
(x86_64/aarch64), Linux (gnu + static musl), and Windows.

MIT — <https://github.com/Eagle-Logic/context>
