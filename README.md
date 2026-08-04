# context (`ctx`)

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Deterministic AST skeleton maps of a codebase, built for ultra-dense context
injection into coding agents (Claude Code in particular).

`ctx` parses Rust, Python, TypeScript/TSX, and Markdown sources, strips every
implementation body, and emits a compact, deterministic topology map:

- **Nodes** — modules, structs/enums/traits/impls, classes, functions
  (signature + file + line).
- **Edges** — resolved internal import edges (`deps:`), external
  crates/packages (`extern:`), and per-function call edges you can walk
  transitively (`ctx trace`, `ctx path`).

No embeddings, no fuzzy retrieval. The map is a pure function of the source
tree: same code in, same map out.

## Install

Requires a Rust toolchain (1.85+).

```sh
cargo install --git https://github.com/Eagle-Logic/context

# or from a clone
git clone https://github.com/Eagle-Logic/context
cd context && cargo install --path .
```

This installs a single binary, `ctx`. No runtime dependencies — the
tree-sitter grammars are compiled in.

## Usage

```sh
# Strategy A: full map for session boot (small/medium repos)
ctx map ~/projects/myrepo -o CODEBASE_MAP.md

# Strategy B: pruned slice for large repos — one module plus its
# immediate upstream (dependencies) and downstream (dependents)
ctx subtree core::inference ~/projects/myrepo

# Global high-level view: module list + dependency edges only
ctx modules ~/projects/myrepo

# Where is a symbol defined? (jump-to-def without knowing the module)
ctx def SteerConfig ~/projects/myrepo
ctx def 'Type::method' ~/projects/myrepo   # qualified to disambiguate

# Who calls this? (resolved reverse call edges — the blast radius)
ctx callers basename ~/projects/myrepo

# How does execution get here? (transitive call tree, not one hop)
ctx trace decode_step ~/projects/myrepo --depth 4
ctx trace decode_step ~/projects/myrepo --reverse    # everything that reaches it

# The shortest call path between two symbols, hop by hop
ctx path main flush_kv_cache ~/projects/myrepo

# Everything needed to edit a symbol, in one call (def + types + callees + callers)
ctx context streamChat ~/projects/myrepo --max-tokens 4000

# The modules that matter most (dependency centrality; --churn weights by volatility)
ctx core ~/projects/myrepo
ctx core ~/projects/myrepo --churn

# Breaking-change check: public API removed/changed since a ref + who breaks
ctx changed --api ~/projects/myrepo --since main

# Coverage report: internal call-graph recall + exactly what went unpinned
ctx doctor ~/projects/myrepo
ctx doctor ~/projects/myrepo --explain   # full per-name census

# Impact map of your current diff: changed modules + deps + callers
ctx changed ~/projects/myrepo              # working tree vs HEAD
ctx changed ~/projects/myrepo --since main # vs a ref/branch

# Structural diff between two refs (review a whole branch/PR)
ctx diff main..feature ~/projects/myrepo        # changed modules + who they break
ctx diff main..feature ~/projects/myrepo --api  # breaking API changes across the range

# Fit a map to a token budget (richest view that fits; never truncates)
ctx map ~/projects/myrepo --max-tokens 8000

# Cross-language parity: is the Rust port faithful to the Python source?
ctx parity research/gate.py src/gate.rs                    # missing / arity drift / dropped calls
ctx parity research/gate.py src/gate.rs --aliases py-rust  # bridge __init__→new etc.
ctx parity research/gate.py src/gate.rs --strict           # exit non-zero if anything is missing

# Machine-readable graph
ctx map ~/projects/myrepo --format json

# Print the recommended CLAUDE.md discovery-protocol block
ctx snippet >> ~/projects/myrepo/CLAUDE.md
```

`core` ranks modules by dependency centrality — PageRank over the module
graph, so the modules everything else leans on float to the top. It's the
"where's the heart of this codebase" answer for an unfamiliar repo, computed
deterministically rather than guessed.

`changed --api` is a pre-merge safety gate: it builds the public API surface
both at a base ref (in a throwaway detached worktree) and in the working
tree, then reports the public items that were **removed** or whose
**signature changed** — each with the callers it breaks — plus additions as
non-breaking. Turns "did I break the API?" into a one-command check for CI or
a pre-push hook.

`context` is the agent-native command: one call returns the definition, the
type definitions referenced in its signature, its callees, and its callers —
trimmed to `--max-tokens` — so an agent can gather the full editing context
for a symbol without a map→def→callers→subtree dance.

