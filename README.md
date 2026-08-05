# Actime

[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](https://opensource.org/licenses/MIT)
[![CI](https://github.com/eunomia-bpf/actime/actions/workflows/ci.yml/badge.svg)](https://github.com/eunomia-bpf/actime/actions/workflows/ci.yml)

**One runtime for AI coding agents: sandbox isolation, kernel-enforced policy, system evidence, and session history, in one command, with no changes to the agent.**

```console
$ actime run -- claude
```

Your engineers are running Claude Code, Codex, Gemini CLI, and OpenCode on
laptops, CI runners, and shared boxes. Each one can spawn shells, rewrite files,
reach the network, and burn a machine's CPU. When something goes wrong, the
agent's own transcript is the only account of what happened, written by the
thing you are trying to audit.

Actime runs the agent you already use, unmodified, inside a sandbox, and
watches and constrains it **from outside** using eBPF. What you get back is not
a log the agent wrote. It is what the kernel saw.

> **The sandbox contains. Actime accounts.**

---

## Quick start

```console
# 1. Install (Linux x86_64/aarch64; or build from source, see below)
curl -fsSL https://raw.githubusercontent.com/eunomia-bpf/actime/main/scripts/install.sh | sh

# 2. See the pipeline end to end: no agent, no root, no Docker needed
actime demo --policy observe

# 3. Check what your machine supports
actime doctor
```

`actime demo` runs a bundled stand-in agent and prints the same report a real
run produces. Its default policy mode is `enforce`, which fails closed when the
policy engine is not installed or not privileged; `--policy observe` records
without blocking and needs no privileges. If the sandbox image is not available
locally, the demo falls back to the host backend and tells you so.

For the policy, evidence, and history planes, install the three engines Actime
drives, then run your real agent:

```console
cargo install actplane agentsight akeep

# first container run needs the sandbox image
actime sandbox pull      # or, from a source checkout: actime sandbox build

actime run -- claude
```

There is nothing else to configure. `actime run` with no `actime.yaml` uses the
`balanced` profile, picks the best sandbox your machine offers, and turns on
every plane your kernel and privileges allow. Whatever is unavailable is
reported in the run manifest and degrades the run rather than aborting it, with
one exception: `policy.mode: enforce` fails closed. See
[Degradation](#degradation).

## What you get back

Every run ends with a report on the terminal and on disk. This is the real
output format (a run with all four planes active):

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

The full record lives in `~/.local/share/actime/runs/<run-id>/`: the manifest,
the effective config, the exact policy that was loaded, every violation as
JSONL, the AgentSight evidence database, and a rendered `report.md`. Override
the root with `ACTIME_HOME`. `actime report <id> --markdown` adds a timeline
section; `--json` emits the whole record for tooling.

## Why this layer

Most agent guardrails sit at the tool layer: the framework asks permission
before it calls a tool. That works right up to the moment the agent writes a
shell script, and a coding agent's whole job is writing shell scripts.

Actime enforces below the tool layer, at the syscall boundary, where the effect
actually happens:

| The agent does | Tool-layer guardrail | Actime |
|---|---|---|
| calls a blocked tool | blocked | blocked |
| `bash -c` the same thing | passes | blocked |
| a Python subprocess does it | passes | blocked |
| a subagent it spawned does it | usually passes | blocked, lineage is inherited |
| reads a secret, then posts it from another process | invisible | blocked, the label follows the data |

That last row is the one worth dwelling on. Actime does not just match calls,
it tracks **where data came from**. A value read out of `.env`, copied into a
temp file, piped through `jq`, and sent by a subprocess is still labeled when it
reaches the socket:

```
rule no-secret-egress:
  kill connect endpoint "*" if AGENT and SECRET
  because "This process holds data derived from a secret file..."
```

No syscall allowlist can express that, because the offending syscall is an
ordinary `connect`. What makes it a violation is its history.

## Architecture

```
                 your agent, unmodified
                 claude · codex · gemini · opencode · any command
                              │
        ┌─────────────────────▼─────────────────────┐
        │  isolation plane      sandbox              │   ← contains
        │  docker · podman · bubblewrap · host       │
        └─────────────────────┬─────────────────────┘
                              │  process tree
   ═══════════════════════════╪═══════════════════════  sandbox boundary
                              │
        ┌─────────────────────▼─────────────────────┐
        │  policy plane      ActPlane   (eBPF)      │   ← constrains
        │  evidence plane    AgentSight (eBPF)      │   ← accounts
        │  history plane     Akeep                  │   ← preserves
        └─────────────────────┬─────────────────────┘
                              │
                       run record + report
```

The policy and evidence planes deliberately run on the **host**, outside the
sandbox, attached to the container's process tree. This is the point: an agent
that gets root inside its own sandbox still cannot turn off the recorder or
edit the record.

Actime does not reimplement any of this. It composes four existing projects
into one runtime, one config file, and one report:

| Plane | Project | What it contributes |
|---|---|---|
| Isolation | Docker / Podman / [Bubblewrap](https://github.com/containers/bubblewrap) | the boundary |
| Policy | [ActPlane](https://github.com/eunomia-bpf/ActPlane) | labeled information-flow enforcement in the kernel |
| Evidence | [AgentSight](https://github.com/eunomia-bpf/agentsight) | process, file, network, TLS, and resource evidence |
| History | [Akeep](https://github.com/eunomia-bpf/akeep) | versioned, restorable agent session history |

The eBPF instrumentation underneath comes from
[bpftime](https://github.com/eunomia-bpf/bpftime). Each project remains useful
on its own; Actime is how you install, run, and operate them together.

Actime needs `actplane` ≥ 0.1.8, `agentsight` ≥ 0.2.60, and `akeep` ≥ 0.2.0.
`actime doctor` checks the versions it finds and tells you what to upgrade.

## Sandbox modes

Sandboxed is the default, but Actime is useful in all four shapes:

```console
actime run -- claude                  # auto: docker → podman → bubblewrap → host
actime run --sandbox docker -- claude # container, workspace bind-mounted
actime run --sandbox bwrap  -- claude # namespaces only, no container runtime needed
actime run --sandbox host   -- claude # no isolation; policy and evidence still apply
actime attach --comm claude           # an agent that is already running
```

`--sandbox host` matters more than it sounds. On a developer workstation the
agent often *must* see the real machine. You lose the isolation plane and keep
the other three: the policy and evidence planes attach to the agent's process
tree directly on the host.

See [docs/sandbox.md](docs/sandbox.md).

## Policy

Policies are ActPlane rules over real OS effects. Actime ships three packs and
you can add your own:

```yaml
# actime.yaml
policy:
  mode: enforce                 # off | observe | enforce
  packs:
    - coding-agent-baseline     # system fence, evidence integrity, destructive VCS
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

When a rule fires, the agent is told why, in words, through its own hook
interface, so it corrects course instead of retrying the same blocked action.

See [docs/policies.md](docs/policies.md).

## Degradation

Actime is built to be useful on a laptop with no root and no Docker, and
stricter as the environment allows. Nothing below is an error:

| Missing | What happens |
|---|---|
| root / `CAP_BPF` | policy and evidence off, isolation and history still run |
| Docker and Podman | falls back to bubblewrap, then host, with a warning |
| `actplane` | policy plane off in `observe` mode; a hard failure in `enforce` (fail closed) |
| `agentsight` | evidence plane off; process-level fallback still records argv, exit, duration |
| `akeep` | history plane off |
| kernel < 5.10 | policy plane off, with your kernel version in the reason |

Every run produces a manifest and a report, even when only the fallback ran.
`actime doctor` tells you exactly which planes your machine supports and how to
turn on the rest.

## Requirements

- Linux. The policy and evidence planes need kernel 5.10+ with BTF
  (`/sys/kernel/btf/vmlinux`); 6.1+ is recommended for the full runtime. The
  isolation and history planes have no kernel requirement.
- Root, or `CAP_BPF`/`CAP_PERFMON` on the engine binaries, for the policy and
  evidence planes only. Everything else runs unprivileged.
- Docker, Podman, or Bubblewrap for the isolation plane. Optional; without any
  of them Actime uses the `host` backend.

macOS is not supported: the prebuilt binaries are Linux-only, and the eBPF
planes need a Linux host kernel.

## Documentation

- [Quick start](docs/quickstart.md)
- [Configuration reference](docs/configuration.md): every field of `actime.yaml`
- [Sandbox backends](docs/sandbox.md)
- [Policies](docs/policies.md): writing and testing your own rules
- [Evidence and reports](docs/evidence.md): the run record, JSON, export
- [Design](docs/DESIGN.md): the architecture contract
- [FAQ](docs/faq.md)

## Who this is for

Platform and security teams that already have coding agents inside the
building, on machines the company owns, and need to answer: which agents ran,
what did they touch, what did they send where, what did they cost, and what
stopped them. Actime is not a hosted sandbox service and does not want your
code. It is local-first and sends nothing anywhere.

## Related work

Actime deliberately does not compete with hosted agent-execution platforms
(AWS AgentCore, E2B, Daytona): bring your own environment and Actime layers
onto it. It also does not sit at the identity or tool-authorization layer. It
owns the layer between: what the agent's actions actually *do* to a machine,
and the record of it.

## Contributing

Issues and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md)
and [SECURITY.md](SECURITY.md).

## License

MIT. See [LICENSE](LICENSE).
