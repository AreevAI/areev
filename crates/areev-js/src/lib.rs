//! areev — Node.js (napi-rs) bindings for Areev.
//!
//! Mirrors the Python binding (crates/areev-py): thin and version-stable by
//! design — scalar args in, JSON strings out for anything structured; every
//! error surfaces as a JS `Error`. turso/tokio are native, so this is a
//! *native* Node addon (napi-rs), not WASM. Build with
//! `napi build --platform --release`; `require('areev')`.

use areev_cal::{CalExecutor, CalExecutorConfig, CalStoreFacade, AreevFacade};
use areev_core::error::{AreevError, Hash};
use areev_store::memory_tool::MemoryTool;
use areev_store::{
    parse_relations, Axis, CommandEmbed, Areev as RustAreev, Direction, FactDraft, TelemetryMode,
};
use areev_loop_adapter::{now_ms, BorrowedSubstrate};
use napi::bindgen_prelude::{Buffer, Uint8Array};
use napi_derive::napi;
use serde_json::json;
use areev_loop::{Decision, Engine, ObserverType, RecStatus, RunOptions, ScopeSet};

/// Drop a memory schema entirely — the postgres backend's memory-level
/// erasure primitive (`DROP SCHEMA … CASCADE`), the analogue of deleting a
/// memory file. Destroys the memory AND its telemetry/blobs. Admin surface:
/// gate it like any destructive operation in your host.
/// Read one CAS blob from a memory's `.blobs` sidecar without opening the
/// database.
///
/// The embedded backend's lock is exclusive, so a `--tool-cmd` subprocess
/// cannot open the memory its own run is holding. No lock is needed: a blob is
/// immutable and its address is its checksum, re-verified here. `null` means
/// the blob is sealed — open the memory with its passphrase for that.
#[napi(ts_return_type = "Buffer | null")]
pub fn read_blob_offline(db_path: String, uri: String) -> napi::Result<Option<napi::bindgen_prelude::Buffer>> {
    match areev_store::read_blob_offline(&db_path, &uri) {
        Ok(Some(b)) => Ok(Some(b.into())),
        Ok(None) => Ok(None),
        Err(e) => Err(err(e)),
    }
}

#[napi]
pub fn drop_postgres_schema(url: String, schema: String) -> napi::Result<()> {
    areev_store::pg::drop_postgres_schema(&url, &schema).map_err(err)
}

fn err<E: std::fmt::Display>(e: E) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

/// Resolve an LLM backend the same two ways the CLI does: a subprocess
/// (`llmCmd`, the zero-dependency escape hatch) or a built-in HTTP provider
/// (`model`, key read from the environment). The subprocess wins when both are
/// given. Both fail at construction, before anything is written.
fn resolve_llm(
    cmd: Option<String>,
    spec: Option<String>,
) -> napi::Result<Option<Box<dyn areev_loop::LlmBackend>>> {
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
) -> napi::Result<(usize, Vec<FactDraft>, Option<&'static str>)> {
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
/// `None` means all scopes — which is what the bindings always hardcoded, so
/// the separation-of-duties gate could not be enforced from Node at all.
fn parse_scopes(spec: Option<&str>) -> napi::Result<ScopeSet> {
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
                return Err(err(AreevError::Validation(format!(
                    "unknown scope {other:?} — expected a comma-separated subset of: \
                     read, write, review, apply, admin"
                ))))
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

fn parse_hash(hex: &str) -> napi::Result<Hash> {
    Hash::from_hex(hex).map_err(err)
}

/// Runs one store call on libuv's thread pool and settles a JS promise with
/// the result.
///
/// Every method here used to do its work inline, on the thread calling into
/// the addon — which in Node is the thread running everything else. A single
/// `importBundle` or `migrate` stopped timers, sockets and the HTTP server for
/// as long as it took. Node has exactly one place to put blocking work, and
/// this is it.
///
/// `Task::compute` deliberately runs on a libuv worker rather than on a tokio
/// runtime: the store owns a current-thread runtime and drives it with
/// `block_on`, and doing that from inside another runtime's worker panics.
/// A libuv thread has no runtime attached, so `block_on` is free to take it.
/// One job type per return type, rather than one generic job.
///
/// A generic `StoreJob<T>` compiles and runs, but napi's TypeScript generator
/// cannot see through the parameter: a type alias comes out as the literal
/// `Job<string>` (which is not a type the `.d.ts` defines) and the un-aliased
/// form degrades to `Promise<unknown>`. Both hand callers a binding that works
/// at runtime and lies at compile time. Concrete types generate the real
/// signatures — `Promise<string>`, `Promise<string | null>`, `Promise<void>`.
/// [`areev_store::EmbedBackend`] over a JS callback
/// `embed(text: string): number[]`, bridged with a threadsafe function.
///
/// Every store call in this binding runs on a libuv worker (the job
/// types below), never on the JS thread — so blocking a worker here while
/// the JS thread services the callback is deadlock-free by construction.
/// Keep it that way: a *synchronous* napi method that triggers embedding
/// would wait on its own thread.
struct JsEmbed {
    tsfn: napi::threadsafe_function::ThreadsafeFunction<String, Vec<f64>, String, napi::Status, false>,
    dim: usize,
    model: String,
}

impl areev_store::EmbedBackend for JsEmbed {
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, text: &str) -> areev_core::error::Result<Vec<f32>> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.tsfn.call_with_return_value(
            text.to_string(),
            napi::threadsafe_function::ThreadsafeFunctionCallMode::NonBlocking,
            move |ret: napi::Result<Vec<f64>>, _env| {
                let _ = tx.send(ret);
                Ok(())
            },
        );
        let out = rx
            .recv()
            .map_err(|_| AreevError::Storage("js embedder never answered".into()))?
            .map_err(|e| AreevError::Storage(format!("js embedder raised: {e}")))?;
        Ok(out.into_iter().map(|v| v as f32).collect())
    }
    fn model(&self) -> &str {
        &self.model
    }
}

