//! Docker and Podman backends sharing one CLI-driven implementation.
//!
//! Both engines are controlled via `std::process::Command` only (no Docker
//! API crates, no async). The implementation is parameterized solely by the
//! CLI binary name (`docker` or `podman`).

use std::process::{Child, Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};

use crate::backend::{signal_pid, Backend, Sandbox};
use crate::spec::{NetworkMode, SandboxReport, SandboxSpec};

/// Shared Docker/Podman sandbox, parameterized by CLI binary name.
///
/// Lifecycle:
/// 1. `docker|podman run -d … <image> sleep infinity` (detached container)
/// 2. Read host PID via `inspect --format '{{.State.Pid}}'`
/// 3. `docker|podman exec -i <name> <argv…>` attached to caller stdio
/// 4. On cleanup, `rm -f` the container unless [`SandboxSpec::keep`]
pub struct DockerSandbox {
    /// CLI binary name: `docker` or `podman`.
    binary: &'static str,
    kind: Backend,
    spec: SandboxSpec,
    /// Detached container has been created.
    container_started: bool,
    /// The `exec` child process (agent).
    child: Option<Child>,
    host_pid: Option<i32>,
    exit_code: Option<i32>,
    cleaned: bool,
}

impl DockerSandbox {
    /// Create a sandbox that will use `binary` (`docker` or `podman`).
    pub fn new(binary: &'static str, kind: Backend, spec: SandboxSpec) -> Self {
        Self {
            binary,
            kind,
            spec,
            container_started: false,
            child: None,
            host_pid: None,
            exit_code: None,
            cleaned: false,
        }
    }

    /// Build the exact `run -d …` argument vector (including the binary name
    /// as argv\[0\]) for a given spec. Pure function used by unit tests.
    pub fn build_run_argv(binary: &str, spec: &SandboxSpec) -> Vec<String> {
        let mut args = vec![
            binary.to_string(),
            "run".to_string(),
            "-d".to_string(),
            "--name".to_string(),
            spec.name.clone(),
        ];

        for m in &spec.mounts {
            args.push("-v".to_string());
            args.push(m.to_docker_volume());
        }

        args.push("-w".to_string());
        args.push(spec.workdir.display().to_string());

        for (k, v) in &spec.env {
            args.push("-e".to_string());
            args.push(format!("{k}={v}"));
        }

        if let Some(cpus) = spec.cpus {
            args.push("--cpus".to_string());
            // Avoid trailing `.0` noise when the value is integral.
            if (cpus.fract()).abs() < f64::EPSILON {
                args.push(format!("{}", cpus as i64));
            } else {
                args.push(cpus.to_string());
            }
        }

        if let Some(ref mem) = spec.memory {
            args.push("--memory".to_string());
            args.push(mem.clone());
        }

        match spec.network {
            NetworkMode::Allow => {
                // Default bridge network; do not pass --network.
            }
            NetworkMode::Deny => {
                args.push("--network".to_string());
                args.push("none".to_string());
            }
            NetworkMode::Egress => {
                // Best-effort: use the default bridge. Authoritative egress
                // control is ActPlane connect rules on the host.
                args.push("--network".to_string());
                args.push("bridge".to_string());
            }
        }

        args.push(spec.image.clone());
        // Keep the container alive so we can `exec` the agent into it.
        args.push("sleep".to_string());
        args.push("infinity".to_string());
        args
    }

    /// Build the exact `exec -i …` argument vector (including binary name).
    pub fn build_exec_argv(binary: &str, name: &str, argv: &[String]) -> Vec<String> {
        let mut args = vec![
            binary.to_string(),
            "exec".to_string(),
            "-i".to_string(),
            name.to_string(),
        ];
        args.extend(argv.iter().cloned());
        args
    }

    /// Build the inspect command used for `host_pid()`.
    pub fn build_inspect_pid_argv(binary: &str, name: &str) -> Vec<String> {
        vec![
            binary.to_string(),
            "inspect".to_string(),
            "--format".to_string(),
            "{{.State.Pid}}".to_string(),
            name.to_string(),
        ]
    }

    fn run_detached_container(&mut self) -> Result<()> {
        if self.container_started {
            return Ok(());
        }
        let argv = Self::build_run_argv(self.binary, &self.spec);
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| anyhow!("empty docker run argv"))?;

