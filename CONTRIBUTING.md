# Contributing to Actime

Thanks for considering a contribution. Actime is the effect plane for AI coding
agents: it attaches three planes (policy, evidence, history) to an agent
wherever that agent already runs. Actime does not manage sandboxes. The
contract for all of it is [docs/DESIGN.md](./docs/DESIGN.md). Read it before
changing anything public.

This document covers the practicalities. Engineers working in the repo should
also read `AGENTS.md` (symlinked to `CLAUDE.md`), which carries the same
engineering invariants.

## The one rule that matters most

**`docs/DESIGN.md` is the implementation contract.** If your change alters a
public type, CLI flag, config field, profile, run-directory layout, or
degradation behavior, you **must** update `docs/DESIGN.md` in the same change.
Documentation (`docs/*.md`) that describes the changed behavior must move with
it. A PR that changes behavior without updating the contract will be sent back.

## Get set up

You need Rust (MSRV 1.85), and optionally the three engines for full
end-to-end runs:

```sh
git clone https://github.com/eunomia-bpf/actime
cd actime
cargo build --workspace
cargo test --workspace
```

The three engines Actime drives are separate projects. Install them to exercise
the full pipeline:

```sh
cargo install actplane agentsight akeep
```

## Build, test, and smoke-check

Run these before pushing. CI runs the same gate:

```sh
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --workspace
cargo test --workspace

cargo run -p actime -- doctor
cargo run -p actime -- run --policy off --no-history -- /bin/echo hi   # smoke test, no root needed
```

The unprivileged smoke run must work on a laptop with no agent installed, no
root, and no engines: it produces a manifest and a report with the unavailable
planes marked `Disabled`, each with a reason. If you break that path, you have
broken the onboarding path.

## Engineering invariants

These are the rules that are easy to get wrong. They are restated from the repo
guidance so they are not missed:

- **Fail soft everywhere except `policy.mode: enforce`.** A missing engine, no
  root, an old kernel -- none of these may abort a run. They degrade a
  plane and record the reason in the manifest. The single exception is `enforce`
  mode, which fails closed if the policy plane cannot load.
- **Every run produces a manifest and a report**, even when only the
  process-level fallback ran. Nothing in the exit path may be conditional on a
  plane having worked.
- **`${WORKSPACE}` substitution** happens once, in `compose_policy`. Policy
  files must never hardcode a path; the same policy has to work at a guest path
  when Actime runs inside a container and at the real host path.
- **Never create containers.** `attach --container` / `--pod` only resolve
  already-existing targets. If the target is missing, error clearly and stop.
- **Harvest after engine exit.** Prefer natural ActPlane exit, then SIGTERM
  with a bounded grace, then SIGKILL. If `events.jsonl` is empty after a kill
  that the kernel already performed, recover the violation from the engine log
  and `policy.dsl`.
- **Do not promise host-side tamper-resistance in deployment B.** When Actime
  runs inside the same container as the agent, root inside can interfere;
  doctor and docs must say so honestly.
- **AgentSight's SQLite schema is not a stable interface.** Query
  `sqlite_master` and `PRAGMA table_info` before selecting. An unrecognized
  schema degrades to zero counters; it never fails the report.
- **No `unwrap` / `expect` / `panic` in non-test code.** Return
  `anyhow::Result`.

## Repository layout

```
crates/actime-core/      config, components, run store, evidence, reports, doctor
crates/actime-cli/       the `actime` binary and run orchestration
policies/                ActPlane DSL packs, embedded into the binary
profiles/                observe / balanced / strict, embedded into the binary
scripts/install.sh       one-line installer
```

`actime-core` must not depend on process-spawning code beyond what doctor and
the report layer need; the CLI owns `run` / `attach` orchestration. Policy
packs and profiles are `include_str!`d into the binary; if you add a pack or
profile, register it in `crates/actime-cli/src/embedded.rs` too.

## Policy changes

`policies/*.dsl` must compile. Validate against the real compiler before
committing:

```sh
sed 's|${WORKSPACE}|/workspace|g' policies/coding-agent-baseline.dsl > /tmp/p.dsl
actplane --rule "$(cat /tmp/p.dsl)" compile --json | jq .warnings
```

Warnings must be empty. In particular, `block exec` with an argv token cannot
deny before the fact (argv is only available after exec), so argv-sensitive
rules use `kill`.

Write `because` clauses as instructions to a capable colleague. They are
delivered to the agent verbatim as corrective feedback, so they should say what
to do instead, not just what went wrong.

## Commit and pull request flow

1. Open a pull request against `main`. Small, focused PRs review faster.
2. Fill in the PR template, including whether the change touches the public
   contract.
3. Make sure CI is green: fmt, clippy with `-D warnings`, build, test,
   shellcheck.
4. Address review feedback in new commits; avoid force-pushing during review
   unless asked. We squash on merge, so your commit history is yours.

We do not require a CONTRIBUTING CLA. Contributions fall under the project's MIT
license.

## Reporting issues

Open an issue using one of the templates. For bugs, include `actime doctor --json`
output, the run id, your kernel version, and the deployment position and attach
target -- the bug report template asks for exactly these.

For security issues, do **not** open a public issue. See [SECURITY.md](./SECURITY.md).

## Code of conduct

By participating you agree to uphold the [Contributor Covenant Code of Conduct,
version 2.1](./CODE_OF_CONDUCT.md). Be excellent to each other.
