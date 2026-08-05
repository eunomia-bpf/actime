# Configuration

Actime is configured by a single optional `actime.yaml`. Every field is
optional. `actime run -- claude` with **no config file at all** works, using
the built-in `balanced` profile.

> This document is a complete reference for the schema defined in
> [docs/DESIGN.md §4](./DESIGN.md#4-configuration-actimeyaml), cross-checked
> against `crates/actime-core/src/config.rs`. If a field, value, or flag is not
> listed here, it does not exist. The schema version is `1`.
>
> There is no `sandbox` section. Actime does not manage sandboxes; see
> [deployment.md](./deployment.md) for how Actime composes with yours.

## Resolution order

Actime builds the effective config in layers. The **first file found wins**;
its values are merged over the named profile. The order is:

1. A file passed with `--config <FILE>`.
2. `./actime.yaml` in the current directory, then walking up to the git root.
3. `~/.config/actime/actime.yaml`.
4. The built-in `balanced` profile (always available, no file needed).

`--profile P` starts from that profile instead of a discovered file, so an
explicit `--profile strict` cannot be quietly softened by an `actime.yaml`
sitting in the repository. The other `actime run` flags (see
[CLI overrides](#cli-overrides)) override individual fields on top of
everything else. The fully resolved config for each run is written to
`actime.yaml` inside that run's directory, so you can always see exactly what
was in effect.

If you want a starting point, generate one:

```sh
actime init                    # write actime.yaml from the balanced profile
actime init --profile strict
actime init --print            # print instead of writing
actime init --force            # overwrite an existing actime.yaml
```

## Full annotated reference

```yaml
version: 1
profile: balanced            # observe | balanced | strict | <path to profile yaml>

policy:
  mode: enforce              # off | observe | enforce
  packs:                     # built-in packs from policies/
    - coding-agent-baseline
  files: []                  # extra ActPlane policy files
  feedback: true             # inject corrective feedback the agent can read

evidence:
  enabled: true
  capture: [process, file, network, ssl, resource]
  export: []                 # otlp | sqlite | json (json always written)
  redact: true               # strip auth headers and secret-shaped values

history:
  enabled: true
  commit_on_exit: true
  message: null              # default: "actime run <id>"

limits:
  wall_clock: null           # e.g. "2h": kill the run after this
```

### Top-level

| Field | Type | Default | Meaning |
|-------|------|---------|---------|
| `version` | integer | `1` | Config schema version. Currently `1`. |
| `profile` | string \| path | `balanced` | One of `observe`, `balanced`, `strict`, or a path to a profile yaml. The profile is the base layer that file values are merged over. |

### `policy` -- policy plane (ActPlane)

For `actime run`, the policy plane writes the composed policy into the run
directory (`policy.yaml` for the engine, `policy.dsl` for humans) and launches
the agent under `actplane run` so enforcement is launch-time. For
`actime attach`, it attaches `actplane` to the target's host pid. In `enforce`
mode a failure to start the policy plane aborts the run (fail closed). In
`observe` mode it degrades.

| Field | Type | Default | Meaning |
|-------|------|---------|---------|
| `mode` | enum | `enforce` | `off` = plane disabled. `observe` = log violations, never block. `enforce` = apply effects (notify / block / kill); fail closed on attach error. |
| `packs` | list of pack names | `["coding-agent-baseline"]` | Built-in packs shipped under `policies/` and embedded in the binary: `coding-agent-baseline`, `no-vcs-write`, `no-secret-egress`. |
| `files` | list of paths | `[]` | Extra ActPlane policy files, merged on top of the packs. |
| `feedback` | boolean | `true` | Whether ActPlane should deliver the rule's `because` text to the agent as corrective feedback. In 0.1.0 the generated `policy.yaml` always contains the feedback block; `false` is recorded and shown as `feedback off` in the plane status but does not yet remove it. |

`${WORKSPACE}` in policy files is substituted once, when the policy is
composed, with the workspace path as the agent sees it — the real host path
for `actime run`, or the guest path when Actime runs inside a container
(deployment B). Use it so the same policy file works in both.

### `evidence` -- evidence plane (AgentSight)

The evidence plane records what the agent actually did. In 0.1.0 Actime
invokes it as `agentsight record --no-server --db <run-dir>/evidence.db` with
`--pid <host pid>`, or `--binary-path docker://…` / `k8s://…` when the target
is a container or pod. It is always fail-soft.

| Field | Type | Default | Meaning |
|-------|------|---------|---------|
| `enabled` | boolean | `true` | Master switch. When `false`, the plane is disabled with that reason. |
| `capture` | list | `[process, file, network, ssl, resource]` | Intended capture classes. Accepted and recorded in the run's effective config, but 0.1.0 does not pass them to AgentSight; the engine records its own defaults. |
| `export` | list | `[]` | Intended export sinks (`otlp`, `sqlite`, `json`). Same caveat: recorded but not yet wired to the engine in 0.1.0. |
| `redact` | boolean | `true` | Intended redaction of auth headers and secret-shaped values. Same caveat as `capture`. |

If `agentsight` is not installed or cannot start, the plane is disabled and
Actime still records argv, exit code, and duration (the process-level
fallback).

### `history` -- history plane (Akeep)

The history plane records the agent's session files and makes runs replayable
via Akeep. It runs after the agent exits, with a hard timeout so a stuck vault
can never hang the run. `actime attach` never commits history; the plane shows
`Disabled` with the reason `attach does not commit history`.

| Field | Type | Default | Meaning |
|-------|------|---------|---------|
| `enabled` | boolean | `true` | Master switch. |
| `commit_on_exit` | boolean | `true` | Run `akeep commit` when the run exits. |
| `message` | string | `"actime run <id>"` | Commit message, used verbatim. When unset, the default message contains the run id. |

### `limits`

| Field | Type | Default | Meaning |
|-------|------|---------|---------|
| `wall_clock` | duration string | unset | Kill the run after this wall-clock time, e.g. `"30s"`, `"90m"`, `"2h"`, `"1d"`. Enforced by the orchestrator with SIGTERM, then SIGKILL after a short grace period. |

## Profiles

A profile is a preset base layer. Choose one with `profile:` in `actime.yaml`
or `--profile` on the CLI. File values are merged over the profile.

| Profile | Policy | Evidence | History | Notes |
|---------|--------|----------|---------|-------|
| `observe` | `observe`, `coding-agent-baseline`, feedback off (nothing blocked) | on | on | The onboarding default for a new team. Nothing is ever blocked. |
| `balanced` (default) | `enforce`, `coding-agent-baseline`, feedback on | on | on | Blocks destructive and exfiltration-shaped effects; allows normal development. |
| `strict` | `enforce`, `coding-agent-baseline` + `no-vcs-write` + `no-secret-egress`, feedback on | on, `export: [otlp]` (recorded; not yet wired to the engine in 0.1.0) | on | Also sets `limits.wall_clock: 4h`. Use for sensitive codebases and CI gates. |

## CLI overrides

These `actime run` flags override individual config fields on top of everything
else. First-file-wins for config, then these flags win on top.

| Flag | Config field | Effect |
|------|--------------|--------|
| `--config <FILE>` | (resolution layer 1) | Load config from this file instead of searching. |
| `--profile P` | `profile` | Use profile `P` as the base layer (`observe` / `balanced` / `strict` / path). |
| `--policy MODE` | `policy.mode` | Force policy mode (`off` / `observe` / `enforce`). Also on `actime attach`. |
| `--no-evidence` | `evidence.enabled` | Set to `false`. |
| `--no-history` | `history.enabled` | Set to `false`. |
| `--timeout <DURATION>` | `limits.wall_clock` | Kill the run after this long, e.g. `30m` or `2h`. |
| `--fail-on-violation` | (exit code only) | Any `kill` or `block` violation forces exit code `3`. Does not change what the planes do. |

The exit code of `actime run` is the agent's exit code, except that
`--fail-on-violation` makes a `kill`/`block` violation force exit code `3`.

Global flags that work on every subcommand: `--config <FILE>`,
`--profile <NAME>`, and `-q` / `--quiet` (suppress the banner and progress
lines; the report is still printed).

## Full command surface

Beyond `run`, Actime exposes these commands (from
[DESIGN.md §10](./DESIGN.md#10-cli-surface), checked against `--help`):

```
actime init [--force] [--print]
actime run [--policy MODE] [--no-evidence] [--no-history]
           [--fail-on-violation] [--timeout D] -- <cmd>...
actime attach (--pid N | --comm NAME | --container REF | --pod NS/POD)
              [--policy MODE]
actime status
actime runs [--json] [--limit N]        # default limit 20
actime report [RUN] [--json|--markdown] # RUN defaults to `latest`
actime policy (list | show PACK | check | explain)
actime keep (commit [-m MSG] | log | restore [RUN] [--to DIR])
actime doctor [--json]
```

| Command | Purpose |
|---------|---------|
| `actime init` | Write a starter `actime.yaml` for a profile. `--force` overwrites, `--print` skips writing. |
| `actime run -- <cmd>...` | The main entry point. Everything after `--` is the agent command, run as a host child. No container is created. |
| `actime attach` | Attach the policy and evidence planes to an already-running target: a host pid, a comm name, an existing Docker/Podman container, or an existing pod on this node. Post-hoc: it binds future events only, and never commits history. Actime never creates containers; a missing target is a clear error. |
| `actime status` | List runs that are still in progress. |
| `actime runs` | List recorded runs, newest first. `--json` for machines, `--limit N` to cap. |
| `actime report` | Render a run's report. Accepts a run id or `latest`. `--json` or `--markdown`. |
| `actime policy` | Inspect policy packs: `list`, `show PACK`, `check` (compile the configured policy without loading it), `explain` (what this kernel can enforce before the fact). `check` and `explain` call the installed `actplane` binary and need no privileges. |
| `actime keep` | History operations: `commit` (with `-m MSG`), `log`, `restore RUN [--to DIR]`. All three delegate to the installed `akeep` binary. |
| `actime doctor` | Fail-soft environment check, including which deployment position it detects. `--json` for machines. Exits `0` with warnings, `1` if any check failed. |

There is no `sandbox` subcommand, no `--sandbox` flag, no `demo` command, and
no `shell` command. Isolation is the user's responsibility; see
[deployment.md](./deployment.md).

## Environment variables

- `ACTIME_HOME` -- root of the run store and the engine lookup directory
  (`$ACTIME_HOME/bin`). Default: `~/.local/share/actime`.
- `ACTIME_NONINTERACTIVE` -- when set, Actime never prompts for a sudo
  password; privileged engine launches use `sudo -n` and fail fast instead.
- `NO_COLOR` -- also disables interactive sudo prompts (and color).

Engines are resolved in this order: `PATH`, `$ACTIME_HOME/bin`, `~/.cargo/bin`.

## Examples

Minimal -- there is no file; `actime run -- claude` just works.

Lock down a sensitive repo to `strict`:

```yaml
version: 1
profile: strict
```

Observe-only onboarding with no blocking, custom time budget:

```yaml
version: 1
profile: observe
limits:
  wall_clock: "1h"
```

A team policy file on top of the default pack:

```yaml
version: 1
policy:
  mode: enforce
  packs:
    - coding-agent-baseline
  files:
    - ./team-policy.dsl
```

CI gate that fails the build on any policy violation:

```sh
actime run --profile strict --fail-on-violation -- ./run-tests.sh
```
