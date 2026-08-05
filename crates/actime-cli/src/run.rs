//! `actime run`, `shell`, `attach`, and `demo` — the orchestration sequence.
//!
//! See `docs/DESIGN.md` section 7. The ordering matters: the sandbox is brought
//! up *before* the agent exists so the policy and evidence planes can attach to
//! its process tree first. Everything is fail-soft except `policy.mode:
//! enforce`, which aborts the run when the policy plane cannot load.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use actime_core::config::{CliOverrides, Config, PolicyMode};
use actime_core::evidence::Evidence;
use actime_core::run::{PlaneState, Run, RunStore};
use actime_core::{components::Components, report};
use actime_sandbox::{Backend, Mount, NetworkMode, Sandbox, SandboxSpec};

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
    /// `--sandbox`.
    pub sandbox: Option<String>,
    /// `--policy`.
    pub policy: Option<String>,
    /// `--image`.
    pub image: Option<String>,
    /// `--no-evidence`.
    pub no_evidence: bool,
    /// `--no-history`.
    pub no_history: bool,
    /// `--fail-on-violation`.
    pub fail_on_violation: bool,
    /// `--timeout`.
    pub timeout: Option<String>,
}

/// Run an agent under the full runtime. Returns the process exit code.
pub fn run(ctx: &Ctx, req: RunRequest) -> Result<i32> {
    let cwd = std::env::current_dir().context("resolving the current directory")?;
    let mut cfg = ctx.load_config(&cwd)?;

    cfg.merge_cli(&CliOverrides {
        sandbox_backend: req.sandbox.clone(),
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
    if let Some(image) = &req.image {
        cfg.sandbox.image = image.clone();
    }
    if let Some(t) = &req.timeout {
        cfg.limits.wall_clock = Some(
            actime_core::config::parse_duration(t)
                .with_context(|| format!("parsing --timeout {t}"))?,
        );
    }

    let components = Components::detect();
    let store = RunStore::open_default()?;
    // `create` snapshots the fully merged config next to the manifest, so even
    // a run that dies during setup leaves behind exactly what it was asked to
    // do.
    let mut run = store.create(&req.argv, &cfg)?;

    for c in components.iter() {
        if let Some(v) = &c.version {
            run.manifest
                .components
                .insert(c.name.to_string(), v.clone());
        }
    }

    let outcome = orchestrate(ctx, &mut run, &cfg, &components, &req, &cwd);

    // The exit path is unconditional: whatever happened above, the manifest and
    // the report are written.
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

/// The part of a run that can fail. Planes are torn down on every path.
fn orchestrate(
    ctx: &Ctx,
    run: &mut Run,
    cfg: &Config,
    components: &Components,
    req: &RunRequest,
    cwd: &Path,
) -> Result<i32> {
    let backend = resolve_backend(cfg)?;
    let spec = build_spec(cfg, run, cwd, backend)?;
    let isolation_note = spec_note(backend);

    let mut sandbox = actime_sandbox::create(backend, spec)
        .with_context(|| format!("creating the {} sandbox", backend.as_str()))?;

    // Step 3: bring the sandbox up with no agent in it, so the planes can
    // attach to a process tree that has not run any agent code yet.
    if let Err(e) = sandbox.start() {
        let _ = sandbox.cleanup();
        return Err(e).with_context(|| format!("starting the {} sandbox", backend.as_str()));
    }

    let host_pid = sandbox.host_pid();
    let evidence_target = sandbox.evidence_target();
    let sb_report = sandbox.report();
    run.manifest.sandbox.backend = backend.as_str().to_string();
    run.manifest.sandbox.name = sb_report.name.clone();
    run.manifest.sandbox.host_pid = host_pid;
    run.manifest.sandbox.isolation = backend != Backend::Host;
    run.manifest.sandbox.note = isolation_note;
    run.manifest.planes.isolation = if backend == Backend::Host {
        PlaneState::Degraded("host mode: no isolation".into())
    } else {
        PlaneState::Active
    };

    // The workspace path as the *agent* sees it. Policy files are written once,
    // against this path, so the same policy works inside and outside a sandbox.
    let workspace = match backend {
        Backend::Docker | Backend::Podman => cfg.sandbox.workdir.clone(),
        Backend::Bwrap | Backend::Host => cwd.display().to_string(),
    };

    // Step 4: the policy plane.
    // Host mode uses `actplane run` (wrap) per DESIGN.md §7; containers attach.
    let wrap_host = backend == Backend::Host && cfg.policy.mode != PolicyMode::Off;
    let dsl = compose(cfg, &workspace)?;
    let packs = if cfg.policy.packs.is_empty() {
        "custom".to_string()
    } else {
        cfg.policy.packs.join(", ")
    };
    // ActPlane `run` scopes feedback under parent(feedback)/runs/<id>/. Keep
    // that subtree under actplane/ so the rest of the run dir stays clean and
    // harvest knows where to look (see DESIGN.md §5).
    let actplane_dir = run.dir.join("actplane");
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
        host_pid,
        wrap_command: wrap_host,
        feedback_enabled: cfg.policy.feedback,
        log: run.dir.join("policy-engine.log"),
    });

    // Fail closed: `enforce` means enforce, or do not run at all.
    if cfg.policy.mode == PolicyMode::Enforce && !policy.outcome.is_active() {
        policy.stop();
        let _ = sandbox.cleanup();
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

    // Step 5: the evidence plane. Always fail-soft.
    let mut evidence = EvidencePlane::start(EvidencePlaneSpec {
        binary: components.agentsight.path.as_deref(),
        version: components.agentsight.version.as_deref(),
        enabled: cfg.evidence.enabled,
        target: evidence_target,
        host_pid,
        db: run.evidence_db_path(),
        log: run.dir.join("evidence-engine.log"),
    });
    run.manifest.planes.evidence = to_plane_state(&evidence.outcome);

    if !ctx.quiet {
        eprintln!(
            "{}",
            ui::banner(
                run.id.as_str(),
                backend.as_str(),
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
        eprintln!();
    }

    let _ = run.save_manifest();

    // Step 6: the agent.
    // Host + active policy → wrap with `actplane run` so enforcement is real.
    // Host wrap uses a dedicated waiter (not Sandbox::wait alone) because
    // actplane 0.1.8 often outlives the agent.
    let started = Instant::now();
    let exit = if wrap_host && policy.outcome.is_active() {
        match components.actplane.path.as_deref() {
            Some(bin) => run_host_policy_wrap(
                bin,
                &run.policy_path(),
                &req.argv,
                &run.dir.join("policy-engine.log"),
                cfg.limits.wall_clock,
                ctx.quiet,
            ),
            None => {
                // Should be unreachable: PolicyPlane::start would have disabled.
                let spawn_result = sandbox.spawn(&req.argv);
                match spawn_result {
                    Ok(()) => {
                        wait_for_agent(sandbox.as_mut(), cfg.limits.wall_clock, ctx.quiet, false)
                    }
                    Err(e) => Err(e),
                }
            }
        }
    } else {
        let spawn_result = sandbox.spawn(&req.argv);
        match spawn_result {
            Ok(()) => wait_for_agent(sandbox.as_mut(), cfg.limits.wall_clock, ctx.quiet, false),
            Err(e) => Err(e),
        }
    };

    policy.stop();
    evidence.stop();
    // Give agentsight a moment to flush WAL into evidence.db after SIGTERM.
    if matches!(
        run.manifest.planes.evidence,
        PlaneState::Active | PlaneState::Degraded(_)
    ) {
        std::thread::sleep(Duration::from_millis(300));
    }
    let _ = sandbox.cleanup();

    // ActPlane `run` scopes events under actplane/runs/<id>/events.jsonl.
    // Harvest them into the run's violations.jsonl so the report sees them.
    harvest_actplane_events(run);

    run.manifest.summary.duration_seconds = started.elapsed().as_secs_f64();

    let exit = exit.with_context(|| format!("running `{}`", req.argv.join(" ")))?;

    // Step 7: the history plane, after the agent has written its session files.
    // Bound is already inside HistoryPlane; demo must not hang here either.
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

/// Copy ActPlane-scoped event files into the run's `violations.jsonl`.
///
/// ActPlane 0.1.x `run` always rewrites feedback paths via
/// `scoped_feedback_paths`: events land under
/// `actplane/runs/run-<pid>-<ts>/events.jsonl`, not at the `events:` path in
/// policy.yaml. We also accept the older layout `runs/` at the run root.
fn harvest_actplane_events(run: &Run) {
    let mut collected = String::new();
    for root in [run.dir.join("actplane").join("runs"), run.dir.join("runs")] {
        append_events_from_run_tree(&root, &mut collected);
    }
    // Fallback: synthesize a violation line from feedback.txt or console kill
    // banners if the JSONL event file was empty (flush race / domain filter).
    if collected.is_empty() {
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
    }
    if collected.is_empty() {
        // Last resort: policy-engine.log / any *.log under the run may have the
        // "🚫 KILLED" banner ActPlane prints to stderr.
        if let Some(line) = synthesize_violation_from_kill_banner(&run.dir) {
            collected.push_str(&line);
            collected.push('\n');
        }
    }
    if collected.is_empty() {
        return;
    }
    let dest = run.violations_path();
    let mut existing = std::fs::read_to_string(&dest).unwrap_or_default();
    if !existing.is_empty() && !existing.ends_with('\n') {
        existing.push('\n');
    }
    existing.push_str(&collected);
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
        // Also accept events.jsonl directly under a leaf (not only one level deep).
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

/// Best-effort parse of ActPlane feedback.txt into one violations.jsonl line.
fn synthesize_violation_from_feedback_tree(root: &Path) -> Option<String> {
    // Walk one or two levels for feedback.txt.
    let mut candidates = vec![root.join("feedback.txt")];
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            candidates.push(e.path().join("feedback.txt"));
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

/// Parse ActPlane's human feedback block into a minimal JSON violation.
///
/// Example text:
/// ```text
/// [ActPlane] Operation killed by rule `destructive-vcs`.
/// - Target operation: exec /usr/bin/git
/// - Reason: Force-pushing...
/// ```
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

/// Parse ActPlane's console kill banner into a violation line.
///
/// ```text
/// 🚫 KILLED: process 'git' (pid 58816, ppid 58060) — /usr/bin/git
///    effect: kill
///    reason: Force-pushing...
/// ```
fn parse_kill_banner(text: &str) -> Option<String> {
    let mut target = None;
    let mut reason = None;
    let mut effect = None;
    let mut saw_killed = false;
    for line in text.lines() {
        let l = line.trim();
        if l.contains("KILLED:") {
            saw_killed = true;
            effect = Some("kill");
            if let Some(idx) = l.rfind('—').or_else(|| l.rfind('-')) {
                let t =
                    l[idx + l[idx..].chars().next().map(|c| c.len_utf8()).unwrap_or(1)..].trim();
                if !t.is_empty() {
                    target = Some(t.to_string());
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
    // Rule name is not always on the banner; prefer destructive-vcs when the
    // reason mentions force-push (the demo's known kill).
    let reason = reason.unwrap_or_default();
    let rule = if reason.to_ascii_lowercase().contains("force")
        || reason.to_ascii_lowercase().contains("hard-reset")
    {
        "destructive-vcs"
    } else {
        "policy"
    };
    let target = target.unwrap_or_default();
    let effect = effect.unwrap_or("kill");
    Some(flat_violation_json(rule, effect, &target, &reason))
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

/// Collect evidence, finalize the manifest, render the report, choose the code.
fn finish(ctx: &Ctx, run: &mut Run, exit: i32, fail_on_violation: bool) -> Result<i32> {
    run.manifest.exit_code = Some(exit);
    run.manifest.ended_at = Some(now_rfc3339());

    let ev = Evidence::collect(run).unwrap_or_default();
    let duration = run.manifest.summary.duration_seconds;
    run.manifest.summary = ev.summary.clone();
    run.manifest.summary.duration_seconds = duration;

    // Honesty rule: never leave the evidence plane as Active when it produced
    // no observational data. An empty report labeled "Active" is worse than a
    // clear Degraded reason.
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

/// Launch the agent under `actplane run` on the host and wait with idle reaping.
///
/// Stderr is tee'd to `log_path` (and the terminal) so kill banners can be
/// harvested even when ActPlane's events.jsonl flush races teardown.
fn run_host_policy_wrap(
    actplane: &Path,
    policy_yaml: &Path,
    agent: &[String],
    log_path: &Path,
    limit: Option<Duration>,
    quiet: bool,
) -> Result<i32> {
    let argv = PolicyPlane::wrap_argv(actplane, policy_yaml, agent);
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("empty actplane wrap argv"))?;

    let log = std::fs::File::create(log_path)
        .with_context(|| format!("creating {}", log_path.display()))?;
    // Duplicate stderr to both the terminal and the log file via a pipe + tee thread.
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .env("ACTPLANE_HOOK_PROFILE", "full")
        .spawn()
        .with_context(|| format!("spawning host policy wrap `{program}`"))?;

    let wrap_pid = child.id() as i32;
    if let Some(mut stderr) = child.stderr.take() {
        let mut log = log;
        let mut terminal = std::io::stderr();
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut stderr, &mut TeeWriter(&mut log, &mut terminal));
        });
    }

    let code = wait_for_wrap_pid(&mut child, wrap_pid, limit, quiet)?;
    // Prefer the code from wait_for_wrap_pid (which maps idle-reap of a stuck
    // actplane wrapper to 0 so short agents like `echo` keep exit 0). Only
    // use a later wait status if we have not already decided.
    // Child::drop calls blocking wait(); forget if still live.
    match child.try_wait() {
        Ok(Some(st)) => {
            let st_code = exit_status_code(st);
            if code == 0 && st_code >= 128 {
                // We SIGTERM'd the wrapper after the agent finished.
                Ok(0)
            } else {
                Ok(st_code)
            }
        }
        Ok(None) => {
            std::mem::forget(child);
            Ok(code)
        }
        Err(_) => {
            std::mem::forget(child);
            Ok(code)
        }
    }
}

/// Dual writer: log file + terminal.
struct TeeWriter<'a>(&'a mut std::fs::File, &'a mut std::io::Stderr);

impl std::io::Write for TeeWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.0.write_all(buf);
        self.1.write_all(buf)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let _ = self.0.flush();
        self.1.flush()
    }
}

/// Wait on a host-wrap child by pid tree, with wall-clock and idle bounds.
fn wait_for_wrap_pid(
    child: &mut std::process::Child,
    wrap_pid: i32,
    limit: Option<Duration>,
    quiet: bool,
) -> Result<i32> {
    let started = Instant::now();
    let deadline = limit.map(|l| Instant::now() + l);
    let mut idle_since: Option<Instant> = None;
    // Allow ActPlane eBPF setup before treating "no agent" as idle.
    const START_GRACE: Duration = Duration::from_millis(2500);
    // Continuous period with no non-wrapper descendant before we reap.
    const WRAP_IDLE: Duration = Duration::from_millis(1200);

    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(exit_status_code(status)),
            Ok(None) => {}
            Err(e) => return Err(e).context("polling host policy wrap"),
        }

        if wrap_tree_has_agent(wrap_pid) {
            idle_since = None;
        } else if started.elapsed() >= START_GRACE {
            // No agent in the tree (either it never appeared, or it already
            // exited between polls — including very short commands like echo).
            let since = *idle_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= WRAP_IDLE {
                if !quiet {
                    eprintln!(
                        "{}",
                        ui::warn(
                            "actplane run outlived the agent; terminating the policy wrapper so the run can finish"
                        )
                    );
                }
                std::thread::sleep(Duration::from_millis(400));
                // The agent already finished; do not surface SIGTERM(143) from
                // killing the stuck wrapper as the run's exit code.
                let _ = terminate_child(child)?;
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
                return terminate_child(child);
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

fn terminate_child(child: &mut std::process::Child) -> Result<i32> {
    let pid = child.id() as i32;
    // SAFETY: positive pid; ESRCH is fine.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let grace = Instant::now() + Duration::from_millis(1000);
    while Instant::now() < grace {
        match child.try_wait() {
            Ok(Some(st)) => return Ok(exit_status_code(st)),
            Ok(None) => std::thread::sleep(Duration::from_millis(40)),
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
    // Caller forgets an unreaped child so Drop::wait cannot hang the run.
    Ok(137)
}

/// Wait for the sandboxed process, with optional wall-clock limit and
/// host-wrap idle reaping.
///
/// When `reap_idle_wrap` is true the sandbox child is `actplane run` (possibly
/// under `sudo`). ActPlane 0.1.8 often keeps that process alive after the agent
/// exits (stuck singleton event-loop join). Once the wrap tree contains only
/// wrapper processes (`sudo` / `actplane`) for a short idle window, we
/// SIGTERM/SIGKILL it so the run always reaches `finish()`.
fn wait_for_agent(
    sandbox: &mut dyn Sandbox,
    limit: Option<Duration>,
    quiet: bool,
    reap_idle_wrap: bool,
) -> Result<i32> {
    let started = Instant::now();
    let deadline = limit.map(|l| Instant::now() + l);
    let mut idle_since: Option<Instant> = None;
    const START_GRACE: Duration = Duration::from_millis(2500);
    const WRAP_IDLE: Duration = Duration::from_millis(1200);

    loop {
        if let Some(code) = sandbox.try_wait()? {
            return Ok(code);
        }

        if reap_idle_wrap {
            if let Some(pid) = sandbox.host_pid() {
                if wrap_tree_has_agent(pid) {
                    idle_since = None;
                } else if started.elapsed() >= START_GRACE {
                    let since = *idle_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= WRAP_IDLE {
                        if !quiet {
                            eprintln!(
                                "{}",
                                ui::warn(
                                    "actplane run outlived the agent; terminating the policy wrapper so the run can finish"
                                )
                            );
                        }
                        std::thread::sleep(Duration::from_millis(400));
                        let _ = terminate_sandbox(sandbox)?;
                        return Ok(0);
                    }
                }
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
                return terminate_sandbox(sandbox);
            }
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}

/// SIGTERM then SIGKILL the sandbox child; never block forever on wait.
fn terminate_sandbox(sandbox: &mut dyn Sandbox) -> Result<i32> {
    let _ = sandbox.signal(libc::SIGTERM);
    let grace = Instant::now() + Duration::from_millis(1000);
    while Instant::now() < grace {
        if let Some(code) = sandbox.try_wait()? {
            return Ok(code);
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    let _ = sandbox.signal(libc::SIGKILL);
    let kill_grace = Instant::now() + Duration::from_millis(500);
    while Instant::now() < kill_grace {
        if let Some(code) = sandbox.try_wait()? {
            return Ok(code);
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    // Last resort: do not hang the product on an unreapable child.
    Ok(137)
}

/// True when the process tree under `root_pid` still contains a non-wrapper
/// process (the agent or its subprocesses). Wrappers are `sudo` and `actplane`.
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

/// All descendant pids of `root` (not including `root`).
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

/// Compose the policy DSL from the configured packs and files.
fn compose(cfg: &Config, workspace: &str) -> Result<String> {
    let mut extra = Vec::new();
    for path in &cfg.policy.files {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading the policy file {path}"))?;
        extra.push((path.clone(), text));
    }
    embedded::compose_policy(&cfg.policy.packs, &extra, workspace)
}

/// Pick the backend, honoring `auto` and the strict profile's refusal of host mode.
fn resolve_backend(cfg: &Config) -> Result<Backend> {
    let requested = cfg.sandbox.backend.trim().to_ascii_lowercase();
    let strict = cfg.profile == "strict";

    if requested != "auto" {
        let backend = parse_backend(&requested)?;
        if strict && backend == Backend::Host {
            bail!(
                "the `strict` profile requires a sandbox, and `--sandbox host` has none.\n\
                 Install Docker, Podman, or bubblewrap, or use `--profile balanced`."
            );
        }
        return Ok(backend);
    }

    let available = Backend::detect_available();
    let chosen = available
        .iter()
        .copied()
        .find(|b| !strict || *b != Backend::Host);

    match chosen {
        Some(b) => Ok(b),
        None => bail!(
            "the `strict` profile requires a sandbox, but no container runtime or \
             bubblewrap was found.\n\
             Install Docker, Podman, or bubblewrap, or use `--profile balanced`."
        ),
    }
}

fn parse_backend(name: &str) -> Result<Backend> {
    Ok(match name {
        "docker" => Backend::Docker,
        "podman" => Backend::Podman,
        "bwrap" | "bubblewrap" => Backend::Bwrap,
        "host" | "none" => Backend::Host,
        other => {
            bail!("unknown sandbox backend `{other}`. Use auto, docker, podman, bwrap, or host.")
        }
    })
}

/// A note recorded in the manifest when the isolation plane is not real.
fn spec_note(backend: Backend) -> Option<String> {
    match backend {
        Backend::Host => Some("no isolation: the agent ran directly on the host".into()),
        Backend::Bwrap => Some("namespace isolation only; no container runtime".into()),
        _ => None,
    }
}

/// Translate the config into a sandbox spec.
fn build_spec(cfg: &Config, run: &Run, cwd: &Path, backend: Backend) -> Result<SandboxSpec> {
    let mut spec = SandboxSpec::new(format!("actime-{}", run.id), cfg.sandbox.image.clone());

    // Container backends chdir to the guest workdir (default `/workspace`).
    // Host and bwrap run against real host paths: the agent must inherit the
    // caller's cwd, never a leftover host `/workspace` from a container mount.
    match backend {
        Backend::Docker | Backend::Podman => {
            spec.workdir = PathBuf::from(&cfg.sandbox.workdir);
        }
        Backend::Bwrap => {
            // bwrap still uses guest paths for --chdir once mounts are applied.
            spec.workdir = PathBuf::from(&cfg.sandbox.workdir);
        }
        Backend::Host => {
            spec.workdir = cwd.to_path_buf();
        }
    }

    let mut mounts = Vec::new();
    for m in &cfg.sandbox.mounts {
        mounts.push(Mount::parse(m).with_context(|| format!("parsing the mount `{m}`"))?);
    }
    spec.mounts = mounts;
    spec.resolve_mount_hosts(cwd);

    spec.network = match cfg.sandbox.network {
        actime_core::config::NetworkMode::Allow => NetworkMode::Allow,
        actime_core::config::NetworkMode::Deny => NetworkMode::Deny,
        actime_core::config::NetworkMode::Egress => NetworkMode::Egress,
    };
    spec.allow_egress = cfg.sandbox.allow_egress.clone();
    spec.cpus = cfg.sandbox.cpus;
    spec.memory = cfg.sandbox.memory.clone();
    spec.keep = cfg.sandbox.keep;

    // Only named variables cross the boundary, and only when they are set.
    let mut env: Vec<(String, String)> = Vec::new();
    for name in &cfg.sandbox.env_passthrough {
        if let Ok(v) = std::env::var(name) {
            env.push((name.clone(), v));
        }
    }
    env.push(("ACTIME_RUN_ID".into(), run.id.to_string()));
    if backend == Backend::Docker || backend == Backend::Podman {
        env.push(("ACTIME_WORKSPACE".into(), cfg.sandbox.workdir.clone()));
    } else {
        env.push(("ACTIME_WORKSPACE".into(), cwd.display().to_string()));
    }
    // Widen ActPlane's hook budget when the agent is launched under
    // `actplane run` (host wrap path inherits this env).
    env.push(("ACTPLANE_HOOK_PROFILE".into(), "full".into()));
    spec.env = env;

    Ok(spec)
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

/// `actime shell` — an interactive shell inside the sandbox, same planes.
pub fn shell(ctx: &Ctx, sandbox: Option<String>, image: Option<String>) -> Result<i32> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let argv = vec![shell];
    run(
        ctx,
        RunRequest {
            argv,
            sandbox,
            policy: None,
            image,
            no_evidence: false,
            no_history: true,
            fail_on_violation: false,
            timeout: None,
        },
    )
}

/// `actime attach` — bind the planes to an agent that is already running.
pub fn attach(
    ctx: &Ctx,
    pid: Option<i32>,
    comm: Option<String>,
    policy: Option<String>,
) -> Result<i32> {
    let cwd = std::env::current_dir().context("resolving the current directory")?;
    let mut cfg = ctx.load_config(&cwd)?;
    if let Some(mode) = policy {
        cfg.policy.mode = mode.parse().map_err(|e| anyhow::anyhow!("{e}"))?;
    }

    let pid = match (pid, comm.as_deref()) {
        (Some(pid), _) => pid,
        (None, Some(comm)) => find_pid_by_comm(comm)?,
        (None, None) => bail!("give either --pid or --comm"),
    };

    let components = Components::detect();
    let store = RunStore::open_default()?;
    let argv = vec![format!("attach:{pid}")];
    let mut run = store.create(&argv, &cfg)?;

    run.manifest.planes.isolation =
        PlaneState::Disabled("attach binds an existing process; it does not isolate it".into());

    let dsl = compose(&cfg, &cwd.display().to_string())?;
    let packs = cfg.policy.packs.join(", ");
    let mut policy_plane = PolicyPlane::start(PolicyPlaneSpec {
        binary: components.actplane.path.as_deref(),
        version: components.actplane.version.as_deref(),
        mode: &cfg.policy.mode.to_string(),
        dsl: &dsl,
        packs: &packs,
        policy_yaml: run.policy_path(),
        violations: run.violations_path(),
        feedback: run.dir.join("feedback.txt"),
        audit: run.dir.join("policy-audit.jsonl"),
        host_pid: Some(pid),
        wrap_command: false,
        feedback_enabled: cfg.policy.feedback,
        log: run.dir.join("policy-engine.log"),
    });
    run.manifest.planes.policy = to_plane_state(&policy_plane.outcome);

    let mut evidence = EvidencePlane::start(EvidencePlaneSpec {
        binary: components.agentsight.path.as_deref(),
        version: components.agentsight.version.as_deref(),
        enabled: cfg.evidence.enabled,
        target: None,
        host_pid: Some(pid),
        db: run.evidence_db_path(),
        log: run.dir.join("evidence-engine.log"),
    });
    run.manifest.planes.evidence = to_plane_state(&evidence.outcome);
    let _ = run.save_manifest();

    eprintln!(
        "{}",
        ui::banner(
            run.id.as_str(),
            &format!("attached pid {pid}"),
            &cfg.policy.mode.to_string(),
            if evidence.outcome.is_active() {
                "on"
            } else {
                "off"
            },
        )
    );
    eprintln!(
        "{}",
        ui::note(
            "attach is post-hoc: it binds future events from this process tree, but it \
             cannot reconstruct what happened before now. Press Ctrl-C to detach."
        )
    );

    // Hold the planes open until the target exits or the user detaches.
    while process_is_alive(pid) {
        std::thread::sleep(Duration::from_millis(500));
    }

    policy_plane.stop();
    evidence.stop();
    finish(ctx, &mut run, 0, false)
}

/// `actime demo` — the bundled stand-in agent, end to end.
pub fn demo(ctx: &Ctx, sandbox: Option<String>, policy: &str) -> Result<i32> {
    let dir = std::env::temp_dir().join(format!("actime-demo-{}", std::process::id()));
    std::fs::create_dir_all(&dir).context("creating the demo directory")?;
    // When actime runs under sudo, ActPlane drops the wrapped agent back to
    // SUDO_UID. The scratch dir must be writable by that user or the demo
    // agent cannot create its files.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777));
        if let (Ok(uid), Ok(gid)) = (std::env::var("SUDO_UID"), std::env::var("SUDO_GID")) {
            if let (Ok(uid), Ok(gid)) = (uid.parse::<u32>(), gid.parse::<u32>()) {
                // SAFETY: chown on a path we just created; failure is non-fatal.
                unsafe {
                    let c = std::ffi::CString::new(dir.display().to_string()).ok();
                    if let Some(c) = c {
                        let _ = libc::chown(c.as_ptr(), uid, gid);
                    }
                }
            }
        }
    }
    // The script is named `actime-demo-agent` on purpose: the shipped policy
    // packs list that name as an AGENT source, so the demo exercises real rules.
    let script = dir.join("actime-demo-agent");
    {
        let mut f = std::fs::File::create(&script)
            .with_context(|| format!("writing {}", script.display()))?;
        f.write_all(embedded::DEMO_AGENT.as_bytes())?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    }

    if !ctx.quiet {
        eprintln!(
            "{}",
            ui::note(&format!(
                "running the bundled stand-in agent in {}",
                dir.display()
            ))
        );
    }

    // Run the demo from the scratch dir so the stand-in agent can write its
    // files regardless of what the caller's cwd is (or whether /workspace
    // exists on the host). Use a *relative* argv so the same path works on
    // the host and inside a container where `.` is mounted at `/workspace`.
    let prev = std::env::current_dir().ok();
    if let Err(e) = std::env::set_current_dir(&dir) {
        bail!("cannot enter demo directory {}: {e}", dir.display());
    }

    let demo_argv = vec!["./actime-demo-agent".to_string()];

    let req = RunRequest {
        argv: demo_argv.clone(),
        sandbox: sandbox.clone(),
        policy: Some(policy.to_string()),
        image: None,
        no_evidence: false,
        // Skip history in the demo so a stuck akeep vault cannot delay the
        // out-of-box experience (akeep is still hard-bounded on normal runs).
        no_history: true,
        fail_on_violation: false,
        // Hard ceiling so a stuck wrap engine can never hang forever.
        timeout: Some("90s".into()),
    };

    let result = match run(ctx, req) {
        Err(e) if should_fallback_demo_to_host(sandbox.as_deref(), &e) => {
            if !ctx.quiet {
                eprintln!(
                    "{}",
                    ui::warn(&format!(
                        "docker/podman sandbox could not start ({e:#}); falling back to --sandbox host"
                    ))
                );
            }
            run(
                ctx,
                RunRequest {
                    argv: demo_argv,
                    sandbox: Some("host".into()),
                    policy: Some(policy.to_string()),
                    image: None,
                    no_evidence: false,
                    no_history: true,
                    fail_on_violation: false,
                    timeout: Some("90s".into()),
                },
            )
        }
        other => other,
    };

    if let Some(p) = prev {
        let _ = std::env::set_current_dir(p);
    }
    result
}

/// Whether a failed demo under docker/podman should retry on the host backend.
fn should_fallback_demo_to_host(requested: Option<&str>, err: &anyhow::Error) -> bool {
    let backend = requested.unwrap_or("auto");
    // Explicit host requests never fall back (they already are host).
    if matches!(backend, "host" | "none" | "bwrap" | "bubblewrap") {
        return false;
    }
    let msg = format!("{err:#}").to_ascii_lowercase();
    msg.contains("sandbox image")
        || msg.contains("unable to find image")
        || msg.contains("manifest unknown")
        || msg.contains("no such image")
        || msg.contains("could not be pulled")
        || msg.contains("starting the docker sandbox")
        || msg.contains("starting the podman sandbox")
}

/// Find the newest pid whose `comm` matches, so `--comm claude` picks the agent
/// the user just started rather than one from an hour ago.
fn find_pid_by_comm(comm: &str) -> Result<i32> {
    let mut best: Option<(u64, i32)> = None;
    let entries = std::fs::read_dir("/proc").context("reading /proc")?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let Ok(found) = std::fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        if found.trim() != comm {
            continue;
        }
        let starttime = std::fs::read_to_string(entry.path().join("stat"))
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

/// Field 22 of `/proc/<pid>/stat` is the process start time. The comm field can
/// contain spaces and parentheses, so parse after the final `)`.
fn parse_starttime(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

fn process_is_alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Component versions as a map, for the manifest.
#[allow(dead_code)]
fn component_versions(c: &Components) -> BTreeMap<String, String> {
    c.iter()
        .filter_map(|c| c.version.as_ref().map(|v| (c.name.to_string(), v.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_names_map_to_variants() {
        assert_eq!(parse_backend("docker").expect("docker"), Backend::Docker);
        assert_eq!(parse_backend("podman").expect("podman"), Backend::Podman);
        assert_eq!(parse_backend("bwrap").expect("bwrap"), Backend::Bwrap);
        assert_eq!(
            parse_backend("bubblewrap").expect("bubblewrap"),
            Backend::Bwrap
        );
        assert_eq!(parse_backend("host").expect("host"), Backend::Host);
        assert_eq!(parse_backend("none").expect("none"), Backend::Host);
    }

    #[test]
    fn an_unknown_backend_lists_the_valid_ones() {
        let err = parse_backend("firecracker").unwrap_err().to_string();
        assert!(err.contains("firecracker"));
        assert!(err.contains("docker"));
        assert!(err.contains("host"));
    }

    #[test]
    fn strict_refuses_host_mode() {
        let mut cfg = Config::builtin_profile("strict").expect("strict profile");
        cfg.sandbox.backend = "host".into();
        let err = resolve_backend(&cfg).unwrap_err().to_string();
        assert!(err.contains("strict"));
        assert!(err.contains("requires a sandbox"));
    }

    #[test]
    fn balanced_allows_host_mode() {
        let mut cfg = Config::builtin_profile("balanced").expect("balanced profile");
        cfg.sandbox.backend = "host".into();
        assert_eq!(resolve_backend(&cfg).expect("host"), Backend::Host);
    }

    #[test]
    fn the_workspace_note_marks_host_mode_as_unisolated() {
        assert!(spec_note(Backend::Host)
            .expect("note")
            .contains("no isolation"));
        assert!(spec_note(Backend::Bwrap).is_some());
        assert!(spec_note(Backend::Docker).is_none());
    }

    #[test]
    fn proc_stat_starttime_survives_a_comm_with_spaces() {
        // A synthetic stat line: pid, "(weird name)", state, then 19 more
        // fields before starttime at field 22.
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
    fn a_sandboxed_run_composes_policy_against_the_guest_workspace() {
        let cfg = Config::builtin_profile("balanced").expect("balanced");
        let dsl = compose(&cfg, "/workspace").expect("compose");
        assert!(dsl.contains("/workspace"));
        assert!(!dsl.contains("${WORKSPACE}"));
    }

    #[test]
    fn demo_falls_back_to_host_on_missing_image() {
        assert!(should_fallback_demo_to_host(
            Some("docker"),
            &anyhow::anyhow!(
                "starting the docker sandbox: sandbox image `ghcr.io/eunomia-bpf/actime-sandbox:latest` is not available locally and could not be pulled."
            )
        ));
        assert!(should_fallback_demo_to_host(
            Some("auto"),
            &anyhow::anyhow!("Unable to find image 'x' locally")
        ));
        // Explicit host never "falls back".
        assert!(!should_fallback_demo_to_host(
            Some("host"),
            &anyhow::anyhow!("sandbox image missing")
        ));
        assert!(!should_fallback_demo_to_host(
            Some("docker"),
            &anyhow::anyhow!("permission denied while trying to connect to the docker API")
        ));
    }

    #[test]
    fn host_build_spec_uses_caller_cwd_not_guest_workspace() {
        let cfg = Config::builtin_profile("balanced").expect("balanced");
        let cwd = PathBuf::from("/tmp/actime-host-cwd-test");
        // Build a synthetic Run without touching the store.
        let run = actime_core::run::Run {
            id: actime_core::run::RunId("test-run".into()),
            dir: PathBuf::from("/tmp"),
            manifest: actime_core::run::Manifest::new(
                "test-run",
                &["echo".into()],
                &cfg,
                cwd.clone(),
            ),
        };
        let spec = build_spec(&cfg, &run, &cwd, Backend::Host).expect("spec");
        assert_eq!(spec.workdir, cwd);
        let docker = build_spec(&cfg, &run, &cwd, Backend::Docker).expect("spec");
        assert_eq!(docker.workdir, PathBuf::from(&cfg.sandbox.workdir));
    }

    #[test]
    fn parse_ppid_from_stat_handles_comm_with_spaces() {
        // pid (comm with spaces) state ppid ...
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
}
