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
import json, pathlib, re, subprocess, sys

CTX = "./target/release/ctx"
BYTES_PER_TOKEN = 3  # crate::BYTES_PER_TOKEN


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

print(
    f"README standing-cost figures current: snippet {snippet}, "
    f"MCP {total:,} ({tools:,}+{instructions}), ratio {ratio:.1f}x"
)
