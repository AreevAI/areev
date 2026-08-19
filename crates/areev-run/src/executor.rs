//! Host tool execution (§4's concurrency rule): a bounded thread pool runs
//! effects in parallel; ALL store writes stay on the driver thread. The
//! executor is a host-supplied trait — Areev never executes anything
//! itself (Tool grains are data; execution is the host's job), it hands the
//! host a name, a pinned definition hash, the input, and the deterministic
//! idempotency key the crash-redelivery contract depends on (§5.3).

use areev_run_core::{EffectOutcome, FailCause, JournalKey, NodeExecutor};
use serde_json::Value;
use std::sync::mpsc;
use std::sync::Arc;

/// What a host tool run produced. The driver turns this into an
/// [`EffectOutcome`] with deterministic storage accounting.
pub enum ExecResult {
    Ok(Value),
    Err { cause: FailCause, detail: String },
}

/// The host's executor. `idempotency_key` is stable across crash
/// redeliveries of the same attempt — an executor that performs external
/// side effects should deduplicate on it; one that cannot should be run
/// with `on_dangling = fail`.
pub trait HostToolExecutor: Send + Sync {
    fn execute(
        &self,
        tool_name: &str,
        tool_hash: &str,
        input: &Value,
        idempotency_key: &str,
    ) -> ExecResult;

    /// May this host execute the code blob at `uri`, pinned by a Definition
    /// with address `tool_hash`?
    ///
    /// Checked at run start so an unpinned executor refuses before the run
    /// takes a lease, not three supersteps in. **The default is `false`** —
    /// a Definition can arrive by bundle import from an author nobody
    /// vouched for, so the authorization to execute has to come from the
    /// host, not from the memory. See [`CodeExecutor`].
    fn code_allowed(&self, _tool_hash: &str, _uri: &str) -> bool {
        false
    }

    /// Every distinct outbound refusal this executor has seen.
    ///
    /// Read, not drained: the driver journals them so a refusal is auditable
    /// from the memory rather than only from a terminal that has scrolled,
    /// and the CLI still prints the whole set when the run ends. Default:
    /// none, for executors with no egress.
    fn refusals(&self) -> Vec<crate::broker::EgressRefusal> {
        Vec::new()
    }

    /// Execute a content-addressed code blob. `bytes` were read and
    /// hash-verified on the driver thread; this runs on a pool worker.
    ///
    /// The default refuses, so a host that never opted in cannot be handed
    /// code by a plan.
    fn execute_code(
        &self,
        _tool_name: &str,
        _tool_hash: &str,
        uri: &str,
        _bytes: &[u8],
        _input: &Value,
        _idempotency_key: &str,
    ) -> ExecResult {
        ExecResult::Err {
            cause: FailCause::ExecutorError,
            detail: format!(
                "this host does not run code-carrying tools, so {uri} was not executed"
            ),
        }
    }
}

/// A registry-backed executor: tools dispatch by name; unknown names fail
/// with `ExecutorError` (never a panic — a plan naming a tool the host
/// didn't register is the host's configuration error, reported per attempt).
#[derive(Default)]
pub struct ExecutorRegistry {
    tools: std::collections::BTreeMap<String, Box<dyn HostToolExecutor>>,
}

impl ExecutorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool_name: &str, exec: Box<dyn HostToolExecutor>) {
        self.tools.insert(tool_name.to_string(), exec);
    }
}

impl HostToolExecutor for ExecutorRegistry {
    fn execute(
        &self,
        tool_name: &str,
        tool_hash: &str,
        input: &Value,
        idempotency_key: &str,
    ) -> ExecResult {
        match self.tools.get(tool_name) {
            Some(t) => t.execute(tool_name, tool_hash, input, idempotency_key),
            None => ExecResult::Err {
                cause: FailCause::ExecutorError,
                detail: format!("no executor registered for tool '{tool_name}'"),
            },
        }
    }
}

