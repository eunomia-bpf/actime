//! Run store: identifiers, manifests, and on-disk layout for one agent execution.
//!
//! Layout (see `docs/DESIGN.md` §5):
//!
//! ```text
//! $ACTIME_HOME/runs/<run-id>/
//!   manifest.json
//!   actime.yaml
//!   policy.yaml            ActPlane project file (engine loads this)
//!   policy.dsl             composed pure DSL (human-readable)
//!   violations.jsonl
//!   evidence.db
//!   events.jsonl
//!   stdout.log / stderr.log
//!   report.md
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::enforceability::RuleEnforceability;

// ---------------------------------------------------------------------------
// RunId
// ---------------------------------------------------------------------------

/// Unique run identifier: `YYYYMMDD-HHMMSS-<4 hex>`.
///
/// Example: `20260804-153012-a3f1`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub String);

impl RunId {
    /// Generate a new run id from the local clock and a small random suffix.
    pub fn generate() -> RunId {
        let now = Local::now();
        let stamp = now.format("%Y%m%d-%H%M%S");
        let hex = random_hex4();
        RunId(format!("{stamp}-{hex}"))
    }

    /// Validate that `s` matches `YYYYMMDD-HHMMSS-<4 hex>`.
    pub fn parse(s: &str) -> Result<RunId> {
        if is_valid_run_id(s) {
            Ok(RunId(s.to_string()))
        } else {
            bail!("invalid run id `{s}` (expected YYYYMMDD-HHMMSS-<4 hex>)");
        }
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for RunId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

fn is_valid_run_id(s: &str) -> bool {
    // YYYYMMDD-HHMMSS-xxxx  → 8 + 1 + 6 + 1 + 4 = 20
    let b = s.as_bytes();
    if b.len() != 20 {
        return false;
    }
    if b[8] != b'-' || b[15] != b'-' {
        return false;
    }
    b[0..8].iter().all(|c| c.is_ascii_digit())
        && b[9..15].iter().all(|c| c.is_ascii_digit())
        && b[16..20].iter().all(|c| c.is_ascii_hexdigit())
}

fn random_hex4() -> String {
    // Mix clock nanos with pid — no extra deps, good enough for collision avoidance.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let n = (nanos ^ pid.wrapping_mul(0x9E37_79B9)) & 0xFFFF;
    format!("{n:04x}")
}

// ---------------------------------------------------------------------------
// Plane state
// ---------------------------------------------------------------------------

/// Lifecycle state of one Actime plane for a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlaneState {
    /// Plane ran successfully.
    Active,
    /// Plane ran with reduced capability; reason explains how.
    Degraded(String),
    /// Plane did not run; reason explains why.
    Disabled(String),
}

impl fmt::Display for PlaneState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlaneState::Active => write!(f, "active"),
            PlaneState::Degraded(r) => write!(f, "degraded ({r})"),
            PlaneState::Disabled(r) => write!(f, "disabled ({r})"),
        }
    }
}

impl PlaneState {
    /// Short status label without reason: Active / Degraded / Disabled.
    pub fn label(&self) -> &'static str {
        match self {
            PlaneState::Active => "Active",
            PlaneState::Degraded(_) => "Degraded",
            PlaneState::Disabled(_) => "Disabled",
        }
    }

    /// Optional reason string.
    pub fn reason(&self) -> Option<&str> {
        match self {
            PlaneState::Active => None,
            PlaneState::Degraded(r) | PlaneState::Disabled(r) => Some(r.as_str()),
        }
    }
}

/// Which planes were actually active for a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaneStatus {
    /// Policy (ActPlane) plane.
    pub policy: PlaneState,
    /// Evidence (AgentSight) plane.
    pub evidence: PlaneState,
    /// History (Akeep) plane.
    pub history: PlaneState,
}

