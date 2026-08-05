//! Human and machine-readable run reports.
//!
//! - [`render_text`] — terminal summary (no ANSI colors)
//! - [`render_markdown`] — `report.md` body
//! - [`render_json`] — structured JSON for tooling

use std::fmt::Write as _;

use anyhow::{Context, Result};
use serde_json::json;

use crate::evidence::{Evidence, Violation};
use crate::run::{PlaneState, PlaneStatus, Run, RunSummary};

/// Render a clean terminal report.
///
/// Sections: Run header, Planes, Summary, Policy violations, Next steps.
/// Columns are aligned; text is truncated to `width`. No ANSI colors.
pub fn render_text(run: &Run, ev: &Evidence, width: usize) -> String {
    let width = width.max(40);
    let mut out = String::new();
    let rule = "-".repeat(width.min(72));

    // --- Run header ---
    let _ = writeln!(out, "Actime run report");
    let _ = writeln!(out, "{rule}");
    let _ = writeln!(out, "  Run id:     {}", run.manifest.id);
    let _ = writeln!(out, "  Agent:      {}", run.manifest.agent);
    let argv = truncate(&run.manifest.argv.join(" "), width.saturating_sub(14));
    let _ = writeln!(out, "  Argv:       {argv}");
    let dur = format_secs(
        ev.summary
            .duration_seconds
            .max(run.manifest.summary.duration_seconds),
    );
    let _ = writeln!(out, "  Duration:   {dur}");
    let exit = match run.manifest.exit_code {
        Some(c) => c.to_string(),
        None => "—".into(),
    };
    let _ = writeln!(out, "  Exit code:  {exit}");
    let _ = writeln!(out, "  Profile:    {}", run.manifest.profile);
    let _ = writeln!(out);

    // --- Target ---
    let _ = writeln!(out, "Target");
    let _ = writeln!(out, "{rule}");
    let t = &run.manifest.target;
    let _ = writeln!(out, "  kind:       {}", t.kind);
    if let Some(ref spec) = t.spec {
        let _ = writeln!(
            out,
            "  spec:       {}",
            truncate(spec, width.saturating_sub(14))
        );
    }
    if let Some(pid) = t.host_pid {
        let _ = writeln!(out, "  host_pid:   {pid}");
    }
    if let Some(ref et) = t.evidence_target {
        let _ = writeln!(out, "  evidence:   {et}");
    }
    if let Some(ref note) = t.note {
        let _ = writeln!(
            out,
            "  note:       {}",
            truncate(note, width.saturating_sub(14))
        );
    }
    let _ = writeln!(out);

    // --- Planes ---
    let _ = writeln!(out, "Planes");
    let _ = writeln!(out, "{rule}");
    render_plane_line(&mut out, "policy", &run.manifest.planes.policy, width);
    render_plane_line(&mut out, "evidence", &run.manifest.planes.evidence, width);
    render_plane_line(&mut out, "history", &run.manifest.planes.history, width);
    let _ = writeln!(out);

    // --- Summary ---
    let _ = writeln!(out, "Summary");
    let _ = writeln!(out, "{rule}");
    render_summary_text(&mut out, &ev.summary);
    let _ = writeln!(out);

    // --- Unenforceable rules (honest observe / mixed packs) ---
    if !run.manifest.unenforceable_rules.is_empty() {
        let _ = writeln!(
            out,
            "Unenforceable rules ({})",
            run.manifest.unenforceable_rules.len()
        );
        let _ = writeln!(out, "{rule}");
        let _ = writeln!(out, "  {:<24} {:<8} REASON", "RULE", "EFFECT");
        for r in &run.manifest.unenforceable_rules {
            let _ = writeln!(
                out,
                "  {:<24} {:<8} {}",
                truncate(&r.name, 24),
                truncate(&r.effect, 8),
                truncate(&r.reason, width.saturating_sub(36))
            );
        }
        let _ = writeln!(out);
    }

    // --- Violations table ---
    let _ = writeln!(out, "Policy violations ({})", ev.violations.len());
    let _ = writeln!(out, "{rule}");
    if ev.violations.is_empty() {
        let _ = writeln!(out, "  (none)");
    } else {
        render_violations_table(&mut out, &ev.violations, width);
    }
    let _ = writeln!(out);

    // --- Next steps ---
    let _ = writeln!(out, "Next steps");
    let _ = writeln!(out, "{rule}");
    for step in next_steps(run, ev) {
        let _ = writeln!(out, "  • {step}");
    }

    out
}

