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

/// Split a namespace argument that may be a single name or a comma list
/// (#303, #307). Trimmed, non-empty — so `"a, b"` and `"a,b"` agree, and a
/// trailing comma is not a namespace.
fn split_ns_list(ns: &str) -> Vec<String> {
    ns.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn err<E: std::fmt::Display>(e: E) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}


/// A host-supplied 32-byte anonymization root, given as 64 hex characters.
///
/// The FFI convention is scalars in, so the key arrives as hex rather than as
/// a Buffer. It is the HKDF root for the session/memory/vault subkeys, it is
/// never persisted, and rotating it is a crypto-erasure of the mapping table —
/// so a malformed or wrong-length value has to fail here, loudly, at open.
/// Silently deriving a *different* token space would look like working
/// software right up until a rehydrate came back empty.
fn parse_anon_key(hex: &str) -> napi::Result<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 {
        return Err(err(format!(
            "anonKey must be 64 hex characters (a 32-byte key); got {} characters",
            hex.len()
        )));
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let pair = &hex[i * 2..i * 2 + 2];
        *byte = u8::from_str_radix(pair, 16)
            .map_err(|_| err(format!("anonKey is not hex: {pair:?} at byte {i}")))?;
    }
    Ok(out)
}

/// Resolve the **tool-calling** model an abstract workflow node needs, the same
/// spec grammar and env-key discipline as the CLI's `areev run --model`.
///
/// A different seam from [`resolve_llm`]: the loop's reflection wants a
/// completion backend, while the runtime's abstract nodes want a model that can
/// emit tool calls. Without one, a plan carrying an abstract node refuses at
/// load with `RUN-E006` — which is exactly what a binding host used to get,
/// unavoidably, because the binding hard-coded `llm: None`.
fn resolve_toolcall_llm(
    model: Option<String>,
    base_url: Option<String>,
    key_env: Option<String>,
) -> napi::Result<Option<std::sync::Arc<dyn areev_llm::ToolCallLlm>>> {
    match model {
        Some(spec) => Ok(Some(
            areev_llm::resolve_toolcall(&spec, base_url.as_deref(), key_env.as_deref())
                .map_err(err)?,
        )),
        None => Ok(None),
    }
}

/// Build through CAL's one grain builder and validate the schedule, storing
/// nothing.
///
/// `add("trigger", …)` already refuses an *incoherent* declaration, but the
/// cron parse, the UTC-only refusal and the composite gate-vs-members check
/// live in `areev-trigger`, which sits ABOVE `areev-cal` — so the builder
/// itself cannot call them. Running the check through a *validate-only* sink
/// keeps one builder as the source of truth for what a trigger's fields mean,
/// and leaves the actual write to `cal_add`, so a trigger picks up the
/// authorization check, `author_did` attribution and ingress transform every
/// other grain type gets rather than a second, drifting write path.
struct ValidateTriggerOnly;

impl areev_cal::json_build::GrainSink for ValidateTriggerOnly {
    type Out = ();
    fn consume<G: areev_core::types::Grain + Clone + 'static>(
        self,
        grain: &G,
    ) -> areev_core::error::Result<()> {
        if let Some(t) =
            (grain as &dyn std::any::Any).downcast_ref::<areev_core::types::Trigger>()
        {
            areev_trigger::schedule::validate(t)
                .map_err(|e| AreevError::Validation(e.to_string()))?;
        }
        Ok(())
    }
}

/// Bridges the trigger evaluator to the real runtime.
///
/// The duplicate rule is the whole idempotency story: `Runner::start` refuses
/// an existing run id, and that refusal — not a lease, not a lock — is what
/// makes a re-delivered item a skip instead of a second run.
struct RunnerStarter {
    runner: areev_run::Runner,
    opts: areev_run::RunOptions,
}

impl areev_trigger::RunStarter for RunnerStarter {
    fn start(
        &self,
        workflow: &str,
        run_id: &str,
        input: serde_json::Value,
    ) -> areev_trigger::StartResult {
        let hash = match Hash::from_hex(workflow) {
            Ok(h) => h,
            Err(e) => {
                return areev_trigger::StartResult::Failed(format!("workflow {workflow}: {e}"))
            }
        };
        match self.runner.start(&hash, run_id, input, &self.opts) {
            Ok(_) => areev_trigger::StartResult::Started,
            Err(areev_run::CoreRunError::Tainted { why }) if why.contains("already exists") => {
                areev_trigger::StartResult::Duplicate
            }
            Err(e) => areev_trigger::StartResult::Failed(e.to_string()),
        }
    }
}

/// `cal_add`, with the schedule check `add("trigger", …)` used to skip.
///
/// The CAL grain builder refuses an *incoherent* trigger, but the cron parse,
/// the UTC-only rule and the composite gate-vs-members check live in
/// `areev-trigger`, which sits ABOVE `areev-cal` — so the builder cannot call
/// them and every write through it stored declarations that could never fire
/// (#67). Routing BOTH `add` and `triggerAdd` through here means the generic
/// authoring path a host actually reaches for is not the unvalidated one.
///
/// Validate first, store second, and let `cal_add` do the write so a trigger
/// keeps the authorization check, `author_did` attribution and ingress
/// transform every other grain type gets.
fn validated_cal_add(
    facade: &AreevFacade,
    grain_type: &str,
    fields: &serde_json::Map<String, serde_json::Value>,
) -> areev_core::error::Result<Hash> {
    if grain_type == "trigger" {
        areev_cal::json_build::build_grain_from_json(grain_type, fields, ValidateTriggerOnly)?;
    }
    facade.cal_add(grain_type, fields)
}

/// An evaluator that can inspect but not act — no connector, no runtime, no
/// credentials. What `list`, `show`, `status` and `render` want, and the shape
/// that makes "reading cannot fire anything" true by construction rather than
/// by remembering to pass `None` four times.
fn js_read_only_evaluator(
    facade: std::sync::Arc<AreevFacade>,
    ns: &str,
) -> areev_trigger::Evaluator {
    areev_trigger::Evaluator::read_only(
        facade,
        std::sync::Arc::new(areev_trigger::SystemClock),
        ns,
    )
}

/// The host config a connector that is a GRAIN runs under (#185) — the same
/// pin the run path takes, read off the same `JsExecutorPin`, so a host grants
/// an address once and it means one thing.
///
/// `None` when nothing is pinned: a trigger naming a connector Definition then
/// refuses with `TRG-E012` naming the address, rather than falling through to
/// whatever `connectorCmd` happens to be.
fn js_connector_code(pin: &JsExecutorPin, db: &str) -> Option<areev_trigger::ConnectorCode> {
    let allow: Vec<String> = pin
        .allow_executor
        .as_deref()?
        .split(',')
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string)
        .collect();
    if allow.is_empty() {
        return None;
    }
    Some(areev_trigger::ConnectorCode {
        allow,
        cache_dir: pin.executor_cache.as_deref().map(std::path::PathBuf::from),
        sandbox_cmd: pin.sandbox_cmd.clone(),
        timeout: pin.executor_timeout_secs.and_then(|s| u64::try_from(s).ok()).map(|s| {
            if s == 0 {
                None
            } else {
                Some(std::time::Duration::from_secs(s))
            }
        }),
        env: js_tool_env_policy(pin.tool_env.as_deref()),
        db_locator: Some(db.to_string()),
    })
}

