# Deployment positions

Actime does not manage sandboxes. Its position relative to a sandbox is a
**deployment choice**, not an architectural constraint. This document covers
the three supported positions: when to choose each, exactly how to set it up,
what the tamper story really is, and what breaks.

The invariant that holds in every position: Actime observes and enforces
**below the tool layer**, at the syscall/effect boundary. A policy rule holds
whether the agent used a tool call, a bash one-liner, a Python subprocess, or a
subagent it spawned. What changes between positions is *who can interfere with
the recorder*.

> The sandbox contains. Actime accounts.

That line describes roles, not position. Read the tamper story for your
position before you rely on it.

## At a glance

| | Position | When | Tamper story |
|---|----------|------|--------------|
| **A** | Outside the sandbox — Actime on the host, attached to an existing container's process tree | You own the host and have root/`CAP_BPF` there | Strongest: root inside the container cannot disable the recorder or edit the record |
| **B** | Inside the sandbox — Actime in the same container/VM as the agent | You do not own the host: E2B, Daytona, AWS AgentCore, managed Kubernetes | Weaker: root inside the container can interfere |
| **C** | No sandbox at all — a plain process on a machine | The common workstation case | Same as host-side attach to a plain process tree |

`actime doctor` detects which situation it is in. Inside a container it says so
and reports which capabilities are missing, with the fix.

## Position A: Actime on the host, sandbox on the host

The strongest position. The sandbox (a container or pod you or your platform
created) contains the agent; Actime runs on the host, outside that boundary,
and attaches the policy and evidence planes to the container's process tree
from outside.

**Choose it when** you own the host: developer workstations, CI runners,
self-managed Kubernetes nodes.

**Setup.** Create the sandbox however you normally would — Actime never
creates, starts, stops, or removes containers. Then attach:

```console
# you (or your platform) create and start the sandbox
docker run -d --name agent-box your-agent-image sleep infinity

# Actime attaches to the already-running container's process tree
sudo actime attach --container agent-box

# or a Kubernetes pod on this node
sudo actime attach --pod default/agent-0
```

`--container` accepts a Docker or Podman name or id and resolves it with
`docker inspect` / `podman inspect` to the container's host pid. `--pod` takes
`namespace/name`, resolves it through `kubectl` to the container id, then to a
host pid. In both cases the evidence plane is pointed at the container
(`docker://…` / `k8s://…`), not just the pid, so AgentSight sees the container
boundary. If the target does not exist, Actime prints a clear error and stops.

Attach binds the planes to future events; it does not reconstruct anything the
agent did before you attached. Actime holds until the target exits or you
detach with Ctrl-C, then writes the manifest and report as usual. The history
plane does not run under `attach` (there is no run exit to commit on), so the
report shows it `Disabled` with the reason `attach does not commit history`.

**Tamper story.** The recorder and the record live on the host. An agent that
gets root *inside* its container cannot kill the recorder, unload the eBPF
programs, or edit the run directory — those are all on the other side of the
container boundary, in a mount namespace the container cannot see.

**Needs.** Root or `CAP_BPF` (often plus `CAP_PERFMON`) on the host, for the
policy and evidence planes. Docker/Podman/`kubectl` present for target
resolution.

**What breaks.** You cannot use this position when you do not own the host —
which is exactly the situation managed sandbox platforms put you in. That is
what position B is for.

## Position B: Actime inside the sandbox

Actime runs in the same container or VM as the agent, wrapping it directly:

```console
actime run -- claude
```

**Choose it when** you do not own the host. This is the only option on E2B,
Daytona, AWS AgentCore, managed Kubernetes, or anyone else's microVM — you
cannot attach from a host you cannot log into. It is also the path for a
sandbox vendor that ships Actime inside its image, so every sandbox comes up
with the planes already on.

**Setup.** The container needs the eBPF capabilities granted to it. With
Docker:

```console
docker run --rm -it \
  --cap-add BPF --cap-add PERFMON \
  -v "$PWD":/workspace -w /workspace \
  your-agent-image \
  actime run -- claude
```

On kernels older than 5.8 there is no `CAP_BPF`; grant `CAP_SYS_ADMIN`
instead. With Kubernetes, put the capabilities in the pod spec:

```yaml
securityContext:
  capabilities:
    add: ["BPF", "PERFMON"]
```

Inside, Actime behaves exactly as it does on a workstation: same
`actime.yaml`, same `actime run` / `actime report`, same run directory. The
`${WORKSPACE}` substitution in policies uses the path as the agent sees it in
the container, so the same policy pack works at the guest path and at a real
host path.

**Tamper story — read this before relying on it.** The recorder now shares a
kernel boundary with the agent but not a container boundary. An agent that
gets root inside the sandbox *can* interfere: it can kill the Actime process,
delete the run directory, or (with the same capabilities you granted for
eBPF) detach the probes. Actime's report records plane states honestly, so a
record that ends abruptly is visible as such — but this position is not
tamper-resistant against root in the same container, and no Actime document
or message should claim otherwise. If you need the strong guarantee, you need
position A, which means owning the host.

What you still get in position B, even against an agent without root:
kernel-enforced policy on every effect, the full evidence record, and session
history — enforced below the tool layer, which a non-root agent cannot route
around.

**Needs.** `CAP_BPF` (often plus `CAP_PERFMON`, or `CAP_SYS_ADMIN` on older
kernels) granted to the container, and a kernel with BTF
(`/sys/kernel/btf/vmlinux`). If the platform will not grant the capability,
the policy and evidence planes degrade and the reason lands in the manifest;
`actime doctor` inside the container tells you exactly what is missing.

**What breaks.** Platforms that refuse extra capabilities leave you with the
history plane and the process-level fallback only. Some platforms hide
`/sys/kernel/btf` — same result. And the tamper caveat above is inherent to
the position, not a configuration problem.

## Position C: no sandbox at all

A plain process on a machine:

```console
actime run -- claude
```

Actime launches the agent as a host child in your real cwd and environment
(under `actplane run` when the policy plane is on) and attaches the planes to
its process tree. No container is created, ever.

**Choose it when** you are on a workstation and the agent must see the real
machine anyway — real home directory, real credentials, real hardware. This is
the most common case.

**Tamper story.** The same as host-side attach: the recorder runs outside the
agent's process tree, so the agent (without root) cannot disable it or edit
the record. There is no container boundary because there is no container;
containment is simply not part of this position's job.

**What breaks.** Nothing isolates the agent's resources or filesystem view —
that is what a sandbox is for, and Actime does not provide one. The policy
plane still stops the effects its rules cover (`git push --force`, `rm -rf`,
secret egress), but an unconstrained agent otherwise shares your machine.
Pair position C with the `coding-agent-baseline` pack at minimum.

## Choosing

- Own the host? **A** for sandboxes, **C** for plain processes. This is the
  only way to get the strong tamper story.
- On someone else's platform (E2B, Daytona, AgentCore, managed Kubernetes)?
  **B** — the only option, and a real one; just do not read a host-side
  guarantee into it.
- Unsure what you are looking at? Run `actime doctor`. The `deployment` check
  names the position it detects and prints the capability fix for your case.

Whichever position you choose, the artifacts are identical: a manifest, a
report, and a run directory under `~/.local/share/actime/runs/<run-id>/`
(override with `ACTIME_HOME`). See [evidence.md](./evidence.md) for the record
layout and [configuration.md](./configuration.md) for every config field.
