//! Configuration loading, profiles, and CLI overrides for Actime.
//!
//! Resolution order (see `docs/DESIGN.md` §4):
//!
//! 1. Explicit `--config <FILE>`
//! 2. `actime.yaml` walking from `start_dir` up to the git root
//! 3. `~/.config/actime/actime.yaml`
//! 4. Built-in `balanced` profile
//!
//! Every field is optional in YAML (`#[serde(default)]`). A named profile is
//! loaded first; the on-disk file is then layered on top.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

// ---------------------------------------------------------------------------
// Duration helpers ("2h" / "90m" / "30s")
// ---------------------------------------------------------------------------

/// Parse a human duration string into [`Duration`].
///
/// Accepts forms like `30s`, `90m`, `2h`, `1d`, plain integer seconds (`"60"`),
/// and optional fractional seconds (`"1.5s"`). Units are case-insensitive.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty duration string");
    }

    let bytes = s.as_bytes();
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let start_digits = i;
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
        i += 1;
    }
    if i == start_digits {
        bail!("duration `{s}` does not start with a number");
    }
    if s.starts_with('-') {
        bail!("duration must be non-negative, got `{s}`");
    }
    let num_str = &s[start_digits..i];
    let value: f64 = num_str
        .parse()
        .with_context(|| format!("invalid duration number in `{s}`"))?;
    if value < 0.0 || !value.is_finite() {
        bail!("duration must be a non-negative finite number, got `{s}`");
    }

    let unit = s[i..].trim().to_ascii_lowercase();
    let secs = match unit.as_str() {
        "" | "s" | "sec" | "secs" | "second" | "seconds" => value,
        "m" | "min" | "mins" | "minute" | "minutes" => value * 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => value * 3600.0,
        "d" | "day" | "days" => value * 86400.0,
        other => bail!("unknown duration unit `{other}` in `{s}` (expected s|m|h|d)"),
    };

    Ok(Duration::from_secs_f64(secs))
}

/// Format a [`Duration`] back to a compact human string (preferring h/m/s).
pub fn format_duration(d: &Duration) -> String {
    let secs = d.as_secs();
    if secs > 0 && secs % 86400 == 0 {
        return format!("{}d", secs / 86400);
    }
    if secs > 0 && secs % 3600 == 0 {
        return format!("{}h", secs / 3600);
    }
    if secs > 0 && secs % 60 == 0 {
        return format!("{}m", secs / 60);
    }
    if d.subsec_nanos() == 0 {
        return format!("{secs}s");
    }
    let total = d.as_secs_f64();
    let s = format!("{total:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s}s")
}

fn deserialize_opt_duration<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    match opt {
        None => Ok(None),
        Some(s) if s.trim().is_empty() => Ok(None),
        Some(s) => parse_duration(&s)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

fn serialize_opt_duration<S>(d: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match d {
        None => serializer.serialize_none(),
        Some(dur) => serializer.serialize_str(&format_duration(dur)),
    }
}

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Policy plane mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PolicyMode {
    /// Policy plane off.
    Off,
    /// Record violations but never block.
    Observe,
    /// Enforce policy (default for balanced/strict).
    #[default]
    Enforce,
}

impl fmt::Display for PolicyMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PolicyMode::Off => write!(f, "off"),
            PolicyMode::Observe => write!(f, "observe"),
            PolicyMode::Enforce => write!(f, "enforce"),
        }
    }
}

impl FromStr for PolicyMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(PolicyMode::Off),
            "observe" => Ok(PolicyMode::Observe),
            "enforce" => Ok(PolicyMode::Enforce),
            other => bail!("unknown policy mode `{other}` (expected off|observe|enforce)"),
        }
    }
}

// ---------------------------------------------------------------------------
// Config structs
// ---------------------------------------------------------------------------

/// Fully resolved Actime configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Schema version (currently `1`).
    #[serde(default = "default_version")]
    pub version: u32,
    /// Named profile this config is based on (`observe` | `balanced` | `strict`).
    #[serde(default = "default_profile")]
    pub profile: String,
    /// Policy plane settings.
    #[serde(default)]
    pub policy: PolicyConfig,
    /// Evidence plane settings.
    #[serde(default)]
    pub evidence: EvidenceConfig,
    /// History plane settings.
    #[serde(default)]
    pub history: HistoryConfig,
    /// Run limits.
    #[serde(default)]
    pub limits: LimitsConfig,
}

fn default_version() -> u32 {
    1
}

fn default_profile() -> String {
    "balanced".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self::builtin_profile("balanced").unwrap_or_else(|_| Config {
            version: 1,
            profile: "balanced".into(),
            policy: PolicyConfig::default(),
            evidence: EvidenceConfig::default(),
            history: HistoryConfig::default(),
            limits: LimitsConfig::default(),
        })
    }
}

