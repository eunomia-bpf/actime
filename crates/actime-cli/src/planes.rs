//! The policy, evidence, and history planes.
//!
//! Each plane wraps one external engine as a child process and reports whether
//! it came up. Nothing here returns `Err` for an environment problem: a missing
//! binary, no root, or an unsupported kernel produces a [`Outcome::Disabled`]
//! with a human-readable reason, which the caller records in the manifest. The
//! single exception is handled by the caller, not here: `policy.mode: enforce`
//! turns a non-active policy plane into an aborted run (fail closed).
//!
//! Every external engine invocation is bounded: startup is polled for a short
//! settle window, and foreground tools like `akeep commit` have a hard timeout.
//! A hung engine must never hang an Actime run.

use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// How long to wait for an engine child to stay alive after spawn before
/// treating the plane as active.
const ENGINE_SETTLE: Duration = Duration::from_millis(400);

/// Hard cap on `akeep commit` / `akeep init`. A locked or hung vault must not
/// block the run exit path. Keep this short: the run is already done and the
/// user is waiting on a report.
const AKEEP_TIMEOUT: Duration = Duration::from_secs(8);

/// Whether a plane came up, and why not when it did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The plane is running. The string is shown in the report, e.g.
    /// `"actplane 0.1.8 · coding-agent-baseline · enforce"`.
    Active(String),
    /// The plane is running with reduced capability.
    Degraded(String),
    /// The plane is not running.
    Disabled(String),
}

impl Outcome {
    /// True when the plane is doing its job.
    pub fn is_active(&self) -> bool {
        matches!(self, Outcome::Active(_))
    }

    /// The word shown in the report's Planes section.
    pub fn label(&self) -> &'static str {
        match self {
            Outcome::Active(_) => "active",
            Outcome::Degraded(_) => "degraded",
            Outcome::Disabled(_) => "disabled",
        }
    }

    /// The explanatory detail.
    pub fn detail(&self) -> &str {
        match self {
            Outcome::Active(d) | Outcome::Degraded(d) | Outcome::Disabled(d) => d,
        }
    }
}

