//! The run manifest (§5.3/§6.1 V7): everything a resume must know, written
//! ONCE at run start — the plan hash, the frozen tool resolutions, the
//! budgets, the principal, the redaction posture. Persisted through the
//! facade's `record_run_manifest` shape (a value-level State at epoch 0 +
//! a `run:<id> mg:harness <hash>` Fact), so the manifest replicates with
//! the file and `runs_touching` can join runs to their configuration.
//!
//! **Resolution freeze**: Named/Abstract resolution happens here, once; the
//! outcome is pinned. Resume reads the manifest and NEVER re-resolves — a
//! newer tool definition appearing between pause and resume must not
//! silently change the run.
//!
//! v1 ownership note: the F7 owner-nonce check (resume-of-a-copy detection
//! via the op-log head) needs an op-cursor read API the store does not yet
//! expose; v1 ships taint detection (forked journal tips → `RUN-E016`) and
//! the explicit `--fork` path, and records this as the documented gap.

use crate::err_run;
use areev_core::error::{Hash, Result};
use areev_core::types::Grain;
use areev_run_core::{Budgets, NodeExecutor, PlanGraph, RunError};
use areev_store::Areev;
use serde_json::json;
use std::collections::BTreeMap;

/// §5.4: what a fork descends from. Journal grains of the inherited prefix
/// carry the PARENT's run id; the fork's verifiability is this recorded
/// base-checkpoint hash (the seed checkpoint `derived_from`s it).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ForkBase {
    pub base_run: String,
    pub base_checkpoint: String,
    pub base_superstep: u64,
}

/// One frozen resolution.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PinnedTool {
    pub node: String,
    pub tool_hash: String,
    pub tool_name: String,
    /// "host" | "client" (the definition's `executor_kind`, default host).
    pub executor: String,
    /// The definition's `executor_uri`, when it names a `cas://sha256:` code
    /// blob. Pinned here so a run executes the address it resolved at start
    /// even if the Definition is superseded mid-run — the same resolution
    /// freeze the bindings get. Absent for every ordinary tool, so existing
    /// manifests serialize byte-identically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor_uri: Option<String>,
    /// The Definition's declared runtime ("wasm32-areev"), pinned with the
    /// address so a mid-run supersession cannot re-route a blob from the
    /// sandbox to native exec. Absent = native, and existing manifests
    /// serialize byte-identically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// The declared sandbox limits (`{"fuel", "max_pages"}`), pinned with
    /// the runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_limits: Option<serde_json::Value>,
    /// The Definition's declared `capabilities` (#101), frozen at start
    /// beside the pinned runtime so the effective set — `declared ∩
    /// host-granted` — replays identically and a mid-run supersession cannot
    /// widen what a module may reach. Absent for every non-capability tool,
    /// so existing manifests serialize byte-identically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<serde_json::Value>,
}

/// The manifest, as serialized into the run-config State grain.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunManifest {
    pub run_id: String,
    pub plan_hash: String,
    pub principal: String,
    pub pinned: Vec<PinnedTool>,
    pub budgets: BudgetsSpec,
    pub ask_ttl_sec: Option<i64>,
    /// D9: the redaction posture, explicit in every manifest — "none" or a
    /// redactor id — so every audit sees what the journal did or did not
    /// scrub.
    pub redaction: String,
    /// The run's input — §5's function is `(plan, manifest, input, journal)`
    /// and the manifest is where the input is journaled; verify replays
    /// seed from here, never from a checkpoint's post-reducer context.
    pub input: serde_json::Value,
    /// Per-LLM-call `max_tokens` — both the request ceiling handed to the
    /// model and the §6.7 per-dispatch reservation the scheduler checks
    /// before emitting an LLM effect.
    #[serde(default)]
    pub llm_max_tokens: Option<u32>,
    /// Typed reducers (§6.5): state_key → builtin reducer name, read off
    /// the Workflow grain's `reducers` field and FROZEN here — a resume
    /// must merge exactly as the original run did. Undeclared keys are LWW.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reducers: BTreeMap<String, String>,
    /// Present exactly when this run is a §5.4 fork.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_of: Option<ForkBase>,
}

/// Serializable twin of the core's `Budgets` (kept separate so the manifest
/// wire shape is owned here, not by the pure crate).
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct BudgetsSpec {
    pub max_supersteps: Option<u64>,
    pub max_tokens: Option<u64>,
    pub max_usd_micros: Option<u64>,
    pub max_wall_ms: Option<u64>,
    pub max_storage_bytes: Option<u64>,
}

