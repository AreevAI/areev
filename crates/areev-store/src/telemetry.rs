//! Telemetry sidecar — `<file>.telemetry.db`.
//!
//! Disposable, rebuildable, **never-syncing** evidence about recall behavior:
//! which grains get retrieved, which queries return nothing, how often. It
//! feeds the telemetry-fed Areev Loop analyzers (cold grains, coverage gaps,
//! budget pressure) — the signals that turn memory *hygiene* into memory
//! *utility*.
//!
//! Design constraints (proposal §8):
//! - A **separate Turso db file**, encrypted under the SAME key as the main
//!   file (crypto-erasure covers it; a plaintext sidecar holding query text +
//!   top-hits would outlive erased grains).
//! - Recall-path writes are **buffered and non-blocking**: the hot path only
//!   pushes a compact event into an in-memory ring; every SQLite write happens
//!   off-path (flush on write ops / close / explicit). Nothing lands inside
//!   the benched recall (~136µs) or 50ms voice-cadence budgets.
//! - **Host-only** mode (`off | aggregate | full`), never persisted in the
//!   main file — telemetry is host config, not a file-truth.
//! - It **never syncs** (the hub carries the memory file only) and is
//!   rebuildable, so losing it costs evidence detail, never state.

use crate::db::{with_txn, Db, TursoDb};
use crate::{now_ms, pi, pt};
use areev_core::{Hash, Result};
use std::collections::VecDeque;

/// How much recall telemetry to retain. Host-only; never a file-truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TelemetryMode {
    /// No sidecar, no writes — behaves exactly as pre-telemetry Areev. The
    /// library default (a bare `open()` never enables telemetry); agent-facing
    /// hosts opt into `Aggregate`.
    #[default]
    Off,
    /// Rollups only (grain-access counts, query stats, budget counters).
    /// Bounded and cheap.
    Aggregate,
    /// Aggregate + a per-recall ring log (query text + top hits) for the
    /// console Sessions view. Higher volume; encrypted at rest.
    Full,
}

impl TelemetryMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            TelemetryMode::Off => "off",
            TelemetryMode::Aggregate => "aggregate",
            TelemetryMode::Full => "full",
        }
    }
    /// Parse a host-supplied mode string; `None` on an unknown value.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "" => Some(TelemetryMode::Off),
            "aggregate" | "agg" | "on" => Some(TelemetryMode::Aggregate),
            "full" => Some(TelemetryMode::Full),
            _ => None,
        }
    }
    fn records_log(&self) -> bool {
        matches!(self, TelemetryMode::Full)
    }
}

/// One recall, captured on the hot path and buffered in memory. Kept minimal:
/// the only heap work on the recall path is these few short-string clones plus
/// one `Vec<Hash>` (Hash is `Copy`, so no per-hash allocation).
pub struct RecallEvent {
    pub ts_ms: i64,
    /// Host-scoped trajectory identifier. Deliberately excluded from
    /// `query_key`: rollups aggregate an intent across runs.
    pub run_id: Option<String>,
    pub ns: String,
    pub subject: Option<String>,
    pub relation: Option<String>,
    pub query: Option<String>,
    pub n_results: usize,
    pub latency_us: i64,
    pub hashes: Vec<Hash>,
}

impl RecallEvent {
    /// Stable per-intent key: same recall intent → same key, so rollups
    /// accumulate. Free-text query is lower-cased/trimmed; structural recalls
    /// key on subject+relation.
    fn query_key(&self) -> String {
        format!(
            "{}\u{1}{}\u{1}{}\u{1}{}",
            self.ns,
            self.subject.as_deref().unwrap_or(""),
            self.relation.as_deref().unwrap_or(""),
            self.query.as_deref().unwrap_or("").trim().to_lowercase(),
        )
    }
    /// A short human-readable sample of the recall intent (for the console /
    /// coverage-gap surfacing).
    fn sample(&self) -> String {
        self.query
            .as_deref()
            .or(self.subject.as_deref())
            .unwrap_or("")
            .to_string()
    }
    /// Whether this recall counts as a free-text "question" (the coverage-gap
    /// signal). Structural subject-only recalls feed grain-access, not queries.
    fn is_query(&self) -> bool {
        self.query.as_deref().map(|q| !q.trim().is_empty()).unwrap_or(false)
    }
}