        let output = Command::new(program)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| format!("failed to execute `{} run`", self.binary))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let trimmed = stderr.trim();
            // Missing / unpullable image is the common first-run failure; point
            // the user at the exact recovery steps instead of a raw docker dump.
            if image_missing_error(trimmed, &self.spec.image) {
                bail!(
                    "sandbox image `{}` is not available locally and could not be pulled.\n\
                     \n\
                     Fix one of:\n\
                       • build it:  actime sandbox build\n\
                       • pull it:   actime sandbox pull\n\
                       • or set sandbox.image in actime.yaml (or `--image`) to an image you have\n\
                     \n\
                     For a no-container smoke test: actime demo --sandbox host --policy observe\n\
                     \n\
                     {engine} said: {detail}",
                    self.spec.image,
                    engine = self.binary,
                    detail = first_line_of(trimmed),
                );
            }
            bail!(
                "`{} run` failed (status {}): {}",
                self.binary,
                output.status,
                trimmed
            );
        }

        self.container_started = true;
        self.host_pid = self.inspect_host_pid().ok();
        Ok(())
    }

    fn inspect_host_pid(&self) -> Result<i32> {
        let argv = Self::build_inspect_pid_argv(self.binary, &self.spec.name);
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| anyhow!("empty inspect argv"))?;

        let output = Command::new(program)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| format!("failed to execute `{} inspect`", self.binary))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("`{} inspect` failed: {}", self.binary, stderr.trim());
        }

        let text = String::from_utf8_lossy(&output.stdout);
        let pid_str = text.trim();
        let pid: i32 = pid_str
            .parse()
            .with_context(|| format!("invalid container pid from inspect: {pid_str:?}"))?;
        if pid <= 0 {
            bail!("container pid is {pid} (container may not be running)");
        }
        Ok(pid)
    }

    fn remove_container(&self) -> Result<()> {
        let output = Command::new(self.binary)
            .args(["rm", "-f", &self.spec.name])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| format!("failed to execute `{} rm`", self.binary))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Already-removed containers are fine.
            let lower = stderr.to_ascii_lowercase();
            if lower.contains("no such container") || lower.contains("no such object") {
                return Ok(());
            }
            bail!(
                "`{} rm -f {}` failed: {}",
                self.binary,
                self.spec.name,
                stderr.trim()
            );
        }
        Ok(())
    }
}

impl Sandbox for DockerSandbox {
    fn kind(&self) -> Backend {
        self.kind
    }

    fn host_pid(&self) -> Option<i32> {
        self.host_pid
    }

    fn evidence_target(&self) -> Option<String> {
        // AgentSight accepts docker:// for Docker; use the CLI name for clarity.
        Some(format!("{}://{}", self.binary, self.spec.name))
    }

    fn start(&mut self) -> Result<()> {
        self.run_detached_container()
    }

    fn spawn(&mut self, argv: &[String]) -> Result<()> {
        if self.child.is_some() || self.exit_code.is_some() {
            bail!("sandbox process already spawned");
        }
        if argv.is_empty() {
            bail!("cannot spawn sandbox with empty argv");
        }

        self.run_detached_container()?;

        let exec_argv = Self::build_exec_argv(self.binary, &self.spec.name, argv);
        let (program, args) = exec_argv
            .split_first()
            .ok_or_else(|| anyhow!("empty docker exec argv"))?;

        let child = Command::new(program)
            .args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("failed to execute `{} exec`", self.binary))?;

        // Prefer the container init PID; fall back to the exec child if inspect failed.
        if self.host_pid.is_none() {
            self.host_pid = Some(child.id() as i32);
        }

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
        let status = child.wait().context("waiting for container exec process")?;
        let code = status.code().unwrap_or_else(|| {
            // Signaled: map to 128+sig convention when possible.
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
        // Prefer signaling the container's init PID on the host so the whole
        // tree can be targeted; also try the exec child if present.
        if let Some(pid) = self.host_pid {
            signal_pid(pid, sig)?;
        }
        if let Some(child) = self.child.as_ref() {
            let exec_pid = child.id() as i32;
            if self.host_pid != Some(exec_pid) {
                let _ = signal_pid(exec_pid, sig);
            }
        }
        Ok(())
    }

    fn cleanup(&mut self) -> Result<()> {
        if self.cleaned {
            return Ok(());
        }
        self.cleaned = true;

        // Drop any still-running exec child.
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }

        if self.container_started && !self.spec.keep {
            self.remove_container()?;
        }
        Ok(())
    }

    fn report(&self) -> SandboxReport {
        SandboxReport::isolated(
            self.kind,
            self.spec.name.clone(),
            Some(self.spec.image.clone()),
            self.host_pid,
            self.spec.network,
        )
    }
}

