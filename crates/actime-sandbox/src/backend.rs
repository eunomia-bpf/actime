//! Backend selection, probing, and the [`Sandbox`] trait.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::bwrap::BwrapSandbox;
use crate::docker::DockerSandbox;
use crate::host::HostSandbox;
use crate::spec::{SandboxReport, SandboxSpec};

/// Sandbox backend kinds supported by Actime.
///
/// Probe order for `auto` is Docker → Podman → Bubblewrap → Host
/// ([`Backend::detect_available`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// Docker Engine via the `docker` CLI.
    Docker,
    /// Podman via the `podman` CLI (same driver as Docker).
    Podman,
    /// Bubblewrap (`bwrap`) user-namespace sandbox.
    Bwrap,
    /// No isolation — plain child process.
    Host,
}

impl Backend {
    /// Human-readable name used in logs and doctor output.
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Docker => "docker",
            Backend::Podman => "podman",
            Backend::Bwrap => "bwrap",
            Backend::Host => "host",
        }
    }

    /// Probe backends in order and return those that are available.
    ///
    /// Order: Docker, Podman, Bwrap, Host. Host is always available and always
    /// appears last in the result.
    pub fn detect_available() -> Vec<Backend> {
        let candidates = [
            Backend::Docker,
            Backend::Podman,
            Backend::Bwrap,
            Backend::Host,
        ];
        let mut available = Vec::new();
        for backend in candidates {
            if backend.probe().available {
                available.push(backend);
            }
        }
        // Host must always be present as a last-resort backend.
        if !available.contains(&Backend::Host) {
            available.push(Backend::Host);
        }
        available
    }

    /// Probe whether this backend can be used on the current machine.
    ///
    /// Never panics. On failure, [`BackendProbe::reason`] is a precise,
    /// human-readable explanation (binary missing, daemon unreachable,
    /// permission denied, …).
    pub fn probe(self) -> BackendProbe {
        match self {
            Backend::Docker => probe_container_cli("docker"),
            Backend::Podman => probe_container_cli("podman"),
            Backend::Bwrap => probe_bwrap(),
            Backend::Host => BackendProbe {
                available: true,
                version: None,
                reason: None,
            },
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Backend {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "docker" => Ok(Backend::Docker),
            "podman" => Ok(Backend::Podman),
            "bwrap" | "bubblewrap" => Ok(Backend::Bwrap),
            "host" => Ok(Backend::Host),
            other => bail!("unknown sandbox backend `{other}` (expected docker|podman|bwrap|host)"),
        }
    }
}

/// Result of probing a single backend for availability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendProbe {
    /// Whether the backend can be used right now.
    pub available: bool,
    /// Version string when the tool reported one.
    pub version: Option<String>,
    /// Why the backend is unavailable (set when `available` is false).
    pub reason: Option<String>,
}

impl BackendProbe {
    /// Construct a successful probe with an optional version.
    pub fn ok(version: Option<String>) -> Self {
        Self {
            available: true,
            version,
            reason: None,
        }
    }

    /// Construct a failed probe with a human-readable reason.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            version: None,
            reason: Some(reason.into()),
        }
    }

    /// Format a short diagnostic line for doctor / CLI output.
    pub fn format_reason(&self, backend: Backend) -> String {
        if self.available {
            match &self.version {
                Some(v) => format!("{backend}: available ({v})"),
                None => format!("{backend}: available"),
            }
        } else {
            let why = self.reason.as_deref().unwrap_or("unavailable");
            format!("{backend}: unavailable — {why}")
        }
    }
}

/// A live sandbox instance.
///
/// Lifecycle: construct via [`create`], then [`Sandbox::spawn`],
/// [`Sandbox::wait`] / [`Sandbox::signal`], and finally [`Sandbox::cleanup`].
pub trait Sandbox: Send {
    /// Backend kind for this instance.
    fn kind(&self) -> Backend;

    /// PID on the *host* of the sandbox's root process.
    ///
    /// This is the anchor the policy and evidence planes attach to. `None`
    /// means the backend cannot expose one and eBPF planes must degrade.
    fn host_pid(&self) -> Option<i32>;

    /// Backend-native target string for AgentSight `--binary-path`,
    /// e.g. `docker://actime-<id>`. `None` for host and bwrap modes.
    fn evidence_target(&self) -> Option<String>;

    /// Bring the sandbox up *without* starting the agent.
    ///
    /// After this returns, [`Sandbox::host_pid`] and
    /// [`Sandbox::evidence_target`] must be usable, so the policy and evidence
    /// planes can attach before any agent code runs. Docker and Podman start
    /// the container detached with an idle entrypoint; bwrap and host have
    /// nothing to do and use the default no-op.
    ///
    /// Calling [`Sandbox::spawn`] without calling `start` first is allowed:
    /// backends that need it start lazily. Idempotent.
    fn start(&mut self) -> Result<()> {
        Ok(())
    }