/// The acting evaluator. Mirrors the CLI's construction, including the two
/// deliberate `None`s: without a connector a due polling trigger fails loudly
/// (`TRG-E003`) rather than looking healthy while doing nothing, and without a
/// `toolCmd` the pass ingests items and records firings but starts nothing.
#[allow(clippy::too_many_arguments)]
fn js_evaluator(
    facade: std::sync::Arc<AreevFacade>,
    path: &str,
    ns: String,
    db: String,
    principal: String,
    connector_cmd: Option<String>,
    tool_cmd: Option<String>,
    credentials_json: Option<String>,
    llm: Option<std::sync::Arc<dyn areev_llm::ToolCallLlm>>,
    pin: JsExecutorPin,
    egress: JsEgressPin,
    opts: areev_run::RunOptions,
) -> napi::Result<areev_trigger::Evaluator> {
    // A connector IS a tool — JSON in, JSON out, one process per invocation —
    // so there is one subprocess contract to learn and connectors inherit its
    // timeout, output cap and secret scrub.
    // A connector holds the third-party credential more often than a tool
    // does, so `toolEnv` has to reach it too — the CLI applies the same policy
    // here (`trigger_cli.rs`), and `docs/triggers.md` says so.
    let connector_env = js_tool_env_policy(pin.tool_env.as_deref());
    let connector: Option<std::sync::Arc<dyn areev_run::HostToolExecutor>> = connector_cmd
        .clone()
        .or_else(|| tool_cmd.clone())
        .map(|cmd| {
            let ce = areev_run::CommandExecutor::new(&cmd);
            let ce = match connector_env {
                Some(p) => ce.with_env_policy(p),
                None => ce,
            };
            std::sync::Arc::new(ce) as std::sync::Arc<dyn areev_run::HostToolExecutor>
        });

    // A firing gets the runner `runStart` builds, pin included (#90). Gating
    // on `toolCmd` alone was the same reduction one level down: a plan whose
    // nodes are all pinned code, or all abstract, needs no subprocess, and
    // gating on one meant such a plan was ingested, recorded as fired, and
    // never started.
    let can_execute = tool_cmd.is_some() || pin.allow_executor.is_some() || llm.is_some();
    // Built before `pin` is moved into the runner below: the connector's code
    // and a firing's nodes run off ONE pin.
    let connector_code = js_connector_code(&pin, &db);
    // The runs a firing starts get the broker `runStart` would build (#201)
    // — distinct from the connector-poll credentials below.
    let handle = if can_execute { js_egress_handle(path, &egress)? } else { None };
    let starter: Option<std::sync::Arc<dyn areev_trigger::RunStarter>> = can_execute.then(|| {
        std::sync::Arc::new(RunnerStarter {
            runner: js_runner_pinned(
                std::sync::Arc::clone(&facade),
                ns.clone(),
                principal.clone(),
                tool_cmd,
                llm,
                // The trigger surface takes no `onEvent` (#182): a firing
                // starts a real run, so this is a knowable asymmetry with
                // `runStart`, not an oversight.
                pin,
                handle,
                None,
            ),
            opts,
        }) as std::sync::Arc<dyn areev_trigger::RunStarter>
    });

    // Credentials are named here and READ here, so a value never appears in a
    // grain, in the host's arguments, or in the connector's environment.
    // Unlike the CLI, an unset variable is an error rather than a silent drop:
    // a dropped credential surfaces downstream as an unexplained 401 from
    // someone else's API.
    let mut credentials = std::collections::BTreeMap::new();
    if let Some(raw) = credentials_json {
        let map: std::collections::BTreeMap<String, String> = serde_json::from_str(&raw)
            .map_err(|e| err(format!("credentialsJson: expected {{\"name\": \"ENV_VAR|cmd:CMD|vault:PATH#FIELD\"}}: {e}")))?;
        for (name, spec) in map {
            // `@principal` binds a credential to a run principal for its use
            // in a started RUN (#101); this connector-poll path is the
            // trigger's own standing egress, so the owner is dropped here.
            // A `cmd:`/`vault:` spec names a resolver instead of a variable
            // (#113) and is minted at call time inside the broker — the value
            // still never reaches the connector.
            let (source, _owner) = areev_run::CredentialSource::from_spec(&spec)
                .map_err(|e| err(format!("credential {name:?}: {e}")))?;
            credentials.insert(name, source);
        }
    }
    // `credentials` (the CLI's `--credential` spelling) configures the
    // connector too, exactly as one `--credential` flag does both.
    credentials.extend(egress.spec().map_err(err)?.unowned_credentials().map_err(err)?);

    Ok(areev_trigger::Evaluator {
        facade,
        clock: std::sync::Arc::new(areev_trigger::SystemClock),
        connector,
        connector_code,
        starter,
        credentials,
        ns,
        principal,
    })
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

/// `recommendations(filter)`'s `include` option (#348): `"proposal"` (or its
/// alias `"detail"`) adds `action_kind` and the flattened proposal to each
/// row; absent leaves the row shape unchanged. Anything else is refused
/// rather than silently ignored.
fn include_proposal(filter: Option<&serde_json::Value>) -> napi::Result<bool> {
    match filter.and_then(|v| v.get("include")) {
        None | Some(serde_json::Value::Null) => Ok(false),
        Some(v) => match v.as_str() {
            Some("proposal" | "detail") => Ok(true),
            _ => Err(err(format!("unknown include {v} — expected \"proposal\" or \"detail\""))),
        },
    }
}

fn status_from_str(s: &str) -> Option<RecStatus> {
    match s {
        "pending" => Some(RecStatus::Pending),
        "approved" => Some(RecStatus::Approved),
        "rejected" => Some(RecStatus::Rejected),
        "applied" => Some(RecStatus::Applied),
        "rolled_back" => Some(RecStatus::RolledBack),
        "expired" => Some(RecStatus::Expired),
        "withdrawn" => Some(RecStatus::Withdrawn),
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

/// [`areev_run::RunObserver`] over a JS callback `(event: string) => void` —
/// the binding form of the CLI's `--events`, which is literally
/// `eprintln!("{}", serde_json::to_string(ev)?)`. So the payload IS that same
/// line: one §6.10 `RunEvent` as a JSON object with an `"event"` tag, which
/// the caller `JSON.parse`s. No per-language event class, and no
/// `Deserialize` on `RunEvent` — the enum is append-only and every added
/// field is `Option` + `skip_serializing_if`, so a subscriber matches on the
/// tag and a run with no model emits the lines it always did.
///
/// **Why this has to be a threadsafe function.** Three threads are involved:
/// the JS thread the method is called on, the libuv worker the `AsyncTask`
/// body runs on, and the EventBus's OWN delivery thread, which is where
/// `RunObserver::event` fires. A plain `Function` is neither `Send` nor
/// callable off the JS thread, so it could never reach the third one. The
/// TSFN is built synchronously in the method body (which needs the JS
/// thread) and then moved into the job — which is why `runStart` and
/// `runResume` return `napi::Result<AsyncTask<…>>`: building it can fail.
///
/// It is released when this observer drops, and that happens inside the job:
/// `EventBus::drop` drains the queue and joins its worker before
/// `Runner::start` returns.
struct JsEvents {
    tsfn: napi::threadsafe_function::ThreadsafeFunction<String, (), String, napi::Status, false>,
}

impl areev_run::RunObserver for JsEvents {
    fn event(&self, ev: &areev_run::RunEvent) {
        let Ok(line) = serde_json::to_string(ev) else { return };
        // NonBlocking on purpose: events are observational (§6.10 — the
        // journal is byte-identical with no subscriber, a subscriber, and a
        // deliberately slow one), and the bus above already drops the oldest
        // and counts it rather than backpressuring the run.
        let _ = self.tsfn.call(
            line,
            napi::threadsafe_function::ThreadsafeFunctionCallMode::NonBlocking,
        );
    }
}

/// Turn an optional JS callback into the run's §6.10 observer.
///
/// **Call this on the JS thread, before spawning the job** — building the
/// threadsafe function is the part that needs it.
///
/// Attaching one also turns on `TokenChunk` deltas for abstract nodes: the
/// driver only builds a token sink when a bus exists, so model text streams
/// through the same callback with no further plumbing. Those are
/// observational too — the journaled result is the model's final message, not
/// the concatenated deltas.
fn js_observer(
    on_event: Option<napi::bindgen_prelude::Function<String, ()>>,
) -> napi::Result<Option<std::sync::Arc<dyn areev_run::RunObserver>>> {
    match on_event {
        None => Ok(None),
        Some(f) => {
            let tsfn = f.build_threadsafe_function().build()?;
            Ok(Some(std::sync::Arc::new(JsEvents { tsfn })
                as std::sync::Arc<dyn areev_run::RunObserver>))
        }
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
    /// Store call returning a flag (`anonymizeEgressFloor`).
    BoolJob => bool,
    /// Store call returning a count.
    U32Job => u32,
    /// Store call returning an op-log cursor.
    I64Job => i64,
    /// Store call returning raw bytes (`getBlob`). Blobs are binary, so the
    /// JSON-out convention deliberately does not apply to them.
    BufferJob => Buffer,
}

/// Options for `packInstall` (#341). Every field is optional.
#[napi(object)]
#[derive(Default)]
pub struct PackInstallOptions {
    /// The plan hash the deployment expects: some Workflow grain in the pack
    /// must build to it, else `PCK-E002` with nothing written.
    pub expected_hash: Option<String>,
    /// The namespace for grains when neither the grain nor the manifest names
    /// one. Part of those grains' content when it applies.
    pub ns: Option<String>,
    /// Host executor pins, `{ tool: address }` (tool = `tool_name`, pack-local
    /// grain id, or symbolic blob name; address = `<hex>`, `sha256:<hex>` or
    /// `cas://sha256:<hex>`). Checked against the pack's code, never written;
    /// a mismatch or a pin naming no code-carrying tool is `PCK-E005`.
    pub executor_pins: Option<std::collections::HashMap<String, String>>,
    /// Check everything, write nothing.
    pub dry_run: Option<bool>,
}

/// The `DOMAIN-Ennn` code a message leads with, else napi's generic status.
fn leading_code(msg: &str) -> String {
    let tok = msg.split(':').next().unwrap_or("");
    let ok = tok.len() >= 8
        && tok.as_bytes()[..3].iter().all(u8::is_ascii_uppercase)
        && tok[3..].starts_with("-E")
        && tok[5..].bytes().all(|b| b.is_ascii_digit());
    if ok { tok.to_string() } else { "GenericFailure".to_string() }
}

/// A pack job (#341): like [`StringJob`], but a refusal rejects with a JS
/// `Error` whose `code` is the typed cause (`PCK-E002`, `AUT-E001`, …) rather
/// than napi's generic `GenericFailure` — so a host branches on
/// `err.code`, not on message text.
pub struct PackJob {
    work: Option<Box<PackWork>>,
}

/// `Ok(report JSON)` or `Err((code, message))`.
type PackWork = dyn FnOnce() -> Result<String, (String, String)> + Send;

impl PackJob {
    fn spawn(
        work: impl FnOnce() -> Result<String, (String, String)> + Send + 'static,
    ) -> napi::bindgen_prelude::AsyncTask<Self> {
        napi::bindgen_prelude::AsyncTask::new(PackJob { work: Some(Box::new(work)) })
    }
}

impl napi::Task for PackJob {
    type Output = Result<String, (String, String)>;
    type JsValue = String;

    fn compute(&mut self) -> napi::Result<Self::Output> {
        match self.work.take() {
            Some(work) => Ok(work()),
            None => Err(err("store job polled twice")),
        }
    }

    fn resolve(&mut self, env: napi::Env, output: Self::Output) -> napi::Result<String> {
        use napi::bindgen_prelude::JsObjectValue;
        use napi::JsValue;
        match output {
            Ok(s) => Ok(s),
            Err((code, msg)) => {
                // `napi_create_error` sets `code` from the status string, so
                // the coded error is built on the JS thread and rejected
                // verbatim.
                let mut obj = env.create_error(napi::Error::from_reason(msg))?;
                obj.set_named_property("code", code)?;
                Err(napi::Error::from(obj.to_unknown()))
            }
        }
    }
}

/// Validate an agent pack directory with no memory at all (#341) — the
/// library behind `areev pack validate`. Resolves to the pack report as a
/// JSON string: grains with their content addresses, blobs, the registry
/// keys, `allow_executor`, `executors` (every code-carrying tool with its
/// address) and warnings. Rejects with `err.code` = `PCK-E001`..`PCK-E005`.
#[napi(ts_return_type = "Promise<string>")]
pub fn pack_validate(dir: String) -> napi::bindgen_prelude::AsyncTask<PackJob> {
    PackJob::spawn(move || {
        let r = areev_pack::pack::validate_pack(std::path::Path::new(&dir))
            .map_err(|e| (e.code().to_string(), e.to_string()))?;
        serde_json::to_string(&r).map_err(|e| ("SYS-E001".to_string(), e.to_string()))
    })
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

/// `setDecider`'s resolution (docs/decision-model-proposal.md §5) — the same
/// rule as Python's `set_decider`. With neither `spec` nor `cmd`, the
/// environment names the chain (`AREEV_DECIDE`, `AREEV_DECIDE_CMD`,
/// `AREEV_DECIDE_TIMEOUT_MS`); an explicit `timeoutMs` still wins over the env
/// deadline. `Ok(None)` = nothing configured.
fn resolve_decider(
    spec: Option<&str>,
    cmd: Option<&str>,
    timeout_ms: Option<u64>,
) -> Result<Option<std::sync::Arc<dyn areev_llm::DecisionBackend>>, areev_llm::DecideError> {
    if spec.is_none() && cmd.is_none() {
        return match timeout_ms {
            None => areev_llm::env_chain(),
            Some(ms) => {
                let env = |k: &str| std::env::var(k).ok();
                areev_llm::resolve_chain(
                    env("AREEV_DECIDE").as_deref(),
                    env("AREEV_DECIDE_CMD").as_deref(),
                    Some(std::time::Duration::from_millis(ms)),
                )
            }
        };
    }
    let deadline = timeout_ms
        .map(std::time::Duration::from_millis)
        .unwrap_or(areev_llm::decide::DEFAULT_DECIDE_TIMEOUT);
    areev_llm::resolve_chain(spec, cmd, Some(deadline))
}

/// Whether `decide` must pseudonymize `state` before it leaves the process
/// (docs/decision-model-proposal.md §2 "Egress"): the handle's namespace is
/// under an egress policy (declared, or forced by the host floor), or any
/// namespace in the file declares one — `state` can carry text read from
/// any of them. Fails SAFE: a principal that may not read the policy, or a
/// poisoned gate, counts as egress-on, never as off.
fn decide_egress_active(facade: &AreevFacade, ns: &str) -> bool {
    let active = facade.store_as(areev_core::authz::Verb::Read, ns, |m| {
        if m.anon_declared().iter().any(|(_, mode)| mode == "egress") {
            return Ok(true);
        }
        m.anon_active_mode(ns).map(|mode| mode.as_deref() == Some("egress"))
    });
    !matches!(active, Ok(Ok(false)))
}

/// The installed chain as `decide` must call it: wrapped in
/// [`areev_llm::PseudonymizingDecider`] (session scope — the decorator is
/// store-free, exactly as the CLI wraps its LLM backend for `remember`) when
/// egress anonymization is active, else as installed. The wrap failing
/// fails the call; raw state never goes out as a fallback.
fn egress_decider(
    facade: &AreevFacade,
    ns: &str,
    chain: std::sync::Arc<dyn areev_llm::DecisionBackend>,
) -> Result<std::sync::Arc<dyn areev_llm::DecisionBackend>, AreevError> {
    if !decide_egress_active(facade, ns) {
        return Ok(chain);
    }
    let policy = areev_core::anon::AnonPolicy { scope: "session".into(), ..Default::default() };
    Ok(std::sync::Arc::new(areev_llm::PseudonymizingDecider::new(chain, policy)?))
}

/// The decision-backend host config one handle carries (never persisted in
/// the file): the chain `set_decider` installed, and which reranker the
/// bindings put on the store. An explicit command reranker
/// (`set_reranker_command`) always wins over the decision reranker.
#[derive(Default)]
struct HostDecide {
    chain: Option<std::sync::Arc<dyn areev_llm::DecisionBackend>>,
    /// `set_reranker_command` installed an explicit reranker.
    command_rerank: bool,
    /// A [`areev_store::DecisionRerank`] over `chain` is installed.
    decision_rerank: bool,
}

/// Stand-in for "the decision chain was cleared": the store has no reranker
/// uninstall, and a backend that always errs makes every reranked recall fail
/// open to fusion order AND fusion scores — exactly the uninstalled result.
struct NoRerank;

impl areev_store::RerankBackend for NoRerank {
    fn rerank(&self, _query: &str, _docs: &[&str]) -> areev_core::error::Result<Vec<f32>> {
        Err(AreevError::Validation("no reranker installed".into()))
    }
    fn model(&self) -> &str {
        "none"
    }
}

/// Put the store's reranker in line with `st`: with a chain and no command
/// reranker, a [`areev_store::DecisionRerank`] over the chain — wrapped in
/// [`areev_llm::PseudonymizingDecider`] when egress anonymization is active,
/// since the candidates' grain text is its `state`; with the chain cleared,
/// [`NoRerank`] in place of a decision reranker installed earlier. Rebuilt
/// (not patched) each time, so a policy change re-evaluates the wrap. Needs
/// `admin` on `"*"`, like every reranker install.
fn sync_decision_reranker(facade: &AreevFacade, ns: &str, st: &mut HostDecide) -> Result<(), AreevError> {
    if st.command_rerank {
        return Ok(());
    }
    match &st.chain {
        Some(chain) => {
            let backend = egress_decider(facade, ns, chain.clone())?;
            facade.store_as(areev_core::authz::Verb::Admin, "*", |m| {
                m.set_reranker(Box::new(areev_store::DecisionRerank::new(backend)))
            })?;
            st.decision_rerank = true;
        }
        None if st.decision_rerank => {
            facade.store_as(areev_core::authz::Verb::Admin, "*", |m| m.set_reranker(Box::new(NoRerank)))?;
            st.decision_rerank = false;
        }
        None => {}
    }
    Ok(())
}

/// Re-evaluate the decision reranker's egress wrap after an anonymization
/// change (a policy set/cleared, the floor moved): the reranker sends grain
/// text, so turning egress on must not leave an unwrapped backend installed.
/// A no-op unless `setDecider` installed one.
fn resync_decision_reranker(
    facade: &AreevFacade,
    ns: &str,
    decide: &std::sync::Mutex<HostDecide>,
) -> Result<(), AreevError> {
    let mut st = decide.lock().unwrap_or_else(|p| p.into_inner());
    if st.decision_rerank && st.chain.is_some() {
        sync_decision_reranker(facade, ns, &mut st)?;
    }
    Ok(())
}

/// Milliseconds as a recall deadline: `None` or `0` is unbounded (the
/// `AREEV_RECALL_DEADLINE_MS` rule).
fn recall_deadline_from_ms(ms: Option<u64>) -> Option<std::time::Duration> {
    ms.filter(|ms| *ms > 0).map(std::time::Duration::from_millis)
}

/// One scored hybrid-recall row — the MCP `areev_search` shape
/// `{hash, type, fields, score}`.
fn scored_row(g: &areev_core::format::deserialize::DeserializedGrain, score: f32) -> serde_json::Value {
    json!({
        "hash": g.hash.to_hex(),
        "type": format!("{:?}", g.grain_type).to_lowercase(),
        "fields": g.fields,
        "score": score,
    })
}

/// `decide`'s request: `state` is JSON when it parses to a string, object or
/// array, else the text itself; `questions` is the wire `questions` object.
fn decide_request(state: &str, questions: &str) -> Result<areev_llm::DecideRequest, areev_llm::DecideError> {
    let state = match serde_json::from_str::<serde_json::Value>(state) {
        Ok(v) if v.is_string() || v.is_object() || v.is_array() => v,
        _ => serde_json::Value::String(state.to_string()),
    };
    let questions: serde_json::Value = serde_json::from_str(questions).map_err(|e| {
        areev_llm::DecideError::InvalidQuestion(format!("`questions` is not JSON: {e}"))
    })?;
    let questions = areev_llm::decide::questions_from_wire(&questions)?;
    Ok(areev_llm::DecideRequest::new(state, questions))
}

/// Verb check for the binding methods that reach the store directly instead
/// of through a gated `cal_*` facade method. `authFile`/`principal` is
/// documented to fail closed (CAL 1.3 §9), so a sandboxed handle must not be
/// able to erase — or export a subject's dossier, or read a namespace it was
/// never granted — merely by calling the binding method rather than the CAL
/// statement that does the same thing.
///
/// Asks the facade's EFFECTIVE rights, not its bound set, so the check is
/// right under an active `PrincipalSession` too.
fn check_verb(
    facade: &AreevFacade,
    verb: areev_core::authz::Verb,
    ns: &str,
) -> napi::Result<()> {
    facade.effective_authz().check(verb, ns).map_err(err)
}

/// `check_verb` over every namespace a single call spans — a `related` walk
/// or a change feed given a comma list. Fails closed on the first namespace
/// the principal cannot reach: a walk is not composable from the namespaces
/// it was allowed, so a partial answer would silently mean something else.
fn check_verb_all(
    facade: &AreevFacade,
    verb: areev_core::authz::Verb,
    scope: &[String],
) -> napi::Result<()> {
    let rights = facade.effective_authz();
    for ns in scope {
        rights.check(verb, ns).map_err(err)?;
    }
    Ok(())
}

/// Keep only the rows a memory-WIDE read may disclose to this principal.
///
/// The counterpart to `check_verb` for reads that take no namespace at all
/// (the op-log feed, reverse provenance, the policy listings): refusing them
/// outright would break an owner-equivalent host, and answering them whole
/// discloses every namespace. `ns_of` names each row's namespace; the owner
/// keeps everything.
fn filter_readable<T>(
    facade: &AreevFacade,
    rows: Vec<T>,
    ns_of: impl Fn(&T) -> String,
) -> Vec<T> {
    let rights = facade.effective_authz();
    if rights.is_owner() {
        return rows;
    }
    rows.into_iter()
        .filter(|r| rights.allows(areev_core::authz::Verb::Read, &ns_of(r)))
        .collect()
}

#[napi]
pub struct Areev {
    /// Shared so a queued job can hold the store open independently of the JS
    /// object that started it.
    facade: FacadeSlot,
    /// The memory's path or DSN — the credential broker's blob door reads
    /// stored bytes from it by path (#106), lock-free, and the trigger
    /// evaluator's per-poll broker serves a grain connector's
    /// `areev::blob_get` the same way (#185). Neither read opens the memory.
    path: String,
    ns: String,
    /// Host-asserted actor label stamped on every loop audit grain (§6.6).
    actor: String,
    /// ONE executor for the life of the handle, so its parse cache survives
    /// between calls.
    ///
    /// A real-time turn calls this binding with a statement STRING and used to
    /// build a fresh `CalExecutor` per call — which meant lexing, parsing and
    /// validating the same statement on every single turn, with a cache that
    /// could never hit. Hoisting the executor onto the handle is what makes
    /// the plan cache reachable from JavaScript at all.
    executor: std::sync::Arc<CalExecutor>,
    /// The decision backend chain `setDecider` installed, and which reranker
    /// the binding put on the store — host config beside the embedder, never
    /// persisted in the file. Shared (like the facade slot) so a queued
    /// `decide` job can hold the chain independently of the JS object.
    decide: std::sync::Arc<std::sync::Mutex<HostDecide>>,
}

#[napi]
impl Areev {
    #[napi(constructor)]
    #[allow(clippy::too_many_arguments)] // a flat FFI surface; each knob is a distinct scalar
    pub fn new(
        path: String,
        ns: Option<String>,
        passphrase: Option<String>,
        actor: Option<String>,
        telemetry: Option<String>,
        principal: Option<String>,
        index_text: Option<bool>,
        anon_key: Option<String>,
        read_only: Option<bool>,
    ) -> napi::Result<Self> {
        let memory_path = path.clone();
        let ns = ns.unwrap_or_else(|| "shared".to_string());
        let actor = actor.unwrap_or_else(|| "user:local".to_string());
        // `readOnly` refuses every write (STO-E004), creates nothing, and
        // issues no DDL on postgres — SELECT-only verification instead, which
        // is what makes a USAGE+SELECT role a workable identity. `indexText`
        // re-stamps the file's declaration, which is a write, so refuse the
        // pair up front rather than partway through open.
        let read_only = read_only.unwrap_or(false);
        if read_only && index_text.is_some() {
            return Err(err(
                "readOnly cannot be combined with an explicit indexText: indexText always \
                 re-stamps the file's declaration, and a read-only open never writes. Drop \
                 indexText (a read-only open honors whatever the file already declares) or \
                 drop readOnly",
            ));
        }
        // Recall-telemetry sidecar (host capability, §8): agents are the main
        // telemetry producers, so the binding default is `aggregate`; pass
        // telemetry="off" to disable. Never a file-truth. A read-only handle
        // attaches no sidecar regardless (its flush is a write), so an
        // unasked-for one resolves straight to `off` — nothing to warn about.
        let tel = match telemetry.as_deref() {
            Some(v) => TelemetryMode::parse(v)
                .ok_or_else(|| err(format!("unknown telemetry mode '{v}' (off|aggregate|full)")))?,
            None if read_only => TelemetryMode::Off,
            None => TelemetryMode::Aggregate,
        };
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
        // `anonKey` is the host-supplied HKDF root for the anonymization
        // session/memory/vault subkeys, used instead of the page key when
        // given. It is what makes the mapping vault and deterministic
        // value-derived tokens work on the Postgres backend (which refuses
        // `encryptionKey` outright, a page-cipher capability) and on plaintext
        // files. Never persisted: rotating it is a crypto-erasure of the
        // mapping table, so it belongs in a KMS, not in the memory.
        //
        // Parsed BEFORE the store is opened, so a malformed key fails without
        // leaving a handle behind — which on Node would need an explicit
        // close() nobody has a reference to.
        let anon = anon_key.as_deref().map(parse_anon_key).transpose()?;
        let is_pg = areev_store::is_pg_dsn(&path);
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
                // An `anonKey` is the whole reason the vault and
                // value-derived tokens are reachable on this backend at all:
                // there is no page key here to derive them from.
                // `readOnly` takes the explicit-options path too, having no
                // other way to carry into the open.
                match (index_text, anon) {
                    (None, None) if tel != TelemetryMode::Off && !read_only => {
                        RustAreev::open_postgres_with_telemetry(&url, &schema, tel).map_err(err)?
                    }
                    (None, None) if !read_only => {
                        RustAreev::open_postgres(&url, &schema).map_err(err)?
                    }
                    (want_text, anon) => RustAreev::open_postgres_with(
                        &url,
                        &schema,
                        areev_store::AreevOptions {
                            index_text: want_text
                                .unwrap_or(areev_store::AreevOptions::default().index_text),
                            anon_key: anon,
                            telemetry: tel,
                            read_only,
                            ..areev_store::AreevOptions::default()
                        },
                    )
                    .map_err(err)?,
                }
            }
            // Supplying key material makes the open explicit, which re-stamps
            // the file's declarations — the same trade `passphrase` alone has
            // always made (it routes through `open_with` too), reported either
            // way by `openWarnings()`.
            (false, pass) => match (index_text, anon, pass) {
                (None, None, Some(p)) if !read_only => {
                    RustAreev::open_with_passphrase_telemetry(&path, &p, tel).map_err(err)?
                }
                (None, None, None) if !read_only => {
                    RustAreev::open_with_telemetry(&path, tel).map_err(err)?
                }
                (want_text, anon, pass) => {
                    let key = match pass {
                        // Deriving writes the .kdf sidecar when absent, so the
                        // read-only precondition is checked first or a refused
                        // open leaves a stray file behind.
                        Some(p) => {
                            areev_store::read_only_requires_existing(&path, read_only)
                                .map_err(err)?;
                            Some(*RustAreev::derive_key_for(&path, &p).map_err(err)?)
                        }
                        None => None,
                    };
                    RustAreev::open_with(
                        &path,
                        areev_store::AreevOptions {
                            index_text: want_text
                                .unwrap_or(areev_store::AreevOptions::default().index_text),
                            encryption_key: key,
                            anon_key: anon,
                            telemetry: tel,
                            read_only,
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
        Ok(Areev {
            facade,
            path: memory_path,
            ns,
            actor,
            executor: std::sync::Arc::new(CalExecutor::new(CalExecutorConfig::default())),
            decide: std::sync::Arc::new(std::sync::Mutex::new(HostDecide::default())),
        })
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
            // Check BEFORE constructing: `CommandEmbed::new` probes the command
            // by running it, and an ungranted caller must not be able to spawn
            // a subprocess on the way to being refused.
            check_verb(&facade, areev_core::authz::Verb::Admin, "*")?;
            let ce = CommandEmbed::new(&cmd, model.as_deref()).map_err(err)?;
            facade
                .store_as(areev_core::authz::Verb::Admin, "*", |m| m.set_embedder(Box::new(ce)))
                .map_err(err)?;
            Ok(())
        })
    }

    /// Install a decision (System One) backend chain
    /// (docs/decision-model-proposal.md) — the JS mirror of Python's
    /// `set_decider`. `spec` is the ordered, comma-separated provider list
    /// (`typesafe:jev-latest,llm:ollama:…`); `cmd` a command backend appended
    /// last (stdin: wire request JSON, stdout: wire response JSON; no shell);
    /// `timeoutMs` the per-call deadline (default 2000). With neither `spec`
    /// nor `cmd`, the chain comes from `AREEV_DECIDE` / `AREEV_DECIDE_CMD` /
    /// `AREEV_DECIDE_TIMEOUT_MS`; when nothing is configured anywhere (or
    /// `spec` is `''`), the decider is cleared. A bad spec or a missing
    /// provider key throws (`DEC-E001`). Synchronous: resolving a chain does
    /// no I/O. Needs `admin` on `"*"` — a command backend is a subprocess.
    /// Host config — never persisted in the file.
    ///
    /// Installing a chain also installs it as the recall reranker
    /// (`DecisionRerank`: `search()` reorders by it and each row's `score` is
    /// its answer) — unless `setRerankerCommand` installed a command
    /// reranker, which always wins. Clearing the chain uninstalls the
    /// decision reranker (recall falls back to fusion order). Under egress
    /// anonymization the backend sees pseudonymized state, both here and in
    /// `decide`.
    #[napi]
    pub fn set_decider(
        &self,
        spec: Option<String>,
        cmd: Option<String>,
        timeout_ms: Option<u32>,
    ) -> napi::Result<()> {
        let facade = take_facade(&self.facade)?;
        check_verb(&facade, areev_core::authz::Verb::Admin, "*")?;
        let chain = resolve_decider(spec.as_deref(), cmd.as_deref(), timeout_ms.map(u64::from))
            .map_err(err)?;
        let mut st = self.decide.lock().unwrap_or_else(|p| p.into_inner());
        st.chain = chain;
        sync_decision_reranker(&facade, &self.ns, &mut st).map_err(err)
    }

    /// Ask the installed decision backend typed questions about `state` —
    /// the JS mirror of Python's `decide`. `state` is text, or a JSON
    /// string/object/array document; `questions` is the wire `questions`
    /// object as JSON. Resolves to the wire response plus provenance as JSON:
    /// `{model, answers, usage?, provider, calibrated, latency_ms}`. No
    /// decider → rejects with `DEC-E001`; every failure names its `DEC-Ennn`
    /// code. A promise: a remote backend is a network round trip.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn decide(
        &self,
        state: String,
        questions: String,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let decide = self.decide.clone();
        let ns = self.ns.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let decider = decide.lock().unwrap_or_else(|p| p.into_inner()).chain.clone();
            let decider = decider.ok_or_else(|| {
                err(areev_llm::DecideError::NotConfigured(
                    "no decision backend: call setDecider(...) or set AREEV_DECIDE / AREEV_DECIDE_CMD \
                     and call setDecider()"
                        .into(),
                ))
            })?;
            let decider = egress_decider(&facade, &ns, decider).map_err(err)?;
            let req = decide_request(&state, &questions).map_err(err)?;
            let decision = decider.decide(&req).map_err(err)?;
            Ok(decision.to_json().to_string())
        })
    }

    /// Bound every hybrid recall this handle makes — `search()` and CAL's
    /// free-text `RECALL` — to `ms` milliseconds (the JS mirror of Python's
    /// `set_recall_deadline_ms`); past it a leg fails open (partial results,
    /// never an error) and a reranker that has not started is skipped.
    /// `null` or `0` restores the unbounded default. Host config, never
    /// persisted. Set it while no call is in flight on this handle (a
    /// running call shares the facade), else it throws.
    #[napi]
    pub fn set_recall_deadline_ms(&self, ms: Option<u32>) -> napi::Result<()> {
        let mut slot = self.facade.lock().unwrap_or_else(|e| e.into_inner());
        let arc = slot.as_mut().ok_or_else(|| {
            err(AreevError::Validation(
                "this handle is closed — open a new Areev for further calls".into(),
            ))
        })?;
        let facade = std::sync::Arc::get_mut(arc).ok_or_else(|| {
            err("setRecallDeadlineMs: this handle is shared with a call still in flight — \
                 await it (or set the deadline before starting it)")
        })?;
        facade.set_recall_deadline(recall_deadline_from_ms(ms.map(u64::from)));
        Ok(())
    }

    /// The recall deadline `setRecallDeadlineMs` installed, in ms (`null` =
    /// unbounded).
    #[napi]
    pub fn recall_deadline_ms(&self) -> napi::Result<Option<u32>> {
        let facade = take_facade(&self.facade)?;
        Ok(facade
            .recall_deadline()
            .map(|d| u32::try_from(d.as_millis()).unwrap_or(u32::MAX)))
    }

    /// Install a command reranker — the JS mirror of Python's
    /// `set_reranker_command` (same contract as the CLI's `--rerank-cmd` and
    /// MCP's `AREEV_RERANK_CMD`): the command gets `{"query": "...", "docs":
    /// ["...", ...]}` on stdin and prints a JSON array of `docs.length`
    /// numbers, higher = more relevant. No shell; not probed (synchronous) —
    /// a broken command fails open to fusion order at recall time.
    /// `search()` then reorders by it, each row's `score` being its
    /// min-max-normalized answer (top = 1.0); CAL uses it under `WITH
    /// rerank`. `model` is the observability label. An explicit command
    /// reranker wins over the decision reranker `setDecider` installs. Needs
    /// `admin` on `"*"`.
    #[napi]
    pub fn set_reranker_command(&self, cmd: String, model: Option<String>) -> napi::Result<()> {
        let facade = take_facade(&self.facade)?;
        check_verb(&facade, areev_core::authz::Verb::Admin, "*")?;
        let rr = areev_store::CommandRerank::new(&cmd, model.as_deref()).map_err(err)?;
        let mut st = self.decide.lock().unwrap_or_else(|p| p.into_inner());
        facade
            .store_as(areev_core::authz::Verb::Admin, "*", |m| m.set_reranker(Box::new(rr)))
            .map_err(err)?;
        st.command_rerank = true;
        st.decision_rerank = false;
        Ok(())
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
        facade
                .store_as(areev_core::authz::Verb::Admin, "*", |m| m.set_embedder(Box::new(backend)))
                .map_err(err)?;
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
    /// string shaped like `recall()` plus each hit's `score` in `[0, 1]`
    /// (the MCP `areev_search` row): rank-normalized fusion (top = 1.0), or
    /// the installed reranker's normalized answer (`setRerankerCommand` /
    /// `setDecider`), which search always uses when present.
    /// `setRecallDeadlineMs` bounds it. Errors loudly when the file has
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
            // Scored, like MCP's `areev_search`: each row carries the store's
            // normalized relevance (top hit = 1.0; the reranker's
            // min-max-normalized answer when one is installed, which search
            // then always uses). The handle's recall deadline applies.
            let deadline = facade.recall_deadline();
            let hits = facade
                .store_read(&ns, |m| {
                    let tuning = areev_store::RecallTuning {
                        rerank: m.has_reranker(),
                        ..Default::default()
                    };
                    m.recall_hybrid_scored(
                        &ns,
                        subject.as_deref(),
                        relation.as_deref(),
                        Some(&query),
                        k,
                        deadline,
                        tuning,
                    )
                })
                .map_err(err)?;
            let out: Vec<serde_json::Value> = hits.iter().map(|(g, score)| scored_row(g, *score)).collect();
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
                .store_checked(areev_core::authz::Verb::Admin, "*", |m| m.rebuild_text_index())
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
                .store_checked(areev_core::authz::Verb::Admin, "*", |m| m.rebuild_link_indexes())
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
            // One binding method, five store operations: `view` reads,
            // `create`/`str_replace`/`insert`/`rename` write, and `delete`
            // DESTROYS. Gating the method as a whole would either refuse a
            // granted read or admit an ungranted erasure, so the command
            // names the verb. An unrecognized command needs `admin`: a
            // command added to `MemoryTool` later is then gated until it is
            // mapped here, rather than silently ungated.
            let verb = match cmd.get("command").and_then(|v| v.as_str()) {
                Some("view") => areev_core::authz::Verb::Read,
                Some("create") | Some("str_replace") | Some("insert") | Some("rename") => {
                    areev_core::authz::Verb::Write
                }
                Some("delete") => areev_core::authz::Verb::Delete,
                _ => areev_core::authz::Verb::Admin,
            };
            facade
                .store_checked(verb, &ns, |m| {
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
                .store_write(&ns, |m| {
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
            Ok(validated_cal_add(&facade, &grain_type, &fields).map_err(err)?.to_hex())
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
                .store_read(&ns, |m| m.recall(&ns, &subject, relation.as_deref(), k))
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
                .store_read(&ns, |m| m.latest(&ns, &subject, &relation))
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
                .store_write(&ns, |m| m.capture(&ns, &content, &meta))
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
                .store_write(&ns, |m| m.attach_facts(&ns, &event, &drafts, &attribution))
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
        let ex = self.executor.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let res = ex.execute(&query, &*facade).map_err(err)?;
            serde_json::to_string(&res.payload_json().map_err(err)?).map_err(err)
        })
    }

    /// Parse and validate a statement now, and keep the plan, so the first
    /// real turn does not pay for it.
    ///
    /// The handle is the statement TEXT — there is no opaque handle object to
    /// leak or invalidate, because the executor already keys its plan cache on
    /// exactly that. Call this at startup for each statement your agent runs
    /// on the hot path: it turns a bad statement into a startup error instead
    /// of a first-turn error, and guarantees the plan is warm rather than
    /// hoping it is.
    ///
    /// Returns `{statement, cached}` where `cached` is the number of plans
    /// held.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn cal_prepare(&self, query: String) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let ex = self.executor.clone();
        StringJob::spawn(move || {
            ex.parse_cached(&query).map_err(err)?;
            Ok(json!({"statement": query, "cached": ex.plan_cache_len()}).to_string())
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
                .store_read(&ns, |m| m.history(&ns, &subject, &relation))
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
            let kids = filter_readable(&facade, kids, |g| {
                g.get_str("namespace").unwrap_or("shared").to_string()
            });
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
                .store_read(&ns, |m| {
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
            facade.store_checked(areev_core::authz::Verb::Write, "*", |m| m.put_blob(&bytes)).map_err(err)
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
            let bytes = facade.store_checked(areev_core::authz::Verb::Read, "*", |m| m.get_blob(&uri)).map_err(err)?;
            Ok(bytes.into())
        })
    }

    /// Store statistics as JSON.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn stats(&self) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let s = facade.store_checked(areev_core::authz::Verb::Read, "*", |m| m.stats()).map_err(err)?;
            Ok(json!({
                "grains": s.grains, "current": s.current, "triples": s.triples,
                "terms": s.terms, "ops": s.ops, "events_indexed": s.events_indexed,
            })
            .to_string())
        })
    }

    // ----- grain attestation (docs/grain-attestation-plan.md) -----

    /// Install the host's author key (a 32-byte Ed25519 seed as 64 hex
    /// characters). Every grain written from now on is followed by its
    /// attestation. Returns the key id. Host config, never persisted.
    #[napi]
    pub fn set_signing_key(&self, seed_hex: String) -> napi::Result<String> {
        let facade = take_facade(&self.facade)?;
        facade.store_checked(areev_core::authz::Verb::Admin, "*", |m| m.set_signing_key_hex(&seed_hex)).map_err(err)
    }

    /// The installed author key as JSON `{"key_id", "public_key"}`, or null.
    #[napi]
    pub fn signing_key(&self) -> napi::Result<Option<String>> {
        let facade = take_facade(&self.facade)?;
        Ok(facade
            .store_checked(areev_core::authz::Verb::Admin, "*", |m| Ok::<_, AreevError>(m.signing_key()))
            .map_err(err)?
            .map(|(id, pk)| json!({"key_id": id, "public_key": pk}).to_string()))
    }

    /// Install the trusted-authors document (JSON: `{"keys": {key_id:
    /// public_key_hex}, "policy": "off|verify|require"}`). Governs bundle
    /// import and `verifyAttestations`. Returns the number of keys.
    #[napi]
    pub fn set_trusted_authors(&self, json: String) -> napi::Result<u32> {
        let facade = take_facade(&self.facade)?;
        facade
            .store_checked(areev_core::authz::Verb::Admin, "*", |m| m.set_trusted_authors(&json))
            .map(|n| n as u32)
            .map_err(err)
    }

    /// Override the installed trusted-authors policy: off | verify | require.
    #[napi]
    pub fn set_attest_policy(&self, policy: String) -> napi::Result<()> {
        let p = areev_store::AttestPolicy::parse(&policy).map_err(err)?;
        let facade = take_facade(&self.facade)?;
        facade
            .store_checked(areev_core::authz::Verb::Admin, "*", |m| {
                m.set_attest_policy(p);
                Ok::<_, AreevError>(())
            })
            .map_err(err)
    }

    /// Attest one stored grain with the installed key. Idempotent. Resolves
    /// to the attestation grain's hash.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn attest(&self, hash: String) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let h = Hash::from_hex(&hash).map_err(err)?;
            let a = facade.store_checked(areev_core::authz::Verb::Admin, "*", |m| m.attest(&h)).map_err(err)?;
            Ok(a.to_hex())
        })
    }

    /// Attest every attestable grain the installed key has not attested yet
    /// (optionally only namespaces starting with `nsPrefix`). Resolves to
    /// JSON `{"attested", "skipped"}`.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn attest_all(&self, ns_prefix: Option<String>) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let st = facade
                .store_checked(areev_core::authz::Verb::Admin, "*", |m| m.attest_all(ns_prefix.as_deref()))
                .map_err(err)?;
            serde_json::to_string(&st).map_err(|e| err(AreevError::Internal(e.to_string())))
        })
    }

    /// Check every stored attestation against the trusted authors. Read-only.
    /// Resolves to the report as JSON; never rejects on a bad attestation —
    /// read `attest_invalid` and `invalid`.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn verify_attestations(&self) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let r = facade.store_checked(areev_core::authz::Verb::Read, "*", |m| m.verify_attestations()).map_err(err)?;
            serde_json::to_string(&r).map_err(|e| err(AreevError::Internal(e.to_string())))
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
                .store_checked(areev_core::authz::Verb::Admin, "*", |m| m.bundle_since(since.unwrap_or(0), &path))
                .map_err(err)?;
            Ok(st.last_op_seq)
        })
    }

    /// Install an agent pack directory into this memory (#341) — the library
    /// behind `areev pack install`, under THIS handle's bound principal.
    /// Resolves to the pack report as a JSON string (the fields
    /// `areev pack install --format json` prints, plus `executors`).
    ///
    /// All-or-nothing: every grain is built and addressed, `expectedHash` and
    /// `executorPins` are checked, and every write is authorized BEFORE the
    /// first one; the grains then go in as one batch. A principal without
    /// `write` on the pack's namespace rejects with `code: "AUT-E001"` and the
    /// memory untouched. Rejections carry `err.code` = `PCK-E001`..`PCK-E005`
    /// or the `AUT-*`/`STO-*` code passed through.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn pack_install(
        &self,
        dir: String,
        options: Option<PackInstallOptions>,
    ) -> napi::bindgen_prelude::AsyncTask<PackJob> {
        let slot = self.facade.clone();
        let o = options.unwrap_or_default();
        let opts = areev_pack::pack::InstallOptions {
            dry_run: o.dry_run.unwrap_or(false),
            namespace: o.ns,
            expected_hash: o.expected_hash,
            executor_pins: o.executor_pins.unwrap_or_default().into_iter().collect(),
        };
        PackJob::spawn(move || {
            // Through the facade, so the install is authorized as this
            // handle's principal — never an owner store (#316).
            let facade = take_facade(&slot).map_err(|e| (leading_code(&e.reason), e.reason))?;
            let r = areev_pack::pack::install_pack(&facade, std::path::Path::new(&dir), &opts)
                .map_err(|e| (e.code().to_string(), e.to_string()))?;
            serde_json::to_string(&r).map_err(|e| ("SYS-E001".to_string(), e.to_string()))
        })
    }

    /// Apply a bundle (fast-forward, idempotent). Returns ops applied.
    #[napi(ts_return_type = "Promise<number>")]
    pub fn import_bundle(&self, path: String) -> napi::bindgen_prelude::AsyncTask<U32Job> {
        let slot = self.facade.clone();
        U32Job::spawn(move || {
            let facade = take_facade(&slot)?;
            let st = facade.store_checked(areev_core::authz::Verb::Admin, "*", |m| m.import_bundle(&path)).map_err(err)?;
            Ok(st.applied as u32)
        })
    }

    /// Integrity + content-address verification. Throws on failure.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn verify(&self) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let r = facade.store_checked(areev_core::authz::Verb::Read, "*", |m| m.verify()).map_err(err)?;
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
            // #303: a comma list walks the SET. A walk is not composable from
            // per-namespace calls, so a host doing it itself re-implements
            // the BFS and its depth and cap mean something different from
            // Areev's.
            let scope = split_ns_list(&ns);
            check_verb_all(&facade, areev_core::authz::Verb::Read, &scope)?;
            let reached = facade
                .with_store(|m| {
                    if scope.len() > 1 {
                        m.related_scoped(&scope, &start, &refs, dir, depth, limit)
                    } else {
                        m.related(&ns, &start, &refs, dir, depth, limit)
                    }
                })
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
                .store_read(&ns, |m| m.entity_at(&ns, &subject, &relation, at, ax))
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
                .store_read(&ns, |m| {
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
                .store_read(&ns, |m| m.run_grains(&ns, &run_id, after_seq, limit))
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
                .store_read(&ns, |m| {
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
                .store_checked(areev_core::authz::Verb::Admin, "*", |m| m.set_grain_embedding(&h, &vec32))
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

    /// The bulk form of `addEmbedding`: one transaction for
    /// `itemsJson = [{"hash": "<64-hex>", "vector": [..]}, ...]`. An unknown
    /// hash or a dimension mismatch refuses the whole batch before anything
    /// is written. Resolves to `{"written": n}`.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn add_embeddings(&self, items_json: String) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let items = areev_store::parse_embedding_items(&items_json).map_err(err)?;
            let n = facade.store_checked(areev_core::authz::Verb::Admin, "*", |m| m.set_grain_embeddings(&items)).map_err(err)?;
            Ok(json!({"written": n}).to_string())
        })
    }

    /// Build the ANN (pgvector HNSW) index over the stored vectors. Postgres
    /// only — the embedded engine rejects with `STO-E007`. Defaults are
    /// pgvector's (`m` 16, `efConstruction` 64, `efSearch` 40). Resolves to
    /// `{"index": name}`. Grade it with `vectorRecallCheck` before relying on it.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn ensure_vector_index(
        &self,
        m: Option<u32>,
        ef_construction: Option<u32>,
        ef_search: Option<u32>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let (m, efc, efs) = (
            m.unwrap_or(16) as usize,
            ef_construction.unwrap_or(64) as usize,
            ef_search.unwrap_or(40) as usize,
        );
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let name = facade
                .store_checked(areev_core::authz::Verb::Admin, "*", |s| {
                    s.ensure_vector_index(m, efc, efs)?;
                    s.vector_index()
                })
                .map_err(err)?;
            Ok(json!({"index": name}).to_string())
        })
    }

    /// Drop the ANN index, returning vector recall to an exact scan.
    /// Resolves to `{"index": null}`.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn drop_vector_index(&self) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            facade.store_checked(areev_core::authz::Verb::Admin, "*", |s| s.drop_vector_index()).map_err(err)?;
            Ok(json!({"index": serde_json::Value::Null}).to_string())
        })
    }

    /// `{"index": name}` if an ANN index is built, `{"index": null}` if not.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn vector_index(&self) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let name = facade.with_store(|s| s.vector_index()).map_err(err)?;
            Ok(json!({"index": name}).to_string())
        })
    }

    /// Grade the ANN index against the exact scan with YOUR query vectors:
    /// `queriesJson` is a JSON array of vectors, `k` the cutoff (default 10),
    /// `ns` the scope you really query with, `efSearch` an optional retune of
    /// the index for this session first. Resolves to
    /// `{"index", "ef_search", "k", "queries", "recall"}`; with no index built
    /// the read path is exact and the report says so.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn vector_recall_check(
        &self,
        queries_json: String,
        k: Option<u32>,
        ns: Option<String>,
        ef_search: Option<u32>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let k = k.unwrap_or(10) as usize;
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let queries: Vec<Vec<f32>> = serde_json::from_str(&queries_json).map_err(|e| {
                err(format!("queries must be a JSON array of number arrays: {e}"))
            })?;
            let report = facade
                .store_read(&ns, |s| {
                    if let Some(ef) = ef_search {
                        s.set_vector_ef_search(ef as usize)?;
                    }
                    s.vector_recall_check(&ns, &queries, k)
                })
                .map_err(err)?;
            serde_json::to_string(&report).map_err(err)
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
                .store_read(&ns, |m| m.runs_touching(&ns, &h, depth))
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
                .store_read(&ns, |m| m.step_actions(&ns, &wf, node.as_deref(), limit))
                .map_err(err)?;
            let steps: Vec<serde_json::Value> = rows
                .into_iter()
                .map(|(n, h)| json!({"node": n, "hash": h.to_hex()}))
                .collect();
            Ok(json!({"workflow": wf.to_hex(), "steps": steps}).to_string())
        })
    }

    /// Last `n` grains of one conversation, oldest→newest (transcript order).
    ///
    /// The read a chat or voice agent makes on every single turn. Backed by
    /// `idx_thread(ns, session, seq)`, so the bound is n turns of THIS session
    /// rather than n rows of the namespace — unlike a namespace scan filtered
    /// afterwards, which can miss the conversation entirely on a busy
    /// namespace. Exact namespace only: a session lives in one by construction.
    ///
    /// Returns `{ns, session, grains: [{hash, type, fields}]}`.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn thread_tail(
        &self,
        session: String,
        n: Option<u32>,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let n = n.unwrap_or(20) as usize;
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            check_verb(&facade, areev_core::authz::Verb::Read, &ns)?;
            let tail = facade.with_store(|m| m.thread_tail(&ns, &session, n)).map_err(err)?;
            let grains: Vec<serde_json::Value> = tail
                .iter()
                .map(|g| {
                    serde_json::json!({
                        "hash": g.hash.to_hex(),
                        "type": g.grain_type.as_str(),
                        "fields": g.fields.clone().into_iter().collect::<serde_json::Map<_, _>>(),
                    })
                })
                .collect();
            Ok(json!({"ns": ns, "session": session, "grains": grains}).to_string())
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
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        // `ns` targets a namespace other than the session's, exactly as
        // `add()` does — kept in lockstep with the Python binding.
        let ns = ns.unwrap_or_else(|| self.ns.clone());
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

    /// Record a governed corpus export a host performed itself — the same
    /// immutable export-manifest grain `areev corpus` writes (the CLI verb
    /// remains the paved road; this is for hosts that select and serialize
    /// in-process). `subjectFingerprints` and `sourceHashes` are JSON string
    /// arrays. Returns the manifest hash — the lineage anchor
    /// `recordAdapter` requires.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn record_corpus_export(
        &self,
        selector: String,
        destination: String,
        recipient: Option<String>,
        subject_fingerprints: Option<String>,
        source_hashes: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let fps: Vec<String> =
                serde_json::from_str(subject_fingerprints.as_deref().unwrap_or("[]"))
                    .map_err(|e| napi::Error::from_reason(format!("subjectFingerprints must be a JSON string array: {e}")))?;
            let hashes: Vec<String> = serde_json::from_str(source_hashes.as_deref().unwrap_or("[]"))
                .map_err(|e| napi::Error::from_reason(format!("sourceHashes must be a JSON string array: {e}")))?;
            let hash = facade
                .record_corpus_export(&selector, &destination, recipient.as_deref(), now_ms(), &fps, &hashes)
                .map_err(err)?;
            Ok(json!({"hash": hash.to_hex()}).to_string())
        })
    }

    /// Register a host-trained adapter (the tuning seam) — the same
    /// registration `areev tune` performs after its trainer returns, for
    /// hosts that train in-process. `reply` is the adapter-reference JSON
    /// (`{"adapter": {"uri", "sha256"}, "base_model", "serves_as", …}`),
    /// `manifestHash` a recorded corpus export, `evalsetHash` the Rule E1
    /// pin. Validation is the facade's: an incomplete reply writes nothing.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn record_adapter(
        &self,
        reply: String,
        manifest_hash: String,
        evalset_hash: String,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let hash = facade
                .record_adapter(&reply, &manifest_hash, &evalset_hash, now_ms())
                .map_err(err)?;
            Ok(json!({"hash": hash.to_hex()}).to_string())
        })
    }

    /// Run one analysis pass. Bare it never gates. `fullSweep` re-analyzes
    /// the whole memory (`areev loop reflect` semantics); `policy` is a path
    /// to a host `loop-policy.json` — the only way auto-apply is granted
    /// from the bindings. `baseUrl` / `keyEnv` point the model leg
    /// (reflection and grounding) at a gateway with a key named by variable
    /// — the CLI's `--llm-base-url` / `--llm-api-key-env`, and `runStart`'s
    /// pair (#346). Returns run-outcome JSON.
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
        base_url: Option<String>,
        key_env: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        // Every field is named today; the struct update stays so a field added
        // to `RunOptions` defaults here instead of breaking the build.
        #[allow(clippy::needless_update)]
        let opts = RunOptions {
            min_new: min_new.map(|n| n as u64),
            min_new_errors: min_new_errors.map(|n| n as u64),
            if_stale_ms: if_stale.as_deref().and_then(parse_duration_ms),
            namespaces: Vec::new(),
            full_sweep: full_sweep.unwrap_or(false),
            // The handle's actor — the same identity review/apply stamp — so
            // the trigger of an LLM/external finding cannot approve it.
            triggering_actor: Some(self.actor.clone()),
        ..Default::default()
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
                engine = engine.with_llm(
                    areev_llm::resolve(&spec, base_url.as_deref(), key_env.as_deref()).map_err(err)?,
                );
            }
            // Optional separate grounding backend (defaults to the reflection model).
            if let Some(cmd) = ground_cmd {
                let g = areev_loop::CommandLlm::new(&cmd, None).map_err(err)?;
                engine = engine.with_ground_llm(Box::new(g));
            } else if let Some(spec) = ground_model {
                engine =
                    engine.with_ground_llm(
                        areev_llm::resolve(&spec, base_url.as_deref(), key_env.as_deref())
                            .map_err(err)?,
                    );
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

    /// Score a candidate loop configuration against the recorded past,
    /// beside the incumbent (`areev loop replay`). `request` is the JSON
    /// `ReplayRequest`: `{"config": {"<analyzer id>": {...}}, "policy": {...},
    /// "window": "90d" | "since_ms": n, "step": "per-pass" | "1d"}`; `policy`
    /// is a path to a host `loop-policy.json` for the incumbent. Reads only;
    /// the model and external analyzers are reported `not_replayed`. Returns
    /// the report JSON.
    #[napi]
    pub fn loop_replay(
        &self,
        request: Option<String>,
        policy: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let req = areev_loop::replay::ReplayRequest::from_json(request.as_deref().unwrap_or("{}")).map_err(err)?;
            let mut engine = Engine::with_builtins();
            if let Some(path) = policy {
                let s = std::fs::read_to_string(&path)
                    .map_err(|e| err(format!("policy {path}: {e}")))?;
                engine = engine.with_policy(areev_loop::Policy::from_json(&s).map_err(err)?);
            }
            let (candidate, opts) = req.resolve(now_ms()).map_err(err)?;
            let sub = BorrowedSubstrate::new(&facade);
            let res = engine.replay(&sub, &candidate, &opts).map_err(err)?;
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
        let filter = filter.and_then(|f| serde_json::from_str::<serde_json::Value>(&f).ok());
        let requested = filter
            .as_ref()
            .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(str::to_string));
        let with_proposal = match include_proposal(filter.as_ref()) {
            Ok(b) => b,
            Err(e) => return StringJob::spawn(move || Err(e)),
        };
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
        // Coverage-filtered (#312).
        let recs = areev_loop_adapter::visible_recommendations(
            &Engine::with_builtins(),
            &sub,
            &facade.authz(),
            status,
        )
        .map_err(err)?;
        let rows: Vec<_> = recs
            .iter()
            .map(|r| {
                let mut row = json!({
                    "hash": r.hash,
                    "status": r.status.as_str(),
                    "severity": r.severity.as_str(),
                    "analyzer": r.analyzer,
                    "summary": r.summary.render(),
                    "target_ref": r.target_ref,
                    "destructive": r.destructive,
                    // Rule E1's pin, so a reviewer can see which evalset a
                    // code or adapter revision will be held to BEFORE they
                    // approve it. Absent on every other kind — the engine
                    // refuses a pin anywhere else.
                    "evalset_hash": r.evalset_hash,
                    "rollbackable": r.rollbackable,
                });
                if with_proposal {
                    let o = row.as_object_mut().expect("json! object");
                    o.insert("action_kind".into(), json!(r.action_kind));
                    o.extend(areev_loop_adapter::proposal_fields(r));
                }
                row
            })
            .collect();
        serde_json::to_string(&rows).map_err(err)
        })
    }

    /// One recommendation as `areev loop show` prints it — the review
    /// surface, including the proposal body (`cal` / `edit` / `data`) and
    /// `action_kind`, so a host can measure a proposal before it approves or
    /// applies it (#348). A hash prefix is accepted. Coverage-filtered like
    /// `recommendations()`: a recommendation the principal cannot see is
    /// "not found", never a denial that discloses it exists.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn recommendation(&self, hash: String) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let sub = BorrowedSubstrate::new(&facade);
            let recs = areev_loop_adapter::visible_recommendations(
                &Engine::with_builtins(),
                &sub,
                &facade.authz(),
                None,
            )
            .map_err(err)?;
            let r = areev_loop_adapter::find_recommendation(&recs, &hash).map_err(err)?;
            serde_json::to_string(&areev_loop_adapter::recommendation_detail(r)).map_err(err)
        })
    }

    /// Approve and apply a recommendation in one audited step (§6.6). The
    /// `because` reason is mandatory. A refused apply leaves the recommendation
    /// **pending**, so it can still be dismissed.
    ///
    /// `scopes` is a comma-separated subset of `read,write,review,apply,admin`;
    /// omit it for all scopes.
    #[napi(ts_return_type = "Promise<string>")]
    /// A code or adapter revision applies only through its recorded gating
    /// edge: pass `gatingRun` (an `eval-…` run id from `areev eval run`) and
    /// the evidence is loaded from the journaled `mg:eval_run` summary —
    /// never from these arguments. Same contract as the CLI's `--gating-run`.
    pub fn apply_recommendation(
        &self,
        hash: String,
        because: String,
        allow_destructive: Option<bool>,
        scopes: Option<String>,
        gating_run: Option<String>,
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
            // recommendation where the reviewer could no longer reject it. The
            // gating edge loads FIRST for the same reason: a bad run id must
            // fail before any state transition.
            let gating = match &gating_run {
                Some(run_id) => Some(engine.gating_evidence(&sub, &hash, run_id).map_err(err)?),
                None => None,
            };
            engine
                .preflight_apply(&sub, &hash, &scopes, allow_destructive, gating.is_some())
                .map_err(err)?;
            engine
                .review(&mut sub, &hash, Decision::Approve, &actor, ObserverType::Human, &scopes, &because, now)
                .map_err(err)?;
            let applied = match &gating {
                Some(g) => engine
                    .apply_gated(&mut sub, &hash, &actor, ObserverType::Human, &scopes, &because, allow_destructive, g, now)
                    .map_err(err)?,
                None => engine
                    .apply(&mut sub, &hash, &actor, ObserverType::Human, &scopes, &because, allow_destructive, now)
                    .map_err(err)?,
            };
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
        let (decide, own_ns) = (self.decide.clone(), self.ns.clone());
        UnitJob::spawn(move || {
            let facade = take_facade(&slot)?;
            facade
                .store_checked(areev_core::authz::Verb::Admin, &ns, |m| {
                    m.set_anon_policy(&ns, &policy_json)
                })
                .map_err(err)?;
            resync_decision_reranker(&facade, &own_ns, &decide).map_err(err)
        })
    }

    /// Remove one namespace's anonymization policy (missing is not an error).
    #[napi(ts_return_type = "Promise<void>")]
    pub fn clear_anon_policy(&self, ns: String) -> napi::bindgen_prelude::AsyncTask<UnitJob> {
        let slot = self.facade.clone();
        let (decide, own_ns) = (self.decide.clone(), self.ns.clone());
        UnitJob::spawn(move || {
            let facade = take_facade(&slot)?;
            facade
                .store_checked(areev_core::authz::Verb::Admin, &ns, |m| m.clear_anon_policy(&ns))
                .map_err(err)?;
            resync_decision_reranker(&facade, &own_ns, &decide).map_err(err)
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
            let policies = filter_readable(&facade, policies, |(ns, _)| ns.clone());
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
            let maps = filter_readable(&facade, maps, |(ns, _, _)| ns.clone());
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
    /// Raising it needs no grant; lowering it needs `admin` on `"*"` (#345).
    #[napi(ts_return_type = "Promise<void>")]
    pub fn set_anonymize_egress_floor(
        &self,
        on: bool,
    ) -> napi::bindgen_prelude::AsyncTask<UnitJob> {
        let slot = self.facade.clone();
        let (decide, ns) = (self.decide.clone(), self.ns.clone());
        UnitJob::spawn(move || {
            let facade = take_facade(&slot)?;
            facade.set_anonymize_egress_floor(on).map_err(err)?;
            resync_decision_reranker(&facade, &ns, &decide).map_err(err)
        })
    }

    /// Whether the egress anonymization floor is on for this handle.
    #[napi(ts_return_type = "Promise<boolean>")]
    pub fn anonymize_egress_floor(&self) -> napi::bindgen_prelude::AsyncTask<BoolJob> {
        let slot = self.facade.clone();
        BoolJob::spawn(move || Ok(take_facade(&slot)?.anonymize_egress_floor()))
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
            // Check BEFORE constructing: `CommandAnonymize::new` probes the
            // command by running it.
            check_verb(&facade, areev_core::authz::Verb::Admin, "*")?;
            let backend = areev_store::CommandAnonymize::new(&cmd).map_err(err)?;
            facade
                .store_as(areev_core::authz::Verb::Admin, "*", |m| {
                    m.set_anonymizer(Box::new(backend));
                })
                .map_err(err)?;
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
    ///
    /// `onEvent` is a callback taking ONE argument: a §6.10 run event as a
    /// JSON string, exactly the line the CLI's `--events` prints. It is
    /// observational — the journal is byte-identical with or without it, and a
    /// slow callback delays events rather than the run. Attaching one also
    /// turns on `TokenChunk` deltas from abstract nodes' model turns, which
    /// are observational in the same sense (the journaled result is the final
    /// message, not the concatenated deltas).
    ///
    /// Note `RunFinished` is emitted at a TERMINAL outcome: a run that parks
    /// on a human gate ends this leg at `AskRaised`, and the `runResume` leg
    /// carries `RunResumed` … `RunFinished`.
    #[napi(ts_return_type = "Promise<string>")]
    #[allow(clippy::too_many_arguments)] // a flat FFI surface; each knob is a distinct scalar
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
        model: Option<String>,
        base_url: Option<String>,
        key_env: Option<String>,
        llm_max_tokens: Option<u32>,
        allow_executor: Option<String>,
        executor_cache: Option<String>,
        sandbox_cmd: Option<String>,
        executor_timeout_secs: Option<i64>,
        tool_env: Option<String>,
        on_event: Option<napi::bindgen_prelude::Function<String, ()>>,
        credentials: Option<String>,
        allow_hosts: Option<String>,
        tool_egress: Option<String>,
        credential_ttl_secs: Option<i64>,
        resolver_env: Option<String>,
        // Appended, never inserted: JS has no keyword arguments, so every
        // existing positional `runStart(…)` call must keep meaning what it
        // meant. A new knob goes at the end of the list, always.
        max_effects_per_attempt: Option<u32>,
        llm_tool_result_chars: Option<u32>,
        llm_context_tokens: Option<i64>,
    ) -> napi::Result<napi::bindgen_prelude::AsyncTask<StringJob>> {
        // Built HERE, on the JS thread, before the job is queued — see
        // [`JsEvents`] for why a plain function cannot reach the event bus.
        let observer = js_observer(on_event)?;
        let slot = self.facade.clone();
        let path = self.path.clone();
        let ns = self.ns.clone();
        let actor = self.actor.clone();
        Ok(StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let egress = js_egress_handle(&path, &JsEgressPin {
                credentials, allow_hosts, tool_egress, credential_ttl_secs, resolver_env,
            })?;
            let input: serde_json::Value = match input_json {
                Some(raw) => serde_json::from_str(&raw)
                    .map_err(|e| err(AreevError::Validation(format!("inputJson: {e}"))))?,
                None => json!({}),
            };
            let h = areev_core::error::Hash::from_hex(&workflow).map_err(err)?;
            // Resolved before the run starts, so a bad model spec or a missing
            // key fails without journaling a run that cannot advance.
            let llm = resolve_toolcall_llm(model, base_url, key_env)?;
            let runner = js_runner_pinned(
                facade,
                ns,
                actor,
                tool_cmd,
                llm,
                JsExecutorPin { allow_executor, executor_cache, sandbox_cmd, executor_timeout_secs, tool_env },
                egress,
                observer,
            );
            let opts = js_run_options_full(
                max_tokens,
                max_usd_micros,
                max_wall_ms,
                ask_ttl_sec,
                JsRunLimits {
                    llm_max_tokens,
                    max_effects_per_attempt,
                    llm_tool_result_chars,
                    llm_context_tokens,
                },
            )?;
            let session = runner.start(&h, &run_id, input, &opts).map_err(run_err)?;
            Ok(run_session_json(session).to_string())
        }))
    }

    /// Resume a parked/interrupted run from its latest checkpoint.
    ///
    /// Takes `model` for the same reason `runStart` does: resuming a plan with
    /// abstract nodes still has to execute them, and the backend is host config
    /// that is deliberately not journaled with the run. Same reasoning for
    /// `onEvent`: a resume emits `RunResumed` and the rest of the stream —
    /// including the `RunFinished` a parked start leg never reached — so a
    /// host that watched the start must be able to watch the rest.
    #[napi(ts_return_type = "Promise<string>")]
    #[allow(clippy::too_many_arguments)] // a flat FFI surface; each knob is a distinct scalar
    pub fn run_resume(
        &self,
        run_id: String,
        tool_cmd: Option<String>,
        model: Option<String>,
        base_url: Option<String>,
        key_env: Option<String>,
        llm_max_tokens: Option<u32>,
        allow_executor: Option<String>,
        executor_cache: Option<String>,
        sandbox_cmd: Option<String>,
        executor_timeout_secs: Option<i64>,
        tool_env: Option<String>,
        on_event: Option<napi::bindgen_prelude::Function<String, ()>>,
        credentials: Option<String>,
        allow_hosts: Option<String>,
        tool_egress: Option<String>,
        credential_ttl_secs: Option<i64>,
        resolver_env: Option<String>,
    ) -> napi::Result<napi::bindgen_prelude::AsyncTask<StringJob>> {
        let observer = js_observer(on_event)?;
        let slot = self.facade.clone();
        let path = self.path.clone();
        let ns = self.ns.clone();
        let actor = self.actor.clone();
        Ok(StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let egress = js_egress_handle(&path, &JsEgressPin {
                credentials, allow_hosts, tool_egress, credential_ttl_secs, resolver_env,
            })?;
            let llm = resolve_toolcall_llm(model, base_url, key_env)?;
            let runner = js_runner_pinned(
                facade,
                ns,
                actor,
                tool_cmd,
                llm,
                JsExecutorPin { allow_executor, executor_cache, sandbox_cmd, executor_timeout_secs, tool_env },
                egress,
                observer,
            );
            // No `maxEffectsPerAttempt` here on purpose: it was frozen into the
            // manifest at start and is read back from there, so a resume cannot
            // re-bound a loop the start already bounded.
            let opts = js_run_options_full(
                None,
                None,
                None,
                None,
                JsRunLimits { llm_max_tokens, ..Default::default() },
            )?;
            let session = runner.resume(&run_id, &opts).map_err(run_err)?;
            Ok(run_session_json(session).to_string())
        }))
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

    /// Queue a steering message: the next superstep hands it to its nodes
    /// under `$inbox`.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn run_input(
        &self,
        run_id: String,
        message: String,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = self.ns.clone();
        let actor = self.actor.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let runner = js_runner(facade, ns, actor.clone(), None);
            runner.input(&run_id, &message, &actor).map_err(run_err)?;
            Ok(json!({"queued": run_id, "by": actor}).to_string())
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

    /// Ask a live run to PAUSE at its next superstep boundary (#344): the
    /// open superstep finishes and checkpoints, nothing past it dispatches,
    /// and the driving `runStart`/`runResume` returns `{"parked": …}` with
    /// `kind`/`reason` `"paused"` (its `onEvent` stream ends at `RunPaused`).
    /// `runResume` continues it under the same run id, manifest and pins.
    ///
    /// Safe to call from an `onEvent` callback — that is the shape a host
    /// metering work in its own units uses. Needs `run.execute`, the grant
    /// `runResume` takes. Idempotent (`already: true` answers the standing
    /// request); rejects with `RUN-E029` on a finished run or a pending
    /// cancel. Returns the receipt JSON: `run_id`, `status`
    /// (`requested` | `paused`), `already`, `request`, `paused_by`,
    /// `because`, `requested_at`.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn run_pause(
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
            let receipt = runner
                .pause(&run_id, &actor, because.as_deref().unwrap_or("paused"))
                .map_err(run_err)?;
            serde_json::to_string(&receipt).map_err(err)
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
    ///
    /// `optionsJson` is a JSON object (#277). `{"reexecute": "pure"}`
    /// rehearses a candidate VERSION rather than only a candidate plan: a
    /// bound node whose candidate Definition is a pure `wasm32-areev` module
    /// is RE-RUN in the sandbox on the replayed input instead of being
    /// answered from the journal, so a patch that changes only a tool's bytes
    /// stops rehearsing as `same`. That needs the same host authorization a
    /// run needs, carried in the same object: `allow_executor` /
    /// `allowExecutor`, `sandbox_cmd`, `executor_cache`,
    /// `executor_timeout_secs`. Everything else still answers from the
    /// journal and is reported under `not_reexecuted` with the reason.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn run_shadow(
        &self,
        run_ids: Vec<String>,
        plan: Option<String>,
        plan_body: Option<String>,
        options_json: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = self.ns.clone();
        let actor = self.actor.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let (opts, pin) = js_shadow_options(options_json)?;
            let runner = js_runner_pinned(facade, ns, actor, None, None, pin, None, None);
            // With `plan` (a Workflow hash) or `planBody` (an unstored draft,
            // a JSON object string) this is the plan-change rehearsal: the
            // runs re-driven under the candidate, effects answered from the
            // journal, nothing dispatched or written.
            let candidate = match (plan, plan_body) {
                (Some(_), Some(_)) => return Err(err("give plan or planBody, not both")),
                (Some(h), None) => Some(areev_run::PlanCandidate::Hash(
                    areev_core::error::Hash::from_hex(&h).map_err(|e| err(e.to_string()))?,
                )),
                (None, Some(body)) => {
                    let v: serde_json::Value =
                        serde_json::from_str(&body).map_err(|e| err(format!("planBody: {e}")))?;
                    let fields = v.as_object().cloned().ok_or_else(|| err("planBody must be a JSON object"))?;
                    Some(areev_run::PlanCandidate::Body(fields))
                }
                (None, None) => None,
            };
            match candidate {
                Some(c) => {
                    let report = runner.shadow_plan_with(&run_ids, &c, &opts).map_err(err)?;
                    serde_json::to_string(&report).map_err(err)
                }
                None if opts.reexecute != areev_run::Reexecute::Off => {
                    Err(err("options.reexecute needs a candidate: pass plan or planBody"))
                }
                None => {
                    let report = runner.shadow_eval(&run_ids);
                    serde_json::to_string(&report).map_err(err)
                }
            }
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
    /// Returns `[{op_seq, hlc, op, hash, ns}...]`, ascending; pass the last
    /// `op_seq` back as the next cursor. `hlc` is a STRING: HLC values
    /// exceed 2^53 and would silently lose bits in `JSON.parse`.
    ///
    /// `ns` (a name or a comma list) narrows the feed to those namespaces
    /// and attributes every row, TOMBSTONES INCLUDED (#307) — resolving a
    /// forget's hash cannot, because the grain is gone. `op_seq` stays the
    /// memory-wide sequence, so a scoped cursor is still comparable with an
    /// unscoped one.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn changes_since(
        &self,
        after_op_seq: Option<i64>,
        limit: Option<u32>,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let after = after_op_seq.unwrap_or(0);
        let limit = (limit.unwrap_or(500) as usize).clamp(1, 10_000);
        let scope = ns.map(|n| split_ns_list(&n)).unwrap_or_default();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            // A named scope is checked outright, so an ungranted namespace is
            // a refusal rather than a silently short feed. An UNSCOPED call is
            // the memory-wide feed — it stays available (an owner replicates
            // with it) but discloses only the namespaces this principal reads.
            check_verb_all(&facade, areev_core::authz::Verb::Read, &scope)?;
            let rows = facade
                .with_store(|m| m.changes_since_scoped(after, &scope, limit))
                .map_err(err)?;
            // A row we cannot attribute to a namespace is not one we can prove
            // this principal may see, so it drops out for everyone but the owner.
            let rows = filter_readable(&facade, rows, |r| r.ns.clone().unwrap_or_default());
            let out: Vec<serde_json::Value> = rows
                .into_iter()
                .map(|r| {
                    json!({"op_seq": r.op_seq, "hlc": r.hlc.to_string(), "op": r.op,
                           "hash": r.hash.to_hex(), "ns": r.ns})
                })
                .collect();
            serde_json::to_string(&out).map_err(err)
        })
    }

    /// Drop every recall-telemetry row for one exact namespace (#306).
    ///
    /// Reaches the row no other scrub can: a zero-result free-text query
    /// names no grain hash, so the per-hash scrub cannot find it, and the
    /// per-subject scrub only reaches it if the erased identity happens to
    /// appear in the text.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn telemetry_scrub_namespace(
        &self,
        ns: String,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            facade
                .store_checked(areev_core::authz::Verb::Erase, &ns, |m| m.telemetry_scrub_namespace(&ns))
                .map_err(err)?;
            Ok(json!({"scrubbed": ns}).to_string())
        })
    }

    /// A value that changes whenever this memory's authorization policy does
    /// (#309) — a single indexed read a host caching bound sessions makes
    /// per request instead of re-resolving grants. A CHANGE DETECTOR:
    /// compare for equality, never for ordering.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn authz_epoch(&self) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let epoch = facade.authz_epoch().map_err(err)?;
            // A STRING, like `hlc`: the value can exceed 2^53 and would
            // silently lose bits in `JSON.parse`.
            Ok(json!({"epoch": epoch.to_string()}).to_string())
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

    // ── triggers: standing rules that start workflows ────────────────────
    //
    // `areev trigger` in library form. There is still no daemon and no
    // scheduler: the cadence is data in the memory and evaluation is a call
    // the host makes on its own heartbeat. `triggerRun` is one-shot and
    // idempotent, so it is safe to invoke concurrently from several nodes.

    /// Declare a trigger. `fieldsJson` is the same object `add("trigger", …)`
    /// takes (`kind`, `workflow`, and the kind's own requirements); `because`
    /// records why the rule exists, which is what makes it auditable.
    ///
    /// Prefer this over `add("trigger", …)`: both refuse an incoherent
    /// declaration, but only this path also parses the cron expression,
    /// refuses a non-UTC timezone (`TRG-E006`) and checks a composite's gate
    /// against its own members. A trigger that can never fire has exactly one
    /// symptom — nothing happening — so it is worth refusing at authoring
    /// time. Resolves to the content address.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn trigger_add(
        &self,
        fields_json: String,
        because: String,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let default_ns = self.ns.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let mut fields: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(&fields_json).map_err(err)?;
            fields
                .entry("namespace".to_string())
                .or_insert_with(|| json!(ns.unwrap_or(default_ns.clone())));
            fields.insert("because".to_string(), json!(because));
            validated_cal_add(&facade, "trigger", &fields).map(|h| h.to_hex()).map_err(err)
        })
    }

    /// Every trigger declaration in this namespace, newest first. JSON list.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn trigger_list(&self) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = self.ns.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let rows = js_read_only_evaluator(facade, &ns).declarations().map_err(err)?;
            let rows: Vec<_> = rows
                .iter()
                .map(|(h, t)| {
                    json!({
                        "trigger": h, "name": areev_trigger::trigger_name(t),
                        "kind": t.kind.as_str(), "workflow": t.workflow,
                        "connector": t.connector, "scope": t.scope, "enabled": t.enabled,
                    })
                })
                .collect();
            Ok(json!(rows).to_string())
        })
    }

    /// Runtime state for every trigger: due, paused, leased, exhausted, the
    /// last firing and the last error. JSON list, parity with
    /// `areev trigger status`.
    ///
    /// **Answers for THIS host.** Evaluation state lives in `trg:<hash>` meta
    /// rows that deliberately do not replicate, so a memory restored from
    /// production cannot inherit production's cursor and silently skip real
    /// work. A dashboard rendering this is reporting its own machine's
    /// scheduling health, not the fleet's.
    ///
    /// A row carrying `unusable` can never fire as written — a cron that does
    /// not parse, a timezone this build refuses, a composite gate naming a
    /// member the declaration does not carry. Reported rather than left to
    /// look like a healthy trigger waiting its turn.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn trigger_status(&self) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = self.ns.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let rows = js_read_only_evaluator(facade, &ns).status().map_err(err)?;
            serde_json::to_string(&rows).map_err(err)
        })
    }

    /// One trigger's state, by content address or a unique prefix of one.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn trigger_show(&self, trigger: String) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = self.ns.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let found = js_read_only_evaluator(facade, &ns)
                .status()
                .map_err(err)?
                .into_iter()
                .find(|s| s.trigger.starts_with(&trigger))
                .ok_or_else(|| err(format!("no trigger matching '{trigger}' in {ns}")))?;
            serde_json::to_string(&found).map_err(err)
        })
    }

    /// Evaluate every due trigger once and resolve to the report.
    ///
    /// The whole command in one call: claim, poll, dedup, start. `dryRun`
    /// reports what would happen and touches nothing — the safe first call on
    /// a new deployment. `connectorCmd` executes polling connectors (without
    /// one a due polling trigger fails loudly with `TRG-E003` rather than
    /// looking healthy while doing nothing), and `toolCmd` is what lets a
    /// firing actually start its workflow — without it the pass ingests items
    /// and records firings but starts nothing, which is a useful mode rather
    /// than a broken one.
    ///
    /// `credentialsJson` maps a credential name to the **environment
    /// variable** its value is read from (`{"gmail": "GMAIL_TOKEN"}`), never
    /// to the value itself: the connector is handed the broker's address and
    /// never the secret. An unset variable is refused here rather than leaving
    /// the connector to make an unauthenticated call.
    ///
    /// `allowExecutor` reaches the firing's runner exactly as it does
    /// `runStart` (#90), so a plan with a code-carrying or sandboxed node
    /// executes from a trigger exactly as it does by hand. Without the pin
    /// such a node refuses with `RUN-E018` — the authorization to execute code
    /// must come from the host, never from the file that carries it.
    #[napi(ts_return_type = "Promise<string>")]
    #[allow(clippy::too_many_arguments)] // a flat FFI surface; each knob is a distinct scalar
    pub fn trigger_run(
        &self,
        only: Option<String>,
        dry_run: Option<bool>,
        lease_secs: Option<u32>,
        max_items: Option<u32>,
        connector_cmd: Option<String>,
        tool_cmd: Option<String>,
        credentials_json: Option<String>,
        node: Option<String>,
        model: Option<String>,
        base_url: Option<String>,
        key_env: Option<String>,
        max_tokens: Option<i64>,
        max_usd_micros: Option<i64>,
        max_wall_ms: Option<i64>,
        ask_ttl_sec: Option<i64>,
        llm_max_tokens: Option<u32>,
        allow_executor: Option<String>,
        executor_cache: Option<String>,
        sandbox_cmd: Option<String>,
        executor_timeout_secs: Option<i64>,
        tool_env: Option<String>,
        credentials: Option<String>,
        allow_hosts: Option<String>,
        tool_egress: Option<String>,
        credential_ttl_secs: Option<i64>,
        resolver_env: Option<String>,
        max_effects_per_attempt: Option<u32>,
        llm_tool_result_chars: Option<u32>,
        llm_context_tokens: Option<i64>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let path = self.path.clone();
        let ns = self.ns.clone();
        let db = self.path.clone();
        let actor = self.actor.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let llm = resolve_toolcall_llm(model, base_url, key_env)?;
            let ev = js_evaluator(
                facade,
                &path,
                ns,
                db,
                actor,
                connector_cmd,
                tool_cmd,
                credentials_json,
                llm,
                JsExecutorPin { allow_executor, executor_cache, sandbox_cmd, executor_timeout_secs, tool_env },
                JsEgressPin { credentials, allow_hosts, tool_egress, credential_ttl_secs, resolver_env },
                js_run_options_full(max_tokens, max_usd_micros, max_wall_ms, ask_ttl_sec,
                                    JsRunLimits {
                    llm_max_tokens,
                    max_effects_per_attempt,
                    llm_tool_result_chars,
                    llm_context_tokens,
                })?,
            )?;
            let mut opts = areev_trigger::EvalOptions {
                dry_run: dry_run.unwrap_or(false),
                only,
                ..Default::default()
            };
            if let Some(secs) = lease_secs {
                opts.lease = std::time::Duration::from_secs(secs as u64);
            }
            if let Some(n) = max_items {
                opts.max_items = n as usize;
            }
            if let Some(n) = node {
                opts.node = n;
            }
            let report = ev.run(&opts).map_err(err)?;
            serde_json::to_string(&report).map_err(err)
        })
    }

    /// Hand a webhook or manual payload to a trigger. Areev never opens a
    /// port: the host owns the listener and hands the payload over.
    ///
    /// Takes the same budgets and the same executor pin as `triggerRun`: a
    /// delivery starts a real run.
    #[napi(ts_return_type = "Promise<string>")]
    #[allow(clippy::too_many_arguments)] // a flat FFI surface; each knob is a distinct scalar
    pub fn trigger_deliver(
        &self,
        trigger: String,
        payload_json: String,
        connector_cmd: Option<String>,
        tool_cmd: Option<String>,
        credentials_json: Option<String>,
        model: Option<String>,
        base_url: Option<String>,
        key_env: Option<String>,
        max_tokens: Option<i64>,
        max_usd_micros: Option<i64>,
        max_wall_ms: Option<i64>,
        ask_ttl_sec: Option<i64>,
        llm_max_tokens: Option<u32>,
        allow_executor: Option<String>,
        executor_cache: Option<String>,
        sandbox_cmd: Option<String>,
        executor_timeout_secs: Option<i64>,
        tool_env: Option<String>,
        credentials: Option<String>,
        allow_hosts: Option<String>,
        tool_egress: Option<String>,
        credential_ttl_secs: Option<i64>,
        resolver_env: Option<String>,
        max_effects_per_attempt: Option<u32>,
        llm_tool_result_chars: Option<u32>,
        llm_context_tokens: Option<i64>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let path = self.path.clone();
        let ns = self.ns.clone();
        let db = self.path.clone();
        let actor = self.actor.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let payload: serde_json::Value = serde_json::from_str(&payload_json)
                .map_err(|e| err(format!("payloadJson is not JSON: {e}")))?;
            let llm = resolve_toolcall_llm(model, base_url, key_env)?;
            let ev = js_evaluator(
                facade,
                &path,
                ns,
                db,
                actor,
                connector_cmd,
                tool_cmd,
                credentials_json,
                llm,
                JsExecutorPin { allow_executor, executor_cache, sandbox_cmd, executor_timeout_secs, tool_env },
                JsEgressPin { credentials, allow_hosts, tool_egress, credential_ttl_secs, resolver_env },
                js_run_options_full(max_tokens, max_usd_micros, max_wall_ms, ask_ttl_sec,
                                    JsRunLimits {
                    llm_max_tokens,
                    max_effects_per_attempt,
                    llm_tool_result_chars,
                    llm_context_tokens,
                })?,
            )?;
            let report = ev.deliver(&trigger, payload).map_err(err)?;
            serde_json::to_string(&report).map_err(err)
        })
    }

    /// Stop a trigger firing without deleting it. Mandatory reason.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn trigger_pause(
        &self,
        trigger: String,
        because: String,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        self.set_trigger_paused(trigger, because, true)
    }

    /// Let a paused trigger fire again. Mandatory reason.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn trigger_resume(
        &self,
        trigger: String,
        because: String,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        self.set_trigger_paused(trigger, because, false)
    }

    /// Render heartbeat config for infrastructure you already run
    /// (`cron`, `launchd`, `systemd`, `k8s-cronjob`) and create nothing.
    ///
    /// The rendered interval is the GCD of the declared intervals floored at
    /// 60s, not the shortest one — the memory owns the real cadence, so this
    /// is deliberately coarser. Resolves to
    /// `{"target", "heartbeatSecs", "config"}`; `config` is the text to
    /// install.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn trigger_render(
        &self,
        target: String,
        db: String,
        exe: Option<String>,
        extra_args: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = self.ns.clone();
        StringJob::spawn(move || {
            let facade = take_facade(&slot)?;
            let declarations: Vec<_> = js_read_only_evaluator(facade, &ns)
                .declarations()
                .map_err(err)?
                .into_iter()
                .map(|(_, t)| t)
                .collect();
            let heartbeat = areev_trigger::render::heartbeat_secs(&declarations);
            // The host is not the `areev` binary here, so there is no
            // current_exe() worth guessing from — name it or get the one on
            // PATH.
            let exe = exe.unwrap_or_else(|| "areev".to_string());
            let extra = extra_args.unwrap_or_default();
            let ctx = areev_trigger::render::RenderContext {
                exe: &exe,
                db: &db,
                ns: &ns,
                heartbeat_secs: heartbeat,
                extra_args: &extra,
            };
            let config = areev_trigger::render::render(&target, &ctx).map_err(err)?;
            Ok(json!({
                "target": target,
                "heartbeatSecs": heartbeat,
                "config": config,
            })
            .to_string())
        })
    }
}

