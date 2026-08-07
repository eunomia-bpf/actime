//! Everything that is not `run` / `attach`: init, status, runs, report, policy,
//! keep, and doctor.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context as _, Result};

use actime_core::components::Components;
use actime_core::config::Config;
use actime_core::doctor::{self, CheckStatus};
use actime_core::observations::Observations;
use actime_core::report;
use actime_core::run::RunStore;

use crate::embedded;
use crate::planes;
use crate::ui;

/// Flags that apply to every subcommand.
pub struct Context {
    /// `--config`.
    pub config_path: Option<PathBuf>,
    /// `--profile`.
    pub profile: Option<String>,
    /// `--quiet`.
    pub quiet: bool,
}

impl Context {
    /// Load the config for `cwd`, applying `--config` and `--profile`.
    pub fn load_config(&self, cwd: &std::path::Path) -> Result<Config> {
        let mut cfg = match &self.profile {
            // An explicit --profile starts from that profile rather than from a
            // discovered file, so `--profile strict` cannot be quietly softened
            // by an actime.yaml sitting in the repository.
            Some(name) => Config::builtin_profile(name).with_context(|| {
                format!("unknown profile `{name}`. Use observe, balanced, or strict.")
            })?,
            None => Config::load(self.config_path.as_deref(), cwd)?,
        };
        if let Some(name) = &self.profile {
            cfg.profile = name.clone();
        }
        Ok(cfg)
    }
}

/// `actime init` — write a starter actime.yaml.
pub fn init(ctx: &Context, force: bool, print: bool) -> Result<i32> {
    let profile = ctx.profile.as_deref().unwrap_or("balanced");
    let yaml = embedded::profile(profile).ok_or_else(|| {
        anyhow::anyhow!("unknown profile `{profile}`. Use observe, balanced, or strict.")
    })?;

    if print {
        print!("{yaml}");
        return Ok(0);
    }

    let path = PathBuf::from("actime.yaml");
    if path.exists() && !force {
        bail!(
            "actime.yaml already exists. Use --force to overwrite it, or --print to see \
             the {profile} profile without writing anything."
        );
    }
    std::fs::write(&path, yaml).with_context(|| format!("writing {}", path.display()))?;

    eprintln!("wrote actime.yaml ({profile} profile)");
    eprintln!();
    eprintln!("Next:");
    eprintln!("  actime doctor         see which planes this machine supports");
    eprintln!("  actime run -- claude  run your agent under the three planes");
    Ok(0)
}

/// `actime status` — runs that are still in progress.
pub fn status(_ctx: &Context) -> Result<i32> {
    let store = RunStore::open_default()?;
    let manifests = store.list()?;
    let live: Vec<_> = manifests.iter().filter(|m| m.ended_at.is_none()).collect();

    if live.is_empty() {
        println!("No runs in progress.");
        println!();
        println!("{}", ui::dim("`actime runs` lists finished runs."));
        return Ok(0);
    }

    println!("{:<26} {:<12} {:<12} STARTED", "RUN", "AGENT", "TARGET");
    for m in live {
        println!(
            "{:<26} {:<12} {:<12} {}",
            m.id, m.agent, m.target.kind, m.started_at
        );
    }
    Ok(0)
}

/// `actime runs` — recorded runs, newest first.
pub fn runs(_ctx: &Context, json: bool, limit: usize) -> Result<i32> {
    let store = RunStore::open_default()?;
    let mut manifests = store.list()?;
    manifests.truncate(limit);

    if json {
        println!("{}", serde_json::to_string_pretty(&manifests)?);
        return Ok(0);
    }

    if manifests.is_empty() {
        println!("No runs recorded yet.");
        println!();
        println!("{}", ui::dim("Try `actime run -- /bin/echo hi`."));
        return Ok(0);
    }

    println!(
        "{:<26} {:<10} {:<10} {:<8} {:<6} VIOLATIONS",
        "RUN", "AGENT", "TARGET", "POLICY", "EXIT"
    );
    for m in &manifests {
        println!(
            "{:<26} {:<10} {:<10} {:<8} {:<6} {}",
            m.id,
            truncate(&m.agent, 10),
            truncate(&m.target.kind, 10),
            truncate(m.planes.policy.label(), 8),
            m.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into()),
            m.summary.violations,
        );
    }
    Ok(0)
}

