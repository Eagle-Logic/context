# `ctx` by example

Every command below was run against **this repository** with `ctx 0.20.0`, and
every block is verbatim output — nothing edited for effect.

The repo under analysis: **14 Rust files, 2 Markdown files, 16 modules, 4,748
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

mcp::dispatch  (src/mcp.rs:361)  → query::coverage_report
query::tests::coverage_separates_internal_external_and_blind_spots  (src/query.rs:2856)  → coverage_report
query::tests::doctor_names_what_it_could_not_pin  (src/query.rs:2194)  → coverage_report
query::tests::doctor_recall_excludes_provably_external_calls  (src/query.rs:2169)  → coverage_report
query::tests::markdown_links_resolve_headings_and_flag_broken  (src/query.rs:2829)  → coverage_report

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
          → query::coverage_report  [src/query.rs:1548]
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
├─ extract::disambiguate_module_names  [src/extract/mod.rs:248]
├─ extract::lang_selected  [src/extract/mod.rs:110]
│  └─ extract::filter  [src/extract/mod.rs:53]
├─ extract::markdown::extract  [src/extract/markdown.rs:45]  [+1 outside graph]
│  ├─ extract::markdown::assign_lines  [src/extract/markdown.rs:136]  (depth limit)
│  ├─ extract::markdown::atx_heading  [src/extract/markdown.rs:198]
│  ├─ extract::markdown::clip  [src/extract/markdown.rs:433]
│  ├─ extract::markdown::collect_link_defs  [src/extract/markdown.rs:381]  (depth limit)
│  ├─ extract::markdown::extract_links  [src/extract/markdown.rs:310]  (depth limit)
│  ├─ extract::markdown::fence_open  [src/extract/markdown.rs:286]
│  ├─ extract::markdown::frontmatter_lines  [src/extract/markdown.rs:218]
│  ├─ extract::markdown::is_structural  [src/extract/markdown.rs:297]
│  ├─ extract::markdown::link_def  [src/extract/markdown.rs:393]
│  ├─ extract::markdown::nest  [src/extract/markdown.rs:177]  (depth limit)
│  ├─ extract::markdown::setext_heading  [src/extract/markdown.rs:267]
│  ├─ extract::markdown::slug  [src/extract/markdown.rs:13]
│  └─ extract::markdown::strip_inline  [src/extract/markdown.rs:429]
├─ extract::module_name  [src/extract/mod.rs:453]
│  └─ model::Lang::sep  [src/model.rs:143]
├─ extract::python::extract  [src/extract/python.rs:8]  [+1 outside graph]
│  ├─ extract::python::module_level_item  [src/extract/python.rs:34]  (depth limit)
│  └─ extract::python::visit  [src/extract/python.rs:70]  (depth limit)
├─ extract::resolve_deps  [src/extract/mod.rs:513]
│  ├─ extract::apply_calls  [src/extract/mod.rs:1850]  (depth limit)
│  ├─ extract::build_universe  [src/extract/mod.rs:1047]  (depth limit)
│  ├─ extract::compute_calls  [src/extract/mod.rs:1703]  (depth limit)
│  ├─ extract::display_reexport  [src/extract/mod.rs:1857]  (depth limit)
│  ├─ model::Module::resolve_segs~  [src/model.rs:250]  (depth limit)
│  └─ extract::resolve_from  [src/extract/mod.rs:720]  (depth limit)
├─ extract::rust::extract  [src/extract/rust.rs:8]  [+1 outside graph]
│  └─ extract::rust::visit  [src/extract/rust.rs:24]  (depth limit)
├─ extract::slash_path  [src/extract/mod.rs:443]
├─ extract::typescript::extract  [src/extract/typescript.rs:11]  [+1 outside graph]
│  ├─ extract::typescript::module_level_item  [src/extract/typescript.rs:42]  (depth limit)
│  └─ extract::typescript::visit  [src/extract/typescript.rs:81]  (depth limit)
└─ extract::walker  [src/extract/mod.rs:88]
   └─ extract::filter  [src/extract/mod.rs:53]
```

Three annotations carry the honesty:

- `~` on `resolve_segs` — inferred from a receiver whose type isn't written in
  the source. Verify that one.
- `[+1 outside graph]` — a branch left the resolved edges here.
- `(depth limit)` — cut by `--depth 2`, not a dead end.

---

## 4. Did I break the API? — `ctx changed --api`

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

Modules: 16  (markdown 2, rust 14)

## Internal recall — the number to trust
  1023/1065 = 96.1%   of call sites that could be internal, ctx pinned this many.

A call site is "could be internal" when the callee name is defined somewhere
under this root. Calls into std or a third-party crate are excluded, because no
internal edge could exist for them however good the resolver gets.

## Every call site, bucketed
call sites:            4748
  internal edges:      1023   [15 heuristic (~), 0 dispatch fan-out (*)]
  external (provable): 3683   (77.6%)  callee defined nowhere here — std/extern
  unresolved internal: 42     (0.9%)  the real misses — see below

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
  extract::python                    10 unresolved   (module recall 84%)
  mcp                                 5 unresolved   (module recall 91%)
  extract                             3 unresolved   (module recall 98%)
  git                                 1 unresolved   (module recall 97%)
  query                               1 unresolved   (module recall 99%)

## Low-confidence zones (edges to distrust — grep to confirm)
  parity                           18% heuristic (11/61 edges)
  extract                          1% heuristic (2/165 edges)
  crate                            1% heuristic (1/147 edges)
  query                            1% heuristic (1/190 edges)
```

96.1% recall comes with **the exact grep list for the other 3.9%** — eight
names, with counts and the modules they live in.

The denominator is honest too. 3,683 of 4,748 call sites go into `std` or a
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
  0.3060    11    0  model  [18 items]
  0.1606    10    1  extract  [67 items]
  0.0694     4    2  render  [8 items]
  0.0559     1    0  EXAMPLES  [11 items]
  0.0541     3    3  view  [11 items]
  0.0519     2    4  query  [120 items]
  0.0302     0    4  crate  [46 items]
  0.0302     0    2  extract::markdown  [31 items]
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

extract::Universe   [struct]   src/extract/mod.rs:1028
    struct Universe { methods: MethodIndex, all_names: BTreeSet<String>, module_segs: BTreeSet<String>, implementors: HashMap<String, BTreeSet<(String, String)>>, fields: HashMap<String, BTreeMap<String, String>> }  — Whole-tree symbol evidence, built once and shared by every module's
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
