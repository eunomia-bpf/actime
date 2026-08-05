# CLAUDE.md

Guidance for coding agents working in this repository. `AGENTS.md` is a symlink
to this file so Claude Code, Codex, and others share the same instructions.

## Overview

Actime is the effect plane for AI coding agents. It attaches three planes to an
agent wherever that agent already runs: policy
([ActPlane](https://github.com/eunomia-bpf/ActPlane), eBPF), evidence
([AgentSight](https://github.com/eunomia-bpf/agentsight), eBPF), and history
([Akeep](https://github.com/eunomia-bpf/akeep)).

Actime does **not** manage sandboxes. Bring your own sandbox, or none at all.
Actime attaches to the process tree either way. Its position relative to a
sandbox is a deployment choice (host-side attach, in-container, or bare host),
not an architectural constraint. The invariant that is always true is that
Actime observes and enforces **below the tool layer**, at the syscall/effect
boundary.

> The sandbox contains. Actime accounts.

That line describes roles, not position. Do not promise host-side
tamper-resistance when Actime runs inside the same container as the agent.

**`docs/DESIGN.md` is the implementation contract.** Read it before changing
any public type, CLI flag, config field, or file layout, and update it in the
same change when the contract itself moves.

## Build and test

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all

cargo run -p actime -- doctor
cargo run -p actime -- run --policy off --no-history -- /bin/echo hi
```

The three engines Actime drives are separate projects. Install them to exercise
the full pipeline:

```bash
cargo install actplane agentsight akeep
```

## Architecture

```
crates/actime-core/      config, components, run store, evidence, reports, doctor
crates/actime-cli/       the `actime` binary and the run orchestration
policies/                ActPlane DSL packs, embedded into the binary
profiles/                observe / balanced / strict, embedded into the binary
```

`actime-core` must not depend on process-spawning code beyond what doctor and
the report layer need. The CLI owns `run` / `attach` orchestration.

Policy packs and profiles are `include_str!`d into the binary by
`crates/actime-cli/src/embedded.rs`, so a single downloaded binary works with
nothing else on disk. If you add a pack, add it to `PACKS` there too.

## Rules that are easy to get wrong

- **Fail soft everywhere except `enforce`.** A missing engine, no root, an old
  kernel — none of these may abort a run. They degrade a plane and record the
  reason in the manifest. The one exception is `policy.mode: enforce`, which
  fails closed if the policy plane cannot load.
- **Every run produces a manifest and a report**, even when only the
  process-level fallback ran. Nothing in the exit path may be conditional on a
  plane having worked.
- **`${WORKSPACE}` substitution** happens once, in `compose_policy`. Policy
  files must never hardcode a path: the same policy must work at a guest path
  and at the real host cwd.
- **Never create containers.** `attach --container` / `--pod` only resolve
  already-existing targets. If the target is missing, error clearly and stop.
- **Harvest after engine exit.** Prefer natural ActPlane exit, then SIGTERM with
  a bounded grace, then SIGKILL. If `events.jsonl` is empty after a kill that
  the kernel already performed, recover the violation from the engine log and
  `policy.dsl` — a tool that enforces and cannot prove it is worse than useless.
- **AgentSight's SQLite schema is not a stable interface.** Query
  `sqlite_master` and `PRAGMA table_info` before selecting. A schema we do not
  recognize degrades to zero counters; it never fails the report.
- **No unwrap/expect/panic in non-test code.** Return `anyhow::Result`.
- **`PlaneStatus` has exactly three fields:** policy, evidence, history. There
  is no isolation plane.

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
same change only when that doc is in scope. Never document a flag that does not
exist. The architecture contract lives in `docs/DESIGN.md`.
