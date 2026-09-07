//! Backend seam: the minimal sync SQL surface `Areev` drives.
//!
//! The store logic — index maintenance, fork election, BM25 scoring — lives in
//! `lib.rs` and is backend-agnostic; only the transport behind this trait
//! differs. `TursoDb` is the embedded engine (async turso behind a private
//! current-thread runtime, exactly the sync-over-async shape `Areev` always
//! had). A Postgres backend implements the same trait so both backends run the
//! identical store logic, which is what keeps fork/head/oplog semantics in
//! parity by construction.

use std::cell::RefCell;
use std::collections::HashMap;

use areev_core::error::{AreevError, Result};
use turso::Value;

pub(crate) fn db_err<E: std::fmt::Display>(e: E) -> AreevError {
    AreevError::Storage(e.to_string())
}

/// One materialized result row; columns are positional per the SELECT list.
/// Accessors mirror the strictness of the old `v_i64`/`v_blob`/`v_f64`
/// helpers: `i64`/`blob` accept only their exact variant, `f64` widens from
/// Integer — a coercion contract both backends must satisfy.
pub(crate) struct Row(pub(crate) Vec<Value>);

impl Row {
    pub(crate) fn i64(&self, i: usize) -> Option<i64> {
        match self.0.get(i) {
            Some(Value::Integer(x)) => Some(*x),
            _ => None,
        }
    }
    pub(crate) fn f64(&self, i: usize) -> Option<f64> {
        match self.0.get(i) {
            Some(Value::Real(r)) => Some(*r),
            Some(Value::Integer(x)) => Some(*x as f64),
            _ => None,
        }
    }
    pub(crate) fn blob(&self, i: usize) -> Option<Vec<u8>> {
        match self.0.get(i) {
            Some(Value::Blob(b)) => Some(b.clone()),
            _ => None,
        }
    }
    pub(crate) fn text(&self, i: usize) -> Option<&str> {
        match self.0.get(i) {
            Some(Value::Text(t)) => Some(t.as_str()),
            _ => None,
        }
    }
}

