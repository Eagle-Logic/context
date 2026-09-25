#!/usr/bin/env bash
#
# Reproduce every measured claim in the README, on repositories that are not
# this one.
#
#   ./bench/run.sh            # regenerate BENCHMARK.md
#   ./bench/run.sh --stdout   # print instead of writing
#
# Why this exists. The numbers a static-analysis tool reports about itself are
# the least interesting ones it can produce, and they are not even stable: this
# repository contains the README that reports on it, so editing that README
# changes the figure it quotes. Third-party corpora are both more credible to
# someone who did not write the tool and — because nothing here edits them — the
# only figures that hold still.
#
# Everything below is measured with the SAME estimator on both sides of every
# comparison (`len/3`, the one `ctx --metrics` uses), so the ratios are
# apples-to-apples even though the absolutes will differ from a real tokenizer.
#
# ctx itself never touches the network; that property is load-bearing and is not
# being relaxed for a benchmark. The cloning lives here, in the harness.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CACHE="${BENCH_CACHE:-${TMPDIR:-/tmp}/ctx-bench}"
MANIFEST="$ROOT/bench/repos.tsv"
OUT="$ROOT/BENCHMARK.md"
TO_STDOUT=0
[ "${1:-}" = "--stdout" ] && TO_STDOUT=1

# Prefer an explicit binary, then a release build, then whatever is on PATH.
CTX="${CTX:-}"
if [ -z "$CTX" ]; then
  if [ -x "$ROOT/target/release/ctx" ]; then CTX="$ROOT/target/release/ctx"
  elif [ -x "$ROOT/target/debug/ctx" ]; then CTX="$ROOT/target/debug/ctx"
  else CTX="$(command -v ctx || true)"; fi
fi
[ -n "$CTX" ] && [ -x "$CTX" ] || { echo "no ctx binary; cargo build --release or set CTX=" >&2; exit 1; }

# The estimator, applied identically to both sides of every comparison.
est_tokens () { awk 'END { printf "%d", int(NR_BYTES/3) }' NR_BYTES="$(wc -c)" </dev/null; }
tokens_of () { local n; n=$(wc -c); echo $(( n / 3 )); }

# The grep a competent agent would actually write: call syntax, language
# filtered, skipping the directories ctx skips. Not a naive `grep -rn name`,
# because beating a strawman proves nothing.
# Searched from INSIDE the tree, against `.`, so hits read `./src/x.rs:12:` and
# not `/wherever/the/cache/lives/src/x.rs:12:`. The cost of an answer is counted
# in bytes, so an absolute path would bill grep for the length of a directory
# name that has nothing to do with either tool — and would make the figure
# depend on where the corpus happened to be cloned. Measured on ripgrep, the
# prefix alone was 9,322 tokens, 21% of that row. It also biased the comparison
# in ctx's favour, since ctx already reports paths relative to its root.
fair_grep () {
  local dir="$1" sym="$2"
  ( cd "$dir" 2>/dev/null || return 0
    grep -rnE "\\b${sym}\\(" . \
      --include='*.rs' --include='*.py' --include='*.ts' --include='*.tsx' --include='*.go' \
      --exclude-dir=.git --exclude-dir=node_modules --exclude-dir=target \
      --exclude-dir=__pycache__ --exclude-dir=.venv --exclude-dir=venv \
      --exclude-dir=dist --exclude-dir=build 2>/dev/null || true )
}

# Fetch one pinned commit, shallow. Cached, because re-cloning on every run
# makes a benchmark nobody runs twice.
fetch_repo () {
  local name="$1" sha="$2" url="$3" dir="$CACHE/$name"
  if [ -d "$dir/.git" ] && [ "$(git -C "$dir" rev-parse HEAD 2>/dev/null)" = "$sha" ]; then
    return 0
  fi
  rm -rf "$dir"; mkdir -p "$dir"
  git -C "$dir" init -q .
  git -C "$dir" remote add origin "$url"
  git -C "$dir" fetch -q --depth 1 origin "$sha"
  git -C "$dir" checkout -q FETCH_HEAD
}

