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
from _staleness import refuse_if_stale
import pathlib, re, subprocess, sys

CTX = "./target/release/ctx"
PINNED = ("changed", "move-plan", "parity")
CHECK = "--check" in sys.argv


refuse_if_stale(CTX)

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


def rewrite_prose_figures(text: str) -> str:
    """Derive every figure EXAMPLES.md states in prose rather than in a block.

    The code blocks are all regenerated, so the four figures quoted in the
    surrounding sentences were the only published numbers in this file that
    nothing checked. All four drifted the moment `src/lib.rs` and `fuzz/` were
    added — the header still claimed 22 modules against a real 31 — and
    `--check` passed throughout, because `--check` only ever compared blocks.

    A figure that appears in prose is not a different kind of figure. This is
    the same duplicated-fact bug the extension list had in five places; the fix
    is the same one, which is to derive it.
    """
    doc = subprocess.run([CTX, "doctor"], capture_output=True, text=True)
    if doc.returncode != 0:
        print(f"ctx doctor failed:\n{doc.stderr}", file=sys.stderr)
        sys.exit(1)

    mods = re.search(r"^Modules: (\d+)\s+\(([^)]*)\)", doc.stdout, re.M)
    sites = re.search(r"^call sites:\s+(\d+)", doc.stdout, re.M)
    if not mods or not sites:
        print("could not read module/call-site counts from `ctx doctor`", file=sys.stderr)
        sys.exit(1)

    total = int(mods.group(1))
    per_lang = [(k, int(v)) for k, v in (p.rsplit(" ", 1) for p in mods.group(2).split(", "))]

    # The sentence says "files" per language and "modules" in total, which is
    # only the same number while one file is one module. Go breaks that — a Go
    # module is a package directory with many files — so if this repo ever
    # gains Go or Java, fail here rather than publish the wrong noun.
    if sum(n for _, n in per_lang) != total:
        print(
            f"per-language counts {per_lang} do not sum to {total} modules: a language "
            "here maps many files to one module, so 'files' is now the wrong word",
            file=sys.stderr,
        )
        sys.exit(1)

    display = {
        "rust": "Rust",
        "python": "Python",
        "typescript": "TypeScript",
        "go": "Go",
        "java": "Java",
        "markdown": "Markdown",
    }
    parts = ", ".join(
        f"{n} {display.get(k, k)} file{'s' if n != 1 else ''}"
        for k, n in sorted(per_lang, key=lambda kv: -kv[1])
    )
    new = (
        f"The repo under analysis: **{parts}, {total} modules,\n"
        f"{int(sites.group(1)):,} call sites.**"
    )
    recall = re.search(r"^\s+(\d+)/(\d+) = ([\d.]+)%", doc.stdout, re.M)
    external = re.search(r"^\s+external \(provable\):\s+(\d+)", doc.stdout, re.M)
    if not recall or not external:
        print("could not read recall/external counts from `ctx doctor`", file=sys.stderr)
        sys.exit(1)

    # The miss list is what "the exact grep list" refers to, so its length has
    # to come from the list itself rather than from someone's memory of it.
    missed = re.search(
        r"^## What ctx missed.*?\n(?:.*?\n)??((?:\s+\d+\s+\S+\n)+)",
        doc.stdout,
        re.M | re.S,
    )
    if not missed:
        print("could not read the miss list from `ctx doctor`", file=sys.stderr)
        sys.exit(1)
    n_missed = len(missed.group(1).strip().split("\n"))

    pct = float(recall.group(3))
    words = {
        1: "one", 2: "two", 3: "three", 4: "four", 5: "five", 6: "six",
        7: "seven", 8: "eight", 9: "nine", 10: "ten", 11: "eleven",
        12: "twelve", 13: "thirteen", 14: "fourteen", 15: "fifteen",
    }

    edits = [
        (
            r"The repo under analysis: \*\*.*?call sites\.\*\*",
            new,
            "the 'repo under analysis' sentence",
        ),
        (
            r"[\d.]+% recall comes with \*\*the exact grep list for the other "
            r"[\d.]+%\*\* — \S+\nnames,",
            f"{pct:g}% recall comes with **the exact grep list for the other "
            f"{100 - pct:.1f}%** — {words.get(n_missed, str(n_missed))}\nnames,",
            "the grep-list sentence",
        ),
        (
            r"The denominator is honest too\. [\d,]+ of [\d,]+ call sites",
            f"The denominator is honest too. {int(external.group(1)):,} of "
            f"{int(sites.group(1)):,} call sites",
            "the denominator sentence",
        ),
        (
            r"counts\n> toward the \d+ modules above",
            f"counts\n> toward the {total} modules above",
            "the Markdown-is-a-node note",
        ),
    ]
    for pattern, value, what in edits:
        text, n = re.subn(pattern, lambda _, v=value: v, text, count=1, flags=re.S)
        if n != 1:
            print(f"could not find {what} in EXAMPLES.md", file=sys.stderr)
            sys.exit(1)
    return text


fresh = rewrite_prose_figures(fresh)


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
