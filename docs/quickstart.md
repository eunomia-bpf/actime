# Quickstart

This guide takes you from a fresh Linux machine to a real run report in about
five minutes. Actime runs an **unmodified** coding agent and accounts for what
it does across four planes: isolation, policy, evidence, history. The fastest
proof is `actime demo --policy observe`, which needs no agent, no root, and no
Docker.

> Contract reference: every field, flag, and behavior below is defined in
> [docs/DESIGN.md](./DESIGN.md). This document only shows you how to use it.

## 1. Install

One line, into `~/.local/bin` (Linux x86_64 or aarch64):

```sh
curl -fsSL https://raw.githubusercontent.com/eunomia-bpf/actime/main/scripts/install.sh | sh
```

If `~/.local/bin` is not on your `PATH`, the installer tells you and prints the
exact `export` line to add to your shell profile. Set `ACTIME_VERSION` to pin a
release tag, or `ACTIME_INSTALL_DIR` to install elsewhere.

You can also install from a source checkout:

```sh
git clone https://github.com/eunomia-bpf/actime
cd actime
cargo install --path crates/actime-cli
```

## 2. Check the machine

`actime doctor` is fail-soft by design: on a fresh machine it warns that the
three engines are not installed, and that is expected at this point. Real
output from a machine with Docker but no engines and no root:

```text
$ actime doctor
ok    os                           Linux
ok    kernel                       kernel 6.15.11-061511-generic (≥ 6.1)
ok    btf                          /sys/kernel/btf/vmlinux present
warn  cap_bpf                      neither root nor CAP_BPF; policy and evidence planes will disable
      → Run as root, or grant CAP_BPF (e.g. `sudo setcap cap_bpf,cap_perfmon+ep $(which actplane)`), ...
warn  actplane                     actplane not found on PATH, ~/.local/share/actime/bin, or ~/.cargo/bin
      → cargo install actplane
warn  agentsight                   agentsight not found on PATH, ~/.local/share/actime/bin, or ~/.cargo/bin
      → cargo install agentsight
warn  akeep                        akeep not found on PATH, ~/.local/share/actime/bin, or ~/.cargo/bin
      → cargo install akeep
ok    run_store                    writable at ~/.local/share/actime
ok    config                       profile=balanced policy=enforce sandbox.backend=auto evidence=on history=on
ok    sandbox: docker              docker: available (29.1.3)
ok    sandbox: podman              podman: available (4.9.3)
ok    sandbox: bwrap               bwrap: available (bubblewrap 0.9.0)

0 check(s) failed, 4 warning(s). Actime still runs: unavailable planes degrade rather than stopping a run.
```

Every warning carries its own fix line. `doctor` exits `0` when nothing failed
(warnings are fine) and `1` when a check failed; `actime doctor --json` emits
the same checks for tooling.

The three engines are separate projects. They are what give you kernel-level
policy enforcement and evidence; without them Actime still runs the agent,
isolates it, records the run, and writes a report.

```sh
cargo install actplane     # policy plane  (≥ 0.1.8; needs root or CAP_BPF)
cargo install agentsight   # evidence plane (≥ 0.2.60; needs root or CAP_BPF)
cargo install akeep        # history plane  (≥ 0.2.0; no privileges needed)
```

## 3. The 30-second demo

`actime demo` runs a bundled script that reads files, spawns subprocesses,
opens a network connection, touches credential-shaped paths, and attempts one
policy-violating action (`git push --force`), then prints the report.

The demo's default policy mode is `enforce`, which **fails closed** when the
policy plane cannot start (no `actplane`, or no root/`CAP_BPF`). For a first
look with no privileges at all, use `observe`:

```sh
actime demo --policy observe
```

Real output on a machine with Docker but without the engines installed:

