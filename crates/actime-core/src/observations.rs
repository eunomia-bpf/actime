//! Observations aggregation: violations, AgentSight SQLite, and timeline.
//!
//! [`Observations::collect`] is fail-soft: malformed JSONL lines are skipped, and
//! an unknown or incomplete AgentSight schema degrades to zero counts rather
//! than failing the whole collection.

use std::fs;
use std::io::{BufRead, BufReader};

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::run::{Run, RunSummary};

/// Maximum timeline entries retained after merge/sort.
pub const TIMELINE_CAP: usize = 500;

/// One policy violation (one line of `violations.jsonl`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Violation {
    /// Timestamp (typically RFC 3339 or engine-native).
    pub ts: String,
    /// Rule identifier that fired.
    pub rule: String,
    /// Effect applied: `notify` | `block` | `kill`.
    pub effect: String,
    /// Operation (e.g. `connect`, `open`, `exec`).
    pub op: String,
    /// Target of the operation (path, host, argv, …).
    pub target: String,
    /// Process id.
    pub pid: i32,
    /// Process command name.
    pub comm: String,
    /// Human-readable reason.
    pub reason: String,
}

/// One entry in the merged chronological timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineEntry {
    /// Timestamp string used for ordering.
    pub ts: String,
    /// Kind: `violation`, `event`, `llm`, `process`, …
    pub kind: String,
    /// Short human summary.
    pub summary: String,
}

/// Aggregated observations for a run.
///
/// [`Default`] is the empty observations set, which is what callers fall back to
/// when a run directory has no violations file and no observability database. That
/// fallback is deliberate: a report is always produced, even for a run where
/// every eBPF plane was disabled.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Observations {
    /// Parsed policy violations (malformed lines skipped).
    pub violations: Vec<Violation>,
    /// Summary counters (merged from violations + observability.db + events).
    pub summary: RunSummary,
    /// Chronological timeline, capped at [`TIMELINE_CAP`].
    pub timeline: Vec<TimelineEntry>,
}

impl Observations {
    /// Collect observations from a run directory.
    ///
    /// Reads `violations.jsonl`, optionally `observability.db` (schema-tolerant),
    /// and `events.jsonl`. Never fails solely because of schema mismatch.
    pub fn collect(run: &Run) -> Result<Observations> {
        let violations = read_violations(&run.violations_path())?;
        let mut summary = RunSummary::default();

        // Seed summary from the manifest (duration, exit-related counters).
        summary.duration_seconds = run.manifest.summary.duration_seconds;
        summary.cpu_seconds = run.manifest.summary.cpu_seconds;
        summary.peak_rss_bytes = run.manifest.summary.peak_rss_bytes;

        // Violation counters.
        summary.violations = violations.len() as u64;
        for v in &violations {
            match v.effect.to_ascii_lowercase().as_str() {
                "block" => summary.blocked += 1,
                "kill" => summary.killed += 1,
                _ => {}
            }
        }

        // AgentSight SQLite — never fail on schema issues.
        if run.observability_db_path().is_file() {
            if let Ok(db_counts) = collect_from_sqlite(&run.observability_db_path()) {
                merge_db_counts(&mut summary, &db_counts);
            }
        }

        // events.jsonl → timeline + light counters.
        let mut timeline: Vec<TimelineEntry> = Vec::new();
        for v in &violations {
            timeline.push(TimelineEntry {
                ts: v.ts.clone(),
                kind: "violation".into(),
                summary: format!("[{}] {} {} — {}", v.effect, v.rule, v.target, v.reason),
            });
        }

        if run.events_path().is_file() {
            let events = read_events(&run.events_path())?;
            for ev in events {
                // Light counter bumps from event kinds.
                match ev.kind.to_ascii_lowercase().as_str() {
                    "process" | "exec" | "spawn" => summary.processes += 1,
                    "file_write" | "write" => summary.files_written += 1,
                    "connect" | "network" | "endpoint" => summary.endpoints += 1,
                    "llm" | "llm_call" => summary.llm_calls += 1,
                    _ => {}
                }
                timeline.push(TimelineEntry {
                    ts: ev.ts,
                    kind: ev.kind,
                    summary: ev.summary,
                });
            }
        }

        // Sort by timestamp string (RFC3339 sorts lexicographically).
        timeline.sort_by(|a, b| a.ts.cmp(&b.ts));
        if timeline.len() > TIMELINE_CAP {
            timeline.truncate(TIMELINE_CAP);
        }

        Ok(Observations {
            violations,
            summary,
            timeline,
        })
    }
}

