//! The `actime` binary.
//!
//! Actime is the effect plane for AI coding agents: policy, evidence, and
//! history attached to an agent wherever it already runs. Bring your own
//! execution environment. See `docs/DESIGN.md`.

mod commands;
mod embedded;
mod planes;
mod run;
mod ui;

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// Exit code used when `--fail-on-violation` is set and a rule blocked or
/// killed something. Chosen to not collide with the common shell conventions
/// or with a plausible agent exit code.
pub const EXIT_VIOLATION: i32 = 3;

#[derive(Parser)]
#[command(
    name = "actime",
    version,
    about = "Effect plane for AI coding agents: kernel policy, system evidence, and session history. Bring your own execution environment.",
    long_about = None,
    after_help = "EXAMPLES:\n  \
      # check what your machine supports\n  \
      actime doctor\n\n  \
      # run your agent under the three planes (host process tree)\n  \
      actime run -- claude\n\n  \
      # learn first: record everything, block nothing\n  \
      actime run --policy observe -- codex\n\n  \
      # attach to something already running\n  \
      actime attach --comm claude\n  \
      actime attach --pid 4213\n  \
      actime attach --container my-agent-box\n  \
      actime attach --pod default/agent-0\n\n  \
      # read the record\n  \
      actime report            # the latest run\n  \
      actime report --json     # for a SIEM\n\n\
    Docs: https://github.com/eunomia-bpf/actime"
)]
struct Cli {
    /// Path to actime.yaml. Defaults to discovering it upward from the current
    /// directory, then ~/.config/actime/actime.yaml, then the built-in profile.
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Built-in profile to start from: observe, balanced, or strict.
    #[arg(long, global = true, value_name = "NAME")]
    profile: Option<String>,

    /// Suppress the banner and progress lines; the report is still printed.
    #[arg(short, long, global = true)]
    quiet: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Write a starter actime.yaml for this project.
    Init(InitArgs),

    /// Run an agent as a host child and attach the three planes.
    Run(RunArgs),

    /// Attach the planes to something already running.
    Attach(AttachArgs),

    /// Show runs that are currently in progress.
    Status,

    /// List recorded runs, newest first.
    Runs(RunsArgs),

    /// Print the report for a run.
    Report(ReportArgs),

    /// Inspect and validate policy packs.
    Policy {
        #[command(subcommand)]
        command: PolicyCommands,
    },

    /// Manage agent session history.
    Keep {
        #[command(subcommand)]
        command: KeepCommands,
    },

    /// Diagnose which planes this machine supports.
    Doctor(DoctorArgs),
}

#[derive(Args)]
struct InitArgs {
    /// Overwrite an existing actime.yaml.
    #[arg(short, long)]
    force: bool,
    /// Print the file instead of writing it.
    #[arg(long)]
    print: bool,
}

#[derive(Args)]
struct RunArgs {
    /// Policy mode: off, observe, or enforce.
    #[arg(long, value_name = "MODE")]
    policy: Option<String>,

    /// Turn off the evidence plane for this run.
    #[arg(long)]
    no_evidence: bool,

    /// Turn off the history plane for this run.
    #[arg(long)]
    no_history: bool,

    /// Exit with code 3 if any rule blocked or killed an action.
    #[arg(long)]
    fail_on_violation: bool,

    /// Kill the run after this long, e.g. 30m or 2h.
    #[arg(long, value_name = "DURATION")]
    timeout: Option<String>,

    /// The agent command and its arguments.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    cmd: Vec<String>,
}

#[derive(Args)]
#[command(group(
    clap::ArgGroup::new("target")
        .required(true)
        .multiple(false)
        .args(["pid", "comm", "container", "pod"])
))]
struct AttachArgs {
    /// Host pid of an already-running agent.
    #[arg(long)]
    pid: Option<i32>,
    /// Command name of an already-running agent, e.g. claude.
    #[arg(short, long)]
    comm: Option<String>,
    /// Existing Docker/Podman container name or id (must already be running).
    #[arg(long, value_name = "NAME|ID")]
    container: Option<String>,
    /// Existing Kubernetes pod on this node as namespace/name.
    #[arg(long, value_name = "NS/POD")]
    pod: Option<String>,
    /// Policy mode: off, observe, or enforce.
    #[arg(long, value_name = "MODE")]
    policy: Option<String>,
}

#[derive(Args)]
struct RunsArgs {
    /// Emit JSON instead of a table.
    #[arg(long)]
    json: bool,
    /// Show at most this many runs.
    #[arg(long, default_value_t = 20)]
    limit: usize,
}

#[derive(Args)]
struct ReportArgs {
    /// Run id, or `latest`.
    #[arg(default_value = "latest")]
    run: String,
    /// Emit JSON.
    #[arg(long, conflicts_with = "markdown")]
    json: bool,
    /// Emit Markdown.
    #[arg(long)]
    markdown: bool,
}

#[derive(Subcommand)]
enum PolicyCommands {
    /// List the shipped policy packs.
    List,
    /// Print a pack's rules.
    Show {
        /// Pack name, e.g. coding-agent-baseline.
        pack: String,
    },
    /// Compile the configured policy without loading anything. Needs no privileges.
    Check,
    /// Explain what this kernel can enforce before the fact, and what it cannot.
    Explain,
}

