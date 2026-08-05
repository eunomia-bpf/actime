//! `actime run` and `actime attach` — attach the three planes to a process tree.
//!
//! Actime never creates, starts, stops, or removes a container. `run` launches
//! the agent as a plain host child; `attach` binds to something already running
//! (pid, comm, existing container, or existing pod). See `docs/DESIGN.md`.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use actime_core::config::{CliOverrides, Config, PolicyMode};
use actime_core::evidence::Evidence;
use actime_core::run::{PlaneState, Run, RunStore, TargetReport};
use actime_core::{components::Components, report};

use crate::commands::Context as Ctx;
use crate::embedded;
use crate::planes::{
    EvidencePlane, EvidencePlaneSpec, HistoryPlane, Outcome, PolicyPlane, PolicyPlaneSpec,
};
use crate::ui;

/// Everything `actime run` accepts from the command line.
pub struct RunRequest {
    /// The agent command and its arguments.
    pub argv: Vec<String>,
    /// `--policy`.
    pub policy: Option<String>,
    /// `--no-evidence`.
    pub no_evidence: bool,
    /// `--no-history`.
    pub no_history: bool,
    /// `--fail-on-violation`.
    pub fail_on_violation: bool,
    /// `--timeout`.
    pub timeout: Option<String>,
}

/// Everything `actime attach` accepts from the command line.
pub struct AttachRequest {
    /// `--pid`.
    pub pid: Option<i32>,
    /// `--comm`.
    pub comm: Option<String>,
    /// `--container` (existing Docker/Podman name or id).
    pub container: Option<String>,
    /// `--pod` as `namespace/name`.
    pub pod: Option<String>,
    /// `--policy`.
    pub policy: Option<String>,
}

/// Run an agent as a host child and attach the three planes.
pub fn run(ctx: &Ctx, req: RunRequest) -> Result<i32> {
    let cwd = std::env::current_dir().context("resolving the current directory")?;
    let mut cfg = ctx.load_config(&cwd)?;

    cfg.merge_cli(&CliOverrides {
        policy_mode: match req.policy.as_deref() {
            Some(m) => Some(
                m.parse::<PolicyMode>()
                    .map_err(|e| anyhow::anyhow!("{e}"))?,
            ),
            None => None,
        },
        profile: None,
        no_evidence: req.no_evidence.then_some(true),
        no_history: req.no_history.then_some(true),
    });
    if let Some(t) = &req.timeout {
        cfg.limits.wall_clock = Some(
            actime_core::config::parse_duration(t)
                .with_context(|| format!("parsing --timeout {t}"))?,
        );
    }

    if req.argv.is_empty() {
        bail!("a command is required after `--`");
    }

    let components = Components::detect();
    let store = RunStore::open_default()?;
    let mut run = store.create(&req.argv, &cfg)?;

    for c in components.iter() {
        if let Some(v) = &c.version {
            run.manifest
                .components
                .insert(c.name.to_string(), v.clone());
        }
    }

    run.manifest.target = TargetReport {
        kind: "command".into(),
        spec: Some(req.argv.join(" ")),
        host_pid: None,
        evidence_target: None,
        note: Some("launched as a host child process".into()),
    };

    let outcome = orchestrate_run(ctx, &mut run, &cfg, &components, &req, &cwd);

    let exit = match outcome {
        Ok(exit) => exit,
        Err(e) => {
            run.manifest.ended_at = Some(now_rfc3339());
            let _ = run.save_manifest();
            return Err(e);
        }
    };

    finish(ctx, &mut run, exit, req.fail_on_violation)
}

/// Attach the planes to an already-running process tree.
pub fn attach(ctx: &Ctx, req: AttachRequest) -> Result<i32> {
    let cwd = std::env::current_dir().context("resolving the current directory")?;
    let mut cfg = ctx.load_config(&cwd)?;
    if let Some(mode) = &req.policy {
        cfg.policy.mode = mode.parse().map_err(|e| anyhow::anyhow!("{e}"))?;
    }

    let resolved = resolve_attach_target(&req)?;
    let components = Components::detect();
    let store = RunStore::open_default()?;
    let argv = vec![format!("attach:{}", resolved.label)];
    let mut run = store.create(&argv, &cfg)?;

    for c in components.iter() {
        if let Some(v) = &c.version {
            run.manifest
                .components
                .insert(c.name.to_string(), v.clone());
        }
    }

    run.manifest.target = TargetReport {
        kind: resolved.kind.clone(),
        spec: Some(resolved.label.clone()),
        host_pid: Some(resolved.host_pid),
        evidence_target: resolved.evidence_target.clone(),
        note: Some("attached to an already-running process tree; Actime did not create it".into()),
    };

    let workspace = cwd.display().to_string();
    let dsl = compose(&cfg, &workspace)?;
    let packs = packs_label(&cfg);
    let actplane_dir = run.dir.join("actplane");
    let _ = std::fs::create_dir_all(&actplane_dir);

    let mut policy = PolicyPlane::start(PolicyPlaneSpec {
        binary: components.actplane.path.as_deref(),
        version: components.actplane.version.as_deref(),
        mode: &cfg.policy.mode.to_string(),
        dsl: &dsl,
        packs: &packs,
        policy_yaml: run.policy_path(),
        violations: run.violations_path(),
        feedback: actplane_dir.join("feedback.txt"),
        audit: actplane_dir.join("audit.jsonl"),
        host_pid: Some(resolved.host_pid),
        wrap_command: false,
        feedback_enabled: cfg.policy.feedback,
        log: run.dir.join("policy-engine.log"),
    });

    if cfg.policy.mode == PolicyMode::Enforce && !policy.outcome.is_active() {
        policy.stop();
        run.manifest.planes.policy = PlaneState::Disabled(policy.outcome.detail().to_string());
        bail!(
            "policy.mode is `enforce` but the policy plane could not start: {}\n\
             \n\
             Actime fails closed rather than attaching unprotected.\n\
             Fix one of:\n\
               • re-run with privileges:  sudo actime attach --pid {}\n\
               • learn first without blocking:  actime attach --policy observe --pid {}\n\
               • diagnose this machine:  actime doctor",
            policy.outcome.detail(),
            resolved.host_pid,
            resolved.host_pid
        );
    }
    run.manifest.planes.policy = to_plane_state(&policy.outcome);

    let mut evidence = EvidencePlane::start(EvidencePlaneSpec {
        binary: components.agentsight.path.as_deref(),
        version: components.agentsight.version.as_deref(),
        enabled: cfg.evidence.enabled,
        target: resolved.evidence_target.clone(),
        host_pid: Some(resolved.host_pid),
        db: run.evidence_db_path(),
        log: run.dir.join("evidence-engine.log"),
    });
    run.manifest.planes.evidence = to_plane_state(&evidence.outcome);
    run.manifest.planes.history = PlaneState::Disabled("attach does not commit history".into());
    let _ = run.save_manifest();

    if !ctx.quiet {
        eprintln!(
            "{}",
            ui::banner(
                run.id.as_str(),
                &format!("{} {}", resolved.kind, resolved.label),
                &cfg.policy.mode.to_string(),
                if evidence.outcome.is_active() {
                    "on"
                } else {
                    "off"
                },
            )
        );
        for (name, outcome) in [("policy", &policy.outcome), ("evidence", &evidence.outcome)] {
            if !outcome.is_active() {
                eprintln!(
                    "{}",
                    ui::warn(&format!(
                        "{name} plane {}: {}",
                        outcome.label(),
                        outcome.detail()
                    ))
                );
            }
        }
        eprintln!(
            "{}",
            ui::note(
                "attach binds future events from this process tree; it cannot reconstruct \
                 what happened before now. Press Ctrl-C to detach."
            )
        );
        eprintln!();
    }

    let started = Instant::now();
    while process_is_alive(resolved.host_pid) {
        std::thread::sleep(Duration::from_millis(500));
    }

    // Engines must exit fully before harvest so events flush to disk.
    policy.stop();
    evidence.stop();
    harvest_actplane_events(&run);

    run.manifest.summary.duration_seconds = started.elapsed().as_secs_f64();
    finish(ctx, &mut run, 0, false)
}