/// `actime report` — the unified report for one run.
pub fn report(_ctx: &Context, id: &str, json: bool, markdown: bool) -> Result<i32> {
    let store = RunStore::open_default()?;
    let run = store.get(id).with_context(|| {
        if id == "latest" {
            "no runs recorded yet. Try `actime run -- /bin/echo hi`.".to_string()
        } else {
            format!("no run `{id}`. Use `actime runs` to list them.")
        }
    })?;
    let ev = Observations::collect(&run).unwrap_or_default();

    if json {
        println!("{}", report::render_json(&run, &ev)?);
    } else if markdown {
        print!("{}", report::render_markdown(&run, &ev));
    } else {
        print!("{}", report::render_text(&run, &ev, ui::width()));
    }
    Ok(0)
}

/// `actime policy list`.
pub fn policy_list() -> Result<i32> {
    println!("Policy packs shipped with actime:");
    println!();
    for p in embedded::PACKS {
        println!("  {}", ui::bold(p.name));
        println!("      {}", p.summary);
        println!(
            "      {} rules",
            p.source
                .lines()
                .filter(|l| l.trim_start().starts_with("rule "))
                .count()
        );
        println!();
    }
    println!(
        "{}",
        ui::dim("`actime policy show <pack>` prints the rules.")
    );
    Ok(0)
}

/// `actime policy show`.
pub fn policy_show(name: &str) -> Result<i32> {
    let pack = embedded::pack(name).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown pack `{name}`. Known packs: {}",
            embedded::PACKS
                .iter()
                .map(|p| p.name)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    print!("{}", pack.source);
    Ok(0)
}

