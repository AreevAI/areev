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
pub struct CommandExecutor {
    cmd: String,
}

impl CommandExecutor {
    pub fn new(cmd: &str) -> Self {
        CommandExecutor { cmd: cmd.to_string() }
    }
}

impl HostToolExecutor for CommandExecutor {
    fn execute(
        &self,
        tool_name: &str,
        tool_hash: &str,
        input: &Value,
        idempotency_key: &str,
    ) -> ExecResult {
        use std::io::Write;
        use std::process::{Command, Stdio};
        // The platform shell: /bin/sh -c on unix, cmd /C on Windows.
        #[cfg(not(windows))]
        let mut shell = Command::new("/bin/sh");
        #[cfg(not(windows))]
        shell.arg("-c");
        #[cfg(windows)]
        let mut shell = Command::new("cmd");
        #[cfg(windows)]
        shell.arg("/C");
        let mut child = match shell
            .arg(&self.cmd)
            .env("AREEV_TOOL_NAME", tool_name)
            .env("AREEV_TOOL_HASH", tool_hash)
            .env("AREEV_IDEMPOTENCY_KEY", idempotency_key)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                return ExecResult::Err {
                    cause: FailCause::ExecutorError,
                    detail: format!("spawn '{}': {e}", self.cmd),
                }
            }
        };
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(input.to_string().as_bytes());
        }
        drop(child.stdin.take());
        let out = match child.wait_with_output() {
            Ok(o) => o,
            Err(e) => {
                return ExecResult::Err {
                    cause: FailCause::ExecutorError,
                    detail: format!("wait: {e}"),
                }
            }
        };
        if !out.status.success() {
            return ExecResult::Err {
                cause: FailCause::ExecutorError,
                detail: format!(
                    "tool command exited {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            };
        }
        match serde_json::from_slice::<Value>(&out.stdout) {
            Ok(v) => ExecResult::Ok(v),
            Err(e) => ExecResult::Err {
                cause: FailCause::ExecutorError,
                detail: format!("tool output is not JSON: {e}"),
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
                    executor.execute(&tool_name, &tool_hash, &job.input, &job.idempotency_key)
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
