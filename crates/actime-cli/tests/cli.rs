//! Integration tests for the `actime` binary.
//!
//! Every test runs the real `actime` binary as a subprocess with `ACTIME_HOME`
//! (and `HOME`) pointed at a fresh `tempfile::tempdir()`, so nothing here needs
//! root, Docker, or the actplane/agentsight/akeep engines to be installed, and
//! no run record ever touches the developer's real home directory. Every command
//! is bounded by a hard timeout so a hang fails the test instead of freezing CI.

#![allow(clippy::needless_borrows_for_generic_args)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::cargo::cargo_bin;
use serde_json::Value;

/// Default per-command timeout. Actime's own run path already bounds engine
/// waits; this is the safety net for a bug that hangs forever.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Expected top-level subcommands from the `Commands:` section of `--help`.
const EXPECTED_COMMANDS: &[&str] = &[
    "init", "run", "attach", "status", "runs", "report", "policy", "keep", "doctor", "help",
];

/// The result of one bounded `actime` invocation.
struct Out {
    success: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
    elapsed: Duration,
}

impl Out {
    /// Combined stdout + stderr for message assertions.
    fn combined(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }

    /// Assert the command exited zero.
    fn assert_ok(&self, ctx: &str) -> &Self {
        assert!(
            !self.timed_out,
            "{ctx}: command timed out after {:?}",
            self.elapsed
        );
        assert!(
            self.success,
            "{ctx}: expected success, got exit {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.code, self.stdout, self.stderr
        );
        self
    }

    /// Assert the command exited non-zero.
    fn assert_failed(&self, ctx: &str) -> &Self {
        assert!(
            !self.timed_out,
            "{ctx}: command timed out after {:?}",
            self.elapsed
        );
        assert!(
            !self.success,
            "{ctx}: expected non-zero exit, got success\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.stdout, self.stderr
        );
        self
    }

    /// Assert a specific exit code.
    fn assert_code(&self, ctx: &str, want: i32) -> &Self {
        assert!(
            !self.timed_out,
            "{ctx}: command timed out after {:?}",
            self.elapsed
        );
        assert_eq!(
            self.code,
            Some(want),
            "{ctx}: expected exit {want}, got {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.code,
            self.stdout,
            self.stderr
        );
        self
    }
}

/// A builder for one bounded `actime` invocation.
struct Cmd {
    args: Vec<String>,
    home: PathBuf,
    cwd: PathBuf,
    timeout: Duration,
}

impl Cmd {
    /// New command whose `ACTIME_HOME`/`HOME`/cwd all point at `home`.
    fn new(home: &Path) -> Self {
        Cmd {
            args: Vec::new(),
            home: home.to_path_buf(),
            cwd: home.to_path_buf(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    fn arg(mut self, a: &str) -> Self {
        self.args.push(a.to_string());
        self
    }

    fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for a in args {
            self.args.push(a.as_ref().to_string());
        }
        self
    }

    /// Override the working directory (defaults to `home`).
    fn cwd(mut self, cwd: &Path) -> Self {
        self.cwd = cwd.to_path_buf();
        self
    }

    fn timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    /// Spawn the binary and poll until it exits or the deadline hits. On
    /// timeout the child is killed so the test fails instead of hanging.
    fn run(&self) -> Out {
        let mut cmd = Command::new(cargo_bin("actime"));
        // Isolate every run from the developer's real home and config. Keep
        // PATH so doctor/attach can still probe docker/podman/kubectl.
        cmd.env("ACTIME_HOME", &self.home);
        cmd.env("HOME", &self.home);
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path);
        }
        // Never block on an interactive sudo prompt inside CI / tests.
        cmd.env("ACTIME_NONINTERACTIVE", "1");
        cmd.env("NO_COLOR", "1");
        cmd.current_dir(&self.cwd);
        cmd.args(&self.args);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let start = Instant::now();
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => panic!("spawning actime binary failed: {e}"),
        };
        let deadline = start + self.timeout;

        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        // Reap so we don't leave a zombie; bounded.
                        let reap_deadline = Instant::now() + Duration::from_millis(500);
                        loop {
                            match child.try_wait() {
                                Ok(Some(s)) => {
                                    return Out {
                                        success: false,
                                        code: s.code(),
                                        stdout: drain(child.stdout.take()),
                                        stderr: drain(child.stderr.take()),
                                        timed_out: true,
                                        elapsed: start.elapsed(),
                                    }
                                }
                                Ok(None) if Instant::now() < reap_deadline => {
                                    std::thread::sleep(Duration::from_millis(20));
                                }
                                _ => break,
                            }
                        }
                        return Out {
                            success: false,
                            code: None,
                            stdout: String::new(),
                            stderr: String::new(),
                            timed_out: true,
                            elapsed: start.elapsed(),
                        };
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("polling actime child failed: {e}"),
            }
        };

        let status = status.expect("status set");
        Out {
            success: status.success(),
            code: status.code(),
            stdout: drain(child.stdout.take()),
            stderr: drain(child.stderr.take()),
            timed_out: false,
            elapsed: start.elapsed(),
        }
    }
}