/// A subprocess executor — the CLI's zero-dependency host-tool seam, the
/// same pattern as `--embed-cmd` / `--llm-cmd`: the command receives the
/// input JSON on stdin and `AREEV_TOOL_NAME` / `AREEV_TOOL_HASH` /
/// `AREEV_IDEMPOTENCY_KEY` in the environment, and prints the result JSON on
/// stdout. Non-zero exit or unparseable output is an `ExecutorError`
/// (retryable per the §6.3 table). The subprocess NEVER opens the memory
/// file — the parent journals on its behalf (§4's single-writer rule, and
/// the process-wide open-path registry would refuse it anyway).
/// The broker a tool reaches instead of holding a token.
///
/// Held by the executor rather than the driver because the grants are host
/// configuration, not plan data — the same reason the code-executor allowlist
/// lives here. Cloning is cheap; the broker itself is shared.
#[derive(Clone)]
pub struct EgressHandle {
    broker: Arc<crate::broker::Broker>,
}

impl EgressHandle {
    pub fn new(broker: Arc<crate::broker::Broker>) -> Self {
        EgressHandle { broker }
    }
    fn refusals(&self) -> Vec<crate::broker::EgressRefusal> {
        self.broker.refusals()
    }
    /// The env a tool named `tool_name` gets. Empty when it has no grant, so
    /// a tool nobody authorized cannot even see the broker.
    fn env_for(&self, tool_name: &str) -> Vec<(&'static str, String)> {
        match self.broker.token_for(tool_name) {
            Some(tok) => vec![
                ("AREEV_EGRESS_URL", self.broker.url().to_string()),
                ("AREEV_EGRESS_TOKEN", tok.to_string()),
            ],
            None => Vec::new(),
        }
    }
}

pub struct CommandExecutor {
    cmd: String,
    /// Wall-clock ceiling per invocation. Before 1.3 there was none, and a tool
    /// that never exited held its pool worker — then the whole driver at the
    /// next wave boundary — indefinitely.
    timeout: Option<std::time::Duration>,
    /// When set, tools reach the network through the broker instead of
    /// holding credentials themselves.
    egress: Option<EgressHandle>,
}

impl CommandExecutor {
    pub fn new(cmd: &str) -> Self {
        CommandExecutor {
            cmd: cmd.to_string(),
            timeout: Some(areev_core::proc::DEFAULT_TIMEOUT),
            egress: None,
        }
    }

    /// Route this executor's tools through a credential broker.
    pub fn with_egress(mut self, egress: EgressHandle) -> Self {
        self.egress = Some(egress);
        self
    }

    /// Override the per-invocation ceiling. `None` restores the pre-1.3
    /// wait-forever behaviour for a host that genuinely wants it.
    pub fn with_timeout(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.timeout = timeout;
        self
    }
}

impl HostToolExecutor for CommandExecutor {
    fn refusals(&self) -> Vec<crate::broker::EgressRefusal> {
        self.egress.as_ref().map(|e| e.refusals()).unwrap_or_default()
    }

    fn execute(
        &self,
        tool_name: &str,
        tool_hash: &str,
        input: &Value,
        idempotency_key: &str,
    ) -> ExecResult {
        use areev_core::proc::{self, SpawnPolicy};
        use std::process::Command;
        // The platform shell: /bin/sh -c on unix, cmd /C on Windows. The
        // Windows command string must go through raw_arg — Command::arg
        // MSVC-quotes embedded quotes, which cmd.exe does not parse.
        #[cfg(not(windows))]
        let mut shell = Command::new("/bin/sh");
        #[cfg(not(windows))]
        shell.arg("-c").arg(&self.cmd);
        #[cfg(windows)]
        let mut shell = Command::new("cmd");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            shell.raw_arg("/C").raw_arg(&self.cmd);
        }
        let policy = SpawnPolicy { timeout: self.timeout, ..SpawnPolicy::default() };
        let mut env: Vec<(&str, &str)> = vec![
            ("AREEV_TOOL_NAME", tool_name),
            ("AREEV_TOOL_HASH", tool_hash),
            ("AREEV_IDEMPOTENCY_KEY", idempotency_key),
        ];
        // A tool never receives a credential value — it receives the broker's
        // address and a capability token, and posts the call it wants.
        let brokered = self.egress.as_ref().map(|e| e.env_for(tool_name)).unwrap_or_default();
        env.extend(brokered.iter().map(|(k, v)| (*k, v.as_str())));
        let out = match proc::run(
            shell,
            Some(input.to_string().as_bytes()),
            &env,
            &policy,
        ) {
            Ok(o) => o,
            Err(e) => {
                return ExecResult::Err {
                    cause: FailCause::ExecutorError,
                    detail: format!("spawn '{}': {e}", self.cmd),
                }
            }
        };
        // A timeout is its own cause, not a generic executor error: §6.3 makes
        // Timeout retryable for tool effects, so a wedged tool now gets the
        // node's retry budget instead of parking a pool worker forever.
        if out.timed_out {
            return ExecResult::Err {
                cause: FailCause::Timeout,
                detail: format!(
                    "tool command exceeded its {}s ceiling and was killed",
                    policy.timeout.map(|d| d.as_secs()).unwrap_or(0)
                ),
            };
        }
        if let Some(why) = out.failure("tool command") {
            return ExecResult::Err { cause: FailCause::ExecutorError, detail: why };
        }
        match serde_json::from_slice::<Value>(&out.stdout) {
            Ok(v) => ExecResult::Ok(v),
            Err(e) => ExecResult::Err {
                cause: FailCause::ExecutorError,
                detail: if out.stdout_truncated {
                    format!("tool output exceeded the capture cap and is not JSON: {e}")
                } else {
                    format!("tool output is not JSON: {e}")
                },
            },
        }
    }
}