/// The storage transport. Sync by contract — each backend hides its own
/// executor, so point reads never pay an executor hop at the call site.
///
/// The `_hot` variants cache the prepared statement keyed by the SQL literal;
/// call them only with fixed `&'static str` statements (the hot probes and the
/// per-grain write statements), never with `format!`-built SQL.
pub(crate) trait Db: Send {
    fn execute(&self, sql: &str, params: Vec<Value>) -> Result<u64>;
    fn query(&self, sql: &str, params: Vec<Value>) -> Result<Vec<Row>>;
    fn execute_hot(&self, sql: &'static str, params: Vec<Value>) -> Result<u64>;
    fn query_hot(&self, sql: &'static str, params: Vec<Value>) -> Result<Vec<Row>>;
    fn begin(&self) -> Result<()>;
    fn commit(&self) -> Result<()>;
    fn rollback(&self) -> Result<()>;
    /// Whether multi-id reads should batch into one statement (`IN`/`ANY`)
    /// rather than loop indexed point reads. In-process engines answer false
    /// — a cached point probe is ~µs and Turso does not optimize a
    /// parameterized IN on the PK into probes (measured: a table scan per
    /// call, ~8x on the voice frame path). Networked backends answer true —
    /// there, the round trip dominates and one batch beats k probes.
    fn prefers_batched_reads(&self) -> bool {
        false
    }

    /// Make the vector storage for `dim`-sized embeddings usable, or say why
    /// not. The embedded engine's `embeddings` table is dimension-less BLOB
    /// storage created with the schema, so the default is a no-op; the
    /// Postgres backend creates `vector(dim)` here (the dim is only known
    /// when the host installs an embedder) and hard-errors on a dim
    /// mismatch — a mismatched write would otherwise fail mid-transaction.
    fn ensure_embeddings(&self, _dim: usize) -> Result<()> {
        Ok(())
    }

    /// Build the backend's approximate-nearest-neighbour index over
    /// `embeddings.vec`, replacing the exact scan for *unfiltered* k-NN.
    ///
    /// Opt-in, and deliberately not part of the schema every open applies:
    /// an ANN index changes vector recall from exact to **approximate**, which
    /// is a semantics change no caller should get by upgrading. The default
    /// refuses rather than no-ops — a silent no-op would let a host believe a
    /// million-grain corpus was indexed while every query still scanned it.
    ///
    /// Note what this does NOT fix: a k-NN already narrowed by namespace or
    /// subject is served from `idx_grains_ns_s` as an exact search over a
    /// small candidate set, and is both faster and exact. ANN earns its
    /// approximation only on the whole-corpus query that has nothing to
    /// filter on.
    fn ensure_ann_index(&self, _m: usize, _ef_construction: usize, _ef_search: usize) -> Result<()> {
        Err(AreevError::AnnIndexUnsupported(
            "the embedded engine stores vectors as BLOBs and scans them exactly; \
             scope the query by namespace or subject instead, or use the postgres backend"
                .into(),
        ))
    }

    /// Remove the ANN index, returning vector recall to an exact scan.
    fn drop_ann_index(&self) -> Result<()> {
        Err(AreevError::AnnIndexUnsupported(
            "no ANN index exists on this backend".into(),
        ))
    }

    /// The ANN index's name if one is built, `None` if not. Never errors on a
    /// backend without ANN support — "is one present?" is answerable there
    /// (no), unlike "build one".
    fn ann_index_name(&self) -> Result<Option<String>> {
        Ok(None)
    }

    // ---- multi-writer arbitration hooks -------------------------------
    //
    // The embedded engine keeps its single-writer-per-file model: every
    // default below says "no opinion" and the store falls back to its
    // process-local counters and dictionary. A backend that admits several
    // concurrent writers (Postgres) overrides them to make allocation and
    // dictionary state DB-authoritative.

    /// Claim a contiguous id block for a write transaction: `n_seq` grain
    /// seqs, `n_op` op-log seqs, `n_hlc` HLC ticks (with `wall_hlc` the
    /// caller's wall clock already shifted into HLC space, and `hlc_floor`
    /// raising the clock without consuming — the import path). MUST be
    /// called INSIDE the write transaction: the returned block commits or
    /// rolls back with it, and the row lock it takes serializes concurrent
    /// write transactions per memory — which is also what keeps op-log
    /// order equal to commit order for followers.
    fn reserve_write(
        &self,
        _n_seq: i64,
        _n_op: i64,
        _n_hlc: i64,
        _wall_hlc: i64,
        _hlc_floor: i64,
    ) -> Result<Option<WriteIds>> {
        Ok(None)
    }

    /// Atomically get-or-create the dictionary id for `term`.
    fn intern_term(&self, _term: &str) -> Result<Option<i64>> {
        Ok(None)
    }

    /// Authoritative forward dictionary lookup, consulted on a process-local
    /// miss (another writer may have interned the term after this handle
    /// opened).
    fn lookup_term(&self, _term: &str) -> Result<Option<i64>> {
        Ok(None)
    }

    /// Authoritative reverse dictionary lookup on a process-local miss.
    fn lookup_term_str(&self, _id: i64) -> Result<Option<String>> {
        Ok(None)
    }

    /// Tombstone one dictionary string in place once nothing references it
    /// (`Areev::scrub_term_if_unreferenced`): the string becomes an
    /// unrecallable placeholder while the id stays valid. A backend that keys
    /// the dictionary by digest overrides this to re-key the row as well.
    /// Returns rows changed (0 or 1); the `NOT LIKE` guard skips rows already
    /// tombstoned so repeat erasures do not re-count them.
    fn scrub_term(&self, id: i64, placeholder: &str) -> Result<u64> {
        self.execute(
            "UPDATE terms SET term = ?2 WHERE id = ?1 \
             AND NOT EXISTS (SELECT 1 FROM triples WHERE s=?1 OR p=?1 OR o=?1) \
             AND NOT EXISTS (SELECT 1 FROM thread_idx WHERE session=?1) \
             AND NOT EXISTS (SELECT 1 FROM run_idx WHERE run=?1) \
             AND NOT EXISTS (SELECT 1 FROM grains WHERE ns=?1) \
             AND term NOT LIKE ?3",
            vec![crate::pi(id), crate::pt(placeholder), crate::pt("\u{1}erased:%")],
        )
    }

    /// Live BM25 collection stats (doc count, total token length) where the
    /// process-local counters can be stale under concurrent writers.
    fn collection_stats(&self) -> Result<Option<(i64, i64)>> {
        Ok(None)
    }

    /// Row-lock suffix for in-transaction read-then-write rechecks
    /// (`" FOR UPDATE"` on backends with concurrent writers, empty where a
    /// single writer holds the file).
    fn for_update(&self) -> &'static str {
        ""
    }
}

/// A reserved contiguous id block: the FIRST seq / op_seq / HLC of the
/// requested ranges.
pub(crate) struct WriteIds {
    pub(crate) seq0: i64,
    pub(crate) op0: i64,
    pub(crate) hlc0: i64,
}

/// Run `f` atomically: BEGIN, then COMMIT on Ok / best-effort ROLLBACK on Err.
pub(crate) fn with_txn<T>(db: &dyn Db, f: impl FnOnce() -> Result<T>) -> Result<T> {
    db.begin()?;
    match f() {
        Ok(v) => {
            db.commit()?;
            Ok(v)
        }
        Err(e) => {
            let _ = db.rollback();
            Err(e)
        }
    }
}

/// The embedded Turso backend: async engine behind a private current-thread
/// runtime. Owns the `Database` handle (kept alive for the connection) and a
/// statement cache that subsumes the old per-field `st_*` slots.
pub(crate) struct TursoDb {
    rt: tokio::runtime::Runtime,
    _db: turso::Database,
    conn: turso::Connection,
    /// Keyed by the SQL literal; only `_hot` calls populate it, so growth is
    /// bounded by the set of hot statement literals in this crate.
    cache: RefCell<HashMap<&'static str, turso::Statement>>,
}

impl TursoDb {
    /// Build the engine (optionally with the AES-256-GCM page cipher — the key
    /// must be supplied at build time so an encrypted header decrypts on
    /// reopen). Schema DDL is the caller's job: it is store logic, not
    /// transport.
    pub(crate) fn open(path: &str, enc_key: Option<&[u8; 32]>) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(db_err)?;
        let (db, conn) = rt.block_on(async {
            let mut b = turso::Builder::new_local(path).experimental_index_method(true);
            if let Some(k) = enc_key {
                // Wipe our hex rendering of the key after the builder copies
                // it; the engine necessarily retains its own copy while open.
                let hexkey = zeroize::Zeroizing::new(hex32(k));
                b = b.experimental_encryption(true).with_encryption(turso::EncryptionOpts {
                    cipher: "aes256gcm".to_string(),
                    hexkey: (*hexkey).clone(),
                });
            }
            let db = b.build().await.map_err(db_err)?;
            let conn = db.connect().map_err(db_err)?;
            Ok::<_, AreevError>((db, conn))
        })?;
        Ok(Self { rt, _db: db, conn, cache: RefCell::new(HashMap::new()) })
    }