/// Per-grain recall rollup (feeds `cold_grains`).
#[derive(Debug, Clone)]
pub struct AccessStat {
    pub hash: String,
    pub ns: String,
    pub recall_count: i64,
    pub last_ms: i64,
}

/// Per-query rollup (feeds `coverage_gap` / dead-query surfacing).
#[derive(Debug, Clone)]
pub struct QueryStat {
    pub key: String,
    pub ns: String,
    pub sample: String,
    pub run_count: i64,
    pub last_ms: i64,
    pub empty_count: i64,
    pub sum_results: i64,
}

/// One row from full-mode recall telemetry. Aggregate mode intentionally
/// returns none because it does not retain per-recall trajectory detail.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallLogEntry {
    pub ts_ms: i64,
    pub run_id: Option<String>,
    pub ns: String,
    pub subject: Option<String>,
    pub query: Option<String>,
    pub n_results: i64,
    pub latency_us: i64,
    pub top_hashes: Vec<String>,
}

/// Assembly-budget rollup (feeds `budget_pressure`).
#[derive(Debug, Clone, Default)]
pub struct BudgetStat {
    pub sample_count: i64,
    pub overflow_count: i64,
}

const TELEM_SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS telem_meta(k TEXT PRIMARY KEY, v TEXT)",
    "CREATE TABLE IF NOT EXISTS telem_recall_log(
        id INTEGER PRIMARY KEY,
        ts_ms INTEGER, run_id TEXT, ns TEXT, subject TEXT, query TEXT,
        n_results INTEGER, latency_us INTEGER, top_hashes TEXT)",
    "CREATE INDEX IF NOT EXISTS idx_telem_recall_ts ON telem_recall_log(ts_ms)",
    "CREATE TABLE IF NOT EXISTS telem_grain_access(
        hash TEXT PRIMARY KEY, ns TEXT,
        recall_count INTEGER, first_ms INTEGER, last_ms INTEGER)",
    "CREATE TABLE IF NOT EXISTS telem_query_stat(
        qkey TEXT PRIMARY KEY, ns TEXT, sample TEXT,
        run_count INTEGER, last_ms INTEGER, empty_count INTEGER, sum_results INTEGER)",
    "CREATE TABLE IF NOT EXISTS telem_budget_stat(
        id INTEGER PRIMARY KEY, sample_count INTEGER, overflow_count INTEGER)",
];

/// In-memory buffer cap (drop-oldest). Telemetry is disposable — a pure-recall
/// loop that never flushes bounds memory here rather than stalling recall.
const RING_CAP: usize = 8192;
/// Recall-log retention (90 days), pruned opportunistically on flush.
const LOG_RETAIN_MS: i64 = 90 * 24 * 3600 * 1000;
/// Recall-log row cap (~64 MiB proxy), newest-kept, pruned on flush.
const LOG_ROW_CAP: i64 = 200_000;

/// Postgres telemetry DDL: same tables (`telem_` prefix keeps them from
/// colliding with the store's own tables — on this backend they live in the
/// MEMORY's schema, so `DROP SCHEMA` erasure and `pg_dump -n` cover them),
/// with IDENTITY standing in for the rowid auto-assign and every integer
/// widened to bigint per the coercion contract.
#[cfg(feature = "postgres")]
const TELEM_SCHEMA_PG: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS telem_meta(k text PRIMARY KEY, v text)",
    "CREATE TABLE IF NOT EXISTS telem_recall_log(
        id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
        ts_ms bigint, run_id text, ns text, subject text, query text,
        n_results bigint, latency_us bigint, top_hashes text)",
    "CREATE INDEX IF NOT EXISTS idx_telem_recall_ts ON telem_recall_log(ts_ms)",
    "CREATE TABLE IF NOT EXISTS telem_grain_access(
        hash text PRIMARY KEY, ns text,
        recall_count bigint, first_ms bigint, last_ms bigint)",
    "CREATE TABLE IF NOT EXISTS telem_query_stat(
        qkey text PRIMARY KEY, ns text, sample text,
        run_count bigint, last_ms bigint, empty_count bigint, sum_results bigint)",
    "CREATE TABLE IF NOT EXISTS telem_budget_stat(
        id bigint PRIMARY KEY, sample_count bigint, overflow_count bigint)",
];

/// The telemetry sidecar: its own backend (own engine/connection — fully
/// decoupled from the main store's), an in-memory recall buffer, and the
/// recorded mode. Every write here is off the recall path.
pub struct Telemetry {
    db: Box<dyn Db>,
    mode: TelemetryMode,
    buf: VecDeque<RecallEvent>,
    prune_countdown: u32,
}