/// Runs code-carrying tools, and refuses every address the host did not pin.
///
/// ## Why the allowlist is here and not in the memory
///
/// A `Tool` Definition can name its executor by content address
/// (`executor_uri: "cas://sha256:..."`), and the blob travels with the memory
/// — bundles carry blobs, so importing a peer's memory imports their
/// connector code. Auto-executing it would be remote code execution by
/// design, and the failure is not hypothetical: the January 2026 n8n
/// community-node compromise exfiltrated decrypted OAuth tokens, and the
/// malicious node never violated a sandbox. It read a credential it was
/// given and made a request it was allowed to make.
///
/// So this follows the split the codebase already makes twice — trigger
/// evaluation state does not replicate, and host config never lands in the
/// file:
///
/// > **The declaration replicates. The authorization to execute never does.**
///
/// An operator pins addresses with `--allow-executor`. An address that is
/// not pinned is refused at run start (`RUN-E018`) naming the address, so
/// the fix is a copy-paste. There is deliberately no grant form: `mg:permits`
/// Facts live in the file and replicate, and a permission that arrives in
/// the same bundle as the code it authorizes is not a permission.
///
/// ## Materialization
///
/// The blob is written to `<cache>/<hex>` once and reused. The path IS the
/// content address, so a poisoned cache entry cannot masquerade as another
/// executor, and `get_blob` re-verifies the digest on every dispatch anyway.
pub struct CodeExecutor {
    inner: Arc<dyn HostToolExecutor>,
    allowed: std::collections::BTreeSet<String>,
    cache_dir: std::path::PathBuf,
    timeout: Option<std::time::Duration>,
}

impl CodeExecutor {
    /// Wrap `inner`, which still handles every tool that names no executor.
    pub fn new(inner: Arc<dyn HostToolExecutor>) -> Self {
        CodeExecutor {
            inner,
            allowed: Default::default(),
            cache_dir: std::env::temp_dir().join("areev-executors"),
            timeout: Some(std::time::Duration::from_secs(300)),
        }
    }

    /// Pin one content address (64 hex, or a full `cas://sha256:` URI).
    pub fn allow(mut self, addr: &str) -> Self {
        self.allowed.insert(strip_cas(addr).to_ascii_lowercase());
        self
    }