/// `actime policy check` — compile without loading. Needs no privileges.
pub fn policy_check(ctx: &Context) -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let cfg = ctx.load_config(&cwd)?;
    let components = Components::detect();

    let workspace = cwd.display().to_string();
    let mut extra = Vec::new();
    for path in &cfg.policy.files {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading the policy file {path}"))?;
        extra.push((path.clone(), text));
    }
    let dsl = embedded::compose_policy(&cfg.policy.packs, &extra, &workspace)?;

    let Some(binary) = components.actplane.path.as_deref() else {
        println!(
            "{}",
            ui::warn(
                "actplane is not installed, so the policy could only be composed, not compiled."
            )
        );
        println!("  install it with: cargo install actplane");
        println!();
        println!(
            "Composed {} lines of policy from: {}",
            dsl.lines().count(),
            cfg.policy.packs.join(", ")
        );
        return Ok(0);
    };

    let version = components.actplane.version.as_deref();
    // ActPlane ≥ 0.1.8 accepts `compile --json`. Older builds (e.g. 0.1.5)
    // require `--out` and have no `--json`; degrade with a clear message.
    let supports_json = version
        .map(|v| actime_core::components::compare_semver(v, "0.1.8") >= 0)
        .unwrap_or(true);

    if !supports_json {
        println!(
            "{}",
            ui::warn(&format!(
                "actplane {} at {} is older than 0.1.8 and does not support `compile --json`.",
                version.unwrap_or("?"),
                binary.display()
            ))
        );
        println!(
            "  composed {} lines from: {}",
            dsl.lines().count(),
            cfg.policy.packs.join(", ")
        );
        println!(
            "  upgrade with: cargo install actplane  (need ≥ 0.1.8), or put a newer build first on PATH"
        );
        // Still try a best-effort compile to a temp file so users learn if the
        // DSL itself is rejected by this older binary.
        let tmp =
            std::env::temp_dir().join(format!("actime-policy-check-{}.bin", std::process::id()));
        let out = Command::new(binary)
            .args(["--rule", &dsl, "compile", "--out"])
            .arg(&tmp)
            .output()
            .context("running `actplane compile`")?;
        let _ = std::fs::remove_file(&tmp);
        if out.status.success() {
            println!(
                "{} policy compiles on this older actplane (no JSON report available)",
                ui::green("ok")
            );
            return Ok(0);
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        println!(
            "{}",
            ui::warn(&format!(
                "older actplane rejected the policy (upgrade recommended):\n  {}",
                stderr.trim().lines().next().unwrap_or("(no detail)")
            ))
        );
        return Ok(0);
    }

    // `compile` never loads eBPF, so this is safe to run unprivileged and in CI.
    let out = Command::new(binary)
        .args(["--rule", &dsl, "compile", "--json"])
        .output()
        .context("running `actplane compile`")?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // Older binaries that report no version still fail on --json; degrade.
        if stderr.to_ascii_lowercase().contains("unexpected argument")
            || stderr.to_ascii_lowercase().contains("unrecognized")
            || String::from_utf8_lossy(&out.stdout)
                .to_ascii_lowercase()
                .contains("unexpected argument")
        {
            println!(
                "{}",
                ui::warn(
                    "this actplane does not support `compile --json` (need ≥ 0.1.8). \
                     Policy was composed but not machine-checked."
                )
            );
            println!(
                "  composed {} lines from: {}",
                dsl.lines().count(),
                cfg.policy.packs.join(", ")
            );
            println!("  upgrade with: cargo install actplane");
            return Ok(0);
        }
        bail!("the policy did not compile:\n{}", stderr.trim());
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    let warnings = parsed
        .get("warnings")
        .and_then(|w| w.as_array())
        .cloned()
        .unwrap_or_default();

    // Per-rule enforceability: compile --json (kernel_op, target_kind, patterns)
    // combined with this host's engine feature budget (ActPlane 0.1.8 pin).
    let rows =
        planes::assess_policy_with_compile(&parsed, components.actplane.version.as_deref(), None);
    // Fall back to DSL scan if the JSON had no rule list.
    let rows = if rows.is_empty() {
        planes::classify_rules(&dsl, None)
    } else {
        rows
    };

    let enforceable = rows.iter().filter(|r| r.enforceable).count();
    let total = rows.len();
    println!(
        "{} policy compiled from {} · {enforceable}/{total} rules enforceable on this host",
        ui::green("ok"),
        cfg.policy.packs.join(", ")
    );

    for w in &warnings {
        let msg = w
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unspecified warning");
        println!("{}", ui::warn(msg));
    }

    if !rows.is_empty() {
        println!();
        println!(
            "{:<24} {:<8} {:<12} REASON",
            "RULE", "EFFECT", "ENFORCEABLE"
        );
        for r in &rows {
            let mark = if r.enforceable {
                ui::green("yes")
            } else {
                ui::yellow("no")
            };
            let reason = if r.enforceable {
                String::new()
            } else {
                r.reason.clone()
            };
            println!("{:<24} {:<8} {:<12} {}", r.name, r.effect, mark, reason);
        }
        if rows.iter().any(|r| !r.enforceable) {
            println!();
            println!(
                "{}",
                ui::dim(
                    "`--policy enforce` fails closed if any rule is not enforceable. \
                     Drop those packs, or use `--policy observe`."
                )
            );
        }
    }

    // Warnings are informational: a policy that compiles with warnings still
    // loads, and `check` is meant to be safe to gate CI on. Unenforceable
    // rules do not fail `check` — that is what `enforce` is for.
    Ok(0)
}

/// `actime policy explain` — what this kernel can enforce before the fact.
pub fn policy_explain(ctx: &Context) -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let cfg = ctx.load_config(&cwd)?;
    let components = Components::detect();

    let Some(binary) = components.actplane.path.as_deref() else {
        bail!("actplane is not installed. Install it with: cargo install actplane");
    };

    let mut extra = Vec::new();
    for path in &cfg.policy.files {
        extra.push((path.clone(), std::fs::read_to_string(path)?));
    }
    let dsl = embedded::compose_policy(&cfg.policy.packs, &extra, &cwd.display().to_string())?;

    let status = Command::new(binary)
        .args(["--rule", &dsl, "compile", "--explain"])
        .status()
        .context("running `actplane compile --explain`")?;
    Ok(status.code().unwrap_or(1))
}

/// `actime keep commit`.
pub fn keep_commit(ctx: &Context, message: Option<String>) -> Result<i32> {
    let components = Components::detect();
    let msg = message.unwrap_or_else(|| "actime keep commit".to_string());
    let log = std::env::temp_dir().join("actime-keep.log");
    let (outcome, commit) =
        planes::BackupPlane::commit(components.akeep.path.as_deref(), true, &msg, &log);

    if outcome.is_active() {
        println!(
            "{} {}",
            ui::green("committed"),
            commit.unwrap_or_else(|| "(no id reported)".into())
        );
        Ok(0)
    } else {
        if !ctx.quiet {
            eprintln!("{}", ui::warn(outcome.detail()));
        }
        Ok(1)
    }
}