impl Telemetry {
    /// Open (or create) `<path>.telemetry.db`. `key` reuses the main file's
    /// AEAD key so crypto-erasure covers the sidecar too. Never called for
    /// `TelemetryMode::Off`.
    pub fn open(path: &str, key: Option<&[u8; 32]>, mode: TelemetryMode) -> Result<Self> {
        let sidecar = format!("{}.telemetry.db", path);
        let db: Box<dyn Db> = Box::new(TursoDb::open(&sidecar, key)?);
        for sql in TELEM_SCHEMA {
            db.execute(sql, vec![])?;
        }
        // Pre-rename sidecars carry the old unprefixed tables. Drop them —
        // telemetry is disposable, but an ORPHANED copy would silently
        // escape `scrub`, letting recall evidence outlive a forgotten
        // grain. (File sidecar only: on the postgres backend the memory's
        // schema owns tables with these names.)
        for legacy in ["meta", "recall_log", "grain_access", "query_stat", "budget_stat"] {
            let _ = db.execute(&format!("DROP TABLE IF EXISTS {legacy}"), vec![]);
        }
        Self::finish(db, mode)
    }

    /// Open the telemetry tables inside the memory's own Postgres schema —
    /// its own connection, so flushes never contend with the store's. The
    /// tables ride the schema, so erasure/export cover them like everything
    /// else in the memory.
    #[cfg(feature = "postgres")]
    pub fn open_pg(url: &str, schema: &str, mode: TelemetryMode) -> Result<Self> {
        let db: Box<dyn Db> = Box::new(crate::pg::PgDb::open(url, schema, TELEM_SCHEMA_PG)?);
        Self::finish(db, mode)
    }

    fn finish(db: Box<dyn Db>, mode: TelemetryMode) -> Result<Self> {
        // `CREATE TABLE IF NOT EXISTS` does not evolve an existing disposable
        // sidecar. The ALTER succeeds for v1 and is intentionally ignored for
        // a fresh/v2 sidecar where the column already exists.
        let _ = db.execute("ALTER TABLE telem_recall_log ADD COLUMN run_id TEXT", vec![]);
        db.execute(
            "INSERT OR REPLACE INTO telem_meta(k, v) VALUES ('schema_version', '2')",
            vec![],
        )?;
        Ok(Telemetry {
            db,
            mode,
            buf: VecDeque::new(),
            prune_countdown: 0,
        })
    }

    pub fn mode(&self) -> TelemetryMode {
        self.mode
    }

    /// Hot-path capture: push a recall event into the in-memory ring. **No
    /// I/O** — this is all that runs inside the recall latency budget. Oldest
    /// events are dropped past the cap so recall never stalls on flush.
    #[inline]
    pub fn record(&mut self, ev: RecallEvent) {
        self.buf.push_back(ev);
        while self.buf.len() > RING_CAP {
            self.buf.pop_front();
        }
    }