    pub fn cache_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.cache_dir = dir.into();
        self
    }

    pub fn with_timeout(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    /// Write the blob to the cache if absent and return its path.
    fn materialize(&self, hex: &str, bytes: &[u8]) -> std::io::Result<std::path::PathBuf> {
        let path = self.cache_dir.join(hex);
        if path.exists() {
            return Ok(path);
        }
        std::fs::create_dir_all(&self.cache_dir)?;
        // Write-then-rename: a concurrent driver must never see a half-written
        // executor, and two of them racing must not corrupt one file. The
        // temp name carries the pid so the two do not collide either.
        let tmp = self.cache_dir.join(format!("{hex}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700))?;
        }
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }
}

/// `cas://sha256:<hex>` -> `<hex>`; anything else is returned unchanged.
pub(crate) fn strip_cas(uri: &str) -> &str {
    uri.strip_prefix("cas://sha256:").unwrap_or(uri)
}

impl HostToolExecutor for CodeExecutor {
    fn execute(
        &self,
        tool_name: &str,
        tool_hash: &str,
        input: &Value,
        idempotency_key: &str,
    ) -> ExecResult {
        self.inner.execute(tool_name, tool_hash, input, idempotency_key)
    }

    /// Pass through: the wrapped executor is the one holding a broker.
    fn refusals(&self) -> Vec<crate::broker::EgressRefusal> {
        self.inner.refusals()
    }

    fn code_allowed(&self, _tool_hash: &str, uri: &str) -> bool {
        self.allowed.contains(&strip_cas(uri).to_ascii_lowercase())
    }

    fn execute_code(
        &self,
        tool_name: &str,
        tool_hash: &str,
        uri: &str,
        bytes: &[u8],
        input: &Value,
        idempotency_key: &str,
    ) -> ExecResult {
        // Checked at start too. Re-checked here because a pool worker must
        // not depend on a caller having done it — the same reason the
        // destructive-ops cap is re-applied rather than trusted.
        if !self.code_allowed(tool_hash, uri) {
            return ExecResult::Err {
                cause: FailCause::ExecutorError,
                detail: format!("{uri} is not pinned by this host"),
            };
        }
        let hex = strip_cas(uri).to_ascii_lowercase();
        let path = match self.materialize(&hex, bytes) {
            Ok(p) => p,
            Err(e) => {
                return ExecResult::Err {
                    cause: FailCause::ExecutorError,
                    detail: format!("could not materialize {uri}: {e}"),
                }
            }
        };
        // Spawned directly rather than through a shell: the path is ours and
        // contains no metacharacters, and going direct means no quoting bug
        // can turn a cache path into an argument. A unix blob interprets its
        // own shebang; on Windows the OS decides, which is why a code-carrying
        // executor is platform-specific and the operator pins per platform.
        use areev_core::proc::{self, SpawnPolicy};
        let policy = SpawnPolicy { timeout: self.timeout, ..SpawnPolicy::default() };
        let out = match proc::run(
            std::process::Command::new(&path),
            Some(input.to_string().as_bytes()),
            &[
                ("AREEV_TOOL_NAME", tool_name),
                ("AREEV_TOOL_HASH", tool_hash),
                ("AREEV_IDEMPOTENCY_KEY", idempotency_key),
                ("AREEV_EXECUTOR_URI", uri),
            ],
            &policy,
        ) {
            Ok(o) => o,
            Err(e) => {
                return ExecResult::Err {
                    cause: FailCause::ExecutorError,
                    detail: format!("spawn {}: {e}", path.display()),
                }
            }
        };
        if out.timed_out {
            return ExecResult::Err {
                cause: FailCause::Timeout,
                detail: format!(
                    "executor {uri} exceeded its {}s ceiling and was killed",
                    policy.timeout.map(|d| d.as_secs()).unwrap_or(0)
                ),
            };
        }
        if let Some(why) = out.failure("executor") {
            return ExecResult::Err { cause: FailCause::ExecutorError, detail: why };
        }
        match serde_json::from_slice::<Value>(&out.stdout) {
            Ok(v) => ExecResult::Ok(v),
            Err(e) => ExecResult::Err {
                cause: FailCause::ExecutorError,
                detail: format!("executor output is not JSON: {e}"),
            },
        }
    }
}

/// A §6.10 TokenChunk sink: text deltas stream into it as they arrive.
pub type TokenSink = Box<dyn FnMut(&str) + Send>;

/// A prepared LLM request, built ON THE DRIVER THREAD (the pool never
/// touches the store): the transcript translated to seam messages plus the
/// pinned tool Definitions already fetched.
pub struct PreparedLlm {
    pub messages: Vec<areev_llm::ChatMessage>,
    pub tools: Vec<areev_core::types::Tool>,
    pub max_tokens: u32,
    /// §6.10 TokenChunk sink. None (no observer) uses the plain
    /// non-streaming call. Purely observational: the journaled result is
    /// the same either way.
    pub on_token: Option<TokenSink>,
}

/// One in-flight dispatch handed to the pool.
pub struct DispatchJob {
    pub key: JournalKey,
    pub executor: NodeExecutor,
    pub input: Value,
    pub idempotency_key: String,
    /// Present exactly when this is an LLM effect of an abstract node.
    pub llm: Option<PreparedLlm>,
    /// Present exactly when the pinned Definition named a `cas://` executor.
    /// Read and hash-verified on the driver thread, like `llm`'s pinned
    /// Definitions — the pool never touches a store handle.
    pub code: Option<PreparedCode>,
}

/// A code blob, read off the driver thread and ready to execute.
pub struct PreparedCode {
    /// The `cas://sha256:` address the Definition named.
    pub uri: String,
    /// The blob's bytes. `get_blob` verified the digest on read, so these
    /// bytes ARE the address — that is the whole integrity story.
    pub bytes: Vec<u8>,
}

/// A completed dispatch coming back to the driver thread. Carries the
/// executor the job DISPATCHED under — a flow tool inside an abstract node
/// runs as a Host tool while the node-level executor says Abstract, and the
/// result grain must re-state what actually ran (§5.1).
pub struct DispatchDone {
    pub key: JournalKey,
    pub executor: NodeExecutor,
    pub outcome: EffectOutcome,
}

/// The bounded pool: N worker threads, jobs in, completions out. Workers
/// touch NO store handle — parallelism lives here and only here. LLM
/// effects route to the tool-calling seam; everything else to the host
/// executor.
pub struct Pool {
    tx: mpsc::Sender<DispatchJob>,
    pub done_rx: mpsc::Receiver<DispatchDone>,
    _workers: Vec<std::thread::JoinHandle<()>>,
}

impl Pool {
    pub fn new(
        executor: Arc<dyn HostToolExecutor>,
        llm: Option<Arc<dyn areev_llm::ToolCallLlm>>,
        workers: usize,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<DispatchJob>();
        let rx = Arc::new(std::sync::Mutex::new(rx));
        let (done_tx, done_rx) = mpsc::channel::<DispatchDone>();
        let mut handles = Vec::new();
        for _ in 0..workers.max(1) {
            let rx = Arc::clone(&rx);
            let done_tx = done_tx.clone();
            let executor = Arc::clone(&executor);
            let llm = llm.clone();
            handles.push(std::thread::spawn(move || loop {
                let job = {
                    let guard = rx.lock().expect("pool queue");
                    guard.recv()
                };
                let Ok(job) = job else { return };
                if let Some(prepared) = job.llm {
                    // Same catch_unwind rule as the tool branch below: a
                    // panicking provider (SSE parser, token sink) must be a
                    // Failed effect, never a dead worker holding the pool
                    // half-alive.
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                        || run_llm_effect(llm.as_deref(), prepared),
                    ))
                    .unwrap_or_else(|p| {
                        let msg = p
                            .downcast_ref::<&str>()
                            .map(|s| s.to_string())
                            .or_else(|| p.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "llm executor panicked".into());
                        EffectOutcome::Failed {
                            cause: FailCause::ExecutorError,
                            detail: format!("llm panicked: {msg}"),
                            journal_bytes: 0,
                        }
                    });
                    let done = DispatchDone { key: job.key, executor: job.executor, outcome };
                    if done_tx.send(done).is_err() {
                        return;
                    }
                    continue;
                }
                let (tool_name, tool_hash) = match &job.executor {
                    NodeExecutor::Host { tool_name, tool_hash }
                    | NodeExecutor::Client { tool_name, tool_hash } => {
                        (tool_name.clone(), tool_hash.clone())
                    }
                    NodeExecutor::Subgraph { workflow_hash } => {
                        // Subgraphs never reach the pool — the driver runs
                        // the child inline (it needs the store). A job here
                        // is a driver bug, reported as a failed effect.
                        ("mg:subgraph".to_string(), workflow_hash.clone())
                    }
                    NodeExecutor::Abstract { .. } => {
                        ("mg:llm".to_string(), String::new())
                    }
                };
                // A panicking executor is a FAILED EFFECT, never a dead
                // worker: a worker that dies would leave the pool half-alive
                // and the driver blocked on `done_rx.recv()` forever (the
                // idle workers keep the channel open) — the hang the first
                // integration run of this crate actually produced. The
                // panic message survives in the failure detail; retryability
                // follows the normal §6.3 table (ExecutorError → retryable).
                let executed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    match &job.code {
                        Some(c) => executor.execute_code(
                            &tool_name,
                            &tool_hash,
                            &c.uri,
                            &c.bytes,
                            &job.input,
                            &job.idempotency_key,
                        ),
                        None => executor.execute(
                            &tool_name,
                            &tool_hash,
                            &job.input,
                            &job.idempotency_key,
                        ),
                    }
                }))
                .unwrap_or_else(|p| {
                    let msg = p
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| p.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "executor panicked".into());
                    ExecResult::Err {
                        cause: FailCause::ExecutorError,
                        detail: format!("executor panicked: {msg}"),
                    }
                });
                let outcome = match executed {
                    ExecResult::Ok(result) => {
                        let bytes = crate::journal::outcome_journal_bytes(
                            &EffectOutcome::Completed {
                                result: result.clone(),
                                journal_bytes: 0,
                                input_tokens: 0,
                                output_tokens: 0,
                                usd_micros: 0,
                            },
                        );
                        EffectOutcome::Completed {
                            result,
                            journal_bytes: bytes,
                            input_tokens: 0,
                            output_tokens: 0,
                            usd_micros: 0,
                        }
                    }
                    ExecResult::Err { cause, detail } => {
                        let bytes = detail.len() as u64;
                        EffectOutcome::Failed { cause, detail, journal_bytes: bytes }
                    }
                };
                let done = DispatchDone { key: job.key, executor: job.executor, outcome };
                if done_tx.send(done).is_err() {
                    return;
                }
            }));
        }
        Pool { tx, done_rx, _workers: handles }
    }

    pub fn submit(&self, job: DispatchJob) {
        let _ = self.tx.send(job);
    }
}

