# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

_Nothing yet._

## [0.1.0] - 2026-08-04

Initial public release. Actime is a unified runtime for AI coding agents: it
runs an unmodified agent and wraps it in four planes -- isolation, policy,
evidence, and history. The sandbox contains; Actime accounts.

### Added

- **Four-plane architecture.** Isolation (sandbox), policy (ActPlane), evidence
  (AgentSight), and history (Akeep). Policy and evidence are eBPF programs that
  attach to the sandbox's process tree **from the host**, so the agent cannot
  disable them or edit the record. Actime drives `actplane` ≥ 0.1.8,
  `agentsight` ≥ 0.2.60, and `akeep` ≥ 0.2.0.
- **`actime` CLI** ([DESIGN.md §10](./docs/DESIGN.md#10-cli-surface)):
  `init`, `run`, `shell`, `attach`, `status`, `runs`, `report`, `policy`,
  `keep`, `sandbox`, `doctor`, and `demo`, with global `--config`, `--profile`,
  and `--quiet` flags.
- **`actime run -- <cmd>...`** orchestration that resolves config, creates the
  run directory, starts the sandbox, attaches the policy and evidence planes,
  spawns the agent, and on exit writes a manifest and a rendered report. Every
  step is fail-soft except the policy plane in `enforce` mode, which fails
  closed. `--fail-on-violation` forces exit code 3 when a rule blocked or
  killed an action; `--timeout` enforces a wall-clock limit.
- **Four sandbox backends:** `docker`, `podman`, `bwrap`, and `host`, with
  `auto` probing in that order. Every backend works unprivileged. In 0.1.0 the
  eBPF planes attach on `docker`, `podman`, and `host`; under `bwrap` they are
  disabled because no host pid exists before the agent starts.
- **Three built-in profiles** ([DESIGN.md §9](./docs/DESIGN.md#9-profiles)):
  `observe` (nothing blocked), `balanced` (default; enforces the
  `coding-agent-baseline` pack), and `strict` (refuses the `host` backend,
  enforces `coding-agent-baseline` + `no-vcs-write` + `no-secret-egress`,
  network `egress` with an allowlist, a 4h wall-clock limit, and evidence
  configured for OTLP export).
- **Three policy packs**, embedded in the binary: `coding-agent-baseline`
  (system fence, evidence integrity, credential reporting, destructive VCS,
  mass deletion), `no-vcs-write` (the agent edits, the human publishes), and
  `no-secret-egress` (data labeled from secret files may not reach the
  network). `actime policy list / show / check / explain` inspect and compile
  them without loading anything.
- **Configuration** via `actime.yaml` with a four-layer resolution order
  (`--config`, project file walking to the git root, `~/.config/actime/`, then
  the built-in `balanced` profile) and per-flag CLI overrides. Every field is
  optional; `actime run -- claude` works with no config file at all. The
  `evidence.capture` / `export` / `redact` fields are parsed and recorded in
  0.1.0 but not yet passed to AgentSight.
- **Run store** at `~/.local/share/actime/runs/<run-id>/` (override with
  `ACTIME_HOME`) with a manifest, the effective config, the policy as loaded
  (`policy.yaml` + `policy.dsl`), `violations.jsonl`, the AgentSight SQLite
  store, engine logs, and `report.md`. Every run produces a manifest and a
  report, even when only the process-level fallback ran.
- **Reports** in text, Markdown, and JSON via `actime report`, with plane
  states, summary counters aggregated defensively from the evidence database,
  a violation table, and next-step commands.
- **`actime doctor`** fail-soft environment check (OS, kernel, BTF,
  privileges, engine versions, run store, config, sandbox backends), with
  per-check fixes and `--json` output.
- **`actime demo`** end-to-end run of a bundled stand-in agent. Default
  `--policy enforce` fails closed without a working policy plane;
  `--policy observe` needs no agent, no root, and no Docker, and falls back to
  the host backend when the sandbox image is unavailable.
- **Degradation matrix** ([DESIGN.md §8](./docs/DESIGN.md#8-degradation-matrix)):
  missing root/`CAP_BPF`, Docker/Podman, any of the three engine binaries, or a
  kernel older than 5.10 each degrade specific planes without aborting the run.
- **Default sandbox image** (`sandbox/Dockerfile`): Debian `bookworm-slim` with
  git, ripgrep, jq, build tools, Python 3, and Node.js 22; a non-root `agent`
  user; optional preinstall of Claude Code, Codex, and Gemini CLI
  (`INSTALL_AGENTS=false` to skip).
- **One-line installer** (`scripts/install.sh`) for Linux x86_64 and aarch64,
  with SHA-256 verification and version pinning via `ACTIME_VERSION`.
- **CI** (fmt, clippy with `-D warnings`, build, test, MSRV 1.82 build,
  shellcheck, sandbox image build), **release** workflow (cross-compiled
  tarballs with `.sha256` sidecars and a published sandbox image on
  `ghcr.io/eunomia-bpf/actime-sandbox`), **weekly `cargo audit`**, and
  **Dependabot** for cargo and GitHub Actions.
- **Documentation:** `docs/quickstart.md`, `docs/configuration.md`,
  `docs/sandbox.md`, `docs/policies.md`, `docs/evidence.md`, `docs/faq.md`,
  and the implementation contract `docs/DESIGN.md`.
- **Community files:** `CONTRIBUTING.md`, `SECURITY.md` (report to
  security@eunomia.dev or a GitHub Security Advisory), `CODE_OF_CONDUCT.md`
  (Contributor Covenant 2.1), issue/PR templates, and this changelog.

[Unreleased]: https://github.com/eunomia-bpf/actime/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/eunomia-bpf/actime/releases/tag/v0.1.0