/// Render a Markdown report suitable for `report.md`.
pub fn render_markdown(run: &Run, ev: &Evidence) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Actime run `{}`", run.manifest.id);
    let _ = writeln!(out);
    let _ = writeln!(out, "## Run");
    let _ = writeln!(out);
    let _ = writeln!(out, "| Field | Value |");
    let _ = writeln!(out, "|-------|-------|");
    let _ = writeln!(out, "| Id | `{}` |", run.manifest.id);
    let _ = writeln!(out, "| Agent | {} |", run.manifest.agent);
    let _ = writeln!(
        out,
        "| Argv | `{}` |",
        escape_md(&run.manifest.argv.join(" "))
    );
    let dur = format_secs(
        ev.summary
            .duration_seconds
            .max(run.manifest.summary.duration_seconds),
    );
    let _ = writeln!(out, "| Duration | {dur} |");
    let exit = match run.manifest.exit_code {
        Some(c) => c.to_string(),
        None => "—".into(),
    };
    let _ = writeln!(out, "| Exit code | {exit} |");
    let _ = writeln!(out, "| Profile | {} |", run.manifest.profile);
    let _ = writeln!(out, "| Started | {} |", run.manifest.started_at);
    if let Some(ref ended) = run.manifest.ended_at {
        let _ = writeln!(out, "| Ended | {ended} |");
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "## Target");
    let _ = writeln!(out);
    let _ = writeln!(out, "| Field | Value |");
    let _ = writeln!(out, "|-------|-------|");
    let t = &run.manifest.target;
    let _ = writeln!(out, "| Kind | {} |", escape_md(&t.kind));
    if let Some(ref spec) = t.spec {
        let _ = writeln!(out, "| Spec | `{}` |", escape_md(spec));
    }
    if let Some(pid) = t.host_pid {
        let _ = writeln!(out, "| Host pid | {pid} |");
    }
    if let Some(ref et) = t.evidence_target {
        let _ = writeln!(out, "| Evidence target | `{}` |", escape_md(et));
    }
    if let Some(ref note) = t.note {
        let _ = writeln!(out, "| Note | {} |", escape_md(note));
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "## Planes");
    let _ = writeln!(out);
    let _ = writeln!(out, "| Plane | Status | Reason |");
    let _ = writeln!(out, "|-------|--------|--------|");
    for (name, state) in plane_pairs(&run.manifest.planes) {
        let reason = state.reason().unwrap_or("—");
        let _ = writeln!(
            out,
            "| {name} | {} | {} |",
            state.label(),
            escape_md(reason)
        );
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "## Summary");
    let _ = writeln!(out);
    let _ = writeln!(out, "| Metric | Value |");
    let _ = writeln!(out, "|--------|-------|");
    let s = &ev.summary;
    let _ = writeln!(out, "| Violations | {} |", s.violations);
    let _ = writeln!(out, "| Blocked | {} |", s.blocked);
    let _ = writeln!(out, "| Killed | {} |", s.killed);
    let _ = writeln!(out, "| Processes | {} |", s.processes);
    let _ = writeln!(out, "| Files written | {} |", s.files_written);
    let _ = writeln!(out, "| Endpoints | {} |", s.endpoints);
    let _ = writeln!(out, "| LLM calls | {} |", s.llm_calls);
    let _ = writeln!(out, "| Tokens in | {} |", s.tokens_in);
    let _ = writeln!(out, "| Tokens out | {} |", s.tokens_out);
    let _ = writeln!(out, "| Peak RSS | {} |", format_bytes(s.peak_rss_bytes));
    let _ = writeln!(out, "| CPU seconds | {:.2} |", s.cpu_seconds);
    let _ = writeln!(out);

    if !run.manifest.unenforceable_rules.is_empty() {
        let _ = writeln!(out, "## Unenforceable rules");
        let _ = writeln!(out);
        let refused = run
            .manifest
            .target
            .note
            .as_deref()
            .is_some_and(|n| n.contains("refused before agent launch"));
        if refused {
            let _ = writeln!(
                out,
                "These rules were in the composed policy but this host's ActPlane engine cannot install them. **The run was refused before the agent started** (fail-closed enforce)."
            );
        } else {
            let _ = writeln!(
                out,
                "These rules were in the composed policy but this host's ActPlane engine cannot install them. The observe/enforce run did not watch for them."
            );
        }
        let _ = writeln!(out);
        let _ = writeln!(out, "| Rule | Effect | Reason |");
        let _ = writeln!(out, "|------|--------|--------|");
        for r in &run.manifest.unenforceable_rules {
            let _ = writeln!(
                out,
                "| {} | {} | {} |",
                escape_md(&r.name),
                escape_md(&r.effect),
                escape_md(&truncate(&r.reason, 96))
            );
        }
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "## Policy violations");
    let _ = writeln!(out);
    if ev.violations.is_empty() {
        let _ = writeln!(out, "_None._");
    } else {
        let _ = writeln!(out, "| Rule | Effect | Target | Reason |");
        let _ = writeln!(out, "|------|--------|--------|--------|");
        for v in &ev.violations {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} |",
                escape_md(&v.rule),
                escape_md(&v.effect),
                escape_md(&truncate(&v.target, 48)),
                escape_md(&truncate(&v.reason, 64))
            );
        }
    }
    let _ = writeln!(out);

    if !ev.timeline.is_empty() {
        let _ = writeln!(out, "## Timeline");
        let _ = writeln!(out);
        for e in ev.timeline.iter().take(50) {
            let _ = writeln!(
                out,
                "- `{}` **{}** — {}",
                escape_md(&e.ts),
                escape_md(&e.kind),
                escape_md(&e.summary)
            );
        }
        if ev.timeline.len() > 50 {
            let _ = writeln!(out, "\n_…and {} more entries._", ev.timeline.len() - 50);
        }
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "## Next steps");
    let _ = writeln!(out);
    for step in next_steps(run, ev) {
        let _ = writeln!(out, "- {step}");
    }

    out
}