    /// Start the agent command inside the sandbox.
    ///
    /// Stdio is inherited from the caller. Must only be called once.
    fn spawn(&mut self, argv: &[String]) -> Result<()>;

    /// Wait for the sandboxed process to exit and return its exit code.
    ///
    /// Safe to call after the child has already exited; returns the stored
    /// exit code.
    fn wait(&mut self) -> Result<i32>;

    /// Poll for exit without blocking. `Ok(None)` means still running.
    ///
    /// This is what makes a wall-clock limit enforceable: the caller polls,
    /// and on timeout escalates through [`Sandbox::signal`]. The default
    /// implementation blocks, so a backend that does not override it simply
    /// cannot be timed out.
    fn try_wait(&mut self) -> Result<Option<i32>> {
        self.wait().map(Some)
    }

    /// Forward a POSIX signal to the sandbox root process.
    ///
    /// Safe to call after the child has already exited (no-op / success on
    /// ESRCH).
    fn signal(&mut self, sig: i32) -> Result<()>;

    /// Tear down backend resources (e.g. remove the container).
    ///
    /// Honors [`SandboxSpec::keep`]. Safe to call more than once.
    fn cleanup(&mut self) -> Result<()>;

    /// Snapshot for the run manifest.
    fn report(&self) -> SandboxReport;
}

/// Map an exit status to a code, using the `128 + signal` convention when the
/// process was signaled rather than exiting normally.
pub(crate) fn exit_status_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    1
}

/// Shared non-blocking poll used by every backend's `try_wait`.
///
/// Caches the exit code on first observation so later calls are cheap and
/// remain correct after the child has been reaped.
pub(crate) fn poll_child(
    child: &mut Option<std::process::Child>,
    exit_code: &mut Option<i32>,
) -> Result<Option<i32>> {
    if let Some(code) = *exit_code {
        return Ok(Some(code));
    }
    let Some(c) = child.as_mut() else {
        return Ok(None);
    };
    match c.try_wait().context("polling the sandbox process")? {
        Some(status) => {
            let code = exit_status_code(status);
            *exit_code = Some(code);
            Ok(Some(code))
        }
        None => Ok(None),
    }
}

/// Create a sandbox for `backend` from `spec`.
///
/// Does not require the backend to pass [`Backend::probe`]; forced backends
/// are constructed and fail later at [`Sandbox::spawn`] with a clear error.
pub fn create(backend: Backend, spec: SandboxSpec) -> Result<Box<dyn Sandbox>> {
    match backend {
        Backend::Docker => Ok(Box::new(DockerSandbox::new(
            "docker",
            Backend::Docker,
            spec,
        ))),
        Backend::Podman => Ok(Box::new(DockerSandbox::new(
            "podman",
            Backend::Podman,
            spec,
        ))),
        Backend::Bwrap => Ok(Box::new(BwrapSandbox::new(spec))),
        Backend::Host => Ok(Box::new(HostSandbox::new(spec))),
    }
}

/// Locate `name` on `PATH`.
pub(crate) fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

fn probe_container_cli(binary: &str) -> BackendProbe {
    if find_on_path(binary).is_none() {
        return BackendProbe::unavailable(format!("{binary} binary not found on PATH"));
    }

    // `info` talks to the daemon; more accurate than `version` alone.
    let output = match Command::new(binary).arg("info").output() {
        Ok(o) => o,
        Err(err) => {
            return BackendProbe::unavailable(format!("failed to execute `{binary} info`: {err}"));
        }
    };

    if output.status.success() {
        let version = container_version(binary);
        return BackendProbe::ok(version);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stderr}\n{stdout}").to_ascii_lowercase();

    let reason = if combined.contains("permission denied")
        || combined.contains("permission_denied")
        || combined.contains("access denied")
    {
        format!("{binary} daemon permission denied (is the user in the docker/podman group?)")
    } else if combined.contains("cannot connect")
        || combined.contains("is the docker daemon running")
        || combined.contains("connection refused")
        || combined.contains("no such file or directory")
        || combined.contains("daemon")
            && (combined.contains("not running") || combined.contains("unreachable"))
    {
        format!("{binary} daemon unreachable or not running")
    } else {
        let detail = first_nonempty_line(&stderr)
            .or_else(|| first_nonempty_line(&stdout))
            .unwrap_or("unknown error");
        format!("{binary} info failed: {detail}")
    };

    BackendProbe::unavailable(reason)
}

