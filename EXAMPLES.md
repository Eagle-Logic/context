# `ctx` by example

Every command below was run against **this repository** with `ctx 0.18.0`, and
every block is verbatim output — nothing edited for effect.

The repo under analysis: **14 Rust files, 1 Markdown file, 15 modules, 4,668
call sites.** Small enough to read in a sitting, which makes it a fair place to
check whether the answers are actually right.

Timings are best-of-five on a warm page cache. The graph is rebuilt from source
on *every* command — there is no index, no daemon, no cache to invalidate:

```
ctx map --view skeleton    0.05s
ctx doctor                 0.05s
ctx callers <symbol>       0.05s
```

---

## 1. Who calls this? — `ctx callers`

The blast radius before you change a signature. Only *resolved* call sites, so
it is precise where a text grep floods.

```
$ ctx callers coverage_report
5 caller(s) of 'coverage_report':

mcp::dispatch  (src/mcp.rs:177)  → query::coverage_report
query::tests::coverage_separates_internal_external_and_blind_spots  (src/query.rs:2620)  → coverage_report
query::tests::doctor_names_what_it_could_not_pin  (src/query.rs:2044)  → coverage_report
query::tests::doctor_recall_excludes_provably_external_calls  (src/query.rs:2019)  → coverage_report
query::tests::markdown_links_resolve_headings_and_flag_broken  (src/query.rs:2596)  → coverage_report

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

crate::main  [src/main.rs:439]
  → mcp::run  [src/mcp.rs:14]
    → mcp::handle_method  [src/mcp.rs:54]
      → mcp::tools_call  [src/mcp.rs:163]
        → mcp::dispatch  [src/mcp.rs:177]
          → query::coverage_report  [src/query.rs:1420]
```

---

## 3. What runs underneath this? — `ctx trace`

Transitive, not one hop. Cycles and repeated subtrees are cut with a marker
rather than expanded forever.

```
$ ctx trace build_graph --depth 2
# call tree from 'build_graph'  (depth 2)
~ heuristic edge (verify) · * one branch of a dispatch fan-out

extract::build_graph  [src/extract/mod.rs:115]  [+1 outside graph]
├─ extract::disambiguate_module_names  [src/extract/mod.rs:247]
├─ extract::lang_selected  [src/extract/mod.rs:110]
│  └─ extract::filter  [src/extract/mod.rs:53]
├─ extract::markdown::extract  [src/extract/markdown.rs:45]  [+1 outside graph]
│  ├─ extract::markdown::atx_heading  [src/extract/markdown.rs:198]
│  ├─ extract::markdown::collect_link_defs  [src/extract/markdown.rs:374]  (depth limit)
│  ├─ extract::markdown::slug  [src/extract/markdown.rs:13]
│  └─ extract::markdown::strip_inline  [src/extract/markdown.rs:422]
├─ extract::module_name  [src/extract/mod.rs:366]
│  └─ model::Lang::sep  [src/model.rs:143]
├─ extract::resolve_deps  [src/extract/mod.rs:424]
│  ├─ extract::build_universe  [src/extract/mod.rs:822]  (depth limit)
│  ├─ extract::compute_calls  [src/extract/mod.rs:1455]  (depth limit)
│  ├─ model::Module::resolve_segs~  [src/model.rs:250]  (depth limit)
│  └─ extract::resolve_from  [src/extract/mod.rs:632]  (depth limit)
└─ extract::walker  [src/extract/mod.rs:88]
   └─ extract::filter  [src/extract/mod.rs:53]
```

*(trimmed for length — the real output lists all 8 branches)*

Three annotations carry the honesty:

- `~` on `resolve_segs` — inferred from a receiver whose type isn't written in
  the source. Verify that one.
- `[+1 outside graph]` — a branch left the resolved edges here.
- `(depth limit)` — cut by `--depth 2`, not a dead end.

---

## 4. Did I break the API? — `ctx changed --api`

A pre-merge gate that names the callers a change breaks. This run is against
real history from the session that shipped 0.18.0 — it caught a removal the
author had made four commits earlier:

