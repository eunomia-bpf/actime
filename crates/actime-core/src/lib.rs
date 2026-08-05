//! Core configuration, run store, evidence aggregation, and reporting for Actime.
//!
//! This crate is the integration contract for the Actime agent runtime
//! (`docs/DESIGN.md` §5). It does **not** depend on `actime-sandbox`; the
//! sandbox backend is represented as a plain [`String`] in [`SandboxConfig`].
//!
//! # Modules
//!
//! - [`config`] — `actime.yaml`, profiles, CLI overrides, duration parsing
//! - [`components`] — detect actplane / agentsight / akeep
//! - [`run`] — run ids, manifests, run store
//! - [`evidence`] — violations.jsonl + evidence.db aggregation
//! - [`report`] — text / markdown / JSON reports
//! - [`doctor`] — environment health checks
//!
//! # Example
//!
//! ```no_run
//! use actime_core::{Config, RunStore, Evidence, render_text};
//!
//! let cfg = Config::load(None, std::path::Path::new(".")).unwrap();
//! let store = RunStore::open_default().unwrap();
//! let run = store.create(&["claude".into()], &cfg).unwrap();
//! let ev = Evidence::collect(&run).unwrap();
//! print!("{}", render_text(&run, &ev, 80));
//! ```

#![deny(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod components;
pub mod config;
pub mod doctor;
pub mod evidence;
pub mod report;
pub mod run;

// Re-export the public API surface from DESIGN.md §5.

pub use components::{compare_semver, extract_semver, Component, Components};
pub use config::{
    format_duration, parse_duration, CliOverrides, Config, EvidenceConfig, HistoryConfig,
    LimitsConfig, NetworkMode, PolicyConfig, PolicyMode, SandboxConfig,
};
pub use doctor::{run_checks, Check, CheckStatus};
pub use evidence::{Evidence, TimelineEntry, Violation, TIMELINE_CAP};
pub use report::{render_json, render_markdown, render_text};
pub use run::{
    default_actime_home, detect_agent, Manifest, PlaneState, PlaneStatus, Run, RunId, RunStore,
    RunSummary, SandboxReport,
};

/// Crate version string.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
