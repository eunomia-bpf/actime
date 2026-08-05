//! Sandbox specification types shared by every backend.
//!
//! [`NetworkMode`] is defined here (not in `actime-core`) so this crate is
//! self-contained. Config loaders may re-export or map into these types.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::backend::Backend;

/// Network policy applied inside the sandbox.
///
/// Authoritative egress filtering for [`NetworkMode::Egress`] is performed by
/// ActPlane `connect` rules on the host; the sandbox backend only applies a
/// best-effort container/network default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    /// Full network access (default bridge / host network as appropriate).
    Allow,
    /// No network (`--network none` / `--unshare-net`).
    Deny,
    /// Best-effort restricted egress; ActPlane owns the real allowlist.
    Egress,
}

impl fmt::Display for NetworkMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetworkMode::Allow => write!(f, "allow"),
            NetworkMode::Deny => write!(f, "deny"),
            NetworkMode::Egress => write!(f, "egress"),
        }
    }
}

impl FromStr for NetworkMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "allow" => Ok(NetworkMode::Allow),
            "deny" => Ok(NetworkMode::Deny),
            "egress" => Ok(NetworkMode::Egress),
            other => bail!("unknown network mode `{other}` (expected allow|deny|egress)"),
        }
    }
}

/// A bind mount from the host into the sandbox guest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    /// Absolute (or resolved) path on the host.
    pub host: PathBuf,
    /// Mount point inside the sandbox.
    pub guest: PathBuf,
    /// When true, the mount is read-only inside the guest.
    pub readonly: bool,
}

impl Mount {
    /// Build a mount from host path, guest path, and read-only flag.
    pub fn new(host: impl Into<PathBuf>, guest: impl Into<PathBuf>, readonly: bool) -> Self {
        Self {
            host: host.into(),
            guest: guest.into(),
            readonly,
        }
    }

    /// Parse a mount specification of the form `host:guest[:ro|rw]`.
    ///
    /// Examples:
    /// - `.:/workspace:rw`
    /// - `/data:/data:ro`
    /// - `/tmp/work:/workspace` (read-write by default)
    ///
    /// Host and guest paths must not be empty. Mode is case-insensitive and
    /// accepts `ro` / `readonly` for read-only and `rw` / `readwrite` for
    /// read-write. Unknown modes are an error.
    pub fn parse(spec: &str) -> Result<Self> {
        let spec = spec.trim();
        if spec.is_empty() {
            bail!("empty mount specification");
        }

        // Split into at most three fields on `:`. Paths on Unix rarely contain
        // colons; this matches the actime.yaml `host:container:mode` form.
        let parts: Vec<&str> = spec.splitn(3, ':').collect();
        match parts.as_slice() {
            [host, guest] => {
                if host.is_empty() || guest.is_empty() {
                    bail!("mount `{spec}` has empty host or guest path");
                }
                Ok(Mount::new(*host, *guest, false))
            }
            [host, guest, mode] => {
                if host.is_empty() || guest.is_empty() {
                    bail!("mount `{spec}` has empty host or guest path");
                }
                let readonly = parse_mount_mode(mode)
                    .with_context(|| format!("invalid mount mode in `{spec}`"))?;
                Ok(Mount::new(*host, *guest, readonly))
            }
            _ => bail!("mount `{spec}` must be host:guest or host:guest:mode"),
        }
    }

    /// Format as a Docker/Podman `-v` argument: `host:guest` or `host:guest:ro`.
    pub fn to_docker_volume(&self) -> String {
        let mut s = format!("{}:{}", self.host.display(), self.guest.display());
        if self.readonly {
            s.push_str(":ro");
        }
        s
    }
}

impl FromStr for Mount {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Mount::parse(s)
    }
}

impl fmt::Display for Mount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}",
            self.host.display(),
            self.guest.display(),
            if self.readonly { "ro" } else { "rw" }
        )
    }
}

fn parse_mount_mode(mode: &str) -> Result<bool> {
    match mode.trim().to_ascii_lowercase().as_str() {
        "ro" | "readonly" | "read-only" => Ok(true),
        "rw" | "readwrite" | "read-write" => Ok(false),
        other => Err(anyhow!(
            "unknown mount mode `{other}` (expected ro|rw|readonly|readwrite)"
        )),
    }
}

/// Full description of a sandbox to create.
///
/// Built by the CLI/config layer and passed to [`crate::create`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SandboxSpec {
    /// Container image (Docker/Podman). Ignored by bwrap/host backends.
    pub image: String,
    /// Working directory inside the sandbox (guest path for containers).
    pub workdir: PathBuf,
    /// Bind mounts from host into the sandbox.
    pub mounts: Vec<Mount>,
    /// Environment variables set inside the sandbox (`KEY`, `VALUE`).
    pub env: Vec<(String, String)>,
    /// Network policy.
    pub network: NetworkMode,
    /// Hostnames allowed when [`NetworkMode::Egress`] is selected.
    ///
    /// Enforced authoritatively by ActPlane; sandbox backends treat this as
    /// advisory metadata only.
    pub allow_egress: Vec<String>,
    /// Optional CPU limit (e.g. `4.0` → `--cpus 4`).
    pub cpus: Option<f64>,
    /// Optional memory limit string as accepted by the container runtime
    /// (e.g. `"8G"`).
    pub memory: Option<String>,
    /// When true, leave the container (or other resources) around after exit
    /// for debugging.
    pub keep: bool,
    /// Sandbox / container name, typically `actime-<run-id>`.
    pub name: String,
}

