//! Terminal presentation helpers.
//!
//! Colors are applied here and nowhere else: `actime-core` renders plain text
//! so its output stays usable in files, pipes, and `report.md`.

use std::io::IsTerminal;

/// Whether ANSI styling should be emitted on stderr.
pub fn color_enabled() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if matches!(std::env::var("TERM").as_deref(), Ok("dumb")) {
        return false;
    }
    std::io::stderr().is_terminal()
}

fn paint(code: &str, s: &str) -> String {
    if color_enabled() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Dim secondary text.
pub fn dim(s: &str) -> String {
    paint("2", s)
}

/// Bold primary text.
pub fn bold(s: &str) -> String {
    paint("1", s)
}

/// Green, for an active plane or a passing check.
pub fn green(s: &str) -> String {
    paint("32", s)
}

/// Yellow, for a degraded plane or a warning.
pub fn yellow(s: &str) -> String {
    paint("33", s)
}

/// Red, for a failure or a killed action.
pub fn red(s: &str) -> String {
    paint("31", s)
}

/// The terminal width, clamped to something readable.
pub fn width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .or_else(term_width)
        .unwrap_or(80)
        .clamp(60, 120)
}

#[cfg(unix)]
fn term_width() -> Option<usize> {
    // SAFETY: `winsize` is plain data and `ioctl` only writes into it.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            Some(ws.ws_col as usize)
        } else {
            None
        }
    }
}

#[cfg(not(unix))]
fn term_width() -> Option<usize> {
    None
}

/// A status banner printed when a run starts.
pub fn banner(run_id: &str, target: &str, policy: &str, evidence: &str) -> String {
    format!(
        "{}  run {}   {} {}   {} {}   {} {}",
        bold("actime"),
        run_id,
        dim("target:"),
        target,
        dim("policy:"),
        policy,
        dim("evidence:"),
        evidence,
    )
}

/// A one-line warning, prefixed consistently.
pub fn warn(msg: &str) -> String {
    format!("{} {}", yellow("warning:"), msg)
}

/// A one-line note, prefixed consistently.
pub fn note(msg: &str) -> String {
    format!("{} {}", dim("note:"), msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_is_clamped_to_a_readable_range() {
        // The exact value depends on the environment; the bounds do not.
        let w = width();
        assert!((60..=120).contains(&w), "width was {w}");
    }

    #[test]
    fn plain_text_when_color_is_disabled() {
        // Under `cargo test` stderr is not a tty, so styling is a no-op and
        // the helpers must be identity functions.
        if !color_enabled() {
            assert_eq!(bold("x"), "x");
            assert_eq!(red("x"), "x");
            assert_eq!(dim("x"), "x");
        }
    }

    #[test]
    fn banner_mentions_target_and_planes() {
        let b = banner("20260804-1", "command", "enforce", "on");
        for needle in ["20260804-1", "command", "enforce", "target", "policy"] {
            assert!(b.contains(needle), "banner missing {needle}: {b}");
        }
    }
}
