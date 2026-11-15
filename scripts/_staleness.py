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
