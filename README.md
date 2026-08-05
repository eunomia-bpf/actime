# Actime

[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](https://opensource.org/licenses/MIT)
[![CI](https://github.com/eunomia-bpf/actime/actions/workflows/ci.yml/badge.svg)](https://github.com/eunomia-bpf/actime/actions/workflows/ci.yml)

**The effect plane for AI coding agents: kernel-enforced policy, system evidence, and session history, attached to the agent you already run, wherever it already runs.**

```console
$ actime run -- claude
```

Your engineers are running Claude Code, Codex, Gemini CLI, and OpenCode on
company laptops, CI runners, and cloud sandboxes. Each one can spawn shells,
rewrite files, reach the network, and burn a machine's CPU. When something goes
wrong, the agent's own transcript is the only account of what happened —
written by the thing you are trying to audit.

Actime attaches three planes to the agent's process tree, unmodified:

| Plane | Component | Question it answers |
|-------|-----------|---------------------|
| Policy | [ActPlane](https://github.com/eunomia-bpf/ActPlane) (eBPF) | What is the agent allowed to do? |
| Evidence | [AgentSight](https://github.com/eunomia-bpf/agentsight) (eBPF) | What did the agent actually do? |
| History | [Akeep](https://github.com/eunomia-bpf/akeep) | What did the agent decide, and can we replay it? |

What you get back is not a log the agent wrote. It is what the kernel saw.

Actime does **not** manage sandboxes. Bring your own sandbox, or none at all.
Actime attaches to the process tree either way.

> **The sandbox contains. Actime accounts.**

That line describes roles, not position. The invariant that is always true is
that Actime observes and enforces **below the tool layer**, at the
syscall/effect boundary.

---

## Quick start

```console
# 1. Install (Linux x86_64/aarch64; or build from source, see below)
curl -fsSL https://raw.githubusercontent.com/eunomia-bpf/actime/main/scripts/install.sh | sh

# 2. Check what your machine supports
actime doctor

# 3. A first run — works unprivileged, with no engines installed
actime run --policy off --no-history -- /bin/echo hi

# 4. Read the record
actime report
```

That first run uses no eBPF and no privileges; it still produces a manifest and
a report, with the policy and evidence planes marked `Disabled` and the reason
recorded. For the full three planes, install the engines Actime drives and run
your real agent:

```console
cargo install actplane agentsight akeep

actime run -- claude
```

There is nothing else to configure. `actime run` with no `actime.yaml` uses the
`balanced` profile and turns on every plane your kernel and privileges allow.
Whatever is unavailable degrades the run rather than aborting it, with one
exception: `policy.mode: enforce` fails closed. See
[Degradation](#degradation).

## What you get back

Every run ends with a report on the terminal and on disk. Real output of the
quick-start run above, on a machine without the engines installed:

```text
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

And with all three planes active on a real agent run (illustrative; this one
needs the engines and root or `CAP_BPF`):

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

The full record lives in `~/.local/share/actime/runs/<run-id>/`: the manifest,
the effective config, the exact policy that was loaded, every violation as
JSONL, the AgentSight evidence database, and a rendered `report.md`. Override
the root with `ACTIME_HOME`. `actime report <id> --markdown` adds a timeline
section; `--json` emits the whole record for tooling.

## Where Actime sits: three deployment positions

Actime's position relative to a sandbox is a **deployment choice**, not an
architectural constraint. All three are supported:

| | Position | When to choose it | Tamper story |
|---|----------|-------------------|--------------|
| **A** | **Outside the sandbox** — Actime on the host, attached to an existing container's process tree (`actime attach --container`, `--pod`) | You own the host: workstations, CI runners, self-managed Kubernetes nodes. Needs root or `CAP_BPF` on the host | Strongest: root inside the container cannot disable the recorder or edit the record |
| **B** | **Inside the sandbox** — Actime in the same container/VM as the agent (`actime run -- claude`, inside) | You do not own the host: E2B, Daytona, AWS AgentCore, managed Kubernetes, someone else's microVM. Also how a sandbox vendor ships Actime in its image | Weaker: root inside the container can interfere. The container needs `CAP_BPF` (often plus `CAP_PERFMON`/`CAP_SYS_ADMIN`) granted to it |
| **C** | **No sandbox at all** — a plain process on a machine | The common workstation case | Same as host-side attach to a plain process tree |

Position B is the only option on managed sandbox platforms, and its weaker
tamper story is real: an agent with root in its own container can interfere
with a recorder running in the same container. We say so plainly wherever it
matters, and `actime doctor` detects the in-container case and tells you.
Never read a host-side guarantee into a position-B deployment.

The setup for each position — including the `docker run --cap-add` incantation
for B and host-side attach for A — is in
[docs/deployment.md](docs/deployment.md).

## Why this layer

Most agent guardrails sit at the tool layer: the framework asks permission
before it calls a tool. That works right up to the moment the agent writes a
shell script, and a coding agent's whole job is writing shell scripts.

Actime enforces below the tool layer, at the syscall boundary, where the effect
actually happens. This is independent of where Actime is deployed:

| The agent does | Tool-layer guardrail | Actime |
|---|---|---|
| calls a blocked tool | blocked | blocked |
| `bash -c` the same thing | passes | blocked |
| a Python subprocess does it | passes | blocked |
| a subagent it spawned does it | usually passes | blocked, lineage is inherited |
| reads a secret, then posts it from another process | invisible | blocked, the label follows the data |

That last row is the one worth dwelling on. Actime does not just match calls,
it tracks **where data came from**. A value read out of `.env`, copied into a
temp file, piped through `jq`, and sent by a subprocess is still labeled when
it reaches the socket:

```
rule no-secret-egress:
  kill connect endpoint "*" if AGENT and SECRET
  because "This process holds data derived from a secret file and tried to open a network connection..."
```

No syscall allowlist can express that, because the offending syscall is an
ordinary `connect`. What makes it a violation is its history.

## Architecture

```
                 your agent, unmodified
                 claude · codex · gemini · opencode · any command
                              │
                     existing process tree
                     (a container you made, a pod, or a plain process)
                              │
        ┌─────────────────────▼─────────────────────┐
        │  policy plane      ActPlane   (eBPF)      │   ← constrains
        │  evidence plane    AgentSight (eBPF)      │   ← accounts
        │  history plane     Akeep                  │   ← preserves
        └─────────────────────┬─────────────────────┘
                              │
                       run record + report
```

Actime never creates, starts, stops, or removes a container or pod.
`actime attach --container` / `--pod` only resolve targets that already exist —
that is how Actime composes with other people's sandboxes.

Actime does not reimplement the planes. It composes three existing projects
into one runtime, one config file, and one report:

| Plane | Project | What it contributes |
|---|---|---|
| Policy | [ActPlane](https://github.com/eunomia-bpf/ActPlane) | labeled information-flow enforcement in the kernel |
| Evidence | [AgentSight](https://github.com/eunomia-bpf/agentsight) | process, file, network, TLS, and resource evidence |
| History | [Akeep](https://github.com/eunomia-bpf/akeep) | versioned, restorable agent session history |

The eBPF instrumentation underneath comes from
[bpftime](https://github.com/eunomia-bpf/bpftime). Each project remains useful
on its own; Actime is how you install, run, and operate them together.

Actime needs `actplane` ≥ 0.1.8, `agentsight` ≥ 0.2.60, and `akeep` ≥ 0.2.0.
`actime doctor` checks the versions it finds and tells you what to upgrade.

## Policy

Policies are ActPlane rules over real OS effects. Actime ships three packs and
you can add your own:

```yaml
# actime.yaml
policy:
  mode: enforce                 # off | observe | enforce
  packs:
    - coding-agent-baseline     # destructive VCS, mass deletion
    - no-vcs-write              # the agent edits, the human publishes
    - no-secret-egress          # data labeled from secrets may not reach the network
  files:
    - ./team-policy.dsl
```

```console
actime policy list              # packs and what each one forbids
actime policy show no-vcs-write
actime policy check             # compile and validate without loading anything
actime policy explain           # what your kernel can enforce before the fact
```

`check` and `explain` call the installed `actplane` binary; they compile the
policy but never load it into the kernel, so they need no privileges.

Start with `--policy observe`. Nothing is blocked, everything is recorded, and
after a week you will know which rules you actually want. Then move them to
`enforce` one at a time.

When a rule fires, the agent is told why, in words, through ActPlane's feedback
interface, so it corrects course instead of retrying the same blocked action.

See [docs/policies.md](docs/policies.md).

## Degradation

Actime is built to be useful on a laptop with no root and no container runtime,
and stricter as the environment allows. Nothing below is an error:

| Missing | What happens |
|---|---|
| root / `CAP_BPF` | policy and evidence planes disabled; history still runs; `doctor` explains |
| running inside a container without `CAP_BPF` | same; doctor warns that this is deployment B without host-side tamper-resistance |
| `actplane` | policy plane disabled in `observe` mode; a hard failure in `enforce` (fail closed) |
| `agentsight` | evidence plane disabled; process-level fallback still records argv, exit, duration |
| `akeep` | history plane disabled |
| kernel < 5.10 | policy plane disabled, with your kernel version in the reason |

Every run produces a manifest and a report, even when only the fallback ran.
`actime doctor` tells you exactly which planes your machine supports and how to
turn on the rest.

## Requirements

- Linux. The policy and evidence planes need kernel 5.10+ with BTF
  (`/sys/kernel/btf/vmlinux`); 6.1+ is recommended for the full runtime. The
  history plane has no kernel requirement.
- Root, or `CAP_BPF`/`CAP_PERFMON` on the engine binaries, for the policy and
  evidence planes only. Everything else runs unprivileged.
- A container runtime (Docker/Podman) or `kubectl` only if you want
  `actime attach --container` / `--pod` to resolve those targets. Actime itself
  never starts a container.

macOS is not supported: the prebuilt binaries are Linux-only, and the eBPF
planes need a Linux host kernel.

## Documentation

- [Quick start](docs/quickstart.md)
- [Deployment positions](docs/deployment.md): outside the sandbox, inside it, or no sandbox
- [Configuration reference](docs/configuration.md): every field of `actime.yaml`
- [Policies](docs/policies.md): writing and testing your own rules
- [Evidence and reports](docs/evidence.md): the run record, JSON, export
- [Design](docs/DESIGN.md): the architecture contract
- [FAQ](docs/faq.md)

## Who this is for

Platform and security teams that already have coding agents inside the
building, on machines the company owns — or in sandboxes someone else runs —
and need to answer: which agents ran, what did they touch, what did they send
where, what did they cost, and what stopped them. Actime is not a hosted
sandbox service and does not want your code. It is local-first and sends
nothing anywhere.

## Related work

Actime deliberately does not compete with hosted agent-execution platforms
(AWS AgentCore, E2B, Daytona): bring your own environment and Actime layers
onto it — from the host where you own one, or inside the sandbox where you do
not. It also does not sit at the identity or tool-authorization layer. It owns
the layer between: what the agent's actions actually *do* to a machine, and
the record of it.

## Contributing

Issues and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md)
and [SECURITY.md](SECURITY.md).

## License

MIT. See [LICENSE](LICENSE).