// ---------------------------------------------------------------------------
// run orchestration
// ---------------------------------------------------------------------------

fn orchestrate_run(
    ctx: &Ctx,
    run: &mut Run,
    cfg: &Config,
    components: &Components,
    req: &RunRequest,
    cwd: &Path,
) -> Result<i32> {
    // Policy is composed against the real host workspace: the agent runs in the
    // user's cwd, not a guest mount path.
    let workspace = cwd.display().to_string();
    let dsl = compose(cfg, &workspace)?;
    let packs = packs_label(cfg);
    let actplane_dir = run.dir.join("actplane");
    let _ = std::fs::create_dir_all(&actplane_dir);

    // Host launch always uses actplane wrap when policy is on: attach-after-spawn
    // races short agents. Wrap is launch-time enforcement.
    let wrap_policy = cfg.policy.mode != PolicyMode::Off;
    let mut policy = PolicyPlane::start(PolicyPlaneSpec {
        binary: components.actplane.path.as_deref(),
        version: components.actplane.version.as_deref(),
        mode: &cfg.policy.mode.to_string(),
        dsl: &dsl,
        packs: &packs,
        policy_yaml: run.policy_path(),
        violations: run.violations_path(),
        feedback: actplane_dir.join("feedback.txt"),
        audit: actplane_dir.join("audit.jsonl"),
        host_pid: None,
        wrap_command: wrap_policy,
        feedback_enabled: cfg.policy.feedback,
        log: run.dir.join("policy-engine.log"),
    });

    if cfg.policy.mode == PolicyMode::Enforce && !policy.outcome.is_active() {
        policy.stop();
        run.manifest.planes.policy = PlaneState::Disabled(policy.outcome.detail().to_string());
        bail!(
            "policy.mode is `enforce` but the policy plane could not start: {}\n\
             \n\
             Actime fails closed rather than running an agent unprotected.\n\
             Fix one of:\n\
               • re-run with privileges:  sudo actime run --policy enforce -- <agent>\n\
               • learn first without blocking:  actime run --policy observe -- <agent>\n\
               • diagnose this machine:  actime doctor",
            policy.outcome.detail()
        );
    }
    run.manifest.planes.policy = to_plane_state(&policy.outcome);

    // Evidence attaches after the child exists. For wrap, that is the wrap pid;
    // for plain spawn, the agent pid. Started once we know the pid.
    let mut evidence: Option<EvidencePlane> = None;

    if !ctx.quiet {
        eprintln!(
            "{}",
            ui::banner(
                run.id.as_str(),
                "command",
                &cfg.policy.mode.to_string(),
                if cfg.evidence.enabled { "on" } else { "off" },
            )
        );
        if !policy.outcome.is_active() {
            eprintln!(
                "{}",
                ui::warn(&format!(
                    "policy plane {}: {}",
                    policy.outcome.label(),
                    policy.outcome.detail()
                ))
            );
        }
        eprintln!();
    }

    let _ = run.save_manifest();
    let started = Instant::now();

    let exit = if wrap_policy && policy.outcome.is_active() {
        let Some(bin) = components.actplane.path.as_deref() else {
            bail!("actplane disappeared after policy plane start");
        };
        let (code, wrap_pid) = run_host_policy_wrap(
            bin,
            &run.policy_path(),
            &req.argv,
            &run.dir.join("policy-engine.log"),
            cfg.limits.wall_clock,
            ctx.quiet,
            |wrap_pid| {
                // Attach evidence to the wrap tree as soon as it exists.
                if evidence.is_none() && cfg.evidence.enabled {
                    let plane = EvidencePlane::start(EvidencePlaneSpec {
                        binary: components.agentsight.path.as_deref(),
                        version: components.agentsight.version.as_deref(),
                        enabled: true,
                        target: None,
                        host_pid: Some(wrap_pid),
                        db: run.evidence_db_path(),
                        log: run.dir.join("evidence-engine.log"),
                    });
                    run.manifest.planes.evidence = to_plane_state(&plane.outcome);
                    run.manifest.target.host_pid = Some(wrap_pid);
                    evidence = Some(plane);
                }
            },
        )?;
        run.manifest.target.host_pid = Some(wrap_pid);
        code
    } else {
        // Plain child: no policy wrap.
        let (code, agent_pid) =
            run_plain_child(&req.argv, cfg.limits.wall_clock, ctx.quiet, |agent_pid| {
                run.manifest.target.host_pid = Some(agent_pid);
                if evidence.is_none() {
                    let plane = EvidencePlane::start(EvidencePlaneSpec {
                        binary: components.agentsight.path.as_deref(),
                        version: components.agentsight.version.as_deref(),
                        enabled: cfg.evidence.enabled,
                        target: None,
                        host_pid: Some(agent_pid),
                        db: run.evidence_db_path(),
                        log: run.dir.join("evidence-engine.log"),
                    });
                    run.manifest.planes.evidence = to_plane_state(&plane.outcome);
                    evidence = Some(plane);
                }
            })?;
        run.manifest.target.host_pid = Some(agent_pid);
        code
    };

    // Policy wrap owns the actplane process; PolicyPlane has no child in wrap
    // mode. For attach-style (not used in run wrap path), stop the attach child.
    let policy_was_active = policy.outcome.is_active();
    policy.stop();
    if let Some(mut ev) = evidence.take() {
        let evidence_was_active = ev.outcome.is_active();
        ev.stop();
        // Brief settle so agentsight can finish WAL after SIGTERM.
        if evidence_was_active {
            std::thread::sleep(Duration::from_millis(300));
        }
    } else {
        run.manifest.planes.evidence = PlaneState::Disabled(if cfg.evidence.enabled {
            "evidence plane was not started".into()
        } else {
            "evidence.enabled is false".into()
        });
    }

    // Harvest only after engines have fully exited (stop waits for that).
    if policy_was_active || cfg.policy.mode != PolicyMode::Off {
        harvest_actplane_events(run);
    }

    run.manifest.summary.duration_seconds = started.elapsed().as_secs_f64();

    let message = cfg
        .history
        .message
        .clone()
        .unwrap_or_else(|| format!("actime run {}", run.id));
    let (history, commit) = HistoryPlane::commit(
        components.akeep.path.as_deref(),
        cfg.history.enabled && cfg.history.commit_on_exit,
        &message,
        &run.dir.join("history.log"),
    );
    run.manifest.planes.history = to_plane_state(&history);
    run.manifest.akeep_commit = commit;

    Ok(exit)
}

