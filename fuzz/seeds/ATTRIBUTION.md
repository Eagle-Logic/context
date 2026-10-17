# Seed corpus provenance

Every file in this directory except `extract_markdown/00-SECURITY.md.seed` is
**third-party source code**, copied verbatim from the benchmark corpus pinned
in [`bench/repos.tsv`](../../bench/repos.tsv) and renamed with a `.seed`
suffix. Nothing here is part of `ctx`; it is fuzzer input, chosen because real
idiomatic source is a far better starting point for a mutation fuzzer than
anything written to look like source.

`ctx` is MIT-licensed. These files are not — they keep the licence of the
project they came from, and this file is the notice that accompanies them.

| seeds | upstream | pinned commit | licence |
|---|---|---|---|
| `extract_rust/` | [BurntSushi/ripgrep](https://github.com/BurntSushi/ripgrep) | `3fce3b5` | MIT **or** Unlicense |
| `extract_python/` | [psf/requests](https://github.com/psf/requests) | `611c616` | Apache-2.0 (ships a `NOTICE`) |
| `extract_typescript/` | [sindresorhus/got](https://github.com/sindresorhus/got) | `e1d87d2` | MIT |
| `extract_typescript/` | [preactjs/preact](https://github.com/preactjs/preact) | `8101ff8` | MIT |
| `extract_go/` | [spf13/cobra](https://github.com/spf13/cobra) | `adbc881` | Apache-2.0 |
| `extract_go/` | [go-chi/chi](https://github.com/go-chi/chi) | `3d1777a` | MIT |
| `extract_java/` | [apache/commons-lang](https://github.com/apache/commons-lang) | `66f9309` | Apache-2.0 (ships a `NOTICE`) |
| `extract_markdown/` | [spf13/cobra](https://github.com/spf13/cobra) | `adbc881` | Apache-2.0 |

`extract_markdown/00-SECURITY.md.seed` is this repository's own `SECURITY.md`.

Files that carried a licence header upstream still carry it — the
`commons-lang` seeds are the visible case. Those that did not are covered by
the table above, which is why this file exists rather than a per-file header
being added (editing the bytes would change the input the fuzzer was seeded
with, and reproducibility of the corpus is the point of pinning it).

## NOTICE propagation

Apache-2.0 §4(d) requires redistributions to carry the content of any `NOTICE`
file shipped with the work. Two of the upstreams above ship one:

**`psf/requests`:**

```
Requests
Copyright 2019 Kenneth Reitz
```

**`apache/commons-lang`:**

```
Apache Commons Lang
Copyright 2001-2024 The Apache Software Foundation

This product includes software developed at
The Apache Software Foundation (https://www.apache.org/).
```

`spf13/cobra` is Apache-2.0 and ships `LICENSE.txt` with no `NOTICE`.

## Regenerating

These were taken mechanically: files under 4 KB, first twelve per language by
sorted path, from a checkout of each pinned commit. `bench/run.sh` is what
clones that corpus. Re-deriving them rather than vendoring them would remove
this notice requirement entirely, at the cost of making a first fuzz run
depend on cloning seven repositories — a trade worth revisiting if this list
grows.