impl Areev {
    /// Pause/resume share one read-modify-write against the exact prior row,
    /// so toggling must not clobber a cursor a concurrent firing just
    /// advanced — a lost cursor silently replays or skips a mailbox.
    fn set_trigger_paused(
        &self,
        trigger: String,
        because: String,
        paused: bool,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let slot = self.facade.clone();
        let ns = self.ns.clone();
        StringJob::spawn(move || {
            if because.trim().is_empty() {
                return Err(err(
                    "because is required: pausing a standing rule is an auditable act",
                ));
            }
            let facade = take_facade(&slot)?;
            // Pausing a standing rule changes what the memory does next.
            check_verb(&facade, areev_core::authz::Verb::Write, &ns)?;
            let target = js_read_only_evaluator(std::sync::Arc::clone(&facade), &ns)
                .declarations()
                .map_err(err)?
                .into_iter()
                .find(|(h, _)| h.starts_with(&trigger))
                .ok_or_else(|| err(format!("no trigger matching '{trigger}' in {ns}")))?
                .0;
            let (mut state, raw) = facade
                .with_store(|m| m.trigger_state(&target))
                .map_err(err)?
                .map(|(st, r)| (st, Some(r)))
                .unwrap_or_default();
            state.paused = paused;
            let ok = facade
                .with_store(|m| m.put_trigger_state(&target, raw.as_deref(), &state))
                .map_err(err)?;
            if !ok {
                return Err(err(format!(
                    "trigger {target} changed underneath this call (a firing is in progress) — retry"
                )));
            }
            Ok(json!({"trigger": target, "paused": paused, "because": because}).to_string())
        })
    }
}


