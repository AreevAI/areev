//! areev — Python bindings for Areev (LR-3, alpha).
//!
//! Thin and version-stable by design: scalar args in, JSON strings out
//! for anything structured (the Python wrapper layer can pretty this up;
//! the FFI stays honest). Install `areev`, `import areev`.

use areev_cal::{CalExecutor, CalExecutorConfig, CalStoreFacade, AreevFacade};
use areev_core::error::{Hash, AreevError};
use areev_store::memory_tool::MemoryTool;
use areev_store::{
    parse_relations, Axis, CommandEmbed, Areev as RustAreev, Direction, EmbedBackend, FactDraft,
    TelemetryMode,
};
use areev_loop_adapter::{now_ms, BorrowedSubstrate};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use serde_json::json;
use areev_loop::{Decision, Engine, ObserverType, RecStatus, RunOptions, ScopeSet};

/// Parse a duration like `6h` / `30m` / `2d` / `3600s` into milliseconds.
fn parse_duration_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let n: i64 = s[..split].parse().ok()?;
    let mult = match &s[split..] {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    Some(n * mult)
}

/// Parse a comma-separated scope list (`"review,apply"`) into a [`ScopeSet`].
///
/// `None` means all scopes, which is what the bindings always used — the
/// console is one principal and so was the binding. Hardcoding it meant the
/// separation-of-duties gate (write ≠ review ≠ apply) could not be demonstrated
/// or enforced from Python at all, even though the engine implements it.
fn parse_scopes(spec: Option<&str>) -> PyResult<ScopeSet> {
    let Some(spec) = spec else { return Ok(ScopeSet::all()) };
    let mut out = Vec::new();
    for name in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        out.push(match name {
            "read" => areev_loop::Scope::Read,
            "write" => areev_loop::Scope::Write,
            "review" => areev_loop::Scope::Review,
            "apply" => areev_loop::Scope::Apply,
            "admin" => areev_loop::Scope::Admin,
            other => {
                return Err(err(format!(
                    "unknown scope {other:?} — expected a comma-separated subset of: \
                     read, write, review, apply, admin"
                )))
            }
        });
    }
    Ok(ScopeSet::of(&out))
}

fn status_from_str(s: &str) -> Option<RecStatus> {
    match s {
        "pending" => Some(RecStatus::Pending),
        "approved" => Some(RecStatus::Approved),
        "rejected" => Some(RecStatus::Rejected),
        "applied" => Some(RecStatus::Applied),
        "rolled_back" => Some(RecStatus::RolledBack),
        "expired" => Some(RecStatus::Expired),
        _ => None,
    }
}

/// Open on the server-tier postgres backend from a
/// `postgres://…?schema=<name>` DSN, mirroring the file branches (explicit
/// `index_text` re-stamps; telemetry rides the memory's schema). A
/// passphrase never applies here — the page cipher and `.kdf` sidecar are
/// file-backend capabilities.
#[cfg(feature = "postgres")]
fn open_postgres_from_dsn(
    dsn: &str,
    tel: TelemetryMode,
    index_text: Option<bool>,
    has_passphrase: bool,
) -> areev_core::error::Result<RustAreev> {
    if has_passphrase {
        return Err(AreevError::Validation(
            "passphrase applies to file-backed memories (page cipher + .kdf sidecar); on the \
             postgres backend use TDE/pgcrypto at the deployment layer"
                .into(),
        ));
    }
    let (url, schema) = areev_store::pg::split_schema_url(dsn)?;
    match index_text {
        Some(want_text) => RustAreev::open_postgres_with(
            &url,
            &schema,
            areev_store::AreevOptions {
                index_text: want_text,
                telemetry: tel,
                ..areev_store::AreevOptions::default()
            },
        ),
        None if tel != TelemetryMode::Off => {
            RustAreev::open_postgres_with_telemetry(&url, &schema, tel)
        }
        None => RustAreev::open_postgres(&url, &schema),
    }
}

#[cfg(not(feature = "postgres"))]
fn open_postgres_from_dsn(
    _dsn: &str,
    _tel: TelemetryMode,
    _index_text: Option<bool>,
    _has_passphrase: bool,
) -> areev_core::error::Result<RustAreev> {
    Err(AreevError::Validation(
        "this build lacks the postgres backend (rebuild with the `postgres` feature)".into(),
    ))
}

fn err<E: std::fmt::Display>(e: E) -> PyErr {
    PyValueError::new_err(e.to_string())
}

/// Verb check for the binding methods that reach the store directly instead
/// of through a gated `cal_*` facade method. `principal=` is documented to
/// fail closed (CAL 1.3 §9), so a sandboxed handle must not be able to erase
/// — or export a subject's dossier — merely by calling the binding method
/// rather than the CAL statement that does the same thing.
fn check_verb(facade: &AreevFacade, verb: areev_core::authz::Verb, ns: &str) -> PyResult<()> {
    facade.authz().check(verb, ns).map_err(err)
}

/// Resolve an LLM backend the same two ways the CLI does: a subprocess
/// (`llm_cmd`, the zero-dependency escape hatch) or a built-in HTTP provider
/// (`model`, key read from the environment). The subprocess wins when both are
/// given. Both fail at construction, before anything is written.
fn resolve_llm(
    cmd: Option<String>,
    spec: Option<String>,
) -> PyResult<Option<Box<dyn areev_loop::LlmBackend>>> {
    if let Some(cmd) = cmd {
        return Ok(Some(Box::new(areev_loop::CommandLlm::new(&cmd, None).map_err(err)?)));
    }
    if let Some(spec) = spec {
        return Ok(Some(areev_llm::resolve(&spec, None, None).map_err(err)?));
    }
    Ok(None)
}

/// Run the shared extract → confidence floor → ground pipeline and convert the
/// survivors to store drafts. An extraction that fails names the Event
/// the raw text was already stored under, so the caller can retry against it
/// instead of losing the content.
fn extract_and_ground(
    llm: &dyn areev_loop::LlmBackend,
    grounder: Option<&dyn areev_loop::LlmBackend>,
    source: &Hash,
    content: &str,
    hint: Option<&str>,
    min_confidence: f64,
) -> PyResult<(usize, Vec<FactDraft>, Option<&'static str>)> {
    let hex = source.to_hex();
    let ex = areev_llm::extract_pipeline(llm, grounder, &hex, content, hint, min_confidence)
        .map_err(|e| err(format!("{e} (event {hex} was stored)")))?;
    let drafts = ex
        .facts
        .into_iter()
        .map(|f| FactDraft {
            subject: f.subject,
            relation: f.relation,
            object: f.object,
            confidence: f.confidence,
        })
        .collect();
    let status = if ex.grounded { "verified" } else { "unverified" };
    Ok((ex.proposed, drafts, Some(status)))
}

fn parse_hash(hex: &str) -> PyResult<Hash> {
    Hash::from_hex(hex).map_err(err)
}

/// [`EmbedBackend`] over a Python callable `embed(text: str) -> list[float]`.
/// Store calls run detached from the interpreter (see the note on
/// [`Areev`]), so the embedder has to re-attach to call back into Python.
/// That is the intended direction of travel, and re-attaching on a thread
/// that is already attached is safe too (`Python::attach` is reentrant).
struct PyEmbed {
    f: Py<PyAny>,
    dim: usize,
    model: String,
}

impl EmbedBackend for PyEmbed {
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, text: &str) -> areev_core::error::Result<Vec<f32>> {
        Python::attach(|py| {
            let out = self
                .f
                .call1(py, (text,))
                .map_err(|e| AreevError::Storage(format!("python embedder raised: {e}")))?;
            out.extract::<Vec<f32>>(py).map_err(|e| {
                AreevError::Validation(format!(
                    "python embedder must return a sequence of floats: {e}"
                ))
            })
        })
    }
    fn model(&self) -> &str {
        &self.model
    }
}

/// One memory = one file. Open with `areev.Areev("caller.db", ns="caller")`.
///
/// **One handle per file per process — share it across threads.** The embedded
/// backend is single-writer per file, and a second handle would keep its own
/// sequence and dictionary allocators; opening one raises `STO-E002`. Sharing
/// a single handle across threads is fully supported (see the detach note
/// below), so opening per request or per agent turn is never necessary.
///
/// Every method that reaches the store runs it inside `py.detach(...)`, so the
/// interpreter lock is released for the duration of the storage call. This is
/// not an optimization — it is what makes the type usable from more than one
/// thread. Two reasons:
///
/// 1. A slow call (a bulk `migrate`, a `verify`, a write on a file whose text
///    index has gone linear) would otherwise stall *every* Python thread in the
///    process. Agent hosts push writes onto a background thread precisely so a
///    slow write cannot block the next turn; that only works if the binding
///    actually yields.
/// 2. `with_store` takes a mutex. Waiting on it while holding the GIL is worse
///    than slow — the thread that holds the store cannot make progress on
///    anything that needs the interpreter, so the two locks stack up. Cheap
///    methods therefore detach as well: it is the *waiting* that has to happen
///    outside the GIL, not just the work.
#[pyclass]
struct Areev {
    facade: std::sync::Arc<AreevFacade>,
    ns: String,
    /// Host-asserted actor label stamped on every loop audit grain (§6.6).
    actor: String,
}

