# Actime agent sandbox image

This directory contains the default `Dockerfile` for the container Actime runs
your coding agent in. The image is intentionally generic: a normal Debian
userspace with the toolchain a coding agent tends to need, a non-root `agent`
user, and a writable `/workspace`. It carries **no Actime binaries and no eBPF
code**. The policy plane (ActPlane) and evidence plane (AgentSight) attach to
the container's process tree from the host kernel -- never from inside the
container. This is what makes the record tamper-resistant. See
[docs/sandbox.md](../docs/sandbox.md) for why.

## What is in the image

- Base: `debian:bookworm-slim`.
- System packages: `ca-certificates`, `curl`, `git`, `ripgrep`, `jq`,
  `build-essential`, `python3`, `python3-pip`, `python3-venv`.
- Node.js 22, installed from NodeSource.
- A non-root user `agent` (uid/gid 1000) with a writable `$HOME`.
- `WORKDIR /workspace`, where Actime bind-mounts your project by default.
- Optional preinstalled agents: `@anthropic-ai/claude-code`, `@openai/codex`,
  `@google/gemini-cli`. These are a convenience, not a requirement -- Actime can
  run any command.

The published image lives at
`ghcr.io/eunomia-bpf/actime-sandbox:latest` and is rebuilt on every release.

## Build

Build the bundled image locally:

```sh
docker build -t actime-sandbox sandbox/
```

Or, with Podman:

```sh
podman build -t actime-sandbox sandbox/
```

Skip preinstalling the agent CLIs (faster, smaller, useful for CI or when you
only ever run your own agent command):

```sh
docker build --build-arg INSTALL_AGENTS=false -t actime-sandbox sandbox/
```

Pin a different Node.js major version:

```sh
docker build --build-arg NODE_MAJOR=20 -t actime-sandbox sandbox/
```

Verify the image:

```sh
docker run --rm -it actime-sandbox --version
docker run --rm -it --entrypoint /bin/bash actime-sandbox -c 'id && which node git rg jq python3'
```

The first command should print `bash` (the default `CMD`); the second should
show `uid=1000(agent)` and the paths of the installed tools.

## Customize

Two common reasons to build your own image:

1. **Add tools your agent needs** (a language toolchain, project CLI, internal
   SDK). Extend the published image so you inherit future base updates:

   ```dockerfile
   FROM ghcr.io/eunomia-bpf/actime-sandbox:latest
   USER root
   RUN apt-get update && apt-get install -y --no-install-recommends golang-1.22 \
       && rm -rf /var/lib/apt/lists/*
   USER agent
   ```

2. **Preinstall a private or different agent.** Disable the bundled agents and
   install your own:

   ```dockerfile
   FROM ghcr.io/eunomia-bpf/actime-sandbox:latest
   USER root
   RUN npm install -g @your-org/your-agent
   USER agent
   ```

Keep the non-root `agent` user (uid 1000) and keep `WORKDIR /workspace` so
Actime's default bind-mount still lands in the right place. If you must change
the workdir, update `sandbox.workdir` in your `actime.yaml` to match.

## Point Actime at your image

Actime picks the sandbox image from `sandbox.image` in `actime.yaml`. The
default is `ghcr.io/eunomia-bpf/actime-sandbox:latest`. To use your own:

```yaml
# actime.yaml
sandbox:
  image: registry.example.com/your-org/actime-sandbox:1.2.0
```

Build and push your image, then run as usual:

```sh
actime run -- claude
```

Actime will pull the configured image on first use. For a private registry, make
your container runtime's credentials available to the `docker`/`podman` user
that Actime invokes; Actime itself never handles registry credentials.

## Related

- [docs/sandbox.md](../docs/sandbox.md) -- the four backends, probe order, and
  why the eBPF planes attach from the host.
- [docs/configuration.md](../docs/configuration.md) -- every `sandbox.*` field.
- [docs/DESIGN.md](../docs/DESIGN.md) -- the full design contract.