/// Map a runtime failure into a napi error (the RUN-Ennn code leads).
fn run_err(e: areev_run::CoreRunError) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

/// The runtime driver over one shared facade (Wave 5 JS parity).
///
/// No tool-calling model: the read-only and non-advancing verbs cannot reach
/// an abstract node, so wiring one would only be misleading.
fn js_runner(
    facade: std::sync::Arc<AreevFacade>,
    ns: String,
    principal: String,
    tool_cmd: Option<String>,
) -> areev_run::Runner {
    js_runner_with_llm(facade, ns, principal, tool_cmd, None)
}

/// [`js_runner`] with the tool-calling backend abstract nodes need.
fn js_runner_with_llm(
    facade: std::sync::Arc<AreevFacade>,
    ns: String,
    principal: String,
    tool_cmd: Option<String>,
    llm: Option<std::sync::Arc<dyn areev_llm::ToolCallLlm>>,
) -> areev_run::Runner {
    js_runner_pinned(facade, ns, principal, tool_cmd, llm, JsExecutorPin::default(), None, None)
}

/// The host's authorization to execute code-carrying tools, carried as one
/// value so the trigger surface takes the same four settings `runStart` does
/// without growing four more positional parameters at every call site.
/// `toolEnv` → an allow-list policy, warning on any name already registered as
/// holding a secret. One helper so the connector and the run executors cannot
/// drift apart.
fn js_tool_env_policy(names: Option<&str>) -> Option<areev_core::proc::EnvPolicy> {
    // Presence is the setting (`docs/run.md`): `toolEnv: ""` clears to the
    // minimal set, the strictest posture, exactly as the CLI's `--tool-env ""`
    // does. Only `null` keeps the inherit default. Filtering the empty string
    // out here used to turn the strictest request into the loosest answer.
    let names = names.map(str::trim)?;
    let (policy, dropped) = areev_run::env_allow_policy(names);
    if !dropped.is_empty() {
        eprintln!("areev: toolEnv dropped {} — registered as holding a secret", dropped.join(", "));
    }
    Some(policy)
}