/// True when the current process is already root.
pub fn is_root() -> bool {
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// True when a fully interactive sudo password prompt is safe.
///
/// Requires both stdin and stderr to be terminals, and refuses when
/// `NO_COLOR` is set or `ACTIME_NONINTERACTIVE` is set. Non-interactive runs
/// (CI, pipes, agent-driven invocations) must never hang on a password.
pub fn may_prompt_sudo() -> bool {
    if std::env::var_os("ACTIME_NONINTERACTIVE").is_some() {
        return false;
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Build a command, prefixing `sudo` when the eBPF planes need privileges we
/// do not have.
///
/// Interactive password prompts are allowed only when [`may_prompt_sudo`] is
/// true. Otherwise `sudo -n` is used so a non-interactive run fails fast
/// instead of hanging forever on a prompt nobody will answer.
pub fn privileged(program: &Path, args: &[String]) -> Command {
    if is_root() {
        let mut c = Command::new(program);
        c.args(args);
        return c;
    }
    let mut c = Command::new("sudo");
    if !may_prompt_sudo() {
        // Non-interactive: never block on a password prompt.
        c.arg("-n");
    }
    c.arg("-E").arg(program).args(args);
    c
}

/// Reason string for a plane that needs privileges we cannot get.
fn needs_root_reason(engine: &str) -> String {
    format!(
        "{engine} needs root or CAP_BPF; re-run with `sudo actime …`, grant CAP_BPF, or use `--policy observe`"
    )
}

/// The policy plane: ActPlane, attached to the agent process tree.
pub struct PolicyPlane {
    child: Option<Child>,
    /// How the plane came up.
    pub outcome: Outcome,
    /// The path the composed policy was written to, when one was composed.
    pub policy_file: Option<PathBuf>,
}

/// Everything the policy plane needs to start.
pub struct PolicyPlaneSpec<'a> {
    /// Resolved path to the `actplane` binary, when it was found.
    pub binary: Option<&'a Path>,
    /// Version string for the report.
    pub version: Option<&'a str>,
    /// `off`, `observe`, or `enforce`.
    pub mode: &'a str,
    /// The composed DSL, with `${WORKSPACE}` already substituted.
    pub dsl: &'a str,
    /// Human-readable list of the packs in play, for the report.
    pub packs: &'a str,
    /// Where the generated ActPlane policy YAML is written.
    pub policy_yaml: PathBuf,
    /// Where ActPlane appends one JSON violation per line.
    pub violations: PathBuf,
    /// Where ActPlane writes corrective feedback the agent can read.
    pub feedback: PathBuf,
    /// Where ActPlane writes its audit log.
    pub audit: PathBuf,
    /// Host pid of the process tree root to attach to.
    ///
    /// Ignored when [`Self::wrap_command`] is true (`actime run` launches the
    /// agent under `actplane run` instead of attach-after-spawn).
    pub host_pid: Option<i32>,
    /// When true, only prepare the policy files. The agent is later launched
    /// under `actplane run` so enforcement is launch-time, not post-hoc.
    pub wrap_command: bool,
    /// Whether corrective feedback is delivered to the agent.
    pub feedback_enabled: bool,
    /// Engine stderr goes here so it does not interleave with agent output.
    pub log: PathBuf,
}

impl PolicyPlane {
    /// Compose the ActPlane policy YAML and start the engine (or prepare wrap).
    ///
    /// Never fails for an environment reason; inspect [`PolicyPlane::outcome`].
    pub fn start(spec: PolicyPlaneSpec<'_>) -> PolicyPlane {
        let mut plane = PolicyPlane {
            child: None,
            outcome: Outcome::Disabled(String::new()),
            policy_file: None,
        };

        if spec.mode == "off" {
            plane.outcome = Outcome::Disabled("policy.mode is off".into());
            return plane;
        }

        let Some(binary) = spec.binary else {
            plane.outcome =
                Outcome::Disabled("actplane is not installed; run `cargo install actplane`".into());
            return plane;
        };

        // ActPlane < 0.1.8 has no `attach` and a different run surface.
        if let Some(ver) = spec.version {
            if actime_core::components::compare_semver(ver, "0.1.8") < 0 {
                plane.outcome = Outcome::Disabled(format!(
                    "actplane {ver} is below 0.1.8; run `cargo install actplane`"
                ));
                let _ = write_policy_yaml(&spec);
                plane.policy_file = Some(spec.policy_yaml.clone());
                return plane;
            }
        }

        // The generated YAML is written even when the engine cannot run, so the
        // run record always shows exactly which policy was intended.
        match write_policy_yaml(&spec) {
            Ok(()) => plane.policy_file = Some(spec.policy_yaml.clone()),
            Err(e) => {
                plane.outcome = Outcome::Disabled(format!("writing the policy file failed: {e}"));
                return plane;
            }
        }

        // Wrap path (DESIGN.md): prepare only; the agent is launched under
        // `actplane run -- <cmd>` so enforcement is launch-time, not post-hoc.
        //
        // Outcome is provisional Active: the policy is not verifiably installed
        // until `actplane run` succeeds. Callers must run
        // [`PolicyPlane::confirm_install_from_log`] after the wrap exits and
        // reclassify (and fail closed in enforce) when the engine log shows an
        // install failure. Never leave a post-run report as Active if install
        // failed — that is the same class of dishonesty as a false evidence plane.
        if spec.wrap_command {
            let version = spec.version.unwrap_or("unknown");
            plane.outcome = Outcome::Active(format!(
                "actplane {version} · {} · {} · wrap{}",
                spec.packs,
                spec.mode,
                if spec.feedback_enabled {
                    ""
                } else {
                    " · feedback off"
                }
            ));
            return plane;
        }

        let Some(pid) = spec.host_pid else {
            plane.outcome = Outcome::Disabled("no host pid to attach the policy plane to".into());
            return plane;
        };

        let args = vec![
            "--policy".to_string(),
            spec.policy_yaml.display().to_string(),
            "attach".to_string(),
            "--pid".to_string(),
            pid.to_string(),
        ];

        let log = match File::create(&spec.log) {
            Ok(f) => f,
            Err(e) => {
                plane.outcome = Outcome::Disabled(format!("cannot open the engine log: {e}"));
                return plane;
            }
        };

        let mut cmd = privileged(binary, &args);
        // ActPlane 0.1.8+: request the widest hook budget available so file
        // and path features can load when the engine supports them.
        cmd.env("ACTPLANE_HOOK_PROFILE", "full");
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log));

        match cmd.spawn() {
            Ok(mut child) => match settle_engine(&mut child, ENGINE_SETTLE) {
                Settle::Alive => {
                    // ActPlane can stay up after printing a fatal install
                    // error (feature-budget mismatch). Treat a log Error as
                    // a failed start so we never claim Active falsely.
                    if let Some(err) = engine_log_error(&spec.log) {
                        stop_child(&mut Some(child), Duration::from_millis(500));
                        plane.outcome =
                            Outcome::Disabled(format!("actplane failed to load policy: {err}"));
                    } else {
                        let version = spec.version.unwrap_or("unknown");
                        plane.child = Some(child);
                        plane.outcome = Outcome::Active(format!(
                            "actplane {version} · {} · {}{}",
                            spec.packs,
                            spec.mode,
                            if spec.feedback_enabled {
                                ""
                            } else {
                                " · feedback off"
                            }
                        ));
                    }
                }
                Settle::Exited { code, stderr_tail } => {
                    plane.outcome = Outcome::Disabled(engine_exit_reason(
                        "actplane",
                        code,
                        &stderr_tail,
                        &spec.log,
                    ));
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                plane.outcome = Outcome::Disabled(needs_root_reason("actplane"));
            }
            Err(e) => {
                plane.outcome = Outcome::Disabled(format!("starting actplane failed: {e}"));
            }
        }
        plane
    }

    /// Build argv that launches `agent` under `actplane run` (host wrap path).
    ///
    /// Prefixes `sudo -n -E` when not root and a password prompt is not safe.
    /// Invokes `actplane --policy <file> run -- <cmd>` so the engine installs
    /// the composed policy for that run (not a post-hoc attach delta alone).
    pub fn wrap_argv(actplane: &Path, policy_yaml: &Path, agent: &[String]) -> Vec<String> {
        let mut v = Vec::new();
        if !is_root() {
            v.push("sudo".into());
            if !may_prompt_sudo() {
                v.push("-n".into());
            }
            v.push("-E".into());
        }
        v.push(actplane.display().to_string());
        v.push("--policy".into());
        v.push(policy_yaml.display().to_string());
        v.push("run".into());
        v.push("--".into());
        v.extend(agent.iter().cloned());
        v
    }

    /// After a wrap launch, re-read the engine log and demote Active → Disabled
    /// when the policy was never installed.
    ///
    /// Returns `true` when the plane remains active (or was already non-active).
    /// Returns `false` when an install failure was found and the outcome was
    /// reclassified to Disabled. Callers in `enforce` mode must fail closed.
    pub fn confirm_install_from_log(&mut self, log: &Path) -> bool {
        if !self.outcome.is_active() {
            return true;
        }
        if let Some(err) = engine_log_error(log) {
            self.outcome = Outcome::Disabled(format!("actplane failed to load policy: {err}"));
            return false;
        }
        true
    }

    /// Stop the engine. Safe to call more than once. No-op for wrap mode.
    ///
    /// Uses a multi-second SIGTERM grace so ActPlane can flush events.jsonl.
    pub fn stop(&mut self) {
        stop_child(&mut self.child, Duration::from_secs(5));
    }
}

/// Assess per-rule enforceability for the composed DSL against this host.
///
/// Delegates to [`actime_core::enforceability`] using the released ActPlane
/// 0.1.8 pinned feature budget. Prefer [`assess_policy_with_compile`] when
/// `actplane compile --json` is available.
pub fn classify_rules(
    dsl: &str,
    install_error: Option<&str>,
) -> Vec<actime_core::RuleEnforceability> {
    actime_core::assess_dsl(
        dsl,
        actime_core::engine_supported_features(None),
        install_error,
    )
}

/// Assess enforceability from an `actplane compile --json` report.
pub fn assess_policy_with_compile(
    compile_json: &serde_json::Value,
    actplane_version: Option<&str>,
    install_error: Option<&str>,
) -> Vec<actime_core::RuleEnforceability> {
    actime_core::assess_compile_json(
        compile_json,
        actime_core::engine_supported_features(actplane_version),
        install_error,
    )
}

/// Write the ActPlane project YAML and the pure DSL companion file.
///
/// - `spec.policy_yaml` (typically `policy.yaml`) is what ActPlane loads.
/// - Sibling `policy.dsl` is the composed DSL alone for humans and diffs.
///
/// ActPlane rejects a `.dsl` *extension* even when the file contents are YAML
/// (`policy: |` …), so the engine file must not use `.dsl`.
fn write_policy_yaml(spec: &PolicyPlaneSpec<'_>) -> Result<()> {
    if let Some(parent) = spec.policy_yaml.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut f = File::create(&spec.policy_yaml)
        .with_context(|| format!("creating {}", spec.policy_yaml.display()))?;

    writeln!(f, "# Generated by actime. Edit actime.yaml, not this file.")?;
    writeln!(f, "version: 1")?;
    writeln!(f, "feedback:")?;
    writeln!(f, "  path: {}", yaml_scalar(&spec.feedback))?;
    writeln!(f, "  audit: {}", yaml_scalar(&spec.audit))?;
    writeln!(f, "  events: {}", yaml_scalar(&spec.violations))?;
    writeln!(f, "policy: |")?;
    for line in spec.dsl.lines() {
        if line.is_empty() {
            writeln!(f)?;
        } else {
            writeln!(f, "  {line}")?;
        }
    }

    // Companion pure-DSL file next to the YAML project file.
    let dsl_path = spec
        .policy_yaml
        .parent()
        .map(|p| p.join("policy.dsl"))
        .unwrap_or_else(|| PathBuf::from("policy.dsl"));
    std::fs::write(&dsl_path, spec.dsl)
        .with_context(|| format!("writing {}", dsl_path.display()))?;
    Ok(())
}

/// Quote a path for a YAML scalar.
fn yaml_scalar(p: &Path) -> String {
    format!("{:?}", p.display().to_string())
}

/// The evidence plane: AgentSight, recording the agent process tree.
pub struct EvidencePlane {
    child: Option<Child>,
    /// How the plane came up.
    pub outcome: Outcome,
}

/// Everything the evidence plane needs to start.
pub struct EvidencePlaneSpec<'a> {
    /// Resolved path to the `agentsight` binary, when it was found.
    pub binary: Option<&'a Path>,
    /// Version string for the report.
    pub version: Option<&'a str>,
    /// Whether the evidence plane is wanted at all.
    pub enabled: bool,
    /// AgentSight `--binary-path` form when applicable (`docker://…`, `k8s://…`).
    pub target: Option<String>,
    /// Host pid to attach to when there is no scheme target.
    pub host_pid: Option<i32>,
    /// Where the SQLite evidence store is written.
    pub db: PathBuf,
    /// Engine stderr goes here.
    pub log: PathBuf,
}

