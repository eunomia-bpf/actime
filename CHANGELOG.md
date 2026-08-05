# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

_Nothing yet._

## [0.1.0] - 2026-08-04

Initial public release. Actime is the effect plane for AI coding agents: it
attaches three planes -- policy, evidence, and history -- to an agent wherever
that agent already runs. Actime does not manage sandboxes. Bring your own
sandbox, or none at all; Actime attaches to the process tree either way. The
sandbox contains; Actime accounts.

### Added

- **Three-plane architecture.** Policy ([ActPlane](https://github.com/eunomia-bpf/ActPlane)),
  evidence ([AgentSight](https://github.com/eunomia-bpf/agentsight)), and
  history ([Akeep](https://github.com/eunomia-bpf/akeep)), all attached below
  the tool layer at the syscall/effect boundary. There is no isolation plane.
  Actime drives `actplane` ≥ 0.1.8, `agentsight` ≥ 0.2.60, and `akeep` ≥ 0.2.0.
- **Three deployment positions** ([deployment.md](./docs/deployment.md)):
  outside the sandbox on the host (strongest tamper story), inside the sandbox
  for platforms where you do not own the host (E2B, Daytona, AWS AgentCore,
  managed Kubernetes — documented with its honestly weaker tamper story), or
  no sandbox at all. `actime doctor` detects the position and reports missing
  capabilities with fixes.
- **`actime` CLI** ([DESIGN.md §10](./docs/DESIGN.md#10-cli-surface)):
  `init`, `run`, `attach`, `status`, `runs`, `report`, `policy`, `keep`, and
  `doctor`, with global `--config`, `--profile`, and `--quiet` flags. There is
  no `sandbox` subcommand, no `--sandbox` flag, and no `demo` command.
- **`actime run -- <cmd>...`** orchestration that resolves config, creates the
  run directory, composes the policy, launches the agent as a plain host child
  (under `actplane run` when policy is on, so enforcement is launch-time),
  attaches the evidence plane, and on exit harvests violations, updates the
  manifest, commits history, and renders the report. Every step is fail-soft
  except the policy plane in `enforce` mode, which fails closed.
  `--fail-on-violation` forces exit code 3 when a rule blocked or killed an
  action; `--timeout` enforces a wall-clock limit.
- **`actime attach`** binds the policy and evidence planes to something already
  running: `--pid N`, `--comm NAME`, `--container REF` (an existing
  Docker/Podman container, resolved via `inspect`), or `--pod NS/POD` (an
  existing pod on this node, resolved via `kubectl`). Actime never creates,
  starts, stops, or removes containers; a missing target is a clear error.
  Attach binds future events only and does not commit history.
- **Three built-in profiles** ([DESIGN.md §9](./docs/DESIGN.md#9-profiles)):
  `observe` (nothing blocked), `balanced` (default; enforces the
  `coding-agent-baseline` pack), and `strict` (enforces
  `coding-agent-baseline` + `no-vcs-write` — the exec-based packs released
  ActPlane can install — a 4h wall-clock limit, and evidence configured for
  OTLP export).
- **Three policy packs**, embedded in the binary: `coding-agent-baseline`
  (destructive VCS and mass deletion — the exec-based rules that load and
  enforce on ActPlane 0.1.8 today), `no-vcs-write` (the agent edits, the human
  publishes), and `information-flow` (system fence, evidence integrity,
  credential-access reporting, and the `no-secret-egress` rule — data labeled
  from secret files may not reach the network). `actime policy list / show /
  check / explain` inspect and compile them without loading anything.
- **Rule enforceability as a host property.** Before any run, Actime resolves
  which rules in the composed policy the installed ActPlane engine can
  actually install, combining `actplane compile --json` output with the
  engine's known feature budget. `actime policy check` prints the per-rule
  table (rule, effect, enforceable yes/no, missing features) without loading
  anything and without privileges. `--policy enforce` fails closed before the
  agent starts if any requested rule is not enforceable; `--policy observe`
  proceeds but records the unenforceable rules on the manifest
  (`unenforceable_rules`) and prints them in the report.
- **Configuration** via `actime.yaml` with a four-layer resolution order
  (`--config`, project file walking to the git root, `~/.config/actime/`, then
  the built-in `balanced` profile) and per-flag CLI overrides. Every field is
  optional; `actime run -- claude` works with no config file at all. The
  `evidence.capture` / `export` / `redact` fields are parsed and recorded in
  0.1.0 but not yet passed to AgentSight.
- **Run store** at `~/.local/share/actime/runs/<run-id>/` (override with
  `ACTIME_HOME`) with a manifest, the effective config, the policy as loaded
  (`policy.yaml` + `policy.dsl`), harvested `violations.jsonl`, the AgentSight
  SQLite store, engine logs, and `report.md`. Every run produces a manifest
  and a report, even when only the process-level fallback ran.
- **Reports** in text, Markdown, and JSON via `actime report`, with the attach
  target, plane states, summary counters aggregated defensively from the
  evidence database, a violation table, and next-step commands.
- **`actime doctor`** fail-soft environment check (deployment position, OS,
  kernel, BTF, privileges, engine versions, run store, config), with
  per-check fixes and `--json` output.
- **Degradation matrix** ([DESIGN.md §8](./docs/DESIGN.md#8-degradation-matrix)):
  missing root/`CAP_BPF`, any of the three engine binaries, or a kernel older
  than 5.10 each degrade specific planes without aborting the run. Running in
  a container without `CAP_BPF` degrades the same way, with a doctor warning
  that this is deployment B without host-side tamper-resistance.
- **One-line installer** (`scripts/install.sh`) for Linux x86_64 and aarch64,
  with SHA-256 verification and version pinning via `ACTIME_VERSION`.
- **CI** (fmt, clippy with `-D warnings`, build, test, MSRV 1.82 build,
  shellcheck), **release** workflow (cross-compiled tarballs with `.sha256`
  sidecars), **weekly `cargo audit`**, and **Dependabot** for cargo and
  GitHub Actions.
- **Documentation:** `docs/quickstart.md`, `docs/deployment.md`,
  `docs/configuration.md`, `docs/policies.md`, `docs/evidence.md`,
  `docs/faq.md`, and the implementation contract `docs/DESIGN.md`.
- **Community files:** `CONTRIBUTING.md`, `SECURITY.md` (report to
  security@eunomia.dev or a GitHub Security Advisory), `CODE_OF_CONDUCT.md`
  (Contributor Covenant 2.1), issue/PR templates, and this changelog.

### Known limitations

- **The `information-flow` pack is expressible but not enforceable with
  released ActPlane 0.1.8.** On the attach / runtime-delta path Actime uses,
  the engine's feature budget admits exec sink rules (and plain connect) but
  not open/write sink rules or path contains/suffix matchers. Every rule in
  `information-flow` — including `no-secret-egress`, the labeled secret-egress
  rule — therefore compiles but does not install: `actime policy check`
  reports all four rules as not enforceable, `--policy enforce` fails closed
  if the pack is requested, and no default profile includes it. What the
  policy plane enforces today is the exec-based rules in
  `coding-agent-baseline` and `no-vcs-write`. The pack ships so the design is
  inspectable and becomes enforceable unchanged when the engine enables those
  rule classes.
- The `evidence.capture` / `export` / `redact` fields are parsed and recorded
  but not yet passed to AgentSight (the engine records its own defaults).
- `policy.feedback: false` is recorded and shown as `feedback off` in the
  report, but the generated `policy.yaml` still contains the feedback block.

[Unreleased]: https://github.com/eunomia-bpf/actime/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/eunomia-bpf/actime/releases/tag/v0.1.0
