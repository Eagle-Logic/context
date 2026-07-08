# context (`ctx`)

Deterministic AST skeleton maps of a codebase, built for ultra-dense context
injection into coding agents (Claude Code in particular).

`ctx` parses Rust and Python sources with tree-sitter, strips every
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

# Machine-readable graph
ctx map ~/projects/myrepo --format json

# Print the recommended CLAUDE.md discovery-protocol block
ctx snippet >> ~/projects/myrepo/CLAUDE.md
```

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

| view | contents | eagle-logits-native (419 modules) |
|---|---|---|
| `skeleton` | modules, deps, re-exports, type names | 79 KB (~19k tok) |
| `interface` | + public signatures, struct fields, enum variants | 301 KB (~75k tok) |
| `full` (default) | + private items and call edges | 451 KB (~112k tok) |

On a mid-size repo (70 files) skeleton is ~3k tokens — cheap enough for the
first turn of every session. Rust visibility is `pub`-based; Python uses the
underscore convention (dunders like `__init__` count as public). Trait
methods and trait impls are always interface.

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
agent a lookup protocol: boot with `ctx map --view skeleton`, pull
`ctx subtree <module>` before touching a module, follow call edges instead of
grepping, and only read raw source for implementation bodies.

To keep a committed `CODEBASE_MAP.md` fresh, regenerate it from a git
pre-commit hook or a Claude Code hook (`ctx map . -o CODEBASE_MAP.md`) — or
skip the file entirely and have sessions run `ctx map` at boot; generation is
~100 ms, so freshness is free.

## Design notes

- **Parsing**: tree-sitter (`tree-sitter-rust`, `tree-sitter-python`). Bodies
  are dropped by slicing each definition node up to its `body` field; struct
  fields and enum variants are kept inline because they carry architectural
  signal at negligible token cost.
- **Module names** are path-derived: `src/` components are dropped,
  `mod.rs`/`lib.rs`/`main.rs`/`__init__.py` collapse into their directory. In
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

Adding a language = one extractor file implementing
`extract(src) -> (Vec<Item>, Vec<String /* raw imports */>)` plus the
tree-sitter grammar crate.