#[pymethods]
impl Areev {
    #[new]
    #[allow(clippy::too_many_arguments)] // a flat FFI surface; each knob is a distinct scalar
    #[pyo3(signature = (path, ns = "shared".to_string(), passphrase = None, actor = "user:local".to_string(), telemetry = "aggregate".to_string(), index_text = None, principal = None))]
    fn new(
        py: Python<'_>,
        path: String,
        ns: String,
        passphrase: Option<String>,
        actor: String,
        telemetry: String,
        index_text: Option<bool>,
        principal: Option<String>,
    ) -> PyResult<Self> {
        // Recall-telemetry sidecar (host capability, §8): agents are the main
        // telemetry producers, so the binding default is `aggregate`; pass
        // telemetry="off" to disable. It is never a file-truth.
        let tel = TelemetryMode::parse(&telemetry)
            .ok_or_else(|| err(format!("unknown telemetry mode '{telemetry}' (off|aggregate|full)")))?;
        // Encryption at rest: a passphrase derives an AES-256 key (Argon2id;
        // non-secret salt in a <path>.kdf sidecar). Same key rules as the
        // CLI's --passphrase-env: host-supplied, never stored in the file.
        // Opening builds the schema, creates indexes and loads the dictionary —
        // a bounded but real amount of I/O, and the one call a host is most
        // likely to make while other threads are already running.
        //
        // `index_text` follows the CLI's `--index-text`: left unset, the file's
        // own declaration wins (the file-truth path). Passed explicitly it is a
        // deliberate re-stamp, and the change is reported via `open_warnings()`.
        // Turning it off is how a host trades the BM25 leg for flat write
        // latency — with the index on, every write pays a cost that grows with
        // the file (tursodatabase/turso#8170).
        let store = py
            .detach(|| {
                // A postgres://…?schema=<name> DSN selects the server-tier
                // backend — same API, the memory lives in a Postgres schema.
                if path.starts_with("postgres://") || path.starts_with("postgresql://") {
                    return open_postgres_from_dsn(&path, tel, index_text, passphrase.is_some());
                }
                match (index_text, passphrase) {
                    (None, Some(p)) => RustAreev::open_with_passphrase_telemetry(&path, &p, tel),
                    (None, None) => RustAreev::open_with_telemetry(&path, tel),
                    (Some(want_text), pass) => {
                        let key = match pass {
                            Some(p) => Some(*RustAreev::derive_key_for(&path, &p)?),
                            None => None,
                        };
                        RustAreev::open_with(
                            &path,
                            areev_store::AreevOptions {
                                index_text: want_text,
                                encryption_key: key,
                                telemetry: tel,
                                ..areev_store::AreevOptions::default()
                            },
                        )
                    }
                }
            })
            .map_err(err)?;
        let facade = AreevFacade::with_session(store, Some(ns.clone()), None);
        // `principal=` binds the session to the file's grants for that
        // principal (fail closed — CAL 1.3 §9). Absent, the handle is the
        // owner, as ever. The loop actor follows the bound principal
        // unless overridden explicitly.
        let (facade, actor) = match principal {
            Some(p) => {
                let f = facade.with_principal(&p).map_err(err)?;
                let actor = if actor == "user:local" { p } else { actor };
                (f, actor)
            }
            None => (facade, actor),
        };
        Ok(Areev { facade: std::sync::Arc::new(facade), ns, actor })
    }

    /// Reconciliation warnings from open (file-vs-host declaration changes,
    /// embedding-model mismatches). JSON list string.
    fn open_warnings(&self, py: Python<'_>) -> PyResult<String> {
        let w = py.detach(|| self.facade.with_store(|m| m.open_warnings().to_vec()));
        serde_json::to_string(&w).map_err(err)
    }

    /// Set the host-scoped run identifier copied into subsequent recall
    /// telemetry. Pass `None` to clear it; it is never a file truth.
    #[pyo3(signature = (run_id = None))]
    fn set_run_id(&self, py: Python<'_>, run_id: Option<String>) {
        py.detach(|| self.facade.with_store(|m| m.set_run_id(run_id.as_deref())));
    }

    /// Install an embedding callback: `embed(text: str) -> list[float]`.
    /// Probed once here to learn the dimension (recorded as the file's
    /// embedding provenance). Enables the vector recall leg; grains added
    /// afterwards are embedded — run `reindex_text()`-style backfills via
    /// `migrate`/re-adds, embeddings are not retro-computed.
    #[pyo3(signature = (embed, model = None))]
    fn set_embedder(&self, py: Python<'_>, embed: Py<PyAny>, model: Option<String>) -> PyResult<()> {
        // The probe calls back into Python, so it stays attached; only the
        // install below reaches the store.
        let dim = embed
            .call1(py, ("dimension probe",))
            .and_then(|out| out.extract::<Vec<f32>>(py))
            .map_err(err)?
            .len();
        if dim == 0 {
            return Err(err("embedder returned an empty vector"));
        }
        let backend = PyEmbed {
            f: embed,
            dim,
            model: model.unwrap_or_else(|| "python".to_string()),
        };
        py.detach(|| self.facade.with_store(|m| m.set_embedder(Box::new(backend))));
        Ok(())
    }

    /// Install a command embedder (same contract as the CLI's --embed-cmd):
    /// the command gets the text on stdin and prints a JSON array of numbers.
    #[pyo3(signature = (cmd, model = None))]
    fn set_embedder_command(&self, py: Python<'_>, cmd: String, model: Option<String>) -> PyResult<()> {
        py.detach(|| -> Result<(), AreevError> {
            let ce = CommandEmbed::new(&cmd, model.as_deref())?;
            self.facade.with_store(|m| m.set_embedder(Box::new(ce)));
            Ok(())
        })
        .map_err(err)
    }

    /// Backfill + rebuild the BM25 text index (e.g. after bulk loads, or on
    /// a file that flipped text indexing on later). Returns rows backfilled.
    fn reindex_text(&self, py: Python<'_>) -> PyResult<usize> {
        py.detach(|| self.facade.with_store(|m| m.rebuild_text_index()))
            .map_err(err)
    }

    /// Rebuild the link indexes: reverse provenance, run correlation, and
    /// `related_to` cross-links. Returns index rows written.
    ///
    /// `open()` heals a file that predates these indexes, so this is for
    /// rebuilding on demand — the counterpart of `reindex_text()`, and what
    /// `areev reindex` runs.
    fn reindex_links(&self, py: Python<'_>) -> PyResult<usize> {
        py.detach(|| self.facade.with_store(|m| m.rebuild_link_indexes()))
            .map_err(err)
    }

