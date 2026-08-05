//! Environment and component health checks for `actime doctor`.
//!
//! Every non-Ok [`Check`] carries an actionable [`Check::fix`] string.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::components::{compare_semver, Components};
use crate::config::Config;
use crate::run::default_actime_home;

/// Outcome of a single doctor check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    /// Requirement met.
    Ok,
    /// Soft issue; Actime can run with degradation.
    Warn,
    /// Hard problem for some planes.
    Fail,
    /// Check not applicable on this platform.
    Skip,
}

impl CheckStatus {
    /// Lowercase label for display / JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            CheckStatus::Ok => "ok",
            CheckStatus::Warn => "warn",
            CheckStatus::Fail => "fail",
            CheckStatus::Skip => "skip",
        }
    }
}

/// One doctor check result.
///
/// Serializable because `actime doctor --json` is the thing bug reports are
/// asked to attach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    /// Short check name.
    pub name: String,
    /// Pass / warn / fail / skip.
    pub status: CheckStatus,
    /// Human-readable detail.
    pub detail: String,
    /// Actionable remediation when status is not Ok (or sometimes Warn).
    pub fix: Option<String>,
}

impl Check {
    /// Construct an Ok check.
    pub fn ok(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Ok,
            detail: detail.into(),
            fix: None,
        }
    }

    /// Construct a Warn check with fix.
    pub fn warn(
        name: impl Into<String>,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Warn,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }

    /// Construct a Fail check with fix.
    pub fn fail(
        name: impl Into<String>,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Fail,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }

    /// Construct a Skip check.
    pub fn skip(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Skip,
            detail: detail.into(),
            fix: None,
        }
    }
}

/// Run all doctor checks against the current environment and `cfg`.
///
/// Checks (DESIGN.md §5 / §8):
/// - OS is Linux
/// - Kernel ≥ 5.10 for policy; note 6.1+ for full runtime
/// - BTF at `/sys/kernel/btf/vmlinux`
/// - Root or `CAP_BPF`
/// - Each component present and ≥ min version
/// - Run store writable
/// - Config file found and parseable (when present)
pub fn run_checks(cfg: &Config) -> Vec<Check> {
    let components = Components::detect();
    vec![
        check_os(),
        check_kernel(),
        check_btf(),
        check_cap_bpf(),
        check_component(&components.actplane),
        check_component(&components.agentsight),
        check_component(&components.akeep),
        check_run_store(),
        check_config(cfg),
    ]
}

fn check_os() -> Check {
    #[cfg(target_os = "linux")]
    {
        Check::ok("os", "Linux")
    }
    #[cfg(not(target_os = "linux"))]
    {
        Check::fail(
            "os",
            format!(
                "unsupported OS `{}` (Actime requires Linux)",
                std::env::consts::OS
            ),
            "Run Actime on a Linux host or VM (kernel ≥ 5.10, ideally ≥ 6.1)",
        )
    }
}

fn check_kernel() -> Check {
    let release = read_osrelease().unwrap_or_else(|| "unknown".into());
    let Some((maj, min, _pat)) = parse_kernel_version(&release) else {
        return Check::warn(
            "kernel",
            format!("could not parse kernel version from `{release}`"),
            "Ensure /proc/sys/kernel/osrelease is readable; need Linux ≥ 5.10 for policy",
        );
    };

    if maj < 5 || (maj == 5 && min < 10) {
        return Check::fail(
            "kernel",
            format!("kernel {release} is below 5.10; policy plane requires ≥ 5.10"),
            "Upgrade the kernel to ≥ 5.10 (6.1+ recommended for full eBPF runtime)",
        );
    }

    if maj < 6 || (maj == 6 && min < 1) {
        return Check::warn(
            "kernel",
            format!("kernel {release} supports policy (≥ 5.10); 6.1+ recommended for full runtime"),
            "Consider upgrading to Linux 6.1 or newer for the complete eBPF feature set",
        );
    }

    Check::ok("kernel", format!("kernel {release} (≥ 6.1)"))
}

fn check_btf() -> Check {
    let path = PathBuf::from("/sys/kernel/btf/vmlinux");
    if path.is_file() {
        Check::ok("btf", format!("{} present", path.display()))
    } else {
        Check::warn(
            "btf",
            format!("{} missing; CO-RE eBPF programs may fail", path.display()),
            "Enable CONFIG_DEBUG_INFO_BTF=y and boot a kernel that exposes /sys/kernel/btf/vmlinux",
        )
    }
}

fn check_cap_bpf() -> Check {
    if is_root() {
        return Check::ok("cap_bpf", "running as root");
    }
    if has_cap_bpf() {
        return Check::ok("cap_bpf", "CAP_BPF is set in CapEff");
    }
    Check::warn(
        "cap_bpf",
        "neither root nor CAP_BPF; policy and evidence planes will disable",
        "Run as root, or grant CAP_BPF (e.g. `sudo setcap cap_bpf,cap_perfmon+ep $(which actplane)`), \
         or use `actime run` knowing those planes will degrade (see DESIGN.md §8)",
    )
}