impl SandboxSpec {
    /// Construct a minimal spec with sensible defaults for unit tests and
    /// simple callers.
    pub fn new(name: impl Into<String>, image: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            workdir: PathBuf::from("/workspace"),
            mounts: Vec::new(),
            env: Vec::new(),
            network: NetworkMode::Allow,
            allow_egress: Vec::new(),
            cpus: None,
            memory: None,
            keep: false,
            name: name.into(),
        }
    }

    /// Resolve relative mount host paths against `base` (usually the caller's
    /// cwd). Guest paths and absolute host paths are left unchanged.
    pub fn resolve_mount_hosts(&mut self, base: &Path) {
        for m in &mut self.mounts {
            if m.host.is_relative() {
                m.host = base.join(&m.host);
            }
        }
    }
}

/// Snapshot of how a sandbox was configured and whether isolation was active.
///
/// Embedded in the run manifest (`SandboxReport` field).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SandboxReport {
    /// Backend that ran the agent.
    pub backend: Backend,
    /// Sandbox / container name.
    pub name: String,
    /// Image used, when the backend is container-based.
    pub image: Option<String>,
    /// Host PID of the sandbox root process at report time, if known.
    pub host_pid: Option<i32>,
    /// Network mode applied.
    pub network: NetworkMode,
    /// Whether the isolation plane was actually active.
    pub isolation: bool,
    /// Optional human-readable note (e.g. isolation-off warning for host).
    pub note: Option<String>,
}

impl SandboxReport {
    /// Build a report for an isolated backend.
    pub fn isolated(
        backend: Backend,
        name: impl Into<String>,
        image: Option<String>,
        host_pid: Option<i32>,
        network: NetworkMode,
    ) -> Self {
        Self {
            backend,
            name: name.into(),
            image,
            host_pid,
            network,
            isolation: true,
            note: None,
        }
    }

    /// Build a report for host mode (isolation plane off).
    pub fn host(name: impl Into<String>, host_pid: Option<i32>, network: NetworkMode) -> Self {
        Self {
            backend: Backend::Host,
            name: name.into(),
            image: None,
            host_pid,
            network,
            isolation: false,
            note: Some("isolation plane is off (sandbox backend: host)".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_parse_host_guest_rw() {
        let m = Mount::parse(".:/workspace:rw").unwrap();
        assert_eq!(m.host, PathBuf::from("."));
        assert_eq!(m.guest, PathBuf::from("/workspace"));
        assert!(!m.readonly);
    }

    #[test]
    fn mount_parse_host_guest_ro() {
        let m = Mount::parse("/data:/mnt/data:ro").unwrap();
        assert_eq!(m.host, PathBuf::from("/data"));
        assert_eq!(m.guest, PathBuf::from("/mnt/data"));
        assert!(m.readonly);
    }

    #[test]
    fn mount_parse_default_rw() {
        let m = Mount::parse("/tmp/work:/workspace").unwrap();
        assert!(!m.readonly);
        assert_eq!(m.to_docker_volume(), "/tmp/work:/workspace");
    }

    #[test]
    fn mount_parse_readonly_aliases() {
        assert!(Mount::parse("a:b:readonly").unwrap().readonly);
        assert!(Mount::parse("a:b:read-only").unwrap().readonly);
        assert!(!Mount::parse("a:b:readwrite").unwrap().readonly);
    }

    #[test]
    fn mount_parse_errors() {
        assert!(Mount::parse("").is_err());
        assert!(Mount::parse(":guest").is_err());
        assert!(Mount::parse("host:").is_err());
        assert!(Mount::parse("h:g:weird").is_err());
    }

    #[test]
    fn mount_docker_volume_ro() {
        let m = Mount::new("/h", "/g", true);
        assert_eq!(m.to_docker_volume(), "/h:/g:ro");
        assert_eq!(m.to_string(), "/h:/g:ro");
    }

    #[test]
    fn network_mode_parse_display() {
        assert_eq!(NetworkMode::from_str("Allow").unwrap(), NetworkMode::Allow);
        assert_eq!(NetworkMode::from_str("deny").unwrap(), NetworkMode::Deny);
        assert_eq!(
            NetworkMode::from_str("EGRESS").unwrap(),
            NetworkMode::Egress
        );
        assert_eq!(NetworkMode::Allow.to_string(), "allow");
        assert!(NetworkMode::from_str("foo").is_err());
    }

    #[test]
    fn sandbox_spec_serde_roundtrip() {
        let mut spec = SandboxSpec::new("actime-test", "img:latest");
        spec.mounts.push(Mount::parse(".:/workspace:rw").unwrap());
        spec.env.push(("A".into(), "1".into()));
        spec.network = NetworkMode::Deny;
        spec.cpus = Some(2.0);
        spec.memory = Some("1G".into());
        let json = serde_json::to_string(&spec).unwrap();
        let back: SandboxSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn sandbox_report_host_records_isolation_off() {
        let r = SandboxReport::host("actime-x", Some(42), NetworkMode::Allow);
        assert!(!r.isolation);
        assert_eq!(r.backend, Backend::Host);
        assert!(r.note.as_ref().unwrap().contains("isolation plane is off"));
    }
}