/// Serialize `{manifest, summary, violations, timeline}` as pretty JSON.
pub fn render_json(run: &Run, ev: &Evidence) -> Result<String> {
    let value = json!({
        "manifest": run.manifest,
        "summary": ev.summary,
        "violations": ev.violations,
        "timeline": ev.timeline,
    });
    serde_json::to_string_pretty(&value).context("serializing report JSON")
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn render_plane_line(out: &mut String, name: &str, state: &PlaneState, width: usize) {
    let label = state.label();
    let reason = state.reason().unwrap_or("");
    let head = format!("  {name:<10} {label:<10}");
    if reason.is_empty() {
        let _ = writeln!(out, "{head}");
    } else {
        let budget = width.saturating_sub(head.len() + 2);
        let r = truncate(reason, budget);
        let _ = writeln!(out, "{head} {r}");
    }
}

fn render_summary_text(out: &mut String, s: &RunSummary) {
    let _ = writeln!(
        out,
        "  violations={:<6} blocked={:<6} killed={}",
        s.violations, s.blocked, s.killed
    );
    let _ = writeln!(
        out,
        "  processes={:<7} files_written={:<5} endpoints={}",
        s.processes, s.files_written, s.endpoints
    );
    let _ = writeln!(
        out,
        "  llm_calls={:<7} tokens_in={:<8} tokens_out={}",
        s.llm_calls, s.tokens_in, s.tokens_out
    );
    let _ = writeln!(
        out,
        "  peak_rss={}  cpu={:.2}s  duration={}",
        format_bytes(s.peak_rss_bytes),
        s.cpu_seconds,
        format_secs(s.duration_seconds)
    );
}

fn render_violations_table(out: &mut String, violations: &[Violation], width: usize) {
    // Column budgets.
    let rule_w = 22usize.min(width / 4);
    let effect_w = 8usize;
    let target_w = 24usize.min(width / 3);
    let reason_w = width
        .saturating_sub(rule_w + effect_w + target_w + 10)
        .max(12);

    let _ = writeln!(
        out,
        "  {:<rw$} {:<ew$} {:<tw$} REASON",
        "RULE",
        "EFFECT",
        "TARGET",
        rw = rule_w,
        ew = effect_w,
        tw = target_w
    );

    for v in violations {
        let _ = writeln!(
            out,
            "  {:<rw$} {:<ew$} {:<tw$} {}",
            truncate(&v.rule, rule_w),
            truncate(&v.effect, effect_w),
            truncate(&v.target, target_w),
            truncate(&v.reason, reason_w),
            rw = rule_w,
            ew = effect_w,
            tw = target_w
        );
    }
}

fn plane_pairs(p: &PlaneStatus) -> [(&'static str, &PlaneState); 3] {
    [
        ("policy", &p.policy),
        ("evidence", &p.evidence),
        ("history", &p.history),
    ]
}

fn next_steps(run: &Run, ev: &Evidence) -> Vec<String> {
    let mut steps = Vec::new();
    let id = &run.manifest.id;

    steps.push(format!("actime report {id} --markdown"));
    steps.push(format!("actime report {id} --json"));

    let refused = run
        .manifest
        .target
        .note
        .as_deref()
        .is_some_and(|n| n.contains("refused before agent launch"));
    if refused {
        steps.push(
            "This run was refused before the agent started. Drop unenforceable packs, \
             use `--policy observe`, or run `actime policy check` / `actime doctor`."
                .into(),
        );
    }

    if ev.summary.blocked > 0 || ev.summary.killed > 0 {
        steps.push(format!(
            "Review blocked/killed violations in {}/violations.jsonl",
            run.dir.display()
        ));
    }

    for (name, state) in plane_pairs(&run.manifest.planes) {
        if let PlaneState::Disabled(reason) = state {
            if reason.contains("not started") {
                continue;
            }
            steps.push(format!(
                "Plane `{name}` was disabled ({reason}); run `actime doctor` for fixes"
            ));
        } else if let PlaneState::Degraded(reason) = state {
            steps.push(format!(
                "Plane `{name}` was degraded ({reason}); run `actime doctor`"
            ));
        }
    }

    if steps.len() == 2 {
        steps.push("actime doctor".into());
    }
    steps
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

fn format_secs(secs: f64) -> String {
    if secs < 0.0 || !secs.is_finite() {
        return "—".into();
    }
    if secs < 60.0 {
        return format!("{secs:.1}s");
    }
    let total = secs as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else {
        format!("{m}m{s:02}s")
    }
}

fn format_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if n >= GB {
        format!("{:.1} GiB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1} MiB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KiB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}

fn escape_md(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::evidence::Violation;
    use crate::run::{PlaneState, RunStore};
    use tempfile::TempDir;

    fn synthetic_run() -> (tempfile::TempDir, Run, Evidence) {
        let tmp = TempDir::new().unwrap();
        let store = RunStore::open(tmp.path()).unwrap();
        let mut run = store
            .create(
                &["claude".into(), "-p".into(), "hello".into()],
                &Config::builtin_profile("balanced").unwrap(),
            )
            .unwrap();
        run.manifest.exit_code = Some(0);
        run.manifest.summary.duration_seconds = 12.5;
        run.manifest.target.kind = "command".into();
        run.manifest.target.spec = Some("claude".into());
        run.manifest.planes.policy = PlaneState::Degraded("CAP_BPF not available".into());
        run.manifest.planes.evidence = PlaneState::Disabled("agentsight not found".into());
        run.manifest.planes.history = PlaneState::Active;
        run.save_manifest().unwrap();

        let violations = vec![
            Violation {
                ts: "2026-08-04T15:30:12Z".into(),
                rule: "no-secret-egress".into(),
                effect: "block".into(),
                op: "connect".into(),
                target: "evil.example.com:443".into(),
                pid: 42,
                comm: "curl".into(),
                reason: "non-allowlisted host".into(),
            },
            Violation {
                ts: "2026-08-04T15:30:13Z".into(),
                rule: "no-vcs-write".into(),
                effect: "notify".into(),
                op: "open".into(),
                target: "/repo/.git/config".into(),
                pid: 43,
                comm: "git".into(),
                reason: "write to VCS metadata".into(),
            },
        ];
        let summary = RunSummary {
            violations: 2,
            blocked: 1,
            duration_seconds: 12.5,
            processes: 5,
            llm_calls: 3,
            tokens_in: 1000,
            tokens_out: 200,
            ..Default::default()
        };
        let timeline = vec![];
        let ev = Evidence {
            violations,
            summary,
            timeline,
        };
        (tmp, run, ev)
    }

    #[test]
    fn render_text_contains_sections() {
        let (_tmp, run, ev) = synthetic_run();
        let text = render_text(&run, &ev, 80);
        assert!(text.contains("Actime run report"));
        assert!(text.contains("Run id:"));
        assert!(text.contains("claude"));
        assert!(text.contains("Planes"));
        assert!(text.contains("Target"));
        assert!(text.contains("Active"));
        assert!(text.contains("Degraded"));
        assert!(text.contains("Summary"));
        assert!(text.contains("Policy violations"));
        assert!(text.contains("no-secret-egress"));
        assert!(text.contains("Next steps"));
        // No ANSI escape sequences.
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn render_markdown_has_tables() {
        let (_tmp, run, ev) = synthetic_run();
        let md = render_markdown(&run, &ev);
        assert!(md.contains("# Actime run"));
        assert!(md.contains("| Field | Value |"));
        assert!(md.contains("## Planes"));
        assert!(md.contains("## Summary"));
        assert!(md.contains("## Policy violations"));
        assert!(md.contains("no-vcs-write"));
    }

    #[test]
    fn render_json_roundtrip_shape() {
        let (_tmp, run, ev) = synthetic_run();
        let js = render_json(&run, &ev).unwrap();
        let v: serde_json::Value = serde_json::from_str(&js).unwrap();
        assert!(v.get("manifest").is_some());
        assert!(v.get("summary").is_some());
        assert!(v.get("violations").is_some());
        assert!(v.get("timeline").is_some());
        assert_eq!(v["violations"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn truncate_respects_width() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 8).chars().count(), 8);
        assert!(truncate("hello world", 8).ends_with('…'));
    }
}
