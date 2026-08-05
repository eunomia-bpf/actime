# Quickstart

This guide takes you from a fresh Linux machine to a real run report in about
five minutes. Actime runs an **unmodified** coding agent and accounts for what
it does across three planes: policy, evidence, history. It never creates
containers or sandboxes — it attaches to the process tree you already have.
The fastest proof needs no agent, no root, and no Docker:
`actime run --policy off --no-history -- /bin/echo hi`.

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

`actime doctor` is fail-soft by design: it reports which planes your machine
supports, and every warning carries its own fix line. Real output (on a
machine with the engines installed but older than the minimum, and no root):

```text
$ actime doctor
ok    deployment                   running on a host (deployment A/C). Host-side attach is available ...
ok    os                           Linux
ok    kernel                       kernel 6.15.11-061511-generic (≥ 6.1)
ok    btf                          /sys/kernel/btf/vmlinux present
warn  cap_bpf                      neither root nor CAP_BPF; policy and evidence planes will disable
      → Run as root, or grant CAP_BPF (e.g. `sudo setcap cap_bpf,cap_perfmon+ep $(which actplane)`), ...
warn  actplane                     actplane 0.1.5 at ~/.local/bin/actplane is below minimum 0.1.8
      → cargo install actplane and ensure version ≥ 0.1.8
warn  agentsight                   agentsight 0.2.45 at ~/.local/bin/agentsight is below minimum 0.2.60
      → cargo install agentsight and ensure version ≥ 0.2.60
ok    akeep                        akeep 0.2.0 at ~/.local/bin/akeep (≥ 0.2.0)
ok    run_store                    writable at ~/.local/share/actime
ok    config                       profile=balanced policy=enforce evidence=on history=on

0 check(s) failed, 3 warning(s). Actime still runs: unavailable planes degrade rather than stopping a run.
```

`doctor` exits `0` when nothing failed (warnings are fine) and `1` when a
check failed; `actime doctor --json` emits the same checks for tooling. The
first check, `deployment`, names your
[deployment position](./deployment.md) — on a plain host it tells you
host-side attach is available; inside a container it warns that this is
deployment B and reports which capabilities are missing.

The three engines are separate projects. They are what give you kernel-level
policy enforcement and evidence; without them Actime still runs the agent,
records the run, and writes a report.

```sh
cargo install actplane     # policy plane  (≥ 0.1.8; needs root or CAP_BPF)
cargo install agentsight   # evidence plane (≥ 0.2.60; needs root or CAP_BPF)
cargo install akeep        # history plane  (≥ 0.2.0; no privileges needed)
```

`doctor` tells you which *planes* your machine supports. The companion
question — which *policy rules* this host can actually enforce — is answered
by `actime policy check`, which compiles the configured policy, loads nothing,
and needs no privileges:

```text
$ actime policy check
ok policy compiled from coding-agent-baseline · 2/2 rules enforceable on this host

RULE                     EFFECT   ENFORCEABLE  REASON
destructive-vcs          kill     yes
mass-deletion            kill     yes
```

Enforceability is a host property, not a pack property. With released ActPlane
0.1.8, exec-based rules install and fire; the file-sink and label-propagation
rules in the shipped `information-flow` pack do not, and `check` says so one
rule at a time with the missing engine feature named. Run it whenever you
change `policy.packs`, and in CI before any `enforce` gate — a rule
`enforce` cannot install aborts the run (fail closed), so this table is how
you find out before the agent does.

## 3. A first run — no privileges, no engines

The shortest path from nothing to a result:

```sh
actime run --policy off --no-history -- /bin/echo hi
```

Everything after `--` is the agent command. Real output:

```text
actime  run 20260805-124610-f7db   target: command   policy: off   evidence: on
warning: policy plane disabled: policy.mode is off

hi

Actime run report
------------------------------------------------------------------------
  Run id:     20260805-124610-f7db
  Agent:      command
  Argv:       /bin/echo hi
  Duration:   1.7s
  Exit code:  0
  Profile:    balanced

Target
------------------------------------------------------------------------
  kind:       command
  spec:       /bin/echo hi
  host_pid:   3577921
  note:       launched as a host child process

Planes
------------------------------------------------------------------------
  policy     Disabled   policy.mode is off
  evidence   Degraded   agentsight produced no process/file/network observatio…
  history    Disabled   history.enabled is false

Summary
------------------------------------------------------------------------
  violations=0      blocked=0      killed=0
  processes=0       files_written=0     endpoints=0
  llm_calls=0       tokens_in=0        tokens_out=0
  peak_rss=0 B  cpu=0.00s  duration=1.7s
```

The point of this run is not the `echo` — it is that every `actime run`
produces a manifest and a report, even when every plane is off or degraded,
and each plane's state comes with the reason. Nothing in the exit path is
conditional on a plane having worked.

## 4. Run a real agent