/// The `optionsJson` object a `runShadow` takes (#277): the re-execution
/// mode plus the host pins a pure re-execution needs, since re-running a
/// candidate's module is the same act as running it — the authorization has
/// to come from the host, never from the file.
///
/// snake_case is canonical (the report's own fields are), and the camelCase
/// spelling is accepted too, so ONE documented object works from Node and
/// from Python rather than two that drift.
fn js_shadow_options(
    options: Option<String>,
) -> napi::Result<(areev_run::ShadowOptions, JsExecutorPin)> {
    let Some(text) = options else {
        return Ok((areev_run::ShadowOptions::default(), JsExecutorPin::default()));
    };
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| err(format!("optionsJson: {e}")))?;
    let obj = v.as_object().ok_or_else(|| err("optionsJson must be a JSON object"))?;
    let pick = |snake: &str, camel: &str| obj.get(snake).or_else(|| obj.get(camel));
    let string = |snake: &str, camel: &str| -> napi::Result<Option<String>> {
        match pick(snake, camel) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(err(format!("optionsJson.{snake} must be a string"))),
        }
    };
    let reexecute = match string("reexecute", "reexecute")? {
        None => areev_run::Reexecute::Off,
        Some(mode) => areev_run::Reexecute::parse(&mode).ok_or_else(|| {
            err(format!("optionsJson.reexecute takes 'pure' or 'off', not {mode:?}"))
        })?,
    };
    let pin = JsExecutorPin {
        allow_executor: string("allow_executor", "allowExecutor")?,
        executor_cache: string("executor_cache", "executorCache")?,
        sandbox_cmd: string("sandbox_cmd", "sandboxCmd")?,
        executor_timeout_secs: match pick("executor_timeout_secs", "executorTimeoutSecs") {
            None | Some(serde_json::Value::Null) => None,
            Some(n) => Some(
                n.as_i64()
                    .ok_or_else(|| err("optionsJson.executor_timeout_secs must be a whole number"))?,
            ),
        },
        tool_env: None,
    };
    Ok((areev_run::ShadowOptions::reexecute(reexecute), pin))
}

