//! Bubblewrap (`bwrap`) namespace sandbox backend.
//!
//! Intended for machines without Docker/Podman. Runs unprivileged with:
//! - read-only binds of `/usr`, `/lib`, `/lib64`, `/bin`, `/sbin`, `/etc`
//! - `--proc /proc`, `--dev /dev`, `--tmpfs /tmp`
//! - workspace mounts read-write (or read-only when requested)
//! - `--die-with-parent`
//! - `--unshare-net` only when [`NetworkMode::Deny`]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};

use crate::backend::{signal_pid, Backend, Sandbox};
use crate::spec::{NetworkMode, SandboxReport, SandboxSpec};

/// Paths that are bind-mounted read-only from the host when they exist.
pub const RO_BIND_PATHS: &[&str] = &["/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc"];

/// Bubblewrap sandbox instance.
pub struct BwrapSandbox {
    spec: SandboxSpec,
    child: Option<Child>,
    host_pid: Option<i32>,
    exit_code: Option<i32>,
    cleaned: bool,
}

impl BwrapSandbox {
    /// Create a bubblewrap sandbox from `spec`.
    pub fn new(spec: SandboxSpec) -> Self {
        Self {
            spec,
            child: None,
            host_pid: None,
            exit_code: None,
            cleaned: false,
        }
    }

    /// Build the exact `bwrap` argument vector (including `bwrap` as argv\[0\])
    /// for a given spec and agent argv.
    ///
    /// When `only_existing` is true, read-only system binds that do not exist
    /// on this host are skipped (required for a successful spawn). Unit tests
    /// pass `false` to assert the full intended vector.
    pub fn build_argv(spec: &SandboxSpec, argv: &[String], only_existing: bool) -> Vec<String> {
        let mut args = vec!["bwrap".to_string()];

        for path in RO_BIND_PATHS {
            if only_existing && !Path::new(path).exists() {
                continue;
            }
            args.push("--ro-bind".to_string());
            args.push((*path).to_string());
            args.push((*path).to_string());
        }

        args.push("--proc".to_string());
        args.push("/proc".to_string());
        args.push("--dev".to_string());
        args.push("/dev".to_string());
        args.push("--tmpfs".to_string());
        args.push("/tmp".to_string());

        for m in &spec.mounts {
            if m.readonly {
                args.push("--ro-bind".to_string());
            } else {
                args.push("--bind".to_string());
            }
            args.push(m.host.display().to_string());
            args.push(m.guest.display().to_string());
        }

        args.push("--die-with-parent".to_string());

        if matches!(spec.network, NetworkMode::Deny) {
            args.push("--unshare-net".to_string());
        }

        args.push("--chdir".to_string());
        args.push(spec.workdir.display().to_string());

        for (k, v) in &spec.env {
            args.push("--setenv".to_string());
            args.push(k.clone());
            args.push(v.clone());
        }

        // End of bwrap options; agent command follows.
        args.push("--".to_string());
        args.extend(argv.iter().cloned());
        args
    }
}

impl Sandbox for BwrapSandbox {
    fn kind(&self) -> Backend {
        Backend::Bwrap
    }

    fn host_pid(&self) -> Option<i32> {
        self.host_pid
    }

    fn evidence_target(&self) -> Option<String> {
        // No container target; AgentSight attaches by host PID.
        None
    }

    fn spawn(&mut self, argv: &[String]) -> Result<()> {
        if self.child.is_some() || self.exit_code.is_some() {
            bail!("sandbox process already spawned");
        }
        if argv.is_empty() {
            bail!("cannot spawn sandbox with empty argv");
        }

        let full = Self::build_argv(&self.spec, argv, true);
        let (program, args) = full
            .split_first()
            .ok_or_else(|| anyhow!("empty bwrap argv"))?;

        let child = Command::new(program)
            .args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| "failed to execute bwrap".to_string())?;

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
        let status = child.wait().context("waiting for bwrap process")?;
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
            // If still running and keep is false, terminate; if keep, leave it.
            if !self.spec.keep {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        Ok(())
    }