    /// Import another memory system's export. `source`: mem0 | mem0-history |
    /// langgraph | letta | letta-archival | zep | jsonl. `payload` is the
    /// export file's contents; `history` the optional mem0 history payload.
    /// (basic-memory vault directories import via the CLI: `areev migrate`.)
    /// Returns the report as JSON: {added, superseded, forgotten, skipped,
    /// notes}. Re-runs skip what is already imported.
    #[pyo3(signature = (source, payload, history = None, ns = None))]
    fn migrate(
        &self,
        py: Python<'_>,
        source: String,
        payload: String,
        history: Option<String>,
        ns: Option<String>,
    ) -> PyResult<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let rep = py
            .detach(|| {
                self.facade.with_store(|m| {
                    areev_store::migrate::migrate_payload(
                        m,
                        &ns,
                        &source,
                        &payload,
                        history.as_deref(),
                    )
                })
            })
            .map_err(err)?;
        Ok(rep.to_json().to_string())
    }

    /// Add a Fact. Returns the content address (64-hex).
    ///
    /// With `idempotent=True`, if the current head for `(subject, relation)`
    /// already holds this exact object, no grain is written and the existing
    /// head's hash is returned (value-level dedup, not just byte-identical).
    #[allow(clippy::too_many_arguments)] // a flat FFI surface; each knob is a distinct scalar
    #[pyo3(signature = (subject, relation, object, confidence = 0.9, ns = None, idempotent = false))]
    fn add_fact(
        &self,
        py: Python<'_>,
        subject: String,
        relation: String,
        object: String,
        confidence: f64,
        ns: Option<String>,
        idempotent: bool,
    ) -> PyResult<String> {
        let mut fields = serde_json::Map::new();
        fields.insert("subject".into(), json!(subject));
        fields.insert("relation".into(), json!(relation));
        fields.insert("object".into(), json!(object));
        fields.insert("confidence".into(), json!(confidence));
        fields.insert("namespace".into(), json!(ns.unwrap_or_else(|| self.ns.clone())));
        py.detach(|| -> Result<String, AreevError> {
            if idempotent {
                Ok(self.facade.cal_add_if_novel("fact", &fields)?.0.to_hex())
            } else {
                Ok(self.facade.cal_add("fact", &fields)?.to_hex())
            }
        })
        .map_err(err)
    }

    /// Add any grain type from a JSON fields object. Returns the hash.
    #[pyo3(signature = (grain_type, fields_json, ns = None))]
    fn add(
        &self,
        py: Python<'_>,
        grain_type: String,
        fields_json: String,
        ns: Option<String>,
    ) -> PyResult<String> {
        let mut fields: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&fields_json).map_err(err)?;
        fields
            .entry("namespace".to_string())
            .or_insert_with(|| json!(ns.unwrap_or_else(|| self.ns.clone())));
        py.detach(|| self.facade.cal_add(&grain_type, &fields).map(|h| h.to_hex()))
            .map_err(err)
    }

    /// Add many grains in one store transaction. `grains_json` is a JSON list
    /// of `{"grain_type": "fact", "fields": {...}}` objects (`"type"` is
    /// accepted for `"grain_type"`); each `fields` object is validated exactly
    /// as `add()` validates its own, so one malformed entry fails the call and
    /// writes nothing. Returns a JSON list of hashes, in input order.
    ///
    /// Worth ~1.6x over the same grains through `add()` one at a time, and
    /// only when the BM25 text index is off — with it on, per-row index cost
    /// swamps any batching (see `index_text` on the constructor). For loading
    /// another system's export, prefer `migrate()`, which already defers and
    /// rebuilds the index around the load.
    #[pyo3(signature = (grains_json, ns = None))]
    fn add_batch(&self, py: Python<'_>, grains_json: String, ns: Option<String>) -> PyResult<String> {
        let items: Vec<serde_json::Value> = serde_json::from_str(&grains_json).map_err(err)?;
        let default_ns = ns.unwrap_or_else(|| self.ns.clone());
        let mut entries = Vec::with_capacity(items.len());
        for (i, item) in items.iter().enumerate() {
            let grain_type = item
                .get("grain_type")
                .or_else(|| item.get("type"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| err(format!("grain {i}: missing 'grain_type'")))?
                .to_string();
            let mut fields = item
                .get("fields")
                .and_then(|v| v.as_object())
                .cloned()
                .ok_or_else(|| err(format!("grain {i}: missing 'fields' object")))?;
            fields
                .entry("namespace".to_string())
                .or_insert_with(|| json!(default_ns));
            entries.push((grain_type, fields));
        }
        let hashes = py
            .detach(|| self.facade.cal_add_batch(&entries))
            .map_err(err)?;
        let out: Vec<String> = hashes.iter().map(|h| h.to_hex()).collect();
        serde_json::to_string(&out).map_err(err)
    }

    /// Free-text recall over the BM25 (and vector, when an embedder is
    /// installed) legs, fused with the structural leg when `subject` or
    /// `relation` is given — the same path as the CLI's `areev search` and as
    /// CAL's `RECALL ... ABOUT "..."`. Returns a JSON list string shaped like
    /// `recall()`.
    ///
    /// This is what a per-turn agent host wants for "what do I know that
    /// relates to what the user just said": `recall()` needs a subject you
    /// already know, and getting here otherwise meant hand-writing CAL.
    #[pyo3(signature = (query, subject = None, relation = None, k = 10, ns = None))]
    fn search(
        &self,
        py: Python<'_>,
        query: String,
        subject: Option<String>,
        relation: Option<String>,
        k: usize,
        ns: Option<String>,
    ) -> PyResult<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        // With neither leg available there is nothing to rank on, and
        // `recall_hybrid` answers with an empty list — which reads as "you
        // have no matching memories" when the truth is "this file cannot
        // answer free-text queries at all". Say which.
        let (has_text, has_vec) = py.detach(|| {
            self.facade
                .with_store(|m| (m.index_text_enabled(), m.embedder_dim().is_some()))
        });
        if !has_text && !has_vec {
            // Reopening with index_text=True is only half the remedy: the
            // re-stamp turns indexing on for *future* writes, so grains written
            // while it was off stay invisible and `search()` returns [] with no
            // error at all. Going from a loud, actionable error to a silent
            // empty list reads as "there is nothing stored", not "the index
            // needs rebuilding" — so name the second step here, where the user
            // is actually looking.
            return Err(err(
                "search() needs a text or vector leg: this file has the BM25 index off \
                 (index_text=False) and no embedder installed — reopen with \
                 index_text=True and then call reindex_text() to index the grains \
                 written while it was off, or call \
                 set_embedder()/set_embedder_command()",
            ));
        }
        let grains = py
            .detach(|| {
                self.facade.with_store(|m| {
                    m.recall_hybrid(
                        &ns,
                        subject.as_deref(),
                        relation.as_deref(),
                        Some(&query),
                        k,
                        None,
                    )
                })
            })
            .map_err(err)?;
        let out: Vec<serde_json::Value> = grains
            .iter()
            .map(|g| {
                json!({
                    "hash": g.hash.to_hex(),
                    "type": format!("{:?}", g.grain_type).to_lowercase(),
                    "fields": g.fields,
                })
            })
            .collect();
        serde_json::to_string(&out).map_err(err)
    }

    /// Structural recall, newest-first. Returns a JSON list string.
    #[pyo3(signature = (subject, relation = None, k = 16, ns = None))]
    fn recall(
        &self,
        py: Python<'_>,
        subject: String,
        relation: Option<String>,
        k: usize,
        ns: Option<String>,
    ) -> PyResult<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let grains = py
            .detach(|| {
                self.facade
                    .with_store(|m| m.recall(&ns, &subject, relation.as_deref(), k))
            })
            .map_err(err)?;
        let out: Vec<serde_json::Value> = grains
            .iter()
            .map(|g| {
                json!({
                    "hash": g.hash.to_hex(),
                    "type": format!("{:?}", g.grain_type).to_lowercase(),
                    "fields": g.fields,
                })
            })
            .collect();
        serde_json::to_string(&out).map_err(err)
    }

    /// Current head for (subject, relation) — JSON string or None.
    #[pyo3(signature = (subject, relation, ns = None))]
    fn latest(
        &self,
        py: Python<'_>,
        subject: String,
        relation: String,
        ns: Option<String>,
    ) -> PyResult<Option<String>> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let head = py
            .detach(|| self.facade.with_store(|m| m.latest(&ns, &subject, &relation)))
            .map_err(err)?;
        Ok(head.map(|g| {
            json!({
                "hash": g.hash.to_hex(),
                "fields": g.fields,
            })
            .to_string()
        }))
    }

    /// Supersede old_hash with a new version (append-only evolution).
    #[pyo3(signature = (old_hash, grain_type, fields_json, ns = None))]
    fn supersede(
        &self,
        py: Python<'_>,
        old_hash: String,
        grain_type: String,
        fields_json: String,
        ns: Option<String>,
    ) -> PyResult<String> {
        let old = parse_hash(&old_hash)?;
        let mut fields: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&fields_json).map_err(err)?;
        fields
            .entry("namespace".to_string())
            .or_insert_with(|| json!(ns.unwrap_or_else(|| self.ns.clone())));
        py.detach(|| {
            self.facade
                .cal_supersede(&old, &grain_type, &fields)
                .map(|h| h.to_hex())
        })
        .map_err(err)
    }

    /// Erase a grain from the hot store (tombstoned). Host-level op.
    /// Routed through the facade so it carries the same `delete` check and
    /// Tier-2 audit record as CAL's `FORGET <hash>`.
    fn forget(&self, py: Python<'_>, hash: String) -> PyResult<()> {
        let h = parse_hash(&hash)?;
        py.detach(|| self.facade.cal_delete(&h, None)).map_err(err)
    }

    /// Right-to-erasure for one identity: erase every grain holding a
    /// STRUCTURED reference to `subject` in the session namespace (or `ns`) —
    /// the full supersession history, grains referencing it in object
    /// position, its thread events, its dictionary entry, and erased-only
    /// vocabulary — with replicating tombstones. Returns the erasure report
    /// as JSON (counts only, no identity material). Host-level destructive
    /// op: gate it like your other compliance endpoints. See docs/erasure.md
    /// for the scope contract (only structured references are findable).
    /// Identity matching always covers partition-style keys (`pat`,
    /// `pat#visit1`, `pat:thread-2` — never `patricia`); pass
    /// `text_mentions=True` to ALSO erase grains whose indexed text mentions
    /// the identity's tokens (search symmetry — opt-in because token
    /// matching over-reaches for common-word identifiers).
    #[pyo3(signature = (subject, ns = None, text_mentions = false))]
    fn forget_subject(
        &self,
        py: Python<'_>,
        subject: String,
        ns: Option<String>,
        text_mentions: bool,
    ) -> PyResult<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        check_verb(&self.facade, areev_core::authz::Verb::Erase, &ns)?;
        let opts = areev_store::ErasureOptions { text_mentions };
        let rep = py
            .detach(|| self.facade.with_store(|m| m.forget_subject_with(&ns, &subject, opts)))
            .map_err(err)?;
        Ok(json!({
            "grains_erased": rep.grains_erased,
            "terms_removed": rep.terms_removed,
            "vocab_removed": rep.vocab_removed,
            "blobs_reclaimed": rep.blobs_reclaimed,
        })
        .to_string())
    }

    /// DSAR read (GDPR Art. 15/20): everything `forget_subject` WOULD erase
    /// for one identity — exact + partition keys, full history — as
    /// `{"identity_names": [...], "grains": [{hash, type, fields}, ...]}`.
    /// The same selector as erasure, in show-me mode.
    #[pyo3(signature = (subject, ns = None, text_mentions = false))]
    fn subject_report(
        &self,
        py: Python<'_>,
        subject: String,
        ns: Option<String>,
        text_mentions: bool,
    ) -> PyResult<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        // The DSAR read is `read`-gated, exactly as `REPORT SUBJECT` is.
        check_verb(&self.facade, areev_core::authz::Verb::Read, &ns)?;
        let opts = areev_store::ErasureOptions { text_mentions };
        let rep = py
            .detach(|| self.facade.with_store(|m| m.subject_report_with(&ns, &subject, opts)))
            .map_err(err)?;
        let grains: Vec<serde_json::Value> = rep
            .grains
            .iter()
            .map(|g| {
                json!({
                    "hash": g.hash.to_hex(),
                    "type": g.grain_type.as_str(),
                    "fields": g.fields.clone().into_iter().collect::<serde_json::Map<_, _>>(),
                })
            })
            .collect();
        Ok(json!({ "identity_names": rep.identity_names, "grains": grains }).to_string())
    }

    /// Export the subject selection as a portable MGB1 bundle (GDPR Art. 20
    /// portability) — importable into any OMS store. Returns the bundle
    /// stats JSON.
    #[pyo3(signature = (path, subject, ns = None, text_mentions = false))]
    fn subject_bundle(
        &self,
        py: Python<'_>,
        path: String,
        subject: String,
        ns: Option<String>,
        text_mentions: bool,
    ) -> PyResult<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        check_verb(&self.facade, areev_core::authz::Verb::Read, &ns)?;
        let opts = areev_store::ErasureOptions { text_mentions };
        let stats = py
            .detach(|| {
                self.facade.with_store(|m| m.subject_bundle_with(&ns, &subject, opts, &path))
            })
            .map_err(err)?;
        Ok(json!({
            "ops": stats.ops,
            "bytes": stats.bytes,
            "last_op_seq": stats.last_op_seq,
        })
        .to_string())
    }

    /// Retention sweep: erase every grain with `created_at` older than
    /// `cutoff_ms` (epoch milliseconds), optionally limited to one grain
    /// type (e.g. "event") and scoped to the session namespace (or `ns`;
    /// pass ns="" to sweep every namespace). Same erasure semantics as
    /// `forget_subject` minus the identity sweep. Returns the report JSON.
    #[pyo3(signature = (cutoff_ms, ns = None, grain_type = None))]
    fn forget_older_than(
        &self,
        py: Python<'_>,
        cutoff_ms: i64,
        ns: Option<String>,
        grain_type: Option<String>,
    ) -> PyResult<String> {
        let gt = match &grain_type {
            Some(s) => Some(
                areev_core::types::GrainType::from_str(s)
                    .ok_or_else(|| err(format!("unknown grain type '{s}'")))?,
            ),
            None => None,
        };
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let ns_opt = if ns.is_empty() { None } else { Some(ns) };
        // ns="" sweeps every namespace, so it takes `erase` on all of them.
        check_verb(
            &self.facade,
            areev_core::authz::Verb::Erase,
            ns_opt.as_deref().unwrap_or("*"),
        )?;
        let rep = py
            .detach(|| {
                self.facade
                    .with_store(|m| m.forget_older_than(ns_opt.as_deref(), cutoff_ms, gt))
            })
            .map_err(err)?;
        Ok(json!({
            "grains_erased": rep.grains_erased,
            "terms_removed": rep.terms_removed,
            "vocab_removed": rep.vocab_removed,
            "blobs_reclaimed": rep.blobs_reclaimed,
        })
        .to_string())
    }

    /// remember(): store content as an Event, then attach the facts
    /// distilled from it. Three routes to those facts, in precedence order:
    /// `facts_json` (pre-extracted by the host — a JSON list of
    /// {subject, relation, object, confidence}), `llm_cmd` (a subprocess
    /// backend), or `model` ("openai:gpt-4o-mini", key from the env).
    ///
    /// The raw text is written before the model is called, so a failed
    /// extraction never costs the raw text — the error names the hash it was
    /// stored under. Model-extracted facts are stamped
    /// `verification_status="unverified"` unless `ground_model`/`ground_cmd`
    /// runs a separate entailment pass (proposer ≠ scorer); facts it does not
    /// support are dropped and survivors are stamped `"verified"`.
    ///
    /// The raw text is stored as an **Event** grain (a transcript turn) —
    /// pass `session_id`/`role` to place it in a conversation thread, and
    /// `run_id` to place it in a run that `run_trace()` can read back.
    ///
    /// Returns {"event", "facts"} JSON, plus {"model", "proposed",
    /// "dropped", "verification_status"} when a model ran.
    #[allow(clippy::too_many_arguments)] // a flat FFI surface; each knob is a distinct scalar
    #[pyo3(signature = (content, facts_json = None, observer = "python".to_string(), ns = None,
                        model = None, llm_cmd = None, ground_model = None, ground_cmd = None,
                        extract_hint = None, min_confidence = None, session_id = None,
                        role = None, run_id = None))]
    fn remember(
        &self,
        py: Python<'_>,
        content: String,
        facts_json: Option<String>,
        observer: String,
        ns: Option<String>,
        model: Option<String>,
        llm_cmd: Option<String>,
        ground_model: Option<String>,
        ground_cmd: Option<String>,
        extract_hint: Option<String>,
        min_confidence: Option<f64>,
        session_id: Option<String>,
        role: Option<String>,
        run_id: Option<String>,
    ) -> PyResult<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        // Held across a network round trip when a model is attached — the
        // interpreter lock must not be.
        py.detach(|| {
            let explicit = match facts_json {
                Some(j) => Some(FactDraft::from_json_array(&j).map_err(err)?),
                None => None,
            };
            let llm = match explicit {
                Some(_) => None,
                None => resolve_llm(llm_cmd, model)?,
            };
            let grounder = match llm {
                Some(_) => resolve_llm(ground_cmd, ground_model)?,
                None => None,
            };
            let meta = areev_store::Capture {
                observer: Some(observer.as_str()),
                session_id: session_id.as_deref(),
                role: role.as_deref(),
                run_id: run_id.as_deref(),
            };
            let event = self
                .facade
                .with_store(|m| m.capture(&ns, &content, &meta))
                .map_err(err)?;

            let (proposed, drafts, status) = match &llm {
                None => {
                    let d = explicit.unwrap_or_default();
                    (d.len(), d, None)
                }
                Some(l) => extract_and_ground(
                    l.as_ref(),
                    grounder.as_deref(),
                    &event,
                    &content,
                    extract_hint.as_deref(),
                    min_confidence.unwrap_or(0.0),
                )
                .map_err(err)?,
            };
            let attribution = areev_store::FactAttribution {
                verification_status: status,
                extractor_model: llm.as_ref().map(|l| l.model()),
            };
            let facts = self
                .facade
                .with_store(|m| m.attach_facts(&ns, &event, &drafts, &attribution))
                .map_err(err)?;

            let mut out = json!({
                "event": event.to_hex(),
                "facts": facts.iter().map(|h| h.to_hex()).collect::<Vec<_>>(),
            });
            if let (Some(obj), Some(l)) = (out.as_object_mut(), &llm) {
                obj.insert("model".into(), json!(l.model()));
                obj.insert("verification_status".into(), json!(status));
                obj.insert("proposed".into(), json!(proposed));
                obj.insert("dropped".into(), json!(proposed.saturating_sub(facts.len())));
            }
            Ok(out.to_string())
        })
    }

    /// Execute CAL. Returns the wire-format payload as a JSON string.
    /// Non-fatal `CAL-Wnnn` warnings ride along under a `warnings` key
    /// (absent when the query raised none).
    fn cal(&self, py: Python<'_>, query: String) -> PyResult<String> {
        let res = py
            .detach(|| {
                let ex = CalExecutor::new(CalExecutorConfig::default());
                ex.execute(&query, &*self.facade)
            })
            .map_err(err)?;
        serde_json::to_string(&res.payload_json().map_err(err)?).map_err(err)
    }

    /// Anthropic memory-tool command (LR-13): pass the tool-call dict as
    /// JSON; returns the tool result text. Wire this as your MemoryToolHandler.
    #[pyo3(signature = (command_json, ns = None))]
    fn memory_tool(
        &self,
        py: Python<'_>,
        command_json: String,
        ns: Option<String>,
    ) -> PyResult<String> {
        let cmd: serde_json::Value = serde_json::from_str(&command_json).map_err(err)?;
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        py.detach(|| {
            self.facade.with_store(|m| {
                let mut t = MemoryTool::new(m, &ns);
                t.execute(&cmd)
            })
        })
        .map_err(err)
    }

    /// Reverse provenance: grains distilled from `source_hash` (its
    /// `derived_from`), newest first, as a JSON list string. The
    /// credit-assignment / episode-unlearn query.
    fn provenance(&self, py: Python<'_>, source_hash: String) -> PyResult<String> {
        let h = source_hash.strip_prefix("sha256:").unwrap_or(&source_hash);
        let parent = parse_hash(h)?;
        let kids = py
            .detach(|| self.facade.with_store(|m| m.grains_derived_from(&parent)))
            .map_err(err)?;
        let out: Vec<serde_json::Value> = kids
            .iter()
            .map(|g| {
                json!({
                    "hash": g.hash.to_hex(),
                    "type": format!("{:?}", g.grain_type).to_lowercase(),
                    "subject": g.get_str("subject"),
                    "relation": g.get_str("relation"),
                    "object": g.get_str("object"),
                })
            })
            .collect();
        serde_json::to_string(&out).map_err(err)
    }

    /// Advise-mode novelty check: nearest existing grains to `text`, optionally
    /// scoped to (subject, relation), as a JSON list of {hash, similarity,
    /// object}, most similar first. Requires an installed embedder. Never
    /// writes — the caller decides supersede-vs-add.
    #[pyo3(signature = (text, subject = None, relation = None, k = 5, ns = None))]
    fn nearest(
        &self,
        py: Python<'_>,
        text: String,
        subject: Option<String>,
        relation: Option<String>,
        k: usize,
        ns: Option<String>,
    ) -> PyResult<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        // Detaching matters especially here: a Python embedder re-attaches from
        // inside the store call, and it cannot do that if we never let go.
        let matches = py
            .detach(|| {
                self.facade.with_store(|m| {
                    m.nearest_semantic(&ns, subject.as_deref(), relation.as_deref(), &text, k)
                })
            })
            // The remedy has to name the API the caller is holding. This error
            // used to arrive from the store naming `--embed-cmd`, a CLI flag
            // that does not exist in Python — and `pip install areev` ships no
            // `areev` binary, so the advice was unactionable as written.
            .map_err(|e| match e {
                AreevError::Validation(msg) if msg.contains("requires an installed embedder") => {
                    err(AreevError::Validation(
                        "nearest() requires an embedder; install one with set_embedder(fn) \
                         or set_embedder_command(cmd)"
                            .to_string(),
                    ))
                }
                other => err(other),
            })?;
        let out: Vec<serde_json::Value> = matches
            .iter()
            .map(|(h, sim)| json!({"hash": h.to_hex(), "similarity": sim}))
            .collect();
        serde_json::to_string(&out).map_err(err)
    }

    /// Supersession-chain history for (subject, relation), newest first.
    #[pyo3(signature = (subject, relation, ns = None))]
    fn history(
        &self,
        py: Python<'_>,
        subject: String,
        relation: String,
        ns: Option<String>,
    ) -> PyResult<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let versions = py
            .detach(|| self.facade.with_store(|m| m.history(&ns, &subject, &relation)))
            .map_err(err)?;
        let out: Vec<serde_json::Value> = versions
            .iter()
            .map(|v| {
                json!({
                    "hash": v.hash.to_hex(), "object": v.object,
                    "created_at": v.created_at, "confidence": v.confidence,
                    "superseded_by": v.superseded_by.map(|h| h.to_hex()),
                })
            })
            .collect();
        serde_json::to_string(&out).map_err(err)
    }

    /// Store statistics as JSON.
    fn stats(&self, py: Python<'_>) -> PyResult<String> {
        let s = py
            .detach(|| self.facade.with_store(|m| m.stats()))
            .map_err(err)?;
        Ok(json!({
            "grains": s.grains, "current": s.current, "triples": s.triples,
            "terms": s.terms, "ops": s.ops, "events_indexed": s.events_indexed,
        })
        .to_string())
    }

    /// Bounded k-hop walk over the entity graph.
    ///
    /// `relations` is comma-separated. `direction` is out|in|both — in/both use
    /// the reverse index, which only covers relations the file declares
    /// entity-valued, so they find nothing for relations outside that set.
    #[allow(clippy::too_many_arguments)] // a flat FFI surface; each knob is a distinct scalar
    #[pyo3(signature = (start, relations, direction = "out".to_string(), depth = 2, limit = 64, ns = None))]
    fn related(
        &self,
        py: Python<'_>,
        start: String,
        relations: String,
        direction: String,
        depth: usize,
        limit: usize,
        ns: Option<String>,
    ) -> PyResult<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let rels = parse_relations(&relations);
        if rels.is_empty() {
            return Err(PyValueError::new_err(
                "relations must name at least one relation",
            ));
        }
        let dir = Direction::parse(&direction)
            .ok_or_else(|| PyValueError::new_err("direction must be one of: out, in, both"))?;
        let reached = py
            .detach(|| {
                let refs: Vec<&str> = rels.iter().map(String::as_str).collect();
                self.facade
                    .with_store(|m| m.related(&ns, &start, &refs, dir, depth, limit))
            })
            .map_err(err)?;
        Ok(json!({"start": start, "reached": reached}).to_string())
    }

    /// As-of read on two axes: `world` = what was true at `at`,
    /// `knowledge` = what the agent knew at `at`. `at` is epoch milliseconds.
    #[pyo3(signature = (subject, relation, at, axis = "world".to_string(), ns = None))]
    fn entity_at(
        &self,
        py: Python<'_>,
        subject: String,
        relation: String,
        at: i64,
        axis: String,
        ns: Option<String>,
    ) -> PyResult<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let ax = Axis::parse(&axis)
            .ok_or_else(|| PyValueError::new_err("axis must be one of: world, knowledge"))?;
        let found = py
            .detach(|| {
                self.facade
                    .with_store(|m| m.entity_at(&ns, &subject, &relation, at, ax))
            })
            .map_err(err)?;
        Ok(match found {
            Some(g) => json!({"found": true, "grain": g}).to_string(),
            None => json!({"found": false}).to_string(),
        })
    }

    /// Execution records for a workflow: which grains ran which of its nodes.
    ///
    /// A Workflow grain is immutable, so runs point at the plan rather than
    /// mutating it — retries show up as several records for one node.
    #[pyo3(signature = (workflow, node = None, limit = 64, ns = None))]
    fn step_actions(
        &self,
        py: Python<'_>,
        workflow: String,
        node: Option<String>,
        limit: usize,
        ns: Option<String>,
    ) -> PyResult<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let wf = Hash::from_hex(&workflow).map_err(err)?;
        let rows = py
            .detach(|| {
                self.facade
                    .with_store(|m| m.step_actions(&ns, &wf, node.as_deref(), limit))
            })
            .map_err(err)?;
        let steps: Vec<serde_json::Value> = rows
            .into_iter()
            .map(|(n, h)| json!({"node": n, "hash": h.to_hex()}))
            .collect();
        Ok(json!({"workflow": wf.to_hex(), "steps": steps}).to_string())
    }

    /// What a run recorded, and what it produced downstream.
    ///
    /// Returns `{run_id, trace, produced}` — `trace` is the run's own grains,
    /// `produced` is what was derived from them and is not itself part of the
    /// run (extracted facts, distilled lessons). This is the query that crosses
    /// from execution history into semantic memory.
    #[pyo3(signature = (run_id, limit = 64, include_yield = true, ns = None))]
    fn run_trace(
        &self,
        py: Python<'_>,
        run_id: String,
        limit: usize,
        include_yield: bool,
        ns: Option<String>,
    ) -> PyResult<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let (trace, produced) = py
            .detach(|| {
                self.facade.with_store(|m| {
                    let t = m.run_trace(&ns, &run_id, limit)?;
                    let p = if include_yield {
                        m.run_yield(&ns, &run_id, limit)?
                    } else {
                        Vec::new()
                    };
                    Ok::<_, AreevError>((t, p))
                })
            })
            .map_err(err)?;
        Ok(json!({"run_id": run_id, "trace": trace, "produced": produced}).to_string())
    }

    /// One page of a run's grains, **oldest first**, from an exclusive
    /// `after_seq` cursor — the complete journal read. `run_trace` clamps at
    /// 1024 with no cursor; past ~340 supersteps a runtime resuming from it
    /// would silently truncate its own journal. Returns
    /// `{run_id, entries: [{seq, grain}...], next_after_seq}` —
    /// `next_after_seq` is null when the run is exhausted; otherwise pass it
    /// back as the next `after_seq`.
    #[pyo3(signature = (run_id, after_seq = 0, limit = 500, ns = None))]
    fn run_grains(
        &self,
        py: Python<'_>,
        run_id: String,
        after_seq: i64,
        limit: usize,
        ns: Option<String>,
    ) -> PyResult<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let page = py
            .detach(|| self.facade.with_store(|m| m.run_grains(&ns, &run_id, after_seq, limit)))
            .map_err(err)?;
        let exhausted = page.len() < limit.clamp(1, 1024);
        let next = (!exhausted).then(|| page.last().map(|(s, _)| *s)).flatten();
        let entries: Vec<serde_json::Value> = page
            .into_iter()
            .map(|(seq, g)| json!({"seq": seq, "grain": g}))
            .collect();
        Ok(json!({"run_id": run_id, "entries": entries, "next_after_seq": next}).to_string())
    }

    /// Vector-in nearest-neighbour search: cosine top-k against a
    /// **caller-supplied query vector** — no embedder needed. The read half of
    /// the external-embedding seam for hosts that own their embedding
    /// pipeline (a framework adapter must search with *its* model's vectors;
    /// re-embedding with a different local model would compare two vector
    /// spaces and return silently wrong similarity). Dimension is checked
    /// against the file's declared embedding provenance. Returns the same
    /// `[{hash, similarity}]` shape as `nearest()`.
    #[pyo3(signature = (vector, subject = None, relation = None, k = 5, ns = None))]
    fn nearest_vector(
        &self,
        py: Python<'_>,
        vector: Vec<f32>,
        subject: Option<String>,
        relation: Option<String>,
        k: usize,
        ns: Option<String>,
    ) -> PyResult<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let matches = py
            .detach(|| {
                self.facade.with_store(|m| {
                    m.nearest_vector(&ns, subject.as_deref(), relation.as_deref(), &vector, k)
                })
            })
            .map_err(err)?;
        let out: Vec<serde_json::Value> = matches
            .iter()
            .map(|(h, sim)| json!({"hash": h.to_hex(), "similarity": sim}))
            .collect();
        serde_json::to_string(&out).map_err(err)
    }

    /// Install or replace the indexed embedding for a stored grain — the
    /// write half of the external-embedding seam (and the backfill primitive).
    /// The vector never enters the grain blob (embeddings are index-layer
    /// data, not content), so the grain's hash is unchanged. The first
    /// external vector on an undeclared file stamps
    /// `embedding_model = "external"`. Honest boundary: bundles carry blobs,
    /// not index rows — replicas must have vectors re-supplied.
    fn add_embedding(&self, py: Python<'_>, hash: String, vector: Vec<f32>) -> PyResult<String> {
        let h = Hash::from_hex(&hash).map_err(err)?;
        py.detach(|| self.facade.with_store(|m| m.set_grain_embedding(&h, &vector)))
            .map_err(err)?;
        Ok(hash)
    }

    /// The file's declared embedding provenance as `{model, dim}`, or null
    /// when the file has never seen a vector. Adapters check this before
    /// supplying vectors so a dimension mismatch is caught at setup, not as a
    /// per-write error.
    fn declared_embedding(&self, py: Python<'_>) -> PyResult<String> {
        let d = py.detach(|| {
            self.facade
                .with_store(|m| m.declared_embedding().map(|(m2, d)| (m2.to_string(), d)))
        });
        Ok(match d {
            Some((model, dim)) => json!({"model": model, "dim": dim}).to_string(),
            None => "null".to_string(),
        })
    }

    // ---- the `areev run` runtime (Wave 5 parity) ---------------------------
    // Same FFI convention as everything above: scalars in, JSON strings out.
    // Host tools execute through `tool_cmd` (the CLI's --tool-cmd subprocess
    // seam: input JSON on stdin, result JSON on stdout); without one, host
    // tools fail with a clear message rather than being faked.

    /// Start a governed run of the Workflow at `workflow` (64-hex). Returns
    /// the session JSON: `{"finished": …}` or `{"parked": envelope}`.
    #[pyo3(signature = (workflow, run_id, input_json = None, tool_cmd = None,
                        max_tokens = None, max_usd_micros = None, max_wall_ms = None,
                        ask_ttl_sec = None))]
    #[allow(clippy::too_many_arguments)]
    fn run_start(
        &self,
        py: Python<'_>,
        workflow: String,
        run_id: String,
        input_json: Option<String>,
        tool_cmd: Option<String>,
        max_tokens: Option<u64>,
        max_usd_micros: Option<u64>,
        max_wall_ms: Option<u64>,
        ask_ttl_sec: Option<i64>,
    ) -> PyResult<String> {
        let input: serde_json::Value = match input_json {
            Some(raw) => serde_json::from_str(&raw).map_err(|e| err(format!("input_json: {e}")))?,
            None => json!({}),
        };
        let h = areev_core::error::Hash::from_hex(&workflow).map_err(err)?;
        let runner = self.runner(tool_cmd);
        let opts = run_options(max_tokens, max_usd_micros, max_wall_ms, ask_ttl_sec);
        let session = py
            .detach(|| runner.start(&h, &run_id, input, &opts))
            .map_err(err)?;
        Ok(run_session_json(session).to_string())
    }

    /// Resume a parked/interrupted run from its latest checkpoint.
    #[pyo3(signature = (run_id, tool_cmd = None))]
    fn run_resume(&self, py: Python<'_>, run_id: String, tool_cmd: Option<String>) -> PyResult<String> {
        let runner = self.runner(tool_cmd);
        let opts = run_options(None, None, None, None);
        let session = py.detach(|| runner.resume(&run_id, &opts)).map_err(err)?;
        Ok(run_session_json(session).to_string())
    }

    /// Answer a pending Client ask. `responder` is REQUIRED: approval
    /// separation of duties (responder ≠ triggering principal) is checked
    /// structurally, and on governed sessions the responder's own grants
    /// must cover `run.respond`.
    #[pyo3(signature = (run_id, tool_call_id, result_json, responder, is_error = false))]
    fn run_respond(
        &self,
        py: Python<'_>,
        run_id: String,
        tool_call_id: String,
        result_json: String,
        responder: String,
        is_error: bool,
    ) -> PyResult<String> {
        let result: serde_json::Value =
            serde_json::from_str(&result_json).map_err(|e| err(format!("result_json: {e}")))?;
        let runner = self.runner(None);
        py.detach(|| runner.respond(&run_id, &tool_call_id, result, is_error, &responder))
            .map_err(err)?;
        Ok(json!({"responded": tool_call_id, "run_id": run_id}).to_string())
    }

    /// Write the kill-switch marker (the lowest-privilege run verb).
    #[pyo3(signature = (run_id, because = "canceled".to_string()))]
    fn run_cancel(&self, py: Python<'_>, run_id: String, because: String) -> PyResult<String> {
        let runner = self.runner(None);
        let actor = self.actor.clone();
        py.detach(|| runner.cancel(&run_id, &actor, &because)).map_err(err)?;
        Ok(json!({"canceled": run_id}).to_string())
    }

    /// Journal-consistent replay: re-derives every checkpoint, byte-compares
    /// against the stored chain, writes nothing. JSON report.
    fn run_verify(&self, py: Python<'_>, run_id: String) -> PyResult<String> {
        let runner = self.runner(None);
        let report = py.detach(|| runner.verify(&run_id)).map_err(err)?;
        serde_json::to_string(&report).map_err(|e| err(e.to_string()))
    }

    /// Shadow evaluation: replay these runs from their journals with ZERO
    /// effect dispatches (the replay path holds no executor). JSON report.
    fn run_shadow(&self, py: Python<'_>, run_ids: Vec<String>) -> PyResult<String> {
        let runner = self.runner(None);
        let report = py.detach(|| runner.shadow_eval(&run_ids));
        serde_json::to_string(&report).map_err(|e| err(e.to_string()))
    }

    /// §5.4 time-travel fork / migration. Returns the seed checkpoint hash.
    #[pyo3(signature = (base_run_id, new_run_id, at_superstep = None, plan = None))]
    fn run_fork(
        &self,
        py: Python<'_>,
        base_run_id: String,
        new_run_id: String,
        at_superstep: Option<u64>,
        plan: Option<String>,
    ) -> PyResult<String> {
        let plan_hash = match plan {
            Some(p) => Some(areev_core::error::Hash::from_hex(&p).map_err(err)?),
            None => None,
        };
        let runner = self.runner(None);
        let opts = run_options(None, None, None, None);
        let seed = py
            .detach(|| runner.fork(&base_run_id, at_superstep, &new_run_id, plan_hash.as_ref(), &opts))
            .map_err(err)?;
        Ok(seed.to_hex())
    }

    /// Recent run ids, newest first. JSON list string.
    #[pyo3(signature = (limit = 20))]
    fn run_list(&self, py: Python<'_>, limit: usize) -> PyResult<String> {
        let runner = self.runner(None);
        let ids = py.detach(|| runner.recent_runs(limit)).map_err(err)?;
        serde_json::to_string(&ids).map_err(|e| err(e.to_string()))
    }

    /// Op-log cursor read — the change feed the audit/evidence story rides
    /// (same shape as `areev changes-since` and `GET /api/changes`). Returns
    /// `[{op_seq, hlc, op, hash}...]`, ascending; pass the last `op_seq`
    /// back as the next cursor.
    #[pyo3(signature = (after_op_seq = 0, limit = 500))]
    fn changes_since(&self, py: Python<'_>, after_op_seq: i64, limit: usize) -> PyResult<String> {
        let limit = limit.clamp(1, 10_000);
        let rows = py
            .detach(|| self.facade.with_store(|m| m.changes_since(after_op_seq, limit)))
            .map_err(err)?;
        let out: Vec<serde_json::Value> = rows
            .into_iter()
            .map(|r| json!({"op_seq": r.op_seq, "hlc": r.hlc, "op": r.op, "hash": r.hash.to_hex()}))
            .collect();
        serde_json::to_string(&out).map_err(|e| err(e.to_string()))
    }

    /// Which runs produced or refined this grain — the reverse join.
    ///
    /// Walks the provenance chain both ways from `hash`. Runs that merely
    /// *read* the grain are not recorded: a read leaves no grain behind, so
    /// nothing in an append-only store can attest to it.
    #[pyo3(signature = (hash, depth = 4, ns = None))]
    fn runs_touching(
        &self,
        py: Python<'_>,
        hash: String,
        depth: usize,
        ns: Option<String>,
    ) -> PyResult<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let h = Hash::from_hex(&hash).map_err(err)?;
        let runs = py
            .detach(|| self.facade.with_store(|m| m.runs_touching(&ns, &h, depth)))
            .map_err(err)?;
        Ok(json!({"hash": h.to_hex(), "runs": runs}).to_string())
    }

    /// Incremental backup to a bundle file. Returns last_op_seq cursor.
    #[pyo3(signature = (path, since = 0))]
    fn bundle(&self, py: Python<'_>, path: String, since: i64) -> PyResult<i64> {
        let st = py
            .detach(|| self.facade.with_store(|m| m.bundle_since(since, &path)))
            .map_err(err)?;
        Ok(st.last_op_seq)
    }

    /// Apply a bundle (fast-forward, idempotent). Returns ops applied.
    fn import_bundle(&self, py: Python<'_>, path: String) -> PyResult<usize> {
        let st = py
            .detach(|| self.facade.with_store(|m| m.import_bundle(&path)))
            .map_err(err)?;
        Ok(st.applied)
    }

    /// Integrity + content-address verification. Raises on failure.
    fn verify(&self, py: Python<'_>) -> PyResult<String> {
        let r = py
            .detach(|| self.facade.with_store(|m| m.verify()))
            .map_err(err)?;
        if r.integrity != "ok" || r.hash_mismatches > 0 || r.undecodable > 0 {
            return Err(err(AreevError::Storage(format!(
                "verification failed: integrity={} mismatches={} undecodable={}",
                r.integrity, r.hash_mismatches, r.undecodable
            ))));
        }
        Ok(json!({"integrity": r.integrity, "grains": r.grains}).to_string())
    }

    // ── Areev Loop: the governed self-improvement loop (§6.6) ────────────────────

    /// Record a tool call as a Tool grain — the flagship analyzer's food. One
    /// line per call in the agent's tool loop. `thread` groups a session.
    ///
    /// `call_id` is the invocation's own id (the provider's `tool_call_id`),
    /// stored on the grain and queryable — the correlation key back to the LLM
    /// transcript. Omitted, one is synthesized. Either way each call is its own
    /// occurrence, which is what makes a tool that failed five times read as
    /// five failures; recording is append-only, never de-duplicating.
    /// Wave-0 extension: `run_id` correlates the call to a run
    /// (`run_trace`/`runs_touching` read it back); `workflow_hash` + `node_id`
    /// (both or neither) write the `mg:step_action` execution-record link to
    /// the plan; `status`/`failure_cause`/`executor_kind`/`correlation_id`
    /// carry the async lifecycle. Enum strings validate strictly — an unknown
    /// value raises naming the accepted set.
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (name, result, is_error = false, thread = None, call_id = None, input = None,
                        run_id = None, workflow_hash = None, node_id = None, status = None,
                        failure_cause = None, executor_kind = None, correlation_id = None))]
    fn record_tool_call(
        &self,
        py: Python<'_>,
        name: String,
        result: String,
        is_error: bool,
        thread: Option<String>,
        call_id: Option<String>,
        input: Option<String>,
        run_id: Option<String>,
        workflow_hash: Option<String>,
        node_id: Option<String>,
        status: Option<String>,
        failure_cause: Option<String>,
        executor_kind: Option<String>,
        correlation_id: Option<String>,
    ) -> PyResult<String> {
        py.detach(|| {
            self.facade
                .record_tool_call(
                    &self.ns,
                    &name,
                    input.as_deref(),
                    &result,
                    is_error,
                    thread.as_deref(),
                    call_id.as_deref(),
                    run_id.as_deref(),
                    workflow_hash.as_deref(),
                    node_id.as_deref(),
                    status.as_deref(),
                    failure_cause.as_deref(),
                    executor_kind.as_deref(),
                    correlation_id.as_deref(),
                )
                .map(|h| h.to_hex())
        })
        .map_err(err)
    }

    /// Persist a content-addressed harness config and the run -> config link.
    /// `config` is a JSON object containing the pinned model/runtime, sampling
    /// parameters, system prompt and tool catalogue. Returns both hashes.
    fn record_run_manifest(
        &self,
        py: Python<'_>,
        run_id: String,
        config: String,
    ) -> PyResult<String> {
        py.detach(|| self.facade.record_run_manifest(&run_id, &config))
            .map(|(config, link)| {
                json!({"config_hash": config.to_hex(), "link_hash": link.to_hex()}).to_string()
            })
            .map_err(err)
    }

    /// Run one analysis pass. Bare (all args `None`) it never gates — an
    /// evaluator's first call always runs. `full_sweep=True` re-analyzes the
    /// whole memory (the `areev loop reflect` semantics); `policy` is a path
    /// to a host `loop-policy.json` — the only way auto-apply is granted
    /// from the bindings, same file the CLI takes. Returns the run-outcome
    /// JSON.
    #[allow(clippy::too_many_arguments)] // a flat FFI surface; each knob is a distinct scalar
    #[pyo3(signature = (min_new = None, min_new_errors = None, if_stale = None, model = None, llm_cmd = None, ground_model = None, ground_cmd = None, analyzer_cmd = None, full_sweep = false, policy = None))]
    fn loop_run(
        &self,
        py: Python<'_>,
        min_new: Option<u64>,
        min_new_errors: Option<u64>,
        if_stale: Option<String>,
        model: Option<String>,
        llm_cmd: Option<String>,
        ground_model: Option<String>,
        ground_cmd: Option<String>,
        analyzer_cmd: Option<String>,
        full_sweep: bool,
        policy: Option<String>,
    ) -> PyResult<String> {
        let opts = RunOptions {
            min_new,
            min_new_errors,
            if_stale_ms: if_stale.as_deref().and_then(parse_duration_ms),
            namespaces: Vec::new(),
            full_sweep,
            // The handle's actor — the same identity review/apply stamp — so
            // the trigger of an LLM/external finding cannot approve it.
            triggering_actor: Some(self.actor.clone()),
        };
        // The longest call on this surface by far — a sweep plus, optionally,
        // several LLM round trips. Holding the interpreter lock across it would
        // freeze the host for the whole reflection pass.
        let res = py.detach(|| -> PyResult<_> {
            // Optional verified LLM reflection: `model="claude-sonnet"` (key from the
            // env) attaches a built-in HTTP backend; `llm_cmd="..."` a subprocess.
            let mut engine = Engine::with_builtins();
            // Host policy file (§6.2) — mirrors the CLI's --policy. Host config,
            // read per call, never persisted in the memory file.
            if let Some(path) = policy {
                let s = std::fs::read_to_string(&path)
                    .map_err(|e| err(format!("policy {path}: {e}")))?;
                engine = engine.with_policy(areev_loop::Policy::from_json(&s).map_err(err)?);
            }
            if let Some(cmd) = llm_cmd {
                let llm = areev_loop::CommandLlm::new(&cmd, None).map_err(err)?;
                engine = engine.with_llm(Box::new(llm));
            } else if let Some(spec) = model {
                engine = engine.with_llm(areev_llm::resolve(&spec, None, None).map_err(err)?);
            }
            // Optional separate grounding backend (defaults to the reflection model).
            if let Some(cmd) = ground_cmd {
                let g = areev_loop::CommandLlm::new(&cmd, None).map_err(err)?;
                engine = engine.with_ground_llm(Box::new(g));
            } else if let Some(spec) = ground_model {
                engine =
                    engine.with_ground_llm(areev_llm::resolve(&spec, None, None).map_err(err)?);
            }
            // Optional external analyzer (advisory only — never auto-applies).
            if let Some(cmd) = analyzer_cmd {
                engine.register(Box::new(areev_loop::CommandAnalyzer::new(&cmd).map_err(err)?));
            }
            let mut sub = BorrowedSubstrate::new(&self.facade);
            engine.run(&mut sub, &opts, now_ms()).map_err(err)
        })?;
        serde_json::to_string(&res).map_err(err)
    }

    /// List recommendations. `filter` is optional JSON, e.g. `{"status":
    /// "pending"}`; omit or `{"status":"all"}` for every status. JSON list.
    #[pyo3(signature = (filter = None))]
    fn recommendations(&self, py: Python<'_>, filter: Option<String>) -> PyResult<String> {
        // `{"status":"all"}` clears the filter; a named status selects it;
        // anything else (including no filter at all) defaults to pending.
        //
        // This used to be one chain ending in `.or(Some(Pending))`, so `"all"`
        // filtered itself to `None` and then had the pending default put
        // straight back — `"all"` behaved as `"pending"`, and an applied
        // recommendation was missing from a list that promised every status.
        let requested = filter
            .and_then(|f| serde_json::from_str::<serde_json::Value>(&f).ok())
            .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(str::to_string));
        let status = match requested.as_deref() {
            Some("all") => None,
            Some(s) => Some(status_from_str(s).ok_or_else(|| {
                err(format!(
                    "unknown status {s:?} — expected one of: all, pending, approved, \
                     applied, rejected, rolled_back, expired"
                ))
            })?),
            None => Some(RecStatus::Pending),
        };
        let recs = py
            .detach(|| {
                let sub = BorrowedSubstrate::new(&self.facade);
                Engine::with_builtins().recommendations(&sub, status)
            })
            .map_err(err)?;
        let rows: Vec<_> = recs
            .iter()
            .map(|r| {
                json!({
                    "hash": r.hash,
                    "status": r.status.as_str(),
                    "severity": r.severity.as_str(),
                    "analyzer": r.analyzer,
                    "summary": r.summary.render(),
                    "target_ref": r.target_ref,
                    "destructive": r.destructive,
                })
            })
            .collect();
        serde_json::to_string(&rows).map_err(err)
    }

    /// Approve and apply a recommendation in one audited step (§6.6). The
    /// `because` reason is mandatory. Non-rollbackable (destructive) payloads
    /// are rejected unless `allow_destructive=True` — and a refused apply
    /// leaves the recommendation **pending**, so it can still be dismissed.
    ///
    /// `scopes` is a comma-separated subset of
    /// `read,write,review,apply,admin`; omit it for all scopes. Pass a narrow
    /// set to enforce separation of duties from the host.
    #[pyo3(signature = (hash, because, allow_destructive = false, scopes = None))]
    fn apply_recommendation(
        &self,
        py: Python<'_>,
        hash: String,
        because: String,
        allow_destructive: bool,
        scopes: Option<String>,
    ) -> PyResult<String> {
        let scopes = parse_scopes(scopes.as_deref())?;
        let applied = py
            .detach(|| {
                let mut sub = BorrowedSubstrate::new(&self.facade);
                let engine = Engine::with_builtins();
                let now = now_ms();
                // Ask before approving. The approval is a real state
                // transition, and `approved` has no exit but `applied` or
                // `expired` — so recording it first and then hitting the
                // destructive gate stranded the recommendation somewhere the
                // reviewer could no longer reject it. The CLI's separate
                // approve/apply verbs never had this trap; the fused call did.
                engine.preflight_apply(&sub, &hash, &scopes, allow_destructive)?;
                engine.review(&mut sub, &hash, Decision::Approve, &self.actor, ObserverType::Human, &scopes, &because, now)?;
                engine.apply(&mut sub, &hash, &self.actor, ObserverType::Human, &scopes, &because, allow_destructive, now)
            })
            .map_err(err)?;
        Ok(json!({"hash": hash, "rollbackable": applied.rollbackable}).to_string())
    }

    /// Approve a recommendation **without** applying it — the two-person flow
    /// the CLI's separate `approve` verb enables, so a supervising agent can
    /// approve for a human to apply later. Parity with `areev loop approve`.
    #[pyo3(signature = (hash, because, scopes = None))]
    fn approve_recommendation(
        &self,
        py: Python<'_>,
        hash: String,
        because: String,
        scopes: Option<String>,
    ) -> PyResult<String> {
        let scopes = parse_scopes(scopes.as_deref())?;
        py.detach(|| {
            let mut sub = BorrowedSubstrate::new(&self.facade);
            Engine::with_builtins().review(
                &mut sub, &hash, Decision::Approve, &self.actor, ObserverType::Human,
                &scopes, &because, now_ms(),
            )
        })
        .map_err(err)?;
        Ok(json!({"hash": hash, "status": "approved"}).to_string())
    }

    /// Health snapshot of the loop: when it last ran, how much is un-analyzed
    /// since, the queue counts, and whether it looks stalled. Parity with bare
    /// `areev loop` and `GET /api/loop/health` — a host that wires the run
    /// to a hook or cron needs this to notice the trigger came unwired.
    fn loop_health(&self, py: Python<'_>) -> PyResult<String> {
        let health = py
            .detach(|| {
                let sub = BorrowedSubstrate::new(&self.facade);
                Engine::with_builtins().health(&sub, now_ms())
            })
            .map_err(err)?;
        serde_json::to_string(&health).map_err(err)
    }

    /// The analyzer roster: id, whether it is enabled, and its parameters.
    /// JSON list, parity with `areev loop analyzers`.
    fn loop_analyzers(&self, py: Python<'_>) -> PyResult<String> {
        let list = py
            .detach(|| {
                let sub = BorrowedSubstrate::new(&self.facade);
                Engine::with_builtins().analyzer_settings(&sub)
            })
            .map_err(err)?;
        serde_json::to_string(&list).map_err(err)
    }

    /// Enable/disable one analyzer, or set its parameters — reachable from the
    /// console's Setup tab (`POST /api/loop/config`) but not, until now, from
    /// the bindings. `params_json` is an optional JSON object of parameter
    /// overrides. Returns the analyzer's resulting config.
    #[pyo3(signature = (analyzer_id, enabled = None, params_json = None))]
    fn set_analyzer_config(
        &self,
        py: Python<'_>,
        analyzer_id: String,
        enabled: Option<bool>,
        params_json: Option<String>,
    ) -> PyResult<String> {
        let params = match params_json {
            Some(ref s) => serde_json::from_str(s).map_err(err)?,
            None => None,
        };
        let update = areev_loop::AnalyzerConfigUpdate {
            enabled,
            params,
            ..Default::default()
        };
        let cfg = py
            .detach(|| {
                let mut sub = BorrowedSubstrate::new(&self.facade);
                Engine::with_builtins().set_analyzer_config(
                    &mut sub,
                    &analyzer_id,
                    update,
                    &ScopeSet::all(),
                )
            })
            .map_err(err)?;
        serde_json::to_string(&cfg).map_err(err)
    }

    /// Detect sensitive spans in free text with the built-in Tier-0 chain
    /// (docs/anonymization-proposal.md P0). Pure text — touches no grains.
    /// Returns JSON `{"text": <nfc text>, "detections": [...]}`; offsets are
    /// UTF-8 bytes into the returned normalized text.
    #[pyo3(signature = (text, policy_json = None))]
    fn scan_text(
        &self,
        py: Python<'_>,
        text: String,
        policy_json: Option<String>,
    ) -> PyResult<String> {
        py.detach(|| self.facade.scan_text(&text, policy_json.as_deref()))
            .map_err(err)
    }

    /// Pseudonymize free text: detected spans become typed placeholders
    /// (`[PERSON_1]`) and the reversible spans' placeholder→value map is
    /// returned to the caller. Returns JSON
    /// `{"text", "mapping", "mapping_id", "replaced"}`. `key_hex` keys the
    /// `mapping_id` derivation; without it, don't ship the id anywhere the
    /// mapping doesn't also travel.
    #[pyo3(signature = (text, policy_json = None, key_hex = None))]
    fn anonymize_text(
        &self,
        py: Python<'_>,
        text: String,
        policy_json: Option<String>,
        key_hex: Option<String>,
    ) -> PyResult<String> {
        py.detach(|| {
            self.facade
                .anonymize_text(&text, policy_json.as_deref(), key_hex.as_deref())
        })
        .map_err(err)
    }

    /// Restore originals in an LLM response: replaces exact placeholder
    /// tokens using `mapping_json` (object of placeholder → value). Returns
    /// JSON `{"text", "replaced", "unmatched"}` — unmatched tokens are left
    /// intact and reported, never guessed.
    #[pyo3(signature = (text, mapping_json))]
    fn rehydrate_text(
        &self,
        py: Python<'_>,
        text: String,
        mapping_json: String,
    ) -> PyResult<String> {
        py.detach(|| self.facade.rehydrate_text(&text, &mapping_json))
            .map_err(err)
    }

    /// Reject a recommendation with a reason (the library-friendly name for
    /// `areev loop reject`).
    #[pyo3(signature = (hash, why, scopes = None))]
    fn dismiss_recommendation(
        &self,
        py: Python<'_>,
        hash: String,
        why: String,
        scopes: Option<String>,
    ) -> PyResult<String> {
        let scopes = parse_scopes(scopes.as_deref())?;
        py.detach(|| {
            let mut sub = BorrowedSubstrate::new(&self.facade);
            Engine::with_builtins()
                .review(&mut sub, &hash, Decision::Reject, &self.actor, ObserverType::Human, &scopes, &why, now_ms())
        })
        .map_err(err)?;
        Ok(json!({"hash": hash, "status": "rejected"}).to_string())
    }

    /// Roll back an applied recommendation (retracts the grains it created).
    /// Mandatory reason; fails for non-rollbackable applies (FORGET has no
    /// inverse). Parity with `areev loop rollback`.
    fn rollback_recommendation(
        &self,
        py: Python<'_>,
        hash: String,
        because: String,
    ) -> PyResult<String> {
        py.detach(|| {
            let mut sub = BorrowedSubstrate::new(&self.facade);
            Engine::with_builtins()
                .rollback(&mut sub, &hash, &self.actor, ObserverType::Human, &ScopeSet::all(), &because, now_ms())
        })
        .map_err(err)?;
        Ok(json!({"hash": hash, "status": "rolled_back"}).to_string())
    }

    /// Measured outcomes of applied recommendations — the Verify gate's
    /// record (`held` / `regressed` per checkpoint). JSON list, parity with
    /// `areev loop outcomes`.
    fn loop_outcomes(&self, py: Python<'_>) -> PyResult<String> {
        let outs = py
            .detach(|| {
                let sub = BorrowedSubstrate::new(&self.facade);
                Engine::with_builtins().outcomes(&sub)
            })
            .map_err(err)?;
        serde_json::to_string(&outs).map_err(err)
    }
}