# The symbols to compare on, chosen mechanically so the selection cannot be
# accused of flattering either tool.
#
# The ranking is done by grep alone — the most frequent call-syntax identifiers
# in the tree — so what gets measured is chosen WITHOUT consulting ctx. That
# deliberately surfaces grep's worst cases, since a common name is exactly where
# scanning lines hurts, so treat the ratios as an upper bound on ctx's advantage
# rather than a typical one. Every row is printed, including the ones ctx loses:
# a tool that reports only its best ratio is advertising, not measuring.
#
# ctx is consulted for one thing only, as a filter: does this repository define
# the name at all? "Who calls `len`" is not a question about this codebase, and
# it is also what keeps language keywords and builtins out without this script
# having to carry a keyword list per language.
pick_symbols () {
  local dir="$1" lang="$2" n="${3:-4}"
  grep -rhoE '\b[A-Za-z_][A-Za-z0-9_]{2,}\(' "$dir" \
      --include='*.rs' --include='*.py' --include='*.ts' --include='*.tsx' --include='*.go' \
      --exclude-dir=.git --exclude-dir=node_modules --exclude-dir=target \
      --exclude-dir=__pycache__ --exclude-dir=.venv --exclude-dir=venv \
      --exclude-dir=dist --exclude-dir=build 2>/dev/null \
    | tr -d '(' | sort | uniq -c | sort -rn | head -80 | awk '{print $2}' \
    | while read -r sym; do
        [ -n "$sym" ] || continue
        # Callables only. `ctx def` also resolves types, and "who calls
        # `Request`" is not a caller question — ctx answers it with signature
        # references, which is a different comparison and would muddy the table.
        if "$CTX" def "$sym" "$dir" --lang "$lang" 2>/dev/null \
             | grep -qE '\[(fn|def)\]'; then
          echo "$sym"
        fi
      done | head -n "$n"
}