#[derive(Default)]
struct JsExecutorPin {
    allow_executor: Option<String>,
    executor_cache: Option<String>,
    sandbox_cmd: Option<String>,
    executor_timeout_secs: Option<i64>,
    /// Comma list of variables a host tool may keep. Unset inherits this
    /// process's environment minus the registered secrets; a list — the
    /// empty list included — clears it and passes only those, plus the
    /// minimal set a command needs to start.
    tool_env: Option<String>,
}

/// The pin-aware factory (#87): `allowExecutor` is the same comma list as
/// the CLI's `--allow-executor` — without it, a plan naming a code-carrying
/// Definition refuses at start (RUN-E018), because the authorization to
/// execute code must come from the host, never the file.
/// `executorTimeoutSecs` (#133) overrides the fixed 300s ceiling either
/// executor otherwise runs a tool under — `0` waits forever.
/// `observer` is the §6.10 event sink (#182) — `None` for the verbs that do
/// not advance a run, since they emit nothing to watch.
#[allow(clippy::too_many_arguments)]
/// The credential broker's settings (#201): the CLI's `--credential`,
/// `--allow-host`, `--tool-egress`, `--credential-ttl` and `--resolver-env`
/// spec strings, verbatim, parsed by `areev_run::EgressSpec`.
#[derive(Default)]
struct JsEgressPin {
    credentials: Option<String>,
    allow_hosts: Option<String>,
    tool_egress: Option<String>,
    credential_ttl_secs: Option<i64>,
    resolver_env: Option<String>,
}