`trace` and `path` are the execution-tracing commands. `callers` answers "who
calls this" for exactly one hop; `trace` walks the call graph transitively —
forward (what runs underneath a symbol) or `--reverse` (everything that reaches
it) — cutting cycles and repeated subtrees with a marker instead of expanding
forever. `path <from> <to>` gives the shortest route between two symbols, so
"how does a request get from `main` to here" is one command rather than a
grep chain. Both mark each hop's confidence (`~` receiver-inferred, `*` one
branch of a dispatch fan-out) and report call edges that leave the resolved
graph, so a trace never quietly implies more certainty than it has.

`doctor` is a coverage/blind-spot report — deliberately honest about what `ctx`
does *not* model. Its headline is **internal recall**: resolved edges over the
call sites that could have been internal at all. A call into `std` or a
third-party crate is excluded from the denominator, because no internal edge
could exist for it however good the resolver gets — classification is by
evidence (is this name defined anywhere under the root?), not by a hardcoded
list. The report then names **every callee it could not pin**, with counts, so
"87% recall" comes with the exact grep list for the other 13%. `--explain`
adds the full per-name census including what was ruled external. Run it once
on a new repo to calibrate how much to trust the map.

`callers` and `context` each end with a completeness line for the specific
symbol asked about: whether any call site bearing that name went unresolved,
and if so where. An agent should not have to run `doctor` to learn that the one
symbol it cares about is among the misses — or, more usefully, that it isn't,
and the confirming grep can be skipped.

`changed` turns a diff into an impact map: it runs `git diff` (working tree
vs HEAD, or vs `--since <ref>`, including untracked files), maps the changed
files onto modules, and renders those modules plus their upstream
dependencies and — the point — their **downstream callers**, i.e. everything
a review should re-check. An unborn HEAD or a clean tree is handled without
error.

`diff <A>..<B>` is `changed` between two arbitrary refs — the whole-branch /
whole-PR view. It maps the file changes onto **B's** module graph (built in a
throwaway worktree when B is a ref; the working tree when B is omitted, as in
`ctx diff main`), so the topology reflects the target state. `--api` runs the
breaking-change surface diff across the same range. `A...B` is accepted and
treated like `A..B`.

`map --max-tokens <N>` fits the output to a budget: it emits the richest view
at or below `--view` whose (~`len/4`) token count fits, and reports the view
it chose on stderr. It **never truncates** — if even `skeleton` is over
budget it emits the whole skeleton and says so — so the map an agent receives
is always structurally complete.

`parity <source> <port>...` answers "is this port a faithful structural copy?"
across languages. Because the `Item` model is language-neutral, a source
module and its port are two renderings of one skeleton; parity flattens each
into a bag of members keyed by `(container, canonical-name, role)` — collapsing
`camelCase`/`snake_case`/`PascalCase` and treating a Rust `impl Type { fn m }`
the same as a Python `class Type: def m` — then reports what **diverged**:
members **missing** from the port, **arity drift** (receiver-excluded param
count), dropped **internal calls**, **moves** (name matches, container
differs), and **additions**. Multiple targets are compared as a union (a
Python file that split into several Rust modules). `--strict` exits non-zero
on any missing member, for CI. It is deterministic and **structure-only** —
never semantics — and it leans on the port preserving names, so it is a
faithfulness check for mechanical ports, not a similarity score for rewrites.

Because it leans on names, `canon()` alone bridges most Python↔Rust dunder
renames for free (it strips the underscores, so `__len__`↔`len`, `__eq__`↔`eq`,
`__hash__`↔`hash`, `__next__`↔`next` already align). For the renames it can't —
chiefly `__init__`→`new`, plus `__str__`→`fmt`/`to_string`, `__getitem__`→
`index`, `__iter__`→`into_iter` — pass `--aliases py-rust`. Every alias-based
match is reported in its own **Aligned via alias** section (`__init__ → new
(via init → new)`), never silently folded — so the fuzz you opted into is
always visible.

`def`, `callers`, `context`, `trace`, and `path` all accept a bare name
(`to_config`) or a qualified name (`SteerOverride::to_config`, `pkg.mod.fn`);
a bare name lists every match so overloads are disambiguated by module +
signature. `callers` is the inverse of the per-function `→ callee` edges in
`map`/`subtree`: it reports only *resolved* call sites, so it is precise where
a text grep floods on a common method name. `trace` and `path` walk those same
edges transitively. All three close with a completeness line stating whether
any call site bearing that name went unresolved, so the answer's limits travel
with the answer.

`subtree` accepts a full module name (`core::inference`, `pkg.utils.validation`)
or any trailing suffix (`inference`).

## Detail views (the budget ladder)

`map` and `subtree` take `--view` to scale detail to informational need —
each level adds a whole category, so token spend buys precision, not noise:

