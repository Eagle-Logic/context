#!/usr/bin/env python3
"""Guard the three facts that make the MCP registry listing work.

`server.json` is metadata about a crate that lives somewhere else: the registry
hosts no artifacts, it points at crates.io and then verifies that the crate
really is the one claiming the name. Every part of that check happens on a
server, days after the commit, with the only error surfacing as a failed
release. So the parts that can drift are checked here instead.

1. **The version matches `Cargo.toml`.** The release workflow overwrites it
   from the tag, so a mismatch never reaches the registry — but it would still
   be wrong in the repository, and `server.json` is a file people read to find
   out what ctx is.

2. **`README.md` carries the `mcp-name:` token, matching `server.json`.** This
   is how the registry proves crate ownership for `registryType: cargo`: it
   fetches the README that crates.io renders and looks for the token.

3. **The token is not inside an HTML comment.** This is the trap. For PyPI and
   NuGet the documented form is `<!-- mcp-name: ... -->`, and it works, because
   those registries keep HTML comments. crates.io strips them during
   markdown->HTML conversion, so the commented form renders to nothing and
   verification fails against a README that looks correct in the editor and in
   GitHub's preview. Copying the form from the wrong registry's example is the
   obvious way to break this, so it is the case worth a test.

Run by CI and again by the release workflow before it publishes.
"""

import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
problems = []

server = json.loads((ROOT / "server.json").read_text(encoding="utf-8"))
cargo = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
readme = (ROOT / "README.md").read_text(encoding="utf-8")

crate_version = re.search(r'^version = "([^"]+)"', cargo, re.M).group(1)
if server["version"] != crate_version:
    problems.append(
        f"server.json version is {server['version']}, Cargo.toml is {crate_version}"
    )

for pkg in server.get("packages", []):
    if pkg.get("registryType") == "cargo" and pkg.get("version") != crate_version:
        problems.append(
            f"server.json cargo package version is {pkg.get('version')}, "
            f"Cargo.toml is {crate_version}"
        )

name = server["name"]
token = f"mcp-name: {name}"
if token not in readme:
    problems.append(
        f"README.md does not contain the ownership token `{token}`. "
        "The registry reads it from the crates.io-rendered README."
    )
else:
    # Only a comment that actually encloses the token is a problem: `-->` must
    # come after it with no intervening `<!--`.
    for match in re.finditer(re.escape(token), readme):
        before = readme[: match.start()]
        after = readme[match.end() :]
        opened = before.rfind("<!--") > before.rfind("-->")
        closes = after.find("-->")
        reopens = after.find("<!--")
        if opened and closes != -1 and (reopens == -1 or closes < reopens):
            problems.append(
                f"the `{token}` token is inside an HTML comment. crates.io "
                "strips comments when rendering, so the registry would see "
                "nothing. Make it visible markdown text."
            )
            break

if problems:
    print("server.json / README.md registry metadata is WRONG:")
    for p in problems:
        print(f"  - {p}")
    sys.exit(1)

print(
    f"server.json current: {name} {crate_version}, "
    "ownership token present as visible text"
)
