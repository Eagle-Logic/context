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

The repo under analysis: **15 Rust files, 4 Markdown files, 19 modules, 6,293
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
query::tests::a_broken_link_reports_its_own_line_not_its_headings  (src/query.rs:3318-3330  #f94b64f4da35)  → coverage_report
query::tests::coverage_separates_internal_external_and_blind_spots  (src/query.rs:3333-3344  #adf073f295c1)  → coverage_report
query::tests::doctor_names_what_it_could_not_pin  (src/query.rs:2459-2468  #6cd4660b675e)  → coverage_report
query::tests::doctor_recall_excludes_provably_external_calls  (src/query.rs:2434-2446  #6c3b7200040d)  → coverage_report
query::tests::markdown_links_resolve_headings_and_flag_broken  (src/query.rs:3291-3315  #839e24537089)  → coverage_report

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

crate::main  [src/main.rs:466]
  → mcp::run  [src/mcp.rs:191]
    → mcp::handle_method  [src/mcp.rs:260]
      → mcp::tools_call  [src/mcp.rs:418]
        → mcp::dispatch  [src/mcp.rs:434]
          → query::coverage_report  [src/query.rs:1813]
```

---

## 3. What runs underneath this? — `ctx trace`

Transitive, not one hop. Cycles and repeated subtrees are cut with a marker
rather than expanded forever.

```
$ ctx trace build_graph --depth 2
# call tree from 'build_graph'  (depth 2)
~ heuristic edge (verify) · * one branch of a dispatch fan-out

extract::build_graph  [src/extract/mod.rs:226]  [+1 outside graph]
├─ extract::disambiguate_module_names  [src/extract/mod.rs:373]
├─ extract::go::extract  [src/extract/go.rs:9]  [+1 outside graph]
│  ├─ extract::go::function  [src/extract/go.rs:669]  (depth limit)
│  ├─ extract::go::imports  [src/extract/go.rs:238]  (depth limit)
│  ├─ extract::go::interfaces_in  [src/extract/go.rs:168]  (depth limit)
│  ├─ model::content_hash  [src/model.rs:178]
│  ├─ extract::go::package_level_item  [src/extract/go.rs:197]  (depth limit)
│  ├─ extract::go::receiver_of_method  [src/extract/go.rs:499]  (depth limit)
│  ├─ extract::go::returns_in  [src/extract/go.rs:115]  (depth limit)
│  ├─ extract::go::types  [src/extract/go.rs:320]  (depth limit)
│  └─ extract::go::values  [src/extract/go.rs:468]  (depth limit)
├─ extract::go_modules  [src/extract/mod.rs:927]
│  ├─ extract::go_segs  [src/extract/mod.rs:915]
│  └─ extract::walker  [src/extract/mod.rs:109]  (depth limit)
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
├─ model::Lang::sep  [src/model.rs:165]
├─ extract::module_name  [src/extract/mod.rs:608]
│  ├─ model::Lang::sep  [src/model.rs:165]
│  └─ extract::safe_seg  [src/extract/mod.rs:604]  (depth limit)
├─ extract::nearest_go_module  [src/extract/mod.rs:972]
├─ extract::needs_jsx  [src/extract/mod.rs:222]
├─ extract::python::extract  [src/extract/python.rs:9]  [+1 outside graph]
│  ├─ extract::python::module_level_item  [src/extract/python.rs:35]  (depth limit)
│  └─ extract::python::visit  [src/extract/python.rs:73]  (depth limit)
├─ extract::resolve_deps  [src/extract/mod.rs:683]
│  ├─ extract::apply_calls  [src/extract/mod.rs:2473]  (depth limit)
│  ├─ extract::build_universe  [src/extract/mod.rs:1493]  (depth limit)
│  ├─ extract::compute_calls  [src/extract/mod.rs:2310]  (depth limit)
│  ├─ extract::display_reexport  [src/extract/mod.rs:2480]  (depth limit)
│  ├─ extract::go_mod_index  [src/extract/mod.rs:989]
│  ├─ extract::go_packages  [src/extract/mod.rs:1033]  (depth limit)
│  ├─ model::Module::resolve_segs~  [src/model.rs:319]  (depth limit)
│  └─ extract::resolve_from  [src/extract/mod.rs:1163]  (depth limit)
├─ extract::rust::extract  [src/extract/rust.rs:9]  [+1 outside graph]
│  └─ extract::rust::visit  [src/extract/rust.rs:25]  (depth limit)
├─ extract::slash_path  [src/extract/mod.rs:578]
├─ extract::source_files  [src/extract/mod.rs:142]
│  ├─ extract::lang_selected  [src/extract/mod.rs:131]  (depth limit)
│  └─ extract::walker  [src/extract/mod.rs:109]  (depth limit)
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

Modules: 19  (markdown 4, rust 15)

## Internal recall — the number to trust
  1355/1422 = 95.3%   of call sites that could be internal, ctx pinned this many.

A call site is "could be internal" when the callee name is defined somewhere
under this root. Calls into std or a third-party crate are excluded, because no
internal edge could exist for them however good the resolver gets.

## Every call site, bucketed
call sites:            6293
  internal edges:      1355   [17 heuristic (~), 0 dispatch fan-out (*)]
  external (provable): 4871   (77.4%)  callee defined nowhere here — std/extern
  unresolved internal: 67     (1.1%)  the real misses — see below

## What ctx missed (callee names that exist here but went unpinned)
grep these; every other edge in the map is one ctx could prove.
     47  walk
      9  context
      3  path
      2  est_tokens
      2  flatten
      1  as_path
      1  name
      1  render_budgeted
      1  subtree_text

## Where the misses are
  extract::go                        21 unresolved   (module recall 89%)
  extract::typescript                13 unresolved   (module recall 91%)
  extract::rust                      11 unresolved   (module recall 93%)
  extract::python                    10 unresolved   (module recall 85%)
  extract                             5 unresolved   (module recall 98%)
  mcp                                 5 unresolved   (module recall 92%)
  git                                 1 unresolved   (module recall 97%)
  query                               1 unresolved   (module recall 100%)

## Low-confidence zones (edges to distrust — grep to confirm)
  parity                           18% heuristic (11/61 edges)
  refactor                         5% heuristic (1/20 edges)
  extract                          1% heuristic (3/228 edges)
  crate                            1% heuristic (1/149 edges)
  query                            0% heuristic (1/236 edges)

## Not modeled (blind spots)
  source files present that ctx does not parse:
  .sh       1
  (supported: .rs .py .ts .tsx .js .jsx .go .md)
```

95.3% recall comes with **the exact grep list for the other 4.7%** — nine
names, with counts and the modules they live in.

The denominator is honest too. 4,871 of 6,293 call sites go into `std` or a
third-party crate, where no internal edge could ever exist, so they're excluded
rather than quietly inflating the percentage. That classification is by evidence
— *is this name defined anywhere under the root?* — not a hardcoded list.

The honest bit isn't that coverage is high. It's that the gaps are enumerable.

> `ctx` treats Markdown as part of the graph, which is why this file counts
> toward the 19 modules above — a document about the tool is a node in the
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
  0.2954    13    0  model  [19 items]
  0.1525    11    1  extract  [100 items]
  0.0601     4    2  render  [13 items]
  0.0468     3    3  view  [11 items]
  0.0448     2    4  query  [141 items]
  0.0384     1    0  BENCHMARK  [4 items]
  0.0384     1    0  EXAMPLES  [13 items]
  0.0270     0    0  SECURITY  [6 items]
```

`model` on top with 13 inbound and 0 outbound is the right answer: it's the
shared data model every other module depends on and which depends on nothing.

`EXAMPLES` and `BENCHMARK` in that list are this file and its neighbour.
Markdown is part of the graph, so the README's link to each is a real edge —
which is also why `ctx doctor` above counts 19 modules and not 15. A document
about the tool is a node in the graph the tool builds.

---

## 9. Where is this defined? — `ctx def`

Jump-to-def without knowing the file, across languages.

```
$ ctx def Universe
1 definition(s) of 'Universe':

extract::Universe   [struct]   src/extract/mod.rs:1474-1491  #85eadbaee90b
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
- query::context  (src/query.rs:1303-1588  #b0710a0017d0)  @ 1348, 1424

completeness: no call site named `signature_types` went unresolved anywhere in this tree —
this caller list is complete to the limit of what ctx parses.
```

Two things to notice. The callee list says **where** each call happens
(`@ 1184`), and the caller list says where it calls *back* (`@ 1348, 1424` —
`context` calls this from two places). Those are spans, not lines: a call spread
over five lines reports `1345-1349`. The section headings name the direction,
because outgoing dependencies and incoming dependents answer different
questions — "what does this need" versus "what breaks if I change it".

### `--include-source` — when the answer should not need a follow-up read

````
$ ctx context Receiver --include-source
# Context: model::Receiver

## Definition
model::Receiver   [enum]   src/model.rs:266-295  #eaedf052414f
    pub enum Receiver { Free | SelfType | SelfField | Typed | Dyn | Returned | Unknown }  — How a callee was referenced — governs how confidently a receiver method

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
```

## Referenced by — dependents (14 signature(s))
- extract  (src/extract/mod.rs:1916)
    fn field_receiver(ty: &str, uni: &Universe) -> Receiver
- extract::go  (src/extract/go.rs:520)
    struct TypeEnv { vars: HashMap<String, Receiver> }
- extract::go  (src/extract/go.rs:527)
    fn classify_type(raw: &str, ifaces: &BTreeSet<String>) -> Option<Receiver>
- extract::go  (src/extract/go.rs:635)
    fn param_types(node: Node, src: &str, ifaces: &BTreeSet<String>) -> Vec<(String, Receiver)>
- extract::go  (src/extract/go.rs:791)
    fn short_var_bindings( n: Node, src: &str, env: &TypeEnv, ifaces: &BTreeSet<String>, ) -> Vec<(String, Receiver)>
- extract::go  (src/extract/go.rs:836)
    fn var_bindings( n: Node, src: &str, env: &TypeEnv, ifaces: &BTreeSet<String>, ) -> Vec<(String, Receiver)>
- extract::go  (src/extract/go.rs:875)
    fn value_type(v: Node, src: &str, env: &TypeEnv, ifaces: &BTreeSet<String>) -> Option<Receiver>
- extract::go  (src/extract/go.rs:946)
    fn new_arg_type(v: Node, src: &str, ifaces: &BTreeSet<String>) -> Option<Receiver>
- extract::go  (src/extract/go.rs:965)
    let push = |path: String, recv: Receiver, out: &mut Vec<RawCall>|
- extract::rust  (src/extract/rust.rs:133)
    struct TypeEnv { vars: HashMap<String, Receiver> }
- extract::rust  (src/extract/rust.rs:237)
    fn classify_type(raw: &str, generics: &HashMap<String, String>) -> Option<Receiver>
- extract::rust  (src/extract/rust.rs:469)
    fn let_binding(n: Node, src: &str, env: &TypeEnv) -> Option<(String, Receiver)>
- extract::rust  (src/extract/rust.rs:567)
    fn receiver_of(v: Node, src: &str, env: &TypeEnv) -> Receiver
- model  (src/model.rs:301)
    pub struct RawCall { pub path: String, pub recv: Receiver, pub line: usize, pub end_line: usize }
Signature references only: uses inside function BODIES are not indexed.

completeness: no call site named `Receiver` went unresolved anywhere in this tree —
this caller list is complete to the limit of what ctx parses.
````

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