```text
note: running the bundled stand-in agent in /tmp/actime-demo-4178070
actime  run 20260805-001728-bf7b   sandbox: docker   policy: observe   evidence: off
warning: policy plane disabled: actplane is not installed; run `cargo install actplane`
warning: evidence plane disabled: agentsight is not installed; run `cargo install agentsight`

actime-demo-agent: pretending to be a coding agent in /workspace

  read looking around the project
  exec running subprocesses (git, grep, python)
  write editing files
  connect opening a network connection
  read touching credential-shaped paths (policy: notify)
  exec attempting: git push --force  (policy: kill)

  note git push --force did not succeed (rc=128)
  done demo agent finished

Actime run report
------------------------------------------------------------------------
  Run id:     20260805-001728-bf7b
  Agent:      command
  Argv:       ./actime-demo-agent
  Duration:   5.5s
  Exit code:  0
  Profile:    balanced

Planes
------------------------------------------------------------------------
  isolation  Active
  policy     Disabled   actplane is not installed; run `cargo install actplane…
  evidence   Disabled   agentsight is not installed; run `cargo install agen…
  history    Disabled   akeep is not installed; run `cargo install akeep`

Summary
------------------------------------------------------------------------
  violations=0      blocked=0      killed=0
  processes=0       files_written=0     endpoints=0
  llm_calls=0       tokens_in=0        tokens_out=0
  peak_rss=0 B  cpu=0.00s  duration=5.5s

Policy violations (0)
------------------------------------------------------------------------
  (none)

Next steps
------------------------------------------------------------------------
  • actime report 20260805-001728-bf7b --markdown
  • actime report 20260805-001728-bf7b --json
  • Plane `policy` was disabled (...); run `actime doctor` for fixes
```

Two things to know about the demo:

- If the sandbox image is missing and Docker is the selected backend, the demo
  falls back to `--sandbox host` with a warning, so it works on a machine that
  has never pulled the image. Without Docker it uses bwrap or host directly.
- With the engines installed and sufficient privileges, plain `actime demo`
  runs the same script in `enforce` mode and the policy plane kills the
  `git push --force` attempt; the report's violation table shows it.

## 4. Run a real agent

Point Actime at any agent command. The agent runs **unmodified**:

```sh
actime run -- claude
```

Everything after `--` is the agent command. Actime resolves the `balanced`
profile by default, picks a sandbox backend automatically (Docker, then Podman,
then Bubblewrap, then host), and runs the four planes.

The first container run needs the sandbox image
(`ghcr.io/eunomia-bpf/actime-sandbox:latest`):

```sh
actime sandbox pull      # pull the published image
# or, from a source checkout:
actime sandbox build     # build sandbox/Dockerfile locally
```

If the image is missing and cannot be pulled, `actime run` stops with an error
naming the fixes (`actime sandbox build`, `actime sandbox pull`, `--image`);
only the demo falls back to host automatically. Use
`--sandbox bwrap` or `--sandbox host` to run without the image, or `--image`
to point at your own.

The policy and evidence planes need root or `CAP_BPF`. When you run
unprivileged, Actime invokes the engines through `sudo` (never prompting in
non-interactive sessions); without privileges those two planes degrade and the
reason is recorded in the manifest. In `enforce` mode a policy plane that
cannot start aborts the run instead of running unprotected.

When the agent exits you get the report on the terminal. With all four planes
active it looks like this (real output format; values depend on the run):

```text
Actime run report
------------------------------------------------------------------------
  Run id:     20260804-153012-a3f1
  Agent:      claude
  Argv:       claude -p add retry logic to the uploader and run the tests
  Duration:   4m12s
  Exit code:  0
  Profile:    balanced

Planes
------------------------------------------------------------------------
  isolation  Active
  policy     Active
  evidence   Active
  history    Active

Summary
------------------------------------------------------------------------
  violations=2      blocked=0      killed=1
  processes=214     files_written=18    endpoints=6
  llm_calls=37      tokens_in=411995   tokens_out=9102
  peak_rss=1.9 GiB  cpu=221.00s  duration=4m12s

Policy violations (2)
------------------------------------------------------------------------
  RULE                 EFFECT   TARGET                   REASON
  credential-access    notify   /home/dev/.aws/credenti… The agent read cr…
  destructive-vcs      kill     /usr/bin/git             Force-pushing, ha…

Next steps
------------------------------------------------------------------------
  • actime report 20260804-153012-a3f1 --markdown
  • actime report 20260804-153012-a3f1 --json
  • Review blocked/killed violations in ~/.local/share/actime/runs/20260804-153012-a3f1/violations.jsonl
```