impl Drop for DockerSandbox {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

/// True when docker/podman failed because the image is missing or the registry
/// has no such manifest.
fn image_missing_error(stderr: &str, image: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    let image_l = image.to_ascii_lowercase();
    lower.contains("unable to find image")
        || lower.contains("manifest unknown")
        || (lower.contains("not found") && lower.contains(&image_l))
        || lower.contains("pull access denied")
        || lower.contains("does not exist")
        || lower.contains("no such image")
}

fn first_line_of(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::Mount;
    use std::path::PathBuf;

    fn sample_spec() -> SandboxSpec {
        let mut spec =
            SandboxSpec::new("actime-abc123", "ghcr.io/eunomia-bpf/actime-sandbox:latest");
        spec.workdir = PathBuf::from("/workspace");
        spec.mounts.push(Mount::new(".", "/workspace", false));
        spec.mounts
            .push(Mount::new("/etc/passwd", "/etc/passwd", true));
        spec.env
            .push(("ANTHROPIC_API_KEY".into(), "sk-test".into()));
        spec.cpus = Some(2.0);
        spec.memory = Some("8G".into());
        spec.network = NetworkMode::Deny;
        spec
    }

    #[test]
    fn docker_run_argv_exact() {
        let spec = sample_spec();
        let argv = DockerSandbox::build_run_argv("docker", &spec);
        assert_eq!(
            argv,
            vec![
                "docker",
                "run",
                "-d",
                "--name",
                "actime-abc123",
                "-v",
                ".:/workspace",
                "-v",
                "/etc/passwd:/etc/passwd:ro",
                "-w",
                "/workspace",
                "-e",
                "ANTHROPIC_API_KEY=sk-test",
                "--cpus",
                "2",
                "--memory",
                "8G",
                "--network",
                "none",
                "ghcr.io/eunomia-bpf/actime-sandbox:latest",
                "sleep",
                "infinity",
            ]
        );
    }

    #[test]
    fn podman_run_argv_uses_binary_name() {
        let mut spec = sample_spec();
        spec.network = NetworkMode::Allow;
        let argv = DockerSandbox::build_run_argv("podman", &spec);
        assert_eq!(argv[0], "podman");
        assert!(!argv.iter().any(|a| a == "--network"));
    }

    #[test]
    fn docker_run_argv_egress_uses_bridge() {
        let mut spec = sample_spec();
        spec.network = NetworkMode::Egress;
        let argv = DockerSandbox::build_run_argv("docker", &spec);
        let net_pos = argv.iter().position(|a| a == "--network").unwrap();
        assert_eq!(argv[net_pos + 1], "bridge");
    }

    #[test]
    fn docker_exec_argv_exact() {
        let argv = DockerSandbox::build_exec_argv(
            "docker",
            "actime-abc123",
            &["claude".into(), "--version".into()],
        );
        assert_eq!(
            argv,
            vec![
                "docker",
                "exec",
                "-i",
                "actime-abc123",
                "claude",
                "--version",
            ]
        );
    }

    #[test]
    fn docker_inspect_pid_argv_exact() {
        let argv = DockerSandbox::build_inspect_pid_argv("docker", "actime-abc123");
        assert_eq!(
            argv,
            vec![
                "docker",
                "inspect",
                "--format",
                "{{.State.Pid}}",
                "actime-abc123",
            ]
        );
    }

    #[test]
    fn evidence_target_format() {
        let spec = SandboxSpec::new("actime-x", "img");
        let sb = DockerSandbox::new("docker", Backend::Docker, spec);
        assert_eq!(sb.evidence_target().as_deref(), Some("docker://actime-x"));

        let spec = SandboxSpec::new("actime-y", "img");
        let sb = DockerSandbox::new("podman", Backend::Podman, spec);
        assert_eq!(sb.evidence_target().as_deref(), Some("podman://actime-y"));
    }

    #[test]
    fn fractional_cpus_formatting() {
        let mut spec = SandboxSpec::new("n", "img");
        spec.cpus = Some(1.5);
        let argv = DockerSandbox::build_run_argv("docker", &spec);
        let pos = argv.iter().position(|a| a == "--cpus").unwrap();
        assert_eq!(argv[pos + 1], "1.5");
    }

    #[test]
    fn image_missing_error_recognizes_common_docker_messages() {
        let img = "ghcr.io/eunomia-bpf/actime-sandbox:latest";
        assert!(image_missing_error(
            "Unable to find image 'ghcr.io/eunomia-bpf/actime-sandbox:latest' locally\n\
             docker: Error response from daemon: manifest unknown",
            img
        ));
        assert!(image_missing_error("Error: no such image", img));
        assert!(!image_missing_error(
            "permission denied while trying to connect",
            img
        ));
    }
}
