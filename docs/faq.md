# FAQ

Short, direct answers. Where a claim is a contract, the
[design document](./DESIGN.md) is authoritative.

## Does Actime need root?

No. The Actime binary and the history plane run unprivileged.
`actime run -- claude` works as a normal user.

Only the **policy** (ActPlane) and **evidence** (AgentSight) planes need root
or `CAP_BPF`, because they load eBPF programs into the kernel. When you run
unprivileged, Actime launches the engines through `sudo` (interactively it may
ask for your password once; in non-interactive sessions it uses `sudo -n` and
fails fast instead of hanging). If the privileges are not there, those two
planes degrade to `Disabled` with the reason in the report, while history and
the process-level evidence fallback keep working. One exception: in
`policy.mode: enforce` a policy plane that cannot start aborts the run
(fail closed). Run `actime doctor` to see exactly which planes are available on
your machine; it prints the `setcap` command that grants the engines
`CAP_BPF`/`CAP_PERFMON` if you prefer not to use sudo.

## Where does Actime sit relative to my sandbox?

Wherever you put it — that is a deployment choice, not an architectural
constraint. Actime never creates, starts, stops, or removes containers. Three
positions, all supported:

- **Outside the sandbox** (position A): Actime on the host, attached to an
  existing container or pod with `actime attach --container REF` /
  `--pod NS/POD`. Strongest tamper story: root inside the container cannot
  disable the recorder or edit the record. Needs host root or `CAP_BPF`.
- **Inside the sandbox** (position B): Actime in the same container/VM as the
  agent, running `actime run -- claude`. The only option on platforms where you
  do not own the host (E2B, Daytona, AWS AgentCore, managed Kubernetes).
  Weaker tamper story: root inside the container can interfere, and we say so
  plainly. Needs `CAP_BPF` granted to the container.
- **No sandbox at all** (position C): a plain process on a machine, the common
  workstation case.

The full setup for each — including the exact `docker run --cap-add`
incantation for position B — is in [deployment.md](./deployment.md). What is
always true, in every position, is that Actime enforces and observes below the
tool layer, at the syscall boundary.

## Does Actime work with E2B, Daytona, or AWS AgentCore?

Yes — that is deployment position B. You cannot attach from a host you do not
own, so Actime runs inside the sandbox, alongside the agent. Install it in the
sandbox image (or have the vendor ship it), grant the container `CAP_BPF`
(often plus `CAP_PERFMON`; `CAP_SYS_ADMIN` on kernels older than 5.8), and run
the agent under `actime run`. See [deployment.md](./deployment.md) for the
exact setup, and read the tamper-story section there before you rely on the
record for incident response.

## Does it work without Docker?

Yes. Actime does not use Docker to run anything — `actime run` launches the
agent as a plain host child. Docker, Podman, or `kubectl` are only needed to
*resolve* attach targets: `actime attach --container` uses
`docker inspect` / `podman inspect`, and `--pod` uses `kubectl`. If you never
attach to containers, you never need a container runtime.

## Does Actime slow the agent down?

Not in the path that matters. Actime does **not** proxy LLM traffic or sit
between the agent and its model (that is an explicit non-goal). Agent network
calls go direct. The only cost is the eBPF probes: the policy and evidence
planes attach per-syscall programs in the kernel's fast path, and evidence is
aggregated and written to the run directory, not copied through Actime on the
hot path. For a typical coding session the per-syscall overhead is not
noticeable relative to model latency.

## Does Actime send anything to the network?

No. Actime is local-first and ships **no telemetry**. All run data is written
under `~/.local/share/actime/runs/<run-id>/` on your machine (override with
`ACTIME_HOME`).

The `evidence.export` field is reserved for optional sinks such as `otlp`; in
0.1.0 it is recorded in the effective config but not yet wired to the evidence
engine, so nothing leaves the box. The only network traffic on the machine is
the agent's own, which you control with the policy plane's `connect` rules.

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

The shipped policy packs label `claude`, `codex`, `gemini`, `opencode`,
`openclaw`, `aider`, and `cursor-agent` processes as agent lineage; add your
own `source AGENT = exec "**/your-agent"` line in a policy file for anything
else (see [policies.md](./policies.md)).

## What about macOS?

Not supported. The prebuilt binaries are Linux-only (the installer refuses
other platforms), and the **policy and evidence planes are Linux-only** because
they are eBPF programs that need a Linux kernel to attach to. Use a Linux
machine, VM, or container.

## Where is my data stored?

Under two directories:

- `~/.local/share/actime/runs/<run-id>/` — one directory per run, holding the
  manifest, effective config, the policy that was loaded, violations, evidence,
  engine logs, and the rendered report. Override the root with `ACTIME_HOME`.
- `~/.config/actime/actime.yaml` — your user-level config (one of the
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

## Can Actime stop an agent from leaking my secrets?

Not yet, and we would rather say that plainly than let you assume it. The
`information-flow` pack expresses exactly this — data labeled from secret
files (`.env`, `~/.ssh/id_*`, `~/.aws/credentials`, …) may not reach the
network, with the label following the data across copies, pipes, and
subprocesses — but enforcing it needs engine features (file-source label
propagation, path matchers) that released ActPlane 0.1.8 does not provide on
the attach path. The rule compiles; it does not install. `actime policy check`
reports it as not enforceable, and `--policy enforce` refuses to start a run
that requests it. What the policy plane enforces today is exec-level rules
(`git --force`, `rm -rf`, `git push`), which hold below the tool layer. See
[policies.md](./policies.md) for the full picture.

## Can I see what a policy will block before running?

Yes. The policy subcommand inspects packs without running anything:

```sh
actime policy list                 # packs shipped with actime
actime policy show coding-agent-baseline
actime policy check                # per rule: enforceable on this host, or not, and why
actime policy explain              # how each clause lowers to kernel matchers
```

`check` is the useful one before a run: it prints a table of every configured
rule with an enforceable yes/no and the missing engine feature when the answer
is no. It loads nothing into the kernel and needs no privileges, so it belongs
in CI — run it before any `enforce` gate, because `enforce` fails closed
(aborts the run) when a requested rule is not enforceable.

## Is it safe to run in CI?

Yes. A common pattern is to gate a job on policy:

```sh
actime policy check    # first: confirm every configured rule is enforceable here
actime run --profile strict --fail-on-violation -- ./run-agent-task.sh
```

Run `actime policy check` first because `enforce` fails closed — a requested
rule the runner's engine cannot install aborts the run before the agent
starts. `check` needs no privileges and prints the per-rule table, so a CI
lint step can catch a policy that would never load.

`--fail-on-violation` forces exit code `3` on any `kill` or `block` violation,
so a CI step fails cleanly when the agent steps out of bounds. Otherwise the
exit code of `actime run` is the agent's own exit code. Actime never prompts
for a sudo password when stdin is not a terminal (and never when
`ACTIME_NONINTERACTIVE` or `NO_COLOR` is set), so a CI job cannot hang on a
password prompt; without passwordless sudo the eBPF planes simply degrade. On a
CI runner you own, position A applies: the runner is the host, and the record
is outside anything the agent can reach.

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

That removes everything Actime put on your machine.
