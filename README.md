# context (`ctx`)

Deterministic AST skeleton maps of a codebase, built for ultra-dense context
injection into coding agents (Claude Code in particular).

`ctx` parses Rust, Python, TypeScript/TSX, and Markdown sources, strips every
implementation body, and emits a compact, deterministic topology map:

- **Nodes** — modules, structs/enums/traits/impls, classes, functions
  (signature + file + line).
- **Edges** — resolved internal import edges (`deps:`) and external
  crates/packages (`extern:`).

No embeddings, no fuzzy retrieval. The map is a pure function of the source
tree: same code in, same map out.

## Usage

```sh
cargo install --path .

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

# Everything needed to edit a symbol, in one call (def + types + callees + callers)
ctx context streamChat ~/projects/myrepo --max-tokens 4000

# The modules that matter most (dependency centrality; --churn weights by volatility)
ctx core ~/projects/myrepo
ctx core ~/projects/myrepo --churn

# Breaking-change check: public API removed/changed since a ref + who breaks
ctx changed --api ~/projects/myrepo --since main

# Coverage / blind-spot report: how much resolved, where to distrust
ctx doctor ~/projects/myrepo

# Impact map of your current diff: changed modules + deps + callers
ctx changed ~/projects/myrepo              # working tree vs HEAD
ctx changed ~/projects/myrepo --since main # vs a ref/branch

# Structural diff between two refs (review a whole branch/PR)
ctx diff main..feature ~/projects/myrepo        # changed modules + who they break
ctx diff main..feature ~/projects/myrepo --api  # breaking API changes across the range

# Fit a map to a token budget (hard cap: reduces detail, then prunes
# least-central modules; never truncates within a kept module)
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

`doctor` is a coverage/blind-spot report — deliberately honest about what `ctx`
does *not* model. It splits every call site into internal edges resolved
(with the heuristic `~` share), ubiquitous std/builtin calls (never edged by
design), and external/unpinned calls, then lists the low-confidence modules
(high `~` ratio) and the source files it can't parse at all (`.cpp`, `.go`,
…). Run it once on a new repo to calibrate how much to trust the map — and to
know exactly when to fall back to grep.

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

`map --max-tokens <N>` fits the output to a budget and is a **hard cap**. It
first reduces detail, emitting the richest view at or below `--view` whose
(~`len/4`) token count fits, and reports the view it chose on stderr. When even
`skeleton` is over budget, detail is exhausted and the only remaining lever is
dropping modules, so the least-central ones are pruned (PageRank, the same
ranking `core` uses) until the map fits. The output states what was omitted, and
dependency edges still name pruned modules — that name is what to feed
`subtree`/`context` next.

Items inside a kept module are never truncated: you get fewer modules, each
still whole. Only when a single module alone exceeds the budget does `ctx` emit
an over-budget map, and it says so on stderr. Without `--max-tokens`, nothing is
ever pruned.

`callers` reports its own recall, and **never claims completeness**. Reverse edges
exist only for calls that resolved, and resolution drops rather than guesses, so
the result is a floor. Two loss channels are knowable and are reported in band:

- **Ambiguous name** — more than one definition, so a call through an opaque
  receiver cannot be pinned to one of them. Prints `INCOMPLETE` with the
  definition count (`ambiguous_name` in JSON). Computed from the bare name, so
  qualifying the query cannot silence it.
- **Suppressed ubiquitous name** — `get`, `open`, `push`, `len` and friends are
  never indexed, including a project-defined method that merely shares the name.
  Prints `NOT INDEXED`, because an empty result there carries no information at
  all (`suppressed_common_name` in JSON).

Both set `lower_bound: true`. The absence of a flag means "no *known* reason to
distrust this" — not a guarantee: a call made at module level, or through a
function-local import, can still be missed. Before changing a signature, run the
`rg` command ctx prints. This matters because `callers` is the
pre-signature-change safety check, and "no callers" reads as "safe to change".

Heuristic method attribution is **language-scoped**. A unique method name is
evidence only within one language; across languages it is coincidence, so
`tok.apply_chat_template(...)` in Python is no longer attributed to a Rust method
of the same name. Before this guard, 45 of 50 reported callers of that name were
artifacts, ~18% of module dep edges were impossible Python->Rust edges, and those
edges distorted `core`'s ranking.

`--exclude '<glob>'` and `--lang` are global, repeatable flags for scoping the
scan. Vendored trees, archived docs and dead code are usually tracked, so
`.gitignore` will not exclude them: `--exclude 'docs/archive/**'`. And because a
docs tree can dominate a *code* map — 78% of one 720-module repo's skeleton view —
`--lang code` restricts the scan to Rust/Python/TypeScript, cutting that map from
~126k to ~27k tokens.

Module names are unique. They derive from paths, so `src/lib.rs` and `src/main.rs`
both want to be `crate`, and a `native/README.md` collides with the
`src/native/mod.rs` it documents. Because resolution indexes by name, a collision
used to make one module unreachable and silently drop its reverse edges. Code now
keeps the bare name, prose is renamed to `name@stem`, and every rename is reported
on stderr.

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

`def` and `callers` accept a bare name (`to_config`) or a qualified name
(`SteerOverride::to_config`, `pkg.mod.fn`); a bare name lists every match so
overloads are disambiguated by module + signature. `callers` is the inverse of
the per-function `→ callee` edges in `map`/`subtree`: it reports only *resolved*
call sites, so it is precise where a text grep floods on a common method name —
though it inherits the resolver's heuristics (a receiver `.m()` call is
attributed to the enclosing impl when the type is unknown).

`subtree` accepts a full module name (`core::inference`, `pkg.utils.validation`)
or any trailing suffix (`inference`).

## Detail views (the budget ladder)

`map` and `subtree` take `--view` to scale detail to informational need —
each level adds a whole category, so token spend buys precision, not noise:

| view | contents | 720-module polyglot repo | same, `--lang code` |
|---|---|---|---|
| `skeleton` | modules, deps, re-exports, type names | 504 KB (~126k tok) | 105 KB (~26k tok) |
| `interface` | + public signatures, struct fields, enum variants | 827 KB (~207k tok) | — |
| `full` (default) | + private items and call edges | 1.03 MB (~258k tok) | — |

Those are the *unbudgeted* sizes, and at that scale none of them belong in a
context window: pass `--max-tokens` (a hard cap) or `--lang code` (77% of that
repo's map is Markdown). On a small repo (16 modules) `full` is ~10k tokens, cheap
enough for the first turn of a session. Sizes scale with the repo, not with the
view alone — measure before reading. Rust visibility is `pub`-based; Python uses the
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
```

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
agent a lookup protocol: orient with `ctx core` or a budgeted
`ctx map --max-tokens`, pull `ctx subtree <module>` before touching a module,
follow call edges instead of grepping, and only read raw source for
implementation bodies.

