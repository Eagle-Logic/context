"""Refuse to publish or check a figure measured by the wrong binary.

Shared by `regen-examples.py` and `check-readme-figures.py`, which between them
write and verify every hand-copied figure in the repo. It lived in the
regenerator alone until a checker was added that could be fooled the same way —
a stale binary makes a check pass against yesterday's truth, which is worse
than no check, because it reports green.

`bench/run.sh` carries the same three checks in shell for the same three
failure modes, and the reasoning is written out there. In short:

  1. `--version` against Cargo.toml is a CONTENT check, and it replaced an
     mtime test that fired on every release — a release commit touches
     Cargo.toml, cargo relinks a cached binary carrying its original mtime, and
     a correct binary read as stale.
  2. Source mtimes stay, because a branch switch can fool cargo's own
     fingerprint, which is the 0.24.0 failure this guard exists for.
"""

import pathlib
import subprocess
import sys

# The scope every published self-measurement is taken at.
#
# ctx's public surface is the CLI and the MCP server — Rust. `scripts/` is the
# maintenance tooling that writes and checks the figures, and ctx resolves it
# badly: the 13 unresolved `group` call sites in the whole-repo report were all
# in here, and `scripts/check-dockerfile.py` scored 0% module recall the day it
# was added. So every edit to a helper moved the README's headline number and
# failed CI until someone re-copied an excerpt, which taught exactly one lesson
# — that the number is noise. A figure that moves for reasons unrelated to the
# thing it measures stops being read.
#
# `--exclude 'scripts/**'` and not `--lang rust`: this keeps every LANGUAGE ctx
# supports in scope, including the Markdown graph, so no extractor is hidden
# from its own report. It removes files, not capabilities. Markdown was measured
# and makes no difference to recall either way (94.4% with it, 94.4% without),
# so there is no case for dropping it beyond the churn, and it is the subject of
# a README section.
#
# This raises the published figure, 94.4% -> 95.1%, which is the direction that
# deserves suspicion. Two things keep it honest: ctx prints `SCOPE: FILTERED`
# naming the exact filter, and that line is part of the README excerpt the
# checker verifies — so the narrowed scope cannot be published without the
# disclosure next to it. The same `--exclude 'scripts/**'` already scopes the
# `changed --api --strict` gate in CI, for the same reason written out there.
#
# What is NOT scoped this way: the broken-link check, which should cover every
# file, and BENCHMARK.md, which measures seven third-party repositories and is
# where ctx's Python quality is actually reported.
DOGFOOD_SCOPE = ["--exclude", "scripts/**"]


def refuse_if_stale(ctx: str) -> None:
    """Exit non-zero unless `ctx` was built from the sources now on disk."""
    binary = pathlib.Path(ctx)
    if not binary.exists():
        print(f"no binary at {ctx}; cargo build --release --locked", file=sys.stderr)
        sys.exit(1)

    manifest = pathlib.Path("Cargo.toml")
    if manifest.exists():
        want = next(
            (
                line.split('"')[1]
                for line in manifest.read_text().splitlines()
                if line.startswith("version = ")
            ),
            None,
        )
        out = subprocess.run([ctx, "--version"], capture_output=True, text=True).stdout.split()
        have = out[1] if len(out) > 1 else None
        if want and want != have:
            print(
                f"stale binary: Cargo.toml says {want}, {ctx} reports {have or 'nothing'}",
                file=sys.stderr,
            )
            print("run: cargo build --release --locked", file=sys.stderr)
            sys.exit(1)

    cutoff = binary.stat().st_mtime
    newer = [str(f) for f in pathlib.Path("src").rglob("*.rs") if f.stat().st_mtime > cutoff]
    if newer:
        print(f"stale binary: {newer[0]} is newer than {ctx}", file=sys.stderr)
        print("run: cargo build --release --locked", file=sys.stderr)
        sys.exit(1)