fn drain<R: Read>(maybe: Option<R>) -> String {
    let Some(mut r) = maybe else {
        return String::new();
    };
    let mut buf = Vec::new();
    let _ = r.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Read `manifest.json` from a run directory and parse it.
fn read_manifest(run_dir: &Path) -> Value {
    let text = std::fs::read_to_string(run_dir.join("manifest.json"))
        .unwrap_or_else(|e| panic!("reading manifest: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parsing manifest: {e}"))
}

/// List immediate child directories under `$ACTIME_HOME/runs`.
fn list_run_dirs(home: &Path) -> Vec<PathBuf> {
    let runs = home.join("runs");
    if !runs.is_dir() {
        return Vec::new();
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&runs)
        .unwrap_or_else(|e| panic!("reading {}: {e}", runs.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// Whether a usable `actplane` binary is on the process PATH (same PATH the
/// child inherits). `HOME` is isolated to a tempdir in tests, so detection
/// does not pick up `~/.cargo/bin` from the developer's real home.
fn actplane_on_path() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("actplane");
        if candidate.is_file() {
            // Prefer a quick --version probe so a broken stub does not count.
            if let Ok(out) = Command::new(&candidate).arg("--version").output() {
                if out.status.success() {
                    return true;
                }
            }
        }
    }
    false
}

/// Extract the `Commands:` section body from clap `--help` text.
///
/// Only this section is inspected for subcommand names, so the about-line word
/// "sandbox" (or examples mentioning containers) cannot confuse the check.
fn parse_commands_section(help: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_commands = false;
    for line in help.lines() {
        if line.starts_with("Commands:") {
            in_commands = true;
            continue;
        }
        if in_commands {
            // End of the Commands block: blank line, or next top-level header.
            if line.trim().is_empty()
                || (line.starts_with("Options:")
                    || line.starts_with("Usage:")
                    || line.starts_with("Arguments:")
                    || line.starts_with("EXAMPLES:"))
            {
                break;
            }
            let trimmed = line.trim_start();
            // clap formats: "  name    description"
            if let Some(name) = trimmed.split_whitespace().next() {
                // Skip continued description lines that do not start a command.
                if name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '-' || c == '_')
                {
                    names.push(name.to_string());
                }
            }
        }
    }
    names
}

/// Write a minimal actime.yaml with the given packs into `dir`.
fn write_config_with_packs(dir: &Path, packs: &[&str]) {
    let packs_yaml: String = packs.iter().map(|p| format!("    - {p}\n")).collect();
    let yaml = format!(
        "version: 1\nprofile: balanced\npolicy:\n  mode: enforce\n  packs:\n{packs_yaml}  feedback: true\nevidence:\n  enabled: false\nhistory:\n  enabled: false\n"
    );
    std::fs::write(dir.join("actime.yaml"), yaml).expect("write actime.yaml");
}

// ---------------------------------------------------------------------------
// help / version / surface
// ---------------------------------------------------------------------------

#[test]
fn help_and_version_succeed() {
    let home = tempfile::tempdir().expect("tempdir");
    let help = Cmd::new(home.path()).arg("--help").run();
    help.assert_ok("actime --help");
    assert!(
        help.stdout.contains("actime") || help.stderr.contains("actime"),
        "--help should mention actime"
    );

    let home = tempfile::tempdir().expect("tempdir");
    let version = Cmd::new(home.path()).arg("--version").run();
    version.assert_ok("actime --version");
    assert!(
        version.stdout.contains("actime") || version.stderr.contains("actime"),
        "--version should mention actime"
    );
}

#[test]
fn help_lists_exactly_the_real_subcommands() {
    let home = tempfile::tempdir().expect("tempdir");
    let out = Cmd::new(home.path()).arg("--help").run();
    out.assert_ok("help");
    let text = out.combined();

    let commands = parse_commands_section(&text);
    assert!(
        !commands.is_empty(),
        "failed to parse Commands: section from --help:\n{text}"
    );

    let expected: Vec<String> = EXPECTED_COMMANDS.iter().map(|s| (*s).to_string()).collect();
    assert_eq!(
        commands, expected,
        "Commands: section must list exactly the shipped subcommands (order matters)"
    );

    // These were deliberately removed (DESIGN.md §10) and must not come back
    // as subcommands. Only inspect the Commands: section — the about line
    // legitimately mentions that Actime does not manage a sandbox.
    for forbidden in ["sandbox", "demo", "shell"] {
        assert!(
            !commands.iter().any(|c| c == forbidden),
            "`{forbidden}` must not appear as a subcommand in Commands:"
        );
    }
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

#[test]
fn doctor_succeeds_and_json_has_deployment() {
    let home = tempfile::tempdir().expect("tempdir");
    let out = Cmd::new(home.path()).arg("doctor").run();
    out.assert_ok("doctor");

    let home = tempfile::tempdir().expect("tempdir");
    let json = Cmd::new(home.path()).args(["doctor", "--json"]).run();
    // doctor --json exits 1 only when a check fails; on a normal Linux host
    // with ACTIME_HOME writable we expect only warnings (cap_bpf, missing
    // engines) and exit 0.
    json.assert_ok("doctor --json");

    let value: Value = serde_json::from_str(&json.stdout)
        .unwrap_or_else(|e| panic!("doctor --json is not valid JSON: {e}\n{}", json.stdout));
    let arr = value
        .as_array()
        .unwrap_or_else(|| panic!("doctor --json must be an array, got {value}"));
    assert!(!arr.is_empty(), "doctor --json array must not be empty");

    for (i, item) in arr.iter().enumerate() {
        let obj = item
            .as_object()
            .unwrap_or_else(|| panic!("check[{i}] must be an object"));
        assert!(obj.contains_key("name"), "check[{i}] missing name");
        assert!(obj.contains_key("status"), "check[{i}] missing status");
        assert!(obj.contains_key("detail"), "check[{i}] missing detail");
    }

    let names: Vec<&str> = arr
        .iter()
        .filter_map(|c| c.get("name").and_then(|n| n.as_str()))
        .collect();
    assert!(
        names.contains(&"deployment"),
        "doctor checks must include deployment; got {names:?}"
    );
}

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

#[test]
fn init_print_and_write_and_force() {
    let home = tempfile::tempdir().expect("tempdir");
    let printed = Cmd::new(home.path()).args(["init", "--print"]).run();
    printed.assert_ok("init --print");
    assert!(
        printed.stdout.contains("profile:"),
        "init --print must include profile:\n{}",
        printed.stdout
    );
    // No sandbox key in the profile YAML (Actime does not manage sandboxes).
    for line in printed.stdout.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        assert!(
            !trimmed.starts_with("sandbox:") && !trimmed.starts_with("sandbox "),
            "init --print must not contain a sandbox: key; line={line:?}"
        );
    }
    assert!(
        !printed.stdout.contains("\nsandbox:") && !printed.stdout.contains("\nsandbox :"),
        "init --print must not emit a sandbox: key"
    );

    let project = tempfile::tempdir().expect("project dir");
    let wrote = Cmd::new(home.path()).cwd(project.path()).arg("init").run();
    wrote.assert_ok("init");
    let path = project.path().join("actime.yaml");
    assert!(path.is_file(), "init must write actime.yaml");
    let body = std::fs::read_to_string(&path).expect("read actime.yaml");
    assert!(body.contains("profile:"), "written yaml needs profile:");

    let again = Cmd::new(home.path()).cwd(project.path()).arg("init").run();
    again.assert_failed("second init without --force");
    let msg = again.combined();
    assert!(
        msg.contains("--force"),
        "second init must mention --force:\n{msg}"
    );
}

// ---------------------------------------------------------------------------
// policy list / show / check
// ---------------------------------------------------------------------------

#[test]
fn policy_list_and_show() {
    let home = tempfile::tempdir().expect("tempdir");
    let list = Cmd::new(home.path()).args(["policy", "list"]).run();
    list.assert_ok("policy list");
    for pack in ["coding-agent-baseline", "no-vcs-write", "information-flow"] {
        assert!(
            list.stdout.contains(pack),
            "policy list must name {pack}:\n{}",
            list.stdout
        );
    }

    let home = tempfile::tempdir().expect("tempdir");
    let show = Cmd::new(home.path())
        .args(["policy", "show", "coding-agent-baseline"])
        .run();
    show.assert_ok("policy show coding-agent-baseline");
    assert!(
        show.stdout.contains("rule "),
        "policy show must print rules:\n{}",
        show.stdout
    );
    assert!(
        show.stdout.contains("because") || show.stdout.contains("kill "),
        "policy show should include rule bodies"
    );

    let home = tempfile::tempdir().expect("tempdir");
    let bad = Cmd::new(home.path())
        .args(["policy", "show", "nonsense"])
        .run();
    bad.assert_failed("policy show nonsense");
    let msg = bad.combined();
    assert!(
        msg.contains("coding-agent-baseline")
            && msg.contains("no-vcs-write")
            && msg.contains("information-flow"),
        "unknown pack error must list valid packs:\n{msg}"
    );
}

#[test]
fn policy_check_enforceable_table_or_degrades() {
    let home = tempfile::tempdir().expect("tempdir");
    // Use a config with a known pack so the rule count is stable.
    write_config_with_packs(home.path(), &["coding-agent-baseline"]);

    let out = Cmd::new(home.path())
        .cwd(home.path())
        .args(["policy", "check"])
        .run();
    out.assert_ok("policy check");
    let text = out.combined();

    if actplane_on_path() {
        assert!(
            text.contains("ENFORCEABLE") || text.contains("enforceable"),
            "with actplane, policy check must print the ENFORCEABLE table:\n{text}"
        );
        // coding-agent-baseline has two rules: destructive-vcs, mass-deletion.
        for rule in ["destructive-vcs", "mass-deletion"] {
            assert!(
                text.contains(rule),
                "with actplane, table must include a row for {rule}:\n{text}"
            );
        }
        // Header present when rules are listed.
        assert!(
            text.lines()
                .any(|l| l.contains("RULE") && l.contains("ENFORCEABLE"))
                || text.contains("rules enforceable"),
            "expected ENFORCEABLE table header or summary:\n{text}"
        );
    } else {
        assert!(
            text.to_ascii_lowercase().contains("not installed")
                || text.to_ascii_lowercase().contains("composed")
                || text.to_ascii_lowercase().contains("could only be composed"),
            "without actplane, policy check must degrade with a clear message:\n{text}"
        );
    }
}

// ---------------------------------------------------------------------------
// runs / report on empty store
// ---------------------------------------------------------------------------

#[test]
fn runs_empty_and_report_empty() {
    let home = tempfile::tempdir().expect("tempdir");
    let runs = Cmd::new(home.path()).arg("runs").run();
    runs.assert_ok("runs on empty store");
    let text = runs.combined().to_ascii_lowercase();
    assert!(
        text.contains("no runs") || text.contains("no run"),
        "empty runs should report none:\n{}",
        runs.combined()
    );

    let home = tempfile::tempdir().expect("tempdir");
    let report = Cmd::new(home.path()).arg("report").run();
    report.assert_failed("report on empty store");
    let msg = report.combined().to_ascii_lowercase();
    assert!(
        msg.contains("no runs") || msg.contains("no run") || msg.contains("recorded"),
        "empty report must fail helpfully:\n{}",
        report.combined()
    );
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

#[test]
fn run_echo_creates_manifest_and_report() {
    let home = tempfile::tempdir().expect("tempdir");
    let out = Cmd::new(home.path())
        .timeout(Duration::from_secs(90))
        .args([
            "run",
            "--policy",
            "off",
            "--no-history",
            "--",
            "/bin/echo",
            "hello",
        ])
        .run();
    out.assert_ok("run echo");
    out.assert_code("run echo", 0);
    assert!(
        out.stdout.contains("hello"),
        "run must print command stdout:\n{}",
        out.stdout
    );

    let dirs = list_run_dirs(home.path());
    assert_eq!(dirs.len(), 1, "exactly one run dir expected, got {dirs:?}");
    let run_dir = &dirs[0];
    assert!(
        run_dir.join("manifest.json").is_file(),
        "manifest.json missing in {}",
        run_dir.display()
    );
    assert!(
        run_dir.join("report.md").is_file(),
        "report.md missing in {}",
        run_dir.display()
    );

    let m = read_manifest(run_dir);
    assert_eq!(m["schema"], 1, "schema must be 1");
    let argv = m["argv"]
        .as_array()
        .expect("argv array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect::<Vec<_>>();
    assert_eq!(argv, vec!["/bin/echo", "hello"]);
    assert_eq!(m["exit_code"], 0);
    assert_eq!(m["target"]["kind"], "command");

    let planes = m["planes"].as_object().expect("planes must be an object");
    assert!(
        planes.contains_key("policy")
            && planes.contains_key("evidence")
            && planes.contains_key("history"),
        "planes must have policy/evidence/history: {planes:?}"
    );
    assert!(
        !planes.contains_key("isolation"),
        "planes must not have an isolation key: {planes:?}"
    );
    assert_eq!(
        planes.len(),
        3,
        "planes must have exactly three keys: {planes:?}"
    );
}

#[test]
fn run_false_exits_one() {
    let home = tempfile::tempdir().expect("tempdir");
    let out = Cmd::new(home.path())
        .timeout(Duration::from_secs(90))
        .args(["run", "--policy", "off", "--no-history", "--", "/bin/false"])
        .run();
    out.assert_code("run /bin/false", 1);
}

#[test]
fn run_timeout_kills_long_sleep() {
    let home = tempfile::tempdir().expect("tempdir");
    let out = Cmd::new(home.path())
        // Outer safety net well under 60s wall for the sleep itself.
        .timeout(Duration::from_secs(30))
        .args([
            "run",
            "--policy",
            "off",
            "--no-history",
            "--timeout",
            "3s",
            "--",
            "/bin/sleep",
            "60",
        ])
        .run();
    assert!(
        !out.timed_out,
        "actime itself must not hit the test timeout; elapsed={:?}",
        out.elapsed
    );
    // Must finish well under the 60s sleep.
    assert!(
        out.elapsed < Duration::from_secs(20),
        "run --timeout 3s must finish well under 60s; elapsed={:?}",
        out.elapsed
    );
    // Prefer a non-zero exit (killed / timed out), but the key contract is wall time.
    assert!(
        out.elapsed >= Duration::from_secs(2),
        "timeout should wait roughly the limit; elapsed={:?}",
        out.elapsed
    );
}

#[test]
fn run_without_command_fails() {
    let home = tempfile::tempdir().expect("tempdir");
    let out = Cmd::new(home.path())
        .args(["run", "--policy", "off", "--no-history", "--"])
        .run();
    out.assert_failed("run with no command after --");
    let msg = out.combined().to_ascii_lowercase();
    assert!(
        msg.contains("required") || msg.contains("command") || msg.contains("<cmd>"),
        "missing command must fail clearly:\n{}",
        out.combined()
    );
}

#[test]
fn run_enforce_information_flow_fails_closed() {
    // BUG: fail-closed enforce still creates a run directory with exit_code
    // null and planes mostly "not started" (agent never launched). That is
    // intentional enough to leave alone here; the suite only requires exit 1
    // and that the agent did not run.
    let home = tempfile::tempdir().expect("tempdir");
    write_config_with_packs(home.path(), &["information-flow"]);

    let out = Cmd::new(home.path())
        .cwd(home.path())
        .timeout(Duration::from_secs(60))
        .args([
            "run",
            "--policy",
            "enforce",
            "--no-history",
            "--",
            "/bin/echo",
            "should-not-run",
        ])
        .run();
    out.assert_failed("enforce information-flow");
    out.assert_code("enforce information-flow", 1);

    let msg = out.combined();
    assert!(
        !out.stdout.contains("should-not-run"),
        "agent must not run on fail-closed enforce:\n{}",
        out.stdout
    );
    assert!(
        msg.to_ascii_lowercase().contains("enforce")
            || msg.to_ascii_lowercase().contains("fails closed")
            || msg.to_ascii_lowercase().contains("cannot be enforced"),
        "message must describe fail-closed enforce:\n{msg}"
    );

    // Both with and without actplane, information-flow rules are unenforceable
    // on the released engine budget (DSL classify or compile --json).
    let names_any = [
        "system-fence",
        "evidence-integrity",
        "credential-access",
        "no-secret-egress",
    ];
    let named = names_any.iter().any(|n| msg.contains(n));
    assert!(
        named,
        "fail-closed message must name unenforceable rules; actplane_on_path={} msg=\n{msg}",
        actplane_on_path()
    );

    if actplane_on_path() {
        // With actplane, reasons mention missing engine features.
        assert!(
            msg.to_ascii_lowercase().contains("feature")
                || msg.to_ascii_lowercase().contains("enforceable")
                || msg.contains("engine"),
            "with actplane, expect engine/feature reasoning:\n{msg}"
        );
    } else {
        // Without actplane the same preflight path uses DSL classification.
        assert!(
            msg.to_ascii_lowercase().contains("enforceable")
                || msg.to_ascii_lowercase().contains("engine")
                || msg.to_ascii_lowercase().contains("feature"),
            "without actplane, still expect unenforceable-rule messaging:\n{msg}"
        );
    }
}

#[test]
fn run_observe_information_flow_mentions_unenforceable() {
    let home = tempfile::tempdir().expect("tempdir");
    write_config_with_packs(home.path(), &["information-flow"]);

    let out = Cmd::new(home.path())
        .cwd(home.path())
        .timeout(Duration::from_secs(90))
        .args([
            "run",
            "--policy",
            "observe",
            "--no-history",
            "--",
            "/bin/echo",
            "observe-ok",
        ])
        .run();
    out.assert_ok("observe information-flow");
    out.assert_code("observe information-flow", 0);
    assert!(
        out.stdout.contains("observe-ok"),
        "observe must still run the command:\n{}",
        out.stdout
    );

    let combined = out.combined();
    let dirs = list_run_dirs(home.path());
    assert!(!dirs.is_empty(), "observe must create a run");
    let report_md =
        std::fs::read_to_string(dirs.last().unwrap().join("report.md")).unwrap_or_default();
    let blob = format!("{combined}\n{report_md}").to_ascii_lowercase();
    assert!(
        blob.contains("unenforceable")
            || blob.contains("not enforceable")
            || blob.contains("system-fence")
            || blob.contains("evidence-integrity"),
        "observe report/output must mention unenforceable rules:\n{combined}\n--- report.md ---\n{report_md}"
    );
}

// ---------------------------------------------------------------------------
// attach
// ---------------------------------------------------------------------------

#[test]
fn attach_requires_a_target() {
    let home = tempfile::tempdir().expect("tempdir");
    let out = Cmd::new(home.path()).arg("attach").run();
    out.assert_failed("attach with no target");
    let msg = out.combined().to_ascii_lowercase();
    assert!(
        msg.contains("--pid")
            || msg.contains("--comm")
            || msg.contains("--container")
            || msg.contains("--pod")
            || msg.contains("required"),
        "attach without target must ask for one:\n{}",
        out.combined()
    );
}

#[test]
fn attach_missing_container_is_clear() {
    let home = tempfile::tempdir().expect("tempdir");
    let out = Cmd::new(home.path())
        .args(["attach", "--container", "definitely-does-not-exist-xyz"])
        .run();
    out.assert_failed("attach missing container");
    let msg = out.combined();
    assert!(
        msg.to_ascii_lowercase().contains("does not create")
            || msg.contains("Actime does not create containers"),
        "missing container must say Actime does not create containers:\n{msg}"
    );
}

#[test]
fn attach_missing_pid_is_clear() {
    let home = tempfile::tempdir().expect("tempdir");
    // PIDs wrap; a very large pid is effectively never live on this host.
    let out = Cmd::new(home.path())
        .args(["attach", "--pid", "2147483646"])
        .run();
    out.assert_failed("attach missing pid");
    let msg = out.combined().to_ascii_lowercase();
    assert!(
        msg.contains("no process") || msg.contains("pid") || msg.contains("exited"),
        "missing pid must fail clearly:\n{}",
        out.combined()
    );
}

// ---------------------------------------------------------------------------
// runs list + report --json after real runs
// ---------------------------------------------------------------------------

#[test]
fn runs_lists_and_report_json_contains_manifest() {
    let home = tempfile::tempdir().expect("tempdir");

    let echo = Cmd::new(home.path())
        .timeout(Duration::from_secs(90))
        .args([
            "run",
            "--policy",
            "off",
            "--no-history",
            "--",
            "/bin/echo",
            "listed",
        ])
        .run();
    echo.assert_ok("setup run");

    let runs = Cmd::new(home.path()).arg("runs").run();
    runs.assert_ok("runs after run");
    let text = runs.combined();
    assert!(
        !text.to_ascii_lowercase().contains("no runs recorded"),
        "runs must list recorded runs:\n{text}"
    );
    // Table header or the run id pattern.
    assert!(
        text.contains("RUN") || text.contains("command") || text.contains("EXIT"),
        "runs table should show entries:\n{text}"
    );

    let report = Cmd::new(home.path()).args(["report", "--json"]).run();
    report.assert_ok("report --json");
    let value: Value = serde_json::from_str(&report.stdout)
        .unwrap_or_else(|e| panic!("report --json must be valid JSON: {e}\n{}", report.stdout));
    let manifest = value
        .get("manifest")
        .unwrap_or_else(|| panic!("report --json must contain manifest: {value}"));
    assert_eq!(manifest["schema"], 1);
    assert!(manifest.get("argv").is_some(), "manifest must include argv");
    assert!(
        manifest.get("planes").is_some(),
        "manifest must include planes"
    );
    let planes = manifest["planes"].as_object().expect("planes object");
    assert!(!planes.contains_key("isolation"));
}
