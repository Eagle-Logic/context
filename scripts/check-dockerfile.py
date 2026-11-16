#!/usr/bin/env python3
"""Guard the two facts in the Dockerfile that drift silently.

The Dockerfile exists for directory checks (Glama starts the server in a
container and sends an introspection request) and for trying the server without
a Rust toolchain. Nothing else in CI builds it, because a docker build costs
minutes for an artifact that is not on anyone's install path — so the two values
that can go stale are checked here instead, for free.

1. **The builder's Rust version matches `rust-version` in Cargo.toml.** The
   README promises 1.90+ and the `msrv` CI job checks the crate against it. A
   Dockerfile pinned to an older image fails to build the moment the crate uses
   a newer feature; pinned to a newer one it builds code the MSRV job says
   should not compile, which is worse, because the image would work while the
   promise it was pinned to had quietly stopped being true.

2. **The entrypoint still runs the MCP server.** The whole point of the image is
   that a directory check can start it and introspect it. An entrypoint edited
   to any other subcommand passes a docker build and fails the listing, days
   later, with nothing pointing here.
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
problems = []

dockerfile = (ROOT / "Dockerfile").read_text(encoding="utf-8")
cargo = (ROOT / "Cargo.toml").read_text(encoding="utf-8")

msrv = re.search(r'^rust-version = "([^"]+)"', cargo, re.M).group(1)
pinned = re.search(r"^FROM rust:([0-9.]+)", dockerfile, re.M)

if not pinned:
    problems.append(
        "Dockerfile has no `FROM rust:<version>` builder stage, so the Rust "
        "version it builds with cannot be checked against the MSRV."
    )
elif pinned.group(1) != msrv:
    problems.append(
        f"Dockerfile builds with rust:{pinned.group(1)}, but Cargo.toml's "
        f"rust-version is {msrv}. Keep them equal: the README promises {msrv}+ "
        "and the msrv CI job checks that promise."
    )

entrypoint = re.search(r'^ENTRYPOINT \[(.+)\]', dockerfile, re.M)
if not entrypoint:
    problems.append("Dockerfile has no ENTRYPOINT, so the image starts nothing.")
else:
    parts = re.findall(r'"([^"]*)"', entrypoint.group(1))
    if parts[:2] != ["ctx", "mcp"]:
        problems.append(
            f"Dockerfile ENTRYPOINT is {parts}, which does not start the MCP "
            "server. A directory check starts this image and sends an "
            "introspection request; anything else fails the listing."
        )

if problems:
    print("Dockerfile is WRONG:")
    for p in problems:
        print(f"  - {p}")
    sys.exit(1)

print(f"Dockerfile current: builds with rust:{msrv}, entrypoint starts `ctx mcp`")