| view | contents | a 419-module Rust workspace |
|---|---|---|
| `skeleton` | modules, deps, re-exports, type names | 79 KB (~19k tok) |
| `interface` | + public signatures, struct fields, enum variants | 301 KB (~75k tok) |
| `full` (default) | + private items and call edges | 451 KB (~112k tok) |

On a mid-size repo (70 files) skeleton is ~3k tokens — cheap enough for the
first turn of every session. Rust visibility is `pub`-based; Python uses the
underscore convention (dunders like `__init__` count as public); TypeScript
uses the `export` keyword (public class methods are interface, `private`/`#`
members are dropped). Trait methods and trait impls are always interface.

## Output shape

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

Two markers qualify an edge. A trailing `~` means the target was inferred from
a receiver whose type is not written in the source — trust it less. A trailing
`*` means the edge is one branch of a dynamic-dispatch fan-out (`dyn Trait`,
`impl Trait`, a bounded generic, a TS `interface`, a Python base class): every
implementation that could run is listed, and exactly one of them does.

A 71-file Rust repo renders to ~65 KB (~16k tokens) in under half a second —
the entire architecture fits in one context window with room to spare.

Each item also carries the first line of its doc comment (Rust `///`, Python
docstring) as a trailing `— summary`, so a signature map doubles as a labeled
one at negligible token cost:

```markdown
- pub fn to_config(&self, n_predict: i32) -> SteerConfig  [L317] → NativeSteerInstruction::to_config  — Build a SteerConfig from the explicit knobs (gate bypassed).
```

## Claude Code integration

`ctx snippet` prints a ready-made "Codebase Discovery Tools" block — append it
to the target repo's `CLAUDE.md` (`ctx snippet >> CLAUDE.md`). It teaches the
agent a lookup protocol: boot with `ctx map --view skeleton`, pull
`ctx subtree <module>` before touching a module, follow call edges instead of
grepping, and only read raw source for implementation bodies.

To keep a committed `CODEBASE_MAP.md` fresh, regenerate it from a git
pre-commit hook or a Claude Code hook (`ctx map . -o CODEBASE_MAP.md`) — or
skip the file entirely and have sessions run `ctx map` at boot; generation is
~100 ms, so freshness is free.

### Markdown as a graph

A docs tree is a graph too — and unlike JSON/XML (which are trees, no
cross-references), Markdown has both halves of ctx's model: **headings are
the nested items** and **links are the edges**. So the same commands answer
doc questions:

- `ctx map docs/ --view skeleton` — the heading outline of the whole corpus,
  each section labelled with its first sentence.
- `ctx subtree <doc>` — a doc plus what it links to and, crucially, its
  **backlinks** (what links *to* it).
- `ctx def <heading-slug>` — jump to a heading; `ctx callers <slug>` — inbound
  links to that section.
- `ctx core docs/` — the most-linked-to docs (your canonical / index pages).

Files are modules (`README.md`/`index.md` collapse to their directory like an
index file); links resolve relative to the linking file (`./x.md#section`,
`../y.md`, `[[WikiPage]]`, and reference-style `[text][ref]` / `[ref]`)
against GitHub-style heading slugs. A link whose file or heading doesn't exist
is a **broken link**; external URLs are left external. `ctx doctor` lists every
broken link (file:line → target) in its own section — an out-of-scope `../`
link is only flagged when its target is genuinely absent from disk, not merely
outside the scanned subtree. Prose→code resolution is not yet modelled.

### MCP server

`ctx mcp` runs a minimal MCP server over stdio (newline-delimited JSON-RPC,
no dependencies) exposing the read-only commands as typed tools — `map`,
`modules`, `subtree`, `def`, `callers`, `context`, `trace`, `path`, `core`,
`doctor` — so an agent calls them structured, without shelling out or a
permission prompt.
Register it once with Claude Code:

```sh
claude mcp add ctx -- ctx mcp
```

Each tool takes a `path` argument (default `.`); `def`/`callers`/`context`
also take `name`, and `subtree` takes `module`. The server builds the graph
per call (~100 ms), so results are always current.

## Design notes

- **Parsing**: tree-sitter (`tree-sitter-rust`, `tree-sitter-python`,
  `tree-sitter-typescript`); Markdown is parsed directly. Bodies
  are dropped by slicing each definition node up to its `body` field; struct
  fields and enum variants are kept inline because they carry architectural
  signal at negligible token cost.
- **Module names** are path-derived: `src/` components are dropped,
  `mod.rs`/`lib.rs`/`main.rs`/`__init__.py`/`index.ts`/`index.tsx` collapse into
  their directory. TypeScript import specifiers are resolved as paths —
  relative `./x`/`../y` against the importing file's directory, bare names
  (`react`, `@/…` aliases) as external. In
  workspaces, components before `src/` become the crate prefix
  (`crates/foo/src/bar.rs` → `crates::foo::bar`), and `crate::` paths resolve
  against that prefix.