impl JsEgressPin {
    fn spec(&self) -> Result<areev_run::EgressSpec, String> {
        let some = |v: &Option<String>| {
            v.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
        };
        let ttl = match self.credential_ttl_secs {
            None => None,
            Some(n) if n >= 0 => Some(n as u64),
            Some(n) => return Err(format!("credentialTtlSecs: expected whole seconds, got {n}")),
        };
        Ok(areev_run::EgressSpec {
            credentials: some(&self.credentials),
            allow_hosts: some(&self.allow_hosts),
            tool_egress: some(&self.tool_egress),
            credential_ttl_secs: ttl,
            resolver_env: some(&self.resolver_env),
        })
    }
}

/// The broker `egress` describes — serving the memory at `path`'s blobs on
/// the same token — or None when nothing was configured.
fn js_egress_handle(
    path: &str,
    egress: &JsEgressPin,
) -> napi::Result<Option<areev_run::EgressHandle>> {
    match egress.spec().map_err(err)?.build().map_err(err)? {
        None => Ok(None),
        Some(broker) => {
            broker.serve_blobs(path);
            Ok(Some(areev_run::EgressHandle::new(std::sync::Arc::new(broker))))
        }
    }
}

#[allow(clippy::too_many_arguments)] // a flat FFI surface; each knob is a distinct scalar
fn js_runner_pinned(
    facade: std::sync::Arc<AreevFacade>,
    ns: String,
    principal: String,
    tool_cmd: Option<String>,
    llm: Option<std::sync::Arc<dyn areev_llm::ToolCallLlm>>,
    pin: JsExecutorPin,
    egress: Option<areev_run::EgressHandle>,
    observer: Option<std::sync::Arc<dyn areev_run::RunObserver>>,
) -> areev_run::Runner {
    let timeout = pin.executor_timeout_secs.map(|secs| {
        if secs <= 0 { None } else { Some(std::time::Duration::from_secs(secs as u64)) }
    });
    let env = js_tool_env_policy(pin.tool_env.as_deref());
    let base: std::sync::Arc<dyn areev_run::HostToolExecutor> = match tool_cmd {
        Some(cmd) if !cmd.trim().is_empty() => {
            let mut ce = areev_run::CommandExecutor::new(&cmd);
            if let Some(t) = timeout {
                ce = ce.with_timeout(t);
            }
            if let Some(p) = env.clone() {
                ce = ce.with_env_policy(p);
            }
            if let Some(h) = &egress {
                ce = ce.with_egress(h.clone());
            }
            std::sync::Arc::new(ce)
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
    let executor: std::sync::Arc<dyn areev_run::HostToolExecutor> = match pin.allow_executor {
        None => base,
        Some(list) => {
            let mut ce = areev_run::CodeExecutor::new(base);
            for addr in list.split(',').map(str::trim).filter(|a| !a.is_empty()) {
                ce = ce.allow(addr);
            }
            if let Some(dir) = pin.executor_cache {
                ce = ce.cache_dir(dir);
            }
            if let Some(cmd) = pin.sandbox_cmd {
                ce = ce.sandbox_cmd(&cmd);
            }
            if let Some(t) = timeout {
                ce = ce.with_timeout(t);
            }
            if let Some(p) = env {
                ce = ce.with_env_policy(p);
            }
            if let Some(h) = egress {
                ce = ce.with_egress(h);
            }
            std::sync::Arc::new(ce)
        }
    };
    areev_run::Runner {
        facade,
        clock: std::sync::Arc::new(areev_run::SystemClock),
        executor,
        llm,
        observer,
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
    js_run_options_full(max_tokens, max_usd_micros, max_wall_ms, ask_ttl_sec, JsRunLimits::default())
}

/// The run-level LLM ceilings a run inherits at start, frozen into its
/// manifest. A struct rather than more positional arguments to
/// [`js_run_options_full`]: they are all small integer options, so a swapped
/// pair would compile and silently bound the wrong thing.
#[derive(Default, Clone, Copy)]
struct JsRunLimits {
    llm_max_tokens: Option<u32>,
    max_effects_per_attempt: Option<u32>,
    llm_tool_result_chars: Option<u32>,
    llm_context_tokens: Option<i64>,
}

fn js_run_options_full(
    max_tokens: Option<i64>,
    max_usd_micros: Option<i64>,
    max_wall_ms: Option<i64>,
    ask_ttl_sec: Option<i64>,
    limits: JsRunLimits,
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
            ..Default::default()
        },
        ask_ttl_sec,
        workers: 4,
        on_dangling: Default::default(),
        llm_max_tokens: limits.llm_max_tokens,
        max_effects_per_attempt: limits.max_effects_per_attempt,
        llm_tool_result_chars: limits.llm_tool_result_chars.map(|n| n as usize),
        llm_context_tokens: u("llmContextTokens", limits.llm_context_tokens)?,
        inject_crash: None,
        ..Default::default()
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
