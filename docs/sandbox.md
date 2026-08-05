# Sandbox and the isolation plane

The sandbox answers one question: **what can the agent reach?** Actime supports
four backends that trade off convenience against isolation strength. The
sandbox is only one of the four planes, though: the policy and evidence planes
do not run inside it.

> Reference: [docs/DESIGN.md §6](./DESIGN.md#6-sandbox-contract-actime-sandbox)
> is the contract this document explains. Where the two disagree, the binary
> behavior described here is what 0.1.0 does.

## Why the eBPF planes attach from the host

This is the load-bearing design decision in Actime, so it comes first.

The policy plane (ActPlane) and evidence plane (AgentSight) are eBPF programs.
Actime attaches them to the sandbox's process tree **from the host kernel**, not
from inside the container. The orchestrator does this in two steps:

1. `Sandbox::start()` brings the sandbox up *without* the agent, so the planes
   can attach to a host pid first.
2. The agent is then spawned inside the already-watched sandbox.

What the attach pid is depends on the backend:

- **docker / podman**: the container's init pid, from
  `inspect --format '{{.State.Pid}}'`. The container is started detached with an
  idle entrypoint, so the pid exists before any agent code runs.
- **host**: the pid of the `actime` process itself. The agent is later spawned
  as its child, so attaching to that process tree covers the agent.
- **bwrap**: no pid is available before the agent starts, so in 0.1.0 the
  policy and evidence planes are disabled under `bwrap` (the report says
  `the sandbox backend does not expose a host pid to attach to`). The
  namespace isolation itself works fine.

The reason this matters more than the mechanism:

> An agent that escapes its tool layer, spawns a shell, or writes a Python
> subprocess still cannot escape the effect plane, and cannot edit the record.

If the policy and evidence engines ran *inside* the sandbox, a sufficiently
clever agent (or a buggy one) could kill them, ptrace them, rewrite the SQLite
store, or simply not launch them. By attaching from the host kernel, the record
is outside the agent's reach by construction. This is why Actime never collapses
`start()` back into `spawn()`: the attach must happen before the agent runs.

## The four backends

| Backend | What it is | What it isolates | Attach pid | eBPF planes in 0.1.0 |
|---------|-----------|------------------|------------|----------------------|
| `docker` | OCI container via Docker | Filesystem, process tree, network namespace | container init pid via `inspect` | yes (root or `CAP_BPF`) |
| `podman` | OCI container via Podman | Same as Docker | container init pid via `inspect` | yes (root or `CAP_BPF`) |
| `bwrap` | Bubblewrap namespace sandbox | Read-only `/`, writable workspace + `$HOME/.cache`, `--die-with-parent` | none before spawn | no (disabled) |
| `host` | No isolation | Nothing | the `actime` process pid | yes (root or `CAP_BPF`) |

Notes per backend:

- **docker / podman** -- the default. The container is named `actime-<run-id>`,
  uses the image from `sandbox.image` (default
  `ghcr.io/eunomia-bpf/actime-sandbox:latest`, built from
  [`sandbox/Dockerfile`](../sandbox/Dockerfile)), and bind-mounts the workspace
  into `sandbox.workdir` (`/workspace` by default). `network: deny` maps to
  `--network none`. `network: egress` starts the container on an internal
  network; see [network modes](#network-modes) for why that is only half the
  story.

- **bwrap** -- for machines with no container runtime. It gives a real namespace
  sandbox: read-only root filesystem, a writable workspace, a writable
  `$HOME/.cache`, and `--die-with-parent` so the sandbox dies if Actime dies.
  Because there is no long-lived sandbox process to attach to before the agent
  starts, the policy and evidence planes sit this one out in 0.1.0; you get
  isolation, history, and the process-level record.

- **host** -- no isolation at all. The agent runs as a normal child process of
  `actime`. The report marks the isolation plane
  `Degraded: host mode: no isolation`. The policy and evidence planes still
  work here (they attach to the process tree on the host), which makes `host`
  the workstation mode: no boundary, full accounting. The `strict` profile
  refuses to run on this backend.

Every backend works with **no privileges**. Only the policy and evidence planes
need root or `CAP_BPF`, and both degrade to disabled with a clear reason when
they cannot get it.

## Probe order (`backend: auto`)

When `sandbox.backend` is `auto` (the default), Actime probes in this order and
uses the first one that works:

1. **Docker**
2. **Podman**
3. **Bubblewrap**
4. **Host** (always available)

`actime sandbox info` shows the probe result for each backend and which one
`auto` would choose:

```text
Sandbox backends, in the order `auto` probes them:

  docker     available
             docker: available (29.1.3)
  podman     available
             podman: available (4.9.3)
  bwrap      available
             bwrap: available (bubblewrap 0.9.0)
  host       available
             host: available

`--sandbox auto` would choose: docker
```

The chosen backend is recorded in the run manifest. You can force a backend
with `--sandbox B` on the CLI or `sandbox.backend` in `actime.yaml`.

## Network modes

`sandbox.network` has three values. The coarse switch lives in the sandbox
backend; the authoritative one lives in the policy plane.

| Mode | Sandbox behavior | What it actually guarantees |
|------|------------------|------------------------------|
| `allow` | normal networking | Nothing: the agent can reach anything reachable from the sandbox's network. |
| `deny` | `--network none` (docker/podman); equivalent namespace isolation elsewhere | No network at all. This is the **coarse** kill switch. |
| `egress` | internal network + DNS-best-effort allowlist (`sandbox.allow_egress`) | Convenience filtering only. DNS is bypassable from inside the container, so this is **not** a security boundary on its own. |

Why two layers? Because `--network none` is all-or-nothing: either the agent has
network or it has none. Real agents need *some* network (to reach an LLM API)
but should not be able to phone home to anything else. That fine-grained control
cannot live in the sandbox, because anything inside the sandbox is under the
agent's control.

So the authoritative egress control is the **ActPlane `connect` rules**, applied
by the policy plane in the host kernel. They fire on the actual `connect()`
syscall, see the real destination, and apply `notify` / `block` / `kill`
effects. An agent cannot bypass them because they are not running inside the
sandbox. The `strict` profile turns this on by combining `network: egress` with
the `no-secret-egress` policy pack and an explicit `allow_egress` list.

In short: `--network none` is the coarse off switch; ActPlane `connect` rules
are the policy.

## Using your own image

Point Actime at any image with `sandbox.image` or `--image`:

```yaml
sandbox:
  image: registry.example.com/your-org/actime-sandbox:1.2.0
```

The image only needs to be a usable Linux userspace with the tools your agent
calls. It carries **no** Actime binaries and **no** eBPF code; the planes
attach from the host. To build your own, start from the default image or the
`Dockerfile`; see [`sandbox/README.md`](../sandbox/README.md) for build flags
(`INSTALL_AGENTS`, `NODE_MAJOR`) and customization recipes.

The first `docker`/`podman` run needs the image locally. `docker run` attempts
a pull automatically; if the image cannot be pulled, the run stops with an
error that names the fixes:

- `actime sandbox pull` pulls the published image.
- `actime sandbox build` builds `sandbox/Dockerfile` from a checkout
  (`--tag` to override the tag).
- `sandbox.image` in `actime.yaml`, or `--image`, points at an image you have.

For private registries, make your `docker`/`podman` credentials available to
the user that runs Actime; Actime itself never handles registry credentials.

`actime shell` opens an interactive shell inside the sandbox (same backend
selection, history plane off), which is the fastest way to poke at an image.

## Privileges summary

| Capability | Needs privileges? | If unavailable |
|------------|-------------------|----------------|
| Isolation plane (any backend) | No | `auto` falls down the probe order to `host` |
| Policy plane (ActPlane) | Yes: root or `CAP_BPF`, kernel >= 5.10 | Disabled in `observe`; hard error in `enforce` (fail closed) |
| Evidence plane (AgentSight) | Yes: root or `CAP_BPF` | Disabled; process-level fallback still records argv/exit/duration |
| History plane (Akeep) | No | Disabled |

When you run unprivileged, Actime launches the engines through `sudo -E`
(interactively it may ask for your password once; in pipes and CI it uses
`sudo -n` and fails fast rather than hanging). Granting
`cap_bpf,cap_perfmon` to the engine binaries with `setcap` avoids sudo
entirely; `actime doctor` prints the exact command.

The combination is deliberate: a developer can run `actime run -- claude`
unprivileged and still get isolation, history, and a report. Raising privileges
(up to and including `sudo`) only upgrades the policy and evidence planes; it
never gives the agent more reach.

## Degradation

The full degradation matrix lives in
[docs/DESIGN.md §8](./DESIGN.md#8-degradation-matrix). For the sandbox
specifically: when Docker and Podman are missing, `auto` selects bwrap and then
host, the manifest records which backend ran, and nothing in the sandbox layer
ever aborts a run. The one sandbox-adjacent hard failure is a missing or
unpullable container image on the `docker`/`podman` backends, which stops the
run with an error naming the fixes (see above).