fn read_violations(path: &std::path::Path) -> Result<Vec<Violation>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let f = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(f);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line.with_context(|| format!("reading {}", path.display()))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(v) = parse_violation_line(line) {
            out.push(v);
        }
        // Skip malformed lines — never fail the whole collect.
    }
    Ok(out)
}

/// Parse one violations.jsonl line.
///
/// Accepts both Actime's flat [`Violation`] shape and ActPlane 0.1.x's
/// `actplane.violation.v1` envelope (`rule: { name, reason }`,
/// `timestamp_unix_ns`, …).
fn parse_violation_line(line: &str) -> Option<Violation> {
    if let Ok(v) = serde_json::from_str::<Violation>(line) {
        if !v.rule.is_empty() || !v.effect.is_empty() {
            return Some(v);
        }
    }
    let raw: serde_json::Value = serde_json::from_str(line).ok()?;
    let obj = raw.as_object()?;

    let rule = obj
        .get("rule")
        .and_then(|r| {
            r.as_str()
                .map(str::to_string)
                .or_else(|| r.get("name").and_then(|n| n.as_str()).map(str::to_string))
        })
        .or_else(|| {
            obj.get("actplane_rule")
                .and_then(|n| n.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();

    let effect = obj
        .get("effect")
        .or_else(|| obj.get("action"))
        .and_then(|e| e.as_str())
        .unwrap_or("")
        .to_string();

    let reason = obj
        .get("reason")
        .and_then(|r| r.as_str())
        .map(str::to_string)
        .or_else(|| {
            obj.get("rule")
                .and_then(|r| r.get("reason"))
                .and_then(|r| r.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();

    let op = obj
        .get("op")
        .and_then(|o| o.as_str())
        .unwrap_or("")
        .to_string();
    let target = obj
        .get("target")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    let comm = obj
        .get("comm")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    let pid = obj
        .get("pid")
        .and_then(|p| p.as_i64().or_else(|| p.as_u64().map(|u| u as i64)))
        .unwrap_or(0) as i32;

    let ts = obj
        .get("ts")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .or_else(|| {
            obj.get("timestamp_unix_ns").and_then(|t| {
                t.as_str()
                    .map(str::to_string)
                    .or_else(|| t.as_u64().map(|n| n.to_string()))
            })
        })
        .unwrap_or_default();

    if rule.is_empty() && effect.is_empty() {
        return None;
    }
    Some(Violation {
        ts,
        rule,
        effect,
        op,
        target,
        pid,
        comm,
        reason,
    })
}

/// Loose event record from `events.jsonl`.
#[derive(Debug, Deserialize)]
struct EventLine {
    #[serde(default)]
    ts: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    summary: String,
    /// Alternate fields some engines use.
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    event: Option<String>,
}

fn read_events(path: &std::path::Path) -> Result<Vec<TimelineEntry>> {
    let f = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(f);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line.with_context(|| format!("reading {}", path.display()))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(raw) = serde_json::from_str::<EventLine>(line) else {
            continue;
        };
        let kind = if !raw.kind.is_empty() {
            raw.kind
        } else {
            raw.event.unwrap_or_else(|| "event".into())
        };
        let summary = if !raw.summary.is_empty() {
            raw.summary
        } else {
            raw.message.unwrap_or_default()
        };
        out.push(TimelineEntry {
            ts: raw.ts,
            kind,
            summary,
        });
    }
    Ok(out)
}

#[derive(Debug, Default)]
struct DbCounts {
    processes: u64,
    files_written: u64,
    endpoints: u64,
    llm_calls: u64,
    tokens_in: u64,
    tokens_out: u64,
    peak_rss_bytes: u64,
    cpu_seconds: f64,
}

fn merge_db_counts(summary: &mut RunSummary, db: &DbCounts) {
    // Prefer DB counts when they are non-zero (more authoritative).
    if db.processes > 0 {
        summary.processes = db.processes;
    }
    if db.files_written > 0 {
        summary.files_written = db.files_written;
    }
    if db.endpoints > 0 {
        summary.endpoints = db.endpoints;
    }
    if db.llm_calls > 0 {
        summary.llm_calls = db.llm_calls;
    }
    if db.tokens_in > 0 {
        summary.tokens_in = db.tokens_in;
    }
    if db.tokens_out > 0 {
        summary.tokens_out = db.tokens_out;
    }
    if db.peak_rss_bytes > 0 {
        summary.peak_rss_bytes = db.peak_rss_bytes;
    }
    if db.cpu_seconds > 0.0 {
        summary.cpu_seconds = db.cpu_seconds;
    }
}

/// Open observability.db read-only and aggregate known tables if present.
///
/// On any error or schema mismatch, returns zero counts (caller treats as soft).
fn collect_from_sqlite(path: &std::path::Path) -> Result<DbCounts> {
    // Open read-only. Prefer immutable so a root-owned DB left by a sudo
    // agentsight run does not fail queries with "attempt to write a readonly
    // database" (SQLite otherwise tries to materialize a WAL index). Fall
    // back to plain mode=ro, then a normal open.
    let conn =
        open_observability_db(path).with_context(|| format!("opening {}", path.display()))?;

    let tables = list_tables(&conn).unwrap_or_default();
    let mut counts = DbCounts::default();

    if tables.iter().any(|t| t == "llm_calls") {
        let cols = table_columns(&conn, "llm_calls").unwrap_or_default();
        if let Ok(n) = count_rows(&conn, "llm_calls") {
            counts.llm_calls = n;
        }
        // Sum token columns if present.
        if cols.iter().any(|c| c == "tokens_in" || c == "input_tokens") {
            let col = if cols.iter().any(|c| c == "tokens_in") {
                "tokens_in"
            } else {
                "input_tokens"
            };
            if let Ok(v) = sum_i64(&conn, "llm_calls", col) {
                counts.tokens_in = v as u64;
            }
        }
        if cols
            .iter()
            .any(|c| c == "tokens_out" || c == "output_tokens")
        {
            let col = if cols.iter().any(|c| c == "tokens_out") {
                "tokens_out"
            } else {
                "output_tokens"
            };
            if let Ok(v) = sum_i64(&conn, "llm_calls", col) {
                counts.tokens_out = v as u64;
            }
        }
    }

    // AgentSight (0.2.x) uses `audit_type` (not `kind` / `type`). Accept all
    // known aliases so a schema rename never zeros the report.
    if tables.iter().any(|t| t == "audit_events") {
        let cols = table_columns(&conn, "audit_events").unwrap_or_default();
        if let Some(kind_col) = first_present(
            &cols,
            &["audit_type", "kind", "event_type", "type", "event"],
        ) {
            // Prefer process_nodes for process counts when present; still
            // accumulate file/network from audit rows.
            let proc = count_where_like(&conn, "audit_events", kind_col, "%process%").unwrap_or(0)
                + count_where_like(&conn, "audit_events", kind_col, "%exec%").unwrap_or(0);
            if counts.processes == 0 {
                counts.processes = proc;
            }
            // File writes: audit_type=file and/or action containing write.
            if cols.iter().any(|c| c == "action") {
                counts.files_written +=
                    count_where_like(&conn, "audit_events", "action", "%write%").unwrap_or(0);
            } else {
                counts.files_written +=
                    count_where_like(&conn, "audit_events", kind_col, "%write%").unwrap_or(0);
                counts.files_written +=
                    count_where_like(&conn, "audit_events", kind_col, "%file%").unwrap_or(0);
            }
            counts.endpoints +=
                count_where_like(&conn, "audit_events", kind_col, "%network%").unwrap_or(0);
            counts.endpoints +=
                count_where_like(&conn, "audit_events", kind_col, "%connect%").unwrap_or(0);
        } else if let Ok(n) = count_rows(&conn, "audit_events") {
            if counts.processes == 0 {
                counts.processes = n;
            }
        }
    }

    if tables.iter().any(|t| t == "process_nodes") {
        if let Ok(n) = count_rows(&conn, "process_nodes") {
            // process_nodes is authoritative for process count when non-empty.
            if n > 0 {
                counts.processes = n;
            }
        }
        let cols = table_columns(&conn, "process_nodes").unwrap_or_default();
        if let Some(col) = first_present(&cols, &["peak_rss_bytes", "peak_rss", "rss", "vm_hwm_kb"])
        {
            if let Ok(v) = max_i64(&conn, "process_nodes", col) {
                // vm_hwm_kb is kilobytes; convert to bytes when that column is used.
                counts.peak_rss_bytes = if col == "vm_hwm_kb" {
                    (v as u64).saturating_mul(1024)
                } else {
                    v as u64
                };
            }
        }
        if let Some(col) = first_present(&cols, &["cpu_seconds", "cpu_sec"]) {
            if let Ok(v) = sum_f64(&conn, "process_nodes", col) {
                counts.cpu_seconds = v;
            }
        }
    }

    // AgentSight stores distinct endpoints in `network_targets`.
    if tables.iter().any(|t| t == "network_targets") {
        if let Ok(n) = count_rows(&conn, "network_targets") {
            if n > 0 {
                counts.endpoints = n;
            }
        }
    }

    Ok(counts)
}

/// Open an AgentSight SQLite store for read aggregation.
fn open_observability_db(path: &std::path::Path) -> Result<Connection> {
    let path_s = path.display().to_string();
    // immutable=1: never attempt journal/WAL writes next to a root-owned file.
    let immutable = format!("file:{path_s}?immutable=1");
    if let Ok(c) = Connection::open_with_flags(
        &immutable,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    ) {
        return Ok(c);
    }
    let ro = format!("file:{path_s}?mode=ro");
    if let Ok(c) = Connection::open_with_flags(
        &ro,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    ) {
        return Ok(c);
    }
    Connection::open(path).map_err(Into::into)
}

/// First column name from `candidates` that appears in `cols`.
fn first_present<'a>(cols: &[String], candidates: &[&'a str]) -> Option<&'a str> {
    for c in candidates {
        if cols.iter().any(|x| x == c) {
            return Some(*c);
        }
    }
    None
}

/// True when the collected observations contains any non-trivial observation.
///
/// Used to stop the report from claiming the observability plane is `Active` when
/// the database is empty or unreadable.
pub fn has_observational_signal(summary: &RunSummary) -> bool {
    summary.processes > 0
        || summary.files_written > 0
        || summary.endpoints > 0
        || summary.llm_calls > 0
        || summary.tokens_in > 0
        || summary.tokens_out > 0
        || summary.peak_rss_bytes > 0
        || summary.cpu_seconds > 0.0
}

fn list_tables(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut names = Vec::new();
    for n in rows.flatten() {
        names.push(n);
    }
    Ok(names)
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    // PRAGMA table_info — table name cannot be bound; sanitize to identifier.
    if !is_safe_ident(table) {
        return Ok(Vec::new());
    }
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&sql)?;
    // table_info columns: cid, name, type, notnull, dflt_value, pk
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let mut cols = Vec::new();
    for n in rows.flatten() {
        cols.push(n);
    }
    Ok(cols)
}

fn is_safe_ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn count_rows(conn: &Connection, table: &str) -> Result<u64> {
    if !is_safe_ident(table) {
        return Ok(0);
    }
    let sql = format!("SELECT COUNT(*) FROM {table}");
    let n: i64 = conn.query_row(&sql, [], |row| row.get(0))?;
    Ok(n.max(0) as u64)
}

fn sum_i64(conn: &Connection, table: &str, col: &str) -> Result<i64> {
    if !is_safe_ident(table) || !is_safe_ident(col) {
        return Ok(0);
    }
    let sql = format!("SELECT COALESCE(SUM({col}), 0) FROM {table}");
    let n: i64 = conn.query_row(&sql, [], |row| row.get(0))?;
    Ok(n)
}

fn sum_f64(conn: &Connection, table: &str, col: &str) -> Result<f64> {
    if !is_safe_ident(table) || !is_safe_ident(col) {
        return Ok(0.0);
    }
    let sql = format!("SELECT COALESCE(SUM({col}), 0) FROM {table}");
    let n: f64 = conn.query_row(&sql, [], |row| row.get(0))?;
    Ok(n)
}

fn max_i64(conn: &Connection, table: &str, col: &str) -> Result<i64> {
    if !is_safe_ident(table) || !is_safe_ident(col) {
        return Ok(0);
    }
    let sql = format!("SELECT COALESCE(MAX({col}), 0) FROM {table}");
    let n: i64 = conn.query_row(&sql, [], |row| row.get(0))?;
    Ok(n)
}

fn count_where_like(conn: &Connection, table: &str, col: &str, pattern: &str) -> Result<u64> {
    if !is_safe_ident(table) || !is_safe_ident(col) {
        return Ok(0);
    }
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE LOWER({col}) LIKE ?1");
    let n: i64 = conn.query_row(&sql, [pattern.to_ascii_lowercase()], |row| row.get(0))?;
    Ok(n.max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::run::RunStore;
    use std::io::Write;
    use tempfile::TempDir;

    fn sample_violation(ts: &str, effect: &str) -> Violation {
        Violation {
            ts: ts.into(),
            rule: "no-secret-egress".into(),
            effect: effect.into(),
            op: "connect".into(),
            target: "evil.example.com:443".into(),
            pid: 1234,
            comm: "curl".into(),
            reason: "blocked egress to non-allowlisted host".into(),
        }
    }

    #[test]
    fn violations_jsonl_skips_malformed() {
        let tmp = TempDir::new().unwrap();
        let store = RunStore::open(tmp.path()).unwrap();
        let cfg = Config::default();
        let run = store.create(&["claude".into()], &cfg).unwrap();

        let path = run.violations_path();
        let mut f = fs::File::create(&path).unwrap();
        let v1 = sample_violation("2026-08-04T15:30:12Z", "block");
        let v2 = sample_violation("2026-08-04T15:30:13Z", "kill");
        writeln!(f, "{}", serde_json::to_string(&v1).unwrap()).unwrap();
        writeln!(f, "this is not json").unwrap();
        writeln!(f, "{{broken").unwrap();
        writeln!(f, "{}", serde_json::to_string(&v2).unwrap()).unwrap();
        writeln!(f).unwrap();

        let ev = Observations::collect(&run).unwrap();
        assert_eq!(ev.violations.len(), 2);
        assert_eq!(ev.summary.violations, 2);
        assert_eq!(ev.summary.blocked, 1);
        assert_eq!(ev.summary.killed, 1);
        assert!(ev.timeline.len() >= 2);
    }

    #[test]
    fn actplane_violation_v1_json_is_parsed() {
        // Regression: ActPlane 0.1.x writes nested `rule.name` / `rule.reason`
        // and `timestamp_unix_ns`, not Actime's flat Violation shape.
        let line = r#"{"action":"kill","effect":"kill","comm":"git","op":"exec","pid":1866,"rule":{"name":"destructive-vcs","reason":"no force"},"target":"/usr/bin/git","timestamp_unix_ns":"1785914473545828862","schema":"actplane.violation.v1"}"#;
        let v = parse_violation_line(line).expect("parse");
        assert_eq!(v.rule, "destructive-vcs");
        assert_eq!(v.effect, "kill");
        assert_eq!(v.op, "exec");
        assert_eq!(v.comm, "git");
        assert_eq!(v.target, "/usr/bin/git");
        assert_eq!(v.pid, 1866);
        assert_eq!(v.reason, "no force");
        assert!(!v.ts.is_empty());
    }

    #[test]
    fn collect_empty_run() {
        let tmp = TempDir::new().unwrap();
        let store = RunStore::open(tmp.path()).unwrap();
        let run = store.create(&["true".into()], &Config::default()).unwrap();
        let ev = Observations::collect(&run).unwrap();
        assert!(ev.violations.is_empty());
        assert_eq!(ev.summary.violations, 0);
        assert!(ev.timeline.is_empty());
    }

    #[test]
    fn sqlite_schema_mismatch_degrades() {
        let tmp = TempDir::new().unwrap();
        let store = RunStore::open(tmp.path()).unwrap();
        let run = store.create(&["true".into()], &Config::default()).unwrap();

        // Create a DB with unexpected tables/columns.
        {
            let conn = Connection::open(run.observability_db_path()).unwrap();
            conn.execute_batch(
                "CREATE TABLE weird_stuff (id INTEGER);
                 INSERT INTO weird_stuff VALUES (1);
                 CREATE TABLE llm_calls (id INTEGER, prompt TEXT);
                 INSERT INTO llm_calls VALUES (1, 'hi');
                 INSERT INTO llm_calls VALUES (2, 'yo');",
            )
            .unwrap();
        }

        let ev = Observations::collect(&run).unwrap();
        // llm_calls exists → count rows, no token columns → zero tokens.
        assert_eq!(ev.summary.llm_calls, 2);
        assert_eq!(ev.summary.tokens_in, 0);
        // No panic, no error.
    }

    #[test]
    fn agentsight_audit_type_schema_is_read() {
        // Regression: AgentSight 0.2.x writes `audit_type` + `action`, not
        // `kind`. Reporting Active with all-zero counters was the failure mode.
        let tmp = TempDir::new().unwrap();
        let store = RunStore::open(tmp.path()).unwrap();
        let run = store.create(&["true".into()], &Config::default()).unwrap();

        {
            let conn = Connection::open(run.observability_db_path()).unwrap();
            conn.execute_batch(
                "CREATE TABLE process_nodes (
                    id TEXT, pid INTEGER, comm TEXT, argv_json TEXT, view_source TEXT
                 );
                 INSERT INTO process_nodes VALUES ('p1', 1, 'sh', '[]', 'view');
                 INSERT INTO process_nodes VALUES ('p2', 2, 'ls', '[]', 'view');
                 CREATE TABLE audit_events (
                    id TEXT, timestamp_ms INTEGER, audit_type TEXT,
                    pid INTEGER, comm TEXT, subject TEXT, action TEXT,
                    target TEXT, status TEXT, summary TEXT, details_json TEXT
                 );
                 INSERT INTO audit_events VALUES
                   ('a1', 1, 'process', 1, 'sh', 'sh', 'exec', '/bin/sh', 'observed', 'exec', '{}'),
                   ('a2', 2, 'file', 1, 'sh', 'sh', 'write', '/tmp/x', 'observed', 'file', '{}'),
                   ('a3', 3, 'file', 1, 'sh', 'sh', 'write', '/tmp/y', 'observed', 'file', '{}'),
                   ('a4', 4, 'process', 2, 'ls', 'ls', 'exit', '', 'success', 'exit', '{}');
                 CREATE TABLE network_targets (
                    id TEXT, host TEXT, count INTEGER, error_count INTEGER
                 );
                 INSERT INTO network_targets VALUES ('n1', 'example.com', 1, 0);",
            )
            .unwrap();
        }

        let ev = Observations::collect(&run).unwrap();
        assert_eq!(ev.summary.processes, 2, "process_nodes rows");
        assert!(
            ev.summary.files_written >= 2,
            "file writes from action=write, got {}",
            ev.summary.files_written
        );
        assert_eq!(ev.summary.endpoints, 1, "network_targets rows");
        assert!(has_observational_signal(&ev.summary));
    }

    #[test]
    fn has_observational_signal_is_false_for_empty_summary() {
        assert!(!has_observational_signal(&RunSummary::default()));
        let s = RunSummary {
            processes: 1,
            ..Default::default()
        };
        assert!(has_observational_signal(&s));
    }

    #[test]
    fn sqlite_full_schema_aggregates() {
        let tmp = TempDir::new().unwrap();
        let store = RunStore::open(tmp.path()).unwrap();
        let run = store.create(&["true".into()], &Config::default()).unwrap();

        {
            let conn = Connection::open(run.observability_db_path()).unwrap();
            conn.execute_batch(
                "CREATE TABLE llm_calls (
                    id INTEGER, tokens_in INTEGER, tokens_out INTEGER
                 );
                 INSERT INTO llm_calls VALUES (1, 100, 50);
                 INSERT INTO llm_calls VALUES (2, 20, 10);
                 CREATE TABLE process_nodes (
                    id INTEGER, peak_rss_bytes INTEGER, cpu_seconds REAL
                 );
                 INSERT INTO process_nodes VALUES (1, 4096, 1.5);
                 INSERT INTO process_nodes VALUES (2, 8192, 0.5);
                 CREATE TABLE audit_events (id INTEGER, kind TEXT);
                 INSERT INTO audit_events VALUES (1, 'file_write');
                 INSERT INTO audit_events VALUES (2, 'connect');
                 INSERT INTO audit_events VALUES (3, 'process_exec');",
            )
            .unwrap();
        }

        let ev = Observations::collect(&run).unwrap();
        assert_eq!(ev.summary.llm_calls, 2);
        assert_eq!(ev.summary.tokens_in, 120);
        assert_eq!(ev.summary.tokens_out, 60);
        assert_eq!(ev.summary.processes, 2); // process_nodes wins
        assert_eq!(ev.summary.peak_rss_bytes, 8192);
        assert!((ev.summary.cpu_seconds - 2.0).abs() < 1e-9);
        assert!(ev.summary.files_written >= 1);
        assert!(ev.summary.endpoints >= 1);
    }

    #[test]
    fn events_jsonl_timeline() {
        let tmp = TempDir::new().unwrap();
        let store = RunStore::open(tmp.path()).unwrap();
        let run = store.create(&["true".into()], &Config::default()).unwrap();

        let mut f = fs::File::create(run.events_path()).unwrap();
        writeln!(
            f,
            r#"{{"ts":"2026-08-04T15:00:00Z","kind":"process","summary":"spawned bash"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"ts":"2026-08-04T15:00:01Z","kind":"llm","summary":"chat completion"}}"#
        )
        .unwrap();
        writeln!(f, "not-json").unwrap();

        let ev = Observations::collect(&run).unwrap();
        assert_eq!(ev.timeline.len(), 2);
        assert_eq!(ev.summary.processes, 1);
        assert_eq!(ev.summary.llm_calls, 1);
        assert_eq!(ev.timeline[0].kind, "process");
    }
}
