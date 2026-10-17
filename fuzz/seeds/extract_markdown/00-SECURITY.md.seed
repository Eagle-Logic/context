# Security Policy

## Reporting a vulnerability

Please report privately, not as a public issue:

**[Open a private security advisory](https://github.com/Eagle-Logic/context/security/advisories/new)**

That channel is visible only to the maintainers until a fix is published, and it
lets us prepare a patched release before the details are public.

Expect an acknowledgement within a few days. `ctx` is maintained by one person,
so please allow reasonable time before disclosing publicly.

## Supported versions

The latest release on [crates.io](https://crates.io/crates/code-context) is the
supported one. Fixes ship as a new version rather than as patches to older
tags — `cargo install code-context` or `brew upgrade ctx` gets you current.

## Scope

`ctx` is a local, read-only static-analysis CLI. It parses source files and
prints text. It does not write to the files it analyses, execute the code it
parses, or make network requests. That shape rules out most of what a security
report usually concerns, so the areas actually worth reporting are:

- **Reading outside the intended tree.** The MCP server pins itself to a root
  (its working directory unless `--root` says otherwise) and refuses a `path`
  that resolves outside it, including via `..` or a symlink. A way around that
  is a vulnerability.
- **Unbounded output over MCP.** Every tool whose output scales with repo size
  is budgeted, because an MCP result lands directly in a model's context. A tool
  call that evades its budget is a vulnerability — it costs the caller real
  money and can crowd out a context window.
- **Crashes or hangs on hostile input.** `ctx` runs over whatever source it is
  pointed at, which may not be trusted. A file that causes a panic, an infinite
  loop, or unbounded memory growth is worth reporting.
- **Anything that makes `ctx` write, execute, or transmit.** It should do none
  of those. A path that does is a bug of a different order than the rest.

## What is not a vulnerability

- **Wrong or missing call edges.** `ctx` resolves what it can prove and reports
  what it could not — that is `ctx doctor`'s whole job. An unresolved edge is a
  coverage gap; please file it as a normal issue with a repro.
- **Reading a file you pointed it at.** The CLI walks what you ask it to walk.
  That is user intent, not escape. The containment guarantee above is specific
  to the MCP server, where a model rather than a person chooses the path.

## Dependencies

`cargo audit` runs on every push and pull request via CI, and Dependabot opens
updates weekly. If you find an advisory affecting a pinned dependency that CI
has not caught, please report it through the advisory link above.