impl EvidencePlane {
    /// Attach AgentSight to the process tree. Never fails for an environment reason.
    pub fn start(spec: EvidencePlaneSpec<'_>) -> EvidencePlane {
        let mut plane = EvidencePlane {
            child: None,
            outcome: Outcome::Disabled(String::new()),
        };

        if !spec.enabled {
            plane.outcome = Outcome::Disabled("evidence.enabled is false".into());
            return plane;
        }

        let Some(binary) = spec.binary else {
            plane.outcome = Outcome::Disabled(
                "agentsight is not installed; run `cargo install agentsight`".into(),
            );
            return plane;
        };

        let mut args = vec![
            "record".to_string(),
            "--no-server".to_string(),
            "--db".to_string(),
            spec.db.display().to_string(),
        ];
        match (&spec.target, spec.host_pid) {
            (Some(target), _) => {
                args.push("--binary-path".to_string());
                args.push(target.clone());
            }
            (None, Some(pid)) => {
                args.push("--pid".to_string());
                args.push(pid.to_string());
            }
            (None, None) => {
                plane.outcome = Outcome::Disabled(
                    "no host pid or container target to attach evidence to".into(),
                );
                return plane;
            }
        }

        let log = match File::create(&spec.log) {
            Ok(f) => f,
            Err(e) => {
                plane.outcome = Outcome::Disabled(format!("cannot open the engine log: {e}"));
                return plane;
            }
        };

        let mut cmd = privileged(binary, &args);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log));

        match cmd.spawn() {
            Ok(mut child) => match settle_engine(&mut child, ENGINE_SETTLE) {
                Settle::Alive => {
                    plane.child = Some(child);
                    plane.outcome = Outcome::Active(format!(
                        "agentsight {}",
                        spec.version.unwrap_or("unknown")
                    ));
                }
                Settle::Exited { code, stderr_tail } => {
                    plane.outcome = Outcome::Disabled(engine_exit_reason(
                        "agentsight",
                        code,
                        &stderr_tail,
                        &spec.log,
                    ));
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                plane.outcome = Outcome::Disabled(needs_root_reason("agentsight"));
            }
            Err(e) => {
                plane.outcome = Outcome::Disabled(format!("starting agentsight failed: {e}"));
            }
        }
        plane
    }

    /// Stop the engine. Safe to call more than once.
    ///
    /// Agentsight is stopped with a short grace; it is not the source of
    /// policy violations, and a long wait would slow every short run.
    pub fn stop(&mut self) {
        stop_child(&mut self.child, Duration::from_secs(1));
    }
}

