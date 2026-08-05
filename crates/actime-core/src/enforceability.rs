//! Per-rule enforceability against the host ActPlane engine feature budget.
//!
//! `actplane compile --json` reports static host/backend support. That is not
//! the same as what a released engine will actually install on the attach /
//! runtime-delta path. Released ActPlane 0.1.8 pins a host-wide singleton with
//! feature budget `0x3f0` (connect, recv, file flow, block exec/file/connect)
//! and does **not** enable path-contains/suffix matchers or open/write sink
//! rule classes (`missing=0xf`). Policies that need those features fail to
//! install entirely — Actime must detect that before claiming enforcement.
//!
//! This module combines compile-JSON clause shapes (kernel_op, target_kind,
//! path patterns, file sources) with the known engine budget so each rule can
//! be marked `enforceable` with a reason.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

// Feature bits mirror ActPlane `bpf/src/lib.rs` (released 0.1.8).
const FEAT_PATH_CONTAINS: u32 = 1 << 0;
const FEAT_PATH_SUFFIX: u32 = 1 << 1;
const FEAT_OPEN_RULES: u32 = 1 << 2;
const FEAT_WRITE_RULES: u32 = 1 << 3;
const FEAT_CONNECT: u32 = 1 << 4;
const FEAT_RECV: u32 = 1 << 5;
const FEAT_FILE_FLOW: u32 = 1 << 6;
const FEAT_BLOCK_EXEC: u32 = 1 << 7;
const FEAT_BLOCK_FILE: u32 = 1 << 8;
const FEAT_BLOCK_CONNECT: u32 = 1 << 9;

/// Feature budget of the ActPlane 0.1.8 pinned singleton (`supported=0x3f0`).
///
/// Path contains/suffix matchers and open/write sink rule classes are omitted.
pub const ENGINE_SUPPORTED_0_1_8: u32 = FEAT_CONNECT
    | FEAT_RECV
    | FEAT_FILE_FLOW
    | FEAT_BLOCK_EXEC
    | FEAT_BLOCK_FILE
    | FEAT_BLOCK_CONNECT;

/// Per-rule enforceability verdict for the composed policy on this host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleEnforceability {
    /// Rule name (`rule <name>:`).
    pub name: String,
    /// Dominant effect among clauses (`kill` / `block` / `notify`).
    pub effect: String,
    /// Whether this host's engine can install and fire the rule.
    pub enforceable: bool,
    /// Why not, or a short note when it is enforceable.
    pub reason: String,
}

/// Resolve the engine feature mask Actime assumes for this ActPlane version.
///
/// Unknown / older versions still use the conservative 0.1.8 pin budget: that
/// is what shipped hosts actually load today.
pub fn engine_supported_features(actplane_version: Option<&str>) -> u32 {
    let _ = actplane_version;
    ENGINE_SUPPORTED_0_1_8
}