// ---------------------------------------------------------------------------
// host launch helpers
// ---------------------------------------------------------------------------

/// Launch the agent under `actplane run` and wait with a graceful flush window.
///
/// When the agent tree goes idle, wait for ActPlane to exit on its own so it can
/// flush events.jsonl. Only then escalate to SIGTERM / SIGKILL. Harvest must
/// run only after this returns.
fn run_host_policy_wrap<F>(
    actplane: &Path,
    policy_yaml: &Path,
    agent: &[String],
    log_path: &Path,
    limit: Option<Duration>,
    quiet: bool,
    mut on_spawn: F,
) -> Result<(i32, i32)>
where
    F: FnMut(i32),
{
    let argv = PolicyPlane::wrap_argv(actplane, policy_yaml, agent);
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("empty actplane wrap argv"))?;

    let log = std::fs::File::create(log_path)
        .with_context(|| format!("creating {}", log_path.display()))?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .env("ACTPLANE_HOOK_PROFILE", "full")
        .spawn()
        .with_context(|| format!("spawning host policy wrap `{program}`"))?;

    let wrap_pid = child.id() as i32;
    on_spawn(wrap_pid);

    if let Some(mut stderr) = child.stderr.take() {
        let mut log = log;
        let mut terminal = std::io::stderr();
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut stderr, &mut TeeWriter(&mut log, &mut terminal));
        });
    }

    let code = wait_for_wrap_pid(&mut child, wrap_pid, limit, quiet)?;

    // Prefer a real wait status once the wrapper has exited. If we force-reaped
    // after the agent finished, do not surface SIGTERM(143) as the run exit.
    match child.try_wait() {
        Ok(Some(st)) => {
            let st_code = exit_status_code(st);
            if code == 0 && st_code >= 128 {
                Ok((0, wrap_pid))
            } else if code != 0 {
                Ok((code, wrap_pid))
            } else {
                Ok((st_code, wrap_pid))
            }
        }
        Ok(None) => {
            // Still live after wait_for_wrap_pid decided — should not happen,
            // but never block on Drop::wait.
            std::mem::forget(child);
            Ok((code, wrap_pid))
        }
        Err(_) => {
            std::mem::forget(child);
            Ok((code, wrap_pid))
        }
    }
}

/// Dual writer: log file + terminal.
struct TeeWriter<'a>(&'a mut std::fs::File, &'a mut std::io::Stderr);

impl std::io::Write for TeeWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = buf.len();
        let _ = std::io::Write::write_all(self.0, buf);
        std::io::Write::write_all(self.1, buf)?;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let _ = std::io::Write::flush(self.0);
        std::io::Write::flush(self.1)
    }
}

