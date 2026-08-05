# Security Policy

## Reporting a vulnerability

Please report security vulnerabilities **privately**. Do **not** open a public
GitHub issue.

Report in either of these ways:

- **Email:** security@eunomia.dev
- **GitHub Security Advisories:**
  https://github.com/eunomia-bpf/actime/security/advisories/new
  (Reports submitted through GitHub let us publish a fixed-version advisory
  directly. Both channels reach the same maintainers.)

When you report, please include:

- A description of the issue and its impact.
- The version of `actime` (`actime --version`) and, if relevant, the versions of
  `actplane`, `agentsight`, and `akeep`.
- The output of `actime doctor --json` and, if you have one, the run id.
- Steps to reproduce, or a proof of concept.

You will receive an initial acknowledgement within **5 business days**.

## What is in scope

Actime's core threat model is that the agent it runs may be adversarial or
compromised. The guarantees we care about most are:

- **The record cannot be tampered with from inside the sandbox.** The policy and
  observability planes attach from the host kernel; anything that lets the agent
  disable them, escape them, or edit the record/manifest from inside the
  sandbox is a serious issue.
- **Policy enforcement cannot be bypassed.** In `enforce` mode, an action that
  should be `block`ed or `kill`ed but is not, is a serious issue.
- **No silent network exfiltration by Actime itself.** Actime is local-first and
  ships no telemetry. Anything that causes it to phone home is a serious issue.
- Crashes, panics, or data loss in `actime run` that prevent a manifest/report
  from being written.

General security hardening of the underlying engines (ActPlane, AgentSight,
Akeep) belongs in those upstream projects. Actime coordinates versions, but each
engine's own security policy governs its eBPF code.

## Supported versions

Only the **latest minor release** line receives security fixes. We do not
maintain long-term backport branches at this stage of the project.

| Version | Supported |
|---------|-----------|
| 0.1.x   | yes (latest) |
| < 0.1   | no |

When a fix is released we will note the affected version range in the advisory
and in [CHANGELOG.md](./CHANGELOG.md).

## Disclosure timeline

We follow a **90-day coordinated disclosure** window:

1. You report privately.
2. We acknowledge within 5 business days and triage.
3. We work with you on a fix and a release date, targeting 90 days from the
   initial report.
4. We publish the fix in a release and a public advisory simultaneously, and
   credit you unless you prefer to remain anonymous.

If a fix is delayed beyond 90 days, we will keep you informed and coordinate on
a revised date. We will not publish details before a fix is available unless
actively being exploited in the wild, in which case we will publish a mitigation
advisory early.