```
$ ctx changed --api --since HEAD~4
# API changes vs HEAD~4
2 removed, 8 changed, 20 added.

## Removed — breaking
- model::Module::name_segs  [fn]  (src/model.rs:173)
    was: pub fn name_segs(&self) -> Vec<String>
    callers (2): extract::candidates, extract::resolve_deps
- query::subtree  [fn]  (src/query.rs:550)
    was: pub fn subtree(g: &Graph, module: &str, json_out: bool) -> String
    callers (1): mcp::dispatch

## Changed signature — potentially breaking
- model::Call  [struct]  (src/model.rs:190)
    was: pub struct Call { pub to: String, pub heuristic: bool }
```

`--strict` makes it a CI gate. It fails on **removals only** — a signature
change might be an added optional parameter, and a gate that cries wolf gets
switched off.

---

## 5. What must a move touch? — `ctx move-plan`

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

Modules: 15  (markdown 1, rust 14)

## Internal recall — the number to trust
  1001/1040 = 96.2%   of call sites that could be internal, ctx pinned this many.

## Every call site, bucketed
call sites:            4668
  internal edges:      1001   [15 heuristic (~), 0 dispatch fan-out (*)]
  external (provable): 3628   (77.7%)  callee defined nowhere here — std/extern
  unresolved internal: 39     (0.8%)  the real misses — see below

## What ctx missed (callee names that exist here but went unpinned)
grep these; every other edge in the map is one ctx could prove.
     26  walk
      7  context
      2  path
      1  flatten
      1  name
      1  render_budgeted
      1  subtree_text

## Where the misses are
  extract::rust                      11 unresolved   (module recall 93%)
  extract::typescript                11 unresolved   (module recall 90%)
  extract::python                    10 unresolved   (module recall 84%)
  extract                             3 unresolved   (module recall 98%)
  mcp                                 2 unresolved   (module recall 94%)
  git                                 1 unresolved   (module recall 97%)
  query                               1 unresolved   (module recall 99%)

## Low-confidence zones (edges to distrust — grep to confirm)
  parity                           18% heuristic (11/61 edges)
```

96.2% recall comes with **the exact grep list for the other 3.8%** — seven
names, with counts and the modules they live in.

The denominator is honest too. 3,628 of 4,668 call sites go into `std` or a
third-party crate, where no internal edge could ever exist, so they're excluded
rather than quietly inflating the percentage. That classification is by evidence
— *is this name defined anywhere under the root?* — not a hardcoded list.

The honest bit isn't that coverage is high. It's that the gaps are enumerable.

> Captured before this file was committed. `ctx` treats Markdown as part of the
> graph, so adding `EXAMPLES.md` moves the header to `Modules: 16 (markdown 2,
> rust 14)`. The recall figures are unchanged — prose contributes links, not
> call sites. Left as captured rather than retouched, since "verbatim" is the
> whole point.

---

## 8. Where's the heart of this codebase? — `ctx core`

PageRank over the module graph. This is the one command that overlaps what
other repo-map tools do, and it's deliberately a small part of the surface.

```
$ ctx core --limit 8
# Core modules — /home/steve/projects/context
Ranked by dependency centrality (PageRank); higher = more depended-upon.

  score    in  out  module
  0.3241    11    0  model  [18 items]
  0.1701    10    1  extract  [67 items]
  0.0735     4    2  render  [8 items]
  0.0573     3    3  view  [11 items]
  0.0550     2    4  query  [120 items]
  0.0320     0    4  crate  [46 items]
  0.0320     0    2  extract::markdown  [31 items]
  0.0320     0    2  extract::python  [26 items]
```

`model` on top with 11 inbound and 0 outbound is the right answer: it's the
shared data model every other module depends on and which depends on nothing.

---

## 9. Where is this defined? — `ctx def`

Jump-to-def without knowing the file, across languages.

```
$ ctx def Universe
1 definition(s) of 'Universe':

extract::Universe   [struct]   src/extract/mod.rs:803
    struct Universe { methods: MethodIndex, all_names: BTreeSet<String>, ... }
      — Whole-tree symbol evidence, built once and shared by every module's
```

The trailing `—` is the first line of the doc comment, so a signature listing
doubles as a labelled one at no extra token cost.

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
