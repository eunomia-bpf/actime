# syntax=docker/dockerfile:1
#
# Default agent sandbox image for Actime.
#
# The image is intentionally generic: it ships a normal Linux userspace with the
# toolchain a coding agent tends to need (git, ripgrep, jq, python3, node 22),
# a non-root `agent` user, and a writable /workspace. It carries no Actime
# binaries and no eBPF code -- the policy and evidence planes attach from the
# host, never from inside this container. See docs/sandbox.md.
#
# Build the bundled image:
#   docker build -t actime-sandbox sandbox/
# or, skip preinstalling the agent CLIs to speed up CI/local iteration:
#   docker build --build-arg INSTALL_AGENTS=false -t actime-sandbox sandbox/

ARG NODE_MAJOR=22

FROM debian:bookworm-slim AS base

# Single apt layer: system packages + Node.js 22 from NodeSource.
ARG NODE_MAJOR
ARG INSTALL_AGENTS=true

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        git \
        ripgrep \
        jq \
        build-essential \
        python3 \
        python3-pip \
        python3-venv \
    && curl -fsSL "https://deb.nodesource.com/setup_${NODE_MAJOR}.x" | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/*

# Non-root user the agent process runs as. UID/GID 1000 keeps it predictable
# for bind-mounted workspaces on most Linux laptops. /workspace is created and
# owned by agent so the image is also usable standalone without a bind mount.
RUN groupadd --system --gid 1000 agent \
    && useradd --uid 1000 --gid 1000 --create-home --shell /bin/bash agent \
    && mkdir -p /workspace \
    && chown 1000:1000 /workspace

# Optionally preinstall the common coding agents so the image is useful
# out-of-the-box. Disable with --build-arg INSTALL_AGENTS=false for faster
# builds (CI) or when you only ever run your own agent command.
RUN if [ "${INSTALL_AGENTS}" = "true" ]; then \
        npm install -g \
            @anthropic-ai/claude-code \
            @openai/codex \
            @google/gemini-cli; \
    fi

# Where Actime bind-mounts the host workspace by default (sandbox.workdir).
WORKDIR /workspace

USER agent

CMD ["bash"]

LABEL org.opencontainers.image.source="https://github.com/eunomia-bpf/actime" \
      org.opencontainers.image.description="Default agent sandbox image for Actime: the runtime that runs an unmodified coding agent and accounts for what it does." \
      org.opencontainers.image.licenses="MIT"