impl Areev {
    /// The runtime driver over this handle's shared facade. Host tools run
    /// through the optional subprocess seam; the session's actor is the
    /// acting principal (respond passes the responder explicitly).
    fn runner(&self, tool_cmd: Option<String>) -> areev_run::Runner {
        let executor: std::sync::Arc<dyn areev_run::HostToolExecutor> = match tool_cmd {
            Some(cmd) if !cmd.trim().is_empty() => {
                std::sync::Arc::new(areev_run::CommandExecutor::new(&cmd))
            }
            _ => {
                struct NoExec;
                impl areev_run::HostToolExecutor for NoExec {
                    fn execute(
                        &self,
                        tool_name: &str,
                        _h: &str,
                        _i: &serde_json::Value,
                        _k: &str,
                    ) -> areev_run::ExecResult {
                        areev_run::ExecResult::Err {
                            cause: areev_run::FailCause::ExecutorError,
                            detail: format!(
                                "no tool_cmd given; cannot execute host tool '{tool_name}'"
                            ),
                        }
                    }
                }
                std::sync::Arc::new(NoExec)
            }
        };
        areev_run::Runner {
            facade: std::sync::Arc::clone(&self.facade),
            clock: std::sync::Arc::new(areev_run::SystemClock),
            executor,
            llm: None,
            observer: None,
            ns: self.ns.clone(),
            principal: self.actor.clone(),
        }
    }
}