**Don't commit the map.** A `CODEBASE_MAP.md` is a derived artifact — a pure
function of the source tree, generated in well under a second — so it belongs in
`.gitignore`, and sessions should regenerate it on demand
(`ctx map . -o CODEBASE_MAP.md`). Committing it buys nothing, costs a ~1 MB diff
whenever sources move, and leaves a stale copy lying around that reads as
authoritative.

Gitignoring it is also load-bearing for correctness: ctx honors `.gitignore`, so
an ignored map is excluded from its own parse. A *tracked* map is Markdown in the
tree, so ctx reads it back as a module of its own headings — each run then
describes the previous run's output and generation never reaches a fixed point.
If you must keep one tracked, list it in a `.ignore` file (honored by
ripgrep-family tools, invisible to git) to restore idempotency.

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
`modules`, `subtree`, `def`, `callers`, `context`, `core`, `doctor` — so an
agent calls them structured, without shelling out or a permission prompt.
Register it once with Claude Code:

```sh
claude mcp add ctx -- ctx mcp
```

Each tool takes a `path` argument (default `.`); `def`/`callers`/`context`
also take `name`, and `subtree` takes `module`. The server builds the graph
per call (~100 ms), so results are always current.

## Design notes

- **Parsing**: tree-sitter (`tree-sitter-rust`, `tree-sitter-python`). Bodies
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
  method calls (`.steer()`, `obj.method()`) resolve to the enclosing
  impl/class first, else through a global method index **only when the name
  is unique codebase-wide and not a ubiquitous std method** (`push`, `get`,
  `items`, ...). Ambiguous or unresolvable calls are dropped, never guessed.
- **Edge confidence**: an edge backed by a resolved import, path, or
  `self`/`Self` receiver is trusted. An edge attributed from an *opaque*
  receiver (`expr.method()`, where the type is unknown) — whether matched to
  the enclosing impl or a unique method name — is a heuristic guess and is
  marked with a trailing `~` in `map`/`subtree`/`callers` (and `heuristic:
  true` in JSON). This surfaces exactly the calls a type-blind resolver can
  get wrong, so a `~`-free edge can be relied on and a `~` edge invites a
  glance at the source.
- **Not captured (yet)**: trait-impl resolution (calls through `dyn Trait` /
  generic bounds stay unresolved unless the method name is unique), calls
  inside nested functions are attributed to the enclosing item.

Adding a language = one extractor file producing a `FileFacts` (items,
imports, re-export bindings, defined names) plus the tree-sitter grammar
crate — and, for a language that resolves imports by path rather than by name
(as TypeScript does), a `candidates()` branch that maps a specifier to
absolute module segments.