fn container_version(binary: &str) -> Option<String> {
    let output = Command::new(binary)
        .args(["version", "--format", "{{.Client.Version}}"])
        .output()
        .ok()?;
    if !output.status.success() {
        // Fallback: first line of `version`.
        let output = Command::new(binary).arg("version").output().ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        return first_nonempty_line(&text).map(|s| s.to_string());
    }
    let v = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

fn probe_bwrap() -> BackendProbe {
    if find_on_path("bwrap").is_none() {
        return BackendProbe::unavailable("bwrap (bubblewrap) binary not found on PATH");
    }

    let output = match Command::new("bwrap").arg("--version").output() {
        Ok(o) => o,
        Err(err) => {
            return BackendProbe::unavailable(format!(
                "failed to execute `bwrap --version`: {err}"
            ));
        }
    };

    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout);
        let version = first_nonempty_line(&text).map(|s| s.to_string());
        return BackendProbe::ok(version);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = stderr.to_ascii_lowercase();
    let reason = if combined.contains("permission denied") {
        "bwrap permission denied (user namespaces may be restricted)".to_string()
    } else {
        let detail = first_nonempty_line(&stderr).unwrap_or("bwrap --version failed");
        format!("bwrap unavailable: {detail}")
    };
    BackendProbe::unavailable(reason)
}

fn first_nonempty_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|l| !l.is_empty())
}

/// Send `sig` to `pid`. Treats ESRCH (no such process) as success so callers
/// may signal after the child has already exited.
pub(crate) fn signal_pid(pid: i32, sig: i32) -> Result<()> {
    if pid <= 0 {
        bail!("invalid pid {pid} for signal {sig}");
    }
    let rc = unsafe { libc::kill(pid, sig) };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(anyhow::anyhow!(
        "failed to send signal {sig} to pid {pid}: {err}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_reason_formatting_available() {
        let p = BackendProbe::ok(Some("24.0.0".into()));
        assert_eq!(
            p.format_reason(Backend::Docker),
            "docker: available (24.0.0)"
        );
        let p2 = BackendProbe::ok(None);
        assert_eq!(p2.format_reason(Backend::Host), "host: available");
    }

    #[test]
    fn probe_reason_formatting_unavailable() {
        let p = BackendProbe::unavailable("docker binary not found on PATH");
        assert_eq!(
            p.format_reason(Backend::Docker),
            "docker: unavailable — docker binary not found on PATH"
        );
    }

    #[test]
    fn detect_available_ordering_always_ends_with_host() {
        let avail = Backend::detect_available();
        assert!(!avail.is_empty());
        assert_eq!(*avail.last().unwrap(), Backend::Host);
        // Host appears exactly once.
        assert_eq!(avail.iter().filter(|b| **b == Backend::Host).count(), 1);
        // Relative order of any present non-host backends is Docker < Podman < Bwrap.
        let rank = |b: Backend| match b {
            Backend::Docker => 0,
            Backend::Podman => 1,
            Backend::Bwrap => 2,
            Backend::Host => 3,
        };
        let ranks: Vec<_> = avail.iter().map(|b| rank(*b)).collect();
        let mut sorted = ranks.clone();
        sorted.sort();
        assert_eq!(ranks, sorted);
    }

    #[test]
    fn host_probe_always_available() {
        let p = Backend::Host.probe();
        assert!(p.available);
        assert!(p.reason.is_none());
    }

    #[test]
    fn backend_from_str_and_display() {
        assert_eq!(Backend::from_str("Docker").unwrap(), Backend::Docker);
        assert_eq!(Backend::from_str("bubblewrap").unwrap(), Backend::Bwrap);
        assert_eq!(Backend::Podman.to_string(), "podman");
        assert!(Backend::from_str("kvm").is_err());
    }

    #[test]
    fn create_host_sandbox() {
        let spec = SandboxSpec::new("actime-t", "unused");
        let sb = create(Backend::Host, spec).unwrap();
        assert_eq!(sb.kind(), Backend::Host);
        assert!(sb.evidence_target().is_none());
    }

    #[test]
    fn create_docker_sandbox_kind() {
        let spec = SandboxSpec::new("actime-t", "img:latest");
        let sb = create(Backend::Docker, spec).unwrap();
        assert_eq!(sb.kind(), Backend::Docker);
    }

    #[test]
    fn backend_serde_roundtrip() {
        for b in [
            Backend::Docker,
            Backend::Podman,
            Backend::Bwrap,
            Backend::Host,
        ] {
            let json = serde_json::to_string(&b).unwrap();
            let back: Backend = serde_json::from_str(&json).unwrap();
            assert_eq!(back, b);
        }
    }
}