fn run_options(
    max_tokens: Option<u64>,
    max_usd_micros: Option<u64>,
    max_wall_ms: Option<u64>,
    ask_ttl_sec: Option<i64>,
) -> areev_run::RunOptions {
    areev_run::RunOptions {
        budgets: areev_run::BudgetsSpec {
            max_supersteps: None,
            max_tokens,
            max_usd_micros,
            max_wall_ms,
            max_storage_bytes: None,
        },
        ask_ttl_sec,
        workers: 4,
        on_dangling: Default::default(),
        llm_max_tokens: None,
        inject_crash: None,
    }
}

fn run_session_json(session: areev_run::RunSession) -> serde_json::Value {
    match session {
        areev_run::RunSession::Finished { outcome, run_id } => {
            json!({"run_id": run_id, "finished": format!("{outcome:?}")})
        }
        areev_run::RunSession::Parked { envelope, run_id } => {
            json!({"run_id": run_id, "parked": envelope})
        }
    }
}

/// Drop a memory schema entirely — the postgres backend's memory-level
/// erasure primitive (`DROP SCHEMA … CASCADE`), the analogue of deleting a
/// memory file. Destroys the memory AND its telemetry/blobs. Admin surface:
/// gate it like any destructive operation in your host.
#[cfg(feature = "postgres")]
#[pyfunction]
fn drop_postgres_schema(py: Python<'_>, url: String, schema: String) -> PyResult<()> {
    py.detach(|| areev_store::pg::drop_postgres_schema(&url, &schema))
        .map_err(err)
}

#[cfg(not(feature = "postgres"))]
#[pyfunction]
fn drop_postgres_schema(_py: Python<'_>, _url: String, _schema: String) -> PyResult<()> {
    Err(err("this build lacks the postgres backend (rebuild with the `postgres` feature)"))
}

#[pymodule]
fn areev(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Areev>()?;
    m.add_function(pyo3::wrap_pyfunction!(drop_postgres_schema, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
