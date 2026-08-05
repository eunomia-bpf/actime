# FAQ

Short, direct answers. Where a claim is a contract, the
[design document](./DESIGN.md) is authoritative.

## Does Actime need root?

No. The Actime binary, the sandbox, and the history plane all run unprivileged.
`actime run -- claude` works as a normal user.

Only the **policy** (ActPlane) and **evidence** (AgentSight) planes need root or
`CAP_BPF`, because they load eBPF programs into the kernel. When you run
unprivileged, Actime launches the engines through `sudo` (interactively it may
ask for your password once; in non-interactive sessions it uses `sudo -n` and
fails fast instead of hanging). If the privileges are not there, those two
planes degrade to `Disabled` with the reason in the report, while isolation,
history, and the process-level evidence fallback keep working. One exception:
in `policy.mode: enforce` a policy plane that cannot start aborts the run
(fail closed). Run `actime doctor` to see exactly which planes are available on
your machine; it prints the `setcap` command that grants the engines
`CAP_BPF`/`CAP_PERFMON` if you prefer not to use sudo.

## Does it work without Docker?

Yes. When `sandbox.backend: auto` (the default) Actime probes in order:
**Docker**, then **Podman**, then **Bubblewrap**, then **host**. If you have no
container runtime it uses the Bubblewrap namespace sandbox, and if that is
missing too, the `host` backend. Two honest caveats:

- On `bwrap`, no host pid exists before the agent starts, so in 0.1.0 the
  policy and evidence planes are disabled there. You still get isolation,
  history, and the process-level record.
- The `docker`/`podman` backends need the sandbox image on first use
  (`actime sandbox pull`, or `actime sandbox build` from a checkout).

`actime demo --policy observe` runs with no agent, no root, and no Docker.
Plain `actime demo` defaults to `enforce` and fails closed if the policy plane
cannot start.

The `strict` profile is the one exception to graceful fallback: it requires a
real sandbox and fails rather than run on `host`.

## Does Actime slow the agent down?

Not in the path that matters. Actime does **not** proxy LLM traffic or sit
between the agent and its model (that is an explicit non-goal). Agent network
calls go direct. The costs are:

- **Sandbox startup** -- pulling an image the first time, then sub-second
  container/bwrap bring-up for subsequent runs. Cached images are fast.
- **eBPF probes** -- the policy and evidence planes attach per-syscall probes in
  the kernel's fast path. Evidence is aggregated and written to the run
  directory, not copied through Actime on the hot path.

For a typical coding session, sandbox startup is amortized over minutes of agent
work, and the per-syscall overhead is not noticeable relative to model latency.

## Does Actime send anything to the network?

No. Actime is local-first and ships **no telemetry**. All run data is written
under `~/.local/share/actime/runs/<run-id>/` on your machine (override with
`ACTIME_HOME`).

The `evidence.export` field is reserved for optional sinks such as `otlp`; in
0.1.0 it is recorded in the effective config but not yet wired to the evidence
engine, so nothing leaves the box. The only network traffic on the machine is
the agent's own, which you control via the sandbox network mode and the policy
plane.

## How does Actime relate to ActPlane, AgentSight, and Akeep?

Actime is the **orchestrator**; it does not write its own eBPF code. Each plane
is a separate project you install independently:

| Plane | Project | Role | Installed via |
|-------|---------|------|---------------|
| Policy | [ActPlane](https://github.com/eunomia-bpf/ActPlane) | Compiles policy packs and enforces `notify` / `block` / `kill` effects in the kernel | `cargo install actplane` (≥ 0.1.8) |
| Evidence | [AgentSight](https://github.com/eunomia-bpf/agentsight) | Records process, file, network, ssl, and resource events | `cargo install agentsight` (≥ 0.2.60) |
| History | [Akeep](https://github.com/eunomia-bpf/akeep) | Records decisions and makes runs replayable | `cargo install akeep` (≥ 0.2.0) |

Actime resolves each engine on `PATH`, then in `~/.local/share/actime/bin`
(`$ACTIME_HOME/bin`), then in `~/.cargo/bin`. It calls the engine with the
right arguments and reads back what it wrote. You can use any of those tools on
their own; Actime's value is wiring them together around an unmodified agent
and always producing a manifest and a report.

## Does it work with agents other than Claude Code?

Yes. Actime runs **any** command. Everything after `--` is the agent command,
untouched:

```sh
actime run -- claude
actime run -- codex
actime run -- gemini
actime run -- opencode
actime run -- ./your-own-agent --flag arg
```

The default sandbox image preinstalls Claude Code, Codex, and Gemini CLI as a
convenience, but they are not required. `actime demo` uses a bundled script, so
it needs no agent at all.

## What about macOS?

Not supported. The prebuilt binaries are Linux-only (the installer refuses
other platforms), and the **policy and evidence planes are Linux-only** because
they are eBPF programs that need a Linux host kernel to attach to. Use a Linux
machine or VM.

## Where is my data stored?

Under two directories:

- `~/.local/share/actime/runs/<run-id>/` -- one directory per run, holding the
  manifest, effective config, the policy that was loaded, violations, evidence,
  engine logs, and the rendered report. Override the root with `ACTIME_HOME`.
- `~/.config/actime/actime.yaml` -- your user-level config (one of the
  resolution layers).

List runs with `actime runs`, inspect one with `actime report <id>`, and see
what is still running with `actime status`.

## Does Actime modify my codebase?

Only the agent does. Actime itself writes to the run directory, never to your
project. The one thing to be aware of: when the history plane is enabled with
`history.commit_on_exit: true` (the default), Akeep commits the agent's session
history when the run exits. If that is not what you want in a given repo,
disable it:

```yaml
history:
  commit_on_exit: false
```

## Can I see what a policy will block before running?

Yes. The policy subcommand inspects packs without running anything:

```sh
actime policy list                 # packs shipped with actime
actime policy show coding-agent-baseline
actime policy check                # compile the configured policy; loads nothing
actime policy explain              # what this kernel can enforce before the fact
```

`check` and `explain` call the installed `actplane` binary and need no
privileges, so `check` belongs in CI.

## Is it safe to run in CI?

Yes. A common pattern is to gate a job on policy:

```sh
actime run --profile strict --fail-on-violation -- ./run-agent-task.sh
```

`--fail-on-violation` forces exit code `3` on any `kill` or `block` violation,
so a CI step fails cleanly when the agent steps out of bounds. Otherwise the
exit code of `actime run` is the agent's own exit code. Actime never prompts
for a sudo password when stdin is not a terminal (and never when
`ACTIME_NONINTERACTIVE` or `NO_COLOR` is set), so a CI job cannot hang on a
password prompt; without passwordless sudo the eBPF planes simply degrade.

## How do I uninstall?

Remove the binary, the run store, and the config:

```sh
rm -f ~/.local/bin/actime
rm -rf ~/.local/share/actime
rm -f ~/.config/actime/actime.yaml
```

If you installed the optional engines, remove them too:

```sh
cargo uninstall actplane agentsight akeep
```

Optionally remove the sandbox image:

```sh
docker rmi ghcr.io/eunomia-bpf/actime-sandbox:latest
```

That removes everything Actime put on your machine.
