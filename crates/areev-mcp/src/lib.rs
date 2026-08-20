//! areev-mcp — the built-in MCP server.
//!
//! Memory-semantic tools over newline-delimited JSON-RPC 2.0 on stdio —
//! not SQL-over-MCP. Tool surface is a tag-grouped set:
//! `areev_recall`, `areev_remember`, `areev_add`, `areev_supersede`,
//! `areev_forget`, `areev_cal`, plus the graph/time tools
//! `areev_related`, `areev_entity_at`, `areev_step_actions`, and the
//! run/memory join `areev_run_trace`, `areev_runs_touching`.
//!
//! Protocol errors are JSON-RPC errors; tool-execution failures are
//! `isError: true` tool results, per the MCP spec.

use std::io::{BufRead, Write};

use areev_cal::store_types::RecallParams;
use areev_cal::{CalExecutor, CalExecutorConfig, CalStoreFacade, AreevFacade};
use areev_core::error::Hash;
use areev_store::{Axis, Capture, Direction};
use areev_loop_adapter::{now_ms, BorrowedSubstrate};
use serde_json::{json, Map, Value};
use areev_loop::{Decision, Engine, ObserverType, RecStatus, RunOptions};

/// Budget flags shared by the run tools.
fn run_opts(args: &Map<String, Value>) -> areev_run::RunOptions {
    areev_run::RunOptions {
        budgets: areev_run::BudgetsSpec {
            max_supersteps: args.get("max_supersteps").and_then(Value::as_u64),
            max_tokens: args.get("max_tokens").and_then(Value::as_u64),
            max_usd_micros: args.get("max_usd_micros").and_then(Value::as_u64),
            max_wall_ms: args.get("max_wall_ms").and_then(Value::as_u64),
            max_storage_bytes: args.get("max_storage_bytes").and_then(Value::as_u64),
        },
        ask_ttl_sec: args.get("ask_ttl_sec").and_then(Value::as_i64),
        workers: 4,
        on_dangling: Default::default(),
        llm_max_tokens: None,
        inject_crash: None,
    }
}

fn run_session_json(session: areev_run::RunSession) -> Value {
    match session {
        areev_run::RunSession::Finished { outcome, run_id } => {
            json!({"run_id": run_id, "finished": format!("{outcome:?}")})
        }
        areev_run::RunSession::Parked { envelope, run_id } => {
            json!({"run_id": run_id, "parked": envelope})
        }
    }
}

pub const SERVER_NAME: &str = "areev";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Latest MCP protocol revision this server speaks.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

pub struct McpServer {
    facade: std::sync::Arc<AreevFacade>,
    executor: CalExecutor,
    default_ns: String,
    allow_destructive_ops: bool,
    /// When set, the session is pinned to this namespace: per-call `namespace`
    /// arguments are ignored and `areev_cal` queries are namespace-overridden,
    /// so an agent cannot read or write outside its partition.
    locked_ns: Option<String>,
    /// Host loop policy (§6.2) for the `areev_loop` engine — the same
    /// `loop-policy.json` the CLI takes (`areev serve --mcp --policy`).
    /// Absent → the closed default (nothing auto-applies). Host config, set at
    /// process start, never controllable by the MCP client.
    loop_policy: Option<areev_loop::Policy>,
    assembly_manifest_sample_rate: f64,
}

impl McpServer {
    pub fn new(facade: AreevFacade, default_ns: Option<String>) -> Self {
        // Single source of truth for the session namespace: explicit arg,
        // else the facade's capability default, else "shared".
        let default_ns = default_ns
            .or_else(|| facade.default_namespace().map(String::from))
            .unwrap_or_else(|| "shared".to_string());
        McpServer {
            facade: std::sync::Arc::new(facade),
            executor: CalExecutor::new(CalExecutorConfig::default())
                .with_governance(std::sync::Arc::new(areev_loop_adapter::LoopGovernance::new())),
            default_ns,
            allow_destructive_ops: true,
            locked_ns: None,
            loop_policy: None,
            assembly_manifest_sample_rate: 0.0,
        }
    }

    /// Attach a host loop policy so an MCP-triggered `areev_loop` run
    /// honors the same auto-apply grants, denies, and severity floors as
    /// `areev loop run --policy`.
    pub fn with_loop_policy(mut self, policy: areev_loop::Policy) -> Self {
        self.loop_policy = Some(policy);
        // The CAL surface's governance host honors the same policy.
        self.rebuild_executor();
        self
    }

    /// Set the host-only sampling fraction for durable ASSEMBLE manifests.
    pub fn assembly_manifest_sample_rate(mut self, rate: f64) -> Self {
        self.assembly_manifest_sample_rate = rate.clamp(0.0, 1.0);
        self.rebuild_executor();
        self
    }

    /// The run tools' acting identity: the SESSION's bound principal when
    /// the host started the server with one, else the fixed `agent:mcp` —
    /// never a client-supplied string. Same rule the loop tools pinned
    /// ("the agent that runs the loop cannot approve what its run
    /// authored"): both the manifest's triggering principal and any
    /// respond's responder carry THIS identity, so an MCP agent can never
    /// satisfy separation of duties by naming two labels for itself. A
    /// deployment that wants MCP-mediated approvals runs a second server
    /// bound to the approver principal.
    fn run_identity(&self) -> String {
        let authz = self.facade.authz();
        if authz.is_owner() {
            "agent:mcp".to_string()
        } else {
            authz.principal().to_string()
        }
    }

