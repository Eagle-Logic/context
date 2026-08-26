# context (`ctx`)

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

**A queryable code graph for coding agents.** Not a map you paste at boot — a
set of queries an agent runs mid-task: *who calls this, how does execution get
here, what breaks if I change it, does my port still match the original.*

One Rust binary. Tree-sitter for Rust, Python, TypeScript/TSX, and Markdown. No
language servers, no embeddings, no index to warm. The graph is a pure function
of the source tree — same code in, same answer out — and it builds in ~100 ms,
so every query runs against current source.

```sh
$ ctx callers resolve_call
1 caller(s) of 'resolve_call':

extract::Walk::rec  (src/extract/mod.rs:1236)  → resolve_call

completeness: no call site named `resolve_call` went unresolved anywhere in this
tree — this blast radius is complete to the limit of what ctx parses.
```

That last line is the whole idea. The answer travels with its own limits.

## Three things you won't find elsewhere

### 1. Every edge tells you how much to trust it

Every static analyzer guesses. `ctx` is the one that says where.

An edge backed by a resolved import, a path, a `self` receiver, or a declared
type is unmarked — rely on it. An edge inferred from an *opaque* receiver
(`expr.method()`, where nothing in the source states the type) is marked `~`. A
branch of a dynamic-dispatch fan-out is marked `*`. And `ctx doctor` names
**every callee it could not pin at all**, with counts:

```
## Internal recall — the number to trust
  787/824 = 95.5%   of call sites that could be internal, ctx pinned this many.

## What ctx missed (callee names that exist here but went unpinned)
grep these; every other edge in the map is one ctx could prove.
     26  walk
      7  context
      2  path
      1  flatten
      1  name

## Low-confidence zones (edges to distrust — grep to confirm)
  parity                           26% heuristic (10/38 edges)
```

The recall number comes with the exact grep list for everything it doesn't
cover. The denominator is honest too: a call into `std` or a third-party crate
is excluded, because no internal edge could exist for it however good the
resolver gets — and that classification is by evidence (is this name defined
anywhere under the root?), not a hardcoded list.

The honest bit isn't that coverage is high. It's that the gaps are enumerable.

### 2. `changed --api` — a breaking-change gate that names who breaks

Builds the public API surface at a base ref (in a throwaway detached worktree)
and in the working tree, then reports what was **removed** or whose
**signature changed** — each with the callers it breaks.

```sh
$ ctx changed --api --since HEAD~1
# API changes vs HEAD~1
0 removed, 4 changed, 7 added.

## Changed signature — potentially breaking
- query::coverage_report  [fn]  (src/query.rs:1254)
    was: pub fn coverage_report(g: &Graph, unsupported: &[(String, usize)], json_out: bool) -> String
    now: pub fn coverage_report(g: &Graph, unsupported: &[(String, usize)], explain: bool, json_out: bool) -> String
    callers (5): mcp::dispatch, query::tests::coverage_separates_internal_external_and_blind_spots, …
- model::Receiver  [enum]  (src/model.rs:186)
    was: pub enum Receiver { Free | SelfType | Unknown }
    now: pub enum Receiver { Free | SelfType | SelfField | Typed | Dyn | Unknown }
```

This isn't a context tool. It's CI. Drop it in a pre-push hook or a pipeline
step and "did I break the API?" becomes one command with an exit code.

### 3. `parity` — cross-language port fidelity

Porting Python to Rust, or TypeScript to Python? `parity` answers "is this port
a faithful structural copy?" Because the item model is language-neutral, a
source module and its port are two renderings of one skeleton.

```sh
$ ctx parity research/gate.py src/gate.rs --aliases py-rust
# ctx parity — source → port
source 6 members · target 5 · aligned 5 (83% of source) · 1 via alias

## Missing in port (1) — in source, no counterpart in target
  fn   Gate.record                  gate.py:18

## Arity drift (1)
  fn   Gate.score               source=2 → port=1   gate.py:7

## Aligned via alias (1) — matched through a rename rule, not exactly
  fn   Gate.__init__  →  Gate.new   (via init → new)

parity: 5/6 source aligned · 1 missing · 1 arity · 0 call · 0 moved
```

A dropped method, a dropped parameter, and the `__init__`→`new` rename, in one
command. `--strict` exits non-zero on any missing member. I'm not aware of
another tool that does this. It might be more or less useful depending on your
specific languages and abstractions. 

