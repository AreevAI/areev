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

    /// Every brokered call that WENT OUT (#101).
    ///
    /// The mirror of [`HostToolExecutor::refusals`], and journaled the same
    /// way: a capability tool's bargain is that its I/O is mediated *and
    /// recorded*, so "it was allowed to reach Gmail" (policy) and "it sent
    /// these four requests" (evidence) are both in the memory. Default: none.
    fn calls(&self) -> Vec<crate::broker::EgressCall> {
        Vec::new()
    }

    /// Every CAS blob a capability tool READ (#106).
    ///
    /// The third audit stream, drained beside [`HostToolExecutor::calls`] at
    /// the same superstep boundary. Default: none.
    fn blob_reads(&self) -> Vec<crate::broker::BlobRead> {
        Vec::new()
    }

    /// Bind the principal the current run executes as, so the broker can
    /// refuse a credential owned by a different principal (#101). Default
    /// no-op: an executor with no broker has nothing to bind. Called by the
    /// driver at drive entry — every run, resume and fork passes through
    /// there — so an owned credential fails closed if this is ever skipped.
    fn bind_run_principal(&self, _principal: &str) {}

    /// Execute a content-addressed code blob. `bytes` were read and
    /// hash-verified on the driver thread; this runs on a pool worker.
    ///
    /// The default refuses, so a host that never opted in cannot be handed
    /// code by a plan.
    fn execute_code(
        &self,
        _tool_name: &str,
        _tool_hash: &str,
        code: &PreparedCode,
        _input: &Value,
        _idempotency_key: &str,
    ) -> ExecResult {
        ExecResult::Err {
            cause: FailCause::ExecutorError,
            detail: format!(
                "this host does not run code-carrying tools, so {} was not executed",
                code.uri
            ),
        }
    }

    /// Whether this host can dispatch a declared (non-native) runtime.
    /// Default `false`: a plan declaring `wasm32-areev` refuses at start on
    /// a host with no sandbox, the same fail-closed shape as the pin.
    fn runtime_supported(&self, _runtime: &str) -> bool {
        false
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
    fn calls(&self) -> Vec<crate::broker::EgressCall> {
        self.broker.calls()
    }
    fn blob_reads(&self) -> Vec<crate::broker::BlobRead> {
        self.broker.blob_reads()
    }
    /// Does this caller hold a grant (and therefore a token)?
    fn token_for(&self, caller: &str) -> Option<&str> {
        self.broker.token_for(caller)
    }
    /// Register a Definition's declared capability set with the broker.
    fn declare(
        &self,
        caller: &str,
        declaration: areev_core::types::capability::Declaration,
        limits: crate::broker::CapabilityLimits,
    ) {
        self.broker.declare(caller, declaration, limits)
    }
    fn bind_run_principal(&self, principal: &str) {
        self.broker.bind_run_principal(principal)
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

    fn calls(&self) -> Vec<crate::broker::EgressCall> {
        self.egress.as_ref().map(|e| e.calls()).unwrap_or_default()
    }

    fn blob_reads(&self) -> Vec<crate::broker::BlobRead> {
        self.egress.as_ref().map(|e| e.blob_reads()).unwrap_or_default()
    }

    fn bind_run_principal(&self, principal: &str) {
        if let Some(e) = &self.egress {
            e.bind_run_principal(principal);
        }
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
    /// Wall-clock ceiling per invocation — same default and same knob shape
    /// as `CommandExecutor`'s (#133): a pinned blob that makes a dozen model
    /// calls (a document-analysis leg, say) needs longer than the
    /// `pdftotext`-shaped tool this default was sized for, and unlike
    /// `CommandExecutor` nothing surfaced `with_timeout` on any host until
    /// #133 — every host now exposes it as `executor_timeout_secs` /
    /// `--executor-timeout`, alongside `allow_executor`/`executor_cache`/
    /// `sandbox_cmd`.
    timeout: Option<std::time::Duration>,
    egress: Option<EgressHandle>,
    /// The sandbox runner, argv-split (`areev-sandbox` or a path to it).
    /// Host config, like the pin: a plan can declare `wasm32-areev`, but only
    /// an operator who configured a sandbox can dispatch it.
    sandbox_cmd: Option<Vec<String>>,
}

impl CodeExecutor {
    /// Wrap `inner`, which still handles every tool that names no executor.
    pub fn new(inner: Arc<dyn HostToolExecutor>) -> Self {
        CodeExecutor {
            inner,
            allowed: Default::default(),
            cache_dir: std::env::temp_dir().join("areev-executors"),
            // Same constant `CommandExecutor::new` uses — one number, so
            // raising the shared default only ever needs one edit (#133).
            timeout: Some(areev_core::proc::DEFAULT_TIMEOUT),
            egress: None,
            sandbox_cmd: None,
        }
    }

    /// Configure the areev-sandbox runner for `runtime: "wasm32-areev"`
    /// Definitions (#86). Argv-split like `--embed-cmd` — never a shell.
    pub fn sandbox_cmd(mut self, cmd: &str) -> Self {
        let argv: Vec<String> = cmd.split_whitespace().map(str::to_string).collect();
        if !argv.is_empty() {
            self.sandbox_cmd = Some(argv);
        }
        self
    }

    /// Route this executor's code blobs through a credential broker — the
    /// same seam `CommandExecutor::with_egress` gives a `--tool-cmd`. A
    /// pinned blob is the authoring style whose provenance the host can
    /// actually prove; it must not get the WEAKER credential story (#87).
    pub fn with_egress(mut self, egress: EgressHandle) -> Self {
        self.egress = Some(egress);
        self
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

    /// Override the per-invocation ceiling (#133). `None` means wait
    /// forever, for a host that genuinely wants that; see the `timeout`
    /// field doc for why a fixed 300s stopped being the right default for
    /// every pinned blob.
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

    /// Tell the broker what this module declared, so `declared ∩ host-granted`
    /// is enforced on every `areev::fetch` (#101).
    ///
    /// Done at dispatch rather than at run start because this is where the
    /// manifest-pinned declaration and the broker handle are both in hand, and
    /// because it must hold for a pool worker that never saw the start path —
    /// the same reason the executor pin is re-checked here rather than trusted.
    /// Re-registering across dispatches is deliberate and does not refill the
    /// call budget.
    fn register_capability(&self, tool_name: &str, code: &PreparedCode) -> Result<(), String> {
        // A capability module with no broker has nowhere to send anything, and
        // the honest failure is at dispatch with the fix named — not a module
        // that starts and then has every call refused by a broker that is not
        // there.
        let Some(egress) = self.egress.as_ref() else {
            return Err(format!(
                "{} declares runtime \"wasm32-areev-io\" but this host configured no credential \
                 broker — a capability module needs --allow-host/--credential/--tool-egress",
                code.uri
            ));
        };
        // Equally honest: a grant is what mints the token the module presents,
        // so without one it holds no `AREEV_EGRESS_TOKEN` and every call would
        // be a 401 with no explanation.
        //
        // This binds blob-only modules (#106) too, and needs no exception: the
        // token is what identifies the CALLER, and `POST /blob` has to know
        // who is asking as much as `POST /` does. A module that reads
        // attachments and touches no network takes the grant that names
        // neither a credential nor a method — `--tool-egress 'parse_pdf::'` —
        // which mints a token and authorizes no egress whatsoever.
        if egress.token_for(tool_name).is_none() {
            return Err(format!(
                "{} declares capabilities but the host granted tool '{tool_name}' no egress — \
                 add --tool-egress '{tool_name}:<credentials>:<methods>' (both may be empty for \
                 a module that only reads blobs)",
                code.uri
            ));
        }
        let declared = match &code.capabilities {
            Some(v) => areev_core::types::capability::Declaration::parse(v)
                .map_err(|e| format!("{} declares malformed capabilities: {e}", code.uri))?,
            // Unreachable through the manifest, which refuses this pairing at
            // start — but a pool worker does not get to assume that.
            None => areev_core::types::capability::Declaration::default(),
        };
        let mut limits = crate::broker::CapabilityLimits::default();
        if let Some(l) = &code.limits {
            if let Some(n) = l.get("max_calls").and_then(Value::as_u64) {
                limits.max_calls = n.min(u64::from(u32::MAX)) as u32;
            }
            if let Some(n) = l.get("max_response_bytes").and_then(Value::as_u64) {
                // Clamp rather than `as usize`-truncate: on a 32-bit target a
                // manifest value above `usize::MAX` would silently wrap to a
                // tiny ceiling that refuses legitimate responses, the same
                // hazard the `max_calls` clamp above avoids.
                limits.max_response_bytes = usize::try_from(n).unwrap_or(usize::MAX);
            }
        }
        egress.declare(tool_name, declared, limits);
        Ok(())
    }
}

/// `cas://sha256:<hex>` -> `<hex>`; anything else is returned unchanged.
pub(crate) fn strip_cas(uri: &str) -> &str {
    uri.strip_prefix("cas://sha256:").unwrap_or(uri)
}

/// The two runtimes that route a pinned blob to areev-sandbox.
///
/// `wasm32-areev` is pure Tier C (#86): no capabilities, re-execution-provable,
/// the frozen import set is exactly `areev::emit`. `wasm32-areev-io` (#101) is
/// the same isolation with ONE more gate, `areev::fetch`, answered by the
/// engine's credential broker — so it is deterministic *modulo journaled
/// effects* rather than provable by re-execution, which is why it is a
/// separate name and not a flag on the first. Both spawn the same binary; the
/// declaration is what decides whether the extra import is linked at all.
pub fn is_sandbox_runtime(runtime: &str) -> bool {
    matches!(runtime, "wasm32-areev" | "wasm32-areev-io")
}

/// Does this runtime admit `areev::fetch`? Only the `-io` variant.
pub fn runtime_allows_capabilities(runtime: Option<&str>) -> bool {
    runtime == Some("wasm32-areev-io")
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

    /// The broker's refusals — read from our own handle when we hold one
    /// (the no-`--tool-cmd` case, where `inner` is a refusing stub), else
    /// passed through to the wrapped executor. Both handles wrap the same
    /// `Arc<Broker>`, so this never double-reports.
    fn calls(&self) -> Vec<crate::broker::EgressCall> {
        match &self.egress {
            Some(e) => e.calls(),
            None => self.inner.calls(),
        }
    }

    fn blob_reads(&self) -> Vec<crate::broker::BlobRead> {
        match &self.egress {
            Some(e) => e.blob_reads(),
            None => self.inner.blob_reads(),
        }
    }

    fn bind_run_principal(&self, principal: &str) {
        match &self.egress {
            Some(e) => e.bind_run_principal(principal),
            None => self.inner.bind_run_principal(principal),
        }
    }

    fn refusals(&self) -> Vec<crate::broker::EgressRefusal> {
        match &self.egress {
            Some(e) => e.refusals(),
            None => self.inner.refusals(),
        }
    }

    fn code_allowed(&self, _tool_hash: &str, uri: &str) -> bool {
        self.allowed.contains(&strip_cas(uri).to_ascii_lowercase())
    }

    fn runtime_supported(&self, runtime: &str) -> bool {
        is_sandbox_runtime(runtime) && self.sandbox_cmd.is_some()
    }

    fn execute_code(
        &self,
        tool_name: &str,
        tool_hash: &str,
        code: &PreparedCode,
        input: &Value,
        idempotency_key: &str,
    ) -> ExecResult {
        let uri = code.uri.as_str();
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
        let path = match self.materialize(&hex, &code.bytes) {
            Ok(p) => p,
            Err(e) => {
                return ExecResult::Err {
                    cause: FailCause::ExecutorError,
                    detail: format!("could not materialize {uri}: {e}"),
                }
            }
        };
        // The declared runtime decides HOW the materialized blob runs (#86):
        // native = the blob is the program; wasm32-areev = the sandbox is
        // the program and the blob is its --module. Re-checked here for the
        // same pool-worker reason as the pin.
        use areev_core::proc::{self, EnvPolicy, SpawnPolicy};
        let sandboxed = matches!(code.runtime.as_deref(), Some(rt) if rt != "native");
        let cmd = match code.runtime.as_deref() {
            None | Some("native") => std::process::Command::new(&path),
            Some(rt) => {
                let Some(argv) = self.sandbox_cmd.as_ref().filter(|_| is_sandbox_runtime(rt))
                else {
                    return ExecResult::Err {
                        cause: FailCause::ExecutorError,
                        detail: format!(
                            "{uri} declares runtime {rt:?}, which this host cannot \
                             dispatch — configure --sandbox-cmd"
                        ),
                    };
                };
                let mut c = std::process::Command::new(&argv[0]);
                c.args(&argv[1..]);
                c.arg("--module").arg(&path);
                if let Some(limits) = &code.limits {
                    if let Some(fuel) = limits.get("fuel").and_then(Value::as_u64) {
                        c.arg("--fuel").arg(fuel.to_string());
                    }
                    if let Some(pages) = limits.get("max_pages").and_then(Value::as_u64) {
                        c.arg("--max-pages").arg(pages.to_string());
                    }
                    if let Some(n) = limits.get("max_response_bytes").and_then(Value::as_u64) {
                        c.arg("--max-response-bytes").arg(n.to_string());
                    }
                    if let Some(n) = limits.get("max_blob_bytes").and_then(Value::as_u64) {
                        c.arg("--max-blob-bytes").arg(n.to_string());
                    }
                }
                // `--allow-fetch` is what LINKS `areev::fetch` into the guest's
                // import set. Without it the sandbox's frozen set is exactly
                // `areev::emit`, so a module that imports `fetch` without a
                // capability declaration is refused at instantiation, by name,
                // before one instruction runs — the same `ForbiddenImport`
                // philosophy #86 established, extended from "which imports" to
                // "which capabilities".
                //
                // The flag is derived from the MANIFEST-pinned runtime, never
                // from the module: a blob cannot talk its way into a gate.
                if runtime_allows_capabilities(Some(rt)) {
                    if let Err(detail) = self.register_capability(tool_name, code) {
                        return ExecResult::Err {
                            cause: FailCause::ExecutorError,
                            detail,
                        };
                    }
                    c.arg("--allow-fetch");
                    // `--allow-blob` links `areev::blob_get` (#106), and comes
                    // off the pinned DECLARATION rather than the runtime
                    // string like `--allow-fetch` does. The asymmetry is
                    // deliberate: egress has a host-side grant behind it that
                    // narrows the runtime's permission afterwards, so the flag
                    // can be broad. A blob read has no such second key — the
                    // declaration is the only thing that says yes — so it must
                    // be the thing the flag is derived from, or every
                    // capability module would silently gain the import.
                    //
                    // Still manifest-pinned, so a mid-run supersession cannot
                    // add it: `code.capabilities` is the frozen copy.
                    if code
                        .capabilities
                        .as_ref()
                        .and_then(|v| areev_core::types::capability::Declaration::parse(v).ok())
                        .is_some_and(|d| d.declares_blob_read())
                    {
                        c.arg("--allow-blob");
                    }
                }
                c
            }
        };
        // Native blobs are spawned directly rather than through a shell: the
        // path is ours and contains no metacharacters, and going direct means
        // no quoting bug can turn a cache path into an argument. A unix blob
        // interprets its own shebang; on Windows the OS decides, which is why
        // a code-carrying executor is platform-specific and the operator pins
        // per platform. The sandbox path constructs argv the same way — the
        // shell never sees any of it.
        // A native blob inherits (minus the registered secrets): it is an
        // ordinary program and may legitimately read an ambient variable. The
        // SANDBOX gets `ClearExcept` instead — it is a wasm host, so the only
        // environment it can justify is what it needs to start, and under #101
        // it is also the process holding a broker token. `InheritExcept` there
        // would hand the operator's whole environment to the least-trusted
        // seam in the tree for no capability it uses. The `AREEV_*` extras are
        // applied AFTER the policy (`proc::run`), so the broker handshake and
        // the tool identity survive the clear.
        let policy = SpawnPolicy {
            timeout: self.timeout,
            env: if sandboxed {
                EnvPolicy::ClearExcept { allow: EnvPolicy::minimal_allow() }
            } else {
                EnvPolicy::default()
            },
            ..SpawnPolicy::default()
        };
        let mut env: Vec<(&str, &str)> = vec![
            ("AREEV_TOOL_NAME", tool_name),
            ("AREEV_TOOL_HASH", tool_hash),
            ("AREEV_IDEMPOTENCY_KEY", idempotency_key),
            ("AREEV_EXECUTOR_URI", uri),
        ];
        // Brokered credentials, on the same terms as `CommandExecutor`:
        // present only when the tool has a grant, so an unauthorized blob
        // cannot even see the broker. Until #101 these were inert in the
        // sandbox — a pure module has no sockets — and were passed for
        // uniformity; the capability runtime is what makes the sandbox
        // binary's Rust half actually use them.
        let brokered = self.egress.as_ref().map(|e| e.env_for(tool_name)).unwrap_or_default();
        env.extend(brokered.iter().map(|(k, v)| (*k, v.as_str())));
        let out = match proc::run(
            cmd,
            Some(input.to_string().as_bytes()),
            &env,
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
    /// The manifest-pinned runtime (#86). `None` = native direct exec;
    /// `"wasm32-areev"` routes to the sandbox.
    pub runtime: Option<String>,
    /// The manifest-pinned sandbox limits (`{"fuel", "max_pages",
    /// "max_calls", "max_response_bytes"}`).
    pub limits: Option<Value>,
    /// The manifest-pinned capability declaration (#101), present only for a
    /// `wasm32-areev-io` module. Frozen with the runtime, so a mid-run
    /// supersession cannot widen what the module may reach.
    pub capabilities: Option<Value>,
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
                            c,
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