/// Policy plane settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyConfig {
    /// `off` | `observe` | `enforce`.
    #[serde(default)]
    pub mode: PolicyMode,
    /// Built-in policy pack names from `policies/`.
    #[serde(default = "default_policy_packs")]
    pub packs: Vec<String>,
    /// Extra ActPlane policy file paths.
    #[serde(default)]
    pub files: Vec<String>,
    /// Inject corrective feedback the agent can read.
    #[serde(default = "default_true")]
    pub feedback: bool,
}

fn default_policy_packs() -> Vec<String> {
    vec!["coding-agent-baseline".into()]
}

fn default_true() -> bool {
    true
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            mode: PolicyMode::Enforce,
            packs: default_policy_packs(),
            files: Vec::new(),
            feedback: true,
        }
    }
}

/// Evidence plane settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceConfig {
    /// Whether the evidence plane is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Capture categories: process, file, network, ssl, resource.
    #[serde(default = "default_capture")]
    pub capture: Vec<String>,
    /// Export targets: otlp | sqlite | json.
    #[serde(default)]
    pub export: Vec<String>,
    /// Strip auth headers and secret-shaped values.
    #[serde(default = "default_true")]
    pub redact: bool,
}

fn default_capture() -> Vec<String> {
    vec![
        "process".into(),
        "file".into(),
        "network".into(),
        "ssl".into(),
        "resource".into(),
    ]
}

impl Default for EvidenceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            capture: default_capture(),
            export: Vec::new(),
            redact: true,
        }
    }
}

/// History plane (Akeep) settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryConfig {
    /// Whether the history plane is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Commit on run exit.
    #[serde(default = "default_true")]
    pub commit_on_exit: bool,
    /// Optional commit message; default is `"actime run <id>"`.
    #[serde(default)]
    pub message: Option<String>,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            commit_on_exit: true,
            message: None,
        }
    }
}

/// Run resource / time limits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct LimitsConfig {
    /// Kill the run after this wall-clock duration (`"2h"`, `"90m"`, …).
    #[serde(
        default,
        deserialize_with = "deserialize_opt_duration",
        serialize_with = "serialize_opt_duration"
    )]
    pub wall_clock: Option<Duration>,
}

// ---------------------------------------------------------------------------
// CLI overrides
// ---------------------------------------------------------------------------

/// Optional flags from the CLI layered onto a loaded [`Config`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliOverrides {
    /// `--policy <mode>`.
    pub policy_mode: Option<PolicyMode>,
    /// `--profile <name>`.
    pub profile: Option<String>,
    /// `--no-evidence`.
    pub no_evidence: Option<bool>,
    /// `--no-history`.
    pub no_history: Option<bool>,
}

// ---------------------------------------------------------------------------
// Built-in profiles (inline YAML — do not include_str! from profiles/)
// ---------------------------------------------------------------------------

const PROFILE_OBSERVE: &str = r#"
version: 1
profile: observe
policy:
  mode: observe
  packs:
    - coding-agent-baseline
  feedback: false
evidence:
  enabled: true
  capture: [process, file, network, ssl, resource]
  redact: true
history:
  enabled: true
  commit_on_exit: true
"#;

const PROFILE_BALANCED: &str = r#"
version: 1
profile: balanced
policy:
  mode: enforce
  packs:
    - coding-agent-baseline
  feedback: true
evidence:
  enabled: true
  capture: [process, file, network, ssl, resource]
  redact: true
history:
  enabled: true
  commit_on_exit: true
"#;

const PROFILE_STRICT: &str = r#"
version: 1
profile: strict
policy:
  mode: enforce
  packs:
    - coding-agent-baseline
    - no-vcs-write
    - no-secret-egress
  feedback: true
evidence:
  enabled: true
  capture: [process, file, network, ssl, resource]
  export: [otlp]
  redact: true
history:
  enabled: true
  commit_on_exit: true
limits:
  wall_clock: 4h
"#;

// ---------------------------------------------------------------------------
// Config impl
// ---------------------------------------------------------------------------