Other agents work the same way; point `--` at the command:

```sh
actime run -- codex
actime run -- gemini
actime run -- opencode
actime run -- ./your-own-agent --flag arg
```

## 5. Read the record

Every run produces a report in three forms. Use whichever fits your workflow:

```sh
actime report                              # latest run, text, to stdout
actime report 20260804-153012-a3f1         # a specific run
actime report --markdown                   # adds a timeline section
actime report --json                       # manifest, summary, violations, timeline
```

The report is also written to disk as `report.md` in the run directory:

```
~/.local/share/actime/runs/<run-id>/
  manifest.json          # everything Actime recorded about the run
  actime.yaml            # the effective, fully resolved config for this run
  report.md              # rendered on exit
  policy.yaml            # the ActPlane project file the engine loaded (policy plane)
  policy.dsl             # the composed policy, human-readable   (policy plane)
  violations.jsonl       # one violation per line, appended live (policy plane)
  evidence.db            # AgentSight SQLite store               (evidence plane)
  *-engine.log, history.log   # engine stderr and plane logs, when a plane ran
```

Set `ACTIME_HOME` to put the run store somewhere else.

List past runs:

```sh
actime runs                      # recent runs, newest first (default 20)
actime runs --json --limit 50    # machine-readable, capped
actime status                    # runs still in progress
```

Replay a run's agent session history with Akeep:

```sh
actime keep log                   # committed versions (delegates to `akeep log`)
actime keep restore 20260804-153012-a3f1          # into a scratch directory
actime keep restore 20260804-153012-a3f1 --to ./restored
```

`keep restore` works for runs whose history plane committed; the commit id is
in the manifest as `akeep_commit`.

## 6. When a plane is degraded

Actime never fails a run just because a plane is missing. Instead the plane
degrades, the report says so, and `doctor` tells you how to fix it. The
[degradation matrix](./DESIGN.md#8-degradation-matrix) defines exactly what
happens for each missing piece.

Read the plane states at the top of the report:

| State | Meaning |
|-------|---------|
| `Active` | the plane ran normally |
| `Degraded` | the plane ran in a reduced form; the reason says how |
| `Disabled` | the plane did not run; the reason says why |

Common reasons and fixes:

- **policy disabled: `actplane is not installed; run 'cargo install actplane'`**.
  The agent ran with no kernel enforcement; nothing was blocked. Install the
  engine and re-run `actime doctor`. The policy plane also needs root or
  `CAP_BPF`: Actime attaches the eBPF programs from the host via `sudo` when
  needed, or you can grant the capability once with
  `sudo setcap cap_bpf,cap_perfmon+ep $(which actplane)`.
- **policy disabled: `actplane 0.1.5 is below 0.1.8`** -- the installed engine
  is too old. `cargo install actplane` and check `actime doctor` again.
- **evidence disabled: `agentsight is not installed`** -- Actime still records
  argv, exit code, and duration (the process-level fallback), but not the full
  process/file/network trace. Fix: `cargo install agentsight`.
- **isolation degraded: `host mode: no isolation`** -- the run used the `host`
  backend, so the agent ran as a normal child process with no isolation. This
  happens when you pass `--sandbox host`, or when `auto` found no Docker,
  Podman, or Bubblewrap. The `strict` profile refuses to run in this state.
- **history degraded: `akeep commit failed: ...`** -- the run finished but the
  session history was not committed; the reason comes from `akeep` and the run
  record is otherwise complete.

If a run surprises you, the first thing to attach to a bug report is the doctor
output and the run report:

```sh
actime doctor --json
actime report latest --markdown
```

## Next

- [configuration.md](./configuration.md) -- every field of `actime.yaml`, the
  resolution order, the three profiles, and every CLI override.
- [sandbox.md](./sandbox.md) -- the four backends, why the eBPF planes attach
  from the host, and how network isolation actually works.
- [faq.md](./faq.md) -- root, Docker, macOS, telemetry, and uninstall.
- [DESIGN.md](./DESIGN.md) -- the implementation contract.