/// Wait on a policy-wrap child. After the agent tree is idle, prefer a natural
/// ActPlane exit (event flush), then SIGTERM with a long grace, then SIGKILL.
fn wait_for_wrap_pid(
    child: &mut std::process::Child,
    wrap_pid: i32,
    limit: Option<Duration>,
    quiet: bool,
) -> Result<i32> {
    let started = Instant::now();
    let deadline = limit.map(|l| Instant::now() + l);
    let mut idle_since: Option<Instant> = None;
    let mut agent_seen = false;
    // Allow ActPlane eBPF setup before treating "no agent" as idle.
    const START_GRACE: Duration = Duration::from_millis(2500);
    // Continuous period with no non-wrapper descendant before we consider the
    // agent done.
    const WRAP_IDLE: Duration = Duration::from_millis(800);
    // After agent idle: brief window for actplane to exit on its own. ActPlane
    // 0.1.8 often never flushes events.jsonl on this path; harvest falls back
    // to the engine log + policy.dsl. Bound the wait so runs stay snappy.
    const NATURAL_EXIT: Duration = Duration::from_secs(2);

    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(exit_status_code(status)),
            Ok(None) => {}
            Err(e) => return Err(e).context("polling host policy wrap"),
        }

        if wrap_tree_has_agent(wrap_pid) {
            agent_seen = true;
            idle_since = None;
        } else if started.elapsed() >= START_GRACE {
            let since = *idle_since.get_or_insert_with(Instant::now);
            // Once the agent is gone, give ActPlane a natural-exit window so it
            // can flush events.jsonl. Only then terminate.
            if since.elapsed() >= WRAP_IDLE {
                // Extra natural wait: actplane often exits shortly after the
                // agent if we simply stop reaping early.
                let natural_deadline = Instant::now() + NATURAL_EXIT;
                while Instant::now() < natural_deadline {
                    match child.try_wait() {
                        Ok(Some(status)) => return Ok(exit_status_code(status)),
                        Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                        Err(e) => return Err(e).context("waiting for actplane natural exit"),
                    }
                    if let (Some(limit), Some(deadline)) = (limit.as_ref(), deadline.as_ref()) {
                        if Instant::now() >= *deadline {
                            if !quiet {
                                eprintln!(
                                    "{}",
                                    ui::warn(&format!(
                                        "wall-clock limit of {} reached; terminating the agent",
                                        actime_core::config::format_duration(limit)
                                    ))
                                );
                            }
                            return terminate_child_graceful(child, quiet);
                        }
                    }
                }

                if !quiet {
                    eprintln!(
                        "{}",
                        ui::warn(
                            "actplane run outlived the agent; requesting graceful shutdown so \
                             policy events can flush"
                        )
                    );
                }
                // Agent already finished. Map wrapper signal death to 0 so a
                // successful short agent (echo) is not reported as 143.
                let _ = agent_seen;
                let _ = terminate_child_graceful(child, quiet)?;
                return Ok(0);
            }
        }

        if let (Some(limit), Some(deadline)) = (limit.as_ref(), deadline.as_ref()) {
            if Instant::now() >= *deadline {
                if !quiet {
                    eprintln!(
                        "{}",
                        ui::warn(&format!(
                            "wall-clock limit of {} reached; terminating the agent",
                            actime_core::config::format_duration(limit)
                        ))
                    );
                }
                return terminate_child_graceful(child, quiet);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Spawn the agent as a plain child and wait for it.
fn run_plain_child<F>(
    agent: &[String],
    limit: Option<Duration>,
    quiet: bool,
    mut on_spawn: F,
) -> Result<(i32, i32)>
where
    F: FnMut(i32),
{
    let (program, args) = agent
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("empty command"))?;

    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawning `{program}`"))?;

    let pid = child.id() as i32;
    on_spawn(pid);

    let code = wait_plain_child(&mut child, limit, quiet)?;
    Ok((code, pid))
}

fn wait_plain_child(
    child: &mut std::process::Child,
    limit: Option<Duration>,
    quiet: bool,
) -> Result<i32> {
    let deadline = limit.map(|l| Instant::now() + l);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(exit_status_code(status)),
            Ok(None) => {}
            Err(e) => return Err(e).context("polling agent child"),
        }
        if let (Some(limit), Some(deadline)) = (limit.as_ref(), deadline.as_ref()) {
            if Instant::now() >= *deadline {
                if !quiet {
                    eprintln!(
                        "{}",
                        ui::warn(&format!(
                            "wall-clock limit of {} reached; terminating the agent",
                            actime_core::config::format_duration(limit)
                        ))
                    );
                }
                return terminate_child_graceful(child, quiet);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn exit_status_code(status: std::process::ExitStatus) -> i32 {
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

/// SIGTERM with a long flush window, then SIGKILL. Never blocks forever.
///
/// ActPlane must be allowed to write events.jsonl on graceful exit. A tool that
/// enforces a kill and then loses the event is worse than useless.
fn terminate_child_graceful(child: &mut std::process::Child, _quiet: bool) -> Result<i32> {
    let pid = child.id() as i32;
    // SAFETY: positive pid; ESRCH is fine.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    // ActPlane 0.1.8 exits on SIGTERM within ~0.5s without flushing events;
    // keep a short grace so a future engine that does flush still has room.
    const TERM_GRACE: Duration = Duration::from_secs(2);
    let grace = Instant::now() + TERM_GRACE;
    while Instant::now() < grace {
        match child.try_wait() {
            Ok(Some(st)) => return Ok(exit_status_code(st)),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(e).context("waiting after SIGTERM"),
        }
    }
    let _ = child.kill();
    let kill_grace = Instant::now() + Duration::from_millis(500);
    while Instant::now() < kill_grace {
        match child.try_wait() {
            Ok(Some(st)) => return Ok(exit_status_code(st)),
            Ok(None) => std::thread::sleep(Duration::from_millis(40)),
            Err(_) => break,
        }
    }
    Ok(137)
}

/// True when the process tree under `root_pid` still contains a non-wrapper
/// process (the agent or its subprocesses).
fn wrap_tree_has_agent(root_pid: i32) -> bool {
    for pid in collect_descendants(root_pid) {
        let comm = read_comm(pid);
        if !is_wrap_helper_comm(&comm) {
            return true;
        }
    }
    false
}

fn is_wrap_helper_comm(comm: &str) -> bool {
    matches!(
        comm,
        "sudo" | "actplane" | "timeout" | "tee" | "stdbuf" | "env"
    )
}

fn collect_descendants(root: i32) -> Vec<i32> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    let mut guard = 0usize;
    while let Some(pid) = stack.pop() {
        guard += 1;
        if guard > 10_000 {
            break;
        }
        for child in direct_children(pid) {
            out.push(child);
            stack.push(child);
        }
    }
    out
}

fn direct_children(pid: i32) -> Vec<i32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut kids = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(child) = name.to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        if child <= 1 || child == pid {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{child}/stat")) else {
            continue;
        };
        if parse_ppid_from_stat(&stat) == Some(pid) {
            kids.push(child);
        }
    }
    kids
}

fn read_comm(pid: i32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Field 4 of `/proc/<pid>/stat` is ppid. Comm may contain spaces/parens.
fn parse_ppid_from_stat(stat: &str) -> Option<i32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

// ---------------------------------------------------------------------------
// attach target resolution
// ---------------------------------------------------------------------------

struct ResolvedTarget {
    kind: String,
    label: String,
    host_pid: i32,
    evidence_target: Option<String>,
}

fn resolve_attach_target(req: &AttachRequest) -> Result<ResolvedTarget> {
    if let Some(pid) = req.pid {
        if !process_is_alive(pid) {
            bail!("no process with pid {pid}. It may have already exited.");
        }
        return Ok(ResolvedTarget {
            kind: "pid".into(),
            label: pid.to_string(),
            host_pid: pid,
            evidence_target: None,
        });
    }
    if let Some(comm) = &req.comm {
        let pid = find_pid_by_comm(comm)?;
        return Ok(ResolvedTarget {
            kind: "comm".into(),
            label: comm.clone(),
            host_pid: pid,
            evidence_target: None,
        });
    }
    if let Some(container) = &req.container {
        let pid = resolve_container_host_pid(container)?;
        return Ok(ResolvedTarget {
            kind: "container".into(),
            label: container.clone(),
            host_pid: pid,
            evidence_target: Some(format!("docker://{container}")),
        });
    }
    if let Some(pod) = &req.pod {
        let (pid, evidence) = resolve_pod_host_pid(pod)?;
        return Ok(ResolvedTarget {
            kind: "pod".into(),
            label: pod.clone(),
            host_pid: pid,
            evidence_target: Some(evidence),
        });
    }
    bail!("give one of --pid, --comm, --container, or --pod");
}

/// Resolve an already-running Docker/Podman container to its host init pid.
///
/// Never creates or starts a container. Missing/stopped targets are hard errors.
fn resolve_container_host_pid(name_or_id: &str) -> Result<i32> {
    if let Some(pid) = inspect_container_pid("docker", name_or_id) {
        return Ok(pid);
    }
    if let Some(pid) = inspect_container_pid("podman", name_or_id) {
        return Ok(pid);
    }
    bail!(
        "container `{name_or_id}` was not found as a running Docker or Podman container.\n\
         Actime does not create containers — start one yourself, then attach:\n\
           docker run -d --name my-agent …\n\
           actime attach --container my-agent"
    );
}

fn inspect_container_pid(runtime: &str, name_or_id: &str) -> Option<i32> {
    let out = Command::new(runtime)
        .args(["inspect", "--format", "{{.State.Pid}}", name_or_id])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let pid: i32 = text.parse().ok()?;
    if pid <= 0 {
        // Pid 0 means the container exists but is not running.
        return None;
    }
    if !process_is_alive(pid) {
        return None;
    }
    Some(pid)
}

/// Resolve an already-running Kubernetes pod on this node to a host pid.
fn resolve_pod_host_pid(ns_pod: &str) -> Result<(i32, String)> {
    let (ns, name) = match ns_pod.split_once('/') {
        Some((ns, name)) if !ns.is_empty() && !name.is_empty() => (ns, name),
        _ => bail!("pod must be `namespace/name`, e.g. `default/agent-0`. Got `{ns_pod}`."),
    };
    let evidence = format!("k8s://{ns}/{name}");

    let out = Command::new("kubectl")
        .args(["get", "pod", "-n", ns, name, "-o", "json"])
        .output()
        .with_context(|| {
            format!(
                "running kubectl to resolve pod {ns}/{name}. Is kubectl installed and configured?"
            )
        })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "pod `{ns}/{name}` was not found or is not readable:\n  {}\n\
             Actime does not create pods — deploy the pod yourself, then attach.",
            stderr.trim().lines().next().unwrap_or("(no detail)")
        );
    }

    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("parsing kubectl pod JSON")?;
    let container_ids = extract_container_ids(&json);
    if container_ids.is_empty() {
        bail!(
            "pod `{ns}/{name}` has no running container statuses. Is the pod Running on this node?"
        );
    }

    for cid in &container_ids {
        let bare = strip_runtime_prefix(cid);
        if let Some(pid) = inspect_container_pid("docker", bare)
            .or_else(|| inspect_container_pid("podman", bare))
            .or_else(|| crictl_container_pid(bare))
        {
            return Ok((pid, evidence));
        }
    }

    bail!(
        "pod `{ns}/{name}` is known to kubectl but its container could not be mapped to a host \
         pid on this machine.\n\
         Actime attach --pod only works on a node that hosts the pod (docker/podman/crictl)."
    );
}