## "How is this different from aider's repo map?"

Aider's repo map — and the MCP servers that repackage it, like RepoMapper —
compresses a repository into a **ranked blob** that gets injected at the start
of a session, sized to a token budget. It's a good answer to "what is this
codebase" for a model that has seen none of it.

`ctx` answers a different question. Once the agent is *in* a task, it doesn't
need the repo ranked — it needs specific facts: `trace`, `path`, `callers`,
`context`, `changed --api`, `parity`. Those are queries mid-task, not a blob at
boot. Ranking is one small command here (`ctx core`), not the product.

The nearer comparison is **Serena**, which does LSP-backed symbol navigation.
Against it, `ctx` trades semantic precision for two things:

| | `ctx` | LSP-based (Serena) |
|---|---|---|
| setup | one binary, grammars compiled in | a language server per language, configured and running |
| startup | ~100 ms graph build, per query | server warm-up, project indexing |
| determinism | pure function of the source tree | depends on server state, versions, build artifacts |
| uncertainty | marked per edge (`~`, `*`) + a miss census | resolved or absent, silently |
| coverage | 4 languages (today), one graph across all of them | as many as you install servers for |

If you need type-perfect resolution inside one language, use a LSP. If you
want the same graph across a polyglot repo with nothing to install and answers
that state their own confidence, that's this.

`ctx` does ship the boot map too (`ctx map`), so you can have it if you want it.
But we'll say plainly what dogfooding taught us: it's the least useful thing
here. [More on why](#maps-when-you-want-them).

## Install

Requires a Rust toolchain (1.85+).

```sh
cargo install --git https://github.com/Eagle-Logic/context

# or from a clone
git clone https://github.com/Eagle-Logic/context
cd context && cargo install --path .
```

Single binary, `ctx`. No runtime dependencies — the tree-sitter grammars are
compiled in.

## The queries

```sh
# Everything needed to edit a symbol, in one call (def + types + callees + callers)
ctx context streamChat --max-tokens 4000

# Who calls this? (resolved reverse call edges — the blast radius)
ctx callers basename

# How does execution get here? (transitive call tree, not one hop)
ctx trace decode_step --depth 4
ctx trace decode_step --reverse       # everything that reaches it

# The shortest call path between two symbols, hop by hop
ctx path main flush_kv_cache

# Where is a symbol defined? (jump-to-def without knowing the module)
ctx def SteerConfig
ctx def 'Type::method'                # qualified to disambiguate

# Breaking-change gate: public API removed/changed since a ref + who breaks
ctx changed --api --since main

# Impact map of your current diff: changed modules + deps + callers
ctx changed                           # working tree vs HEAD
ctx changed --since main

# Structural diff between two refs (review a whole branch/PR)
ctx diff main..feature                # changed modules + who they break
ctx diff main..feature --api          # breaking API changes across the range

# Cross-language parity: is the Rust port faithful to the Python source?
ctx parity research/gate.py src/gate.rs
ctx parity research/gate.py src/gate.rs --aliases py-rust   # bridge __init__→new
ctx parity research/gate.py src/gate.rs --strict            # non-zero if missing

# Coverage report: internal call-graph recall + exactly what went unpinned
ctx doctor
ctx doctor --explain                  # full per-name census

# The modules that matter most (dependency centrality; --churn weights by volatility)
ctx core
ctx core --churn

# Boot maps, when you do want the blob
ctx map -o CODEBASE_MAP.md            # full map
ctx map --view skeleton               # architecture only, cheapest
ctx map --max-tokens 8000             # richest view that fits; never truncates
ctx subtree core::inference           # one module + upstream + downstream
ctx modules                           # module list + dep edges only

# Machine-readable
ctx map --format json

# Print the recommended CLAUDE.md discovery-protocol block
ctx snippet >> CLAUDE.md
```

Every command except `parity` takes a repo path as its last positional argument,
defaulting to `.`; `parity` takes a source and one or more targets instead.

### Execution tracing

`callers` answers "who calls this" for exactly one hop. `trace` walks the call
graph transitively — forward (what runs underneath a symbol) or `--reverse`
(everything that reaches it) — cutting cycles and repeated subtrees with a
marker instead of expanding forever. `path <from> <to>` gives the shortest route
between two symbols:

```
$ ctx path main coverage_report
# path: main → coverage_report  (5 hop(s))
~ heuristic edge (verify) · * one branch of a dispatch fan-out

crate::main  [src/main.rs:274]
  → mcp::run  [src/mcp.rs:14]
    → mcp::handle_method  [src/mcp.rs:44]
      → mcp::tools_call  [src/mcp.rs:132]
        → mcp::dispatch  [src/mcp.rs:146]
          → query::coverage_report  [src/query.rs:1254]
```

"How does a request get from `main` to here" is one command rather than a grep
chain. Both mark each hop's confidence and report call edges that leave the
resolved graph, so a trace never quietly implies more certainty than it has.

### Per-answer completeness

`callers`, `context`, `trace`, and `path` each close with a completeness line
for the specific symbol asked about: whether any call site bearing that name
went unresolved, and if so where. An agent shouldn't have to run `doctor` to
learn that the one symbol it cares about is among the misses — or, more
usefully, that it isn't, and the confirming grep can be skipped.

### Naming

`def`, `callers`, `context`, `trace`, and `path` all accept a bare name
(`to_config`) or a qualified one (`SteerOverride::to_config`, `pkg.mod.fn`); a
bare name lists every match so overloads are disambiguated by module +
signature. `callers` is the inverse of the per-function `→ callee` edges in
`map`/`subtree`: it reports only *resolved* call sites, so it's precise where a
text grep floods on a common method name. `subtree` accepts a full module name
(`core::inference`, `pkg.utils.validation`) or any trailing suffix
(`inference`).

### More on the flagship commands

`context` is the agent-native one: a single call returns the definition, the
type definitions referenced in its signature, its callees, and its callers —
trimmed to `--max-tokens` — so an agent gathers full editing context for a
symbol without a map→def→callers→subtree dance.

`changed` turns a diff into an impact map: it runs `git diff` (working tree vs
HEAD, or vs `--since <ref>`, including untracked files), maps changed files onto
modules, and renders those modules plus their upstream dependencies and — the
point — their **downstream callers**, i.e. everything a review should re-check.
An unborn HEAD or a clean tree is handled without error.

`diff <A>..<B>` is `changed` between two arbitrary refs — the whole-branch /
whole-PR view. It maps the file changes onto **B's** module graph (built in a
throwaway worktree when B is a ref; the working tree when B is omitted, as in
`ctx diff main`), so the topology reflects the target state. `--api` runs the
breaking-change surface diff across the same range. `A...B` is accepted and
treated like `A..B`.

