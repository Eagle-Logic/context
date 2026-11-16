# A container that runs ctx's MCP server over stdio.
#
# Why this exists: Glama (glama.ai/mcp/servers) is the directory that
# awesome-mcp-servers requires a listing in, and its check starts the server in
# a container and sends an introspection request. It is also the honest way to
# let someone try the server without installing a Rust toolchain — the same
# reason the npm package exists.
#
# Not the recommended install. `brew install eagle-logic/tap/ctx`,
# `cargo install code-context`, `npx @eagle-logic/ctx` and the installer scripts
# all give a faster, smaller result. A container is here because a directory
# check needs one.

# ---- build ----------------------------------------------------------------
# Pinned to the MSRV the README promises and CI checks, so this image cannot
# quietly start requiring a newer toolchain than the crate claims to support.
FROM rust:1.90-slim-bookworm AS build

# The tree-sitter grammars are C, compiled by the `cc` crate, so the build needs
# a C compiler. A pure-Rust crate would not — see the same note in
# dist-workspace.toml for the musl target.
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Copy the manifests first so dependency compilation caches independently of
# source edits. `--locked` for the same reason CI uses it: the lockfile is the
# build, and resolving afresh here would mean this image is not what was tested.
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --locked && strip target/release/ctx

# ---- runtime --------------------------------------------------------------
# The binary has no runtime dependencies beyond libc: the grammars are compiled
# in. `-slim` rather than `scratch` because the gnu target links glibc; the
# static musl build in dist-workspace.toml is the one for scratch images.
FROM debian:bookworm-slim

# Not root. The server only ever reads, but a directory check runs this image
# with an arbitrary workspace mounted, and nothing here needs write access.
RUN useradd --create-home --uid 10001 ctx
USER ctx

COPY --from=build /src/target/release/ctx /usr/local/bin/ctx

# The root ctx analyses. Mount the tree to inspect here. No image is published,
# so build it first — the install routes in README.md are the supported ones:
#   docker build -t ctx-mcp .
#   docker run --rm -i -v "$PWD:/workspace:ro" ctx-mcp
WORKDIR /workspace

# stdio transport: the server speaks newline-delimited JSON-RPC on stdin/stdout,
# so `docker run -i` is required and there is no port to expose. `--root .`
# pins the scan to the mounted workspace and refuses paths escaping it.
ENTRYPOINT ["ctx", "mcp", "--root", "."]