/// Execute one LLM turn through the tool-calling seam, converting the
/// response into the journal-shaped outcome the scheduler consumes
/// (`{"text", "tool_calls", "stop_reason"}` + usage figures).
fn run_llm_effect(
    llm: Option<&dyn areev_llm::ToolCallLlm>,
    prepared: crate::executor::PreparedLlm,
) -> EffectOutcome {
    let Some(llm) = llm else {
        return EffectOutcome::Failed {
            cause: FailCause::Unknown,
            detail: "no ToolCallLlm configured (RUN-E006 should have caught this at load)"
                .into(),
            journal_bytes: 0,
        };
    };
    let request = areev_llm::ToolCallRequest {
        system: Some(
            "You are executing one node of a governed workflow. Use the \
             offered tools when needed; when done, answer with the node's \
             final result (a JSON object when structured output is wanted)."
                .into(),
        ),
        messages: prepared.messages,
        tools: &prepared.tools,
        tool_choice: areev_llm::ToolChoice::Auto,
        max_tokens: prepared.max_tokens,
        temperature: 0.0,
    };
    let result = match prepared.on_token {
        Some(mut sink) => llm.call_streaming(&request, &mut *sink),
        None => llm.call(&request),
    };
    match result {
        Ok(resp) => {
            let result = serde_json::json!({
                "text": resp.text,
                "tool_calls": resp.tool_calls.iter().map(|c| serde_json::json!({
                    "id": c.id,
                    "name": c.name,
                    // Unparseable provider arguments surface raw so the
                    // scheduler's re-prompt path sees the garbage (§6.11).
                    "arguments": c.arguments_raw.clone()
                        .map(Value::String)
                        .unwrap_or_else(|| c.arguments.clone()),
                })).collect::<Vec<_>>(),
                "stop_reason": match resp.stop_reason {
                    areev_llm::StopReason::EndTurn => "end_turn",
                    areev_llm::StopReason::ToolUse => "tool_use",
                    areev_llm::StopReason::MaxTokens => "max_tokens",
                    areev_llm::StopReason::Other(_) => "other",
                },
            });
            let bytes = crate::journal::outcome_journal_bytes(&EffectOutcome::Completed {
                result: result.clone(),
                journal_bytes: 0,
                input_tokens: 0,
                output_tokens: 0,
                usd_micros: 0,
            });
            EffectOutcome::Completed {
                result,
                journal_bytes: bytes,
                input_tokens: resp.usage.input_tokens,
                output_tokens: resp.usage.output_tokens,
                usd_micros: 0,
            }
        }
        Err(e) => EffectOutcome::Failed {
            // §6.3 mapping: retryable transport/5xx → ExecutorError (the
            // scheduler retries the TURN); terminal → Unknown (never
            // retried).
            cause: if e.retryable { FailCause::ExecutorError } else { FailCause::Unknown },
            journal_bytes: e.message.len() as u64,
            detail: e.message,
        },
    }
}