`parity` flattens each side into a bag of members keyed by
`(container, canonical-name, role)` — collapsing `camelCase`/`snake_case`/
`PascalCase` and treating a Rust `impl Type { fn m }` the same as a Python
`class Type: def m` — then reports members **missing** from the port, **arity
drift** (receiver-excluded param count), dropped **internal calls**, **moves**
(name matches, container differs), and **additions**. Multiple targets are
compared as a union (a Python file that split into several Rust modules). It's
deterministic and **structure-only** — never semantics — and it leans on the
port preserving names, so it's a faithfulness check for mechanical ports, not a
similarity score for rewrites.

Because it leans on names, `canon()` alone bridges most Python↔Rust dunder
renames for free (it strips the underscores, so `__len__`↔`len`, `__eq__`↔`eq`,
`__hash__`↔`hash`, `__next__`↔`next` already align). For the renames it can't —
chiefly `__init__`→`new`, plus `__str__`→`fmt`/`to_string`,
`__getitem__`→`index`, `__iter__`→`into_iter` — pass `--aliases py-rust`. Every
alias-based match is reported in its own **Aligned via alias** section, never
silently folded, so the fuzz you opted into stays visible.

`core` ranks modules by dependency centrality — PageRank over the module graph,
so the modules everything else leans on float to the top. It's the "where's the
heart of this codebase" answer for an unfamiliar repo. This is the one command
that overlaps what other repo-map tools do, and it's deliberately a small part
of the surface.

## Maps, when you want them

**In our own use, `map` is the least useful command here — including with
`--max-tokens`.** It ships because it's occasionally the right tool, not because
it's the point, and it's listed last for a reason.

The problem isn't the output; it's the shape of the transaction. A map is
breadth paid for up front, before you know which part you'll need, and it's
stale the moment you edit. In practice an agent spends that budget once and then
still runs `ctx context` on the one symbol it actually touches — which would have
answered the question on its own, current at the moment it was asked. Budget
fitting bounds the cost but doesn't change the economics: a cheaper blob is
still a blob.