macro_rules! job_types {
    ($($(#[$m:meta])* $name:ident => $ty:ty),* $(,)?) => {$(
        $(#[$m])*
        pub struct $name {
            work: Option<Box<dyn FnOnce() -> napi::Result<$ty> + Send>>,
        }

        impl $name {
            fn spawn(
                work: impl FnOnce() -> napi::Result<$ty> + Send + 'static,
            ) -> napi::bindgen_prelude::AsyncTask<Self> {
                napi::bindgen_prelude::AsyncTask::new($name { work: Some(Box::new(work)) })
            }
        }

        impl napi::Task for $name {
            type Output = $ty;
            type JsValue = $ty;

            fn compute(&mut self) -> napi::Result<$ty> {
                // Called once per task; the Option exists only because the
                // trait hands out `&mut self` rather than `self`.
                match self.work.take() {
                    Some(work) => work(),
                    None => Err(err("store job polled twice")),
                }
            }

            fn resolve(&mut self, _env: napi::Env, output: $ty) -> napi::Result<$ty> {
                Ok(output)
            }
        }
    )*};
}

job_types! {
    /// Store call whose result is a JSON string — most of this surface.
    StringJob => String,
    /// Store call that can legitimately answer "nothing" (`latest`).
    MaybeStringJob => Option<String>,
    /// Store call kept for its effect (`forget`, `setEmbedderCommand`).
    UnitJob => (),
    /// Store call returning a count.
    U32Job => u32,
    /// Store call returning an op-log cursor.
    I64Job => i64,
    /// Store call returning raw bytes (`getBlob`). Blobs are binary, so the
    /// JSON-out convention deliberately does not apply to them.
    BufferJob => Buffer,
}

/// One memory = one file. Open with `new Areev("caller.db", "caller")`.
///
/// Every method returns a promise. Opening is the one exception — it is
/// synchronous, so a constructor can still fail loudly.
///
/// **Await your writes.** Promises settle in completion order, not call order,
/// and concurrent calls contend for one lock inside the store. Firing
/// `addFact` and `recall` without awaiting leaves which one lands first up to
/// the thread pool.
/// A closable slot holding the store.
///
/// Node has no deterministic drop — an `Areev` that has gone out of JS scope is
/// released whenever GC gets to it — so a handle cannot be closed by letting it
/// fall out of scope the way Rust and Python can. That matters now that a
/// second handle on one file is refused: without an explicit `close()`, a
/// perfectly ordinary open → use → reopen sequence would hit STO-E002 against a
/// handle the caller had already finished with and had no way to release.
///
/// `None` means closed; every method then fails with a message saying so
/// rather than panicking or silently reopening.
type FacadeSlot = std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<AreevFacade>>>>;

fn take_facade(slot: &FacadeSlot) -> napi::Result<std::sync::Arc<AreevFacade>> {
    slot.lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .ok_or_else(|| {
            err(AreevError::Validation(
                "this handle is closed — open a new Areev for further calls".into(),
            ))
        })
}

/// Verb check for the binding methods that reach the store directly instead
/// of through a gated `cal_*` facade method. `authFile`/`principal` is
/// documented to fail closed (CAL 1.3 §9), so a sandboxed handle must not be
/// able to erase — or export a subject's dossier — merely by calling the
/// binding method rather than the CAL statement that does the same thing.
fn check_verb(
    facade: &AreevFacade,
    verb: areev_core::authz::Verb,
    ns: &str,
) -> napi::Result<()> {
    facade.authz().check(verb, ns).map_err(err)
}

#[napi]
pub struct Areev {
    /// Shared so a queued job can hold the store open independently of the JS
    /// object that started it.
    facade: FacadeSlot,
    ns: String,
    /// Host-asserted actor label stamped on every loop audit grain (§6.6).
    actor: String,
}

#[napi]
impl Areev {
    #[napi(constructor)]
    pub fn new(
        path: String,
        ns: Option<String>,
        passphrase: Option<String>,
        actor: Option<String>,
        telemetry: Option<String>,
        principal: Option<String>,
        index_text: Option<bool>,
    ) -> napi::Result<Self> {
        let ns = ns.unwrap_or_else(|| "shared".to_string());
        let actor = actor.unwrap_or_else(|| "user:local".to_string());
        // Recall-telemetry sidecar (host capability, §8): agents are the main
        // telemetry producers, so the binding default is `aggregate`; pass
        // telemetry="off" to disable. Never a file-truth.
        let telemetry = telemetry.unwrap_or_else(|| "aggregate".to_string());
        let tel = TelemetryMode::parse(&telemetry)
            .ok_or_else(|| err(format!("unknown telemetry mode '{telemetry}' (off|aggregate|full)")))?;
        // Encryption at rest: a passphrase derives an AES-256 key (Argon2id;
        // non-secret salt in a <path>.kdf sidecar). Host-supplied, never
        // stored in the file — same rules as the CLI's --passphrase-env.
        //
        // A postgres://…?schema=<name> DSN selects the server-tier backend —
        // same API, the memory lives in a Postgres schema (stateless-host
        // deployments; multiple concurrent writers per memory). The page
        // cipher is file-backend-only, so a passphrase with a DSN is an error.
        // `index_text` follows the CLI's `--index-text` and Python's
        // `index_text=`: left unset, the file's own declaration wins; passed
        // explicitly it is a deliberate re-stamp, reported via
        // `openWarnings()`.
        let is_pg = path.starts_with("postgres://") || path.starts_with("postgresql://");
        let store = match (is_pg, passphrase) {
            (true, Some(_)) => {
                return Err(err(
                    "passphrase applies to file-backed memories (page cipher + .kdf sidecar); \
                     on the postgres backend use TDE/pgcrypto at the deployment layer",
                ))
            }
            (true, None) => {
                let (url, schema) =
                    areev_store::pg::split_schema_url(&path).map_err(err)?;
                match index_text {
                    Some(want_text) => RustAreev::open_postgres_with(
                        &url,
                        &schema,
                        areev_store::AreevOptions {
                            index_text: want_text,
                            telemetry: tel,
                            ..areev_store::AreevOptions::default()
                        },
                    )
                    .map_err(err)?,
                    None if tel != TelemetryMode::Off => {
                        RustAreev::open_postgres_with_telemetry(&url, &schema, tel).map_err(err)?
                    }
                    None => RustAreev::open_postgres(&url, &schema).map_err(err)?,
                }
            }
            (false, pass) => match (index_text, pass) {
                (None, Some(p)) => {
                    RustAreev::open_with_passphrase_telemetry(&path, &p, tel).map_err(err)?
                }
                (None, None) => RustAreev::open_with_telemetry(&path, tel).map_err(err)?,
                (Some(want_text), pass) => {
                    let key = match pass {
                        Some(p) => Some(*RustAreev::derive_key_for(&path, &p).map_err(err)?),
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
                    .map_err(err)?
                }
            },
        };
        // `principal` binds the session to the file's grants for that
        // principal (fail closed — CAL 1.3 §9). Absent, the handle is the
        // owner, as ever; the loop actor follows the bound principal
        // unless `actor` was given explicitly.
        let session = AreevFacade::with_session(store, Some(ns.clone()), None);
        let (session, actor) = match principal {
            Some(p) => {
                let f = session.with_principal(&p).map_err(err)?;
                let actor = if actor == "user:local" { p } else { actor };
                (f, actor)
            }
            None => (session, actor),
        };
        let facade = std::sync::Arc::new(std::sync::Mutex::new(Some(std::sync::Arc::new(session))));
        Ok(Areev { facade, ns, actor })
    }

    /// Release this handle's claim on the memory file.
    ///
    /// One memory is one writer: a second handle on the same file is refused at
    /// open. Rust and Python release on drop, but Node's is whenever GC runs —
    /// so without this there is no way to say "I am done with this file" and
    /// reopen it in the same process. Calling a method afterwards is an error,
    /// not a silent reopen. Idempotent; in-flight async calls finish first.
    #[napi]
    pub fn close(&self) {
        *self.facade.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Reconciliation warnings from open (file-vs-host declaration changes,
    /// embedding-model mismatches). JSON list string.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn open_warnings(&self) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let w = facade.with_store(|m| m.open_warnings().to_vec());
            serde_json::to_string(&w).map_err(err)
        })
    }

    /// Set the host-scoped run identifier copied into subsequent recall
    /// telemetry. Pass null to clear it; it is never a file truth.
    #[napi]
    pub fn set_run_id(&self, run_id: Option<String>) -> napi::Result<()> {
        let facade = take_facade(&self.facade)?;
        facade.with_store(|m| m.set_run_id(run_id.as_deref()));
        Ok(())
    }

    /// Install a command embedder (same contract as the CLI's --embed-cmd):
    /// the command gets the text on stdin and must print a JSON array of
    /// numbers. Probed once here to learn the dimension. Enables the vector
    /// recall leg; grains added afterwards are embedded. (For an
    /// in-process callback embedder, see `setEmbedder`; the command
    /// embedder remains the zero-dependency path.)
    #[napi(ts_return_type = "Promise<void>")]
    pub fn set_embedder_command(
        &self,
        cmd: String,
        model: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<UnitJob> {
        let slot = self.facade.clone();
        UnitJob::spawn(move || {
            let facade = take_facade(&slot)?;
            // Probing the command spawns a child process — worth keeping off
            // the event loop even though it only happens once.
            let ce = CommandEmbed::new(&cmd, model.as_deref()).map_err(err)?;
            facade.with_store(|m| m.set_embedder(Box::new(ce)));
            Ok(())
        })
    }

    /// Install an embedding callback: `embed(text: string): number[]` —
    /// the JS mirror of Python's `set_embedder`. Probed once here (on the
    /// JS thread) to learn the dimension, recorded as the file's embedding
    /// provenance; store-side embeds run on worker threads and call back
    /// through a threadsafe function.
    #[napi]
    pub fn set_embedder(
        &self,
        embed: napi::bindgen_prelude::Function<String, Vec<f64>>,
        model: Option<String>,
    ) -> napi::Result<()> {
        let probe = embed.call("dimension probe".to_string())?;
        let dim = probe.len();
        if dim == 0 {
            return Err(err("embedder returned an empty vector"));
        }
        let tsfn = embed.build_threadsafe_function().build()?;
        let backend = JsEmbed {
            tsfn,
            dim,
            model: model.unwrap_or_else(|| "javascript".to_string()),
        };
        let facade = take_facade(&self.facade)?;
        facade.with_store(|m| m.set_embedder(Box::new(backend)));
        Ok(())
    }

    /// Batch-add grains in ONE write transaction — the JS mirror of
    /// Python's `add_batch`. `grainsJson` is a JSON array of
    /// `{grain_type|type, fields}` objects; returns a JSON array of
    /// hashes. Worth ~1.6x over one-at-a-time `add()`, and only with the
    /// BM25 text index off (with it on, per-row index cost swamps
    /// batching).
    #[napi(ts_return_type = "Promise<string>")]
    pub fn add_batch(
        &self,
        grains_json: String,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let default_ns = ns.unwrap_or_else(|| self.ns.clone());
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let items: Vec<serde_json::Value> = serde_json::from_str(&grains_json).map_err(err)?;
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
            let hashes = facade.cal_add_batch(&entries).map_err(err)?;
            let out: Vec<String> = hashes.iter().map(|h| h.to_hex()).collect();
            serde_json::to_string(&out).map_err(err)
        })
    }

    /// Free-text recall over the BM25 (and vector, when an embedder is
    /// installed) legs — the JS mirror of Python's `search`, the same path
    /// as `areev search` and CAL's `RECALL … ABOUT`. Returns a JSON list
    /// string shaped like `recall()`. Errors loudly when the file has
    /// neither leg, instead of a silent empty list.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn search(
        &self,
        query: String,
        subject: Option<String>,
        relation: Option<String>,
        k: Option<u32>,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let k = k.unwrap_or(10) as usize;
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let (has_text, has_vec) = facade
                .with_store(|m| (m.index_text_enabled(), m.embedder_dim().is_some()));
            if !has_text && !has_vec {
                return Err(err(
                    "search() needs a text or vector leg: this file has the BM25 index off \
                     (indexText: false) and no embedder installed — reopen with indexText: \
                     true and then call reindexText() to index the grains written while it \
                     was off, or call setEmbedder()/setEmbedderCommand()",
                ));
            }
            let grains = facade
                .with_store(|m| {
                    m.recall_hybrid(&ns, subject.as_deref(), relation.as_deref(), Some(&query), k, None)
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
        })
    }

    /// Backfill + rebuild the BM25 text index (e.g. after bulk loads, or on
    /// a file that flipped text indexing on later). Returns rows backfilled.
    #[napi(ts_return_type = "Promise<number>")]
    pub fn reindex_text(&self) -> napi::bindgen_prelude::AsyncTask<U32Job> {
        let slot = self.facade.clone();
        U32Job::spawn(move || {
            let facade = take_facade(&slot)?;
            facade
                .with_store(|m| m.rebuild_text_index())
                .map(|n| n as u32)
                .map_err(err)
        })
    }

    /// Rebuild the link indexes: reverse provenance, run correlation, and
    /// `related_to` cross-links. Returns index rows written.
    ///
    /// `open()` heals a file that predates these indexes, so this is for
    /// rebuilding on demand — the counterpart of `reindexText()`, and what
    /// `areev reindex` runs.
    #[napi(ts_return_type = "Promise<number>")]
    pub fn reindex_links(&self) -> napi::bindgen_prelude::AsyncTask<U32Job> {
        let slot = self.facade.clone();
        U32Job::spawn(move || {
            let facade = take_facade(&slot)?;
            facade
                .with_store(|m| m.rebuild_link_indexes())
                .map(|n| n as u32)
                .map_err(err)
        })
    }

    /// Anthropic memory-tool command (view/create/str_replace/insert/delete/
    /// rename over /memories): pass the tool-call object as JSON; returns the
    /// tool result text. Wire this as your memory-tool backend.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn memory_tool(
        &self,
        command_json: String,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let cmd: serde_json::Value = serde_json::from_str(&command_json).map_err(err)?;
            facade
                .with_store(|m| {
                    let mut t = MemoryTool::new(m, &ns);
                    t.execute(&cmd)
                })
                .map_err(err)
        })
    }

    /// Import another memory system's export. `source`: mem0 | mem0-history |
    /// langgraph | letta | letta-archival | zep | jsonl. `payload` is the
    /// export file's contents; `history` the optional mem0 history payload.
    /// (basic-memory vault directories import via the CLI: `areev migrate`.)
    /// Returns {added, superseded, forgotten, skipped, notes} as JSON.
    /// Re-runs skip what is already imported.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn migrate(
        &self,
        source: String,
        payload: String,
        history: Option<String>,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let rep = facade
                .with_store(|m| {
                    areev_store::migrate::migrate_payload(
                        m,
                        &ns,
                        &source,
                        &payload,
                        history.as_deref(),
                    )
                })
                .map_err(err)?;
            Ok(rep.to_json().to_string())
        })
    }

    /// Add a Fact. Returns the content address (64-hex).
    /// Add a Fact. With `idempotent = true`, a re-add of the value already at
    /// the `(subject, relation)` head writes nothing and returns the existing
    /// hash (value-level dedup, not just byte-identical replay).
    #[napi(ts_return_type = "Promise<string>")]
    pub fn add_fact(
        &self,
        subject: String,
        relation: String,
        object: String,
        confidence: Option<f64>,
        ns: Option<String>,
        idempotent: Option<bool>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let mut fields = serde_json::Map::new();
        fields.insert("subject".into(), json!(subject));
        fields.insert("relation".into(), json!(relation));
        fields.insert("object".into(), json!(object));
        fields.insert("confidence".into(), json!(confidence.unwrap_or(0.9)));
        fields.insert(
            "namespace".into(),
            json!(ns.unwrap_or_else(|| self.ns.clone())),
        );
        let idempotent = idempotent.unwrap_or(false);
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            if idempotent {
                Ok(facade.cal_add_if_novel("fact", &fields).map_err(err)?.0.to_hex())
            } else {
                Ok(facade.cal_add("fact", &fields).map_err(err)?.to_hex())
            }
        })
    }

    /// Add any grain type from a JSON fields object. Returns the hash.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn add(
        &self,
        grain_type: String,
        fields_json: String,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let default_ns = ns.unwrap_or_else(|| self.ns.clone());
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let mut fields: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(&fields_json).map_err(err)?;
            fields
                .entry("namespace".to_string())
                .or_insert_with(|| json!(default_ns));
            Ok(facade.cal_add(&grain_type, &fields).map_err(err)?.to_hex())
        })
    }

    /// Structural recall, newest-first. Returns a JSON list string.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn recall(
        &self,
        subject: String,
        relation: Option<String>,
        k: Option<u32>,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let k = k.unwrap_or(16) as usize;
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let grains = facade
                .with_store(|m| m.recall(&ns, &subject, relation.as_deref(), k))
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
        })
    }

    /// Current head for (subject, relation) — JSON string or null.
    #[napi(ts_return_type = "Promise<string | null>")]
    pub fn latest(
        &self,
        subject: String,
        relation: String,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<MaybeStringJob> {
        let slot = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        MaybeStringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let head = facade
                .with_store(|m| m.latest(&ns, &subject, &relation))
                .map_err(err)?;
            Ok(head.map(|g| {
                json!({
                    "hash": g.hash.to_hex(),
                    "fields": g.fields,
                })
                .to_string()
            }))
        })
    }

    /// Supersede old_hash with a new version (append-only evolution).
    #[napi(ts_return_type = "Promise<string>")]
    pub fn supersede(
        &self,
        old_hash: String,
        grain_type: String,
        fields_json: String,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let default_ns = ns.unwrap_or_else(|| self.ns.clone());
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let old = parse_hash(&old_hash)?;
            let mut fields: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(&fields_json).map_err(err)?;
            fields
                .entry("namespace".to_string())
                .or_insert_with(|| json!(default_ns));
            Ok(facade
                .cal_supersede(&old, &grain_type, &fields)
                .map_err(err)?
                .to_hex())
        })
    }

    /// Erase a grain from the hot store (tombstoned). Host-level op.
    /// Routed through the facade so it carries the same `delete` check and
    /// Tier-2 audit record as CAL's `FORGET <hash>`.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn forget(&self, hash: String) -> napi::bindgen_prelude::AsyncTask<UnitJob> {
        let slot = self.facade.clone();
        UnitJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let h = parse_hash(&hash)?;
            facade.cal_delete(&h, None).map_err(err)
        })
    }

    /// Right-to-erasure for one identity: erase every grain holding a
    /// STRUCTURED reference to `subject` in the session namespace (or `ns`) —
    /// the full supersession history, grains referencing it in object
    /// position, its thread events, its dictionary entry, and erased-only
    /// vocabulary — with replicating tombstones. Resolves to the erasure
    /// report as JSON (counts only, no identity material). Host-level
    /// destructive op: gate it like your other compliance endpoints. See
    /// docs/erasure.md for the scope contract.
    /// Identity matching always covers partition-style keys (`pat`,
    /// `pat#visit1` — never `patricia`); pass `textMentions=true` to ALSO
    /// erase grains whose indexed text mentions the identity's tokens
    /// (search symmetry — opt-in because token matching over-reaches for
    /// common-word identifiers).
    #[napi(ts_return_type = "Promise<string>")]
    pub fn forget_subject(
        &self,
        subject: String,
        ns: Option<String>,
        text_mentions: Option<bool>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let opts = areev_store::ErasureOptions { text_mentions: text_mentions.unwrap_or(false) };
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            check_verb(&facade, areev_core::authz::Verb::Erase, &ns)?;
            let rep =
                facade.with_store(|m| m.forget_subject_with(&ns, &subject, opts)).map_err(err)?;
            Ok(serde_json::json!({
                "grains_erased": rep.grains_erased,
                "terms_removed": rep.terms_removed,
                "vocab_removed": rep.vocab_removed,
                "blobs_reclaimed": rep.blobs_reclaimed,
            })
            .to_string())
        })
    }

    /// DSAR read (GDPR Art. 15/20): everything `forgetSubject` WOULD erase
    /// for one identity — exact + partition keys, full history — as
    /// `{"identity_names": [...], "grains": [{hash, type, fields}, ...]}`.
    /// The same selector as erasure, in show-me mode.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn subject_report(
        &self,
        subject: String,
        ns: Option<String>,
        text_mentions: Option<bool>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let opts = areev_store::ErasureOptions { text_mentions: text_mentions.unwrap_or(false) };
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            // The DSAR read is `read`-gated, exactly as `REPORT SUBJECT` is.
            check_verb(&facade, areev_core::authz::Verb::Read, &ns)?;
            let rep =
                facade.with_store(|m| m.subject_report_with(&ns, &subject, opts)).map_err(err)?;
            let grains: Vec<serde_json::Value> = rep
                .grains
                .iter()
                .map(|g| {
                    serde_json::json!({
                        "hash": g.hash.to_hex(),
                        "type": g.grain_type.as_str(),
                        "fields": g.fields.clone().into_iter().collect::<serde_json::Map<_, _>>(),
                    })
                })
                .collect();
            Ok(serde_json::json!({ "identity_names": rep.identity_names, "grains": grains })
                .to_string())
        })
    }

    /// Export the subject selection as a portable MGB1 bundle (GDPR Art. 20
    /// portability) — importable into any OMS store. Resolves to the bundle
    /// stats JSON.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn subject_bundle(
        &self,
        path: String,
        subject: String,
        ns: Option<String>,
        text_mentions: Option<bool>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let opts = areev_store::ErasureOptions { text_mentions: text_mentions.unwrap_or(false) };
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            check_verb(&facade, areev_core::authz::Verb::Read, &ns)?;
            let stats = facade
                .with_store(|m| m.subject_bundle_with(&ns, &subject, opts, &path))
                .map_err(err)?;
            Ok(serde_json::json!({
                "ops": stats.ops,
                "bytes": stats.bytes,
                "last_op_seq": stats.last_op_seq,
            })
            .to_string())
        })
    }

    /// Retention sweep: erase every grain with `created_at` older than
    /// `cutoffMs` (epoch milliseconds), optionally limited to one grain type
    /// (e.g. "event") and scoped to the session namespace (or `ns`; pass
    /// ns="" to sweep every namespace). Resolves to the report JSON.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn forget_older_than(
        &self,
        cutoff_ms: i64,
        ns: Option<String>,
        grain_type: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let gt = match &grain_type {
                Some(s) => Some(
                    areev_core::types::GrainType::from_str(s)
                        .ok_or_else(|| err(format!("unknown grain type '{s}'")))?,
                ),
                None => None,
            };
            let ns_opt = if ns.is_empty() { None } else { Some(ns.as_str()) };
            // ns="" sweeps every namespace, so it takes `erase` on all of them.
            check_verb(
                &facade,
                areev_core::authz::Verb::Erase,
                ns_opt.unwrap_or("*"),
            )?;
            let rep = facade
                .with_store(|m| m.forget_older_than(ns_opt, cutoff_ms, gt))
                .map_err(err)?;
            Ok(serde_json::json!({
                "grains_erased": rep.grains_erased,
                "terms_removed": rep.terms_removed,
                "vocab_removed": rep.vocab_removed,
                "blobs_reclaimed": rep.blobs_reclaimed,
            })
            .to_string())
        })
    }

    /// remember(): store content as an Event, then attach the facts
    /// distilled from it. Three routes to those facts, in precedence order:
    /// `factsJson` (pre-extracted by the host — a JSON list of
    /// {subject, relation, object, confidence}), `llmCmd` (a subprocess
    /// backend), or `model` ("openai:gpt-4o-mini", key from the env).
    ///
    /// The raw text is written before the model is called, so a failed
    /// extraction never costs the raw text — the error names the hash it was
    /// stored under. Model-extracted facts are stamped
    /// `verification_status="unverified"` unless `groundModel`/`groundCmd`
    /// runs a separate entailment pass (proposer ≠ scorer); facts it does not
    /// support are dropped and survivors are stamped `"verified"`.
    ///
    /// The raw text is stored as an **Event** grain (a transcript turn) —
    /// pass `sessionId`/`role` to place it in a conversation thread.
    ///
    /// Returns {"event", "facts"} JSON, plus {"model", "proposed",
    /// "dropped", "verification_status"} when a model ran.
    #[napi(ts_return_type = "Promise<string>")]
    #[allow(clippy::too_many_arguments)] // a flat FFI surface; each knob is a distinct scalar
    pub fn remember(
        &self,
        content: String,
        facts_json: Option<String>,
        observer: Option<String>,
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
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let observer = observer.unwrap_or_else(|| "node".to_string());
        // Runs on the worker pool: a model call is a network round trip, and
        // blocking the event loop across it was the worst case of the old
        // synchronous surface.
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
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
            let event = facade
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
                )?,
            };
            let attribution = areev_store::FactAttribution {
                verification_status: status,
                extractor_model: llm.as_ref().map(|l| l.model()),
            };
            let facts = facade
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
    #[napi(ts_return_type = "Promise<string>")]
    pub fn cal(&self, query: String) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let ex = CalExecutor::new(CalExecutorConfig::default());
            let res = ex.execute(&query, &*facade).map_err(err)?;
            serde_json::to_string(&res.payload_json().map_err(err)?).map_err(err)
        })
    }

    /// Supersession-chain history for (subject, relation), newest first.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn history(
        &self,
        subject: String,
        relation: String,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let versions = facade
                .with_store(|m| m.history(&ns, &subject, &relation))
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
        })
    }

    /// Reverse provenance: grains distilled from `sourceHash` (their
    /// `derived_from`), newest first, as a JSON list string.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn provenance(&self, source_hash: String) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let h = source_hash.strip_prefix("sha256:").unwrap_or(&source_hash);
            let parent = parse_hash(h)?;
            let kids = facade
                .with_store(|m| m.grains_derived_from(&parent))
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
        })
    }

    /// Advise-mode novelty check: nearest existing grains to `text`, optionally
    /// scoped to (subject, relation), as a JSON list of {hash, similarity},
    /// most similar first. Requires an installed embedder; never writes.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn nearest(
        &self,
        text: String,
        subject: Option<String>,
        relation: Option<String>,
        k: Option<u32>,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let k = k.unwrap_or(5) as usize;
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let matches = facade
                .with_store(|m| {
                    m.nearest_semantic(&ns, subject.as_deref(), relation.as_deref(), &text, k)
                })
                // Name the API the caller is holding, not the CLI's flag —
                // this package ships no `areev` binary. Mirrors areev-py.
                .map_err(|e| match e {
                    AreevError::Validation(msg)
                        if msg.contains("requires an installed embedder") =>
                    {
                        err(AreevError::Validation(
                            "nearest() requires an embedder; install one with \
                             setEmbedderCommand(cmd)"
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
        })
    }

    /// Store bytes in the content-addressed blob store; resolves to the
    /// `cas://sha256:<hex>` URI. Idempotent — the address IS the content.
    ///
    /// Bytes in, URI out: the "JSON strings out" convention covers *structured*
    /// results, and a blob is neither structured nor safely representable as
    /// one — base64 through JSON would inflate every payload by a third and
    /// lose the streaming property the CAS exists to provide.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn put_blob(&self, data: Uint8Array) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let bytes: Vec<u8> = data.to_vec();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            facade.with_store(|m| m.put_blob(&bytes)).map_err(err)
        })
    }

    /// Fetch blob bytes by `cas://sha256:` URI. The content address is
    /// re-verified on read, so corruption surfaces as an error rather than as
    /// wrong bytes.
    #[napi(ts_return_type = "Promise<Buffer>")]
    pub fn get_blob(&self, uri: String) -> napi::bindgen_prelude::AsyncTask<BufferJob> {
        let slot = self.facade.clone();
        BufferJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let bytes = facade.with_store(|m| m.get_blob(&uri)).map_err(err)?;
            Ok(bytes.into())
        })
    }

    /// Store statistics as JSON.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn stats(&self) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let s = facade.with_store(|m| m.stats()).map_err(err)?;
            Ok(json!({
                "grains": s.grains, "current": s.current, "triples": s.triples,
                "terms": s.terms, "ops": s.ops, "events_indexed": s.events_indexed,
            })
            .to_string())
        })
    }

    /// Incremental backup to a bundle file. Returns last_op_seq cursor.
    #[napi(ts_return_type = "Promise<number>")]
    pub fn bundle(
        &self,
        path: String,
        since: Option<i64>,
    ) -> napi::bindgen_prelude::AsyncTask<I64Job> {
        let slot = self.facade.clone();
        I64Job::spawn(move || {
            let facade = take_facade(&slot)?;
            let st = facade
                .with_store(|m| m.bundle_since(since.unwrap_or(0), &path))
                .map_err(err)?;
            Ok(st.last_op_seq)
        })
    }

    /// Apply a bundle (fast-forward, idempotent). Returns ops applied.
    #[napi(ts_return_type = "Promise<number>")]
    pub fn import_bundle(&self, path: String) -> napi::bindgen_prelude::AsyncTask<U32Job> {
        let slot = self.facade.clone();
        U32Job::spawn(move || {
            let facade = take_facade(&slot)?;
            let st = facade.with_store(|m| m.import_bundle(&path)).map_err(err)?;
            Ok(st.applied as u32)
        })
    }

    /// Integrity + content-address verification. Throws on failure.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn verify(&self) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let r = facade.with_store(|m| m.verify()).map_err(err)?;
            if r.integrity != "ok" || r.hash_mismatches > 0 || r.undecodable > 0 {
                return Err(err(AreevError::Storage(format!(
                    "verification failed: integrity={} mismatches={} undecodable={}",
                    r.integrity, r.hash_mismatches, r.undecodable
                ))));
            }
            Ok(json!({"integrity": r.integrity, "grains": r.grains}).to_string())
        })
    }

    /// Bounded k-hop walk over the entity graph.
    ///
    /// `relations` is comma-separated. `direction` is out|in|both — in/both use
    /// the reverse index, which only covers relations the file declares
    /// entity-valued, so they find nothing for relations outside that set.
    ///
    /// Argument validation happens inside the task so a bad `direction` or an
    /// empty relation list *rejects* the promise, matching every other method
    /// here — throwing synchronously would contradict the `Promise<string>`
    /// signature napi generates.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn related(
        &self,
        start: String,
        relations: String,
        direction: Option<String>,
        depth: Option<u32>,
        limit: Option<u32>,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let depth = depth.unwrap_or(2) as usize;
        let limit = limit.unwrap_or(64) as usize;
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let rels = parse_relations(&relations);
            if rels.is_empty() {
                return Err(napi::Error::from_reason(
                    "relations must name at least one relation",
                ));
            }
            let dir = Direction::parse(direction.as_deref().unwrap_or("out")).ok_or_else(|| {
                napi::Error::from_reason("direction must be one of: out, in, both")
            })?;
            let refs: Vec<&str> = rels.iter().map(String::as_str).collect();
            let reached = facade
                .with_store(|m| m.related(&ns, &start, &refs, dir, depth, limit))
                .map_err(err)?;
            Ok(json!({"start": start, "reached": reached}).to_string())
        })
    }

    /// As-of read on two axes: `world` = what was true at `at`,
    /// `knowledge` = what the agent knew at `at`. `at` is epoch milliseconds.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn entity_at(
        &self,
        subject: String,
        relation: String,
        at: i64,
        axis: Option<String>,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let ax = Axis::parse(axis.as_deref().unwrap_or("world"))
                .ok_or_else(|| napi::Error::from_reason("axis must be one of: world, knowledge"))?;
            let found = facade
                .with_store(|m| m.entity_at(&ns, &subject, &relation, at, ax))
                .map_err(err)?;
            Ok(match found {
                Some(g) => json!({"found": true, "grain": g}).to_string(),
                None => json!({"found": false}).to_string(),
            })
        })
    }

    /// What a run recorded, and what it produced downstream.
    ///
    /// Returns `{run_id, trace, produced}` — `trace` is the run's own grains,
    /// `produced` is what was derived from them and is not itself part of the
    /// run. This is the query that crosses from execution history into
    /// semantic memory.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn run_trace(
        &self,
        run_id: String,
        limit: Option<u32>,
        include_yield: Option<bool>,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let limit = limit.unwrap_or(64) as usize;
        let want_yield = include_yield.unwrap_or(true);
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let (trace, produced) = facade
                .with_store(|m| {
                    let t = m.run_trace(&ns, &run_id, limit)?;
                    let p = if want_yield {
                        m.run_yield(&ns, &run_id, limit)?
                    } else {
                        Vec::new()
                    };
                    Ok::<_, areev_core::error::AreevError>((t, p))
                })
                .map_err(err)?;
            Ok(json!({"run_id": run_id, "trace": trace, "produced": produced}).to_string())
        })
    }

    /// One page of a run's grains, **oldest first**, from an exclusive
    /// `afterSeq` cursor — the complete journal read (`runTrace` clamps at
    /// 1024 with no cursor). Returns `{run_id, entries: [{seq, grain}...],
    /// next_after_seq}`; `next_after_seq` is null when exhausted.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn run_grains(
        &self,
        run_id: String,
        after_seq: Option<i64>,
        limit: Option<u32>,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let after_seq = after_seq.unwrap_or(0);
        let limit = limit.unwrap_or(500) as usize;
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let page = facade
                .with_store(|m| m.run_grains(&ns, &run_id, after_seq, limit))
                .map_err(err)?;
            let exhausted = page.len() < limit.clamp(1, 1024);
            let next = (!exhausted).then(|| page.last().map(|(s, _)| *s)).flatten();
            let entries: Vec<serde_json::Value> = page
                .into_iter()
                .map(|(seq, g)| json!({"seq": seq, "grain": g}))
                .collect();
            Ok(json!({"run_id": run_id, "entries": entries, "next_after_seq": next}).to_string())
        })
    }

    /// Vector-in nearest-neighbour search: cosine top-k against a
    /// caller-supplied query vector — no embedder needed. Same
    /// `[{hash, similarity}]` shape as `nearest()`; dimension is checked
    /// against the file's declared embedding provenance.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn nearest_vector(
        &self,
        vector: Vec<f64>,
        subject: Option<String>,
        relation: Option<String>,
        k: Option<u32>,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let k = k.unwrap_or(5) as usize;
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let vec32: Vec<f32> = vector.iter().map(|v| *v as f32).collect();
            let matches = facade
                .with_store(|m| {
                    m.nearest_vector(&ns, subject.as_deref(), relation.as_deref(), &vec32, k)
                })
                .map_err(err)?;
            let out: Vec<serde_json::Value> = matches
                .iter()
                .map(|(h, sim)| json!({"hash": h.to_hex(), "similarity": sim}))
                .collect();
            serde_json::to_string(&out).map_err(|e| err(areev_core::error::AreevError::Internal(e.to_string())))
        })
    }

    /// Install or replace the indexed embedding for a stored grain — the
    /// write half of the external-embedding seam. The vector never enters the
    /// grain blob, so the hash is unchanged. Bundles carry blobs, not index
    /// rows: replicas must have vectors re-supplied.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn add_embedding(
        &self,
        hash: String,
        vector: Vec<f64>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let h = Hash::from_hex(&hash).map_err(err)?;
            let vec32: Vec<f32> = vector.iter().map(|v| *v as f32).collect();
            facade
                .with_store(|m| m.set_grain_embedding(&h, &vec32))
                .map_err(err)?;
            Ok(hash)
        })
    }

    /// The file's declared embedding provenance as `{model, dim}` JSON, or
    /// `null` when the file has never seen a vector.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn declared_embedding(&self) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let d = facade
                .with_store(|m| m.declared_embedding().map(|(m2, d)| (m2.to_string(), d)));
            Ok(match d {
                Some((model, dim)) => json!({"model": model, "dim": dim}).to_string(),
                None => "null".to_string(),
            })
        })
    }

    /// Which runs produced or refined this grain — the reverse join.
    ///
    /// Runs that merely *read* the grain are not recorded: a read leaves no
    /// grain behind, so nothing in an append-only store can attest to it.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn runs_touching(
        &self,
        hash: String,
        depth: Option<u32>,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let depth = depth.unwrap_or(4) as usize;
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let h = parse_hash(&hash)?;
            let runs = facade
                .with_store(|m| m.runs_touching(&ns, &h, depth))
                .map_err(err)?;
            Ok(json!({"hash": h.to_hex(), "runs": runs}).to_string())
        })
    }

    /// Execution records for a workflow: which grains ran which of its nodes.
    ///
    /// A Workflow grain is immutable, so runs point at the plan rather than
    /// mutating it — retries show up as several records for one node.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn step_actions(
        &self,
        workflow: String,
        node: Option<String>,
        limit: Option<u32>,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let limit = limit.unwrap_or(64) as usize;
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let wf = parse_hash(&workflow)?;
            let rows = facade
                .with_store(|m| m.step_actions(&ns, &wf, node.as_deref(), limit))
                .map_err(err)?;
            let steps: Vec<serde_json::Value> = rows
                .into_iter()
                .map(|(n, h)| json!({"node": n, "hash": h.to_hex()}))
                .collect();
            Ok(json!({"workflow": wf.to_hex(), "steps": steps}).to_string())
        })
    }

    // ── Areev Loop: the governed self-improvement loop (§6.6) ────────────────────

    /// Record a tool call as a Tool grain — the flagship analyzer's food.
    ///
    /// `callId` is the invocation's own id (the provider's `tool_call_id`),
    /// stored on the grain and queryable — the correlation key back to the LLM
    /// transcript. Omitted, one is synthesized. Either way each call is its own
    /// occurrence, which is what makes a tool that failed five times read as
    /// five failures; recording is append-only, never de-duplicating.
    /// Wave-0 extension mirrored from Python: `runId` correlates to a run,
    /// `workflowHash` + `nodeId` (both or neither) write the `mg:step_action`
    /// link, and `status`/`failureCause`/`executorKind`/`correlationId` carry
    /// the async lifecycle with strict enum validation.
    #[napi(ts_return_type = "Promise<string>")]
    #[allow(clippy::too_many_arguments)]
    pub fn record_tool_call(
        &self,
        name: String,
        result: String,
        is_error: Option<bool>,
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
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = self.ns.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            Ok(facade
                .record_tool_call(
                    &ns,
                    &name,
                    input.as_deref(),
                    &result,
                    is_error.unwrap_or(false),
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
                .map_err(err)?
                .to_hex())
        })
    }

    /// Persist a content-addressed harness config and the run -> config link.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn record_run_manifest(
        &self,
        run_id: String,
        config: String,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let (config_hash, link_hash) = facade
                .record_run_manifest(&run_id, &config)
                .map_err(err)?;
            Ok(json!({
                "config_hash": config_hash.to_hex(),
                "link_hash": link_hash.to_hex(),
            })
            .to_string())
        })
    }

    /// Run one analysis pass. Bare it never gates. `fullSweep` re-analyzes
    /// the whole memory (`areev loop reflect` semantics); `policy` is a path
    /// to a host `loop-policy.json` — the only way auto-apply is granted
    /// from the bindings. Returns run-outcome JSON.
    #[napi(ts_return_type = "Promise<string>")]
    #[allow(clippy::too_many_arguments)] // a flat FFI surface; each knob is a distinct scalar
    pub fn loop_run(
        &self,
        min_new: Option<u32>,
        min_new_errors: Option<u32>,
        if_stale: Option<String>,
        model: Option<String>,
        llm_cmd: Option<String>,
        ground_model: Option<String>,
        ground_cmd: Option<String>,
        analyzer_cmd: Option<String>,
        full_sweep: Option<bool>,
        policy: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let opts = RunOptions {
            min_new: min_new.map(|n| n as u64),
            min_new_errors: min_new_errors.map(|n| n as u64),
            if_stale_ms: if_stale.as_deref().and_then(parse_duration_ms),
            namespaces: Vec::new(),
            full_sweep: full_sweep.unwrap_or(false),
            // The handle's actor — the same identity review/apply stamp — so
            // the trigger of an LLM/external finding cannot approve it.
            triggering_actor: Some(self.actor.clone()),
        };
        // The longest call on this surface — a sweep plus, optionally, several
        // LLM round trips. Blocking the event loop across that was the worst
        // case of the old synchronous surface.
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            // Optional verified LLM reflection: `model` ("claude-sonnet", key from
            // the env) attaches a built-in HTTP backend; `llmCmd` a subprocess.
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
            let mut sub = BorrowedSubstrate::new(&facade);
            let res = engine.run(&mut sub, &opts, now_ms()).map_err(err)?;
            serde_json::to_string(&res).map_err(err)
        })
    }

    /// List recommendations. `filter` is optional JSON, e.g. `{"status":
    /// "pending"}`; `{"status":"all"}` clears the filter. JSON list.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn recommendations(
        &self,
        filter: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        // `{"status":"all"}` clears the filter; a named status selects it;
        // anything else (including no filter) defaults to pending.
        //
        // This was one chain ending in `.or(Some(Pending))`, so `"all"`
        // filtered itself to `None` and the pending default was put straight
        // back — `"all"` behaved as `"pending"`, and an applied recommendation
        // was missing from a list that promised every status.
        let requested = filter
            .and_then(|f| serde_json::from_str::<serde_json::Value>(&f).ok())
            .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(str::to_string));
        let status = match requested.as_deref() {
            Some("all") => None,
            Some(s) => match status_from_str(s) {
                Some(st) => Some(st),
                None => {
                    let bad = s.to_string();
                    return StringJob::spawn(move || {
                        Err(err(AreevError::Validation(format!(
                            "unknown status {bad:?} — expected one of: all, pending, \
                             approved, applied, rejected, rolled_back, expired"
                        ))))
                    });
                }
            },
            None => Some(RecStatus::Pending),
        };
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
        let sub = BorrowedSubstrate::new(&facade);
        let recs = Engine::with_builtins().recommendations(&sub, status).map_err(err)?;
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
        })
    }

    /// Approve and apply a recommendation in one audited step (§6.6). The
    /// `because` reason is mandatory. A refused apply leaves the recommendation
    /// **pending**, so it can still be dismissed.
    ///
    /// `scopes` is a comma-separated subset of `read,write,review,apply,admin`;
    /// omit it for all scopes.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn apply_recommendation(
        &self,
        hash: String,
        because: String,
        allow_destructive: Option<bool>,
        scopes: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let actor = self.actor.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let mut sub = BorrowedSubstrate::new(&facade);
            let engine = Engine::with_builtins();
            let now = now_ms();
            let scopes = parse_scopes(scopes.as_deref())?;
            let allow_destructive = allow_destructive.unwrap_or(false);
            // Ask before approving. The approval is a real state transition and
            // `approved` has no exit but `applied` or `expired`, so recording it
            // first and then hitting the destructive gate stranded the
            // recommendation where the reviewer could no longer reject it.
            engine
                .preflight_apply(&sub, &hash, &scopes, allow_destructive)
                .map_err(err)?;
            engine
                .review(&mut sub, &hash, Decision::Approve, &actor, ObserverType::Human, &scopes, &because, now)
                .map_err(err)?;
            let applied = engine
                .apply(&mut sub, &hash, &actor, ObserverType::Human, &scopes, &because, allow_destructive, now)
                .map_err(err)?;
            Ok(json!({"hash": hash, "rollbackable": applied.rollbackable}).to_string())
        })
    }

    /// Approve a recommendation **without** applying it — the two-person flow
    /// the CLI's separate `approve` verb enables, so a supervising agent can
    /// approve for a human to apply later.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn approve_recommendation(
        &self,
        hash: String,
        because: String,
        scopes: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let actor = self.actor.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let mut sub = BorrowedSubstrate::new(&facade);
            let scopes = parse_scopes(scopes.as_deref())?;
            Engine::with_builtins()
                .review(&mut sub, &hash, Decision::Approve, &actor, ObserverType::Human, &scopes, &because, now_ms())
                .map_err(err)?;
            Ok(json!({"hash": hash, "status": "approved"}).to_string())
        })
    }

    /// Health snapshot of the loop: when it last ran, how much is un-analyzed
    /// since, the queue counts, and whether it looks stalled. Parity with bare
    /// `areev loop` and `GET /api/loop/health`.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn loop_health(&self) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let sub = BorrowedSubstrate::new(&facade);
            let health = Engine::with_builtins().health(&sub, now_ms()).map_err(err)?;
            serde_json::to_string(&health).map_err(err)
        })
    }

    /// The analyzer roster: id, whether it is enabled, and its settings.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn loop_analyzers(&self) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let sub = BorrowedSubstrate::new(&facade);
            let list = Engine::with_builtins().analyzer_settings(&sub).map_err(err)?;
            serde_json::to_string(&list).map_err(err)
        })
    }

    /// Enable/disable one analyzer, or set its parameters — reachable from the
    /// console's Setup tab (`POST /api/loop/config`) but not, until now, from
    /// the bindings. `paramsJson` is an optional JSON object of overrides.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn set_analyzer_config(
        &self,
        analyzer_id: String,
        enabled: Option<bool>,
        params_json: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let params = match params_json {
                Some(ref s) => serde_json::from_str(s).map_err(err)?,
                None => None,
            };
            let update = areev_loop::AnalyzerConfigUpdate { enabled, params, ..Default::default() };
            let mut sub = BorrowedSubstrate::new(&facade);
            let cfg = Engine::with_builtins()
                .set_analyzer_config(&mut sub, &analyzer_id, update, &ScopeSet::all())
                .map_err(err)?;
            serde_json::to_string(&cfg).map_err(err)
        })
    }

    /// Detect sensitive spans in free text with the built-in Tier-0 chain
    /// (docs/anonymization-proposal.md P0). Pure text — touches no grains.
    /// Returns JSON `{"text": <nfc text>, "detections": [...]}`; offsets are
    /// UTF-8 bytes into the returned normalized text.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn scan_text(
        &self,
        text: String,
        policy_json: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            facade.scan_text(&text, policy_json.as_deref()).map_err(err)
        })
    }

    /// Pseudonymize free text: detected spans become typed placeholders
    /// (`[PERSON_1]`) and the reversible spans' placeholder→value map is
    /// returned to the caller. Returns JSON
    /// `{"text", "mapping", "mapping_id", "replaced"}`. `keyHex` keys the
    /// `mapping_id` derivation; without it, don't ship the id anywhere the
    /// mapping doesn't also travel.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn anonymize_text(
        &self,
        text: String,
        policy_json: Option<String>,
        key_hex: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            facade
                .anonymize_text(&text, policy_json.as_deref(), key_hex.as_deref())
                .map_err(err)
        })
    }

    /// Restore originals in an LLM response: replaces exact placeholder
    /// tokens using `mappingJson` (object of placeholder → value). Returns
    /// JSON `{"text", "replaced", "unmatched"}` — unmatched tokens are left
    /// intact and reported, never guessed.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn rehydrate_text(
        &self,
        text: String,
        mapping_json: String,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            facade.rehydrate_text(&text, &mapping_json).map_err(err)
        })
    }

    /// Declare (or replace) one namespace's anonymization policy — an
    /// `anon:<ns>` file-truth that replicates write-if-absent and stamps
    /// min_reader_version. Egress reads of that namespace are pseudonymized
    /// from now on.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn set_anon_policy(
        &self,
        ns: String,
        policy_json: String,
    ) -> napi::bindgen_prelude::AsyncTask<UnitJob> {
        let slot = self.facade.clone();
        UnitJob::spawn(move || {
            let facade = take_facade(&slot)?;
            facade.with_store(|m| m.set_anon_policy(&ns, &policy_json)).map_err(err)
        })
    }

    /// Remove one namespace's anonymization policy (missing is not an error).
    #[napi(ts_return_type = "Promise<void>")]
    pub fn clear_anon_policy(&self, ns: String) -> napi::bindgen_prelude::AsyncTask<UnitJob> {
        let slot = self.facade.clone();
        UnitJob::spawn(move || {
            let facade = take_facade(&slot)?;
            facade.with_store(|m| m.clear_anon_policy(&ns)).map_err(err)
        })
    }

    /// All declared anonymization policies as JSON `[{ns, policy}]`. An
    /// unreadable row is a hard error, not a skip (fail-closed, D3).
    #[napi(ts_return_type = "Promise<string>")]
    pub fn anon_policies(&self) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let policies = facade.with_store(|m| m.anon_policies()).map_err(err)?;
            let rows: Vec<serde_json::Value> = policies
                .into_iter()
                .map(|(ns, p)| serde_json::json!({"ns": ns, "policy": p}))
                .collect();
            serde_json::to_string(&rows).map_err(err)
        })
    }

    /// This process's live pseudonym mappings as JSON
    /// `[{ns, mapping_id, mapping}]` — the in-process rehydration custody
    /// (D5); mappings never ride MCP/server payloads.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn anon_mappings(&self) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let maps = facade.with_store(|m| m.anon_mappings()).map_err(err)?;
            let rows: Vec<serde_json::Value> = maps
                .into_iter()
                .map(|(ns, id, mapping)| {
                    serde_json::json!({"ns": ns, "mapping_id": id, "mapping": mapping})
                })
                .collect();
            serde_json::to_string(&rows).map_err(err)
        })
    }

    /// Host cap (never persisted): force egress anonymization on for every
    /// namespace without a declared policy. Can never weaken a declared one.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn set_anonymize_egress_floor(
        &self,
        on: bool,
    ) -> napi::bindgen_prelude::AsyncTask<UnitJob> {
        let slot = self.facade.clone();
        UnitJob::spawn(move || {
            let facade = take_facade(&slot)?;
            facade.with_store(|m| m.set_anonymize_egress_floor(on));
            Ok(())
        })
    }

    /// Install a Tier-1 NER detector over the command seam (probed at
    /// install; a broken command errors here, not at the first read).
    #[napi(ts_return_type = "Promise<void>")]
    pub fn set_anonymizer_command(
        &self,
        cmd: String,
    ) -> napi::bindgen_prelude::AsyncTask<UnitJob> {
        let slot = self.facade.clone();
        UnitJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let backend = areev_store::CommandAnonymize::new(&cmd).map_err(err)?;
            facade.with_store(|m| {
                m.set_anonymizer(Box::new(backend));
            });
            Ok(())
        })
    }

    /// Reverse-lookup placeholder tokens (admin-gated, Tier-2 audited by
    /// fingerprint). `tokensJson` is a JSON array of placeholder strings;
    /// returns JSON `{"revealed": {token: value | null}}`.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn reveal_tokens(
        &self,
        ns: String,
        tokens_json: String,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let tokens: Vec<String> = serde_json::from_str(&tokens_json).map_err(err)?;
            facade.reveal_tokens(&ns, &tokens).map_err(err)
        })
    }

    /// Reject a recommendation with a reason (library-friendly `reject`).
    #[napi(ts_return_type = "Promise<string>")]
    pub fn dismiss_recommendation(
        &self,
        hash: String,
        why: String,
        scopes: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let actor = self.actor.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let mut sub = BorrowedSubstrate::new(&facade);
            let scopes = parse_scopes(scopes.as_deref())?;
            Engine::with_builtins()
                .review(&mut sub, &hash, Decision::Reject, &actor, ObserverType::Human, &scopes, &why, now_ms())
                .map_err(err)?;
            Ok(json!({"hash": hash, "status": "rejected"}).to_string())
        })
    }

    /// Roll back an applied recommendation (retracts the grains it created).
    /// Mandatory reason; fails for non-rollbackable applies (FORGET has no
    /// inverse). Parity with `areev loop rollback`.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn rollback_recommendation(
        &self,
        hash: String,
        because: String,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let actor = self.actor.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let mut sub = BorrowedSubstrate::new(&facade);
            Engine::with_builtins()
                .rollback(&mut sub, &hash, &actor, ObserverType::Human, &ScopeSet::all(), &because, now_ms())
                .map_err(err)?;
            Ok(json!({"hash": hash, "status": "rolled_back"}).to_string())
        })
    }

    /// Measured outcomes of applied recommendations — the Verify gate's
    /// record (`held` / `regressed` per checkpoint). JSON list, parity with
    /// `areev loop outcomes`.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn loop_outcomes(&self) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let sub = BorrowedSubstrate::new(&facade);
            let outs = Engine::with_builtins().outcomes(&sub).map_err(err)?;
            serde_json::to_string(&outs).map_err(err)
        })
    }
    // ---- the `areev run` runtime (Wave 5 parity; closes the JS deviation) --
    // Same convention as everything above: scalars in, JSON strings out.
    // Host tools execute through `toolCmd` (subprocess seam: input JSON on
    // stdin, result JSON on stdout); without one, host tools fail loudly.

    /// Start a governed run. Returns the session JSON:
    /// `{"finished": …}` or `{"parked": envelope}`.
    #[napi(ts_return_type = "Promise<string>")]
    #[allow(clippy::too_many_arguments)]
    pub fn run_start(
        &self,
        workflow: String,
        run_id: String,
        input_json: Option<String>,
        tool_cmd: Option<String>,
        max_tokens: Option<i64>,
        max_usd_micros: Option<i64>,
        max_wall_ms: Option<i64>,
        ask_ttl_sec: Option<i64>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = self.ns.clone();
        let actor = self.actor.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let input: serde_json::Value = match input_json {
                Some(raw) => serde_json::from_str(&raw)
                    .map_err(|e| err(AreevError::Validation(format!("inputJson: {e}"))))?,
                None => json!({}),
            };
            let h = areev_core::error::Hash::from_hex(&workflow).map_err(err)?;
            let runner = js_runner(facade, ns, actor, tool_cmd);
            let opts = js_run_options(max_tokens, max_usd_micros, max_wall_ms, ask_ttl_sec)?;
            let session = runner.start(&h, &run_id, input, &opts).map_err(run_err)?;
            Ok(run_session_json(session).to_string())
        })
    }

    /// Resume a parked/interrupted run from its latest checkpoint.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn run_resume(
        &self,
        run_id: String,
        tool_cmd: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = self.ns.clone();
        let actor = self.actor.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let runner = js_runner(facade, ns, actor, tool_cmd);
            let opts = js_run_options(None, None, None, None)?;
            let session = runner.resume(&run_id, &opts).map_err(run_err)?;
            Ok(run_session_json(session).to_string())
        })
    }

    /// Answer a pending Client ask. `responder` is REQUIRED — approval
    /// separation of duties (responder ≠ triggering principal) is structural.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn run_respond(
        &self,
        run_id: String,
        tool_call_id: String,
        result_json: String,
        responder: String,
        is_error: Option<bool>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = self.ns.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let result: serde_json::Value = serde_json::from_str(&result_json)
                .map_err(|e| err(AreevError::Validation(format!("resultJson: {e}"))))?;
            let runner = js_runner(facade, ns, responder.clone(), None);
            runner
                .respond(&run_id, &tool_call_id, result, is_error.unwrap_or(false), &responder)
                .map_err(run_err)?;
            Ok(json!({"responded": tool_call_id, "run_id": run_id}).to_string())
        })
    }

    /// Write the kill-switch marker (the lowest-privilege run verb).
    #[napi(ts_return_type = "Promise<string>")]
    pub fn run_cancel(
        &self,
        run_id: String,
        because: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = self.ns.clone();
        let actor = self.actor.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let runner = js_runner(facade, ns, actor.clone(), None);
            runner
                .cancel(&run_id, &actor, because.as_deref().unwrap_or("canceled"))
                .map_err(run_err)?;
            Ok(json!({"canceled": run_id}).to_string())
        })
    }

    /// Journal-consistent replay; writes nothing. JSON report.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn run_verify(&self, run_id: String) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = self.ns.clone();
        let actor = self.actor.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let runner = js_runner(facade, ns, actor, None);
            let report = runner.verify(&run_id).map_err(run_err)?;
            serde_json::to_string(&report).map_err(err)
        })
    }

    /// Shadow evaluation over journaled runs — zero effect dispatches.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn run_shadow(&self, run_ids: Vec<String>) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = self.ns.clone();
        let actor = self.actor.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let runner = js_runner(facade, ns, actor, None);
            let report = runner.shadow_eval(&run_ids);
            serde_json::to_string(&report).map_err(err)
        })
    }

    /// §5.4 time-travel fork / migration: seed a new run from a base run's
    /// checkpoint (optionally at a specific superstep, optionally onto a new
    /// plan hash). Returns the seed checkpoint hash (hex).
    #[napi(ts_return_type = "Promise<string>")]
    pub fn run_fork(
        &self,
        base_run_id: String,
        new_run_id: String,
        at_superstep: Option<i64>,
        plan: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = self.ns.clone();
        let actor = self.actor.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let at = match at_superstep {
                None => None,
                Some(x) => Some(u64::try_from(x).map_err(|_| {
                    err(AreevError::Validation(format!(
                        "atSuperstep must be >= 0 (got {x})"
                    )))
                })?),
            };
            let plan_hash = match plan {
                Some(p) => Some(areev_core::error::Hash::from_hex(&p).map_err(err)?),
                None => None,
            };
            let runner = js_runner(facade, ns, actor, None);
            let opts = js_run_options(None, None, None, None)?;
            let seed = runner
                .fork(&base_run_id, at, &new_run_id, plan_hash.as_ref(), &opts)
                .map_err(run_err)?;
            Ok(seed.to_hex())
        })
    }

    /// Op-log cursor read — the change feed the audit/evidence story rides.
    /// Returns `[{op_seq, hlc, op, hash}...]`, ascending; pass the last
    /// `op_seq` back as the next cursor. `hlc` is a STRING: HLC values
    /// exceed 2^53 and would silently lose bits in `JSON.parse`.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn changes_since(
        &self,
        after_op_seq: Option<i64>,
        limit: Option<u32>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let after = after_op_seq.unwrap_or(0);
        let limit = (limit.unwrap_or(500) as usize).clamp(1, 10_000);
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let rows = facade
                .with_store(|m| m.changes_since(after, limit))
                .map_err(err)?;
            let out: Vec<serde_json::Value> = rows
                .into_iter()
                .map(|r| {
                    json!({"op_seq": r.op_seq, "hlc": r.hlc.to_string(), "op": r.op, "hash": r.hash.to_hex()})
                })
                .collect();
            serde_json::to_string(&out).map_err(err)
        })
    }

    /// Recent run ids, newest first. JSON list string.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn run_list(&self, limit: Option<u32>) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = self.ns.clone();
        let actor = self.actor.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let runner = js_runner(facade, ns, actor, None);
            let ids = runner.recent_runs(limit.unwrap_or(20) as usize).map_err(run_err)?;
            serde_json::to_string(&ids).map_err(err)
        })
    }

    /// One run's full picture — manifest, budgets, phase, spend, pending
    /// asks, fork lineage. JSON report; same shape as `areev run inspect`.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn run_inspect(&self, run_id: String) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = self.ns.clone();
        let actor = self.actor.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let runner = js_runner(facade, ns, actor, None);
            let report = runner.inspect(&run_id).map_err(run_err)?;
            serde_json::to_string(&report).map_err(err)
        })
    }

    /// The EU AI Act Article 14 report — measured from the journal, not
    /// asserted. `runId` takes precedence; `plan` (a hex hash) resolves to
    /// that plan's newest run; neither given reports on the newest run
    /// overall. JSON report; same shape as `areev run oversight-report`.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn run_oversight_report(
        &self,
        run_id: Option<String>,
        plan: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = self.ns.clone();
        let actor = self.actor.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let plan_hash = match plan {
                Some(p) => Some(areev_core::error::Hash::from_hex(&p).map_err(err)?),
                None => None,
            };
            let runner = js_runner(facade, ns, actor, None);
            let report = runner
                .oversight_report(run_id.as_deref(), plan_hash.as_ref())
                .map_err(run_err)?;
            serde_json::to_string(&report).map_err(err)
        })
    }
}


