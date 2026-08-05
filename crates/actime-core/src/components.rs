//! Discovery of ActPlane, AgentSight, and Akeep binaries.
//!
//! Search order for each component:
//!
//! 1. `$PATH`
//! 2. `~/.local/share/actime/bin`
//! 3. `~/.cargo/bin`
//!
//! Version strings come from `<bin> --version`, taking the first
//! semver-looking token. Minimum versions are fixed by the design contract.

use std::path::{Path, PathBuf};
use std::process::Command;

/// One external engine binary (actplane / agentsight / akeep).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    /// Short name (`"actplane"`, `"agentsight"`, `"akeep"`).
    pub name: &'static str,
    /// Resolved absolute path, if found.
    pub path: Option<PathBuf>,
    /// Parsed version string, if the binary reported one.
    pub version: Option<String>,
    /// Minimum supported version for this component.
    pub min_version: &'static str,
}

impl Component {
    /// Whether the binary is present and its version is ≥ [`Self::min_version`].
    pub fn is_ok(&self) -> bool {
        match (&self.path, &self.version) {
            (Some(_), Some(v)) => compare_semver(v, self.min_version) >= 0,
            (Some(_), None) => true, // present but unparseable version — treat as usable
            _ => false,
        }
    }

    /// Whether the binary is present at all.
    pub fn is_present(&self) -> bool {
        self.path.is_some()
    }
}

/// The three Actime engine components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Components {
    /// ActPlane policy engine.
    pub actplane: Component,
    /// AgentSight evidence engine.
    pub agentsight: Component,
    /// Akeep history engine.
    pub akeep: Component,
}

/// Minimum versions required by Actime (DESIGN.md / components contract).
pub const ACTPLANE_MIN: &str = "0.1.8";
/// Minimum AgentSight version.
pub const AGENTSIGHT_MIN: &str = "0.2.60";
/// Minimum Akeep version.
pub const AKEEP_MIN: &str = "0.2.0";

impl Components {
    /// Detect actplane, agentsight, and akeep on the system.
    pub fn detect() -> Components {
        Components {
            actplane: detect_one("actplane", ACTPLANE_MIN),
            agentsight: detect_one("agentsight", AGENTSIGHT_MIN),
            akeep: detect_one("akeep", AKEEP_MIN),
        }
    }

    /// Return an install hint such as `"cargo install actplane"`.
    pub fn install_hint(name: &str) -> String {
        format!("cargo install {name}")
    }

    /// Iterate over the three components.
    pub fn iter(&self) -> impl Iterator<Item = &Component> {
        [&self.actplane, &self.agentsight, &self.akeep].into_iter()
    }
}

fn detect_one(name: &'static str, min_version: &'static str) -> Component {
    let path = find_binary(name);
    let version = path.as_ref().and_then(|p| probe_version(p));
    Component {
        name,
        path,
        version,
        min_version,
    }
}

/// Search PATH, then `~/.local/share/actime/bin`, then `~/.cargo/bin`.
fn find_binary(name: &str) -> Option<PathBuf> {
    // 1. PATH
    if let Some(p) = which_in_path(name) {
        return Some(p);
    }

    // 2. ~/.local/share/actime/bin  (or $ACTIME_HOME/bin)
    if let Some(home) = actime_home() {
        let candidate = home.join("bin").join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }

    // 3. ~/.cargo/bin
    if let Some(home) = std::env::var_os("HOME") {
        let candidate = PathBuf::from(home).join(".cargo").join("bin").join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }

    None
}

fn actime_home() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("ACTIME_HOME") {
        return Some(PathBuf::from(h));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share").join("actime"))
}

fn which_in_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    // Check execute bit via libc access(X_OK).
    use std::ffi::CString;
    let Ok(c) = CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    // SAFETY: c is a valid NUL-terminated path string.
    unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}

/// Run `<bin> --version` and extract the first semver-looking token.
fn probe_version(path: &Path) -> Option<String> {
    let output = Command::new(path).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let text = if text.trim().is_empty() {
        String::from_utf8_lossy(&output.stderr).into_owned()
    } else {
        text.into_owned()
    };
    extract_semver(&text)
}

