#!/usr/bin/env python3
"""Guard the facts that make the MCP registry listing and the npm route work.

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

# 4. The npm package name the README tells people to run must be the one dist
#    actually publishes. The name is assembled from two files — `npm-scope` in
#    dist-workspace.toml and `npm-package` in Cargo.toml — and spelled out a
#    third time in README.md, so changing the scope or the package silently
#    leaves the README advertising a package that does not exist. The install
#    line is the first thing a reader runs, and `npx` on a wrong name fails
#    with a 404 rather than anything that points at the cause.
#
#    Deliberately NOT asserting an npm entry in server.json: the registry
#    proves npm ownership from an `mcpName` field in the published
#    package.json, and dist's package.json template has no seam for a custom
#    field, so the npm package cannot currently carry it. Declaring the package
#    in server.json before that is solved would fail the whole publish and
#    strand the cargo listing with it.
dist_cfg = (ROOT / "dist-workspace.toml").read_text(encoding="utf-8")
scope_match = re.search(r'^npm-scope = "([^"]+)"', dist_cfg, re.M)
pkg_match = re.search(r'^npm-package = "([^"]+)"', cargo, re.M)

if scope_match and pkg_match:
    npm_name = f"{scope_match.group(1)}/{pkg_match.group(1)}"
    if npm_name not in readme:
        problems.append(
            f"dist publishes the npm package `{npm_name}`, but README.md never "
            "names it. The install instructions would point at nothing."
        )
elif scope_match or pkg_match:
    problems.append(
        "npm-scope (dist-workspace.toml) and npm-package (Cargo.toml) must be "
        "set together; one without the other publishes under a name nobody "
        "documented."
    )
else:
    npm_name = None

for pkg in server.get("packages", []):
    if pkg.get("registryType") == "npm":
        problems.append(
            "server.json declares an npm package. The registry proves npm "
            "ownership from an `mcpName` field in the published package.json, "
            "which dist does not emit — verify the published package carries "
            "it before listing it here, or the publish fails entirely."
        )

if problems:
    print("server.json / README.md registry metadata is WRONG:")
    for p in problems:
        print(f"  - {p}")
    sys.exit(1)

print(
    f"server.json current: {name} {crate_version}, "
    "ownership token present as visible text"
)
if npm_name:
    print(f"npm route current: README documents {npm_name}")