/// Map a runtime failure into a napi error (the RUN-Ennn code leads).
fn run_err(e: areev_run::CoreRunError) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

/// The runtime driver over one shared facade (Wave 5 JS parity).
fn js_runner(
    facade: std::sync::Arc<AreevFacade>,
    ns: String,
    principal: String,
    tool_cmd: Option<String>,
) -> areev_run::Runner {
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
                        detail: format!("no toolCmd given; cannot execute host tool '{tool_name}'"),
                    }
                }
            }
            std::sync::Arc::new(NoExec)
        }
    };
    areev_run::Runner {
        facade,
        clock: std::sync::Arc::new(areev_run::SystemClock),
        executor,
        llm: None,
        observer: None,
        ns,
        principal,
    }
}

fn js_run_options(
    max_tokens: Option<i64>,
    max_usd_micros: Option<i64>,
    max_wall_ms: Option<i64>,
    ask_ttl_sec: Option<i64>,
) -> napi::Result<areev_run::RunOptions> {
    // A negative budget is a caller bug — refusing beats silently treating
    // it as "unlimited" (the failure mode a budget exists to prevent).
    let u = |name: &str, v: Option<i64>| -> napi::Result<Option<u64>> {
        match v {
            None => Ok(None),
            Some(x) => u64::try_from(x).map(Some).map_err(|_| {
                err(AreevError::Validation(format!("{name} must be >= 0 (got {x})")))
            }),
        }
    };
    Ok(areev_run::RunOptions {
        budgets: areev_run::BudgetsSpec {
            max_supersteps: None,
            max_tokens: u("maxTokens", max_tokens)?,
            max_usd_micros: u("maxUsdMicros", max_usd_micros)?,
            max_wall_ms: u("maxWallMs", max_wall_ms)?,
            max_storage_bytes: None,
        },
        ask_ttl_sec,
        workers: 4,
        on_dangling: Default::default(),
        llm_max_tokens: None,
        inject_crash: None,
    })
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
