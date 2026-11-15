# Fuzzing the extractors

`SECURITY.md` names **"crashes or hangs on hostile input"** as one of four
things actually worth reporting against `ctx`, on the grounds that it "runs over
whatever source it is pointed at, which may not be trusted." These targets are
how that claim gets tested instead of asserted.

Each target drives one extractor's `extract(src: &str) -> Result<FileFacts>` —
a pure function over a string, with no filesystem and no CLI in the way, which
is what makes the extractors the right place to point a fuzzer.

| target | extractor |
|---|---|
| `extract_rust` | `src/extract/rust.rs` |
| `extract_python` | `src/extract/python.rs` |
| `extract_typescript` | `src/extract/typescript.rs`, both `tsx` settings |
| `extract_go` | `src/extract/go.rs` |
| `extract_java` | `src/extract/java.rs` |
| `extract_markdown` | `src/extract/markdown.rs` |

## Running

`cargo fuzz` needs a nightly toolchain for `-Zsanitizer=address`. It does not
pick one up on its own, so `+nightly` is not optional — without it the build
fails with "the option `Z` is only accepted on the nightly compiler".

```sh
rustup toolchain install nightly
cargo install cargo-fuzz

./fuzz/run.sh                 # every target, 60s each
./fuzz/run.sh 600             # every target, 10 minutes each
./fuzz/run.sh 600 extract_go  # one target
```

`fuzz/run.sh` exists because the raw invocation has a sharp edge worth
mechanizing rather than remembering: libFuzzer refuses to start if a corpus
directory it is handed does not exist, and `fuzz/corpus/` is `.gitignore`d, so
a fresh clone fails on its first run. The script creates it, and passes the
live corpus *before* the committed seeds — libFuzzer reads every directory but
writes new inputs only to the first, which is what keeps `fuzz/seeds/`
reproducible instead of accumulating whatever one machine happened to find.

Underneath, per target, it is:

```sh
mkdir -p fuzz/corpus/extract_rust
cargo +nightly fuzz run extract_rust fuzz/corpus/extract_rust fuzz/seeds/extract_rust \
  -- -max_total_time=60
```

A crash is written to `fuzz/artifacts/<target>/` and replayed with:

```sh
cargo +nightly fuzz run extract_rust fuzz/artifacts/extract_rust/crash-<hash>
cargo +nightly fuzz tmin extract_rust fuzz/artifacts/extract_rust/crash-<hash>
```

## What is asserted

A fuzzer that only catches panics tests one of the two things that can go wrong
here. The other is a parse that *succeeds* and reports a span pointing
somewhere the definition is not — a wrong answer delivered with no diagnostic,
which is the failure class this project treats as most serious. So
`fuzz/src/lib.rs` checks the span contract on every item and every call site,
recursively through nested items:

- `line >= 1` — spans are 1-based, so 0 means an off-by-one escaped.
- `end_line >= line` — a span may not run backwards.
- `end_line <= src.split('\n').count()` — a span may not end past the file.
  `split`, not `lines`, because tree-sitter rows are 0-based and a trailing
  newline opens one more row; `lines()` under-counts by one on every file that
  ends in a newline.
- `kind` is non-empty — `src/view.rs` dispatches on `kind` by literal match, so
  an empty or novel one silently vanishes from both skeleton and interface
  views rather than erroring.

Non-UTF-8 input returns early. That is scope, not an oversight:
`extract/mod.rs` reads sources with `fs::read_to_string` and skips any file
that fails (`Err(_) => continue`), so a non-UTF-8 file never reaches an
extractor in the first place.

## Seeds and corpus

`fuzz/seeds/` is committed: 69 small real source files taken from the pinned
benchmark repositories in `bench/repos.tsv`, so the fuzzer starts from
idiomatic syntax rather than spending its budget rediscovering that `fn` is a
keyword. Only files under 4 KB — libFuzzer mutates and minimizes faster on
small inputs, and a 40 KB seed mostly buys repetition of syntax the grammar
already covers.

`fuzz/corpus/` is where libFuzzer accumulates its own inputs. It is
`.gitignore`d, because it is a derived artifact that grows without bound; the
seeds are the reproducible part.

## Not covered

Worth stating plainly, since the point of this directory is to know what has
actually been tested:

- **Resolution.** `build_graph` and everything in `extract/mod.rs` after the
  per-file extract — candidate generation, package scoping, re-export chasing —
  is where most of the logic lives and none of it is fuzzed here. It needs a
  whole-tree input rather than one string, which is a different harness.
- **The filesystem walk**, `--exclude` globs, and symlink handling.
- **The MCP server**, including the root-containment guarantee `SECURITY.md`
  makes and the output budgets. Those are the other two reportable areas and
  they have tests, not fuzzing.
