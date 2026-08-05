# Actime Design

This document is the implementation contract. Every crate is written against the
types and invariants described here.

## 1. What Actime is

Actime is a **unified runtime for AI coding agents**. It runs an existing,
unmodified agent (Claude Code, Codex, Gemini CLI, OpenCode, OpenClaw, or any
command) and wraps it in four planes:

| Plane | Component | Question it answers |
|-------|-----------|---------------------|
| Isolation | sandbox backend (Docker / Podman / Bubblewrap / host) | What can the agent reach? |
| Policy | [ActPlane](https://github.com/eunomia-bpf/ActPlane) | What is the agent allowed to do? |
| Evidence | [AgentSight](https://github.com/eunomia-bpf/agentsight) | What did the agent actually do? |
| History | [Akeep](https://github.com/eunomia-bpf/akeep) | What did the agent decide, and can we replay it? |

The load-bearing architectural claim:

> **The sandbox contains. Actime accounts.**
> Policy and evidence live *outside* the sandbox, in the kernel, on the host.
> An agent that escapes its tool layer, spawns a shell, or writes a Python
> subprocess still cannot escape the effect plane, and cannot edit the record.

Default form is **sandboxed**: the agent runs in a container, and the eBPF
policy/evidence engines attach from the host to the container's process tree.
`--sandbox host` runs the same planes directly on the host for workstation use.

## 2. Non-goals

- Actime is not an agent framework, agent loop, or model router.
- Actime does not host compute or sell a microVM fleet.
- Actime does not proxy LLM traffic or require an SDK.
- Actime does not build its own eBPF programs. It orchestrates ActPlane and
  AgentSight, which own their kernel code.

## 3. Repository layout

```
crates/actime-core/      config, profiles, component resolution, run store,
                         evidence aggregation, reports, doctor checks
crates/actime-sandbox/   Sandbox trait + docker / podman / bwrap / host backends
crates/actime-cli/       the `actime` binary (clap surface, orchestration)
profiles/                observe.yaml, balanced.yaml, strict.yaml
policies/                ActPlane policy packs shipped with actime
sandbox/                 Dockerfile for the default agent sandbox image
scripts/install.sh       one-line installer
docs/                    user documentation
tests/                   end-to-end integration tests
```

## 4. Configuration: `actime.yaml`

Resolution order (first hit wins, then merged over the named profile):

1. `--config <FILE>`
2. `./actime.yaml`, then walking up to the git root
3. `~/.config/actime/actime.yaml`
4. built-in `balanced` profile

```yaml
version: 1
profile: balanced            # observe | balanced | strict | <path to profile yaml>

sandbox:
  backend: auto              # auto | docker | podman | bwrap | host
  image: ghcr.io/eunomia-bpf/actime-sandbox:latest
  workdir: /workspace
  mounts:                    # host:container:mode
    - ".:/workspace:rw"
  network: allow             # allow | deny | egress
  allow_egress: []           # hostnames allowed when network: egress
  env_passthrough:           # host env vars copied into the sandbox
    - ANTHROPIC_API_KEY
    - OPENAI_API_KEY
  cpus: null                 # e.g. 4
  memory: null               # e.g. "8G"
  keep: false                # keep container after exit for debugging

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
work, using the `balanced` profile and `backend: auto`.

## 5. Core types (`actime-core`)

These signatures are the integration contract. Do not change them without
updating this document.

```rust
// config.rs
pub struct Config {
    pub version: u32,
    pub profile: String,
    pub sandbox: SandboxConfig,
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
pub enum NetworkMode { Allow, Deny, Egress }

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
    pub sandbox: SandboxReport,
    pub planes: PlaneStatus,                     // which planes were actually active
    pub components: BTreeMap<String, String>,    // name -> version
    pub summary: RunSummary,
    pub exit_code: Option<i32>,
    pub akeep_commit: Option<String>,
}
pub struct PlaneStatus { pub isolation: PlaneState, pub policy: PlaneState, pub evidence: PlaneState, pub history: PlaneState }
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
  actplane/              ActPlane-owned feedback tree (host wrap / `actplane run`)
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
**harvests** those scoped `events.jsonl` files into `violations.jsonl` before
rendering the report. The nested `actplane/runs/` tree is expected, not a leak.

**Host wrap teardown:** the sandbox child under `--sandbox host` with an active
policy is `actplane run -- <agent>`. If that process outlives the agent
(stuck eBPF event-loop join), Actime reaps it after a short idle window so
`finish()` always runs.

## 6. Sandbox contract (`actime-sandbox`)

```rust
pub struct SandboxSpec {
    pub image: String,
    pub workdir: PathBuf,
    pub mounts: Vec<Mount>,
    pub env: Vec<(String, String)>,
    pub network: NetworkMode,
    pub allow_egress: Vec<String>,
    pub cpus: Option<f64>,
    pub memory: Option<String>,
    pub keep: bool,
    pub name: String,             // "actime-<run-id>"
}

pub struct Mount { pub host: PathBuf, pub guest: PathBuf, pub readonly: bool }

/// A live sandbox instance.
pub trait Sandbox: Send {
    fn kind(&self) -> Backend;
    /// PID on the *host* of the sandbox's root process. This is the anchor the
    /// policy and evidence planes attach to. `None` means the backend cannot
    /// expose one, and eBPF planes must degrade.
    fn host_pid(&self) -> Option<i32>;
    /// Backend-native target string for AgentSight `--binary-path`,
    /// e.g. "docker://actime-<id>". `None` for host mode.
    fn evidence_target(&self) -> Option<String>;
    /// Bring the sandbox up *without* starting the agent. After this returns,
    /// `host_pid()` and `evidence_target()` must be usable, so the policy and
    /// evidence planes can attach before any agent code runs. For Docker and
    /// Podman this starts the container detached with a long-lived idle
    /// entrypoint. For bwrap and host it is a no-op.
    fn start(&mut self) -> Result<()>;
    /// Start the agent inside the already-started sandbox. Non-blocking.
    fn spawn(&mut self, argv: &[String]) -> Result<()>;
    fn wait(&mut self) -> Result<i32>;
    fn signal(&mut self, sig: i32) -> Result<()>;
    fn cleanup(&mut self) -> Result<()>;
    fn report(&self) -> SandboxReport;
}

pub enum Backend { Docker, Podman, Bwrap, Host }

impl Backend {
    /// Probe order for `auto`: Docker, Podman, Bubblewrap, Host.
    pub fn detect_available() -> Vec<Backend>;
    pub fn probe(&self) -> BackendProbe;      // available? why not? version?
}

pub fn create(backend: Backend, spec: SandboxSpec) -> Result<Box<dyn Sandbox>>;
```

Backend rules:

- **Docker / Podman** — the default. Container named `actime-<run-id>`, image
  `sandbox/Dockerfile`. The workspace is bind-mounted. `host_pid()` comes from
  `inspect --format '{{.State.Pid}}'`. `network: deny` uses `--network none`;
  `network: egress` starts the container on an internal network and is
  documented as best-effort at the DNS level, with the authoritative egress
  control coming from the ActPlane `connect` rules.
- **Bwrap** — `bubblewrap` namespace sandbox for machines with no container
  runtime. Read-only `/`, writable workspace and `$HOME/.cache`, `--die-with-parent`.
  `host_pid()` is the direct child pid.
- **Host** — no isolation. Runs the command as a normal child. Must still work
  and must print a one-line warning that the isolation plane is off.

Every backend must work with **no privileges**. Only the policy and evidence
planes need root, and both degrade to disabled with a clear reason.

## 7. Orchestration (`actime-cli`)

`actime run -- <argv>` executes this sequence. Every step is fail-soft except
step 4 in `enforce` mode.

1. Resolve config, profile, CLI overrides. Resolve components.
2. `RunStore::create` → run directory, write effective `actime.yaml`.
3. Create the sandbox (`Backend::detect_available` when `auto`) and call
   `start()`, so `host_pid()` is known before the agent exists.
4. Start the **policy plane**: compile the merged policy packs with
   `actplane compile`, then `actplane attach --pid <host_pid>` (sandbox) or
   `actplane run` (host). In `enforce` mode a failure here aborts the run:
   fail closed. In `observe` mode it degrades. Violations are tailed into
   `violations.jsonl`.
5. Start the **evidence plane**: `agentsight record --binary-path <target>` or
   `agentsight record -- <argv>` for host mode, writing into the run directory.
   Always fail-soft.
6. Spawn the agent, stream stdio, forward signals, enforce `limits.wall_clock`.
7. On exit: stop the planes, collect `Evidence`, update the manifest, run
   `akeep commit` when history is enabled, render `report.md`, print the summary.

The exit code of `actime run` is the agent's exit code. `--fail-on-violation`
makes any `kill`/`block` violation force exit code 3.

## 8. Degradation matrix

This is what makes Actime work out of the box. No step below is an error.

| Missing | Effect |
|---------|--------|
| root / `CAP_BPF` | policy + evidence disabled, isolation + history still run; `doctor` explains |
| Docker and Podman | fall back to bwrap, then host, with a warning |
| `actplane` binary | policy plane disabled in `observe`; hard error in `enforce` |
| `agentsight` binary | evidence plane disabled; process-level fallback still records argv, exit, duration |
| `akeep` binary | history plane disabled |
| kernel < 5.10 | policy plane disabled with the kernel version in the reason |

`actime run` always produces a manifest and a report, even when only the
process-level fallback ran.

## 9. Profiles

- **observe** — sandbox `auto`, policy `observe`, evidence on, history on.
  Nothing is ever blocked. The onboarding default for a new team.
- **balanced** (default) — sandbox `auto`, policy `enforce` with the
  `coding-agent-baseline` pack, evidence on, history on. Blocks destructive and
  exfiltration-shaped effects, allows normal development.
- **strict** — sandbox required (run fails if only host is available), policy
  `enforce` with `coding-agent-baseline` + `no-egress` + `no-vcs-write`,
  `network: egress` with an explicit allowlist, evidence on with `otlp` export.

## 10. CLI surface

```
actime init [--profile P] [--force]
actime run [--sandbox B] [--policy MODE] [--profile P] [--no-evidence]
           [--no-history] [--fail-on-violation] [--] <cmd>...
actime shell [--sandbox B]
actime attach (--pid N | --comm NAME)
actime status
actime runs [--json] [--limit N]
actime report [RUN|latest] [--json|--markdown]
actime policy (list | show PACK | check | explain)
actime keep (commit [-m MSG] | log | restore RUN)
actime sandbox (info | build | pull)
actime doctor [--json]
actime demo
```

## 11. Demo

`actime demo` must work on a laptop with no agent installed, no root, and no
Docker. It runs a bundled script that reads files, spawns subprocesses, opens a
network connection, and attempts one policy-violating action, then prints the
report. This is the 30-second proof that the whole pipeline is wired.