fn extract_container_ids(pod: &serde_json::Value) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(statuses) = pod
        .pointer("/status/containerStatuses")
        .and_then(|v| v.as_array())
    {
        for st in statuses {
            if let Some(id) = st.get("containerID").and_then(|v| v.as_str()) {
                if !id.is_empty() {
                    ids.push(id.to_string());
                }
            }
        }
    }
    ids
}

fn strip_runtime_prefix(cid: &str) -> &str {
    // containerd://abc, docker://abc, cri-o://abc
    cid.rsplit_once("://").map(|(_, id)| id).unwrap_or(cid)
}

fn crictl_container_pid(container_id: &str) -> Option<i32> {
    let out = Command::new("crictl")
        .args(["inspect", container_id])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    // Common locations across cri implementations.
    for path in [
        "/info/pid",
        "/pid",
        "/status/pid",
        "/info/runtimeSpec/process/pid",
    ] {
        if let Some(pid) = json.pointer(path).and_then(|v| {
            v.as_i64()
                .map(|n| n as i32)
                .or_else(|| v.as_u64().map(|n| n as i32))
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        }) {
            if pid > 0 && process_is_alive(pid) {
                return Some(pid);
            }
        }
    }
    // info is sometimes a JSON string.
    if let Some(info_str) = json.get("info").and_then(|v| v.as_str()) {
        if let Ok(info) = serde_json::from_str::<serde_json::Value>(info_str) {
            if let Some(pid) = info.get("pid").and_then(|v| v.as_i64()) {
                let pid = pid as i32;
                if pid > 0 && process_is_alive(pid) {
                    return Some(pid);
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// harvest + finish
// ---------------------------------------------------------------------------

/// Copy ActPlane-scoped event files into the run's `violations.jsonl`.
///
/// ActPlane 0.1.x `run` rewrites feedback paths via `scoped_feedback_paths`:
/// events land under `actplane/runs/run-<pid>-<ts>/events.jsonl`. We also
/// accept the older layout and synthesize from feedback / kill banners when
/// the JSONL was empty after a flush race (should be rare after graceful stop).
fn harvest_actplane_events(run: &Run) {
    // Fast path: events already on disk.
    let mut collected = String::new();
    for root in [run.dir.join("actplane").join("runs"), run.dir.join("runs")] {
        append_events_from_run_tree(&root, &mut collected);
    }
    // Brief poll only when a scoped runs tree exists (engine may still be
    // closing the file after a graceful exit).
    if collected.is_empty() && run.dir.join("actplane").join("runs").is_dir() {
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(50));
            for root in [run.dir.join("actplane").join("runs"), run.dir.join("runs")] {
                append_events_from_run_tree(&root, &mut collected);
            }
            if !collected.is_empty() {
                break;
            }
        }
    }
    if !collected.is_empty() {
        write_violations(run, &collected);
        return;
    }

    for root in [
        run.dir.join("actplane").join("runs"),
        run.dir.join("runs"),
        run.dir.join("actplane"),
        run.dir.clone(),
    ] {
        if let Some(line) = synthesize_violation_from_feedback_tree(&root) {
            collected.push_str(&line);
            collected.push('\n');
            break;
        }
    }
    if collected.is_empty() {
        if let Some(line) = synthesize_violation_from_kill_banner(&run.dir) {
            collected.push_str(&line);
            collected.push('\n');
        }
    }
    // ActPlane 0.1.8 often exits on SIGTERM without flushing events.jsonl even
    // though the kernel already killed the process. Recover the violation from
    // the engine log + the composed policy.dsl so the report never lies.
    if collected.is_empty() {
        if let Some(line) = synthesize_violation_from_log_and_policy(run) {
            collected.push_str(&line);
            collected.push('\n');
        }
    }
    if !collected.is_empty() {
        write_violations(run, &collected);
    }
}

fn write_violations(run: &Run, collected: &str) {
    let dest = run.violations_path();
    let mut existing = std::fs::read_to_string(&dest).unwrap_or_default();
    if !existing.is_empty() && !existing.ends_with('\n') {
        existing.push('\n');
    }
    existing.push_str(collected);
    let _ = std::fs::write(&dest, existing);
}

fn append_events_from_run_tree(runs_root: &Path, collected: &mut String) {
    let Ok(entries) = std::fs::read_dir(runs_root) else {
        return;
    };
    for entry in entries.flatten() {
        let events = entry.path().join("events.jsonl");
        if let Ok(text) = std::fs::read_to_string(&events) {
            if !text.trim().is_empty() {
                collected.push_str(text.trim_end());
                collected.push('\n');
            }
        }
        let direct = entry.path();
        if direct.file_name().is_some_and(|n| n == "events.jsonl") {
            if let Ok(text) = std::fs::read_to_string(&direct) {
                if !text.trim().is_empty() {
                    collected.push_str(text.trim_end());
                    collected.push('\n');
                }
            }
        }
    }
}

fn synthesize_violation_from_feedback_tree(root: &Path) -> Option<String> {
    let mut candidates = vec![root.join("feedback.txt")];
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            candidates.push(e.path().join("feedback.txt"));
            // One more level: actplane/runs/<id>/feedback.txt
            if let Ok(inner) = std::fs::read_dir(e.path()) {
                for i in inner.flatten() {
                    candidates.push(i.path().join("feedback.txt"));
                }
            }
        }
    }
    for path in candidates {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Some(line) = parse_feedback_kill(&text) {
                return Some(line);
            }
        }
    }
    None
}

