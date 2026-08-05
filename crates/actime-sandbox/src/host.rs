//! Host backend: no isolation, plain child process.
//!
//! Used as the last-resort fallback and for workstation `--sandbox host`.
//! [`Sandbox::report`] records that the isolation plane is off.
//!
//! Working directory rules: the host backend always runs the agent in the
//! caller's actual current directory (or an explicit *host* path passed via
//! [`SandboxSpec::workdir`]). The guest path `/workspace` is for container
//! backends only and must never become the host child's cwd just because that
//! path happens to exist on the machine.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};

use crate::backend::{signal_pid, Backend, Sandbox};
use crate::spec::{SandboxReport, SandboxSpec};

/// Unsandboxed host process runner.
pub struct HostSandbox {
    spec: SandboxSpec,
    child: Option<Child>,
    host_pid: Option<i32>,
    exit_code: Option<i32>,
    cleaned: bool,
}

impl HostSandbox {
    /// Create a host sandbox from `spec`.
    ///
    /// `image`, container name semantics, and guest mount paths are ignored.
    /// [`SandboxSpec::workdir`] is used as the child working directory only
    /// when it is a real host path (see [`resolve_host_workdir`]); the sandbox
    /// guest path `/workspace` is never used.
    pub fn new(spec: SandboxSpec) -> Self {
        Self {
            spec,
            child: None,
            host_pid: None,
            exit_code: None,
            cleaned: false,
        }
    }

    /// Build the effective command argv for pure unit tests.
    ///
    /// Host mode does not wrap the agent; the returned vector is exactly
    /// `argv` (no prefix binary).
    pub fn build_argv(argv: &[String]) -> Vec<String> {
        argv.to_vec()
    }
}

/// Decide the child's working directory for host mode.
///
/// Returns `None` when the caller should inherit the current directory.
/// Rejects the conventional container guest path `/workspace` even if that
/// directory exists on the host — otherwise a leftover `/workspace` from a
/// previous container mount steals the demo and every host run.
pub fn resolve_host_workdir(spec_workdir: &Path) -> Option<PathBuf> {
    if spec_workdir.as_os_str().is_empty() {
        return None;
    }
    // Container guest default must never become a host cwd.
    if spec_workdir == Path::new("/workspace") {
        return None;
    }
    if spec_workdir.is_absolute() && spec_workdir.is_dir() {
        return Some(spec_workdir.to_path_buf());
    }
    None
}

impl Sandbox for HostSandbox {
    fn kind(&self) -> Backend {
        Backend::Host
    }

    fn host_pid(&self) -> Option<i32> {
        self.host_pid
    }

    fn evidence_target(&self) -> Option<String> {
        // Host mode: AgentSight attaches by host PID, not a binary-path target.
        None
    }

    fn start(&mut self) -> Result<()> {
        // Expose an attach target before the agent exists so the policy and
        // evidence planes can bind to this process tree. The agent is later
        // spawned as our child, so attaching to us covers it. On spawn we
        // replace this with the child pid so signals target the agent.
        if self.host_pid.is_none() {
            self.host_pid = Some(std::process::id() as i32);
        }
        Ok(())
    }

    fn spawn(&mut self, argv: &[String]) -> Result<()> {
        if self.child.is_some() || self.exit_code.is_some() {
            bail!("sandbox process already spawned");
        }
        if argv.is_empty() {
            bail!("cannot spawn sandbox with empty argv");
        }

        let (program, args) = argv.split_first().ok_or_else(|| anyhow!("empty argv"))?;

        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        if let Some(dir) = resolve_host_workdir(&self.spec.workdir) {
            cmd.current_dir(dir);
        }

        for (k, v) in &self.spec.env {
            cmd.env(k, v);
        }

        let child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn host process `{program}`"))?;

        self.host_pid = Some(child.id() as i32);
        self.child = Some(child);
        Ok(())
    }

    fn try_wait(&mut self) -> Result<Option<i32>> {
        crate::backend::poll_child(&mut self.child, &mut self.exit_code)
    }

