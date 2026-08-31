# ctx

[![Crates.io](https://img.shields.io/crates/v/code-context.svg)](https://crates.io/crates/code-context)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

*The binary is `ctx`. The crate is [`code-context`](https://crates.io/crates/code-context)
(`ctx` was taken on crates.io in 2017).*

**A queryable code graph for coding agents.** Not a map you paste at boot — a
set of queries an agent runs mid-task: *who calls this, how does execution get
here, what breaks if I change it, what must a move touch, does my port still
match the original.*

One Rust binary. Tree-sitter for Rust, Python, TypeScript/TSX, and Markdown. No
language servers, no embeddings, no index to warm. The graph is a pure function
of the source tree — same code in, same answer out — and it builds in ~100 ms,
so every query runs against current source.

```sh
$ ctx callers resolve_call
1 caller(s) of 'resolve_call':

extract::Walk::rec  (src/extract/mod.rs:1473)  → resolve_call

completeness: no call site named `resolve_call` went unresolved anywhere in this
tree — this blast radius is complete to the limit of what ctx parses.
```

That last line is the whole idea. The answer travels with its own limits.

**→ [EXAMPLES.md](EXAMPLES.md)** — nine commands run against this repo, verbatim
output, including the parts where `ctx` reports its own limits.

## Four things you won't find elsewhere

### 1. Every edge tells you how much to trust it

Every static analyzer guesses. `ctx` is the one that says where.

An edge backed by a resolved import, a path, a `self` receiver, or a declared
type is unmarked — rely on it. An edge inferred from an *opaque* receiver
(`expr.method()`, where nothing in the source states the type) is marked `~`. A
branch of a dynamic-dispatch fan-out is marked `*`. And `ctx doctor` names
**every callee it could not pin at all**, with counts:

```
## Internal recall — the number to trust
  1058/1100 = 96.2%   of call sites that could be internal, ctx pinned this many.

## What ctx missed (callee names that exist here but went unpinned)
grep these; every other edge in the map is one ctx could prove.
     26  walk
      7  context
      2  path

## Low-confidence zones (edges to distrust — grep to confirm)
  parity                           26% heuristic (10/38 edges)
```

The recall number comes with the exact grep list for everything it doesn't
cover. The denominator is honest too: a call into `std` or a third-party crate
is excluded, because no internal edge could exist for it however good the
resolver gets — and that classification is by evidence (is this name defined
anywhere under the root?), not a hardcoded list.

Heuristic method attribution is also **language-scoped**. A unique method name is
evidence only within one language; across languages it is coincidence, so
`tok.apply_chat_template(...)` in Python is never attributed to a Rust method of
the same name. Before that guard, 45 of 50 reported callers of that name were
artifacts and ~18% of module dep edges were impossible Python→Rust edges — which
then distorted `core`'s ranking.

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
- query::coverage_report  [fn]  (src/query.rs:1420)
    was: pub fn coverage_report(g: &Graph, unsupported: &[(String, usize)], json_out: bool) -> String
    now: pub fn coverage_report(g: &Graph, unsupported: &[(String, usize)], explain: bool, json_out: bool) -> String
    callers (5): mcp::dispatch, query::tests::coverage_separates_internal_external_and_blind_spots, …
```

`--strict` turns it into a CI gate, and fails **only on removals**. That asymmetry
is deliberate. A removal is unambiguous: nothing can call what is gone. A
signature change might be an added optional parameter or a new struct field —
routine, compatible work — and ctx is structural, with no type resolution, so it
cannot tell that from a changed parameter type. Failing on both was tried and
would have blocked ordinary additive commits on the first real repo it met; a gate
that cries wolf gets switched off, which is worse than no gate. So signature
changes are reported, and under `--strict` say explicitly that they were reported
and not failed.

Treat it as a tripwire that surfaces an unintended removal in review, not as a
semver authority — a language-specific tool with a type system
(`cargo-semver-checks`, `apidiff`, `japicmp`) is stricter within its language. The
trade ctx makes is breadth: one gate across Rust, Python and TypeScript in a
polyglot repo.

```yaml
# .github/workflows/ci.yml — PR-only; fetch-depth 0, the default shallow clone
# has no merge base.
- uses: actions/checkout@v7
  with: { fetch-depth: 0 }
- run: cargo install code-context --locked
- run: ctx changed --api --strict --since "origin/${{ github.base_ref }}"
```

This isn't a context tool. It's CI.

### 3. `move-plan` — a refactor oracle, not an actuator

`move-plan <from> <to>` emits every site a module relocation must touch;
`move-verify` checks the result. **ctx never writes source files.**

```sh
ctx move-plan native::gate native::routing::gate   # file move + every import rewrite
# ... agent applies the edits ...
ctx move-verify native::gate native::routing::gate # exits non-zero if anything is orphaned
```

The reasoning: an agent can already make edits cheaply and precisely. What it
cannot do is know it found *every* site, or prove nothing was orphaned. The
scarce thing is ground truth, not typing — and staying read-only keeps ctx free
of partial application and undo semantics.

Scope is bounded by what ctx can prove. Moves ride on import and link resolution
— path arithmetic, deterministic, non-heuristic — which is ctx's strongest
signal, so the site list is exact for the languages it parses. Dependents reached
only by receiver inference have no literal string to rewrite and are listed
separately as *unverified*, never mixed in with real sites. Rust `mod`
declarations are not imports, so the plan names them as a required manual step
rather than omitting them silently. Renaming a *method* is deliberately not
offered: it would ride on receiver inference, which resolves a minority of call
sites, and a plan built on that would silently miss some.

### 4. `parity` — cross-language port fidelity

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
`context`, `changed --api`, `move-plan`, `parity`. Those are queries mid-task,
not a blob at boot. Ranking is one small command here (`ctx core`), not the
product.

The nearer comparison is **Serena**, which does LSP-backed symbol navigation.
Against it, `ctx` trades semantic precision for two things:

| | `ctx` | LSP-based (Serena) |
|---|---|---|
| setup | one binary, grammars compiled in | a language server per language, configured and running |
| startup | ~100 ms graph build, per query | server warm-up, project indexing |
| determinism | pure function of the source tree | depends on server state, versions, build artifacts |
| uncertainty | marked per edge (`~`, `*`) + a miss census | resolved or absent, silently |
| coverage | 4 languages (today), one graph across all of them | as many as you install servers for |

If you need type-perfect resolution inside one language, use an LSP. If you
want the same graph across a polyglot repo with nothing to install and answers
that state their own confidence, that's this.

`ctx` does ship the boot map too (`ctx map`), so you can have it if you want it.
But we'll say plainly what dogfooding taught us: it's the least useful thing
here. [More on why](#maps-when-you-want-them).

## "Why not just grep?"

Often you should. Here is the measurement, including where grep wins.

> **Counting tokens.** Every figure in this README is ctx's own estimate —
> `len/3`, the same one `--max-tokens` and `--metrics` use. Recompute with a
> real tokenizer and you will get different absolutes; the ratios are what the
> argument rests on, and they come from one estimator on both sides. `len/3` is
> also conservative for source code, which tokenizes closer to 3.5–4 characters
> per token, so if anything the grep figures below understate the gap.

The grep below is the one a competent agent would actually write — call syntax,
language-filtered, skipping the same directories ctx skips — not a bare
`grep -rn name`:

```sh
grep -rn --include='*.py' --include='*.rs' --include='*.ts' \
  --exclude-dir={.git,node_modules,target,__pycache__,.venv,venv,dist,build} \
  '\bload(' .
```

**On a small repo, grep wins.** This repository, 14 source files:

```sh
grep -rn --include='*.rs' --exclude-dir=target '\bcoverage_report(' src/
```

194 tokens against `ctx callers coverage_report`'s 215. Jump-to-definition is
worse for ctx — `grep -rn --include='*.rs' --exclude-dir=target 'struct Universe'
src/` is 14 tokens against `ctx def`'s 124. At this size the graph is overhead
and you should use grep.

**On a large repo it inverts.** A 14,073-file tree:

| "who calls X" | `ctx callers` | fair grep | grep lines | callers ctx found |
|---|---|---|---|---|
| `load` | **4,014** | 32,994 | 784 | 90 |
| `run` | **441** | 11,017 | 242 | 8 |
| `boot` | **1,693** | 4,744 | 98 | 31 |

**3× to 25× fewer tokens.** grep's cost scales with how common the *string* is;
ctx's scales with how many *call edges* actually exist. `boot` is the weak case
at 2.8× — worth knowing, because a tool that only ever reports its best ratio is
advertising rather than measuring. `run` is the strong one: 242 matching lines,
genuinely called from 8 places, and grep's output for it would not fit in most
context windows.

**But the size difference is not really the point** — the two answer different
questions. Same task, same repo:

```
grep:  src/mcp.rs:475:    query::coverage_report(&g, &unsupported, barg("explain"), false),
       src/query.rs:1548:pub fn coverage_report(
       src/query.rs:2177:        let out = coverage_report(&g, &[], false, false);

ctx:   mcp::dispatch  (src/mcp.rs:361)  → query::coverage_report
       query::tests::coverage_separates_internal_external_and_blind_spots  (src/query.rs:2856)
       query::tests::doctor_names_what_it_could_not_pin  (src/query.rs:2194)
```

grep returns lines containing the text — including the definition itself, mixed
in with the calls. ctx returns **the functions that call it**. "What breaks if I
change this signature" is answered by a list of callers, not a list of lines, so
grep's output needs a second pass to read around each hit and work out which
function it lands in. That pass is the part ctx has already done.

**And grep is complete where ctx is not.** ctx only reports edges it could
resolve, so it undercounts — which is why every answer carries its own error
bar:

```
completeness: 240 call site(s) named `load` could not be pinned
(scripts.regress 10, api 6, tests::progressive_integration 6, ...) —
this blast radius may be incomplete; grep `load` to confirm.
```

**So: grep is complete and imprecise; ctx is precise and tells you where it
isn't.** They compose. ctx narrows 242 lines to 8 callers and then names the
places you still need grep — which is a better division of labour than either
tool doing the whole job.

## Install

**No Rust toolchain required** — prebuilt binaries for macOS, Linux and Windows:

```sh
# macOS / Linux
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/Eagle-Logic/context/releases/latest/download/code-context-installer.sh | sh

# Windows (PowerShell)
irm https://github.com/Eagle-Logic/context/releases/latest/download/code-context-installer.ps1 | iex
```

```sh
# Homebrew
brew install eagle-logic/tap/ctx
```

With a Rust toolchain (1.88+):

```sh
cargo install code-context        # the binary it installs is `ctx`

# or from source
git clone https://github.com/Eagle-Logic/context
cd context && cargo install --path .
```

Single binary, no runtime dependencies — the tree-sitter grammars are compiled
in. Prebuilt for `x86_64`/`aarch64` macOS, `x86_64` Linux (gnu **and** static
musl, so it drops into a distroless or scratch container), and `x86_64` Windows.

> **macOS:** use the installer script or Homebrew above. The release archives are
> **not code-signed**, so a `.tar.xz` downloaded through a browser gets
> quarantined by Gatekeeper and refuses to run until you clear it
> (`xattr -d com.apple.quarantine ./ctx`). The `curl | sh` path doesn't set the
> quarantine attribute and is unaffected.

## The queries

**Start here:** `ctx core` to orient, `ctx def` to find a symbol, `ctx context`
to work on it. That sequence is the tool. Everything below is a specialisation
of it, and the whole-repo map at the bottom is measured at
[42× the cost of `core`](#measuring-what-it-costs) for the same orientation job.

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
ctx changed --api --strict --since main   # exit non-zero on a removal — for CI

# Plan a module move, then prove it landed
ctx move-plan native::gate native::routing::gate
ctx move-verify native::gate native::routing::gate

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

# Wider views. `subtree` is the one to reach for; `map` is a cold-start or
# human-reader tool, not an agent's opening move — see "Maps, when you want them"
ctx subtree core::inference           # one module + upstream + downstream
ctx modules                           # module list + dep edges only
ctx map --view skeleton               # whole repo, architecture only
ctx map --max-tokens 8000             # hard cap: reduces detail, then prunes
ctx map -o CODEBASE_MAP.md            # write it out (goes stale on the next edit)

# Scoping (global, repeatable)
ctx map --lang code                   # Rust/Python/TypeScript only, no prose
ctx map --exclude 'docs/archive/**'   # vendored trees, dead code

# Machine-readable
ctx map --format json

# Print the agent instructions block, measured for this repo
ctx snippet >> AGENTS.md              # or CLAUDE.md, .cursorrules, …
```

Every command except `parity` takes a repo path as its last positional argument,
defaulting to `.`; `parity` takes a source and one or more targets instead. See
[EXAMPLES.md](EXAMPLES.md) for each of these run against a real repo, with output.

### Scoping the scan

`--exclude '<glob>'` and `--lang` are global, repeatable flags. Vendored trees,
archived docs and dead code are usually tracked, so `.gitignore` will not exclude
them: `--exclude 'docs/archive/**'`. And because a docs tree can dominate a *code*
map — 78% of one 720-module repo's skeleton view — `--lang code` restricts the
scan to Rust/Python/TypeScript, cutting that map from ~169k to ~35k tokens.

Module names are unique. They derive from paths, so `src/lib.rs` and `src/main.rs`
both want to be `crate`, and a `native/README.md` collides with the
`src/native/mod.rs` it documents. Because resolution indexes by name, a collision
used to make one module unreachable and silently drop its reverse edges. Code now
keeps the bare name, prose is renamed to `name@stem`, and every rename is reported
on stderr.

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

crate::main  [src/main.rs:443]
  → mcp::run  [src/mcp.rs:14]
    → mcp::handle_method  [src/mcp.rs:44]
      → mcp::tools_call  [src/mcp.rs:132]
        → mcp::dispatch  [src/mcp.rs:146]
          → query::coverage_report  [src/query.rs:1420]
```

"How does a request get from `main` to here" is one command rather than a grep
chain. Both mark each hop's confidence and report call edges that leave the
resolved graph, so a trace never quietly implies more certainty than it has.

### How `callers` reports its own limits

`callers` **never claims completeness**. Reverse edges exist only for calls that
resolved, and resolution drops rather than guesses, so the result is a floor.
Three loss channels are knowable and reported in band:

- **Ambiguous name** — more than one definition, so a call through an opaque
  receiver cannot be pinned to one of them. Prints `INCOMPLETE` with the
  definition count (`ambiguous_name` in JSON). Computed from the bare name, so
  qualifying the query cannot silence it.
- **Suppressed ubiquitous name** — `get`, `open`, `push`, `len` and friends are
  never indexed, including a project-defined method that merely shares the name.
  Prints `NOT INDEXED`, because an empty result there carries no information at
  all (`suppressed_common_name` in JSON).
- **Measured misses for this name** — the closing completeness line counts call
  sites bearing this name that ctx could not pin, and says where. When it reports
  none, the confirming grep can be skipped.

The first two set `lower_bound: true`. The absence of a flag means "no *known*
reason to distrust this" — not a guarantee: a call made at module level, or
through a function-local import, can still be missed. Before changing a
signature, run the `rg` command ctx prints. This matters because `callers` is the
pre-signature-change safety check, and "no callers" reads as "safe to change".

`context`, `trace`, and `path` close with the same measured completeness line.

### Spans and content hashes

`def`, `callers` and `context` report a **span and a content hash**, not just a
start line:

```
extract::Universe   [struct]   src/extract/mod.rs:1066-1083  #85eadbaee90b
```

The end line is the useful half. With only a start line an agent opens the file
and guesses where to stop, which is how reading a 20-line struct turns into
reading the 1,900-line module it happens to live in. The span says exactly what
to read.

The hash covers the item's full source span, body included, and is
**position-independent** — inserting a comment above a function does not change
its hash. Two uses, both about *not* re-reading: an agent holding a span can
tell it is unchanged without opening the file, and two identical spans are
visibly identical without comparing text.

A one-line item reports `:12`, not `:12-12`. Both appear in `--format json` as
`end_line` and `hash` alongside the existing `line`, so the addition is
non-breaking for machine consumers.

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

**Renamed containers are inferred.** Container is part of every member key, so
renaming a type invalidates all of its members at once — and renaming the main
type is the normal case in a port, not an edge case. `parity` pairs containers by
shared member sets (requiring at least two shared members and a majority of the
smaller container, so unrelated types are never paired), and container and member
renames compose: `TriStateRouter.__init__` → `IntentGate.new` needs both, and
neither alias alone finds it. Every inferred pairing is listed under **Inferred
container renames** — a wrong pairing must never masquerade as a clean parity
result — and `--alias Old=New` overrides inference when the port shares too few
names to infer from.

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
Rust visibility is `pub`-based; Python uses the underscore convention (dunders
like `__init__` count as public); TypeScript uses the `export` keyword (public
class methods are interface, `private`/`#` members are dropped). Trait methods
and trait impls are always interface.

`map --max-tokens <N>` is a **hard cap**. It first reduces detail, emitting the
richest view at or below `--view` whose (~`len/4`) token count fits, and reports
the view it chose on stderr. When even `skeleton` is over budget, detail is
exhausted and the only remaining lever is dropping modules, so the least-central
ones are pruned (PageRank, the same ranking `core` uses) until the map fits. The
output states what was omitted, and dependency edges still name pruned modules —
that name is what to feed `subtree`/`context` next.

Items inside a kept module are never truncated: you get fewer modules, each
still whole. Only when a single module alone exceeds the budget does `ctx` emit
an over-budget map, and it says so on stderr. Without `--max-tokens`, nothing is
ever pruned.

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

Each item also carries the first line of its doc comment (Rust `///`, Python
docstring) as a trailing `— summary`, so a signature map doubles as a labeled
one at negligible token cost:

```markdown
- pub fn to_config(&self, n_predict: i32) -> SteerConfig  [L317] → NativeSteerInstruction::to_config  — Build a SteerConfig from the explicit knobs (gate bypassed).
```

## Agent integration

Two ways in. **The CLI is the one to reach for first**, and it is what this
project's author uses exclusively.

### The CLI, via an instructions file

`ctx snippet` prints a "Codebase Discovery" block **measured for this repo** —
its module count, token cost, and call-resolution rate are read off your actual
source, so the guidance is derived rather than asserted. Append it to whatever
file your agent reads:

```sh
ctx snippet >> AGENTS.md        # or CLAUDE.md, .cursorrules, .windsurfrules …
```

The block names no vendor. It describes commands and a protocol, so it works
with any agent that can read a file and run a shell command. The block is fenced
by markers, so regenerating replaces it in place instead of appending a second,
contradictory copy.

It teaches a query-first protocol: run `ctx context <name>` when you're about to
touch a symbol, `ctx callers` before changing a signature, `ctx trace`/`ctx path`
to follow control flow instead of grepping, and only read raw source for
implementation bodies. Reaching for a whole-repo map is the exception, not the
opening move.

### MCP server, if you want typed tools

`ctx mcp` runs a minimal MCP server over stdio (newline-delimited JSON-RPC, no
dependencies) exposing the read-only commands as typed tools, so an agent calls
them structured, without shelling out or a permission prompt. It speaks the
standard protocol, so any MCP client works:

```sh
claude mcp add ctx -- ctx mcp     # or your client's equivalent
```

Each tool takes a `path` argument (default `.`); `def`/`callers`/`context` also
take `name`, and `subtree` takes `module`.

**It costs more standing context than the CLI**, which is worth knowing given
what the rest of this README argues. Tool definitions are resident in every
turn whether or not a tool is ever called:

| | resident tokens |
|---|---|
| `ctx snippet` block | **616** |
| MCP tool definitions (10 tools) | **1,326** |

2.2x, before either has answered anything. An MCP tool list is itself a blob
injected at boot — the shape this project objects to — so the honest advice is
to use MCP when you want structured calls and no shell permission prompts, and
the CLI when you want the cheapest possible standing cost.

The tool schemas are deliberately terse for this reason. They say what a tool
is for and what it takes, and nothing about how to read the result — every
command's output already carries its own legend, so explaining markers up front
would mean paying for that explanation in every turn instead of only in the
turns where a tool was actually called.

The graph is reused across calls while the source it was built from is
unchanged. Staleness is checked by re-walking for mtime and length — a stat per
file, no reads — so a hit costs a walk instead of a parse, and any edit,
addition or deletion rebuilds. Answers are still always current; the difference
is that an unchanged tree is not re-parsed five times in a row.

It matters most where the parse is expensive. A five-call session on a
14,073-file repo, measured with `--metrics`:

| | first call | later calls | session |
|---|---|---|---|
| rebuilding every call | 938 ms | ~685 ms each | **3,676 ms** |
| reusing the graph | 680 ms | 5–10 ms each | **708 ms** |

The cache is bounded and lives only for the life of the process — the CLI is
one-shot and unaffected.

**Two limits apply over MCP that don't apply on the CLI**, because an MCP result
lands straight in a model's context with no shell to pipe it through and no
person to see the cost before it's paid:

- **Every tool whose output scales with repo size is budgeted** — `map`,
  `subtree`, `modules`, `callers`, `doctor`, `context` — defaulting to 25,000
  tokens (4,000 for `context`). `map` and `subtree` degrade instead of cutting:
  less detail, then fewer modules. The flat reports cut on a line boundary and
  say so in band. Pass `max_tokens` to raise it. `map` also defaults to the
  `skeleton` view rather than the CLI's `full`.
- **The server only reads its own root**, which defaults to the working
  directory it was started in. A `path` that resolves outside it — absolute,
  `../..`, or through a symlink — is refused with a message naming the root.
  Use `ctx mcp --root <dir>` to point it elsewhere deliberately.

Both are default-deny with an explicit opt-out. A model has no legitimate reason
to leave the project it was pointed at, and it cannot see the cost of doing so
until the tokens are already spent.

#### Measuring what it costs

`ctx mcp --metrics <file>` appends one JSON line per tool call — tool,
arguments, output tokens, whether the budget bit, duration — plus a session
total when the client disconnects. `-` writes to stderr. It is off by default
and never touches a tool's response, so enabling it cannot change what the model
sees.

It exists because "query instead of loading a map" was an argument until someone
counted. Measured on a 14,073-file repo, the claim is narrower and sharper than
"queries are cheaper than maps".

Counts are ctx's own `len/3` estimate throughout — see the note under
["Why not just grep?"](#why-not-just-grep).

**A boot-time map is a 42× more expensive way to orient than `ctx core`, and it
does not answer the questions you then have to ask anyway.**

Both are orientation steps — "what is this codebase, where do I start":

| orientation step | tokens |
|---|---|
| `ctx core` | **596** |
| `ctx map` (budgeted; unbudgeted the same map is 408,511) | **24,992** |

The second half matters more than the ratio. After loading that map, the same
session still ran `def`, `context`, `callers` and `trace` on the symbol it was
actually there to change — 8,690 tokens that were needed either way. The map
answered none of them. It was breadth bought before knowing which part was
needed, and the specific answers were bought again afterwards at full price.

For the record, the whole sessions were 9,286 tokens without the map and 33,682
with it. That is a with/without comparison and it flatters the argument: of
course dropping a 24,992-token step is cheaper. The load-bearing number is the
596-vs-24,992 orientation cost and the fact that the map bought nothing the
queries did not have to buy again.

```sh
ctx mcp --metrics /tmp/ctx.jsonl
jq -s 'map(select(.summary|not)) | group_by(.tool)
       | map({tool: .[0].tool, calls: length, tokens: (map(.output_tokens)|add)})' /tmp/ctx.jsonl
```

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
  paths resolve against that prefix. Name collisions rename prose to `name@stem`
  and report it, so resolution never silently loses a module.
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
  codebase-wide, not a ubiquitous std method** (`push`, `get`, `items`, …), and
  **in the same language**. Ambiguous or unresolvable calls are dropped, never
  guessed.
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
  branch is marked `*` (`dispatch: true` in JSON). Deps that exist *solely* via
  receiver inference are tracked separately as soft, so an import-derived dep is
  never diluted by a guess.
- **Not captured (yet)**: receivers whose type comes from an expression ctx does
  not evaluate — `for` bindings, iterator chains, and results of calls that are
  not associated constructors — still fall back to the unique-name heuristic.
  `doctor` names every such miss rather than hiding it.

## Adding a language

Everything downstream of extraction — resolution, the call graph, `core`,
`parity`, `move-plan`, `diff`, the MCP server — consumes one language-neutral
struct and never asks what produced it. So a new language is one extractor file
plus a grammar crate, wired in at a handful of places the compiler will point
you at.

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

**2. Add the `Lang` variant**, then run `cargo check`. Eight exhaustive matches
will fail to compile, and each is a real decision:

| site | decision |
|---|---|
| `extract/mod.rs` `build_graph` | call your `extract()` |
| `extract/mod.rs` `module_name` stem collapse | which filename collapses to its directory (`mod`/`lib`/`main`, `__init__`, `index`) |
| `extract/mod.rs` `module_name` root | the name of the root module (`crate` vs `root`) |
| `extract/mod.rs` `candidates()` | import string → absolute module segments — by name (Rust/Python) or by path (TypeScript) |
| `extract/mod.rs` `filter_note()` | how `--lang` describes your language |
| `model.rs` `Lang::name()` | display string |
| `model.rs` `Lang::sep()` | path separator (`::` vs `.`) |
| `view.rs` `is_public()` | what "public" means: a keyword, a naming convention, an `export` |

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
