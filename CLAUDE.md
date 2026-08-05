# CLAUDE.md

Guidance for coding agents working in this repository. `AGENTS.md` is a symlink
to this file so Claude Code, Codex, and others share the same instructions.

## Overview

Actime is a unified runtime for AI coding agents. It runs an unmodified agent
inside a sandbox and wraps it in four planes: isolation (the sandbox), policy
([ActPlane](https://github.com/eunomia-bpf/ActPlane), eBPF), evidence
([AgentSight](https://github.com/eunomia-bpf/agentsight), eBPF), and history
([Akeep](https://github.com/eunomia-bpf/akeep)).

The policy and evidence planes run on the **host**, attached to the sandbox's
process tree. That is the load-bearing design decision: the agent cannot
disable the recorder or edit the record. Keep it that way.

**`docs/DESIGN.md` is the implementation contract.** Read it before changing
any public type, CLI flag, config field, or file layout, and update it in the
same change when the contract itself moves.

## Build and test

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all

cargo run -p actime -- demo          # end-to-end smoke test, no root needed
cargo run -p actime -- doctor
```

The three engines Actime drives are separate projects. Install them to exercise
the full pipeline:

```bash
cargo install actplane agentsight akeep
```

## Architecture

```
crates/actime-core/      config, components, run store, evidence, reports, doctor
crates/actime-sandbox/   Sandbox trait + docker / podman / bwrap / host backends
crates/actime-cli/       the `actime` binary and the run orchestration
policies/                ActPlane DSL packs, embedded into the binary
profiles/                observe / balanced / strict, embedded into the binary
sandbox/                 Dockerfile for the default agent sandbox image
```

`actime-core` must not depend on `actime-sandbox`. The sandbox backend is a
plain `String` in the config, and the CLI maps it to a `Backend`. This keeps
the config crate usable without pulling in process-spawning code.

Policy packs and profiles are `include_str!`d into the binary by
`crates/actime-cli/src/embedded.rs`, so a single downloaded binary works with
nothing else on disk. If you add a pack, add it to `PACKS` there too.

## Rules that are easy to get wrong

- **Fail soft everywhere except `enforce`.** A missing engine, no root, an old
  kernel, no Docker — none of these may abort a run. They degrade a plane and
  record the reason in the manifest. The one exception is `policy.mode:
  enforce`, which fails closed if the policy plane cannot load.
- **Every run produces a manifest and a report**, even when only the
  process-level fallback ran. Nothing in the exit path may be conditional on a
  plane having worked.
- **`${WORKSPACE}` substitution** happens once, in `compose_policy`. Policy
  files must never hardcode a path, because the same policy has to work at
  `/workspace` in a sandbox and at the real path in host mode.
- **Attach before the agent runs.** `Sandbox::start()` brings the sandbox up
  without the agent so the planes can attach to `host_pid()` first. Do not
  collapse `start()` back into `spawn()`.
- **AgentSight's SQLite schema is not a stable interface.** Query
  `sqlite_master` and `PRAGMA table_info` before selecting. A schema we do not
  recognize degrades to zero counters; it never fails the report.
- **No unwrap/expect/panic in non-test code.** Return `anyhow::Result`.

## Policy changes

`policies/*.dsl` must compile. Validate against the real compiler before
committing:

```bash
sed 's|${WORKSPACE}|/workspace|g' policies/coding-agent-baseline.dsl > /tmp/p.dsl
actplane --rule "$(cat /tmp/p.dsl)" compile --json | jq .warnings
```

Warnings must be empty. In particular, `block exec` with an argv token cannot
deny before the fact — argv is only available after exec — so argv-sensitive
rules use `kill`.

Write `because` clauses as instructions to a capable colleague. They are
delivered to the agent verbatim as corrective feedback, so they should say what
to do instead, not just what went wrong.

## Documentation

Keep the README Quick Start stable; it is the first thing anyone reads. Put
mode-specific behavior, storage layout, and operational caveats in `docs/`.
When a CLI flag or config field changes, update `docs/configuration.md` in the
same change. Never document a flag that does not exist.