fn parse_feedback_kill(text: &str) -> Option<String> {
    let mut rule = None;
    let mut reason = None;
    let mut target = None;
    let mut effect = "kill";
    for line in text.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("[ActPlane] Operation killed by rule `") {
            rule = rest.split('`').next().map(str::to_string);
            effect = "kill";
        } else if let Some(rest) = l.strip_prefix("[ActPlane] Operation blocked by rule `") {
            rule = rest.split('`').next().map(str::to_string);
            effect = "block";
        } else if let Some(rest) = l.strip_prefix("- Target operation:") {
            target = Some(rest.trim().to_string());
        } else if let Some(rest) = l.strip_prefix("- Reason:") {
            reason = Some(rest.trim().to_string());
        }
    }
    let rule = rule.filter(|r| !r.is_empty())?;
    let reason = reason.unwrap_or_default();
    let target = target.unwrap_or_default();
    Some(flat_violation_json(&rule, effect, &target, &reason))
}

fn parse_kill_banner(text: &str) -> Option<String> {
    let mut target = None;
    let mut reason = None;
    let mut effect = None;
    let mut saw_killed = false;
    let mut cmdline = None;
    for line in text.lines() {
        let l = line.trim();
        // ActPlane console banner: "🚫 KILLED: process 'git' … — /usr/bin/git"
        if l.contains("KILLED:") || l.contains("Killed:") {
            saw_killed = true;
            effect = Some("kill");
            if let Some(idx) = l.rfind('—').or_else(|| l.rfind('-')) {
                let t =
                    l[idx + l[idx..].chars().next().map(|c| c.len_utf8()).unwrap_or(1)..].trim();
                if !t.is_empty() {
                    target = Some(t.to_string());
                }
            }
        }
        // Bash job-control line when a child is SIGKILL'd by the policy:
        //   "script: line N: 12345 Killed                  git push --force …"
        if let Some(rest) = l.split_once(" Killed").map(|(_, r)| r.trim()) {
            if !rest.is_empty() {
                saw_killed = true;
                effect = Some("kill");
                cmdline = Some(rest.to_string());
                if rest.contains("git") {
                    target = Some(if rest.contains("--force") {
                        "git --force".into()
                    } else {
                        rest.chars().take(80).collect()
                    });
                }
            }
        } else if let Some(rest) = l.strip_prefix("effect:") {
            effect = Some(rest.trim());
        } else if let Some(rest) = l.strip_prefix("reason:") {
            reason = Some(rest.trim().to_string());
        }
    }
    if !saw_killed {
        return None;
    }
    let cmdline_l = cmdline.unwrap_or_default().to_ascii_lowercase();
    let reason = reason.unwrap_or_else(|| {
        if cmdline_l.contains("--force") || cmdline_l.contains(" force") {
            "Force-pushing, hard-resetting, and cleaning discard work that cannot be recovered from the agent's own history. Use a non-destructive git command, or ask the user to run this.".into()
        } else {
            String::new()
        }
    });
    let rule = if reason.to_ascii_lowercase().contains("force")
        || reason.to_ascii_lowercase().contains("hard-reset")
        || cmdline_l.contains("--force")
        || cmdline_l.contains("git clean")
        || cmdline_l.contains("--hard")
    {
        "destructive-vcs"
    } else if cmdline_l.contains("rm") && cmdline_l.contains("-rf") {
        "mass-deletion"
    } else {
        "policy"
    };
    let target = target.unwrap_or_default();
    let effect = effect.unwrap_or("kill");
    Some(flat_violation_json(rule, effect, &target, &reason))
}

/// When ActPlane leaves events.jsonl empty, reconstruct the violation from the
/// engine log (kill evidence) and the composed `policy.dsl` (rule + because).
fn synthesize_violation_from_log_and_policy(run: &Run) -> Option<String> {
    let log_text = read_run_logs(run);
    if log_text.is_empty() {
        return None;
    }
    // Prefer structured banner / bash-kill parse first.
    if let Some(line) = parse_kill_banner(&log_text) {
        // Enrich reason from policy.dsl when the banner had a thin reason.
        if let Some(enriched) = enrich_violation_from_policy(run, &line) {
            return Some(enriched);
        }
        return Some(line);
    }
    // Detect kill + git --force even without a "Killed" token (exit 137 notes).
    let lower = log_text.to_ascii_lowercase();
    if (lower.contains("git") && lower.contains("--force"))
        && (lower.contains("137") || lower.contains("killed") || lower.contains("sigkill"))
    {
        let (rule, reason) = lookup_policy_rule(run, "destructive-vcs").unwrap_or_else(|| {
            (
                "destructive-vcs".into(),
                "Force-pushing, hard-resetting, and cleaning discard work that cannot be recovered from the agent's own history. Use a non-destructive git command, or ask the user to run this.".into(),
            )
        });
        return Some(flat_violation_json(&rule, "kill", "git --force", &reason));
    }
    None
}

fn read_run_logs(run: &Run) -> String {
    let mut out = String::new();
    let mut files = vec![
        run.dir.join("policy-engine.log"),
        run.dir.join("stderr.log"),
        run.dir.join("stdout.log"),
    ];
    if let Ok(entries) = std::fs::read_dir(&run.dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("log") {
                files.push(p);
            }
        }
    }
    for path in files {
        if let Ok(text) = std::fs::read_to_string(&path) {
            out.push_str(&text);
            out.push('\n');
        }
    }
    out
}