    /// A runtime driver for the run tools. Host tools execute through
    /// `$AREEV_RUN_TOOL_CMD` (the same subprocess seam as the CLI's
    /// `--tool-cmd` — host config, never client-settable); without it, host
    /// tools fail with a clear message rather than being faked. The
    /// principal is always [`run_identity`](Self::run_identity)'s
    /// server-bound value — callers pass it through, never a client string.
    fn runner(&self, principal: &str) -> areev_run::Runner {
        let executor: std::sync::Arc<dyn areev_run::HostToolExecutor> =
            match std::env::var("AREEV_RUN_TOOL_CMD") {
                Ok(cmd) if !cmd.trim().is_empty() => {
                    std::sync::Arc::new(areev_run::CommandExecutor::new(&cmd))
                }
                _ => {
                    struct NoExec;
                    impl areev_run::HostToolExecutor for NoExec {
                        fn execute(
                            &self,
                            tool_name: &str,
                            _h: &str,
                            _i: &Value,
                            _k: &str,
                        ) -> areev_run::ExecResult {
                            areev_run::ExecResult::Err {
                                cause: areev_run::FailCause::ExecutorError,
                                detail: format!(
                                    "no AREEV_RUN_TOOL_CMD configured; cannot execute \
                                     host tool '{tool_name}'"
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
            ns: self.default_ns.clone(),
            principal: principal.to_string(),
        }
    }

    /// The loop engine for a call: builtins + the host policy when set.
    fn engine(&self) -> Engine {
        match &self.loop_policy {
            Some(p) => Engine::with_builtins().with_policy(p.clone()),
            None => Engine::with_builtins(),
        }
    }

    /// Rebuild the executor from the current gate settings. Called by every
    /// builder so `--lock-ns` and `--no-destructive-ops` compose instead of
    /// clobbering each other's config — and the governance host survives
    /// every rebuild.
    fn rebuild_executor(&mut self) {
        self.executor = CalExecutor::new(CalExecutorConfig {
            allow_destructive_ops: self.allow_destructive_ops,
            namespace_override: self.locked_ns.clone(),
            assembly_manifest_sample_rate: self.assembly_manifest_sample_rate,
            ..CalExecutorConfig::default()
        })
        .with_governance(std::sync::Arc::new(match &self.loop_policy {
            Some(p) => areev_loop_adapter::LoopGovernance::with_policy(p.clone()),
            None => areev_loop_adapter::LoopGovernance::new(),
        }));
    }

    /// Permit or forbid destructive operations — the `areev_forget` tool and
    /// `FORGET <hash>` via `areev_cal`. Enabled by default; pass `false` to
    /// serve a read-only session (`areev serve --mcp --no-destructive-ops`).
    pub fn allow_destructive_ops(mut self, allow: bool) -> Self {
        self.allow_destructive_ops = allow;
        self.rebuild_executor();
        self
    }

    /// Pin the session to a single namespace (`areev serve --mcp --lock-ns NS`).
    /// Per-call `namespace` arguments are ignored and CAL queries are
    /// namespace-overridden, so a multi-tenant host can hand an agent a session
    /// it cannot escape. Without this, namespaces are filters, not boundaries.
    pub fn lock_namespace(mut self, ns: impl Into<String>) -> Self {
        let ns = ns.into();
        self.default_ns = ns.clone();
        self.locked_ns = Some(ns);
        self.rebuild_executor();
        self
    }

    /// Serve until EOF on the reader (stdio transport: one JSON-RPC
    /// message per line).
    pub fn serve<R: BufRead, W: Write>(&self, reader: R, mut writer: W) -> std::io::Result<()> {
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let msg: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    let resp = json!({"jsonrpc": "2.0", "id": null,
                        "error": {"code": -32700, "message": format!("parse error: {e}")}});
                    writeln!(writer, "{resp}")?;
                    writer.flush()?;
                    continue;
                }
            };
            let id = msg.get("id").cloned();
            let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
            // Notifications get no response.
            if id.is_none() || id == Some(Value::Null) {
                continue;
            }
            let id = id.unwrap();
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let resp = match method {
                "initialize" => json!({"jsonrpc": "2.0", "id": id, "result": {
                    "protocolVersion": params.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or(PROTOCOL_VERSION),
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
                }}),
                "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tool_defs()}}),
                "tools/call" => {
                    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let args = params
                        .get("arguments")
                        .and_then(|v| v.as_object())
                        .cloned()
                        .unwrap_or_default();
                    match self.call_tool(name, &args) {
                        Ok(text) => json!({"jsonrpc": "2.0", "id": id, "result": {
                            "content": [{"type": "text", "text": text}], "isError": false}}),
                        Err(e) => json!({"jsonrpc": "2.0", "id": id, "result": {
                            "content": [{"type": "text", "text": e}], "isError": true}}),
                    }
                }
                other => json!({"jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": format!("method not found: {other}")}}),
            };
            writeln!(writer, "{resp}")?;
            writer.flush()?;
        }
        Ok(())
    }

    pub fn serve_stdio(&self) -> std::io::Result<()> {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        self.serve(stdin.lock(), stdout.lock())
    }

    fn ns<'a>(&'a self, args: &'a Map<String, Value>) -> &'a str {
        // A locked session ignores caller-supplied namespaces entirely.
        if self.locked_ns.is_some() {
            return &self.default_ns;
        }
        args.get("namespace")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.default_ns)
    }

    /// Set the `namespace` field for a write. A locked session **forces** it
    /// (overwriting any caller-supplied value in `fields`), so an agent cannot
    /// write outside its partition; otherwise it fills in the session default
    /// only when the caller left it unset.
    fn set_ns(&self, fields: &mut Map<String, Value>, args: &Map<String, Value>) {
        let ns = Value::String(self.ns(args).to_string());
        if self.locked_ns.is_some() {
            fields.insert("namespace".to_string(), ns);
        } else {
            fields.entry("namespace".to_string()).or_insert(ns);
        }
    }

