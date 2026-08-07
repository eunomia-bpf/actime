//! Core configuration, run store, observations aggregation, and reporting for Actime.
//!
//! This crate is the integration contract for the Actime effect plane
//! (`docs/DESIGN.md` §5). Actime does not manage sandboxes; it attaches policy,
//! observability, and backup to an agent wherever it already runs.
//!
//! # Modules
//!
//! - [`config`] — `actime.yaml`, profiles, CLI overrides, duration parsing
//! - [`components`] — detect actplane / agentsight / akeep
//! - [`run`] — run ids, manifests, run store
//! - [`observations`] — violations.jsonl + observability.db aggregation
//! - [`report`] — text / markdown / JSON reports
//! - [`doctor`] — environment health checks
//!
//! # Example
//!
//! ```no_run
//! use actime_core::{Config, RunStore, Observations, render_text};
//!
//! let cfg = Config::load(None, std::path::Path::new(".")).unwrap();
//! let store = RunStore::open_default().unwrap();
//! let run = store.create(&["claude".into()], &cfg).unwrap();
//! let ev = Observations::collect(&run).unwrap();
//! print!("{}", render_text(&run, &ev, 80));
//! ```

#![deny(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod components;
pub mod config;
pub mod doctor;
pub mod enforceability;
pub mod observations;
pub mod report;
pub mod run;

// Re-export the public API surface from DESIGN.md §5.

pub use components::{compare_semver, extract_semver, Component, Components};
pub use config::{
    format_duration, parse_duration, BackupConfig, CliOverrides, Config, LimitsConfig,
    ObservabilityConfig, PolicyConfig, PolicyMode,
};
pub use doctor::{run_checks, Check, CheckStatus};
pub use enforceability::{
    assess_compile_json, assess_dsl, engine_supported_features, unenforceable_only,
    RuleEnforceability, ENGINE_SUPPORTED_0_1_8,
};
pub use observations::{
    has_observational_signal, Observations, TimelineEntry, Violation, TIMELINE_CAP,
};
pub use report::{render_json, render_markdown, render_text};
pub use run::{
    default_actime_home, detect_agent, Manifest, PlaneState, PlaneStatus, Run, RunId, RunStore,
    RunSummary, TargetReport,
};

/// Crate version string.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