impl BudgetsSpec {
    pub fn to_budgets(self) -> Budgets {
        Budgets {
            max_supersteps: self.max_supersteps.unwrap_or(Budgets::default().max_supersteps),
            max_tokens: self.max_tokens,
            max_usd_micros: self.max_usd_micros,
            max_wall_ms: self.max_wall_ms,
            max_storage_bytes: self.max_storage_bytes,
        }
    }
}

impl RunManifest {
    /// Freeze resolutions for every node (V3 + V7):
    /// - **Bound** (a `bindings` hash): must resolve to a Tool Definition.
    /// - **Named** (no binding): a Definition head whose `tool_name` equals
    ///   the node id, found via the definition catalogue.
    /// - **Abstract** (neither): Wave 2 (`RUN-E006` until then).
    // Mirrored to the CLI/MCP surfaces like `record_tool_call`: grouped
    // params would make the scalar-in mirrors diverge.
    #[allow(clippy::too_many_arguments)]
    pub fn resolve(
        m: &mut Areev,
        ns: &str,
        run_id: &str,
        plan_hash: &Hash,
        plan: &PlanGraph,
        principal: &str,
        budgets: BudgetsSpec,
        ask_ttl_sec: Option<i64>,
        input: serde_json::Value,
        llm_available: bool,
        llm_max_tokens: Option<u32>,
    ) -> std::result::Result<RunManifest, RunError> {
        let mut pinned = Vec::with_capacity(plan.nodes.len());
        for (i, node) in plan.nodes.iter().enumerate() {
            let resolved = match &plan.bindings[i] {
                Some(hash_str) => {
                    let h = Hash::from_hex(hash_str).map_err(|_| RunError::UnresolvedRef {
                        what: format!("binding for node '{node}' is not a content address"),
                    })?;
                    let g = m.get(&h).map_err(|e| RunError::UnresolvedRef {
                        what: format!("binding for node '{node}': {e}"),
                    })?;
                    if g.grain_type == areev_core::types::GrainType::Workflow {
                        // A binding to another plan = a subgraph node.
                        PinnedTool {
                            node: node.clone(),
                            tool_hash: h.to_hex(),
                            tool_name: node.clone(),
                            executor: "subgraph".into(),
                            executor_uri: None,
                            runtime: None,
                            runtime_limits: None,
                            capabilities: None,
                        }
                    } else {
                        pin_from_definition(node, &h, &g)?
                    }
                }
                None => {
                    // Named: the newest Definition with tool_name == node;
                    // otherwise ABSTRACT (§6.2) — the node label is an
                    // instruction the model interprets — which needs the
                    // tool-calling seam configured.
                    match find_definition_by_name(m, ns, node) {
                        Some((h, g)) => pin_from_definition(node, &h, &g)?,
                        None if llm_available => PinnedTool {
                            node: node.clone(),
                            tool_hash: String::new(),
                            tool_name: node.clone(),
                            executor: "abstract".into(),
                            executor_uri: None,
                            runtime: None,
                            runtime_limits: None,
                            capabilities: None,
                        },
                        None => return Err(RunError::NoToolLlm { node: node.clone() }),
                    }
                }
            };
            pinned.push(resolved);
        }
        // Reducer table (§6.5): declared on the Workflow grain, validated
        // here — an unknown name fails at run start, not at first merge.
        let mut reducers: BTreeMap<String, String> = BTreeMap::new();
        if let Ok(wf_grain) = m.get(plan_hash) {
            if let Some(table) = wf_grain.fields.get("reducers").and_then(|v| v.as_object()) {
                for (key, name) in table {
                    let Some(name) = name.as_str() else {
                        return Err(RunError::InvalidPlan {
                            why: format!("reducer for state key '{key}' is not a string"),
                        });
                    };
                    if !crate::reducers::is_builtin(name) {
                        return Err(RunError::InvalidPlan {
                            why: format!(
                                "unknown reducer '{name}' for state key '{key}' \
                                 (builtins: {:?})",
                                crate::reducers::BUILTIN_REDUCERS
                            ),
                        });
                    }
                    reducers.insert(key.clone(), name.to_string());
                }
            }
        }
        Ok(RunManifest {
            run_id: run_id.to_string(),
            plan_hash: plan_hash.to_hex(),
            principal: principal.to_string(),
            pinned,
            budgets,
            ask_ttl_sec,
            redaction: "none".into(),
            input,
            llm_max_tokens,
            reducers,
            fork_of: None,
        })
    }