Where it does earn its place: a genuine cold start on an unfamiliar repo, a
committed `CODEBASE_MAP.md` for humans, or feeding another tool via `--format
json`. For everything else, reach for `context`, `trace`, `callers`, or
`subtree` — a scoped answer beats a ranked summary, which is the whole argument
of this README.

`map` and `subtree` take `--view` to scale detail to informational need — each
level adds a whole category, so token spend buys precision, not noise:

| view | contents | a 419-module Rust workspace |
|---|---|---|
| `skeleton` | modules, deps, re-exports, type names | 79 KB (~19k tok) |
| `interface` | + public signatures, struct fields, enum variants | 301 KB (~75k tok) |
| `full` (default) | + private items and call edges | 451 KB (~112k tok) |

On a mid-size repo (70 files) skeleton is ~3k tokens, which is cheap enough that
the cost isn't the objection — the objection is that it's the wrong shape.
Rust visibility is `pub`-based; Python uses the
underscore convention (dunders like `__init__` count as public); TypeScript uses
the `export` keyword (public class methods are interface, `private`/`#` members
are dropped). Trait methods and trait impls are always interface.

`map --max-tokens <N>` fits the output to a budget: it emits the richest view at
or below `--view` whose (~`len/4`) token count fits, and reports the view it
chose on stderr. It **never truncates** — if even `skeleton` is over budget it
emits the whole skeleton and says so — so the map an agent receives is always
structurally complete.

### Output shape

```markdown
## prelude  (src/prelude.rs)
deps: core::inference
reexports: core::inference::Engine, core::inference::build as make_engine

## agent  (src/agent.rs)
deps: encode
- pub struct Tracked { pub id: usize, pub sig: Vec<f32>, pub pos: (f32, f32) }  [L64]
- pub trait Environment  [L54]
  - fn reset(&mut self) -> Percept  [L55]
  - fn step(&mut self, action: Action) -> Percept  [L56]
- impl Agent  [L192]
  - pub fn observe(&mut self, action: Option<Action>, p: &Percept)  [L204] → Agent::register_type, dist, encode::cosine
  - pub fn drive(&mut self, env: &mut dyn Environment)  [L221] → world::Grid::step*, sim::Replay::step*
```

Two markers qualify an edge. A trailing `~` means the target was inferred from a
receiver whose type is not written in the source — trust it less. A trailing `*`
means the edge is one branch of a dynamic-dispatch fan-out (`dyn Trait`, `impl
Trait`, a bounded generic, a TS `interface`, a Python base class): every
implementation that could run is listed, and exactly one of them does.

A 71-file Rust repo renders to ~65 KB (~16k tokens) in under half a second — the
entire architecture fits in one context window with room to spare.

Each item also carries the first line of its doc comment (Rust `///`, Python
docstring) as a trailing `— summary`, so a signature map doubles as a labeled
one at negligible token cost:

```markdown
- pub fn to_config(&self, n_predict: i32) -> SteerConfig  [L317] → NativeSteerInstruction::to_config  — Build a SteerConfig from the explicit knobs (gate bypassed).
```

## Agent integration

### MCP server

`ctx mcp` runs a minimal MCP server over stdio (newline-delimited JSON-RPC, no
dependencies) exposing the read-only commands as typed tools — `map`, `modules`,
`subtree`, `def`, `callers`, `context`, `trace`, `path`, `core`, `doctor` — so an
agent calls them structured, without shelling out or a permission prompt.

```sh
claude mcp add ctx -- ctx mcp
```

Each tool takes a `path` argument (default `.`); `def`/`callers`/`context` also
take `name`, and `subtree` takes `module`. The server builds the graph per call
(~100 ms), so results are always current.

### Claude Code

`ctx snippet` prints a ready-made "Codebase Discovery Tools" block — append it to
the target repo's `CLAUDE.md` (`ctx snippet >> CLAUDE.md`). It teaches the agent
a query-first protocol: run `ctx context <name>` when you're about to touch a
symbol, `ctx callers` before changing a signature, `ctx trace`/`ctx path` to
follow control flow instead of grepping, and only read raw source for
implementation bodies. Reaching for a whole-repo map is the exception, not the
opening move.

If you do want a committed `CODEBASE_MAP.md` for human readers, regenerate it
from a git pre-commit hook or a Claude Code hook (`ctx map . -o
CODEBASE_MAP.md`); generation is ~100 ms, so freshness is free.

## Markdown as a graph