/// Assess every rule in an `actplane compile --json` report.
///
/// When `install_error` is set (engine refused the whole delta), every rule is
/// unenforceable with that reason — never a silent subset.
pub fn assess_compile_json(
    report: &Value,
    engine_supported: u32,
    install_error: Option<&str>,
) -> Vec<RuleEnforceability> {
    let file_sources = file_sources_from_compile(report);
    let mut by_name: BTreeMap<String, RuleAccum> = BTreeMap::new();

    if let Some(rules) = report.get("rules").and_then(|r| r.as_array()) {
        for r in rules {
            let name = r
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            let effect = r
                .get("effect")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let op = r
                .get("kernel_op")
                .or_else(|| r.get("clause_op"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let target_kind = r.get("target_kind").and_then(|v| v.as_str()).unwrap_or("");
            let pattern = r
                .get("target_pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let clause_text = r.get("clause_text").and_then(|v| v.as_str()).unwrap_or("");
            let source_text = r.get("source_text").and_then(|v| v.as_str()).unwrap_or("");

            let entry = by_name.entry(name).or_default();
            if !effect.is_empty() {
                entry.effects.insert(effect);
            }
            entry.needed |= features_for_clause(op, target_kind, pattern);
            entry.needed |= features_for_labels(clause_text, &file_sources);
            entry.needed |= features_for_labels(source_text, &file_sources);
        }
    }

    // Prefer backend_support.clauses when rules[] is empty/older.
    if by_name.is_empty() {
        if let Some(clauses) = report
            .pointer("/backend_support/clauses")
            .and_then(|c| c.as_array())
        {
            for c in clauses {
                let name = c
                    .get("rule")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string();
                let effect = c
                    .get("effect")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let op = c.get("op").and_then(|v| v.as_str()).unwrap_or("");
                let target_kind = c.get("target_kind").and_then(|v| v.as_str()).unwrap_or("");
                let pattern = c
                    .get("target_pattern")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let entry = by_name.entry(name).or_default();
                if !effect.is_empty() {
                    entry.effects.insert(effect);
                }
                entry.needed |= features_for_clause(op, target_kind, pattern);
            }
        }
    }

    // File sources in the policy still consume feature budget at install time.
    // Rules that do not reference them stay independent; sources alone do not
    // invent rule names.

    finish_rows(by_name, engine_supported, install_error)
}

/// Assess rules by scanning composed DSL when compile JSON is unavailable.
pub fn assess_dsl(
    dsl: &str,
    engine_supported: u32,
    install_error: Option<&str>,
) -> Vec<RuleEnforceability> {
    let file_sources = file_sources_from_dsl(dsl);
    let mut by_name: BTreeMap<String, RuleAccum> = BTreeMap::new();
    let mut current: Option<String> = None;

    for line in dsl.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("rule ") {
            if let Some(name) = rest.strip_suffix(':').map(str::trim) {
                current = Some(name.to_string());
                by_name.entry(name.to_string()).or_default();
            }
            continue;
        }
        let Some(name) = current.as_ref() else {
            continue;
        };
        if t.starts_with("because ") {
            continue;
        }
        if !(t.starts_with("kill ")
            || t.starts_with("block ")
            || t.starts_with("notify ")
            || t.starts_with("if ")
            || t.starts_with("unless "))
        {
            continue;
        }
        let entry = by_name.entry(name.clone()).or_default();
        if t.starts_with("kill ") {
            entry.effects.insert("kill".into());
        } else if t.starts_with("block ") {
            entry.effects.insert("block".into());
        } else if t.starts_with("notify ") {
            entry.effects.insert("notify".into());
        }
        entry.needed |= features_from_dsl_clause(t);
        entry.needed |= features_for_labels(t, &file_sources);
    }

    finish_rows(by_name, engine_supported, install_error)
}

/// Convenience: unenforceable subset only.
pub fn unenforceable_only(rows: &[RuleEnforceability]) -> Vec<RuleEnforceability> {
    rows.iter().filter(|r| !r.enforceable).cloned().collect()
}

// ---------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RuleAccum {
    effects: BTreeSet<String>,
    needed: u32,
}

fn finish_rows(
    by_name: BTreeMap<String, RuleAccum>,
    engine_supported: u32,
    install_error: Option<&str>,
) -> Vec<RuleEnforceability> {
    by_name
        .into_iter()
        .map(|(name, acc)| {
            let effect = dominant_effect(&acc.effects);
            if let Some(err) = install_error {
                return RuleEnforceability {
                    name,
                    effect,
                    enforceable: false,
                    reason: format!("policy did not install: {err}"),
                };
            }
            let missing = acc.needed & !engine_supported;
            if missing == 0 {
                RuleEnforceability {
                    name,
                    effect,
                    enforceable: true,
                    reason: "enforceable on this host's ActPlane engine".into(),
                }
            } else {
                RuleEnforceability {
                    name,
                    effect,
                    enforceable: false,
                    reason: format!(
                        "engine missing features required on attach/delta path: {}",
                        feature_names(missing).join(", ")
                    ),
                }
            }
        })
        .collect()
}

fn dominant_effect(effects: &BTreeSet<String>) -> String {
    for want in ["kill", "block", "notify"] {
        if effects.iter().any(|e| e == want) {
            return want.to_string();
        }
    }
    effects.iter().next().cloned().unwrap_or_else(|| "—".into())
}

fn features_for_clause(op: &str, target_kind: &str, pattern: &str) -> u32 {
    let mut f = 0u32;
    let op = op.to_ascii_lowercase();
    let kind = target_kind.to_ascii_lowercase();
    match (op.as_str(), kind.as_str()) {
        ("open", "file") => {
            f |= FEAT_OPEN_RULES;
            f |= path_match_features(pattern);
        }
        ("write" | "unlink", "file") => {
            f |= FEAT_WRITE_RULES | FEAT_FILE_FLOW;
            f |= path_match_features(pattern);
        }
        ("connect", _) => {
            f |= FEAT_CONNECT;
        }
        ("recv", _) => {
            f |= FEAT_RECV;
        }
        ("exec", _) => {
            // Exec sinks load under the pinned hook budget.
        }
        _ => {
            if kind == "file" {
                f |= path_match_features(pattern);
            }
        }
    }
    f
}

fn features_from_dsl_clause(line: &str) -> u32 {
    let t = line.trim_start();
    // kill/block/notify <op> <kind> "pattern" …
    let body = t
        .strip_prefix("kill ")
        .or_else(|| t.strip_prefix("block "))
        .or_else(|| t.strip_prefix("notify "))
        .unwrap_or(t);
    let mut parts = body.split_whitespace();
    let op = parts.next().unwrap_or("");
    let kind = parts.next().unwrap_or("");
    let pattern = extract_first_quoted(body).unwrap_or_default();
    features_for_clause(op, kind, &pattern)
}

/// Labels referenced in a clause condition that are defined as file sources
/// pull in those sources' path-matcher features (label propagation).
fn features_for_labels(text: &str, file_sources: &BTreeMap<String, Vec<String>>) -> u32 {
    if file_sources.is_empty() {
        return 0;
    }
    let mut f = 0u32;
    for (label, patterns) in file_sources {
        if label_mentioned(text, label) {
            f |= FEAT_FILE_FLOW;
            for p in patterns {
                f |= path_match_features(p);
            }
        }
    }
    f
}

fn label_mentioned(text: &str, label: &str) -> bool {
    // Word-boundary style: avoid matching SECRET inside SUPERSECRET.
    let bytes = text.as_bytes();
    let lab = label.as_bytes();
    if lab.is_empty() {
        return false;
    }
    let mut i = 0;
    while i + lab.len() <= bytes.len() {
        if &bytes[i..i + lab.len()] == lab {
            let before_ok = i == 0 || !is_ident_byte(bytes[i - 1]);
            let after = i + lab.len();
            let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn file_sources_from_compile(report: &Value) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Some(sources) = report
        .pointer("/backend_support/sources")
        .and_then(|s| s.as_array())
    {
        for s in sources {
            let kind = s.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if kind != "file" {
                continue;
            }
            let label = s
                .get("label")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let pattern = s
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !label.is_empty() {
                out.entry(label).or_default().push(pattern);
            }
        }
    }
    out
}

fn file_sources_from_dsl(dsl: &str) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in dsl.lines() {
        let t = line.trim();
        // source SECRET = file "pattern"
        let Some(rest) = t.strip_prefix("source ") else {
            continue;
        };
        let Some((label, rhs)) = rest.split_once('=') else {
            continue;
        };
        let label = label.trim();
        let rhs = rhs.trim();
        if !rhs.starts_with("file ") {
            continue;
        }
        if let Some(pat) = extract_first_quoted(rhs) {
            out.entry(label.to_string()).or_default().push(pat);
        }
    }
    out
}

fn extract_first_quoted(s: &str) -> Option<String> {
    let start = s.find('"')?;
    let rest = &s[start + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Approximate ActPlane path lowering for feature bits only.
///
/// Mirrors the expensive matcher classes in `actplane-ifc-compiler` path
/// lowering: contains and suffix. Exact/prefix/any do not add path features.
fn path_match_features(pat: &str) -> u32 {
    if pat.is_empty() || pat == "*" || pat == "**" || pat == "**/*" {
        return 0;
    }
    let repo_relative = !pat.starts_with('/');

    if let Some(inner) = pat.strip_prefix("**/").and_then(|r| r.strip_suffix("/**")) {
        if !inner.contains('*') {
            return FEAT_PATH_CONTAINS;
        }
    }
    if let Some(inner) = pat.strip_prefix("**/").and_then(|r| r.strip_suffix("/*")) {
        if !inner.contains('*') {
            return FEAT_PATH_CONTAINS;
        }
    }
    if let Some(inner) = pat.strip_prefix("**/") {
        if inner.starts_with('*') {
            return FEAT_PATH_SUFFIX;
        }
        if !inner.contains('*') {
            return FEAT_PATH_SUFFIX;
        }
        return FEAT_PATH_CONTAINS;
    }
    if let Some(p) = pat.strip_suffix("/**") {
        if repo_relative && !p.contains('*') {
            return FEAT_PATH_CONTAINS;
        }
        return 0; // absolute prefix
    }
    if let Some(p) = pat.strip_suffix("**") {
        if repo_relative && !p.contains('*') {
            return FEAT_PATH_CONTAINS;
        }
        return 0;
    }
    if let Some(p) = pat.strip_suffix("/*") {
        if repo_relative && !p.contains('*') {
            return FEAT_PATH_CONTAINS;
        }
        return 0;
    }
    if let Some(rest) = pat.strip_prefix('*') {
        if repo_relative {
            return FEAT_PATH_CONTAINS;
        }
        if !rest.is_empty() {
            return FEAT_PATH_SUFFIX;
        }
        return 0;
    }
    if pat.contains('*') {
        if repo_relative {
            return FEAT_PATH_CONTAINS;
        }
        return 0; // absolute prefix-style
    }
    if repo_relative {
        return FEAT_PATH_CONTAINS;
    }
    0
}

fn feature_names(bits: u32) -> Vec<&'static str> {
    let mut names = Vec::new();
    if bits & FEAT_PATH_CONTAINS != 0 {
        names.push("path contains matches");
    }
    if bits & FEAT_PATH_SUFFIX != 0 {
        names.push("path suffix matches");
    }
    if bits & FEAT_OPEN_RULES != 0 {
        names.push("open sink rules");
    }
    if bits & FEAT_WRITE_RULES != 0 {
        names.push("write sink rules");
    }
    if bits & FEAT_CONNECT != 0 {
        names.push("connect rules");
    }
    if bits & FEAT_RECV != 0 {
        names.push("recv rules");
    }
    if bits & FEAT_FILE_FLOW != 0 {
        names.push("file flow");
    }
    if bits & FEAT_BLOCK_EXEC != 0 {
        names.push("block exec");
    }
    if bits & FEAT_BLOCK_FILE != 0 {
        names.push("block file");
    }
    if bits & FEAT_BLOCK_CONNECT != 0 {
        names.push("block connect");
    }
    if names.is_empty() {
        names.push("unknown features");
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exec_only_rules_are_enforceable_on_0_1_8() {
        let dsl = r#"
source AGENT = exec "**/claude"
rule destructive-vcs:
  kill exec "git" "--force" if AGENT
  because "no force"
rule mass-deletion:
  kill exec "rm" "-rf" if AGENT
  because "no rm"
"#;
        let rows = assess_dsl(dsl, ENGINE_SUPPORTED_0_1_8, None);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.enforceable), "{rows:?}");
    }

    #[test]
    fn file_write_and_open_rules_are_not_enforceable() {
        let dsl = r#"
source AGENT = exec "**/claude"
rule system-fence:
  block write file "/etc/**" if AGENT
  because "no etc"
rule credential-access:
  notify open file "**/.ssh/id_*" if AGENT
  because "cred"
"#;
        let rows = assess_dsl(dsl, ENGINE_SUPPORTED_0_1_8, None);
        assert_eq!(rows.len(), 2);
        let fence = rows.iter().find(|r| r.name == "system-fence").unwrap();
        assert!(!fence.enforceable);
        assert!(
            fence.reason.contains("write sink") || fence.reason.contains("path contains"),
            "{}",
            fence.reason
        );
        let cred = rows.iter().find(|r| r.name == "credential-access").unwrap();
        assert!(!cred.enforceable);
        assert!(
            cred.reason.contains("open sink") || cred.reason.contains("path"),
            "{}",
            cred.reason
        );
    }

    #[test]
    fn secret_egress_needs_path_features_via_file_source() {
        let dsl = r#"
source AGENT = exec "**/claude"
source SECRET = file "**/.env"
rule no-secret-egress:
  kill connect endpoint "*" if AGENT and SECRET
  because "no egress"
"#;
        let rows = assess_dsl(dsl, ENGINE_SUPPORTED_0_1_8, None);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].enforceable, "{rows:?}");
        assert!(
            rows[0].reason.contains("path suffix")
                || rows[0].reason.contains("path contains")
                || rows[0].reason.contains("missing features"),
            "{}",
            rows[0].reason
        );
    }

    #[test]
    fn pure_connect_without_file_label_is_enforceable() {
        let dsl = r#"
source AGENT = exec "**/claude"
rule no-net:
  kill connect endpoint "*" if AGENT
  because "no net"
"#;
        let rows = assess_dsl(dsl, ENGINE_SUPPORTED_0_1_8, None);
        assert!(rows[0].enforceable, "{rows:?}");
    }

    #[test]
    fn install_error_marks_every_rule_unenforceable() {
        let dsl = r#"
rule destructive-vcs:
  kill exec "git" "--force" if AGENT
"#;
        let rows = assess_dsl(dsl, ENGINE_SUPPORTED_0_1_8, Some("path contains matches"));
        assert!(!rows[0].enforceable);
        assert!(rows[0].reason.contains("path contains"));
    }

    #[test]
    fn assess_compile_json_groups_by_rule_name() {
        let report = json!({
            "rules": [
                {
                    "name": "destructive-vcs",
                    "effect": "kill",
                    "kernel_op": "exec",
                    "target_kind": "exec",
                    "target_pattern": "**/git",
                    "clause_text": "  kill exec \"git\" \"--force\" if AGENT"
                },
                {
                    "name": "system-fence",
                    "effect": "block",
                    "kernel_op": "write",
                    "target_kind": "file",
                    "target_pattern": "/etc/**",
                    "clause_text": "  block write file \"/etc/**\" if AGENT"
                }
            ],
            "backend_support": { "sources": [] }
        });
        let rows = assess_compile_json(&report, ENGINE_SUPPORTED_0_1_8, None);
        assert_eq!(rows.len(), 2);
        assert!(rows
            .iter()
            .any(|r| r.name == "destructive-vcs" && r.enforceable));
        assert!(rows
            .iter()
            .any(|r| r.name == "system-fence" && !r.enforceable));
    }
}