/// The history plane: Akeep, committing agent session history after the run.
pub struct HistoryPlane;

impl HistoryPlane {
    /// Commit the agent's session history and return the commit id.
    ///
    /// Runs after the agent has exited, so this is a foreground call rather
    /// than a managed child. Bounded by [`AKEEP_TIMEOUT`]: a hung or locked
    /// vault degrades the history plane and never blocks the run exit path.
    pub fn commit(
        binary: Option<&Path>,
        enabled: bool,
        message: &str,
        log: &Path,
    ) -> (Outcome, Option<String>) {
        if !enabled {
            return (Outcome::Disabled("history.enabled is false".into()), None);
        }
        let Some(binary) = binary else {
            return (
                Outcome::Disabled("akeep is not installed; run `cargo install akeep`".into()),
                None,
            );
        };

        // Akeep needs an initialized repository. `init` on an existing vault is
        // a no-op; bound it so a stuck vault cannot hang the run either.
        match run_with_timeout(
            Command::new(binary)
                .arg("init")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
            AKEEP_TIMEOUT,
        ) {
            Timed::Timeout => {
                return (
                    Outcome::Degraded(format!(
                        "akeep init timed out after {}; history left uncommitted",
                        format_secs(AKEEP_TIMEOUT)
                    )),
                    None,
                );
            }
            Timed::Error(e) => {
                return (
                    Outcome::Disabled(format!("running akeep init failed: {e}")),
                    None,
                );
            }
            Timed::Finished(_) => {}
        }

        let mut commit_cmd = Command::new(binary);
        commit_cmd
            .args(["commit", "-m", message])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        match run_with_timeout(&mut commit_cmd, AKEEP_TIMEOUT) {
            Timed::Finished(out) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout).to_string();
                let commit = parse_commit_id(&text);
                append_log(log, &text);
                (
                    Outcome::Active(format!(
                        "akeep{}",
                        commit
                            .as_deref()
                            .map(|c| format!(" · commit {c}"))
                            .unwrap_or_default()
                    )),
                    commit,
                )
            }
            Timed::Finished(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                append_log(log, &stderr);
                (
                    Outcome::Degraded(format!("akeep commit failed: {}", first_line(&stderr, 160))),
                    None,
                )
            }
            Timed::Timeout => (
                Outcome::Degraded(format!(
                    "akeep commit timed out after {}; history left uncommitted",
                    format_secs(AKEEP_TIMEOUT)
                )),
                None,
            ),
            Timed::Error(e) => (
                Outcome::Disabled(format!("running akeep failed: {e}")),
                None,
            ),
        }
    }
}