A docs tree is a graph too — and unlike JSON/XML (which are trees, no
cross-references), Markdown has both halves of ctx's model: **headings are the
nested items** and **links are the edges**. So the same commands answer doc
questions:

- `ctx map docs/ --view skeleton` — the heading outline of the whole corpus,
  each section labelled with its first sentence.
- `ctx subtree <doc>` — a doc plus what it links to and, crucially, its
  **backlinks** (what links *to* it).
- `ctx def <heading-slug>` — jump to a heading; `ctx callers <slug>` — inbound
  links to that section.
- `ctx core docs/` — the most-linked-to docs (your canonical / index pages).

Files are modules (`README.md`/`index.md` collapse to their directory like an
index file); links resolve relative to the linking file (`./x.md#section`,
`../y.md`, `[[WikiPage]]`, and reference-style `[text][ref]` / `[ref]`) against
GitHub-style heading slugs. A link whose file or heading doesn't exist is a
**broken link**; external URLs are left external. `ctx doctor` lists every broken
link (file:line → target) in its own section — an out-of-scope `../` link is only
flagged when its target is genuinely absent from disk, not merely outside the
scanned subtree. Prose→code resolution is not yet modelled.

## Design notes

- **Parsing**: tree-sitter (`tree-sitter-rust`, `tree-sitter-python`,
  `tree-sitter-typescript`); Markdown is parsed directly. Bodies are dropped by
  slicing each definition node up to its `body` field; struct fields and enum
  variants are kept inline because they carry architectural signal at negligible
  token cost.
- **Module names** are path-derived: `src/` components are dropped,
  `mod.rs`/`lib.rs`/`main.rs`/`__init__.py`/`index.ts`/`index.tsx` collapse into
  their directory. TypeScript import specifiers are resolved as paths — relative
  `./x`/`../y` against the importing file's directory, bare names (`react`,
  `@/…` aliases) as external. In workspaces, components before `src/` become the
  crate prefix (`crates/foo/src/bar.rs` → `crates::foo::bar`), and `crate::`
  paths resolve against that prefix.
- **Edge resolution** is heuristic but deterministic: `use`/`import` paths are
  expanded (brace groups, aliases, globs), normalized (`crate`/`self`/`super`,
  leading dots in Python), and prefix-matched against the module index, longest
  name first. Unresolvable crate-relative paths are dropped rather than
  misreported; `std`/`core`/`alloc` are suppressed.
- **Re-export chasing**: an import that lands on a facade (Rust `pub use` in a
  prelude/lib, Python `__init__.py` imports) is chased through the binding —
  including aliases and glob re-exports — so the dep edge points at the module
  that actually defines the symbol, not the facade. Facade modules render their
  own `reexports:` line, and a symbol that can't be proven through a glob keeps
  its edge on the facade rather than guessing.
- **Call-graph edges**: every call site inside a function body is collected and
  resolved through the same machinery as imports (bindings, aliases,
  `crate`/`super`/dots, re-export chase), then rendered on the function's line
  (`→ callee, ...`) and folded into module `deps:` — which also catches
  fully-qualified calls (`crate::foo::bar()`) that have no `use`. Receiver method
  calls resolve in confidence order: a `self`/`this` receiver against the
  enclosing impl/class; a receiver whose type is **written in the source**
  (parameter annotation, `let`/`const` binding, constructor call, or declared
  field/property type) against that type; and only then, for a receiver with no
  type at all, through a global method index **when the name is unique
  codebase-wide and not a ubiquitous std method** (`push`, `get`, `items`, …).
  Ambiguous or unresolvable calls are dropped, never guessed.
- **Dynamic dispatch**: a call through a trait object, `impl Trait`, a bounded
  generic, `<T as Trait>::f`, a TypeScript `interface`/`implements`, or a Python
  base class fans out to **every implementation that defines the method**, each
  edge marked `*`. Exactly one branch runs at a time, so this is an
  over-approximation — but a visible over-approximation beats a dropped edge when
  the question is "what could run here". The declaring abstraction is a target
  only when nothing overrides the method, i.e. when its default body is what
  actually executes. Fan-outs wider than 12 collapse to the abstraction with an
  `[N impls]` annotation.
- **Nested callables**: functions declared inside a function body, `let`-bound
  closures, and types declared inside a body are all lifted to child items. Their
  calls are attributed to them rather than smeared onto the enclosing function,
  and calls *to* them resolve lexically (`outer::helper`).