/// `actime keep log`.
pub fn keep_log() -> Result<i32> {
    let components = Components::detect();
    let Some(binary) = components.akeep.path.as_deref() else {
        bail!("akeep is not installed. Install it with: cargo install akeep");
    };
    let status = Command::new(binary).arg("log").status()?;
    Ok(status.code().unwrap_or(1))
}

/// `actime keep restore`.
pub fn keep_restore(_ctx: &Context, id: &str, to: Option<PathBuf>) -> Result<i32> {
    let components = Components::detect();
    let Some(binary) = components.akeep.path.as_deref() else {
        bail!("akeep is not installed. Install it with: cargo install akeep");
    };

    let store = RunStore::open_default()?;
    let run = store.get(id)?;
    let Some(commit) = run.manifest.akeep_commit.clone() else {
        bail!(
            "run {} has no session backup commit. The backup plane was {}.",
            run.id,
            run.manifest.planes.backup
        );
    };

    let target =
        to.unwrap_or_else(|| std::env::temp_dir().join(format!("actime-restore-{}", run.id)));
    let status = Command::new(binary)
        .args(["checkout", &commit, "--to"])
        .arg(&target)
        .status()
        .context("running `akeep checkout`")?;

    if status.success() {
        println!("restored {} into {}", commit, target.display());
    }
    Ok(status.code().unwrap_or(1))
}

/// `actime doctor`.
pub fn doctor(ctx: &Context, json: bool) -> Result<i32> {
    let cwd = std::env::current_dir()?;
    // A broken config must not stop the diagnosis; that is what doctor is for.
    let cfg = ctx.load_config(&cwd).unwrap_or_default();
    let checks = doctor::run_checks(&cfg);

    if json {
        println!("{}", serde_json::to_string_pretty(&checks)?);
        let failed = checks.iter().any(|c| c.status == CheckStatus::Fail);
        return Ok(if failed { 1 } else { 0 });
    }

    let mut failed = 0usize;
    let mut warned = 0usize;
    for c in &checks {
        let mark = match c.status {
            CheckStatus::Ok => ui::green("ok  "),
            CheckStatus::Warn => {
                warned += 1;
                ui::yellow("warn")
            }
            CheckStatus::Fail => {
                failed += 1;
                ui::red("fail")
            }
            CheckStatus::Skip => ui::dim("skip"),
        };
        println!("{mark}  {:<28} {}", c.name, c.detail);
        if let Some(fix) = &c.fix {
            println!("      {}", ui::dim(&format!("→ {fix}")));
        }
    }

    println!();
    if failed == 0 && warned == 0 {
        println!(
            "{}",
            ui::green("Every plane this machine can support is available.")
        );
    } else {
        println!(
            "{} check(s) failed, {} warning(s). Actime still runs: unavailable planes \
             degrade rather than stopping a run.",
            failed, warned
        );
    }
    if !planes::is_root() {
        println!();
        println!(
            "{}",
            ui::dim(
                "Not running as root. The policy and observability planes will ask for sudo \
                 when a run starts."
            )
        );
    }

    Ok(if failed > 0 { 1 } else { 0 })
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max.saturating_sub(1)).chain(['…']).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_strings_and_marks_long_ones() {
        assert_eq!(truncate("claude", 10), "claude");
        assert_eq!(truncate("a-very-long-agent-name", 8), "a-very-…");
    }

    #[test]
    fn an_explicit_profile_overrides_a_discovered_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("actime.yaml"),
            "version: 1\nprofile: observe\npolicy:\n  mode: observe\n",
        )
        .expect("write");

        let ctx = Context {
            config_path: None,
            profile: Some("strict".into()),
            quiet: true,
        };
        let cfg = ctx.load_config(dir.path()).expect("load");
        assert_eq!(cfg.profile, "strict");
        assert_eq!(cfg.policy.mode.to_string(), "enforce");
    }

    #[test]
    fn an_unknown_profile_is_a_clear_error() {
        let ctx = Context {
            config_path: None,
            profile: Some("paranoid".into()),
            quiet: true,
        };
        let err = ctx
            .load_config(std::path::Path::new("."))
            .unwrap_err()
            .to_string();
        assert!(err.contains("paranoid"));
    }
}