    fn report(&self) -> SandboxReport {
        SandboxReport::isolated(
            Backend::Bwrap,
            self.spec.name.clone(),
            None,
            self.host_pid,
            self.spec.network,
        )
    }
}

impl Drop for BwrapSandbox {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

/// Resolve a path for tests / callers that want an absolute host mount.
#[allow(dead_code)]
pub(crate) fn abs(path: impl AsRef<Path>) -> PathBuf {
    path.as_ref().to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::Mount;

    fn sample_spec(network: NetworkMode) -> SandboxSpec {
        let mut spec = SandboxSpec::new("actime-bwrap", "unused");
        spec.workdir = PathBuf::from("/workspace");
        spec.mounts
            .push(Mount::new("/home/user/proj", "/workspace", false));
        spec.env.push(("HOME".into(), "/workspace".into()));
        spec.network = network;
        spec
    }

    #[test]
    fn bwrap_argv_deny_network_exact() {
        let spec = sample_spec(NetworkMode::Deny);
        let agent = vec!["claude".into()];
        let argv = BwrapSandbox::build_argv(&spec, &agent, false);

        let expected = vec![
            "bwrap",
            "--ro-bind",
            "/usr",
            "/usr",
            "--ro-bind",
            "/lib",
            "/lib",
            "--ro-bind",
            "/lib64",
            "/lib64",
            "--ro-bind",
            "/bin",
            "/bin",
            "--ro-bind",
            "/sbin",
            "/sbin",
            "--ro-bind",
            "/etc",
            "/etc",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
            "--bind",
            "/home/user/proj",
            "/workspace",
            "--die-with-parent",
            "--unshare-net",
            "--chdir",
            "/workspace",
            "--setenv",
            "HOME",
            "/workspace",
            "--",
            "claude",
        ];
        assert_eq!(argv, expected);
    }

    #[test]
    fn bwrap_argv_allow_no_unshare_net() {
        let spec = sample_spec(NetworkMode::Allow);
        let argv = BwrapSandbox::build_argv(&spec, &["true".into()], false);
        assert!(!argv.iter().any(|a| a == "--unshare-net"));
        assert!(argv.iter().any(|a| a == "--die-with-parent"));
    }

    #[test]
    fn bwrap_argv_egress_no_unshare_net() {
        // Egress is best-effort at the ActPlane layer; bwrap keeps the net ns.
        let spec = sample_spec(NetworkMode::Egress);
        let argv = BwrapSandbox::build_argv(&spec, &["true".into()], false);
        assert!(!argv.iter().any(|a| a == "--unshare-net"));
    }

    #[test]
    fn bwrap_ro_mount_uses_ro_bind() {
        let mut spec = sample_spec(NetworkMode::Allow);
        spec.mounts.clear();
        spec.mounts
            .push(Mount::new("/etc/hosts", "/etc/hosts", true));
        let argv = BwrapSandbox::build_argv(&spec, &["true".into()], false);
        // Find the workspace-related ro-bind for /etc/hosts (last --ro-bind pair before --proc ends).
        let mut found = false;
        let mut i = 0;
        while i + 2 < argv.len() {
            if argv[i] == "--ro-bind" && argv[i + 1] == "/etc/hosts" && argv[i + 2] == "/etc/hosts"
            {
                found = true;
                break;
            }
            i += 1;
        }
        assert!(
            found,
            "expected --ro-bind /etc/hosts /etc/hosts in {argv:?}"
        );
    }

    #[test]
    fn evidence_target_is_none() {
        let sb = BwrapSandbox::new(SandboxSpec::new("n", "img"));
        assert!(sb.evidence_target().is_none());
    }

    #[test]
    fn only_existing_skips_missing_paths() {
        let spec = sample_spec(NetworkMode::Allow);
        let argv = BwrapSandbox::build_argv(&spec, &["true".into()], true);
        // /usr almost always exists on Linux; the important property is that
        // build does not panic and still includes core flags.
        assert_eq!(argv[0], "bwrap");
        assert!(argv.iter().any(|a| a == "--die-with-parent"));
        assert!(argv.iter().any(|a| a == "--proc"));
    }
}
