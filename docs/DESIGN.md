# Actime Design

This document is the implementation contract. Every crate is written against the
types and invariants described here.

## 1. What Actime is

Actime is the **effect plane for AI coding agents**. It attaches three planes
to an agent wherever that agent already runs:

| Plane | Component | Question it answers |
|-------|-----------|---------------------|
| Policy | [ActPlane](https://github.com/eunomia-bpf/ActPlane) | What is the agent allowed to do? |
| Evidence | [AgentSight](https://github.com/eunomia-bpf/agentsight) | What did the agent actually do? |
| History | [Akeep](https://github.com/eunomia-bpf/akeep) | What did the agent decide, and can we replay it? |

Actime does **not** manage sandboxes. Bring your own sandbox, or none at all.
Actime attaches to the process tree either way.

The roles stay:

> **The sandbox contains. Actime accounts.**

That line describes *roles*, not position. The invariant that is always true is
that Actime observes and enforces **below the tool layer**, at the
syscall/effect boundary — not that it sits below a sandbox.

### Deployment positions

Actime's position relative to a sandbox is a **deployment choice**, not an
architectural constraint. All three must work:

| | Position | When | Tamper story |
|---|----------|------|--------------|
| **A** | **Outside the sandbox** — Actime on the host, attached to an existing container's process tree | Workstations, CI runners, self-managed Kubernetes nodes where you own the host and have root/`CAP_BPF` | Strongest: root inside the container cannot disable the recorder |
| **B** | **Inside the sandbox** — Actime in the same container/VM as the agent | E2B, Daytona, AWS AgentCore, managed Kubernetes, someone else's microVM; also shipping Actime inside a vendor image | Weaker: root inside can interfere. Code and docs must say so honestly — never promise host-side tamper-resistance here |
| **C** | **No sandbox at all** — a process on a machine | Common workstation case | Same as host-side attach to a plain process tree |

For **B**, policy and evidence need `CAP_BPF` (and often `CAP_PERFMON` /
`CAP_SYS_ADMIN` depending on the kernel) granted to the container. `actime
doctor` detects in-container deployment and reports this with an actionable fix.

## 2. Non-goals

- Actime is not an agent framework, agent loop, or model router.
- Actime does not host compute or sell a microVM fleet.
- Actime does not create, start, stop, or remove containers or pods.
- Actime does not proxy LLM traffic or require an SDK.
- Actime does not build its own eBPF programs. It orchestrates ActPlane and
  AgentSight, which own their kernel code.

## 3. Repository layout

```
crates/actime-core/      config, profiles, component resolution, run store,
                         evidence aggregation, reports, doctor checks
crates/actime-cli/       the `actime` binary (clap surface, orchestration)
profiles/                observe.yaml, balanced.yaml, strict.yaml
policies/                ActPlane policy packs shipped with actime
scripts/install.sh       one-line installer
docs/                    user documentation
tests/                   end-to-end integration tests
```

Policy packs and profiles are `include_str!`d into the binary by
`crates/actime-cli/src/embedded.rs`, so a single downloaded binary works with
nothing else on disk.

## 4. Configuration: `actime.yaml`

Resolution order (first hit wins, then merged over the named profile):

1. `--config <FILE>`
2. `./actime.yaml`, then walking up to the git root
3. `~/.config/actime/actime.yaml`
4. built-in `balanced` profile

```yaml
version: 1
profile: balanced            # observe | balanced | strict | <path to profile yaml>

policy:
  mode: enforce              # off | observe | enforce
  packs:                     # built-in packs from policies/
    - coding-agent-baseline
  files: []                  # extra ActPlane policy files
  feedback: true             # inject corrective feedback the agent can read

evidence:
  enabled: true
  capture: [process, file, network, ssl, resource]
  export: []                 # otlp | sqlite | json (json always written)
  redact: true               # strip auth headers and secret-shaped values

history:
  enabled: true
  commit_on_exit: true
  message: null              # default: "actime run <id>"

limits:
  wall_clock: null           # e.g. "2h" — kill the run after this
```

Every field is optional. `actime run -- claude` with no config file at all must
work, using the `balanced` profile.

`${WORKSPACE}` substitution happens once, in `compose_policy`. Policy files
must never hardcode a path: the same policy must work at a real host path
(deployment C) and at a guest path when the user runs Actime inside a container
(deployment B). For `actime run`, the workspace is the caller's real cwd.

## 5. Core types (`actime-core`)

These signatures are the integration contract. Do not change them without
updating this document.

```rust
// config.rs
pub struct Config {
    pub version: u32,
    pub profile: String,
    pub policy: PolicyConfig,
    pub evidence: EvidenceConfig,
    pub history: HistoryConfig,
    pub limits: LimitsConfig,
}
impl Config {
    pub fn load(explicit: Option<&Path>, start_dir: &Path) -> Result<Config>;
    pub fn builtin_profile(name: &str) -> Result<Config>;
    pub fn merge_cli(&mut self, overrides: &CliOverrides);
    pub fn to_yaml(&self) -> Result<String>;
}

pub enum PolicyMode { Off, Observe, Enforce }

// components.rs — resolve the three engines on PATH or in ~/.local/share/actime/bin
pub struct Component { pub name: &'static str, pub path: Option<PathBuf>, pub version: Option<String>, pub min_version: &'static str }
pub struct Components { pub actplane: Component, pub agentsight: Component, pub akeep: Component }
impl Components {
    pub fn detect() -> Components;
    pub fn install_hint(name: &str) -> String;   // "cargo install actplane"
}

// run.rs — one recorded execution
pub struct RunId(pub String);                    // "20260804-153012-a3f1"
pub struct Run {
    pub id: RunId,
    pub dir: PathBuf,                            // ~/.local/share/actime/runs/<id>
    pub manifest: Manifest,
}
pub struct Manifest {
    pub id: String,
    pub schema: u32,                             // 1
    pub started_at: String,                      // RFC3339
    pub ended_at: Option<String>,
    pub argv: Vec<String>,
    pub agent: String,                           // "claude" | "codex" | "command"
    pub cwd: PathBuf,
    pub profile: String,
    pub target: TargetReport,
    pub planes: PlaneStatus,                     // which planes were actually active
    pub components: BTreeMap<String, String>,    // name -> version
    pub summary: RunSummary,
    pub exit_code: Option<i32>,
    pub akeep_commit: Option<String>,
}

/// What Actime attached the planes to. Actime never owns this process tree.
pub struct TargetReport {
    pub kind: String,                 // "command" | "pid" | "comm" | "container" | "pod"
    pub spec: Option<String>,         // user-facing handle
    pub host_pid: Option<i32>,
    pub evidence_target: Option<String>, // "docker://…" | "k8s://…" when applicable
    pub note: Option<String>,
}

/// Exactly three planes. There is no isolation plane.
pub struct PlaneStatus {
    pub policy: PlaneState,
    pub evidence: PlaneState,
    pub history: PlaneState,
}
pub enum PlaneState { Active, Degraded(String), Disabled(String) }
pub struct RunSummary {
    pub violations: u64, pub blocked: u64, pub killed: u64,
    pub processes: u64, pub files_written: u64, pub endpoints: u64,
    pub llm_calls: u64, pub tokens_in: u64, pub tokens_out: u64,
    pub peak_rss_bytes: u64, pub cpu_seconds: f64, pub duration_seconds: f64,
}

pub struct RunStore { root: PathBuf }
impl RunStore {
    pub fn open_default() -> Result<RunStore>;
    pub fn create(&self, argv: &[String], cfg: &Config) -> Result<Run>;
    pub fn list(&self) -> Result<Vec<Manifest>>;
    pub fn get(&self, id: &str) -> Result<Run>;   // also accepts "latest"
    pub fn prune(&self, keep: usize) -> Result<usize>;
}

// evidence.rs — read back what the engines wrote
pub struct Evidence { pub violations: Vec<Violation>, pub summary: RunSummary, pub timeline: Vec<TimelineEntry> }
pub struct Violation {
    pub ts: String, pub rule: String, pub effect: String,   // notify | block | kill
    pub op: String, pub target: String, pub pid: i32, pub comm: String, pub reason: String,
}
impl Evidence {
    pub fn collect(run: &Run) -> Result<Evidence>;
}

// report.rs
pub fn render_text(run: &Run, ev: &Evidence, width: usize) -> String;
pub fn render_json(run: &Run, ev: &Evidence) -> Result<String>;
pub fn render_markdown(run: &Run, ev: &Evidence) -> String;

// doctor.rs
pub struct Check { pub name: String, pub status: CheckStatus, pub detail: String, pub fix: Option<String> }
pub enum CheckStatus { Ok, Warn, Fail, Skip }
pub fn run_checks(cfg: &Config) -> Vec<Check>;
```

### Run directory layout

```
~/.local/share/actime/runs/<run-id>/
  manifest.json          Manifest, rewritten on exit
  actime.yaml            the effective, fully resolved config for this run
  policy.yaml            ActPlane project file (YAML with `policy: |`); the engine loads this
  policy.dsl             composed pure ActPlane DSL (human-readable; not passed to --policy)
  violations.jsonl       harvested policy violations (Actime canonical path)
  evidence.db            AgentSight SQLite store (when the evidence plane ran)
  events.jsonl           normalized evidence events (always written)
  stdout.log / stderr.log
  report.md              rendered on exit
  actplane/              ActPlane-owned feedback tree (`actplane run` wrap)
    feedback.txt         corrective feedback (seed path; engine may re-scope)
    audit.jsonl
    runs/run-<pid>-<ts>/  scoped per-invocation dir written by ActPlane 0.1.x
      events.jsonl       raw ActPlane violation events (source of harvest)
      feedback.txt
      audit.jsonl
```

`Run::policy_path()` returns `policy.yaml` — that is the file ActPlane's `--policy`
flag accepts. ActPlane rejects a `.dsl` extension even when the contents are
YAML, so the engine project file must use a YAML extension. `policy.dsl` remains
in the run directory as the composed DSL alone for inspection and diffs.

**ActPlane feedback scoping (0.1.x):** `actplane run` always rewrites feedback
paths through `scoped_feedback_paths`, placing events under
`parent(feedback)/runs/run-<pid>-<ts>/events.jsonl` rather than the `events:`
path in policy.yaml. Actime therefore seeds feedback under `actplane/` and
**harvests** those scoped `events.jsonl` files into `violations.jsonl` after the
engine has fully exited.

**Policy-wrap teardown:** when policy is on, `actime run` launches the agent
under `actplane run -- <agent>`. If that process outlives the agent, Actime
waits for a natural exit so events can flush, then SIGTERM with a multi-second
grace, then SIGKILL. Harvest runs only after the engine has exited. A tool that
enforces a kill and then loses the event is worse than useless — but nothing
may hang a run forever.

## 6. Target model

Actime records a [`TargetReport`] in every manifest: what process tree the
planes attached to.

| `kind` | How it is resolved |
|--------|--------------------|
| `command` | `actime run -- <cmd>`: plain host child (or under `actplane run` when policy is on) |
| `pid` | `actime attach --pid N` |
| `comm` | `actime attach --comm NAME` (newest matching `/proc/*/comm`) |
| `container` | `actime attach --container REF` → `docker inspect` / `podman inspect` for host pid; evidence target `docker://REF` |
| `pod` | `actime attach --pod NS/POD` → `kubectl get pod -o json` → containerID → docker/podman/crictl inspect; evidence target `k8s://NS/POD` |

Actime **never** creates, starts, stops, or removes a container or pod. If the
target does not exist, print a clear error and stop.

AgentSight already understands `docker://` and `k8s://` as `--binary-path`
schemes; Actime passes those through when applicable.

## 7. Orchestration (`actime-cli`)

### `actime run -- <argv>`

Launches the agent as a plain child in the user's real cwd and environment.
No container is created, ever.

1. Resolve config, profile, CLI overrides. Resolve components.
2. `RunStore::create` → run directory, write effective `actime.yaml`.
3. Compose policy with `${WORKSPACE}` = the real host cwd.
4. **Rule enforceability (before any engine start):** resolve which rules in
   the composed policy this host's ActPlane engine can actually install. See
   §7.1. In `enforce` mode, if any requested rule is not enforceable, **fail
   closed before launching the agent**. In `observe` mode, proceed but record
   unenforceable rules in the manifest and report.
5. **Policy plane:** when mode is not `off`, prepare `policy.yaml` and later
   wrap the agent with `actplane --policy <file> run -- <argv>` (launch-time
   enforcement; the engine installs the composed policy for that run).
   Outcome is only **Active** when the policy is verifiably installed: if the
   engine log reports an install failure (feature budget, CAP_BPF, etc.), the
   plane is reclassified to **Disabled** with the engine's reason. In
   `enforce` mode a failure to start *or* install aborts the run (fail closed)
   rather than reporting Active while nothing was constrained.
6. **Evidence plane:** attach `agentsight record --pid <wrap-or-agent-pid>`
   once the child exists. Always fail-soft.
7. Wait for the agent / wrap. Enforce `limits.wall_clock`. Prefer natural
   engine exit so events flush; then SIGTERM with a long grace; then SIGKILL.
8. On exit: stop engines (bounded), **harvest violations only after engines
   have exited**, collect `Evidence`, update the manifest, run `akeep commit`
   when history is enabled, render `report.md`, print the summary.

### 7.1 Rule enforceability (host property)

The engine's supported rule classes are a **host property**, not a pack
property. Released ActPlane 0.1.8 pins a host-wide eBPF singleton whose feature
budget admits exec sinks (and plain connect) but **not** open/write sink rule
classes or path contains/suffix matchers. A policy that needs those features
fails the whole install — Actime must never report `policy Active` for a
partial or failed install.

Before a run, Actime:

1. Composes the policy (`${WORKSPACE}` substituted).
2. Calls `actplane compile --json` when available (per-clause `kernel_op`,
   `target_kind`, path patterns, file sources).
3. Combines that shape with the known engine feature budget for this ActPlane
   version and marks each rule `enforceable: bool` + `reason`.
4. Surfaces the table via `actime policy check` (no privileges required).

**`--policy enforce`:** if any rule is not enforceable, abort before the agent
starts, list the rules and missing engine features, and tell the operator to
drop those packs or use `--policy observe`. Silent partial enforcement is the
worst outcome.

**`--policy observe`:** proceed; store unenforceable rules on the manifest and
print them in the report so the operator knows the run did not watch for them.

**Install failure after launch:** if the engine log still reports a hard
install error, reclassify the policy plane to **Disabled** with the engine's
reason — never leave `Active` when nothing was constrained.

The exit code of `actime run` is the agent's exit code when known.
`--fail-on-violation` makes any `kill`/`block` violation force exit code 3.

### `actime attach`

Binds the planes to something already running. Does not reconstruct past
events. Holds until the target exits or the user detaches (Ctrl-C).

## 8. Degradation matrix

This is what makes Actime work out of the box. No step below is an error —
except `policy.mode: enforce`, which fails closed if the policy plane cannot
load.

| Missing | Effect |
|---------|--------|
| root / `CAP_BPF` | policy + evidence disabled; history still runs; `doctor` explains |
| running inside a container without CAP_BPF | same; doctor warns that this is deployment B without host-side tamper-resistance |
| `actplane` binary | policy plane disabled in `observe`; hard error in `enforce` |
| `agentsight` binary | evidence plane disabled; process-level fallback still records argv, exit, duration |
| `akeep` binary | history plane disabled |
| kernel < 5.10 | policy plane disabled with the kernel version in the reason |

`actime run` always produces a manifest and a report, even when only the
process-level fallback ran. Nothing in the exit path may be conditional on a
plane having worked.

## 9. Profiles

- **observe** — policy `observe`, evidence on, history on. Nothing is ever
  blocked. The onboarding default for a new team.
- **balanced** (default) — policy `enforce` with the `coding-agent-baseline`
  pack, evidence on, history on. Blocks destructive and exfiltration-shaped
  effects, allows normal development.
- **strict** — policy `enforce` with `coding-agent-baseline` + `no-vcs-write`
  (exec-based packs that released ActPlane can install), evidence on with
  `otlp` export, optional wall-clock limit. Fail closed on the policy plane.
  The `information-flow` pack (file sinks, secret labels) is shipped but not
  part of any default profile until the engine enables those rule classes.

## 10. CLI surface

```
actime init [--force] [--print]
actime run [--policy MODE] [--no-evidence] [--no-history]
           [--fail-on-violation] [--timeout D] -- <cmd>...
actime attach (--pid N | --comm NAME | --container REF | --pod NS/POD)
              [--policy MODE]
actime status
actime runs [--json] [--limit N]
actime report [RUN|latest] [--json|--markdown]
actime policy (list | show PACK | check | explain)
actime keep (commit [-m MSG] | log | restore RUN [--to DIR])
actime doctor [--json]
```

Global flags: `--config FILE`, `--profile NAME`, `--quiet`.

There is no `sandbox` subcommand, no `--sandbox` flag, no `demo` command, and
no `shell` command. Isolation is the user's responsibility.

## 11. Rules that are easy to get wrong

- **Fail soft everywhere except `enforce`.** A missing engine, no root, an old
  kernel — none of these may abort a run. They degrade a plane and record the
  reason in the manifest. The one exception is `policy.mode: enforce`.
- **Every run produces a manifest and a report**, even when only the
  process-level fallback ran.
- **Never create containers.** `attach --container` / `--pod` only resolve
  already-existing targets.
- **Harvest after engine exit.** Violations already produced by the kernel must
  not be lost because Actime reaped ActPlane before it flushed.
- **No unwrap/expect/panic in non-test code.** Return `anyhow::Result`.
- **Do not promise host-side tamper-resistance in deployment B.** Doctor and
  docs must state the weaker story honestly.
- **AgentSight's SQLite schema is not a stable interface.** Query
  `sqlite_master` and `PRAGMA table_info` before selecting.