fn check_component(c: &crate::components::Component) -> Check {
    let name = c.name;
    match (&c.path, &c.version) {
        (None, _) => Check::warn(
            name,
            format!("{name} not found on PATH, ~/.local/share/actime/bin, or ~/.cargo/bin"),
            Components::install_hint(name),
        ),
        (Some(path), Some(ver)) => {
            if compare_semver(ver, c.min_version) >= 0 {
                Check::ok(
                    name,
                    format!("{name} {ver} at {} (≥ {})", path.display(), c.min_version),
                )
            } else {
                Check::warn(
                    name,
                    format!(
                        "{name} {ver} at {} is below minimum {}",
                        path.display(),
                        c.min_version
                    ),
                    format!(
                        "{} and ensure version ≥ {}",
                        Components::install_hint(name),
                        c.min_version
                    ),
                )
            }
        }
        (Some(path), None) => Check::warn(
            name,
            format!(
                "{name} found at {} but version could not be parsed",
                path.display()
            ),
            format!(
                "Run `{} --version` manually; {}",
                path.display(),
                Components::install_hint(name)
            ),
        ),
    }
}

fn check_run_store() -> Check {
    let home = match default_actime_home() {
        Ok(h) => h,
        Err(e) => {
            return Check::fail(
                "run_store",
                format!("cannot resolve actime home: {e}"),
                "Set HOME or ACTIME_HOME to a writable directory",
            );
        }
    };
    let runs = home.join("runs");
    if let Err(e) = fs::create_dir_all(&runs) {
        return Check::fail(
            "run_store",
            format!("cannot create {}: {e}", runs.display()),
            format!("Ensure {} is writable, or set ACTIME_HOME", home.display()),
        );
    }
    // Probe writability with a temp file.
    let probe = runs.join(".actime-doctor-write-probe");
    match fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            Check::ok("run_store", format!("writable at {}", home.display()))
        }
        Err(e) => Check::fail(
            "run_store",
            format!("cannot write to {}: {e}", runs.display()),
            format!(
                "chmod/chown {} or set ACTIME_HOME elsewhere",
                runs.display()
            ),
        ),
    }
}

fn check_config(cfg: &Config) -> Check {
    // Config was already loaded by the caller; report profile + key settings.
    Check::ok(
        "config",
        format!(
            "profile={} policy={} sandbox.backend={} evidence={} history={}",
            cfg.profile,
            cfg.policy.mode,
            cfg.sandbox.backend,
            if cfg.evidence.enabled { "on" } else { "off" },
            if cfg.history.enabled { "on" } else { "off" },
        ),
    )
}

// ---------------------------------------------------------------------------
// Platform helpers
// ---------------------------------------------------------------------------

fn read_osrelease() -> Option<String> {
    fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|s| s.trim().to_string())
}

/// Parse `5.15.0-91-generic` → (5, 15, 0).
fn parse_kernel_version(s: &str) -> Option<(u64, u64, u64)> {
    let mut parts = s.split(|c: char| !c.is_ascii_digit() && c != '.');
    let ver = parts.next().filter(|p| !p.is_empty())?;
    let mut nums = ver.split('.');
    let maj: u64 = nums.next()?.parse().ok()?;
    let min: u64 = nums.next().unwrap_or("0").parse().ok()?;
    let pat: u64 = nums.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    Some((maj, min, pat))
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// CAP_BPF is capability 39 on Linux.
const CAP_BPF: u32 = 39;

fn has_cap_bpf() -> bool {
    // Read CapEff from /proc/self/status (hex bitmask).
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return false;
    };
    for line in status.lines() {
        if let Some(hex) = line.strip_prefix("CapEff:") {
            let hex = hex.trim();
            if let Ok(mask) = u64::from_str_radix(hex, 16) {
                return (mask & (1u64 << CAP_BPF)) != 0;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kernel_versions() {
        assert_eq!(parse_kernel_version("5.10.0"), Some((5, 10, 0)));
        assert_eq!(parse_kernel_version("5.15.0-91-generic"), Some((5, 15, 0)));
        assert_eq!(parse_kernel_version("6.1.0"), Some((6, 1, 0)));
        assert_eq!(parse_kernel_version("6.8.0-40-generic"), Some((6, 8, 0)));
        assert_eq!(parse_kernel_version("4.19.0"), Some((4, 19, 0)));
    }

    #[test]
    fn run_checks_returns_expected_names() {
        let cfg = Config::builtin_profile("balanced").unwrap();
        let checks = run_checks(&cfg);
        let names: Vec<_> = checks.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"os"));
        assert!(names.contains(&"kernel"));
        assert!(names.contains(&"btf"));
        assert!(names.contains(&"cap_bpf"));
        assert!(names.contains(&"actplane"));
        assert!(names.contains(&"agentsight"));
        assert!(names.contains(&"akeep"));
        assert!(names.contains(&"run_store"));
        assert!(names.contains(&"config"));
        // Non-Ok checks should carry a fix.
        for c in &checks {
            if c.status != CheckStatus::Ok && c.status != CheckStatus::Skip {
                assert!(
                    c.fix.is_some(),
                    "check `{}` status {:?} missing fix",
                    c.name,
                    c.status
                );
            }
        }
    }

    #[test]
    fn check_constructors() {
        let ok = Check::ok("x", "fine");
        assert_eq!(ok.status, CheckStatus::Ok);
        assert!(ok.fix.is_none());
        let w = Check::warn("y", "soft", "do this");
        assert_eq!(w.status, CheckStatus::Warn);
        assert_eq!(w.fix.as_deref(), Some("do this"));
    }
}