/// Outcome of waiting on a short-lived child with a deadline.
#[derive(Debug)]
enum Timed {
    Finished(std::process::Output),
    Timeout,
    Error(std::io::Error),
}

/// Spawn `cmd`, wait up to `limit`, and kill the child on timeout.
///
/// Never blocks longer than approximately `limit` plus a small grace period.
fn run_with_timeout(cmd: &mut Command, limit: Duration) -> Timed {
    use std::io::Read;

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Timed::Error(e),
    };
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut out) = child.stdout.take() {
                    let _ = out.read_to_end(&mut stdout);
                }
                if let Some(mut err) = child.stderr.take() {
                    let _ = err.read_to_end(&mut stderr);
                }
                return Timed::Finished(std::process::Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let pid = child.id() as i32;
                    // SAFETY: kill a positive pid; ESRCH is fine.
                    unsafe {
                        libc::kill(pid, libc::SIGTERM);
                    }
                    std::thread::sleep(Duration::from_millis(150));
                    // Prefer SIGKILL quickly; a vault stuck in uninterruptible
                    // sleep (D state) will not reap, so never call blocking
                    // wait() — that would hang the whole run again.
                    let _ = child.kill();
                    let reap_deadline = Instant::now() + Duration::from_millis(400);
                    loop {
                        match child.try_wait() {
                            Ok(Some(_)) => break,
                            Ok(None) if Instant::now() < reap_deadline => {
                                std::thread::sleep(Duration::from_millis(40));
                            }
                            _ => {
                                // Leave a possible zombie rather than hang.
                                // Child::drop would call wait() and block forever
                                // on a D-state process, so forget it.
                                std::mem::forget(child);
                                return Timed::Timeout;
                            }
                        }
                    }
                    return Timed::Timeout;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Timed::Error(e),
        }
    }
}

/// Whether a just-spawned engine is still alive after a short settle window.
enum Settle {
    Alive,
    Exited {
        code: Option<i32>,
        stderr_tail: String,
    },
}

/// Poll `child` for up to `window`. If it exits, capture a short stderr tail
/// from any log the caller already redirected (we only know the exit code
/// here; the reason helper re-reads the log path when provided).
fn settle_engine(child: &mut Child, window: Duration) -> Settle {
    let deadline = Instant::now() + window;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Settle::Exited {
                    code: status.code(),
                    stderr_tail: String::new(),
                };
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    return Settle::Alive;
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(_) => {
                return Settle::Exited {
                    code: None,
                    stderr_tail: String::new(),
                };
            }
        }
    }
}

fn engine_exit_reason(engine: &str, code: Option<i32>, _tail: &str, log: &Path) -> String {
    let log_tail = std::fs::read_to_string(log)
        .ok()
        .map(|s| {
            s.lines()
                .rev()
                .take(4)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let detail = first_line(&log_tail, 160);
    let code_s = code
        .map(|c| format!("exit {c}"))
        .unwrap_or_else(|| "exited".into());

    // Common: sudo -n refused a password, or the engine needs CAP_BPF.
    let lower = detail.to_ascii_lowercase();
    if lower.contains("password")
        || lower.contains("a password is required")
        || lower.contains("sudo:") && lower.contains("password")
        || code == Some(1) && detail.is_empty()
    {
        return needs_root_reason(engine);
    }
    if detail.is_empty() {
        format!("{engine} exited immediately ({code_s})")
    } else {
        format!("{engine} exited immediately ({code_s}): {detail}")
    }
}

/// Pull a fatal `Error: "..."` line out of an engine log, if one is present.
fn engine_log_error(log: &Path) -> Option<String> {
    let text = std::fs::read_to_string(log).ok()?;
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed
            .strip_prefix("Error:")
            .or_else(|| trimmed.strip_prefix("error:"))
        else {
            continue;
        };
        let msg = rest.trim().trim_matches('"').trim();
        if !msg.is_empty() {
            return Some(first_line(msg, 200));
        }
    }
    None
}

fn format_secs(d: Duration) -> String {
    let s = d.as_secs();
    if s == 1 {
        "1s".into()
    } else {
        format!("{s}s")
    }
}

/// Pull a commit-id-looking token out of `akeep commit` output.
fn parse_commit_id(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|t| {
            t.len() >= 7
                && t.len() <= 64
                && t.chars().all(|c| c.is_ascii_hexdigit())
                && t.chars().any(|c| c.is_ascii_digit())
        })
        .map(|s| s.to_string())
}