    fn call_tool(&self, name: &str, args: &Map<String, Value>) -> Result<String, String> {
        match name {
            "areev_recall" => {
                let subject = args
                    .get("subject")
                    .and_then(|v| v.as_str())
                    .ok_or("areev_recall requires 'subject'")?;
                let mut p = RecallParams::default();
                p.subject = Some(subject.to_string());
                p.relation = args.get("relation").and_then(|v| v.as_str()).map(String::from);
                p.namespace = Some(self.ns(args).to_string());
                p.limit = Some(
                    args.get("k").and_then(|v| v.as_u64()).unwrap_or(16) as usize
                );
                let run_id = args.get("run_id").and_then(|v| v.as_str());
                self.facade.with_store(|m| m.set_run_id(run_id));
                let recalled = self.facade.recall(&p);
                // Ambient context must not leak into the next JSON-RPC call,
                // including when this recall failed.
                self.facade.with_store(|m| m.set_run_id(None));
                let hits = recalled.map_err(|e| e.to_string())?;
                let out: Vec<Value> = hits
                    .iter()
                    .map(|h| {
                        json!({
                            "hash": h.hash.to_hex(),
                            "type": format!("{:?}", h.grain.grain_type).to_lowercase(),
                            "fields": h.grain.fields,
                        })
                    })
                    .collect();
                // Egress flag (proposal §4.1): tell the model harness these
                // results are pseudonymized, with mapping *ids* only — the
                // mapping itself never rides an MCP payload (D5 custody).
                let out = match self.facade.anon_egress_report() {
                    Some(report) => json!({"grains": out, "anonymized": report}),
                    None => json!(out),
                };
                Ok(serde_json::to_string(&out).unwrap_or_default())
            }
            "areev_add" => {
                let gtype = args
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("fact");
                let mut fields = args
                    .get("fields")
                    .and_then(|v| v.as_object())
                    .cloned()
                    .ok_or("areev_add requires 'fields' object")?;
                self.set_ns(&mut fields, args);
                // `idempotent: true` collapses a re-add of the value already at
                // the (subject, relation) head — for learning loops that would
                // otherwise re-store the same distilled lesson.
                if args.get("idempotent").and_then(|v| v.as_bool()).unwrap_or(false) {
                    let (h, inserted) =
                        self.facade.cal_add_if_novel(gtype, &fields).map_err(|e| e.to_string())?;
                    Ok(json!({"hash": h.to_hex(), "inserted": inserted}).to_string())
                } else {
                    let h = self.facade.cal_add(gtype, &fields).map_err(|e| e.to_string())?;
                    Ok(json!({"hash": h.to_hex()}).to_string())
                }
            }
            "areev_supersede" => {
                let old = args
                    .get("old_hash")
                    .and_then(|v| v.as_str())
                    .ok_or("areev_supersede requires 'old_hash'")?;
                let old = Hash::from_hex(old).map_err(|e| e.to_string())?;
                let gtype = args.get("type").and_then(|v| v.as_str()).unwrap_or("fact");
                let mut fields = args
                    .get("fields")
                    .and_then(|v| v.as_object())
                    .cloned()
                    .ok_or("areev_supersede requires 'fields' object")?;
                self.set_ns(&mut fields, args);
                let h = self
                    .facade
                    .cal_supersede(&old, gtype, &fields)
                    .map_err(|e| e.to_string())?;
                Ok(json!({"hash": h.to_hex(), "supersedes": old.to_hex()}).to_string())
            }
            "areev_forget" => {
                if !self.allow_destructive_ops {
                    return Err("destructive operations are disabled for this server \
                                (started with --no-destructive-ops)"
                        .to_string());
                }
                let h = args
                    .get("hash")
                    .and_then(|v| v.as_str())
                    .ok_or("areev_forget requires 'hash'")?;
                let h = Hash::from_hex(h).map_err(|e| e.to_string())?;
                // Through the facade, not the raw store: the tool and CAL's
                // `FORGET <hash>` must agree on the `delete` check and both
                // must leave the same Tier-2 audit record.
                self.facade.cal_delete(&h, None).map_err(|e| e.to_string())?;
                Ok(json!({"forgotten": h.to_hex()}).to_string())
            }
            "areev_subject_report" => {
                // The DSAR read (GDPR Art. 15/20): the erasure selector in
                // show-me mode. Read-only — not behind allow_destructive_ops.
                let subject = args
                    .get("subject")
                    .and_then(|v| v.as_str())
                    .ok_or("areev_subject_report requires 'subject'")?;
                let text_mentions =
                    args.get("text_mentions").and_then(|v| v.as_bool()).unwrap_or(false);
                let report = self
                    .facade
                    .cal_subject_report(subject, text_mentions)
                    .map_err(|e| e.to_string())?;
                Ok(json!({
                    "subject": subject,
                    "identity_names": report.identity_names,
                    "count": report.grains.len(),
                    "grains": report.grains,
                })
                .to_string())
            }
            "areev_related" => {
                let start = args
                    .get("start")
                    .and_then(|v| v.as_str())
                    .ok_or("areev_related requires 'start'")?;
                let rels = areev_store::parse_relations(
                    args.get("relations").and_then(|v| v.as_str()).unwrap_or(""),
                );
                if rels.is_empty() {
                    return Err("areev_related requires 'relations' (comma-separated)".into());
                }
                let dir = Direction::parse(
                    args.get("direction").and_then(|v| v.as_str()).unwrap_or("out"),
                )
                .ok_or("direction must be one of: out, in, both")?;
                let depth = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(2) as usize;
                let cap = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(64) as usize;
                let ns = self.ns(args).to_string();
                let refs: Vec<&str> = rels.iter().map(String::as_str).collect();
                let reached = self
                    .facade
                    .with_store(|m| m.related(&ns, start, &refs, dir, depth, cap))
                    .map_err(|e| e.to_string())?;
                Ok(json!({"start": start, "reached": reached}).to_string())
            }
            "areev_entity_at" => {
                let subject = args
                    .get("subject")
                    .and_then(|v| v.as_str())
                    .ok_or("areev_entity_at requires 'subject'")?;
                let relation = args
                    .get("relation")
                    .and_then(|v| v.as_str())
                    .ok_or("areev_entity_at requires 'relation'")?;
                let at = args
                    .get("at")
                    .and_then(|v| v.as_i64())
                    .ok_or("areev_entity_at requires 'at' (epoch ms)")?;
                let axis =
                    Axis::parse(args.get("axis").and_then(|v| v.as_str()).unwrap_or("world"))
                        .ok_or("axis must be one of: world, knowledge")?;
                let ns = self.ns(args).to_string();
                let found = self
                    .facade
                    .with_store(|m| m.entity_at(&ns, subject, relation, at, axis))
                    .map_err(|e| e.to_string())?;
                Ok(match found {
                    Some(g) => json!({"found": true, "grain": g}).to_string(),
                    None => json!({"found": false}).to_string(),
                })
            }
            "areev_step_actions" => {
                let wf = args
                    .get("workflow")
                    .and_then(|v| v.as_str())
                    .ok_or("areev_step_actions requires 'workflow'")?;
                let wf = Hash::from_hex(wf).map_err(|e| e.to_string())?;
                let node = args.get("node").and_then(|v| v.as_str());
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(64) as usize;
                let ns = self.ns(args).to_string();
                let rows = self
                    .facade
                    .with_store(|m| m.step_actions(&ns, &wf, node, limit))
                    .map_err(|e| e.to_string())?;
                let steps: Vec<_> = rows
                    .into_iter()
                    .map(|(n, h)| json!({"node": n, "hash": h.to_hex()}))
                    .collect();
                Ok(json!({"workflow": wf.to_hex(), "steps": steps}).to_string())
            }
            "areev_run_trace" => {
                let run = args
                    .get("run_id")
                    .and_then(|v| v.as_str())
                    .ok_or("areev_run_trace requires 'run_id'")?;
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(64) as usize;
                let ns = self.ns(args).to_string();
                let yield_too = args
                    .get("include_yield")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let trace = self
                    .facade
                    .with_store(|m| m.run_trace(&ns, run, limit))
                    .map_err(|e| e.to_string())?;
                let produced = if yield_too {
                    self.facade
                        .with_store(|m| m.run_yield(&ns, run, limit))
                        .map_err(|e| e.to_string())?
                } else {
                    Vec::new()
                };
                Ok(json!({
                    "run_id": run,
                    "trace": trace,
                    "produced": produced,
                })
                .to_string())
            }
            "areev_runs_touching" => {
                let h = args
                    .get("hash")
                    .and_then(|v| v.as_str())
                    .ok_or("areev_runs_touching requires 'hash'")?;
                let h = Hash::from_hex(h).map_err(|e| e.to_string())?;
                let depth = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(4) as usize;
                let ns = self.ns(args).to_string();
                let runs = self
                    .facade
                    .with_store(|m| m.runs_touching(&ns, &h, depth))
                    .map_err(|e| e.to_string())?;
                Ok(json!({"hash": h.to_hex(), "runs": runs}).to_string())
            }
            "areev_run_start" => {
                let wf = args.get("workflow").and_then(Value::as_str)
                    .ok_or("areev_run_start requires 'workflow'")?;
                let run_id = args.get("run_id").and_then(Value::as_str)
                    .ok_or("areev_run_start requires 'run_id'")?;
                let input = args.get("input").cloned().unwrap_or(json!({}));
                if args.contains_key("principal") {
                    return Err("the run tools act as the server-bound identity; \
                                'principal' is not client-settable (start a second \
                                server with --as to act as another principal)"
                        .into());
                }
                let h = Hash::from_hex(wf).map_err(|e| e.to_string())?;
                let runner = self.runner(&self.run_identity());
                let session = runner
                    .start(&h, run_id, input, &run_opts(args))
                    .map_err(|e| e.to_string())?;
                Ok(run_session_json(session).to_string())
            }
            "areev_run_resume" => {
                let run_id = args.get("run_id").and_then(Value::as_str)
                    .ok_or("areev_run_resume requires 'run_id'")?;
                let session = self
                    .runner(&self.run_identity())
                    .resume(run_id, &run_opts(args))
                    .map_err(|e| e.to_string())?;
                Ok(run_session_json(session).to_string())
            }
            "areev_run_respond" => {
                let run_id = args.get("run_id").and_then(Value::as_str)
                    .ok_or("areev_run_respond requires 'run_id'")?;
                let ask = args.get("tool_call_id").and_then(Value::as_str)
                    .ok_or("areev_run_respond requires 'tool_call_id'")?;
                // The responder is the SERVER-BOUND identity, never a
                // client-named one — a string the client chooses is not an
                // identity, and letting it name one would let an agent
                // satisfy separation of duties by inventing a second label.
                if args.contains_key("responder") {
                    return Err("the responder is the server-bound identity; it is \
                                not client-settable — approvals from another \
                                principal go through a server bound to it (--as), \
                                the console, or the CLI"
                        .into());
                }
                let responder = self.run_identity();
                let result = args.get("result").cloned().unwrap_or(Value::Null);
                let is_error = args.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                self.runner(&responder)
                    .respond(run_id, ask, result, is_error, &responder)
                    .map_err(|e| e.to_string())?;
                Ok(json!({"responded": ask, "run_id": run_id, "responder": responder,
                          "note": "resume the run to spend the compute"}).to_string())
            }
            "areev_run_cancel" => {
                let run_id = args.get("run_id").and_then(Value::as_str)
                    .ok_or("areev_run_cancel requires 'run_id'")?;
                let because = args.get("because").and_then(Value::as_str).unwrap_or("canceled via mcp");
                let who = self.run_identity();
                self.runner(&who)
                    .cancel(run_id, &who, because)
                    .map_err(|e| e.to_string())?;
                Ok(json!({"canceled": run_id}).to_string())
            }
            "areev_run_verify" => {
                let run_id = args.get("run_id").and_then(Value::as_str)
                    .ok_or("areev_run_verify requires 'run_id'")?;
                let report = self
                    .runner("agent:mcp")
                    .verify(run_id)
                    .map_err(|e| e.to_string())?;
                serde_json::to_string(&report).map_err(|e| e.to_string())
            }
            "areev_run_list" => {
                let n = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
                let ids = self
                    .runner("agent:mcp")
                    .recent_runs(n)
                    .map_err(|e| e.to_string())?;
                Ok(json!({"runs": ids}).to_string())
            }
            "areev_tool_provenance" => {
                // §7.4 one-command code forensics, the `areev tool
                // provenance` twin: the recommendations targeting this
                // tool hash + which runs touched it.
                let hash_arg = args
                    .get("hash")
                    .and_then(|v| v.as_str())
                    .ok_or("areev_tool_provenance requires 'hash'")?
                    .to_string();
                let depth = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(2) as usize;
                let runs = match Hash::from_hex(&hash_arg) {
                    Ok(h) => self
                        .facade
                        .with_store(|m| m.runs_touching("agent:harness", &h, depth))
                        .unwrap_or_default(),
                    Err(_) => Vec::new(),
                };
                let target = format!("tool:{hash_arg}");
                let sub = areev_loop_adapter::BorrowedSubstrate::new(&self.facade);
                let engine = areev_loop::Engine::with_builtins();
                let recs: Vec<Value> = engine
                    .recommendations(&sub, None)
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .filter(|r| r.target_ref == target)
                    .map(|r| {
                        json!({
                            "recommendation": r.hash,
                            "status": r.status.as_str(),
                            "summary": r.summary.render(),
                            "evalset_pin": r.evalset_hash,
                        })
                    })
                    .collect();
                Ok(json!({
                    "tool": hash_arg,
                    "recommendations": recs,
                    "runs_touching": runs,
                })
                .to_string())
            }
            "areev_record_tool_call" => {
                let tool_name = args
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .ok_or("areev_record_tool_call requires 'tool_name'")?;
                let result = args
                    .get("result")
                    .and_then(Value::as_str)
                    .ok_or("areev_record_tool_call requires string 'result'")?;
                let input = args.get("input").map(|v| v.to_string());
                let hash = self
                    .facade
                    .record_tool_call(
                        self.ns(args),
                        tool_name,
                        input.as_deref(),
                        result,
                        args.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                        args.get("thread").and_then(Value::as_str),
                        args.get("call_id").and_then(Value::as_str),
                        args.get("run_id").and_then(Value::as_str),
                        args.get("workflow_hash").and_then(Value::as_str),
                        args.get("node_id").and_then(Value::as_str),
                        args.get("status").and_then(Value::as_str),
                        args.get("failure_cause").and_then(Value::as_str),
                        args.get("executor_kind").and_then(Value::as_str),
                        args.get("correlation_id").and_then(Value::as_str),
                    )
                    .map_err(|e| e.to_string())?;
                Ok(json!({"hash": hash.to_hex()}).to_string())
            }
            "areev_run_manifest" => {
                if self.locked_ns.is_some() {
                    return Err(
                        "areev_run_manifest is unavailable in a namespace-locked MCP session"
                            .into(),
                    );
                }
                let run_id = args
                    .get("run_id")
                    .and_then(Value::as_str)
                    .ok_or("areev_run_manifest requires 'run_id'")?;
                let config = args
                    .get("config")
                    .and_then(Value::as_object)
                    .ok_or("areev_run_manifest requires object 'config'")?;
                let config = serde_json::Value::Object(config.clone()).to_string();
                let (config_hash, link_hash) = self
                    .facade
                    .record_run_manifest(run_id, &config)
                    .map_err(|e| e.to_string())?;
                Ok(json!({
                    "run_id": run_id,
                    "config_hash": config_hash.to_hex(),
                    "link_hash": link_hash.to_hex(),
                })
                .to_string())
            }
            "areev_remember" => {
                // Stores the raw content as an Event through `Areev::capture`
                // — the same write path as `areev remember`, the bindings, and
                // `capture-stop`, so the same input yields the same grain on
                // every surface.
                //
                // No LLM extraction knobs here, deliberately: over MCP the
                // client *is* a model, so extraction would be a model calling
                // a model. It follows up with areev_add for the facts it
                // wants to keep.
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or("areev_remember requires 'content'")?;
                let ns = self.ns(args).to_string();
                let meta = Capture {
                    observer: args.get("observer").and_then(|v| v.as_str()),
                    session_id: args.get("session_id").and_then(|v| v.as_str()),
                    role: args.get("role").and_then(|v| v.as_str()),
                    run_id: args.get("run_id").and_then(|v| v.as_str()),
                };
                let h = self
                    .facade
                    .with_store(|m| m.capture(&ns, content, &meta))
                    .map_err(|e| e.to_string())?;
                Ok(json!({"hash": h.to_hex(), "event": h.to_hex(), "stored_as": "event",
                    "note": "distill durable facts with areev_add"}).to_string())
            }
            "areev_cal" => {
                let query = args
                    .get("query")
                    .and_then(|v| v.as_str())
                    .ok_or("areev_cal requires 'query'")?;
                let res = self
                    .executor
                    .execute(query, &*self.facade)
                    .map_err(|e| e.to_string())?;
                // Warnings ride in the payload under `warnings` — an agent
                // reading this tool result has no stderr to check.
                let payload = res.payload_json().map_err(|e| e.to_string())?;
                serde_json::to_string(&payload).map_err(|e| e.to_string())
            }
            "areev_loop" => {
                let opts = RunOptions {
                    min_new: args.get("min_new").and_then(Value::as_u64),
                    min_new_errors: args.get("min_new_errors").and_then(Value::as_u64),
                    if_stale_ms: None,
                    namespaces: Vec::new(),
                    full_sweep: args.get("full_sweep").and_then(Value::as_bool).unwrap_or(false),
                    // Same identity the review/apply path uses below — the
                    // agent that runs the loop cannot approve what its run's
                    // LLM/external analyzers authored.
                    triggering_actor: Some("agent:mcp".to_string()),
                };
                let mut sub = BorrowedSubstrate::new(&self.facade);
                let engine = self.engine();
                let res = engine.run(&mut sub, &opts, now_ms()).map_err(|e| e.to_string())?;
                let pending = engine
                    .recommendations(&sub, Some(RecStatus::Pending))
                    .map_err(|e| e.to_string())?;
                let list: Vec<Value> = pending.iter().map(rec_json).collect();
                Ok(json!({ "run": res, "pending": list }).to_string())
            }
            "areev_recommendations" => {
                let engine = self.engine();
                let action = args.get("action").and_then(Value::as_str);
                if let Some(action) = action {
                    let hash = args
                        .get("hash")
                        .and_then(Value::as_str)
                        .ok_or("areev_recommendations action requires 'hash'")?;
                    let because = args
                        .get("because")
                        .and_then(Value::as_str)
                        .ok_or("areev_recommendations action requires 'because'")?;
                    let actor = "agent:mcp";
                    // Scopes derive from the session's grants (an owner
                    // session — today's only MCP mode — holds all of them).
                    let scopes = areev_loop_adapter::scopes_for(&self.facade.authz());
                    let mut sub = BorrowedSubstrate::new(&self.facade);
                    let now = now_ms();
                    match action {
                        "apply" => {
                            engine
                                .review(&mut sub, hash, Decision::Approve, actor, ObserverType::Agent, &scopes, because, now)
                                .map_err(|e| e.to_string())?;
                            engine
                                .apply(&mut sub, hash, actor, ObserverType::Agent, &scopes, because, false, now)
                                .map_err(|e| e.to_string())?;
                        }
                        "approve" => engine
                            .review(&mut sub, hash, Decision::Approve, actor, ObserverType::Agent, &scopes, because, now)
                            .map_err(|e| e.to_string())?,
                        "reject" | "dismiss" => engine
                            .review(&mut sub, hash, Decision::Reject, actor, ObserverType::Agent, &scopes, because, now)
                            .map_err(|e| e.to_string())?,
                        other => return Err(format!("unknown action '{other}' (apply|approve|reject)")),
                    }
                    Ok(json!({ "hash": hash, "action": action }).to_string())
                } else {
                    // `all` means NO status filter. Writing this as
                    // `.filter(|s| *s != "all").map(...).or(Some(Pending))`
                    // collapsed `all` back to `pending`, because the `.or`
                    // could not tell "the caller said all" from "the caller
                    // said nothing" — both arrive as None. A filter that is
                    // silently ignored fails open into a model's context.
                    let status = match args.get("status").and_then(Value::as_str) {
                        None => Some(RecStatus::Pending),
                        Some("all") => None,
                        Some(s) => Some(status_or_pending(s)),
                    };
                    let sub = BorrowedSubstrate::new(&self.facade);
                    let recs = engine.recommendations(&sub, status).map_err(|e| e.to_string())?;
                    let list: Vec<Value> = recs.iter().map(rec_json).collect();
                    Ok(serde_json::to_string(&list).unwrap_or_default())
                }
            }
            other => Err(format!("unknown tool: {other}")),
        }
    }
}