    /// Off-path drain: persist buffered events into the rollups (and the ring
    /// log in `full` mode) in one transaction. Called from write ops / close /
    /// explicit flush — never from the recall path.
    pub fn flush(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let events: Vec<RecallEvent> = self.buf.drain(..).collect();
        let records_log = self.mode.records_log();
        let prune = {
            self.prune_countdown = self.prune_countdown.saturating_sub(1);
            self.prune_countdown == 0
        };
        if prune {
            self.prune_countdown = 32; // prune the ring roughly every 32 flushes
        }
        let db = self.db.as_ref();
        let flush_body = |db: &dyn Db| -> Result<()> {
            for ev in &events {
                let qkey = ev.query_key();
                // grain-access rollup (all recalls)
                for h in &ev.hashes {
                    let hex = h.to_hex();
                    db.execute(
                        "INSERT OR REPLACE INTO telem_grain_access(hash, ns, recall_count, first_ms, last_ms)
                         VALUES (?1, ?2,
                           COALESCE((SELECT recall_count FROM telem_grain_access WHERE hash=?1),0)+1,
                           COALESCE((SELECT first_ms FROM telem_grain_access WHERE hash=?1),?3),
                           ?3)",
                        vec![pt(&hex), pt(&ev.ns), pi(ev.ts_ms)],
                    )?;
                }
                // query rollup (free-text "questions" only)
                if ev.is_query() {
                    let empty = if ev.n_results == 0 { 1 } else { 0 };
                    db.execute(
                        "INSERT OR REPLACE INTO telem_query_stat(qkey, ns, sample, run_count, last_ms, empty_count, sum_results)
                         VALUES (?1, ?2, ?3,
                           COALESCE((SELECT run_count FROM telem_query_stat WHERE qkey=?1),0)+1,
                           ?4,
                           COALESCE((SELECT empty_count FROM telem_query_stat WHERE qkey=?1),0)+?5,
                           COALESCE((SELECT sum_results FROM telem_query_stat WHERE qkey=?1),0)+?6)",
                        vec![
                            pt(&qkey),
                            pt(&ev.ns),
                            pt(&ev.sample()),
                            pi(ev.ts_ms),
                            pi(empty),
                            pi(ev.n_results as i64),
                        ],
                    )?;
                }
                // per-recall ring log (full mode only)
                if records_log {
                    let hashes: Vec<String> = ev.hashes.iter().map(|h| h.to_hex()).collect();
                    let top = serde_json::to_string(&hashes).unwrap_or_else(|_| "[]".into());
                    db.execute(
                        "INSERT INTO telem_recall_log(ts_ms, run_id, ns, subject, query, n_results, latency_us, top_hashes)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        vec![
                            pi(ev.ts_ms),
                            pt(ev.run_id.as_deref().unwrap_or("")),
                            pt(&ev.ns),
                            pt(ev.subject.as_deref().unwrap_or("")),
                            pt(ev.query.as_deref().unwrap_or("")),
                            pi(ev.n_results as i64),
                            pi(ev.latency_us),
                            pt(&top),
                        ],
                    )?;
                }
            }
            if records_log && prune {
                let cutoff = now_ms() - LOG_RETAIN_MS;
                db.execute("DELETE FROM telem_recall_log WHERE ts_ms < ?1", vec![pi(cutoff)])?;
                db.execute(
                    "DELETE FROM telem_recall_log WHERE id NOT IN
                     (SELECT id FROM telem_recall_log ORDER BY id DESC LIMIT ?1)",
                    vec![pi(LOG_ROW_CAP)],
                )?;
            }
            Ok(())
        };
        if db.prefers_batched_reads() {
            // Shared multi-writer sidecar (postgres): every rollup upsert is
            // atomic on its own, and skipping the wrapping transaction means
            // two instances' flushes can't deadlock on row locks. A partial
            // flush loses only disposable evidence.
            flush_body(db)
        } else {
            with_txn(db, || flush_body(db))
        }
    }

    /// Record one assembly-budget sample (whether it overflowed). Off the
    /// recall path — called from the ASSEMBLE/context budget check.
    pub fn note_budget(&mut self, overflow: bool) -> Result<()> {
        let ov = if overflow { 1 } else { 0 };
        self.db.execute(
            "INSERT OR REPLACE INTO telem_budget_stat(id, sample_count, overflow_count)
             VALUES (1,
               COALESCE((SELECT sample_count FROM telem_budget_stat WHERE id=1),0)+1,
               COALESCE((SELECT overflow_count FROM telem_budget_stat WHERE id=1),0)+?1)",
            vec![pi(ov)],
        )?;
        Ok(())
    }

    /// FORGET hook: synchronously scrub telemetry that references a forgotten
    /// grain, so the sidecar never outlives an erased grain. Drops the
    /// grain-access row, any ring-log rows that surfaced it, and any buffered
    /// events that named it.
    pub fn scrub(&mut self, hash: &Hash) -> Result<()> {
        let hex = hash.to_hex();
        self.buf.retain(|ev| !ev.hashes.iter().any(|h| h == hash));
        let like = format!("%{}%", hex);
        self.db
            .execute("DELETE FROM telem_grain_access WHERE hash=?1", vec![pt(&hex)])?;
        self.db
            .execute("DELETE FROM telem_recall_log WHERE top_hashes LIKE ?1", vec![pt(&like)])?;
        Ok(())
    }

    /// Identity-erasure hook: remove every sidecar row whose KEY or TEXT
    /// embeds the subject string — query rollups key on `ns␁subject␁…` and
    /// the ring log stores subject and query text verbatim, so per-hash
    /// scrubbing alone would let the identifier outlive its grains.
    /// Over-deletion (a query that merely CONTAINS the string) is the safe
    /// direction: telemetry is disposable evidence.
    pub fn scrub_subject(&mut self, subject: &str) -> Result<()> {
        self.buf.retain(|ev| {
            ev.subject.as_deref() != Some(subject)
                && !ev.query.as_deref().is_some_and(|q| q.contains(subject))
        });
        let like = format!("%{}%", crate::like_escape(subject));
        self.db.execute(
            "DELETE FROM telem_recall_log WHERE subject = ?1 OR query LIKE ?2 ESCAPE '\\'",
            vec![pt(subject), pt(&like)],
        )?;
        self.db.execute(
            "DELETE FROM telem_query_stat WHERE qkey LIKE ?1 ESCAPE '\\' OR sample LIKE ?1 ESCAPE '\\'",
            vec![pt(&like)],
        )?;
        Ok(())
    }

    // ---- readers (consumed by the telemetry-fed analyzers + console) ----

    /// Grain-access rollups, optionally scoped to a namespace.
    pub fn access_stats(&self, ns: Option<&str>) -> Result<Vec<AccessStat>> {
        let rows = match ns {
            Some(n) => self.db.query(
                "SELECT hash, ns, recall_count, last_ms FROM telem_grain_access WHERE ns=?1",
                vec![pt(n)],
            )?,
            None => self
                .db
                .query("SELECT hash, ns, recall_count, last_ms FROM telem_grain_access", vec![])?,
        };
        Ok(rows
            .iter()
            .map(|row| AccessStat {
                hash: row.text(0).unwrap_or_default().to_string(),
                ns: row.text(1).unwrap_or_default().to_string(),
                recall_count: row.i64(2).unwrap_or(0),
                last_ms: row.i64(3).unwrap_or(0),
            })
            .collect())
    }

    /// Query rollups, optionally scoped to a namespace.
    pub fn query_stats(&self, ns: Option<&str>) -> Result<Vec<QueryStat>> {
        let sql = "SELECT qkey, ns, sample, run_count, last_ms, empty_count, sum_results FROM telem_query_stat";
        let rows = match ns {
            Some(n) => self.db.query(&format!("{sql} WHERE ns=?1"), vec![pt(n)])?,
            None => self.db.query(sql, vec![])?,
        };
        Ok(rows
            .iter()
            .map(|row| QueryStat {
                key: row.text(0).unwrap_or_default().to_string(),
                ns: row.text(1).unwrap_or_default().to_string(),
                sample: row.text(2).unwrap_or_default().to_string(),
                run_count: row.i64(3).unwrap_or(0),
                last_ms: row.i64(4).unwrap_or(0),
                empty_count: row.i64(5).unwrap_or(0),
                sum_results: row.i64(6).unwrap_or(0),
            })
            .collect())
    }

    /// Full-mode recall rows, newest first, optionally scoped to one run.
    pub fn recall_log(&self, run_id: Option<&str>) -> Result<Vec<RecallLogEntry>> {
        let rows = match run_id {
            Some(run) => self.db.query(
                "SELECT ts_ms, run_id, ns, subject, query, n_results, latency_us, top_hashes \
                 FROM telem_recall_log WHERE run_id=?1 ORDER BY id DESC",
                vec![pt(run)],
            )?,
            None => self.db.query(
                "SELECT ts_ms, run_id, ns, subject, query, n_results, latency_us, top_hashes \
                 FROM telem_recall_log ORDER BY id DESC",
                vec![],
            )?,
        };
        Ok(rows
            .iter()
            .map(|row| RecallLogEntry {
                ts_ms: row.i64(0).unwrap_or(0),
                run_id: row
                    .text(1)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                ns: row.text(2).unwrap_or_default().to_string(),
                subject: row
                    .text(3)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                query: row
                    .text(4)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                n_results: row.i64(5).unwrap_or(0),
                latency_us: row.i64(6).unwrap_or(0),
                top_hashes: row
                    .text(7)
                    .and_then(|json| serde_json::from_str(json).ok())
                    .unwrap_or_default(),
            })
            .collect())
    }

    /// The assembly-budget rollup.
    pub fn budget_stats(&self) -> Result<BudgetStat> {
        let rows = self
            .db
            .query("SELECT sample_count, overflow_count FROM telem_budget_stat WHERE id=1", vec![])?;
        Ok(match rows.first() {
            Some(row) => BudgetStat {
                sample_count: row.i64(0).unwrap_or(0),
                overflow_count: row.i64(1).unwrap_or(0),
            },
            None => BudgetStat::default(),
        })
    }
}