impl Default for PlaneStatus {
    fn default() -> Self {
        Self {
            policy: PlaneState::Disabled("not started".into()),
            evidence: PlaneState::Disabled("not started".into()),
            history: PlaneState::Disabled("not started".into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Target report — what Actime attached to
// ---------------------------------------------------------------------------

/// Snapshot of the process tree Actime attached the planes to.
///
/// Actime does not own sandboxes. The user brings an execution environment;
/// this record describes the attach target (a launched command, a pid, a
/// container that already exists, etc.).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetReport {
    /// `"command"` | `"pid"` | `"comm"` | `"container"` | `"pod"`.
    pub kind: String,
    /// What the user asked for (e.g. `"claude"`, `"4213"`, `"my-agent-box"`).
    pub spec: Option<String>,
    /// Host pid of the process tree root, when known.
    pub host_pid: Option<i32>,
    /// AgentSight `--binary-path` form when applicable (`docker://…`, `k8s://…`).
    pub evidence_target: Option<String>,
    /// Optional human-readable note.
    pub note: Option<String>,
}

impl Default for TargetReport {
    fn default() -> Self {
        Self {
            kind: "command".into(),
            spec: None,
            host_pid: None,
            evidence_target: None,
            note: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Run summary
// ---------------------------------------------------------------------------

/// Aggregated counters for a completed (or in-progress) run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RunSummary {
    /// Total policy violations observed.
    pub violations: u64,
    /// Violations that blocked an operation.
    pub blocked: u64,
    /// Violations that killed a process.
    pub killed: u64,
    /// Processes observed.
    pub processes: u64,
    /// Files written.
    pub files_written: u64,
    /// Distinct network endpoints.
    pub endpoints: u64,
    /// LLM API calls observed.
    pub llm_calls: u64,
    /// Input tokens.
    pub tokens_in: u64,
    /// Output tokens.
    pub tokens_out: u64,
    /// Peak resident set size in bytes.
    pub peak_rss_bytes: u64,
    /// CPU time in seconds.
    pub cpu_seconds: f64,
    /// Wall-clock duration in seconds.
    pub duration_seconds: f64,
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// On-disk record of one run (`manifest.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Run id string.
    pub id: String,
    /// Manifest schema version (currently `1`).
    pub schema: u32,
    /// Start time (RFC 3339).
    pub started_at: String,
    /// End time (RFC 3339), if finished.
    pub ended_at: Option<String>,
    /// Command argv.
    pub argv: Vec<String>,
    /// Detected agent name (`claude` | `codex` | `command` | …).
    pub agent: String,
    /// Working directory at start.
    pub cwd: PathBuf,
    /// Profile name used.
    pub profile: String,
    /// What the planes attached to for this run.
    pub target: TargetReport,
    /// Which planes were active / degraded / disabled.
    pub planes: PlaneStatus,
    /// Component name → version.
    pub components: BTreeMap<String, String>,
    /// Aggregated counters.
    pub summary: RunSummary,
    /// Agent exit code, if finished.
    pub exit_code: Option<i32>,
    /// Akeep commit hash, if history ran.
    pub akeep_commit: Option<String>,
    /// Rules from the composed policy that this host's engine cannot enforce.
    ///
    /// Recorded so observe runs and reports stay honest when a pack needs
    /// ActPlane features the released engine does not enable on attach.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unenforceable_rules: Vec<RuleEnforceability>,
}

impl Manifest {
    /// Build a fresh manifest for a newly created run.
    pub fn new(id: &str, argv: &[String], cfg: &Config, cwd: PathBuf) -> Self {
        Self {
            id: id.to_string(),
            schema: 1,
            started_at: Local::now().to_rfc3339(),
            ended_at: None,
            argv: argv.to_vec(),
            agent: detect_agent(argv),
            cwd,
            profile: cfg.profile.clone(),
            target: TargetReport::default(),
            planes: PlaneStatus::default(),
            components: BTreeMap::new(),
            summary: RunSummary::default(),
            exit_code: None,
            akeep_commit: None,
            unenforceable_rules: Vec::new(),
        }
    }
}

/// Best-effort agent name from argv\[0\].
pub fn detect_agent(argv: &[String]) -> String {
    let Some(prog) = argv.first() else {
        return "command".into();
    };
    let name = Path::new(prog)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(prog);
    let lower = name.to_ascii_lowercase();
    // Strip common wrappers.
    let base = lower
        .strip_suffix(".js")
        .or_else(|| lower.strip_suffix(".py"))
        .unwrap_or(&lower);
    match base {
        "claude" | "claude-code" => "claude".into(),
        "codex" => "codex".into(),
        "gemini" | "gemini-cli" => "gemini".into(),
        "opencode" => "opencode".into(),
        "openclaw" => "openclaw".into(),
        _ => "command".into(),
    }
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

/// One recorded execution: directory + loaded manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    /// Run identifier.
    pub id: RunId,
    /// Absolute path to the run directory.
    pub dir: PathBuf,
    /// Manifest contents.
    pub manifest: Manifest,
}

impl Run {
    /// Persist the current manifest to `manifest.json`.
    pub fn save_manifest(&self) -> Result<()> {
        let path = self.dir.join("manifest.json");
        let json = serde_json::to_string_pretty(&self.manifest).context("serializing manifest")?;
        let mut f =
            fs::File::create(&path).with_context(|| format!("writing {}", path.display()))?;
        f.write_all(json.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        f.write_all(b"\n")?;
        Ok(())
    }

    /// Path to `violations.jsonl`.
    pub fn violations_path(&self) -> PathBuf {
        self.dir.join("violations.jsonl")
    }

    /// Path to `events.jsonl`.
    pub fn events_path(&self) -> PathBuf {
        self.dir.join("events.jsonl")
    }

    /// Path to `evidence.db`.
    pub fn evidence_db_path(&self) -> PathBuf {
        self.dir.join("evidence.db")
    }

    /// Path to `stdout.log`.
    pub fn stdout_path(&self) -> PathBuf {
        self.dir.join("stdout.log")
    }

    /// Path to `stderr.log`.
    pub fn stderr_path(&self) -> PathBuf {
        self.dir.join("stderr.log")
    }

    /// Path to `report.md`.
    pub fn report_path(&self) -> PathBuf {
        self.dir.join("report.md")
    }

    /// Path to effective `actime.yaml`.
    pub fn config_path(&self) -> PathBuf {
        self.dir.join("actime.yaml")
    }

    /// Path to the ActPlane project file (`policy.yaml`).
    ///
    /// ActPlane's `--policy` flag requires a YAML project file (with a
    /// `policy: |` block). A `.dsl` extension is rejected even when the
    /// contents are YAML, so the engine always loads this path.
    pub fn policy_path(&self) -> PathBuf {
        self.dir.join("policy.yaml")
    }

    /// Path to the composed pure ActPlane DSL (`policy.dsl`).
    ///
    /// Human-readable companion to [`Self::policy_path`]; not passed to the
    /// engine.
    pub fn policy_dsl_path(&self) -> PathBuf {
        self.dir.join("policy.dsl")
    }
}

// ---------------------------------------------------------------------------
// RunStore
// ---------------------------------------------------------------------------

/// On-disk store of all Actime runs under `$ACTIME_HOME/runs`.
#[derive(Debug, Clone)]
pub struct RunStore {
    root: PathBuf,
}

impl RunStore {
    /// Open the default store (`$ACTIME_HOME` or `~/.local/share/actime`).
    pub fn open_default() -> Result<RunStore> {
        let root = default_actime_home()?;
        RunStore::open(&root)
    }

    /// Open a store at an explicit root directory (creates `runs/` if needed).
    pub fn open(root: &Path) -> Result<RunStore> {
        let runs = root.join("runs");
        fs::create_dir_all(&runs)
            .with_context(|| format!("creating run store at {}", runs.display()))?;
        Ok(RunStore {
            root: root.to_path_buf(),
        })
    }

    /// Root directory (`$ACTIME_HOME`).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory holding individual runs.
    pub fn runs_dir(&self) -> PathBuf {
        self.root.join("runs")
    }

    /// Create a new run directory and write an initial `manifest.json`.
    pub fn create(&self, argv: &[String], cfg: &Config) -> Result<Run> {
        // Extremely unlikely collision; retry a few times.
        let mut attempts = 0u32;
        let mut id = RunId::generate();
        let dir = loop {
            let dir = self.runs_dir().join(id.as_str());
            if !dir.exists() {
                break dir;
            }
            attempts += 1;
            if attempts > 8 {
                bail!("failed to allocate unique run id after {attempts} attempts");
            }
            id = RunId::generate();
        };

        fs::create_dir_all(&dir)
            .with_context(|| format!("creating run directory {}", dir.display()))?;

        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let manifest = Manifest::new(id.as_str(), argv, cfg, cwd);
        let run = Run { id, dir, manifest };
        run.save_manifest()?;

        // Also write effective config snapshot.
        if let Ok(yaml) = cfg.to_yaml() {
            let _ = fs::write(run.config_path(), yaml);
        }

        Ok(run)
    }

    /// List all manifests, newest first (by run id / directory name).
    pub fn list(&self) -> Result<Vec<Manifest>> {
        let mut entries = Vec::new();
        let runs = self.runs_dir();
        if !runs.is_dir() {
            return Ok(entries);
        }
        let rd = fs::read_dir(&runs).with_context(|| format!("reading {}", runs.display()))?;
        for ent in rd {
            let ent = ent.with_context(|| format!("reading {}", runs.display()))?;
            let path = ent.path();
            if !path.is_dir() {
                continue;
            }
            let manifest_path = path.join("manifest.json");
            if !manifest_path.is_file() {
                continue;
            }
            match load_manifest(&manifest_path) {
                Ok(m) => entries.push(m),
                Err(_) => continue, // skip corrupt
            }
        }
        // Newest first: run ids sort lexicographically by time.
        entries.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(entries)
    }

    /// Load a run by id, or `"latest"` for the newest run.
    pub fn get(&self, id: &str) -> Result<Run> {
        let id = if id == "latest" {
            let list = self.list()?;
            let Some(first) = list.first() else {
                bail!("no runs found in {}", self.runs_dir().display());
            };
            first.id.clone()
        } else {
            id.to_string()
        };

        let dir = self.runs_dir().join(&id);
        if !dir.is_dir() {
            bail!("run `{id}` not found in {}", self.runs_dir().display());
        }
        let manifest = load_manifest(&dir.join("manifest.json"))?;
        Ok(Run {
            id: RunId(id),
            dir,
            manifest,
        })
    }

    /// Keep the newest `keep` runs; delete older ones. Returns number deleted.
    pub fn prune(&self, keep: usize) -> Result<usize> {
        let list = self.list()?;
        if list.len() <= keep {
            return Ok(0);
        }
        let mut deleted = 0;
        for m in list.into_iter().skip(keep) {
            let dir = self.runs_dir().join(&m.id);
            if dir.is_dir() {
                fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
                deleted += 1;
            }
        }
        Ok(deleted)
    }
}

fn load_manifest(path: &Path) -> Result<Manifest> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let m: Manifest =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(m)
}

/// Resolve `$ACTIME_HOME` or `~/.local/share/actime`.
pub fn default_actime_home() -> Result<PathBuf> {
    if let Some(h) = std::env::var_os("ACTIME_HOME") {
        return Ok(PathBuf::from(h));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow::anyhow!("HOME is not set; cannot resolve actime home"))?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("actime"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn run_id_format() {
        let id = RunId::generate();
        assert!(
            is_valid_run_id(id.as_str()),
            "generated id `{}` invalid",
            id.as_str()
        );
        assert_eq!(id.as_str().len(), 20);
        let parsed = RunId::parse(id.as_str()).unwrap();
        assert_eq!(parsed, id);
        assert!(RunId::parse("not-a-valid-id").is_err());
        assert!(RunId::parse("20260804-153012-a3f1").is_ok());
    }

    #[test]
    fn manifest_serde_roundtrip() {
        let cfg = Config::builtin_profile("balanced").unwrap();
        let m = Manifest::new(
            "20260804-153012-a3f1",
            &["claude".into(), "--help".into()],
            &cfg,
            PathBuf::from("/tmp/work"),
        );
        assert_eq!(m.agent, "claude");
        assert_eq!(m.schema, 1);
        let json = serde_json::to_string_pretty(&m).unwrap();
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, m.id);
        assert_eq!(back.argv, m.argv);
        assert_eq!(back.agent, "claude");
        assert_eq!(back.profile, "balanced");
    }

    #[test]
    fn plane_state_serde() {
        let s = PlaneState::Degraded("no CAP_BPF".into());
        let json = serde_json::to_string(&s).unwrap();
        let back: PlaneState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        assert_eq!(back.label(), "Degraded");
    }

    #[test]
    fn run_store_create_get_latest_list_prune() {
        let tmp = TempDir::new().unwrap();
        let store = RunStore::open(tmp.path()).unwrap();
        let cfg = Config::builtin_profile("observe").unwrap();

        let r1 = store.create(&["echo".into(), "hi".into()], &cfg).unwrap();
        assert!(r1.dir.join("manifest.json").is_file());
        // `create` snapshots the effective config alongside the manifest, so a
        // run directory is self-describing even if the process dies next.
        assert!(r1.config_path().is_file());
        assert_eq!(r1.config_path().parent(), Some(r1.dir.as_path()));
        let snapshot = std::fs::read_to_string(r1.config_path()).unwrap();
        assert!(snapshot.contains("observe"));
        assert_eq!(r1.manifest.agent, "command");

        // Ensure distinct timestamps if needed.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let r2 = store.create(&["claude".into()], &cfg).unwrap();
        assert_ne!(r1.id, r2.id);

        let latest = store.get("latest").unwrap();
        // Newest id should be >= both (lexicographic by time).
        assert!(latest.id.as_str() >= r1.id.as_str());
        assert!(latest.id.as_str() >= r2.id.as_str());

        let list = store.list().unwrap();
        assert_eq!(list.len(), 2);

        let got = store.get(r1.id.as_str()).unwrap();
        assert_eq!(got.id, r1.id);
        assert_eq!(got.manifest.argv, vec!["echo", "hi"]);

        // Path helpers.
        assert!(got.violations_path().ends_with("violations.jsonl"));
        assert!(got.events_path().ends_with("events.jsonl"));
        assert!(got.evidence_db_path().ends_with("evidence.db"));
        assert!(got.stdout_path().ends_with("stdout.log"));
        assert!(got.stderr_path().ends_with("stderr.log"));
        assert!(got.report_path().ends_with("report.md"));
        assert!(got.policy_path().ends_with("policy.yaml"));
        assert!(got.policy_dsl_path().ends_with("policy.dsl"));

        let deleted = store.prune(1).unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn detect_agent_names() {
        assert_eq!(detect_agent(&["claude".into()]), "claude");
        assert_eq!(detect_agent(&["/usr/bin/codex".into()]), "codex");
        assert_eq!(detect_agent(&["gemini".into()]), "gemini");
        assert_eq!(detect_agent(&["my-tool".into()]), "command");
        assert_eq!(detect_agent(&[]), "command");
    }

    #[test]
    fn save_manifest_rewrites() {
        let tmp = TempDir::new().unwrap();
        let store = RunStore::open(tmp.path()).unwrap();
        let cfg = Config::default();
        let mut run = store.create(&["true".into()], &cfg).unwrap();
        run.manifest.exit_code = Some(0);
        run.manifest.summary.violations = 3;
        run.save_manifest().unwrap();
        let reloaded = store.get(run.id.as_str()).unwrap();
        assert_eq!(reloaded.manifest.exit_code, Some(0));
        assert_eq!(reloaded.manifest.summary.violations, 3);
    }
}