impl Config {
    /// Load configuration.
    ///
    /// If `explicit` is set, that file is used (and must exist). Otherwise
    /// walks from `start_dir` up to the git root for `actime.yaml`, then
    /// tries `~/.config/actime/actime.yaml`, else the built-in `balanced`
    /// profile.
    ///
    /// When an on-disk file names a `profile`, the built-in profile is loaded
    /// first and the file is merged on top.
    pub fn load(explicit: Option<&Path>, start_dir: &Path) -> Result<Config> {
        let file_path = if let Some(p) = explicit {
            if !p.is_file() {
                bail!("config file not found: {}", p.display());
            }
            Some(p.to_path_buf())
        } else {
            find_actime_yaml(start_dir).or_else(user_config_path)
        };

        match file_path {
            Some(path) => {
                let text = fs::read_to_string(&path)
                    .with_context(|| format!("reading config {}", path.display()))?;
                let partial: PartialConfig = serde_yaml::from_str(&text)
                    .with_context(|| format!("parsing config {}", path.display()))?;

                let profile_name = partial
                    .profile
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("balanced");

                let mut cfg = if is_builtin_profile(profile_name) {
                    Config::builtin_profile(profile_name)?
                } else if Path::new(profile_name).is_file() {
                    let ptext = fs::read_to_string(profile_name)
                        .with_context(|| format!("reading profile {profile_name}"))?;
                    serde_yaml::from_str(&ptext)
                        .with_context(|| format!("parsing profile {profile_name}"))?
                } else {
                    let mut c = Config::builtin_profile("balanced")?;
                    c.profile = profile_name.to_string();
                    c
                };

                merge_partial(&mut cfg, partial);
                Ok(cfg)
            }
            None => Config::builtin_profile("balanced"),
        }
    }

    /// Return a built-in profile by name: `observe`, `balanced`, or `strict`.
    pub fn builtin_profile(name: &str) -> Result<Config> {
        let yaml = match name.trim().to_ascii_lowercase().as_str() {
            "observe" => PROFILE_OBSERVE,
            "balanced" => PROFILE_BALANCED,
            "strict" => PROFILE_STRICT,
            other => {
                bail!("unknown built-in profile `{other}` (expected observe|balanced|strict)")
            }
        };
        let cfg: Config = serde_yaml::from_str(yaml)
            .with_context(|| format!("parsing built-in profile `{name}`"))?;
        Ok(cfg)
    }

    /// Apply CLI overrides in place.
    pub fn merge_cli(&mut self, overrides: &CliOverrides) {
        if let Some(mode) = overrides.policy_mode {
            self.policy.mode = mode;
        }
        if let Some(ref profile) = overrides.profile {
            self.profile = profile.clone();
        }
        if overrides.no_evidence == Some(true) {
            self.evidence.enabled = false;
        }
        if overrides.no_history == Some(true) {
            self.history.enabled = false;
        }
    }

    /// Serialize this config to YAML.
    pub fn to_yaml(&self) -> Result<String> {
        serde_yaml::to_string(self).context("serializing config to YAML")
    }
}

fn is_builtin_profile(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "observe" | "balanced" | "strict"
    )
}

/// Walk from `start` upward looking for `actime.yaml`, stopping at the git root
/// (directory containing `.git`) or the filesystem root.
fn find_actime_yaml(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_file() {
        start.parent().unwrap_or(start).to_path_buf()
    } else {
        start.to_path_buf()
    };

    loop {
        let candidate = dir.join("actime.yaml");
        if candidate.is_file() {
            return Some(candidate);
        }
        let is_git_root = dir.join(".git").exists();
        if is_git_root {
            return None;
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent.to_path_buf(),
            _ => return None,
        }
    }
}

fn user_config_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let p = PathBuf::from(home)
        .join(".config")
        .join("actime")
        .join("actime.yaml");
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Partial merge (for layering a file over a profile)
// ---------------------------------------------------------------------------