/// Find the first `x.y.z` (optionally with pre-release) token in `text`.
pub fn extract_semver(text: &str) -> Option<String> {
    for token in text.split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')') {
        let token = token.trim().trim_start_matches('v');
        if looks_like_semver(token) {
            // Strip trailing punctuation.
            let cleaned: String = token
                .chars()
                .take_while(|c| {
                    c.is_ascii_digit() || *c == '.' || *c == '-' || c.is_ascii_alphanumeric()
                })
                .collect();
            // Keep only major.minor.patch for comparison base.
            if let Some((maj, min, pat)) = parse_semver_parts(&cleaned) {
                return Some(format!("{maj}.{min}.{pat}"));
            }
        }
    }
    None
}

fn looks_like_semver(s: &str) -> bool {
    let mut parts = s.split('.');
    let Some(a) = parts.next() else {
        return false;
    };
    let Some(b) = parts.next() else {
        return false;
    };
    // patch may include pre-release suffix
    let Some(c) = parts.next() else {
        return false;
    };
    a.chars().all(|ch| ch.is_ascii_digit())
        && b.chars().all(|ch| ch.is_ascii_digit())
        && c.chars().next().is_some_and(|ch| ch.is_ascii_digit())
}

/// Parse major.minor.patch from a version string. Extra suffix on patch is ignored.
fn parse_semver_parts(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches('v');
    let mut parts = s.split('.');
    let major: u64 = parts.next()?.parse().ok()?;
    let minor: u64 = parts.next()?.parse().ok()?;
    let patch_raw = parts.next().unwrap_or("0");
    // Take leading digits of patch.
    let patch_digits: String = patch_raw
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let patch: u64 = if patch_digits.is_empty() {
        0
    } else {
        patch_digits.parse().ok()?
    };
    Some((major, minor, patch))
}

/// Compare two semver strings as `x.y.z`.
///
/// Returns negative if `a < b`, zero if equal, positive if `a > b`.
/// Unparseable versions compare as equal to `0.0.0`.
pub fn compare_semver(a: &str, b: &str) -> i32 {
    let pa = parse_semver_parts(a).unwrap_or((0, 0, 0));
    let pb = parse_semver_parts(b).unwrap_or((0, 0, 0));
    match pa.0.cmp(&pb.0) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Greater => 1,
        std::cmp::Ordering::Equal => match pa.1.cmp(&pb.1) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Greater => 1,
            std::cmp::Ordering::Equal => match pa.2.cmp(&pb.2) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Greater => 1,
                std::cmp::Ordering::Equal => 0,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_compare() {
        assert!(compare_semver("0.1.8", "0.1.7") > 0);
        assert!(compare_semver("0.1.8", "0.1.8") == 0);
        assert!(compare_semver("0.1.7", "0.1.8") < 0);
        assert!(compare_semver("0.2.60", "0.2.9") > 0);
        assert!(compare_semver("1.0.0", "0.9.9") > 0);
        assert!(compare_semver("0.2.0", "0.2.0") == 0);
    }

    #[test]
    fn extract_semver_from_version_output() {
        assert_eq!(extract_semver("actplane 0.1.8"), Some("0.1.8".into()));
        assert_eq!(
            extract_semver("agentsight-cli version 0.2.60 (linux)"),
            Some("0.2.60".into())
        );
        assert_eq!(extract_semver("v1.2.3-beta"), Some("1.2.3".into()));
        assert_eq!(extract_semver("no version here"), None);
    }

    #[test]
    fn install_hint_format() {
        assert_eq!(
            Components::install_hint("actplane"),
            "cargo install actplane"
        );
        assert_eq!(
            Components::install_hint("agentsight"),
            "cargo install agentsight"
        );
    }

    #[test]
    fn detect_returns_three_components() {
        let c = Components::detect();
        assert_eq!(c.actplane.name, "actplane");
        assert_eq!(c.agentsight.name, "agentsight");
        assert_eq!(c.akeep.name, "akeep");
        assert_eq!(c.actplane.min_version, ACTPLANE_MIN);
        assert_eq!(c.agentsight.min_version, AGENTSIGHT_MIN);
        assert_eq!(c.akeep.min_version, AKEEP_MIN);
    }

    #[test]
    fn component_not_present_is_not_ok() {
        let c = Component {
            name: "missing",
            path: None,
            version: None,
            min_version: "1.0.0",
        };
        assert!(!c.is_ok());
        assert!(!c.is_present());
    }

    #[test]
    fn component_old_version_not_ok() {
        let c = Component {
            name: "actplane",
            path: Some(PathBuf::from("/usr/bin/actplane")),
            version: Some("0.1.0".into()),
            min_version: "0.1.8",
        };
        assert!(!c.is_ok());
        assert!(c.is_present());
    }
}