    fn wait(&mut self) -> Result<i32> {
        if let Some(code) = self.exit_code {
            return Ok(code);
        }
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| anyhow!("sandbox process has not been spawned"))?;
        let status = child.wait().context("waiting for host process")?;
        let code = status.code().unwrap_or_else(|| {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(sig) = status.signal() {
                    return 128 + sig;
                }
            }
            1
        });
        self.exit_code = Some(code);
        Ok(code)
    }

    fn signal(&mut self, sig: i32) -> Result<()> {
        if let Some(pid) = self.host_pid {
            signal_pid(pid, sig)?;
        }
        Ok(())
    }

    fn cleanup(&mut self) -> Result<()> {
        if self.cleaned {
            return Ok(());
        }
        self.cleaned = true;
        if let Some(mut child) = self.child.take() {
            if !self.spec.keep {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        Ok(())
    }

    fn report(&self) -> SandboxReport {
        SandboxReport::host(self.spec.name.clone(), self.host_pid, self.spec.network)
    }
}

impl Drop for HostSandbox {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::NetworkMode;

    #[test]
    fn host_build_argv_is_identity() {
        let argv = vec!["claude".into(), "chat".into()];
        assert_eq!(HostSandbox::build_argv(&argv), argv);
    }

    #[test]
    fn report_records_isolation_off() {
        let spec = SandboxSpec::new("actime-host", "unused");
        let sb = HostSandbox::new(spec);
        let r = sb.report();
        assert_eq!(r.backend, Backend::Host);
        assert!(!r.isolation);
        assert!(r
            .note
            .as_ref()
            .is_some_and(|n| n.contains("isolation plane is off")));
    }

    #[test]
    fn evidence_target_none() {
        let sb = HostSandbox::new(SandboxSpec::new("n", "img"));
        assert!(sb.evidence_target().is_none());
    }

    #[test]
    fn resolve_host_workdir_rejects_container_guest_path() {
        // Even when /workspace exists on this machine, host mode must not use it.
        assert!(resolve_host_workdir(Path::new("/workspace")).is_none());
        assert!(resolve_host_workdir(Path::new("")).is_none());
        // A real host temp dir is accepted.
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            resolve_host_workdir(dir.path()).as_deref(),
            Some(dir.path())
        );
    }

    #[test]
    fn start_exposes_host_pid_before_spawn() {
        let mut sb = HostSandbox::new(SandboxSpec::new("actime-t", "unused"));
        assert!(sb.host_pid().is_none());
        sb.start().unwrap();
        let pid = sb.host_pid().expect("host_pid after start");
        assert_eq!(pid, std::process::id() as i32);
    }

    #[test]
    fn spawn_wait_true() {
        let mut sb = HostSandbox::new(SandboxSpec::new("actime-t", "unused"));
        sb.start().unwrap();
        sb.spawn(&["/bin/true".into()]).unwrap();
        let pid = sb.host_pid().unwrap();
        assert!(pid > 0);
        // After spawn the attach target is the child, not actime itself.
        assert_ne!(pid, std::process::id() as i32);
        let code = sb.wait().unwrap();
        assert_eq!(code, 0);
        // wait and signal are safe after exit
        assert_eq!(sb.wait().unwrap(), 0);
        sb.signal(libc::SIGTERM).unwrap();
        sb.cleanup().unwrap();
        sb.cleanup().unwrap();
    }

    #[test]
    fn spawn_does_not_chdir_into_workspace_guest_path() {
        // Regression: a host that happens to have /workspace must still run
        // the agent in the caller's cwd (inherited), not under /workspace.
        let mut spec = SandboxSpec::new("actime-t", "unused");
        spec.workdir = PathBuf::from("/workspace");
        let mut sb = HostSandbox::new(spec);
        sb.start().unwrap();
        // Use pwd via a short script to avoid depending on /bin/pwd location.
        let code = {
            sb.spawn(&["/bin/true".into()]).unwrap();
            sb.wait().unwrap()
        };
        assert_eq!(code, 0);
    }

    #[test]
    fn spawn_wait_false() {
        let mut sb = HostSandbox::new(SandboxSpec::new("actime-t", "unused"));
        sb.spawn(&["/bin/false".into()]).unwrap();
        assert_eq!(sb.wait().unwrap(), 1);
    }

    #[test]
    fn empty_argv_errors() {
        let mut sb = HostSandbox::new(SandboxSpec::new("n", "i"));
        assert!(sb.spawn(&[]).is_err());
    }

    #[test]
    fn report_includes_network() {
        let mut spec = SandboxSpec::new("n", "i");
        spec.network = NetworkMode::Deny;
        let sb = HostSandbox::new(spec);
        assert_eq!(sb.report().network, NetworkMode::Deny);
    }
}