/// All-optional mirror of [`Config`] for merge-on-top deserialization.
#[derive(Debug, Default, Deserialize)]
struct PartialConfig {
    version: Option<u32>,
    profile: Option<String>,
    policy: Option<PartialPolicy>,
    evidence: Option<PartialEvidence>,
    history: Option<PartialHistory>,
    limits: Option<PartialLimits>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialPolicy {
    mode: Option<PolicyMode>,
    packs: Option<Vec<String>>,
    files: Option<Vec<String>>,
    feedback: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialEvidence {
    enabled: Option<bool>,
    capture: Option<Vec<String>>,
    export: Option<Vec<String>>,
    redact: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialHistory {
    enabled: Option<bool>,
    commit_on_exit: Option<bool>,
    message: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialLimits {
    #[serde(default, deserialize_with = "deserialize_opt_duration")]
    wall_clock: Option<Duration>,
}

fn merge_partial(cfg: &mut Config, p: PartialConfig) {
    if let Some(v) = p.version {
        cfg.version = v;
    }
    if let Some(profile) = p.profile {
        cfg.profile = profile;
    }
    if let Some(pol) = p.policy {
        if let Some(v) = pol.mode {
            cfg.policy.mode = v;
        }
        if let Some(v) = pol.packs {
            cfg.policy.packs = v;
        }
        if let Some(v) = pol.files {
            cfg.policy.files = v;
        }
        if let Some(v) = pol.feedback {
            cfg.policy.feedback = v;
        }
    }
    if let Some(ev) = p.evidence {
        if let Some(v) = ev.enabled {
            cfg.evidence.enabled = v;
        }
        if let Some(v) = ev.capture {
            cfg.evidence.capture = v;
        }
        if let Some(v) = ev.export {
            cfg.evidence.export = v;
        }
        if let Some(v) = ev.redact {
            cfg.evidence.redact = v;
        }
    }
    if let Some(h) = p.history {
        if let Some(v) = h.enabled {
            cfg.history.enabled = v;
        }
        if let Some(v) = h.commit_on_exit {
            cfg.history.commit_on_exit = v;
        }
        if h.message.is_some() {
            cfg.history.message = h.message;
        }
    }
    if let Some(lim) = p.limits {
        if lim.wall_clock.is_some() {
            cfg.limits.wall_clock = lim.wall_clock;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn duration_parse_units() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("90m").unwrap(), Duration::from_secs(90 * 60));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(2 * 3600));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86400));
        assert_eq!(parse_duration("60").unwrap(), Duration::from_secs(60));
        assert_eq!(
            parse_duration("1.5s").unwrap(),
            Duration::from_secs_f64(1.5)
        );
    }

    #[test]
    fn duration_parse_errors() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("-5s").is_err());
        assert!(parse_duration("2x").is_err());
    }

    #[test]
    fn duration_format_roundtrip_ish() {
        assert_eq!(format_duration(&Duration::from_secs(7200)), "2h");
        assert_eq!(format_duration(&Duration::from_secs(90 * 60)), "90m");
        assert_eq!(format_duration(&Duration::from_secs(30)), "30s");
    }

    #[test]
    fn builtin_profiles() {
        let obs = Config::builtin_profile("observe").unwrap();
        assert_eq!(obs.profile, "observe");
        assert_eq!(obs.policy.mode, PolicyMode::Observe);
        assert!(!obs.policy.feedback);
        assert!(obs.evidence.enabled);

        let bal = Config::builtin_profile("balanced").unwrap();
        assert_eq!(bal.profile, "balanced");
        assert_eq!(bal.policy.mode, PolicyMode::Enforce);
        assert_eq!(bal.policy.packs, vec!["coding-agent-baseline"]);

        let strict = Config::builtin_profile("strict").unwrap();
        assert_eq!(strict.profile, "strict");
        assert!(strict.policy.packs.contains(&"no-vcs-write".to_string()));
        assert_eq!(
            strict.limits.wall_clock,
            Some(Duration::from_secs(4 * 3600))
        );
        assert!(strict.evidence.export.contains(&"otlp".to_string()));
    }

    #[test]
    fn profile_yaml_roundtrip() {
        for name in ["observe", "balanced", "strict"] {
            let cfg = Config::builtin_profile(name).unwrap();
            let yaml = cfg.to_yaml().unwrap();
            let back: Config = serde_yaml::from_str(&yaml).unwrap();
            assert_eq!(back.profile, cfg.profile);
            assert_eq!(back.policy.mode, cfg.policy.mode);
            assert_eq!(back.limits.wall_clock, cfg.limits.wall_clock);
        }
    }

    #[test]
    fn merge_cli_overrides() {
        let mut cfg = Config::builtin_profile("balanced").unwrap();
        let ov = CliOverrides {
            policy_mode: Some(PolicyMode::Observe),
            profile: Some("custom".into()),
            no_evidence: Some(true),
            no_history: Some(true),
        };
        cfg.merge_cli(&ov);
        assert_eq!(cfg.policy.mode, PolicyMode::Observe);
        assert_eq!(cfg.profile, "custom");
        assert!(!cfg.evidence.enabled);
        assert!(!cfg.history.enabled);
    }

    #[test]
    fn load_explicit_file_merges_profile() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("actime.yaml");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "profile: observe\npolicy:\n  mode: off\n").unwrap();

        let cfg = Config::load(Some(&path), dir.path()).unwrap();
        assert_eq!(cfg.profile, "observe");
        assert_eq!(cfg.policy.mode, PolicyMode::Off);
        assert!(!cfg.policy.feedback);
        assert!(cfg.evidence.enabled);
    }

    #[test]
    fn load_missing_falls_back_to_balanced() {
        let dir = TempDir::new().unwrap();
        let cfg = Config::load(None, dir.path()).unwrap();
        assert_eq!(cfg.profile, "balanced");
        assert_eq!(cfg.policy.mode, PolicyMode::Enforce);
    }

    #[test]
    fn empty_yaml_uses_defaults() {
        let cfg: Config = serde_yaml::from_str("{}").unwrap();
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.profile, "balanced");
        assert_eq!(cfg.policy.mode, PolicyMode::Enforce);
    }

    #[test]
    fn policy_mode_parse() {
        assert_eq!(
            PolicyMode::from_str("ENFORCE").unwrap(),
            PolicyMode::Enforce
        );
        assert!(PolicyMode::from_str("weird").is_err());
    }
}