Point Actime at any agent command. The agent runs **unmodified**, as a host
child in your real cwd and environment:

```sh
actime run -- claude
```

With no `actime.yaml`, Actime resolves the built-in `balanced` profile: policy
`enforce` with the `coding-agent-baseline` pack, evidence on, history on. That
pack is deliberately limited to exec-based rules (`destructive-vcs`,
`mass-deletion`) that released ActPlane can actually install — run
`actime policy check` to see the per-rule verdict for any pack you configure.
The policy and evidence planes need root or `CAP_BPF`. When you run unprivileged,
Actime invokes the engines through `sudo` (never prompting in non-interactive
sessions); without privileges those two planes degrade and the reason is
recorded in the manifest. In `enforce` mode a policy plane that cannot start —
or a requested rule that this host's engine cannot enforce — aborts the run
instead of running unprotected.

With the engines installed and sufficient privileges, the report looks like
this (illustrative; values depend on the run):

```text
Actime run report
------------------------------------------------------------------------
  Run id:     20260804-153012-a3f1
  Agent:      claude
  Argv:       claude -p add retry logic to the uploader and run the tests
  Duration:   4m12s
  Exit code:  0
  Profile:    balanced

Target
------------------------------------------------------------------------
  kind:       command
  spec:       claude -p add retry logic to the uploader and run the tests
  host_pid:   4124801
  note:       launched as a host child process

Planes
------------------------------------------------------------------------
  policy     Active
  evidence   Active
  history    Active

Summary
------------------------------------------------------------------------
  violations=2      blocked=0      killed=2
  processes=214     files_written=18    endpoints=6
  llm_calls=37      tokens_in=411995   tokens_out=9102
  peak_rss=1.9 GiB  cpu=221.00s  duration=4m12s

Policy violations (2)
------------------------------------------------------------------------
  RULE                 EFFECT   TARGET                   REASON
  destructive-vcs      kill     /usr/bin/git             Force-pushing, ha…
  mass-deletion        kill     /usr/bin/rm              Recursive deletio…

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

## 5. Attach to something already running

`actime run` launches a new process. `actime attach` binds the planes to one
that already exists — including a container or pod that someone else created:

```sh
actime attach --comm claude              # newest process with this comm name
actime attach --pid 4213                 # a specific host pid
sudo actime attach --container agent-box # an existing Docker/Podman container
sudo actime attach --pod default/agent-0 # an existing pod on this node
```

Actime never creates, starts, stops, or removes containers — `--container` and
`--pod` only resolve targets that already exist. Attaching from the host to a
container's process tree is
[deployment position A](./deployment.md), the strongest tamper story: root
inside the container cannot disable the recorder or edit the record. Attach
binds future events only (nothing is reconstructed), holds until the target
exits or you press Ctrl-C, and does not commit history.

## 6. Read the record

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
  violations.jsonl       # harvested policy violations           (policy plane)
  evidence.db            # AgentSight SQLite store               (evidence plane)
  *-engine.log           # engine stderr, when a plane was attempted
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

## 7. When a plane is degraded

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
  `CAP_BPF`: Actime launches the engine via `sudo` when needed, or you can
  grant the capability once with
  `sudo setcap cap_bpf,cap_perfmon+ep $(which actplane)`.
- **policy disabled: `actplane 0.1.5 is below 0.1.8`** — the installed engine
  is too old. `cargo install actplane` and check `actime doctor` again.
- **policy disabled: `N rule(s) not enforceable on this host's ActPlane
  engine`** — the run was in `observe` mode and the configured packs contain
  rules the installed engine cannot install (with released ActPlane 0.1.8:
  every rule in the `information-flow` pack). The report lists them under
  **Unenforceable rules** with the missing features; `actime policy check`
  shows the same table before you run. In `enforce` mode this aborts the run
  (exit 1) instead.
- **evidence disabled: `agentsight is not installed`** — Actime still records
  argv, exit code, and duration (the process-level fallback), but not the full
  process/file/network trace. Fix: `cargo install agentsight`.
- **history disabled: `attach does not commit history`** — expected for
  `actime attach`; there is no run exit to commit on.
- **history degraded: `akeep commit failed: ...`** — the run finished but the
  session history was not committed; the reason comes from `akeep` and the run
  record is otherwise complete.

If a run surprises you, the first thing to attach to a bug report is the doctor
output and the run report:

```sh
actime doctor --json
actime report latest --markdown
```

## Next

- [deployment.md](./deployment.md) — outside the sandbox, inside it, or no
  sandbox: the three positions and their real tamper stories.
- [configuration.md](./configuration.md) — every field of `actime.yaml`, the
  resolution order, the three profiles, and every CLI override.
- [faq.md](./faq.md) — root, containers, macOS, telemetry, and uninstall.
- [DESIGN.md](./DESIGN.md) — the implementation contract.