- **Edge confidence**: an edge backed by a resolved import, path, `self`/`Self`
  receiver, or a declared receiver type is trusted. An edge attributed from an
  *opaque* receiver (`expr.method()`, where nothing in the source says the type)
  is a heuristic guess, marked with a trailing `~` in
  `map`/`subtree`/`callers`/`trace` (and `heuristic: true` in JSON). A dispatch
  branch is marked `*` (`dispatch: true` in JSON). This surfaces exactly the
  calls a type-blind resolver can get wrong, so an unmarked edge can be relied on
  and a marked one invites a glance at the source.
- **Not captured (yet)**: receivers whose type comes from an expression ctx does
  not evaluate — `for` bindings, iterator chains, and results of calls that are
  not associated constructors — still fall back to the unique-name heuristic.
  `doctor` names every such miss rather than hiding it.

## Adding a language

Everything downstream of extraction — resolution, the call graph, `core`,
`parity`, `diff`, the MCP server — consumes one language-neutral struct and
never asks what produced it. So a new language is one extractor file plus a
grammar crate, wired in at a handful of places the compiler will point you at.

**1. The contract.** Add `src/extract/<lang>.rs` exposing:

```rust
pub fn extract(src: &str) -> Result<FileFacts>
```

`FileFacts` (`src/model.rs`) is four fields: `items`, `imports`, `reexports`,
`defined`. The work is in populating `Item` — and specifically in `raw_calls`,
where each `RawCall` carries a `Receiver` (`Free`, `SelfType`, `SelfField`,
`Typed`, `Dyn`, `Unknown`). **`Receiver` is where edge confidence comes from.**
An extractor that returns `Unknown` everywhere compiles and runs, and produces a
map in which every edge is marked `~`. Getting `Typed` and `Dyn` right is most of
the value. `src/extract/python.rs` is the smallest complete example and the
template worth copying; note that `TypeEnv` (local-binding type tracking) is
deliberately per-language, not shared, so that part is written fresh each time.

**2. Add the `Lang` variant**, then run `cargo check`. Seven exhaustive matches
will fail to compile, and each is a real decision:

| site | decision |
|---|---|
| `extract/mod.rs` dispatch | call your `extract()` |
| `extract/mod.rs` stem collapse | which filename collapses to its directory (`mod`/`lib`/`main`, `__init__`, `index`) |
| `extract/mod.rs` root name | the name of the root module (`crate` vs `root`) |
| `extract/mod.rs` `candidates()` | import string → absolute module segments — by name (Rust/Python) or by path (TypeScript) |
| `model.rs` `Lang::name()` | display string |
| `model.rs` `Lang::sep()` | path separator (`::` vs `.`) |
| `view.rs` visibility | what "public" means: a keyword, a naming convention, an `export` |

Only `candidates()` and the visibility rule are real design work.

**3. Four places the compiler will _not_ catch** — each has a `_` fallback, so
missing one fails silently rather than loudly:

- the extension allowlist in `build_graph` — miss it and your files are never
  walked, and the map is simply empty
- the extension → `Lang` mapping (`_ => continue`)
- `is_package` (`_ => false`)
- `UNSUPPORTED_SOURCE_EXTS` — remove your extension, or `ctx doctor` keeps
  reporting the language as a blind spot after you've added it

Grammars are compiled into the binary, so a new grammar crate is a real
binary-size decision, not just a dependency.

## Contributing

Issues and pull requests are welcome. `cargo test` covers the extractors, the
resolver, and every query command; `cargo clippy` should stay clean.

Two properties are load-bearing, and a change that breaks either needs a good
reason:

- **Determinism.** The map is a pure function of the source tree. No clocks, no
  hash-order iteration, no network.
- **No guessing.** An edge is proven, marked heuristic with `~`, marked as a
  dispatch branch with `*`, or absent. A dropped edge is reported by name in
  `ctx doctor`, never silently swallowed — the honest bit is not that coverage is
  high, it is that the gaps are enumerable. A change that raises coverage by
  guessing, or that reclassifies a miss as "external" without evidence that the
  name is defined nowhere in the tree, is a regression.

`ctx` is used against its own source, so `ctx doctor .` and `ctx diff main` are a
reasonable first review of any patch. Watch internal recall and the miss census
rather than the raw edge count: more edges at the cost of more `~` is not an
improvement.

## License

MIT — see [LICENSE](LICENSE).