#[derive(Subcommand)]
enum KeepCommands {
    /// Commit the current agent session history.
    Commit {
        /// Commit message.
        #[arg(short, long)]
        message: Option<String>,
    },
    /// List committed versions.
    Log,
    /// Restore a run's session history.
    Restore {
        /// Run id, or `latest`.
        #[arg(default_value = "latest")]
        run: String,
        /// Restore into this directory instead of a scratch directory.
        #[arg(long, value_name = "DIR")]
        to: Option<PathBuf>,
    },
}

#[derive(Args)]
struct DoctorArgs {
    /// Emit JSON.
    #[arg(long)]
    json: bool,
}

fn main() {
    let cli = Cli::parse();
    let ctx = commands::Context {
        config_path: cli.config.clone(),
        profile: cli.profile.clone(),
        quiet: cli.quiet,
    };

    let result = match cli.command {
        Commands::Init(a) => commands::init(&ctx, a.force, a.print),
        Commands::Run(a) => run::run(
            &ctx,
            run::RunRequest {
                argv: a.cmd,
                policy: a.policy,
                no_evidence: a.no_evidence,
                no_history: a.no_history,
                fail_on_violation: a.fail_on_violation,
                timeout: a.timeout,
            },
        ),
        Commands::Attach(a) => run::attach(
            &ctx,
            run::AttachRequest {
                pid: a.pid,
                comm: a.comm,
                container: a.container,
                pod: a.pod,
                policy: a.policy,
            },
        ),
        Commands::Status => commands::status(&ctx),
        Commands::Runs(a) => commands::runs(&ctx, a.json, a.limit),
        Commands::Report(a) => commands::report(&ctx, &a.run, a.json, a.markdown),
        Commands::Policy { command } => match command {
            PolicyCommands::List => commands::policy_list(),
            PolicyCommands::Show { pack } => commands::policy_show(&pack),
            PolicyCommands::Check => commands::policy_check(&ctx),
            PolicyCommands::Explain => commands::policy_explain(&ctx),
        },
        Commands::Keep { command } => match command {
            KeepCommands::Commit { message } => commands::keep_commit(&ctx, message),
            KeepCommands::Log => commands::keep_log(),
            KeepCommands::Restore { run, to } => commands::keep_restore(&ctx, &run, to),
        },
        Commands::Doctor(a) => commands::doctor(&ctx, a.json),
    };

    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("{} {e:#}", ui::red("error:"));
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn run_takes_a_trailing_command_with_flags() {
        let cli =
            Cli::try_parse_from(["actime", "run", "--", "claude", "-p", "hello"]).expect("parse");
        match cli.command {
            Commands::Run(a) => assert_eq!(a.cmd, vec!["claude", "-p", "hello"]),
            _ => panic!("expected run"),
        }
    }

    #[test]
    fn run_flags_precede_the_command() {
        let cli = Cli::try_parse_from([
            "actime",
            "run",
            "--policy",
            "observe",
            "--no-history",
            "--",
            "echo",
            "hi",
        ])
        .expect("parse");
        match cli.command {
            Commands::Run(a) => {
                assert_eq!(a.policy.as_deref(), Some("observe"));
                assert!(a.no_history);
                assert_eq!(a.cmd, vec!["echo", "hi"]);
            }
            _ => panic!("expected run"),
        }
    }

    #[test]
    fn attach_accepts_each_target_kind() {
        for args in [
            vec!["actime", "attach", "--pid", "1"],
            vec!["actime", "attach", "--comm", "claude"],
            vec!["actime", "attach", "--container", "box"],
            vec!["actime", "attach", "--pod", "ns/pod"],
        ] {
            Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("parse {args:?}: {e}"));
        }
    }

    #[test]
    fn attach_requires_a_target() {
        assert!(Cli::try_parse_from(["actime", "attach"]).is_err());
    }

    #[test]
    fn attach_rejects_multiple_targets() {
        assert!(
            Cli::try_parse_from(["actime", "attach", "--pid", "1", "--comm", "claude"]).is_err()
        );
    }

    #[test]
    fn report_defaults_to_latest() {
        let cli = Cli::try_parse_from(["actime", "report"]).expect("parse");
        match cli.command {
            Commands::Report(a) => assert_eq!(a.run, "latest"),
            _ => panic!("expected report"),
        }
    }

    #[test]
    fn report_rejects_json_and_markdown_together() {
        assert!(Cli::try_parse_from(["actime", "report", "--json", "--markdown"]).is_err());
    }

    #[test]
    fn global_flags_work_after_the_subcommand() {
        let cli = Cli::try_parse_from(["actime", "doctor", "--profile", "strict"]).expect("parse");
        assert_eq!(cli.profile.as_deref(), Some("strict"));
    }

    #[test]
    fn run_requires_a_command() {
        assert!(Cli::try_parse_from(["actime", "run"]).is_err());
    }

    #[test]
    fn demo_and_sandbox_are_gone() {
        assert!(Cli::try_parse_from(["actime", "demo"]).is_err());
        assert!(Cli::try_parse_from(["actime", "sandbox", "info"]).is_err());
        assert!(Cli::try_parse_from(["actime", "shell"]).is_err());
    }
}