    /// The per-dispatch token reserve (§6.7) — the scheduler refuses to
    /// emit an LLM effect that could not fit under the token ceiling.
    pub fn llm_reserve_tokens(&self) -> u64 {
        u64::from(self.llm_max_tokens.unwrap_or(1024))
    }

    /// The executor vector the scheduler env consumes, in node order. An
    /// abstract node's offered tools are the manifest's HOST-pinned tools —
    /// derived, not stored, so the manifest stays minimal and the offer is
    /// deterministic from it.
    pub fn executors(&self) -> Vec<NodeExecutor> {
        let offered: Vec<areev_run_core::OfferedTool> = self
            .pinned
            .iter()
            .filter(|p| p.executor == "host")
            .map(|p| areev_run_core::OfferedTool {
                tool_name: p.tool_name.clone(),
                tool_hash: p.tool_hash.clone(),
            })
            .collect();
        self.pinned
            .iter()
            .map(|p| match p.executor.as_str() {
                "client" => NodeExecutor::Client {
                    tool_hash: p.tool_hash.clone(),
                    tool_name: p.tool_name.clone(),
                },
                "subgraph" => NodeExecutor::Subgraph { workflow_hash: p.tool_hash.clone() },
                "abstract" => NodeExecutor::Abstract { tools: offered.clone() },
                _ => NodeExecutor::Host {
                    tool_hash: p.tool_hash.clone(),
                    tool_name: p.tool_name.clone(),
                },
            })
            .collect()
    }

    /// Persist via the established run-manifest shape (a value-level State
    /// at epoch 0 + the `run:<id> mg:harness <hash>` link Fact in
    /// `agent:harness`). Store-direct like every other runtime control
    /// write: the driver's `run.execute` check is THE authorization decision
    /// for starting a run — requiring executor principals to additionally
    /// hold `write ON agent:harness` would make every governed deployment
    /// hand out grants on a governance namespace. Attribution still lands on
    /// the grains (`author_did`). Returns (config hash, link hash).
    pub fn persist(&self, m: &mut Areev) -> Result<(Hash, Hash)> {
        let config = serde_json::to_value(json!({ "areev_run": self }))
            .expect("manifest serializes");
        let mut state = areev_core::types::State::new(config)
            .namespace(areev_core::authz::HARNESS_NS)
            .created_at(0);
        state.common.author_did = Some(self.principal.clone());
        let config_hash = m.add(&state)?;
        let mut link = areev_core::types::Fact::new(
            &format!("run:{}", self.run_id),
            "mg:harness",
            &config_hash.to_hex(),
        )
        .namespace(areev_core::authz::HARNESS_NS);
        link.common.author_did = Some(self.principal.clone());
        link.common
            .extra_fields
            .insert("run_id".into(), serde_json::json!(self.run_id));
        let link_hash = m.add(&link)?;
        Ok((config_hash, link_hash))
    }

    /// Load a run's manifest back from the file (resume/verify path).
    pub fn load(m: &mut Areev, run_id: &str) -> std::result::Result<RunManifest, RunError> {
        let link = m
            .latest(areev_core::authz::HARNESS_NS, &format!("run:{run_id}"), "mg:harness")
            .map_err(err_run)?
            .ok_or_else(|| RunError::ManifestMismatch {
                why: format!("no manifest link for run '{run_id}'"),
            })?;
        let config_hash = link
            .get_str("object")
            .and_then(|h| Hash::from_hex(h).ok())
            .ok_or_else(|| RunError::ManifestMismatch {
                why: "manifest link carries no config hash".into(),
            })?;
        let config = m.get(&config_hash).map_err(|e| RunError::ManifestMismatch {
            why: format!("manifest config unreadable: {e}"),
        })?;
        let manifest = config
            .fields
            .get("context")
            .or_else(|| config.fields.get("context_data"))
            .and_then(|c| c.get("areev_run"))
            .cloned()
            .ok_or_else(|| RunError::ManifestMismatch {
                why: "config State carries no areev_run manifest".into(),
            })?;
        serde_json::from_value(manifest).map_err(|e| RunError::ManifestMismatch {
            why: format!("manifest does not parse: {e}"),
        })
    }
}

