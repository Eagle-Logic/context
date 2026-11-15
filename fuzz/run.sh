#!/usr/bin/env bash
# Fuzz the extractors. See fuzz/README.md for what this tests and why.
#
#   ./fuzz/run.sh                 # every target, 60s each
#   ./fuzz/run.sh 300             # every target, 300s each
#   ./fuzz/run.sh 300 extract_go  # one target
#
# The mkdir is not incidental. libFuzzer refuses to start if a corpus
# directory it was handed does not exist, and `fuzz/corpus/` is .gitignored —
# so a fresh clone hits that error on the first run unless something creates
# the directory. Passing the live corpus first and the committed seeds second
# is also deliberate: libFuzzer reads every directory but writes new inputs
# only to the first, which keeps `fuzz/seeds/` reproducible instead of
# accumulating whatever this machine happened to discover.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

SECS="${1:-60}"
shift || true
TARGETS=("$@")
if [ ${#TARGETS[@]} -eq 0 ]; then
  TARGETS=(extract_rust extract_python extract_typescript extract_go extract_java extract_markdown root_containment build_graph)
fi

# `cargo fuzz` needs nightly for -Zsanitizer=address and does not select it
# itself; without +nightly the build fails with "the option `Z` is only
# accepted on the nightly compiler".
if ! rustup toolchain list | grep -q '^nightly'; then
  echo "no nightly toolchain: rustup toolchain install nightly" >&2
  exit 1
fi
if ! cargo +nightly fuzz --version >/dev/null 2>&1; then
  echo "cargo-fuzz not installed: cargo install cargo-fuzz" >&2
  exit 1
fi

failed=()
for t in "${TARGETS[@]}"; do
  echo "=== $t (${SECS}s) ==="
  mkdir -p "fuzz/corpus/$t"
  if cargo +nightly fuzz run "$t" "fuzz/corpus/$t" "fuzz/seeds/$t" \
      -- -max_total_time="$SECS" -print_final_stats=1; then
    :
  else
    failed+=("$t")
  fi
done

if [ ${#failed[@]} -gt 0 ]; then
  echo >&2
  echo "FAILED: ${failed[*]}" >&2
  echo "Crashing inputs are in fuzz/artifacts/<target>/; replay with:" >&2
  echo "  cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<file>" >&2
  exit 1
fi
echo
echo "all ${#TARGETS[@]} target(s) clean at ${SECS}s each"
