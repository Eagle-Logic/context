#!/usr/bin/env python3
"""Regenerate every non-pinned verbatim block in EXAMPLES.md.

Each block looks like:

    <fence>          # ``` or ```` — the count matters
    $ ctx <argv>
    ...output...
    <fence>          # same backtick count

The closing fence must be matched by EXACT backtick count, not by prefix: the
`--include-source` block is fenced with ```` precisely because its output
contains a ```rust fence, and a prefix match stops at that inner fence and
leaves the remainder of the old output in place. That produced a block with two
`## Referenced by` sections before this was fixed.

The three git-ref-dependent blocks are pinned at 0.20.0 by the file's own
header and are left untouched.
"""
import pathlib, re, subprocess, sys

CTX = "./target/release/ctx"
PINNED = ("changed", "move-plan", "parity")
CHECK = "--check" in sys.argv


def refuse_if_stale() -> None:
    """Refuse to run against a binary that does not match the sources.

    `bench/run.sh` carries the same checks for the same failure modes, and the
    reasoning lives there. In short: `--version` against Cargo.toml is a
    CONTENT check, and it replaced an mtime test that fired on every release —
    a release commit touches Cargo.toml, cargo relinks a cached binary carrying
    its original mtime, and a correct binary read as stale. Source mtimes stay,
    because a branch switch can fool cargo's own fingerprint, which is the
    0.24.0 failure this guard exists for.
    """
    ctx = pathlib.Path(CTX)
    if not ctx.exists():
        print(f"no binary at {CTX}; cargo build --release --locked", file=sys.stderr)
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
        out = subprocess.run([CTX, "--version"], capture_output=True, text=True).stdout.split()
        have = out[1] if len(out) > 1 else None
        if want and want != have:
            print(
                f"stale binary: Cargo.toml says {want}, {CTX} reports {have or 'nothing'}",
                file=sys.stderr,
            )
            print("run: cargo build --release --locked", file=sys.stderr)
            sys.exit(1)

    cutoff = ctx.stat().st_mtime
    newer = [str(f) for f in pathlib.Path("src").rglob("*.rs") if f.stat().st_mtime > cutoff]
    if newer:
        print(f"stale binary: {newer[0]} is newer than {CTX}", file=sys.stderr)
        print("run: cargo build --release --locked", file=sys.stderr)
        sys.exit(1)

refuse_if_stale()

p = pathlib.Path("EXAMPLES.md")
lines = p.read_text().split("\n")
out, i, done, skipped = [], 0, [], []
fence = None  # the most recent opening fence

while i < len(lines):
    line = lines[i]
    stripped = line.strip()
    if stripped and set(stripped) == {"`"}:
        fence = stripped  # an all-backticks line opens or closes a block
    if not line.startswith("$ ctx "):
        out.append(line)
        i += 1
        continue
    if fence is None:
        print(f"no opening fence before: {line}", file=sys.stderr)
        sys.exit(1)
    out.append(line)
    argv = line[2:].split()
    if argv[1] in PINNED:
        skipped.append(" ".join(argv))
        i += 1
        continue
    j = i + 1
    while j < len(lines) and lines[j].strip() != fence:
        j += 1
    if j >= len(lines):
        print(f"no closing {fence} after: {line}", file=sys.stderr)
        sys.exit(1)
    res = subprocess.run([CTX] + argv[1:], capture_output=True, text=True)
    if res.returncode != 0:
        print(f"failed: {line}\n{res.stderr}", file=sys.stderr)
        sys.exit(1)
    out.extend(res.stdout.rstrip("\n").split("\n"))
    done.append(" ".join(argv))
    i = j
fresh = "\n".join(out)


def normalize(text: str) -> str:
    """Blank the one value that is environment-specific, not a figure.

    `doctor` and `core` print the canonicalized scan root in their header, so
    the committed file carries the author's path and CI carries the runner's.
    Comparing those is comparing checkout locations; everything else in the
    file is a measurement. Only the comparison is normalized — regeneration
    still writes real output, because the file's header promises verbatim.
    """
    return re.sub(
        r"^(# (?:ctx coverage report|Core modules) — ).*$",
        r"\1<root>",
        text,
        flags=re.M,
    )


if CHECK:
    if normalize(fresh) == normalize(p.read_text()):
        print(f"EXAMPLES.md is current ({len(done)} blocks checked, {len(skipped)} pinned)")
        sys.exit(0)
    print(
        "EXAMPLES.md is STALE. Rebuild and regenerate:\n"
        "    cargo build --release --locked && python3 scripts/regen-examples.py",
        file=sys.stderr,
    )
    import difflib
    for line in list(
        difflib.unified_diff(
            normalize(p.read_text()).split("\n"), normalize(fresh).split("\n"),
            fromfile="EXAMPLES.md (committed)", tofile="EXAMPLES.md (regenerated)",
            lineterm="", n=1,
        )
    )[:40]:
        print(line, file=sys.stderr)
    sys.exit(1)
p.write_text(fresh)
print("regenerated:", *(f"\n   {c}" for c in done))
print("left pinned:", *(f"\n   {c}" for c in skipped))