fn status_or_pending(s: &str) -> RecStatus {
    match s {
        "approved" => RecStatus::Approved,
        "rejected" => RecStatus::Rejected,
        "applied" => RecStatus::Applied,
        "rolled_back" => RecStatus::RolledBack,
        "expired" => RecStatus::Expired,
        _ => RecStatus::Pending,
    }
}

fn rec_json(r: &areev_loop::Recommendation) -> Value {
    json!({
        "hash": r.hash,
        "status": r.status.as_str(),
        "severity": r.severity.as_str(),
        "analyzer": r.analyzer,
        "summary": r.summary.render(),
        "target_ref": r.target_ref,
        "destructive": r.destructive,
    })
}

fn tool_defs() -> Vec<Value> {
    let s = |desc: &str| json!({"type": "string", "description": desc});
    vec![
        json!({
            "name": "areev_recall",
            "description": "Recall current memories about a subject (structural, µs-class). Returns grains newest-first. Under an active anonymization policy the response is {grains, anonymized}: fields are pseudonymized and `anonymized` carries mapping ids (never the mappings).",
            "inputSchema": {"type": "object", "properties": {
                "subject": s("entity to recall about, e.g. 'caller:john'"),
                "relation": s("optional relation filter, e.g. 'prefers'"),
                "namespace": s("optional namespace (defaults to session namespace). Accepts a prefix scope 'org.*' — the namespace 'org' plus every '.'-descendant (org.sales, org.sales.emea); 'organization' never matches. Writes never accept patterns."),
                "k": {"type": "integer", "description": "max results (default 16)"},
                "run_id": s("host trajectory id for recall telemetry")
            }, "required": ["subject"]}
        }),
        json!({
            "name": "areev_add",
            "description": "Add a durable memory grain (append-only; content-addressed). Use type 'fact' with subject/relation/object fields for structured knowledge.",
            "inputSchema": {"type": "object", "properties": {
                "type": s("grain type: fact|event|state|goal|observation|... (default fact)"),
                "fields": {"type": "object", "description": "grain fields, e.g. {subject, relation, object, confidence}"},
                "namespace": s("optional namespace"),
                "idempotent": {"type": "boolean", "description": "if true, skip the write when this exact value is already the current head for (subject, relation) — returns the existing hash and inserted:false. Use when re-learning a fact you may already hold."}
            }, "required": ["fields"]}
        }),
        json!({
            "name": "areev_supersede",
            "description": "Evolve a memory: write a new version superseding old_hash. The old version is preserved (append-only history), never deleted.",
            "inputSchema": {"type": "object", "properties": {
                "old_hash": s("content address (64-hex) of the version to supersede"),
                "type": s("grain type of the new version (default fact)"),
                "fields": {"type": "object"},
                "namespace": s("optional namespace")
            }, "required": ["old_hash", "fields"]}
        }),
        json!({
            "name": "areev_forget",
            "description": "Erase a grain from the hot store (tombstoned in the op-log). The CAL spelling is FORGET <hash> [BECAUSE \"why\"].",
            "inputSchema": {"type": "object", "properties": {
                "hash": s("content address (64-hex) to forget")
            }, "required": ["hash"]}
        }),
        json!({
            "name": "areev_subject_report",
            "description": "DSAR read (GDPR Art. 15/20): everything subject erasure WOULD remove for one identity — exact + partition keys, full history — as {hash, type, fields} grains. Read-only; the CAL spelling is REPORT SUBJECT \"<id>\" [WITH text_mentions], and the erasure mirror is FORGET SUBJECT.",
            "inputSchema": {"type": "object", "properties": {
                "subject": s("the identity to report on"),
                "text_mentions": {"type": "boolean", "description": "also include grains whose indexed text mentions the identity (needs the text index)"}
            }, "required": ["subject"]}
        }),
        json!({
            "name": "areev_related",
            "description": "Walk the entity graph from a starting term over the given relations (bounded k-hop, breadth-first). Reverse/both directions only see relations the file declares as entity-valued.",
            "inputSchema": {"type": "object", "properties": {
                "start": s("entity term to start from, e.g. \"alice\""),
                "relations": s("comma-separated relations, e.g. \"reports_to,mg:knows\""),
                "direction": s("out (default) | in | both"),
                "depth": {"type": "integer", "description": "hops to walk, 1-4 (default 2)"},
                "limit": {"type": "integer", "description": "max entities returned (default 64)"},
                "namespace": s("optional namespace")
            }, "required": ["start", "relations"]}
        }),
        json!({
            "name": "areev_entity_at",
            "description": "As-of read on two axes: what was true in the world at T (world), or what the agent knew at T (knowledge). Use to answer questions about the past rather than the current head.",
            "inputSchema": {"type": "object", "properties": {
                "subject": s("entity, e.g. \"alice\""),
                "relation": s("relation, e.g. \"employer\""),
                "at": {"type": "integer", "description": "point in time, epoch milliseconds"},
                "axis": s("world (default) | knowledge"),
                "namespace": s("optional namespace")
            }, "required": ["subject", "relation", "at"]}
        }),
        json!({
            "name": "areev_step_actions",
            "description": "Execution records for a workflow: which grains ran which of its nodes (OMS mg:step_action). A workflow grain is immutable, so runs point at the plan rather than mutating it; retries appear as several records for one node.",
            "inputSchema": {"type": "object", "properties": {
                "workflow": s("content address (64-hex) of the Workflow grain"),
                "node": s("optional node id, to narrow to one step"),
                "limit": {"type": "integer", "description": "max records returned (default 64)"},
                "namespace": s("optional namespace")
            }, "required": ["workflow"]}
        }),
        json!({
            "name": "areev_run_trace",
            "description": "Everything recorded during a run, plus what the run produced downstream (facts/lessons derived from it). Crosses from execution history into semantic memory in one call.",
            "inputSchema": {"type": "object", "properties": {
                "run_id": s("the run identifier recorded on the run's grains"),
                "include_yield": {"type": "boolean", "description": "also return grains derived from the run (default true)"},
                "limit": {"type": "integer", "description": "max grains per section (default 64)"},
                "namespace": s("optional namespace")
            }, "required": ["run_id"]}
        }),
        json!({
            "name": "areev_runs_touching",
            "description": "Which runs produced or refined this grain — the reverse join, from a piece of memory back into execution history. Walks the provenance chain both ways. Runs that merely read the grain are not recorded: a read leaves no grain.",
            "inputSchema": {"type": "object", "properties": {
                "hash": s("content address (64-hex) of the grain"),
                "depth": {"type": "integer", "description": "provenance hops to walk, max 8 (default 4)"},
                "namespace": s("optional namespace")
            }, "required": ["hash"]}
        }),
        json!({
            "name": "areev_run_start",
            "description": "Start a governed workflow run: journaled, checkpointed, budgeted, resumable. Host tools execute only when the server was configured with AREEV_RUN_TOOL_CMD; Client-gated nodes park the run and return a requires_action envelope whose asks are answered with areev_run_respond by a DIFFERENT principal.",
            "inputSchema": {"type": "object", "properties": {
                "workflow": s("content address (64-hex) of the Workflow grain"),
                "run_id": s("fresh run identity (reuse is refused; fork instead)"),
                "input": {"description": "the run input as any JSON value"},
                "max_tokens": {"type": "integer"}, "max_usd_micros": {"type": "integer"},
                "max_wall_ms": {"type": "integer"}, "max_supersteps": {"type": "integer"}
            }, "required": ["workflow", "run_id"]}
        }),
        json!({
            "name": "areev_run_resume",
            "description": "Resume a parked or interrupted run from its latest checkpoint — settles answered asks, expires stale ones, re-delivers crash-window intents under the same idempotency key (recorded).",
            "inputSchema": {"type": "object", "properties": {
                "run_id": s("the run to resume")
            }, "required": ["run_id"]}
        }),
        json!({
            "name": "areev_run_respond",
            "description": "Answer one pending Client ask by tool_call_id. The responder is the SERVER-BOUND identity (the --as principal, or agent:mcp) — never a client-supplied name, so an agent cannot approve gates on runs that same identity triggered (separation of duties is structural). Approvals from another principal go through a server bound to it, the console, or the CLI. Rejected/late responses are journaled as audit evidence before the error returns.",
            "inputSchema": {"type": "object", "properties": {
                "run_id": s("the parked run"),
                "tool_call_id": s("the ask id from the requires_action envelope (never an index)"),
                "result": {"description": "the response value as any JSON value"},
                "is_error": {"type": "boolean", "description": "true = refuse the ask (journaled as UserAborted)"}
            }, "required": ["run_id", "tool_call_id"]}
        }),
        json!({
            "name": "areev_run_cancel",
            "description": "Write the kill-switch marker for a run (deliberately the lowest-privilege run verb — the brake must never be blocked by missing privilege). A live driver drains at its next superstep boundary; resume finalizes a parked one.",
            "inputSchema": {"type": "object", "properties": {
                "run_id": s("the run to cancel"),
                "because": s("reason, recorded on the cancel Fact")
            }, "required": ["run_id"]}
        }),
        json!({
            "name": "areev_run_verify",
            "description": "Journal-consistent replay of a run: re-derives every checkpoint from the journaled events and byte-compares against the stored chain, writing nothing. Divergences name the differing fields (RUN-E009).",
            "inputSchema": {"type": "object", "properties": {
                "run_id": s("the run to verify")
            }, "required": ["run_id"]}
        }),
        json!({
            "name": "areev_run_list",
            "description": "Recent run ids in this namespace, newest first (manifest census).",
            "inputSchema": {"type": "object", "properties": {
                "limit": {"type": "integer", "description": "max runs (default 20)"}
            }}
        }),
        json!({
            "name": "areev_tool_provenance",
            "description": "One-command forensics for a piece of governed code: the loop recommendations targeting this tool hash (status, summary, pinned evalset) plus which runs touched it — the full chain from code to approval to execution.",
            "inputSchema": {"type": "object", "properties": {
                "hash": s("content address (64-hex) of the tool/code grain"),
                "depth": {"type": "integer", "description": "provenance hops for runs_touching (default 2)"}
            }, "required": ["hash"]}
        }),
        json!({
            "name": "areev_record_tool_call",
            "description": "Record one tool invocation as a trajectory Tool grain, including its JSON arguments, result, error status, thread, provider call id, run correlation, execution lifecycle, and the mg:step_action link to the workflow node it executed.",
            "inputSchema": {"type": "object", "properties": {
                "tool_name": s("tool/function name"),
                "input": {"description": "tool arguments as any JSON value"},
                "result": s("tool result text or JSON string"),
                "is_error": {"type": "boolean", "description": "whether the invocation failed (default false)"},
                "thread": s("optional session/thread id"),
                "call_id": s("optional provider tool-call id; synthesized when absent"),
                "run_id": s("optional run correlation id (read back by areev_run_trace)"),
                "workflow_hash": s("optional Workflow grain hash this call executed a node of; requires node_id"),
                "node_id": s("optional workflow node id; requires workflow_hash"),
                "status": s("optional execution status: pending | completed | failed"),
                "failure_cause": s("optional failure classifier: timeout | executor_error | schema_validation_failed | user_aborted | unknown"),
                "executor_kind": s("optional executor: host | client"),
                "correlation_id": s("optional async correlation id"),
                "namespace": s("optional namespace")
            }, "required": ["tool_name", "result"]}
        }),
        json!({
            "name": "areev_run_manifest",
            "description": "Persist a reproducible run configuration as a content-addressed State plus the run-to-config provenance link.",
            "inputSchema": {"type": "object", "properties": {
                "run_id": s("run identifier used by trajectory grains"),
                "config": {"type": "object", "description": "pinned model/build/adapter/quantization/runtime, sampling parameters, system prompt and tool catalogue"}
            }, "required": ["run_id", "config"]}
        }),
        json!({
            "name": "areev_remember",
            "description": "Store raw conversational content as an Event grain (session transcript). Distill durable knowledge with areev_add afterwards.",
            "inputSchema": {"type": "object", "properties": {
                "content": s("the utterance/observation text"),
                "session_id": s("optional session/thread id"),
                "run_id": s("optional run id — makes this turn readable via areev_run_trace"),
                "role": s("optional: user|assistant|system|tool"),
                "observer": s("optional id of the agent capturing this"),
                "namespace": s("optional namespace")
            }, "required": ["content"]}
        }),
        json!({
            "name": "areev_cal",
            "description": "Execute a CAL statement (RECALL/ASSEMBLE/EXISTS/HISTORY/ADD/SUPERSEDE/...). Destructive statements (FORGET, FORGET SUBJECT, PURGE OLDER THAN) require a BECAUSE reason, are gated by the session's grants and the server's destructive-ops setting, and are audit-logged; destruction takes a hash, an identity, or an age — never a predicate.",
            "inputSchema": {"type": "object", "properties": {
                "query": s("CAL text, e.g. RECALL facts WHERE subject = \"alice\" | COUNT")
            }, "required": ["query"]}
        }),
        json!({
            "name": "areev_loop",
            "description": "Run one governed self-improvement pass (deterministic analyzers; auto-apply only under the host policy the server was started with; LLM reflection attaches on the CLI, not here) and return the run outcome plus the pending recommendation queue. Call at session start; review pending recommendations before acting.",
            "inputSchema": {"type": "object", "properties": {
                "min_new": {"type": "integer", "description": "only run if at least this many new grains since the last run (optional)"},
                "min_new_errors": {"type": "integer", "description": "or this many new tool failures (optional)"}
            }}
        }),
        json!({
            "name": "areev_recommendations",
            "description": "List recommendations, or act on one. Without 'action', lists by status (default pending). With action=apply|approve|reject and a 'hash' + mandatory 'because' reason, performs the audited transition (self-approval of an agent's own proposals is blocked).",
            "inputSchema": {"type": "object", "properties": {
                "status": s("filter: pending|approved|applied|all (default pending)"),
                "action": s("apply|approve|reject (omit to list)"),
                "hash": s("recommendation hash (required for an action)"),
                "because": s("mandatory written reason for the decision")
            }}
        }),
    ]
}