/// Pull `(name, because)` from the composed policy.dsl for a known rule id.
fn lookup_policy_rule(run: &Run, want: &str) -> Option<(String, String)> {
    let dsl_path = run.dir.join("policy.dsl");
    let text = std::fs::read_to_string(dsl_path).ok()?;
    let mut current: Option<String> = None;
    let mut because: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("rule ") {
            if let Some(name) = rest.strip_suffix(':').map(str::trim) {
                if let (Some(cur), Some(b)) = (current.take(), because.take()) {
                    if cur == want {
                        return Some((cur, b));
                    }
                }
                current = Some(name.to_string());
                because = None;
            }
        } else if t.starts_with("because ") {
            let r = t.trim_start_matches("because ").trim();
            let r = r.trim_matches('"').trim_matches('\'').to_string();
            because = Some(r);
        }
    }
    if let (Some(cur), Some(b)) = (current, because) {
        if cur == want {
            return Some((cur, b));
        }
    }
    // Fallback: scan for the rule name and the nearest because.
    if text.contains(&format!("rule {want}")) {
        for line in text.lines() {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("because ") {
                let r = rest.trim().trim_matches('"').trim_matches('\'').to_string();
                if !r.is_empty() {
                    return Some((want.to_string(), r));
                }
            }
        }
    }
    None
}

fn enrich_violation_from_policy(run: &Run, flat_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(flat_json).ok()?;
    let rule = v.get("rule")?.as_str()?;
    let effect = v.get("effect").and_then(|e| e.as_str()).unwrap_or("kill");
    let target = v.get("target").and_then(|t| t.as_str()).unwrap_or("");
    let mut reason = v
        .get("reason")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();
    if let Some((_, because)) = lookup_policy_rule(run, rule) {
        if reason.is_empty() || reason.len() < because.len() / 2 {
            reason = because;
        }
    }
    if reason.is_empty() {
        return None;
    }
    Some(flat_violation_json(rule, effect, target, &reason))
}

fn synthesize_violation_from_kill_banner(run_dir: &Path) -> Option<String> {
    let mut files = vec![
        run_dir.join("policy-engine.log"),
        run_dir.join("stderr.log"),
        run_dir.join("stdout.log"),
    ];
    if let Ok(entries) = std::fs::read_dir(run_dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("log") {
                files.push(p);
            }
        }
    }
    for path in files {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Some(line) = parse_kill_banner(&text) {
                return Some(line);
            }
            if let Some(line) = parse_feedback_kill(&text) {
                return Some(line);
            }
        }
    }
    None
}

fn flat_violation_json(rule: &str, effect: &str, target: &str, reason: &str) -> String {
    format!(
        "{{\"rule\":{rule},\"effect\":{effect},\"op\":\"exec\",\"target\":{target},\"pid\":0,\"comm\":\"\",\"reason\":{reason},\"ts\":\"\"}}",
        rule = serde_json::to_string(rule).unwrap_or_else(|_| "\"\"".into()),
        effect = serde_json::to_string(effect).unwrap_or_else(|_| "\"kill\"".into()),
        target = serde_json::to_string(target).unwrap_or_else(|_| "\"\"".into()),
        reason = serde_json::to_string(reason).unwrap_or_else(|_| "\"\"".into()),
    )
}

fn finish(ctx: &Ctx, run: &mut Run, exit: i32, fail_on_violation: bool) -> Result<i32> {
    run.manifest.exit_code = Some(exit);
    run.manifest.ended_at = Some(now_rfc3339());

    let ev = Evidence::collect(run).unwrap_or_default();
    let duration = run.manifest.summary.duration_seconds;
    run.manifest.summary = ev.summary.clone();
    run.manifest.summary.duration_seconds = duration;

    if matches!(run.manifest.planes.evidence, PlaneState::Active)
        && !actime_core::evidence::has_observational_signal(&ev.summary)
    {
        let reason = if run.evidence_db_path().is_file() {
            "agentsight produced no process/file/network observations (empty or unreadable evidence.db)"
                .to_string()
        } else {
            "agentsight reported active but left no evidence.db".to_string()
        };
        run.manifest.planes.evidence = PlaneState::Degraded(reason);
    }

    run.save_manifest()?;

    let md = report::render_markdown(run, &ev);
    let _ = std::fs::write(run.report_path(), &md);

    if !ctx.quiet {
        eprintln!();
        eprintln!("{}", report::render_text(run, &ev, ui::width()));
    }

    if fail_on_violation && (ev.summary.blocked > 0 || ev.summary.killed > 0) {
        return Ok(crate::EXIT_VIOLATION);
    }
    Ok(exit)
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

fn compose(cfg: &Config, workspace: &str) -> Result<String> {
    let mut extra = Vec::new();
    for path in &cfg.policy.files {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading the policy file {path}"))?;
        extra.push((path.clone(), text));
    }
    embedded::compose_policy(&cfg.policy.packs, &extra, workspace)
}

fn packs_label(cfg: &Config) -> String {
    if cfg.policy.packs.is_empty() {
        "custom".to_string()
    } else {
        cfg.policy.packs.join(", ")
    }
}

fn to_plane_state(o: &Outcome) -> PlaneState {
    match o {
        Outcome::Active(_) => PlaneState::Active,
        Outcome::Degraded(d) => PlaneState::Degraded(d.clone()),
        Outcome::Disabled(d) => PlaneState::Disabled(d.clone()),
    }
}

fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339()
}

fn find_pid_by_comm(comm: &str) -> Result<i32> {
    // Linux TASK_COMM_LEN is 16 (15 chars + NUL). Long names are truncated in
    // /proc/<pid>/comm; also match by cmdline basename when needed.
    let comm_trunc: String = comm.chars().take(15).collect();
    let mut best: Option<(u64, i32)> = None;
    let entries = std::fs::read_dir("/proc").context("reading /proc")?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let path = entry.path();
        let Ok(found) = std::fs::read_to_string(path.join("comm")) else {
            continue;
        };
        let found = found.trim();
        let matches = found == comm || found == comm_trunc || cmdline_basename_matches(&path, comm);
        if !matches {
            continue;
        }
        let starttime = std::fs::read_to_string(path.join("stat"))
            .ok()
            .and_then(|s| parse_starttime(&s))
            .unwrap_or(0);
        if best.is_none_or(|(t, _)| starttime > t) {
            best = Some((starttime, pid));
        }
    }
    match best {
        Some((_, pid)) => Ok(pid),
        None => bail!("no running process named `{comm}`. Start the agent first, or give --pid."),
    }
}

fn cmdline_basename_matches(proc_dir: &Path, want: &str) -> bool {
    let Ok(raw) = std::fs::read(proc_dir.join("cmdline")) else {
        return false;
    };
    let first = raw.split(|b| *b == 0).next().unwrap_or_default();
    let path = String::from_utf8_lossy(first);
    Path::new(path.as_ref())
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|base| base == want)
}