/// First line of `text`, truncated to `max` characters.
fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let cut: String = line.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

fn append_log(path: &Path, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{text}");
    }
}

/// Terminate a managed engine, escalating from SIGTERM to SIGKILL.
///
/// The engines usually run under `sudo`, so the direct child may be the `sudo`
/// process rather than the engine; SIGTERM propagates and the engines tear the
/// eBPF programs down on exit.
///
/// `term_grace` is how long to wait after SIGTERM before SIGKILL. ActPlane needs
/// multi-second grace to flush events.jsonl; agentsight can be shorter. Still
/// bounded: a stuck engine must not hang the run forever.
fn stop_child(slot: &mut Option<Child>, term_grace: Duration) {
    let Some(mut child) = slot.take() else {
        return;
    };
    let pid = child.id() as i32;
    // SAFETY: `kill` with a valid pid; ESRCH on an already-exited child is fine.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let term_deadline = Instant::now() + term_grace;
    while Instant::now() < term_deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return,
        }
    }
    let _ = child.kill();
    // Brief non-blocking reap; never call blocking wait() — actplane/agentsight
    // can sit in D-state on eBPF teardown and hang the whole product.
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(40));
            }
            _ => {
                std::mem::forget(child);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_labels_and_details() {
        let a = Outcome::Active("actplane 0.1.8".into());
        assert!(a.is_active());
        assert_eq!(a.label(), "active");
        assert_eq!(a.detail(), "actplane 0.1.8");

        let d = Outcome::Disabled("no root".into());
        assert!(!d.is_active());
        assert_eq!(d.label(), "disabled");

        assert_eq!(Outcome::Degraded("partial".into()).label(), "degraded");
    }

    #[test]
    fn policy_plane_is_disabled_when_mode_is_off() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plane = PolicyPlane::start(spec_in(&dir, "off", None));
        assert!(matches!(plane.outcome, Outcome::Disabled(_)));
        assert!(plane.outcome.detail().contains("off"));
    }

    #[test]
    fn policy_plane_names_the_install_command_when_actplane_is_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plane = PolicyPlane::start(spec_in(&dir, "enforce", None));
        assert!(plane.outcome.detail().contains("cargo install actplane"));
    }

    #[test]
    fn policy_plane_refuses_actplane_below_attach_support() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binary = PathBuf::from("/nonexistent/actplane");
        let mut spec = spec_in(&dir, "observe", Some(&binary));
        spec.version = Some("0.1.5");
        let plane = PolicyPlane::start(spec);
        assert!(
            plane.outcome.detail().contains("0.1.5"),
            "detail={}",
            plane.outcome.detail()
        );
        assert!(
            plane.outcome.detail().contains("0.1.8") || plane.outcome.detail().contains("attach"),
            "detail={}",
            plane.outcome.detail()
        );
        assert!(!plane.outcome.is_active());
    }

    #[test]
    fn policy_yaml_embeds_the_dsl_and_the_event_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binary = PathBuf::from("/nonexistent/actplane");
        let mut spec = spec_in(&dir, "enforce", Some(&binary));
        spec.host_pid = None; // stop before spawning; the YAML is still written
        let plane = PolicyPlane::start(spec);

        let yaml = std::fs::read_to_string(dir.path().join("policy.yaml")).expect("read");
        assert!(yaml.contains("policy: |"));
        assert!(yaml.contains("  rule demo:"));
        assert!(yaml.contains("violations.jsonl"));
        assert!(yaml.contains("feedback.txt"));
        // The DSL is indented into the block scalar, not inlined.
        assert!(!yaml.contains("\nrule demo:"));
        assert!(plane.policy_file.is_some());

        // Pure DSL companion must exist and must NOT be YAML-wrapped.
        let dsl = std::fs::read_to_string(dir.path().join("policy.dsl")).expect("dsl");
        assert!(dsl.contains("rule demo:"));
        assert!(!dsl.contains("policy: |"));
    }

    #[test]
    fn policy_engine_file_must_not_use_dsl_extension() {
        // Regression: actplane rejects `.dsl` even when contents are YAML.
        let dir = tempfile::tempdir().expect("tempdir");
        let binary = PathBuf::from("/nonexistent/actplane");
        let mut spec = spec_in(&dir, "observe", Some(&binary));
        spec.host_pid = None;
        let plane = PolicyPlane::start(spec);
        let path = plane.policy_file.expect("policy file written");
        assert!(
            path.extension().and_then(|e| e.to_str()) == Some("yaml"),
            "engine policy file must be .yaml, got {}",
            path.display()
        );
        assert!(!path.to_string_lossy().ends_with(".dsl"));
    }

    #[test]
    fn engine_log_error_extracts_fatal_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("e.log");
        std::fs::write(
            &log,
            "some banner\nError: \"install policy in domain 1: runtime policy delta requires features\"\n",
        )
        .expect("write");
        let err = engine_log_error(&log).expect("error");
        assert!(err.contains("install policy"));
        assert!(engine_log_error(dir.path().join("missing").as_path()).is_none());
    }

    #[test]
    fn install_failure_log_never_yields_active() {
        // Regression: an engine that logs an install failure must never leave
        // the policy plane as Active — same honesty class as the evidence plane.
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("policy-engine.log");
        std::fs::write(
            &log,
            "Error: \"install policy in domain 969630630: runtime policy delta requires features not \
             enabled when the eBPF engine was loaded: path contains matches, path suffix matches. \
             needed=0x53, supported=0x3f0, missing=0x3\"\n",
        )
        .expect("write");

        // Attach-style: settle sees Alive but log has Error → Disabled.
        // We cannot spawn a real actplane here; exercise confirm_install_from_log
        // on a provisional wrap Active outcome.
        let mut plane = PolicyPlane {
            child: None,
            outcome: Outcome::Active("actplane 0.1.8 · no-secret-egress · enforce · wrap".into()),
            policy_file: Some(dir.path().join("policy.yaml")),
        };
        assert!(plane.outcome.is_active());
        let still_ok = plane.confirm_install_from_log(&log);
        assert!(!still_ok, "install failure must reclassify");
        assert!(
            !plane.outcome.is_active(),
            "must not be Active after install failure, got {:?}",
            plane.outcome
        );
        assert!(
            matches!(plane.outcome, Outcome::Disabled(_)),
            "expected Disabled, got {:?}",
            plane.outcome
        );
        assert!(
            plane.outcome.detail().contains("install policy")
                || plane.outcome.detail().contains("failed to load policy"),
            "detail={}",
            plane.outcome.detail()
        );

        // Clean log → Active preserved.
        let clean = dir.path().join("clean.log");
        std::fs::write(&clean, "ActPlane: running pid 1 under COMMAND label\n").expect("write");
        let mut ok_plane = PolicyPlane {
            child: None,
            outcome: Outcome::Active("wrap".into()),
            policy_file: None,
        };
        assert!(ok_plane.confirm_install_from_log(&clean));
        assert!(ok_plane.outcome.is_active());
    }

    #[test]
    fn classify_rules_marks_file_rules_unenforceable_and_install_error_all() {
        let dsl = r#"
source AGENT = exec "**/claude"
rule destructive-vcs:
  kill exec "git" "--force" if AGENT
  because "no force"
rule system-fence:
  block write file "/etc/**" if AGENT
  because "no system writes"
"#;
        let rows = classify_rules(dsl, Some("path contains matches missing"));
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| !r.enforceable));
        assert!(rows[0].reason.contains("path contains"));

        let ok = classify_rules(dsl, None);
        assert_eq!(ok.len(), 2);
        assert!(
            ok.iter()
                .any(|r| r.name == "destructive-vcs" && r.enforceable),
            "{ok:?}"
        );
        assert!(
            ok.iter()
                .any(|r| r.name == "system-fence" && !r.enforceable && r.reason.contains("write")),
            "{ok:?}"
        );
    }

    #[test]
    fn enforce_preflight_rejects_unenforceable_subset() {
        // Mirror the run-path gate: if any rule is not enforceable, enforce
        // must fail closed before the agent starts.
        let dsl = r#"
source AGENT = exec "**/claude"
source SECRET = file "**/.env"
rule no-secret-egress:
  kill connect endpoint "*" if AGENT and SECRET
  because "no egress"
"#;
        let rows = classify_rules(dsl, None);
        let bad: Vec<_> = rows.iter().filter(|r| !r.enforceable).collect();
        assert!(!bad.is_empty(), "expected unenforceable rules: {rows:?}");
        // The fail-closed message names rules and reasons.
        let msg = bad
            .iter()
            .map(|r| format!("• {} — {}", r.name, r.reason))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(msg.contains("no-secret-egress"));
        assert!(msg.contains("missing features") || msg.contains("path"));
    }

    #[test]
    fn evidence_plane_reports_why_it_cannot_attach() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binary = PathBuf::from("/nonexistent/agentsight");
        let plane = EvidencePlane::start(EvidencePlaneSpec {
            binary: Some(&binary),
            version: Some("0.2.66"),
            enabled: true,
            target: None,
            host_pid: None,
            db: dir.path().join("evidence.db"),
            log: dir.path().join("evidence.log"),
        });
        assert!(plane.outcome.detail().contains("no host pid"));
    }

    #[test]
    fn evidence_plane_respects_the_enabled_flag() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plane = EvidencePlane::start(EvidencePlaneSpec {
            binary: None,
            version: None,
            enabled: false,
            target: Some("docker://x".into()),
            host_pid: Some(1),
            db: dir.path().join("evidence.db"),
            log: dir.path().join("evidence.log"),
        });
        assert!(plane.outcome.detail().contains("evidence.enabled"));
    }

    #[test]
    fn history_plane_is_disabled_without_akeep() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (outcome, commit) =
            HistoryPlane::commit(None, true, "actime run", &dir.path().join("h.log"));
        assert!(outcome.detail().contains("cargo install akeep"));
        assert!(commit.is_none());
    }

    #[test]
    fn commit_id_is_picked_out_of_akeep_output() {
        assert_eq!(
            parse_commit_id("committed 7f2a91c (12 files)").as_deref(),
            Some("7f2a91c")
        );
        assert_eq!(parse_commit_id("nothing to commit"), None);
    }

    #[test]
    fn first_line_truncates_with_an_ellipsis() {
        assert_eq!(first_line("short", 10), "short");
        assert_eq!(first_line("abcdefghijk", 5), "abcd…");
        assert_eq!(first_line("one\ntwo", 10), "one");
    }

    #[test]
    fn privileged_uses_sudo_only_when_not_root() {
        let cmd = privileged(Path::new("/bin/true"), &["x".into()]);
        let program = cmd.get_program().to_string_lossy().to_string();
        if is_root() {
            assert_eq!(program, "/bin/true");
        } else {
            assert_eq!(program, "sudo");
            let args: Vec<String> = cmd
                .get_args()
                .map(|a| a.to_string_lossy().to_string())
                .collect();
            assert!(args.contains(&"-E".to_string()));
            assert!(args.contains(&"/bin/true".to_string()));
        }
    }

    #[test]
    fn privileged_never_prompts_when_noninteractive_env_is_set() {
        // Regression: a non-interactive run must pass `sudo -n` so it cannot
        // hang forever on a password prompt.
        std::env::set_var("ACTIME_NONINTERACTIVE", "1");
        let cmd = privileged(Path::new("/bin/true"), &["x".into()]);
        std::env::remove_var("ACTIME_NONINTERACTIVE");
        if is_root() {
            return;
        }
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            args.iter().any(|a| a == "-n"),
            "expected sudo -n under ACTIME_NONINTERACTIVE, got {args:?}"
        );
    }

    #[test]
    fn run_with_timeout_kills_a_sleeping_child() {
        let start = Instant::now();
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let result = run_with_timeout(&mut cmd, Duration::from_millis(300));
        assert!(
            matches!(result, Timed::Timeout),
            "expected timeout, got non-timeout"
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timeout wait took too long: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn run_with_timeout_returns_finished_for_true() {
        let mut cmd = Command::new("/bin/true");
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match run_with_timeout(&mut cmd, Duration::from_secs(5)) {
            Timed::Finished(out) => assert!(out.status.success()),
            other => panic!("expected Finished, got non-success: {other:?}"),
        }
    }

    #[test]
    fn history_commit_disabled_when_off() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (outcome, commit) = HistoryPlane::commit(None, false, "msg", &dir.path().join("h.log"));
        assert!(outcome.detail().contains("history.enabled"));
        assert!(commit.is_none());
    }

    fn spec_in<'a>(
        dir: &tempfile::TempDir,
        mode: &'a str,
        binary: Option<&'a Path>,
    ) -> PolicyPlaneSpec<'a> {
        PolicyPlaneSpec {
            binary,
            version: Some("0.1.8"),
            mode,
            dsl: "source AGENT = exec \"**/claude\"\n\nrule demo:\n  notify exec \"git\" if AGENT\n  because \"demo\"\n",
            packs: "coding-agent-baseline",
            policy_yaml: dir.path().join("policy.yaml"),
            violations: dir.path().join("violations.jsonl"),
            feedback: dir.path().join("feedback.txt"),
            audit: dir.path().join("audit.jsonl"),
            host_pid: Some(1),
            wrap_command: false,
            feedback_enabled: true,
            log: dir.path().join("policy.log"),
        }
    }

    #[test]
    fn wrap_argv_includes_policy_and_agent() {
        let argv = PolicyPlane::wrap_argv(
            Path::new("/usr/bin/actplane"),
            Path::new("/tmp/policy.yaml"),
            &["./agent".into(), "arg".into()],
        );
        let joined = argv.join(" ");
        assert!(joined.contains("actplane"));
        assert!(joined.contains("--policy"));
        assert!(joined.contains("/tmp/policy.yaml"));
        assert!(joined.contains("run"));
        assert!(joined.contains("./agent"));
        assert!(joined.contains("arg"));
    }
}