fn pin_from_definition(
    node: &str,
    h: &Hash,
    g: &areev_core::format::deserialize::DeserializedGrain,
) -> std::result::Result<PinnedTool, RunError> {
    if g.get_str("kind") != Some("definition") {
        return Err(RunError::UnresolvedRef {
            what: format!(
                "node '{node}' binds {} which is not a Tool Definition \
                 (kind = {:?})",
                h.to_hex(),
                g.get_str("kind").unwrap_or("execution")
            ),
        });
    }
    let tool_name = g
        .get_str("tool_name")
        .unwrap_or(node)
        .to_string();
    // Refuse a malformed tool name at resolve time, before the run starts.
    // This name reaches a host tool as `$AREEV_TOOL_NAME`, and it arrives from
    // a plan grain — which may have been imported from a bundle whose author we
    // do not vouch for (import verifies content integrity, not authorship). It
    // is not shell-interpolated, so this is not injection; the point is that an
    // attacker-influenced string with newlines or shell metacharacters should
    // never reach a child's environment, because a perfectly ordinary tool
    // script that does something like `eval "$AREEV_TOOL_NAME"` turns it into
    // one. Failing here also fails loudly and early rather than mid-superstep.
    if !areev_core::types::json_schema_subset::is_valid_tool_name(&tool_name) {
        return Err(RunError::UnresolvedRef {
            what: format!(
                "node '{node}' binds {} whose tool_name {tool_name:?} is not a valid \
                 tool name (1-64 chars of [A-Za-z0-9_.-])",
                h.to_hex()
            ),
        });
    }
    let executor = match g.get_str("executor_kind") {
        Some("client") => "client",
        _ => "host",
    };
    // `executor_uri` used to be written, parsed, CAL-buildable — and read by
    // nothing, so a Definition naming `executor://crm.lookup@v3` executed
    // whatever `--tool-cmd` happened to be. That is the failure with no
    // symptom this runtime exists to refuse, so every value now either
    // dispatches or is refused by name.
    let executor_uri = match g.get_str("executor_uri") {
        None => None,
        Some(uri) if executor == "client" => {
            return Err(RunError::CodeExecRefused {
                condition: format!(
                    "node '{node}' binds a client tool carrying executor_uri {uri:?} — a \
                     client tool is answered by a person through `run respond`, so it has \
                     no executor to name"
                ),
            })
        }
        Some(uri) => {
            let hex = crate::executor::strip_cas(uri);
            if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(RunError::CodeExecRefused {
                    condition: format!(
                        "node '{node}' names executor_uri {uri:?}, which this build cannot \
                         dispatch — the only executable form is a content address, \
                         cas://sha256:<64 hex>"
                    ),
                });
            }
            Some(format!("cas://sha256:{}", hex.to_ascii_lowercase()))
        }
    };
    // The declared runtime (#86). Fail closed on both axes: a runtime on a
    // tool with no code blob describes nothing, and an unknown runtime —
    // possible on a grain that arrived by sync from another implementation,
    // since our own write path refuses it — must not silently fall back to
    // native exec, which would run wasm bytes as a program.
    let runtime = match g.get_str("runtime") {
        None | Some("native") => None,
        Some(rt) if executor_uri.is_none() => {
            return Err(RunError::CodeExecRefused {
                condition: format!(
                    "node '{node}' declares runtime {rt:?} but names no executor_uri —                      a runtime routes a code blob, and there is none"
                ),
            })
        }
        Some(rt) if crate::executor::is_sandbox_runtime(rt) => Some(rt.to_string()),
        Some(rt) => {
            return Err(RunError::CodeExecRefused {
                condition: format!(
                    "node '{node}' declares runtime {rt:?}, which this build cannot                      dispatch — accepted: native, wasm32-areev, wasm32-areev-io"
                ),
            })
        }
    };
    let runtime_limits = if runtime.is_some() { g.fields.get("runtime_limits").cloned() } else { None };
    // The declared capability set (#101). Fail closed on both axes, the same
    // way the runtime does: a declaration on a tool whose runtime cannot honour
    // it describes nothing, and a malformed one must be refused at start rather
    // than at the first call — a grain can arrive by sync from another
    // implementation, and our own write path is not the only way in.
    let capabilities = match g.fields.get("capabilities") {
        None => None,
        Some(v) if v.is_null() => None,
        Some(v) => {
            if !crate::executor::runtime_allows_capabilities(runtime.as_deref()) {
                return Err(RunError::CodeExecRefused {
                    condition: format!(
                        "node '{node}' declares capabilities but its runtime is {:?} — \
                         capabilities require runtime \"wasm32-areev-io\"",
                        runtime.as_deref().unwrap_or("native")
                    ),
                });
            }
            areev_core::types::capability::Declaration::parse(v).map_err(|e| RunError::CodeExecRefused {
                condition: format!("node '{node}' declares malformed capabilities: {e}"),
            })?;
            Some(v.clone())
        }
    };
    // The inverse: a capability RUNTIME with nothing declared can reach
    // nothing, and saying so at start beats a module that instantiates and
    // then has every call refused.
    if crate::executor::runtime_allows_capabilities(runtime.as_deref()) && capabilities.is_none() {
        return Err(RunError::CodeExecRefused {
            condition: format!(
                "node '{node}' declares runtime \"wasm32-areev-io\" but no capabilities — \
                 a capability runtime with an empty declaration can reach nothing; \
                 use \"wasm32-areev\" for a pure module"
            ),
        });
    }
    Ok(PinnedTool {
        node: node.to_string(),
        tool_hash: h.to_hex(),
        tool_name,
        executor: executor.into(),
        executor_uri,
        runtime,
        runtime_limits,
        capabilities,
    })
}

