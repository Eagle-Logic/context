#!/usr/bin/env python3
"""Verify the README's standing-cost table against the binary.

These four numbers are the last hand-copied figures in the repo. They moved
four times in one day — 2.2x, 2.4x, 2.6x, 2.8x — as tools gained ceilings and
the server gained instructions, and each move needed someone to remember to
edit a Markdown table. That is the same shape as every published figure this
project has had to correct.

Not a regenerator: the table is prose with emphasis and thousands separators,
and rewriting it mechanically is more fragile than checking it. Run from the
repo root; the snippet figure is measured for this repo.
"""
from _staleness import DOGFOOD_SCOPE, refuse_if_stale
import json, pathlib, re, subprocess, sys

CTX = "./target/release/ctx"
BYTES_PER_TOKEN = 3  # crate::BYTES_PER_TOKEN

refuse_if_stale(CTX)


def tokens(s: str | bytes) -> int:
    return len(s) // BYTES_PER_TOKEN


def rpc(method: str):
    out = subprocess.run(
        [CTX, "mcp"],
        input=json.dumps({"jsonrpc": "2.0", "id": 1, "method": method}),
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    return json.loads(out)["result"]


snippet = tokens(subprocess.run([CTX, "snippet"], capture_output=True, check=True).stdout)
# json.dumps with default separators, which is how the README's figure was
# measured and reproduced exactly.
tools = tokens(json.dumps(rpc("tools/list")["tools"]))
instructions = tokens(rpc("initialize")["instructions"])
total = tools + instructions
ratio = total / snippet

def doctor_excerpt_errors(readme: str) -> list[str]:
    """Verify the README's `ctx doctor` excerpt against what doctor prints now.

    This block is the README's single most load-bearing claim — it is the
    evidence for "every edge tells you how much to trust it" — and nothing
    checked it. It was stale for at least two releases, reading 1397/1467
    against a real 1486/1559, and it had quietly dropped a low-confidence row
    that belonged between the two it showed.

    An excerpt may be SHORTER than the real output; that is what makes it an
    excerpt. It may not be out of date, and it may not skip a row to make the
    picture tidier — so the quoted rows must be a contiguous PREFIX of the real
    ones, not merely a subset of them. A subset check would have called that
    dropped row fine.

    It must also carry doctor's own `SCOPE:` line. The figures are measured at
    `DOGFOOD_SCOPE`, which excludes the maintenance scripts and so reads HIGHER
    than a whole-repo run. Publishing the narrowed number without the line that
    names the filter would be the exact failure this file exists to prevent, and
    a disclosure that lives only in prose is one edit from being gone. So it is
    quoted from the real output and verified against it like any other row.
    """
    out = subprocess.run([CTX, "doctor", *DOGFOOD_SCOPE], capture_output=True, text=True, check=True).stdout
    real = out.split("\n")

    # The fenced block that holds the excerpt, located from the heading outwards
    # rather than by pairing fences. Pairing with a regex is wrong here: this
    # file has ```sh blocks, whose OPENING fence carries a language tag and so
    # does not match a bare ``` opener, while their CLOSING fence does — which
    # offsets every pair after it and silently captured the wrong block. Walking
    # out from a line that can only occur once is unambiguous.
    lines = readme.split("\n")
    try:
        anchor = next(n for n, ln in enumerate(lines) if ln.startswith("## Internal recall"))
    except StopIteration:
        return ["the `## Internal recall` doctor block is no longer in README.md"]

    opens = [n for n in range(anchor, -1, -1) if lines[n].strip() == "```"]
    closes = [n for n in range(anchor, len(lines)) if lines[n].strip() == "```"]
    if not opens or not closes:
        return ["the `## Internal recall` block in README.md is not inside a ``` fence"]
    quoted = [ln for ln in lines[opens[0] + 1 : closes[0]] if ln.strip()]

    def section(heading: str, row: str) -> list[str]:
        """Rows under `heading` in real output, until the first non-matching line."""
        try:
            i = next(n for n, ln in enumerate(real) if ln.startswith(heading))
        except StopIteration:
            return []
        rows = []
        for ln in real[i + 1 :]:
            if re.fullmatch(row, ln):
                rows.append(ln)
            elif rows or not ln.strip() or not ln.startswith(("#", "grep")):
                if rows:
                    break
        return rows

    MISS = r"\s+\d+\s+\S+"
    ZONE = r"\s+\S+\s+\d+% heuristic \(\d+/\d+ edges\)"
    real_miss = section("## What ctx missed", MISS)
    real_zone = section("## Low-confidence zones", ZONE)

    errors = []

    real_scope = next((ln for ln in real if ln.startswith("SCOPE:")), None)
    quoted_scope = next((ln for ln in quoted if ln.startswith("SCOPE:")), None)
    if real_scope and quoted_scope is None:
        errors.append(
            "the excerpt is missing doctor's `SCOPE:` line. These figures are "
            "measured at a narrowed scope and read higher than a whole-repo "
            f"run, so the disclosure has to travel with them:\n      {real_scope}"
        )
    elif quoted_scope and real_scope != quoted_scope:
        errors.append(
            "the quoted `SCOPE:` line does not match what doctor prints:\n"
            f"      README: {quoted_scope}\n      actual: {real_scope or '(no filter)'}"
        )

    for ln in quoted:
        if ln.startswith("#") or ln.startswith("grep these"):
            if ln not in real:
                errors.append(f"heading no longer printed by `ctx doctor`: {ln.strip()!r}")

    for ln in quoted:
        if re.fullmatch(r"\s+\d+/\d+ = [\d.]+%.*", ln) and ln not in real:
            now = next((r for r in real if re.fullmatch(r"\s+\d+/\d+ = [\d.]+%.*", r)), "(gone)")
            errors.append(f"recall line is stale:\n      README: {ln.strip()}\n      actual: {now.strip()}")

    for label, quoted_rows, real_rows in (
        ("miss", [l for l in quoted if re.fullmatch(MISS, l)], real_miss),
        ("low-confidence", [l for l in quoted if re.fullmatch(ZONE, l)], real_zone),
    ):
        if quoted_rows and quoted_rows != real_rows[: len(quoted_rows)]:
            errors.append(
                f"{label} rows are not the top {len(quoted_rows)} of `ctx doctor`:\n"
                + "      README: "
                + " | ".join(r.strip() for r in quoted_rows)
                + "\n      actual: "
                + " | ".join(r.strip() for r in real_rows[: len(quoted_rows)])
            )
    return errors


readme = pathlib.Path("README.md").read_text()
want = {
    "snippet block": (rf"\|\s*`ctx snippet` block\s*\|\s*\*\*{snippet}\*\*\s*\|", f"**{snippet}**"),
    "tool definitions": (rf"\|\s*MCP tool definitions \(10 tools\)\s*\|\s*{tools:,}\s*\|", f"{tools:,}"),
    "server instructions": (rf"\|\s*MCP server instructions\s*\|\s*{instructions}\s*\|", f"{instructions}"),
    "MCP total": (rf"\|\s*\*\*MCP total\*\*\s*\|\s*\*\*{total:,}\*\*\s*\|", f"**{total:,}**"),
    "ratio (body)": (rf"{ratio:.1f}x, before either has answered", f"{ratio:.1f}x"),
    "ratio (intro)": (rf"{ratio:.1f}× the standing cost", f"{ratio:.1f}×"),
}

bad = [(name, expect) for name, (pat, expect) in want.items() if not re.search(pat, readme)]
if bad:
    print("README standing-cost figures are STALE. Measured now:", file=sys.stderr)
    print(f"  snippet            {snippet}", file=sys.stderr)
    print(f"  MCP tools          {tools:,}", file=sys.stderr)
    print(f"  MCP instructions   {instructions}", file=sys.stderr)
    print(f"  MCP total          {total:,}", file=sys.stderr)
    print(f"  ratio              {ratio:.1f}x  (exact {ratio:.3f})", file=sys.stderr)
    print("\nNot found in README.md:", file=sys.stderr)
    for name, expect in bad:
        print(f"  {name}: expected {expect}", file=sys.stderr)
    sys.exit(1)

doctor_errors = doctor_excerpt_errors(readme)
if doctor_errors:
    print("README `ctx doctor` excerpt is STALE:", file=sys.stderr)
    for e in doctor_errors:
        print(f"  - {e}", file=sys.stderr)
    print(
        "\nRe-copy the excerpt from `./target/release/ctx doctor`, keeping the quoted\n"
        "rows as the top N in order — dropping a row to tidy the picture is the\n"
        "failure this check exists for.",
        file=sys.stderr,
    )
    sys.exit(1)

print(
    f"README standing-cost figures current: snippet {snippet}, "
    f"MCP {total:,} ({tools:,}+{instructions}), ratio {ratio:.1f}x"
)
print("README `ctx doctor` excerpt current: recall line and top rows match")