- **Edge resolution** is heuristic but deterministic: `use`/`import` paths are
  expanded (brace groups, aliases, globs), normalized (`crate`/`self`/`super`,
  leading dots in Python), and prefix-matched against the module index,
  longest name first. Unresolvable crate-relative paths are dropped rather
  than misreported; `std`/`core`/`alloc` are suppressed.
- **Re-export chasing**: an import that lands on a facade (Rust `pub use`
  in a prelude/lib, Python `__init__.py` imports) is chased through the
  binding — including aliases and glob re-exports — so the dep edge points
  at the module that actually defines the symbol, not the facade. Facade
  modules render their own `reexports:` line, and a symbol that can't be
  proven through a glob keeps its edge on the facade rather than guessing.
- **Call-graph edges**: every call site inside a function body is collected
  and resolved through the same machinery as imports (bindings, aliases,
  `crate`/`super`/dots, re-export chase), then rendered on the function's
  line (`→ callee, ...`) and folded into module `deps:` — which also catches
  fully-qualified calls (`crate::foo::bar()`) that have no `use`. Receiver
  method calls resolve in confidence order: a `self`/`this` receiver against
  the enclosing impl/class; a receiver whose type is **written in the source**
  (parameter annotation, `let`/`const` binding, constructor call, or declared
  field/property type) against that type; and only then, for a receiver with
  no type at all, through a global method index **when the name is unique
  codebase-wide and not a ubiquitous std method** (`push`, `get`, `items`, …).
  Ambiguous or unresolvable calls are dropped, never guessed.
- **Dynamic dispatch**: a call through a trait object, `impl Trait`, a bounded
  generic, `<T as Trait>::f`, a TypeScript `interface`/`implements`, or a
  Python base class fans out to **every implementation that defines the
  method**, each edge marked `*`. Exactly one branch runs at a time, so this
  is an over-approximation — but a visible over-approximation beats a dropped
  edge when the question is "what could run here". The declaring abstraction
  is a target only when nothing overrides the method, i.e. when its default
  body is what actually executes. Fan-outs wider than 12 collapse to the
  abstraction with an `[N impls]` annotation.
- **Nested callables**: functions declared inside a function body, `let`-bound
  closures, and types declared inside a body are all lifted to child items.
  Their calls are attributed to them rather than smeared onto the enclosing
  function, and calls *to* them resolve lexically (`outer::helper`).
- **Edge confidence**: an edge backed by a resolved import, path, `self`/`Self`
  receiver, or a declared receiver type is trusted. An edge attributed from an
  *opaque* receiver (`expr.method()`, where nothing in the source says the
  type) is a heuristic guess, marked with a trailing `~` in
  `map`/`subtree`/`callers`/`trace` (and `heuristic: true` in JSON). A
  dispatch branch is marked `*` (`dispatch: true` in JSON). This surfaces
  exactly the calls a type-blind resolver can get wrong, so an unmarked edge
  can be relied on and a marked one invites a glance at the source.
- **Not captured (yet)**: receivers whose type comes from an expression ctx
  does not evaluate — `for` bindings, iterator chains, and results of calls
  that are not associated constructors — still fall back to the unique-name
  heuristic. `doctor` names every such miss rather than hiding it.

Adding a language = one extractor file producing a `FileFacts` (items,
imports, re-export bindings, defined names) plus the tree-sitter grammar
crate — and, for a language that resolves imports by path rather than by name
(as TypeScript does), a `candidates()` branch that maps a specifier to
absolute module segments.

## Contributing

Issues and pull requests are welcome. `cargo test` covers the extractors, the
resolver, and every query command; `cargo clippy` should stay clean.

Two properties are load-bearing, and a change that breaks either needs a good
reason:

- **Determinism.** The map is a pure function of the source tree. No clocks, no
  hash-order iteration, no network.
- **No guessing.** An edge is proven, marked heuristic with `~`, marked as a
  dispatch branch with `*`, or absent. A dropped edge is reported by name in
  `ctx doctor`, never silently swallowed — the honest bit is not that coverage
  is high, it is that the gaps are enumerable. A change that raises coverage by
  guessing, or that reclassifies a miss as "external" without evidence that the
  name is defined nowhere in the tree, is a regression.

`ctx` is used against its own source, so `ctx doctor .` and `ctx diff main` are
a reasonable first review of any patch. Watch internal recall and the miss
census rather than the raw edge count: more edges at the cost of more `~` is
not an improvement.

## License

MIT — see [LICENSE](LICENSE).