    /// Clone the cached prepared statement for `sql`, preparing it on first
    /// use. The clone shares the underlying statement, and the borrow is
    /// dropped before any await, so re-entrant use in fetch loops is safe.
    fn hot(&self, sql: &'static str) -> Result<turso::Statement> {
        if let Some(st) = self.cache.borrow().get(sql) {
            return Ok(st.clone());
        }
        let st = self.rt.block_on(self.conn.prepare(sql)).map_err(db_err)?;
        self.cache.borrow_mut().insert(sql, st.clone());
        Ok(st)
    }
}

async fn drain(rows: &mut turso::Rows) -> Result<Vec<Row>> {
    let mut out = Vec::new();
    while let Some(r) = rows.next().await.map_err(db_err)? {
        let n = r.column_count();
        let mut vals = Vec::with_capacity(n);
        for i in 0..n {
            vals.push(r.get_value(i).map_err(db_err)?);
        }
        out.push(Row(vals));
    }
    Ok(out)
}

impl Db for TursoDb {
    fn execute(&self, sql: &str, params: Vec<Value>) -> Result<u64> {
        self.rt.block_on(self.conn.execute(sql, params)).map_err(db_err)
    }

    fn query(&self, sql: &str, params: Vec<Value>) -> Result<Vec<Row>> {
        self.rt.block_on(async {
            let mut rows = self.conn.query(sql, params).await.map_err(db_err)?;
            drain(&mut rows).await
        })
    }