/// Newest Definition with this tool_name, or None. Definitions are few; a
/// bounded scan of the namespace's Tool grains is the v1 catalogue (a
/// dedicated definition index is a later optimization, never a semantics
/// change). Deterministic tiebreak: newest `created_at`, then highest hash
/// — the provisional-head rule's shape, so every host resolves identically.
fn find_definition_by_name(
    m: &mut Areev,
    ns: &str,
    name: &str,
) -> Option<(Hash, areev_core::format::deserialize::DeserializedGrain)> {
    // Journal intents/results are Tool grains IN THE SAME NAMESPACE (two
    // per effect), so a small window would let an active file crowd its own
    // definitions out of the catalogue — and with an LLM configured the
    // miss silently becomes an abstract node. Scan wide until a dedicated
    // definition index exists.
    const CATALOG_SCAN: usize = 200_000;
    let hits = m
        .recent(ns, Some(areev_core::types::GrainType::Tool), CATALOG_SCAN)
        .ok()?;
    let mut best: Option<(i64, String)> = None;
    let mut candidates: BTreeMap<String, areev_core::format::deserialize::DeserializedGrain> =
        BTreeMap::new();
    for g in hits {
        if g.get_str("kind") == Some("definition") && g.get_str("tool_name") == Some(name) {
            let at = g.get_i64("created_at").unwrap_or(0);
            let key = (at, g.hash.to_hex());
            if best.as_ref().is_none_or(|b| key > *b) {
                best = Some(key);
            }
            candidates.insert(g.hash.to_hex(), g);
        }
    }
    let (_, hex) = best?;
    let g = candidates.remove(&hex)?;
    let h = Hash::from_hex(&hex).ok()?;
    Some((h, g))
}

/// The nodes of `plan_hash` that would resolve as **abstract** — no binding
/// and no Definition head whose `tool_name` matches — plus whether the plan
/// itself is present at all.
///
/// A pre-flight read: it takes no lease, writes nothing, and is exactly the
/// half of [`RunManifest::resolve`] that decides `RUN-E006`. It exists so a
/// declaration that names a plan can say at AUTHORING time what the plan will
/// need at fire time (#73): `areev trigger add --workflow <WF>` used to accept
/// a plan whose nodes did not resolve, report `waiting`, and fail at the first
/// firing — on the operator's mailbox rather than at their keyboard.
///
/// Abstract is not an error here, which is why this returns node ids rather
/// than a `Result`: an abstract node is legitimate with a tool-calling model
/// configured. The caller decides whether that is a warning or nothing.
pub fn abstract_nodes(
    m: &mut Areev,
    ns: &str,
    plan_hash: &Hash,
) -> std::result::Result<Vec<String>, String> {
    let wf = m
        .get(plan_hash)
        .map_err(|e| format!("{e}"))?
        .to_workflow()
        .map_err(|e| format!("{e}"))?;
    let plan = PlanGraph::build(&wf).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for (i, node) in plan.nodes.iter().enumerate() {
        if plan.bindings[i].is_none() && find_definition_by_name(m, ns, node).is_none() {
            out.push(node.clone());
        }
    }
    Ok(out)
}
