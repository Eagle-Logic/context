# Benchmark

Every measured claim in the README, re-run on public repositories that are not
this one. Regenerate with:

```sh
cargo build --release && ./bench/run.sh
```

The corpus is pinned to exact commits in [`bench/repos.tsv`](bench/repos.tsv),
so these numbers are reproducible rather than merely reported. Re-pinning is a
deliberate commit that moves the results with it.

> **Estimator.** Token counts are `len/3`, the same estimate `ctx --metrics`
> uses, applied identically to both sides of every comparison. A real tokenizer
> gives different absolutes; the ratios are what the argument rests on, and they
> come from one estimator throughout. `len/3` is conservative for source code,
> which tokenizes closer to 3.5–4 characters per token, so the grep figures here
> understate the gap if anything.

## Internal recall

The number ctx reports about itself, measured on code ctx has never seen.
"Could be internal" excludes calls into the standard library or a third-party
package, because no internal edge could exist for those however good the
resolver gets. Misses are the real ones — names defined *in* the tree that ctx
could not pin — and `ctx doctor` names every one of them with a grep to confirm.

| repo | lang | commit | modules | recall | misses | top unpinned names |
|---|---|---|---|---|---|---|
| ripgrep | rust | `3fce3b5` | 110 | **4601/7038 = 65.4%** | 2437 | `parse_low_raw` ×560, `path` ×371, `build` ×221 |
| requests | python | `611c616` | 37 | **976/1167 = 83.6%** | 191 | `httpbin` ×155, `should_strip_auth` ×9, `httpbin_secure` ×8 |
| got | ts | `e1d87d2` | 88 | **1723/2184 = 78.9%** | 461 | `request` ×91, `now` ×71, `setTimeout` ×55 |
| cobra | go | `adbc881` | 36 | **2278/2283 = 99.8%** | 5 | `string` ×3, `Len` ×2 |
| chi | go | `3d1777a` | 84 | **1569/1752 = 89.6%** | 183 | `ServeHTTP` ×98, `Get` ×41, `Context` ×5 |
| preact | ts | `8101ff8` | 223 | **2176/2787 = 78.1%** | 611 | `createElement` ×92, `render` ×79, `hydrate` ×40 |

## Cost of an answer: `ctx callers` vs a fair grep

The comparison is against the grep a competent agent would actually write — call
syntax, language-filtered, skipping the same directories ctx skips — not a naive
`grep -rn name`.

Symbols are chosen mechanically: of the callables each repo defines, the ones
with the most call-syntax grep hits. That picks grep's *worst* cases by
construction, so treat these ratios as an upper bound on ctx's advantage rather
than a typical one. Rows where ctx costs more are printed too.

They also answer different questions, which no ratio captures: grep returns
lines containing the text, definition included; `ctx callers` returns the
functions that call it. "What breaks if I change this signature" is a list of
callers, not a list of lines.