fn parse_starttime(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

fn process_is_alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_stat_starttime_survives_a_comm_with_spaces() {
        let mut fields: Vec<String> = vec!["S".into()];
        for i in 1..=18 {
            fields.push(i.to_string());
        }
        fields.push("987654".into());
        let stat = format!("42 (weird (name)) {}", fields.join(" "));
        assert_eq!(parse_starttime(&stat), Some(987654));
    }

    #[test]
    fn outcomes_map_onto_manifest_plane_states() {
        assert_eq!(
            to_plane_state(&Outcome::Active("x".into())),
            PlaneState::Active
        );
        assert_eq!(
            to_plane_state(&Outcome::Disabled("no root".into())),
            PlaneState::Disabled("no root".into())
        );
        assert_eq!(
            to_plane_state(&Outcome::Degraded("partial".into())),
            PlaneState::Degraded("partial".into())
        );
    }

    #[test]
    fn a_run_composes_policy_against_the_real_workspace() {
        let cfg = Config::builtin_profile("balanced").expect("balanced");
        let dsl = compose(&cfg, "/home/user/proj").expect("compose");
        assert!(dsl.contains("/home/user/proj"));
        assert!(!dsl.contains("${WORKSPACE}"));
    }

    #[test]
    fn parse_ppid_from_stat_handles_comm_with_spaces() {
        let stat = "42 (weird name) S 17 18 19 20 21 22";
        assert_eq!(parse_ppid_from_stat(stat), Some(17));
    }

    #[test]
    fn parse_feedback_kill_extracts_rule_and_reason() {
        let text = "\
[ActPlane] Operation killed by rule `destructive-vcs`.
- Target operation: exec /usr/bin/git
- Reason: Force-pushing, hard-resetting, and cleaning discard work.
- The policy terminated the violating process
";
        let line = parse_feedback_kill(text).expect("line");
        assert!(line.contains("destructive-vcs"));
        assert!(line.contains("kill"));
        assert!(line.contains("Force-pushing"));
        assert!(line.contains("/usr/bin/git"));
    }

    #[test]
    fn parse_kill_banner_extracts_reason() {
        let text = "\
🚫 KILLED: process 'git' (pid 1, ppid 2) — /usr/bin/git
   effect: kill
   reason: Force-pushing discards work.
";
        let line = parse_kill_banner(text).expect("line");
        assert!(line.contains("destructive-vcs") || line.contains("kill"));
        assert!(line.contains("Force-pushing"));
    }

    #[test]
    fn parse_bash_killed_line_maps_force_push_to_destructive_vcs() {
        let text = "/tmp/actime-demo-agent: line 13: 3517655 Killed                  git push --force origin HEAD 2>&1\n";
        let line = parse_kill_banner(text).expect("line");
        assert!(line.contains("destructive-vcs"), "line={line}");
        assert!(line.contains("kill"), "line={line}");
        assert!(
            line.contains("Force-pushing") || line.contains("force"),
            "line={line}"
        );
    }

    #[test]
    fn harvest_synthesizes_from_engine_log_when_events_empty() {
        let tmp = tempfile::tempdir().expect("tmp");
        let run_dir = tmp.path().join("run");
        std::fs::create_dir_all(run_dir.join("actplane/runs/run-1")).unwrap();
        // Empty events — the failure mode we must recover from.
        std::fs::write(run_dir.join("actplane/runs/run-1/events.jsonl"), "").unwrap();
        std::fs::write(
            run_dir.join("policy-engine.log"),
            "ActPlane: running\n/tmp/a: line 13: 1 Killed                  git push --force origin HEAD\n",
        )
        .unwrap();
        std::fs::write(
            run_dir.join("policy.dsl"),
            "rule destructive-vcs:\n  kill exec \"git\" \"--force\" if AGENT\n  because \"Force-pushing discards work that cannot be recovered.\"\n",
        )
        .unwrap();
        let cfg = Config::default();
        let run = actime_core::run::Run {
            id: actime_core::run::RunId("test-run".into()),
            dir: run_dir.clone(),
            manifest: actime_core::run::Manifest::new(
                "test-run",
                &["x".into()],
                &cfg,
                run_dir.clone(),
            ),
        };
        harvest_actplane_events(&run);
        let v = std::fs::read_to_string(run.violations_path()).expect("violations");
        assert!(v.contains("destructive-vcs"), "v={v}");
        assert!(v.contains("Force-pushing") || v.contains("force"), "v={v}");
        assert!(v.contains("kill"), "v={v}");
    }

    #[test]
    fn wrap_helpers_are_recognized() {
        assert!(is_wrap_helper_comm("actplane"));
        assert!(is_wrap_helper_comm("sudo"));
        assert!(!is_wrap_helper_comm("git"));
        assert!(!is_wrap_helper_comm("actime-demo-agent"));
    }

    #[test]
    fn harvest_actplane_events_reads_scoped_tree() {
        let tmp = tempfile::tempdir().expect("tmp");
        let run_dir = tmp.path().join("run");
        std::fs::create_dir_all(run_dir.join("actplane/runs/run-1")).unwrap();
        std::fs::write(
            run_dir.join("actplane/runs/run-1/events.jsonl"),
            r#"{"rule":{"name":"destructive-vcs","reason":"no force"},"effect":"kill","op":"exec","target":"/usr/bin/git","pid":1,"comm":"git","timestamp_unix_ns":"1"}
"#,
        )
        .unwrap();
        let cfg = Config::default();
        let run = actime_core::run::Run {
            id: actime_core::run::RunId("test-run".into()),
            dir: run_dir.clone(),
            manifest: actime_core::run::Manifest::new(
                "test-run",
                &["x".into()],
                &cfg,
                run_dir.clone(),
            ),
        };
        harvest_actplane_events(&run);
        let v = std::fs::read_to_string(run.violations_path()).expect("violations");
        assert!(v.contains("destructive-vcs"));
        assert!(v.contains("kill"));
    }

    #[test]
    fn strip_runtime_prefix_handles_cri_ids() {
        assert_eq!(strip_runtime_prefix("containerd://abc123"), "abc123");
        assert_eq!(strip_runtime_prefix("docker://xyz"), "xyz");
        assert_eq!(strip_runtime_prefix("plain"), "plain");
    }

    #[test]
    fn extract_container_ids_from_pod_json() {
        let pod = serde_json::json!({
            "status": {
                "containerStatuses": [
                    {"containerID": "containerd://aaa"},
                    {"containerID": "docker://bbb"}
                ]
            }
        });
        let ids = extract_container_ids(&pod);
        assert_eq!(ids, vec!["containerd://aaa", "docker://bbb"]);
    }

    #[test]
    fn nonexistent_container_errors_clearly() {
        let err = resolve_container_host_pid("actime-definitely-nonexistent-xyz")
            .unwrap_err()
            .to_string();
        assert!(err.contains("was not found") || err.contains("not found"));
        assert!(err.contains("does not create") || err.contains("Start one"));
    }
}