emit () {
  local total_repos=0

  cat <<'HEADER'
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

HEADER

  printf '## Internal recall\n\n'
  cat <<'NOTE'
The number ctx reports about itself, measured on code ctx has never seen.
"Could be internal" excludes calls into the standard library or a third-party
package, because no internal edge could exist for those however good the
resolver gets. Misses are the real ones — names defined *in* the tree that ctx
could not pin — and `ctx doctor` names every one of them with a grep to confirm.

NOTE
  printf '| repo | lang | commit | modules | recall | misses | top unpinned names |\n'
  printf '|---|---|---|---|---|---|---|\n'

  while IFS=$'\t' read -r name lang sha url; do
    case "$name" in ''|\#*) continue ;; esac
    fetch_repo "$name" "$sha" "$url" >&2
    local dir="$CACHE/$name" doc
    doc="$("$CTX" doctor "$dir" --lang "$lang" 2>/dev/null || true)"
    local modules recall misses top
    modules=$(printf '%s' "$doc" | sed -n 's/^Modules: \([0-9]*\).*/\1/p' | head -1)
    recall=$(printf '%s' "$doc" | grep -oE '[0-9]+/[0-9]+ = [0-9.]+%' | head -1)
    misses=$(printf '%s' "$doc" | sed -n 's/.*unresolved internal: *\([0-9]*\).*/\1/p' | head -1)
    top=$(printf '%s' "$doc" | sed -n '/What ctx missed/,/^$/p' \
          | grep -E '^ +[0-9]+ +' | head -3 \
          | awk '{printf "%s`%s` ×%s", (NR>1 ? ", " : ""), $2, $1}')
    [ -n "$top" ] || top='none'
    printf '| %s | %s | `%s` | %s | **%s** | %s | %s |\n' \
      "$name" "$lang" "${sha:0:7}" "${modules:-?}" "${recall:-n/a}" "${misses:-?}" "$top"
    total_repos=$((total_repos + 1))
  done < "$MANIFEST"

  printf '\n## Cost of an answer: `ctx callers` vs a fair grep\n\n'
  cat <<'NOTE'
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

NOTE
  printf '| repo | symbol | grep lines | grep tok | ctx tok | ratio | callers | ctx says |\n'
  printf '|---|---|---|---|---|---|---|---|\n'

  local complete_ratios=""
  while IFS=$'\t' read -r name lang sha url; do
    case "$name" in ''|\#*) continue ;; esac
    local dir="$CACHE/$name"
    for sym in $(pick_symbols "$dir" "$lang" 4); do
      local gout ctxout glines gtok ctok ratio found verdict
      gout="$(fair_grep "$dir" "$sym")"
      ctxout="$("$CTX" callers "$sym" "$dir" --lang "$lang" 2>/dev/null || true)"
      glines=$(printf '%s\n' "$gout" | grep -c . || true)
      gtok=$(printf '%s' "$gout" | tokens_of)
      ctok=$(printf '%s' "$ctxout" | tokens_of)
      found=$(printf '%s' "$ctxout" | sed -n 's/^\([0-9]*\) caller(s).*/\1/p' | head -1)

      # The column that keeps this table honest. A cheap answer is only a win
      # if it is a COMPLETE answer, and ctx is the tool that says which it is.
      if printf '%s' "$ctxout" | grep -q 'may be incomplete'; then
        verdict='incomplete'
      elif printf '%s' "$ctxout" | grep -q '^INCOMPLETE:'; then
        verdict='ambiguous'
      elif printf '%s' "$ctxout" | grep -q 'classified external'; then
        # Every internal call resolved, but the name also collides with a
        # std/third-party one. The caller list is right; grep's line count is
        # mostly a different function that happens to share the spelling.
        verdict='name collision'
      elif printf '%s' "$ctxout" | grep -q 'blast radius is complete'; then
        verdict='**complete**'
      else
        verdict='—'
      fi

      if [ "${ctok:-0}" -gt 0 ]; then
        ratio=$(awk -v g="$gtok" -v c="$ctok" 'BEGIN{printf "%.1f", g/c}')
        [ "$verdict" = '**complete**' ] && complete_ratios="$complete_ratios $ratio"
      else
        ratio='n/a'
      fi
      printf '| %s | `%s` | %s | %s | %s | %sx | %s | %s |\n' \
        "$name" "$sym" "$glines" "$gtok" "$ctok" "$ratio" "${found:-0}" "$verdict"
    done
  done < "$MANIFEST"

  printf '\n'
  cat <<'WARN'
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

WARN
  if [ -n "$complete_ratios" ]; then
    printf '%s' "$complete_ratios" | tr ' ' '\n' | grep -E '^[0-9]' \
      | sort -n | awk '
        { a[NR] = $1 }
        END {
          if (NR == 0) exit
          med = (NR % 2) ? a[(NR+1)/2] : (a[NR/2] + a[NR/2+1]) / 2
          printf "Over the %d `complete` answers above, the median is **%.1fx** fewer tokens than the fair grep.\n\n", NR, med
          printf "The median, not the best case or the range. Individual ratios here span two orders of\n"
          printf "magnitude, and the largest of them are the least trustworthy — a ratio is only as good\n"
          printf "as the completeness verdict beside it, and the widest gaps in this corpus are the rows\n"
          printf "where ctx saw least. Read the table, not this line.\n"
        }'
  fi

  printf '\n---\n\n'
  printf 'Generated by `./bench/run.sh` from `bench/repos.tsv` (%s repositories).\n' "$total_repos"
  printf 'ctx version: `%s`.\n' "$("$CTX" --version 2>/dev/null | head -1)"
}

if [ "$TO_STDOUT" = 1 ]; then
  emit
else
  emit > "$OUT"
  echo "wrote $OUT" >&2
fi