| repo | symbol | grep lines | grep tok | ctx tok | ratio | callers | ctx says |
|---|---|---|---|---|---|---|---|
| ripgrep | `new` | 1216 | 80967 | 17594 | 4.6x | 359 | name collision |
| ripgrep | `unwrap` | 1147 | 75965 | 411 | 184.8x | 5 | name collision |
| ripgrep | `arg` | 357 | 22479 | 159 | 141.4x | 2 | **complete** |
| ripgrep | `parse_low_raw` | 565 | 38372 | 108 | 355.3x | 0 | incomplete |
| requests | `get` | 180 | 11532 | 4266 | 2.7x | 89 | name collision |
| requests | `httpbin` | 156 | 10327 | 100 | 103.3x | 0 | incomplete |
| requests | `prepare` | 59 | 3625 | 2008 | 1.8x | 36 | ambiguous |
| requests | `send` | 50 | 2891 | 1206 | 2.4x | 20 | name collision |
| got | `get` | 1308 | 76569 | 285 | 268.7x | 2 | name collision |
| got | `destroy` | 189 | 10171 | 1484 | 6.9x | 33 | incomplete |
| got | `setHeader` | 159 | 9922 | 1544 | 6.4x | 33 | **complete** |
| got | `json` | 141 | 8566 | 996 | 8.6x | 23 | ambiguous |
| cobra | `executeCommand` | 307 | 20238 | 7408 | 2.7x | 193 | **complete** |
| cobra | `Flags` | 205 | 13060 | 3444 | 3.8x | 90 | **complete** |
| cobra | `AddCommand` | 158 | 8915 | 5448 | 1.6x | 135 | **complete** |
| cobra | `Name` | 119 | 7550 | 2365 | 3.2x | 65 | **complete** |
| chi | `Get` | 260 | 16267 | 3382 | 4.8x | 92 | incomplete |
| chi | `Write` | 222 | 12324 | 385 | 32.0x | 4 | incomplete |
| chi | `HandlerFunc` | 169 | 11430 | 153 | 74.7x | 2 | name collision |
| chi | `testRequest` | 154 | 10670 | 1468 | 7.3x | 40 | ambiguous |
| preact | `render` | 58 | 3161 | 272 | 11.6x | 2 | incomplete |
| preact | `createElement` | 21 | 1296 | 142 | 9.1x | 0 | incomplete |
| preact | `createSignal` | 11 | 718 | 252 | 2.8x | 3 | ambiguous |
| preact | `unstable_runWithPriority` | 7 | 454 | 223 | 2.0x | 1 | incomplete |

**Read the last column before the ratio column.** A large ratio on an
`incomplete` row is not a win — it is ctx answering cheaply *because it could not
resolve the calls*, and the tool says so rather than letting the number stand.
Only `complete` rows are counted in the summary below.

| ctx says | what the number means |
|---|---|
| `complete` | every call site with that name resolved. Act on it; no follow-up grep needed. |
| `name collision` | every *internal* call resolved, but the name also belongs to a std/third-party function. The caller list is right and grep's line count is mostly a different function that happens to share the spelling — the ratio is real, but it is measuring grep's false positives, not ctx's compression. |
| `ambiguous` | several definitions share the name, and calls through a receiver ctx could not type were dropped. Lower bound. |
| `incomplete` | ctx could not pin some call sites. The cheap answer is cheap *because the analysis fell down*, and ctx names the unresolved callees so you can grep them. |

That distinction is the product. Every tool in this space can return a short
answer. This one tells you which short answers are short because the question
was small, and which are short because the analysis fell down.

### What `complete` does not cover

`complete` means no call site ctx **parsed** went unresolved. A call site ctx
never parsed cannot be unresolved, so it does not reduce the claim — which is
what "complete to the limit of what ctx parses" is doing in that sentence, and
it is doing more work than it looks.

The sharpest case in this corpus is `ripgrep`/`arg`. ctx reports 2 callers and
says `complete`; the fair grep finds 357 lines, and the ~355 it does not report
are real calls to that same method. They sit inside `rgtest!(...)` macro
invocations, and **ctx does not descend into macro bodies** — so those sites were
never seen, never counted, and never held against the completeness line.

Minimal reproduction:

```rust
pub fn plain() { let mut t = T; t.arg(); }      // seen
wrap!(inside, { let mut t = T; t.arg(); });     // not seen
```

`ctx doctor` reports `call sites: 1` for that file. Both calls are real.

This is the honest reading of the whole benchmark: the calibration claim is
about what ctx *resolved* versus what it *saw*, and a macro-heavy Rust codebase
can hide a large fraction of the third quantity. That is a limitation this
harness found on a third-party repository, which is the argument for running it
on third-party repositories.

Over the 6 `complete` answers above, the median is **3.5x** fewer tokens than the fair grep.

The median, not the best case or the range. Individual ratios here span two orders of
magnitude, and the largest of them are the least trustworthy — a ratio is only as good
as the completeness verdict beside it, and the widest gaps in this corpus are the rows
where ctx saw least. Read the table, not this line.

---

Generated by `./bench/run.sh` from `bench/repos.tsv` (6 repositories).
ctx version: `ctx 0.23.0`.
