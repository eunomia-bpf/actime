//! Sandbox backends for the Actime agent runtime.
//!
//! This crate implements the isolation plane: Docker, Podman, Bubblewrap, and
//! a host (no-isolation) fallback. Policy and evidence attach from the host to
//! the sandbox root process via [`Sandbox::host_pid`] and
//! [`Sandbox::evidence_target`].
//!
//! # Public API
//!
//! The contract is defined in `docs/DESIGN.md` §6:
//!
//! - [`SandboxSpec`], [`Mount`], [`NetworkMode`], [`SandboxReport`]
//! - [`Sandbox`] trait
//! - [`Backend`], [`BackendProbe`], [`create`]
//!
//! # Example
//!
//! ```no_run
//! use actime_sandbox::{create, Backend, SandboxSpec};
//!
//! let spec = SandboxSpec::new("actime-demo", "ghcr.io/eunomia-bpf/actime-sandbox:latest");
//! let mut sb = create(Backend::Host, spec).unwrap();
//! sb.spawn(&["/bin/echo".into(), "hello".into()]).unwrap();
//! let code = sb.wait().unwrap();
//! sb.cleanup().unwrap();
//! assert_eq!(code, 0);
//! ```
//!
//! # Degradation
//!
//! [`Backend::detect_available`] probes Docker → Podman → Bwrap → Host and
//! always includes Host last. Every backend runs without root privileges.

#![deny(missing_docs)]
#![warn(rust_2018_idioms)]

mod backend;
mod bwrap;
mod docker;
mod host;
mod spec;

pub use backend::{create, Backend, BackendProbe, Sandbox};
pub use bwrap::BwrapSandbox;
pub use docker::DockerSandbox;
pub use host::HostSandbox;
pub use spec::{Mount, NetworkMode, SandboxReport, SandboxSpec};

/// Crate version string.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
