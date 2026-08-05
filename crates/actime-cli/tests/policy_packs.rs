//! Static checks on the shipped ActPlane policy packs under `policies/*.dsl`.
//!
//! These do not invoke the `actime` binary. They assert structural invariants
//! of every pack the binary embeds: at least one source, at least one rule
//! block, and a `because` clause for every rule.

use std::fs;
use std::path::{Path, PathBuf};

/// Repo-level `policies/` directory (relative to this crate's manifest).
fn policies_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../policies")
}

/// One rule block extracted from a pack: name + whether a because was seen.
struct RuleBlock {
    name: String,
    has_because: bool,
}

/// Scan ActPlane DSL for `source` lines and `rule` / `because` structure.
fn analyze_pack(source: &str) -> (usize, Vec<RuleBlock>) {
    let mut sources = 0usize;
    let mut rules: Vec<RuleBlock> = Vec::new();

    for line in source.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("source ") {
            sources += 1;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("rule ") {
            // `rule name:` or `rule name :`
            let name = rest.split(':').next().unwrap_or(rest).trim().to_string();
            rules.push(RuleBlock {
                name,
                has_because: false,
            });
            continue;
        }
        if trimmed.starts_with("because ") || trimmed.starts_with("because\"") {
            if let Some(last) = rules.last_mut() {
                last.has_because = true;
            }
        }
    }

    (sources, rules)
}

fn load_dsl_files(dir: &Path) -> Vec<(String, String)> {
    let mut packs = Vec::new();
    let entries = fs::read_dir(dir).unwrap_or_else(|e| {
        panic!(
            "reading policies dir {}: {e} (run tests from the actime workspace)",
            dir.display()
        )
    });
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("dsl") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
        let text =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        packs.push((name, text));
    }
    packs.sort_by(|a, b| a.0.cmp(&b.0));
    packs
}

#[test]
fn every_shipped_pack_has_sources_rules_and_because() {
    let dir = policies_dir();
    assert!(
        dir.is_dir(),
        "policies directory missing at {}",
        dir.display()
    );

    let packs = load_dsl_files(&dir);
    assert!(
        !packs.is_empty(),
        "expected at least one .dsl under {}",
        dir.display()
    );

    // The three packs the CLI embeds and `policy list` names.
    let expected = ["coding-agent-baseline", "information-flow", "no-vcs-write"];
    for name in expected {
        assert!(
            packs.iter().any(|(n, _)| n == name),
            "missing shipped pack {name}.dsl under {}",
            dir.display()
        );
    }

    for (name, source) in &packs {
        let (sources, rules) = analyze_pack(source);
        assert!(
            sources >= 1,
            "pack {name} must have at least one `source` line"
        );
        assert!(
            !rules.is_empty(),
            "pack {name} must have at least one `rule` block"
        );
        for rule in &rules {
            assert!(
                rule.has_because,
                "pack {name}: rule `{}` is missing a `because` clause",
                rule.name
            );
        }
    }
}

#[test]
fn packs_are_nonempty_text() {
    let packs = load_dsl_files(&policies_dir());
    for (name, source) in packs {
        assert!(
            source
                .lines()
                .any(|l| !l.trim().is_empty() && !l.trim().starts_with('#')),
            "pack {name} has no non-comment content"
        );
    }
}