    fn execute_hot(&self, sql: &'static str, params: Vec<Value>) -> Result<u64> {
        let mut st = self.hot(sql)?;
        self.rt.block_on(st.execute(params)).map_err(db_err)
    }

    fn query_hot(&self, sql: &'static str, params: Vec<Value>) -> Result<Vec<Row>> {
        let mut st = self.hot(sql)?;
        self.rt.block_on(async {
            let mut rows = st.query(params).await.map_err(db_err)?;
            drain(&mut rows).await
        })
    }

    fn begin(&self) -> Result<()> {
        self.execute("BEGIN", vec![]).map(|_| ())
    }

    fn commit(&self) -> Result<()> {
        self.execute("COMMIT", vec![]).map(|_| ())
    }

    fn rollback(&self) -> Result<()> {
        self.execute("ROLLBACK", vec![]).map(|_| ())
    }
}

/// Hex-encode a 32-byte key for the Turso page cipher.
fn hex32(k: &[u8; 32]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(64);
    for b in k {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> (TursoDb, tempfile::TempDir) {
        let d = tempfile::TempDir::new().unwrap();
        let db = TursoDb::open(d.path().join("t.db").to_str().unwrap(), None).unwrap();
        (db, d)
    }

    #[test]
    fn execute_query_roundtrip() {
        let (db, _d) = mem();
        db.execute("CREATE TABLE t(a INTEGER, b TEXT, c BLOB, d REAL)", vec![]).unwrap();
        db.execute(
            "INSERT INTO t(a,b,c,d) VALUES (?1,?2,?3,?4)",
            vec![
                Value::Integer(7),
                Value::Text("x".into()),
                Value::Blob(vec![1, 2]),
                Value::Real(1.5),
            ],
        )
        .unwrap();
        let rows = db.query("SELECT a, b, c, d FROM t", vec![]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].i64(0), Some(7));
        assert_eq!(rows[0].text(1), Some("x"));
        assert_eq!(rows[0].blob(2), Some(vec![1, 2]));
        assert_eq!(rows[0].f64(3), Some(1.5));
        // strictness contract
        assert_eq!(rows[0].i64(1), None);
        assert_eq!(rows[0].blob(0), None);
        assert_eq!(rows[0].f64(0), Some(7.0)); // f64 widens from Integer
        assert_eq!(rows[0].i64(9), None); // out of range is None, not a panic
    }

    #[test]
    fn hot_statements_are_cached_and_reentrant() {
        let (db, _d) = mem();
        db.execute("CREATE TABLE t(a INTEGER)", vec![]).unwrap();
        for i in 0..5 {
            db.execute_hot("INSERT INTO t(a) VALUES (?1)", vec![Value::Integer(i)]).unwrap();
        }
        // repeated use of the same cached probe inside a loop
        for i in 0..5 {
            let rows = db
                .query_hot("SELECT a FROM t WHERE a = ?1", vec![Value::Integer(i)])
                .unwrap();
            assert_eq!(rows[0].i64(0), Some(i));
        }
        assert_eq!(db.cache.borrow().len(), 2);
    }

    #[test]
    fn with_txn_commits_on_ok_and_rolls_back_on_err() {
        let (db, _d) = mem();
        db.execute("CREATE TABLE t(a INTEGER)", vec![]).unwrap();
        with_txn(&db, || {
            db.execute("INSERT INTO t(a) VALUES (1)", vec![])?;
            Ok(())
        })
        .unwrap();
        let r: Result<()> = with_txn(&db, || {
            db.execute("INSERT INTO t(a) VALUES (2)", vec![])?;
            Err(AreevError::Storage("boom".into()))
        });
        assert!(r.is_err());
        let rows = db.query("SELECT COUNT(*) FROM t", vec![]).unwrap();
        assert_eq!(rows[0].i64(0), Some(1), "rolled-back insert must not persist");
        // the connection is usable after the rollback
        db.execute("INSERT INTO t(a) VALUES (3)", vec![]).unwrap();
    }
}
