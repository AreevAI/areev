//! Saved query registry — stores and manages named, parameterized CAL queries.
//!
//! Parallel to `templates.rs` for template management (FR-003).
//! Saved queries persist in Fjall meta partition with `qry:` key prefix.

/// Namespace/saved-query slug rule:
/// 2..=64 chars of [a-z0-9-], no leading/trailing '-'.
fn is_valid_slug(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() < 2 || bytes.len() > 64 {
        return false;
    }
    let ok = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
    if !ok(bytes[0]) || bytes[0] == b'-' {
        return false;
    }
    if bytes[bytes.len() - 1] == b'-' {
        return false;
    }
    bytes.iter().all(|&b| ok(b))
}


use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use super::ast::QueryParam;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum number of saved queries per namespace.
pub const MAX_QUERIES_PER_NAMESPACE: usize = 100;
/// Maximum query body size in bytes.
pub const MAX_QUERY_BODY_SIZE: usize = 8192;
/// Maximum parameters per saved query.
pub const MAX_QUERY_PARAMS: usize = 10;

/// Regex for valid query names (same as template names).
/// Allows mixed case, spaces, hyphens, underscores, and digits.
static QUERY_NAME_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^[a-zA-Z][a-zA-Z0-9 _-]{0,63}$").unwrap());

/// Validate an agent-namespaced query name: `agent/<slug>/<suffix>`.
///
/// Used by `is_valid_name` to permit per-agent saved queries without
/// widening the broader query-name regex. Mirrors the agent slug rules
/// by the slug rule shared with namespaces, so the namespace + saved
/// query name stay in lockstep.
fn is_valid_agent_query_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("agent/") else {
        return false;
    };
    let Some(slash) = rest.find('/') else {
        return false;
    };
    let slug = &rest[..slash];
    let suffix = &rest[slash + 1..];
    if !is_valid_slug(slug) {
        return false;
    }
    if suffix.is_empty() || suffix.len() > 16 {
        return false;
    }
    suffix.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

// ---------------------------------------------------------------------------
// Persisted format (JSON in Fjall meta partition)
// ---------------------------------------------------------------------------

/// Serialized form stored in Fjall under `qry:{name}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedQuery {
    pub body: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub params: Vec<QueryParam>,
    /// Epoch seconds of the last time this query was executed via RUN.
    #[serde(default)]
    pub last_run_at: Option<u64>,
    /// Epoch seconds when this query was last created or modified.
    #[serde(default)]
    pub updated_at: Option<u64>,
}

// ---------------------------------------------------------------------------
// Registry types
// ---------------------------------------------------------------------------

/// In-memory entry for a saved query.
#[derive(Debug, Clone)]
pub struct QueryEntry {
    pub body: String,
    pub description: String,
    pub params: Vec<QueryParam>,
    /// Epoch seconds of the last time this query was executed via RUN.
    pub last_run_at: Option<u64>,
    /// Epoch seconds when this query was last created or modified.
    pub updated_at: Option<u64>,
    /// True for server-shipped built-in queries (immutable, not persisted).
    pub builtin: bool,
}

/// Summary info returned by `list()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryListEntry {
    pub name: String,
    pub description: String,
    pub param_count: usize,
    pub body_size: usize,
    /// Epoch seconds of the last time this query was executed via RUN.
    pub last_run_at: Option<u64>,
    /// Epoch seconds when this query was last created or modified.
    pub updated_at: Option<u64>,
    /// True for server-shipped built-in queries.
    pub builtin: bool,
}

// ---------------------------------------------------------------------------
// QueryRegistry
// ---------------------------------------------------------------------------

/// In-memory registry of saved queries, backed by Fjall persistence.
#[derive(Debug)]
pub struct QueryRegistry {
    queries: HashMap<String, QueryEntry>,
}

impl Default for QueryRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryRegistry {
    /// Create a registry pre-loaded with built-in queries.
    pub fn new() -> Self {
        let mut reg = Self {
            queries: HashMap::new(),
        };
        reg.load_builtins();
        reg
    }

    /// Validate a query name.
    ///
    /// Two name shapes are accepted:
    /// - **Traditional**: matches `QUERY_NAME_RE` — starts with a letter,
    ///   alphanumerics + space + underscore + hyphen, max 64 chars.
    /// - **Agent-namespaced**: `agent/<slug>/<suffix>` where `<slug>` is a
    ///   valid agent slug (kebab-case, 2..=64 chars) and `<suffix>` is a
    ///   short lowercase identifier (e.g. `ctx`, `tool_summary`). Used by
    ///   the agent runtime so each agent can hold its own assemble query
    ///   inside its def namespace's saved-query budget (M1-09).
    pub fn is_valid_name(name: &str) -> bool {
        // Reject leading/trailing whitespace and consecutive spaces.
        if name != name.trim() || name.contains("  ") {
            return false;
        }
        if QUERY_NAME_RE.is_match(name) {
            return true;
        }
        is_valid_agent_query_name(name)
    }

    /// Register a new saved query.
    pub fn register(
        &mut self,
        name: &str,
        body: &str,
        description: &str,
        params: &[QueryParam],
    ) -> Result<(), String> {
        self.register_with_last_run(name, body, description, params, None)
    }

    /// Register a new saved query with optional timestamps
    /// (used when rehydrating from Fjall on startup).
    pub fn register_with_last_run(
        &mut self,
        name: &str,
        body: &str,
        description: &str,
        params: &[QueryParam],
        last_run_at: Option<u64>,
    ) -> Result<(), String> {
        self.register_full(name, body, description, params, last_run_at, None)
    }

    /// Register a new saved query with all optional timestamps.
    pub fn register_full(
        &mut self,
        name: &str,
        body: &str,
        description: &str,
        params: &[QueryParam],
        last_run_at: Option<u64>,
        updated_at: Option<u64>,
    ) -> Result<(), String> {
        if !Self::is_valid_name(name) {
            return Err(format!(
                "invalid query name \"{name}\": must start with a letter, max 64 chars, only letters/digits/spaces/hyphens/underscores"
            ));
        }
        if body.len() > MAX_QUERY_BODY_SIZE {
            return Err(format!(
                "query body too large ({} bytes, max {})",
                body.len(),
                MAX_QUERY_BODY_SIZE
            ));
        }
        if params.len() > MAX_QUERY_PARAMS {
            return Err(format!(
                "too many parameters ({}, max {})",
                params.len(),
                MAX_QUERY_PARAMS
            ));
        }
        // Builtins are immutable — user-defined queries can be redefined (upsert).
        if let Some(existing) = self.queries.get(name) {
            if existing.builtin {
                return Err(format!("saved query \"{name}\" already exists"));
            }
        }
        if self.queries.len() >= MAX_QUERIES_PER_NAMESPACE {
            return Err(format!(
                "too many saved queries ({}, max {})",
                self.queries.len(),
                MAX_QUERIES_PER_NAMESPACE
            ));
        }

        let ts = updated_at.unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        });
        self.queries.insert(
            name.to_string(),
            QueryEntry {
                body: body.to_string(),
                description: description.to_string(),
                params: params.to_vec(),
                last_run_at,
                updated_at: Some(ts),
                builtin: false,
            },
        );
        Ok(())
    }

    /// Remove a saved query. Returns error if not found or built-in.
    pub fn delete(&mut self, name: &str) -> Result<(), String> {
        if let Some(entry) = self.queries.get(name) {
            if entry.builtin {
                return Err(format!("cannot delete built-in query \"{name}\""));
            }
        }
        if self.queries.remove(name).is_none() {
            return Err(format!("saved query \"{name}\" not found"));
        }
        Ok(())
    }

    /// Whether a query is a built-in (immutable, not persisted to Fjall).
    pub fn is_builtin(&self, name: &str) -> bool {
        self.queries.get(name).is_some_and(|e| e.builtin)
    }

    /// Get a saved query by name.
    pub fn get(&self, name: &str) -> Option<&QueryEntry> {
        self.queries.get(name)
    }

    /// List all saved queries.
    pub fn list(&self) -> Vec<QueryListEntry> {
        let mut entries: Vec<_> = self
            .queries
            .iter()
            .map(|(name, entry)| QueryListEntry {
                name: name.clone(),
                description: entry.description.clone(),
                param_count: entry.params.len(),
                body_size: entry.body.len(),
                last_run_at: entry.last_run_at,
                updated_at: entry.updated_at,
                builtin: entry.builtin,
            })
            .collect();
        entries.sort_by_key(|b| std::cmp::Reverse(b.updated_at.unwrap_or(0)));
        entries
    }

    /// Number of registered queries.
    pub fn len(&self) -> usize {
        self.queries.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.queries.is_empty()
    }

    // ── Built-in queries ──────────────────────────────────────────────────

    /// Insert a built-in query (immutable, not persisted to Fjall).
    fn insert_builtin(&mut self, name: &str, body: &str, description: &str, params: &[QueryParam]) {
        self.queries.insert(
            name.to_string(),
            QueryEntry {
                body: body.to_string(),
                description: description.to_string(),
                params: params.to_vec(),
                last_run_at: None,
                updated_at: None,
                builtin: true,
            },
        );
    }

    /// Load the 18 built-in queries (10 standard + 1 agent runtime + 7 enterprise harness templates).
    fn load_builtins(&mut self) {
        // ── 1. Customer Support Context ──
        // Agent: Customer support classifier / responder.
        // Layout: user preferences → topic-filtered history → topic-filtered knowledge → recent tools.
        self.insert_builtin(
            "Customer Support Context",
            "ASSEMBLE \"Customer Support Context\" FROM\n\
             \x20 preferences: (RECALL facts ABOUT $topic\n\
             \x20                WHERE subject = $user_id\n\
             \x20                AND relation IS PREFERENCE RECENT 10),\n\
             \x20 history:     (RECALL events ABOUT $topic\n\
             \x20                WHERE subject = $user_id RECENT 20),\n\
             \x20 knowledge:   (RECALL facts ABOUT $topic\n\
             \x20                WHERE subject = $user_id RECENT 15),\n\
             \x20 tools:     (RECALL tools ABOUT $topic\n\
             \x20                WHERE user_id = $user_id RECENT 10)\n\
             BUDGET 4000 tokens\n\
             FORMAT markdown\n\
             WITH dedup, preference_enrichment, recency_weight(0.7)",
            "Customer support agent context: topic-scoped user preferences, interaction history, knowledge, and recent actions",
            &[
                QueryParam { name: "user_id".to_string(), default: None },
                QueryParam { name: "topic".to_string(), default: None },
            ],
        );

        // ── 2. Conversation Resume ──
        // Agent: Any conversational agent resuming a prior session.
        // All sources scoped to both user_id AND session_id.
        self.insert_builtin(
            "Conversation Resume",
            "ASSEMBLE \"Conversation Resume\" FROM\n\
             \x20 preferences: (RECALL facts WHERE subject = $user_id\n\
             \x20                AND session_id = $session_id\n\
             \x20                AND relation IS PREFERENCE RECENT 10),\n\
             \x20 conversation:(RECALL events WHERE subject = $user_id\n\
             \x20                AND session_id = $session_id RECENT 30),\n\
             \x20 knowledge:   (RECALL facts WHERE subject = $user_id\n\
             \x20                AND session_id = $session_id RECENT 15),\n\
             \x20 tools:     (RECALL tools WHERE user_id = $user_id\n\
             \x20                AND session_id = $session_id RECENT 10)\n\
             BUDGET 4000 tokens\n\
             FORMAT markdown\n\
             WITH dedup, recency_weight(0.8), annotate_relative_time",
            "Resume a conversation: user preferences, session history, topic facts, and recent tools",
            &[
                QueryParam { name: "user_id".to_string(), default: None },
                QueryParam { name: "session_id".to_string(), default: None },
            ],
        );

        // ── 3. User Profile Briefing ──
        // Agent: Personal assistant / onboarding / any agent that needs to "know the user".
        // Layout: preferences → knowledge → active goals → recent observations (all scoped to user).
        self.insert_builtin(
            "User Profile Briefing",
            "ASSEMBLE \"User Profile\" FROM\n\
             \x20 preferences:  (RECALL facts WHERE subject = $user_id\n\
             \x20                 AND relation IS PREFERENCE RECENT 20),\n\
             \x20 consents:     (RECALL consents WHERE subject = $user_id RECENT 10),\n\
             \x20 goals:        (RECALL goals WHERE subject = $user_id\n\
             \x20                 AND goal_state = \"active\" RECENT 10),\n\
             \x20 observations: (RECALL observations WHERE subject = $user_id\n\
             \x20                 RECENT 15)\n\
             BUDGET 3000 tokens\n\
             FORMAT markdown\n\
             WITH dedup, preference_enrichment, subject_affinity(0.8)",
            "User profile briefing: preferences, knowledge, active goals, and behavioral observations",
            &[QueryParam { name: "user_id".to_string(), default: None }],
        );

        // ── 4. Knowledge Researcher ──
        // Agent: Research / RAG agent investigating a topic in depth.
        // Layout: high-confidence facts → reasoning chains → observations → events (all scoped to subject).
        self.insert_builtin(
            "Knowledge Researcher",
            "ASSEMBLE \"Knowledge Research\" FROM\n\
             \x20 evidence:   (RECALL facts ABOUT $topic\n\
             \x20               WHERE confidence >= 0.6 RECENT 25),\n\
             \x20 analysis:   (RECALL reasonings ABOUT $topic RECENT 10),\n\
             \x20 field_notes:(RECALL observations ABOUT $topic RECENT 10),\n\
             \x20 timeline:   (RECALL events ABOUT $topic RECENT 10)\n\
             BUDGET 5000 tokens\n\
             FORMAT markdown\n\
             WITH contradiction_detection, dedup, score_breakdown",
            "Deep research context: facts, reasoning chains, observations, and events with contradiction analysis",
            &[QueryParam { name: "topic".to_string(), default: None }],
        );

        // ── 5. Task Executor ──
        // Agent: Agentic task runner (coding agent, automation agent).
        // Layout: active goals → current workflow → recent tools → error tools → state → constraints.
        self.insert_builtin(
            "Task Executor",
            "ASSEMBLE \"Task Execution Context\" FROM\n\
             \x20 goals:     (RECALL goals WHERE subject = $user_id\n\
             \x20              AND goal_state = \"active\" RECENT 10),\n\
             \x20 workflows: (RECALL workflows WHERE subject = $user_id RECENT 5),\n\
             \x20 tools:   (RECALL tools WHERE user_id = $user_id RECENT 20),\n\
             \x20 errors:    (RECALL tools WHERE user_id = $user_id\n\
             \x20              AND is_error = true RECENT 10),\n\
             \x20 state:     (RECALL states WHERE subject = $user_id RECENT 5),\n\
             \x20 constraints:(RECALL facts WHERE subject = $user_id\n\
             \x20              AND confidence >= 0.5 RECENT 10)\n\
             BUDGET 5000 tokens\n\
             FORMAT markdown\n\
             WITH conflict_resolution, dedup, recency_weight(0.8)",
            "Task execution context: goals, workflows, tools, errors, state, and constraints for agentic task runners",
            &[QueryParam { name: "user_id".to_string(), default: None }],
        );

        // ── 6. Meeting Debrief ──
        // Agent: Meeting summarizer / follow-up agent.
        // All sources scoped to both user_id AND session_id so a non-existent session returns empty.
        self.insert_builtin(
            "Meeting Debrief",
            "ASSEMBLE \"Meeting Debrief\" FROM\n\
             \x20 conversation:(RECALL events WHERE subject = $user_id\n\
             \x20                AND session_id = $session_id RECENT 50),\n\
             \x20 decisions:   (RECALL consensuses WHERE subject = $user_id\n\
             \x20                AND session_id = $session_id RECENT 10),\n\
             \x20 action_items:(RECALL tools WHERE user_id = $user_id\n\
             \x20                AND session_id = $session_id RECENT 15),\n\
             \x20 goals:       (RECALL goals WHERE subject = $user_id\n\
             \x20                AND session_id = $session_id RECENT 10)\n\
             BUDGET 5000 tokens\n\
             FORMAT markdown\n\
             WITH dedup, annotate_relative_time, session_affinity(0.9)",
            "Meeting debrief: conversation events, decisions, action items, and goals from a session",
            &[
                QueryParam { name: "user_id".to_string(), default: None },
                QueryParam { name: "session_id".to_string(), default: None },
            ],
        );

        // ── 7. Recommendation Context ──
        // Agent: Recommendation / personalization agent.
        // All sources scoped to both user_id AND topic.
        self.insert_builtin(
            "Recommendation Context",
            "ASSEMBLE \"Recommendation Context\" FROM\n\
             \x20 preferences: (RECALL facts ABOUT $topic\n\
             \x20                WHERE subject = $user_id\n\
             \x20                AND relation IS PREFERENCE RECENT 20),\n\
             \x20 interactions:(RECALL events ABOUT $topic\n\
             \x20                WHERE subject = $user_id RECENT 20),\n\
             \x20 observations:(RECALL observations ABOUT $topic\n\
             \x20                WHERE subject = $user_id RECENT 15),\n\
             \x20 knowledge:   (RECALL facts ABOUT $topic\n\
             \x20                WHERE subject = $user_id AND confidence >= 0.6 RECENT 15)\n\
             BUDGET 4000 tokens\n\
             FORMAT markdown\n\
             WITH preference_enrichment, diversity(0.5), subject_affinity(0.8), dedup, recency_weight(0.6)",
            "Recommendation agent context: topic-scoped user preferences, interactions, observations, and knowledge",
            &[
                QueryParam { name: "user_id".to_string(), default: None },
                QueryParam { name: "topic".to_string(), default: None },
            ],
        );

        // ── 8. Compliance Audit ──
        // Agent: Compliance / data governance agent.
        // Layout: active consents → data-handling events → policy observations → permission facts.
        self.insert_builtin(
            "Compliance Audit",
            "ASSEMBLE \"Compliance Audit\" FROM\n\
             \x20 consents:      (RECALL consents WHERE subject = $user_id RECENT 20),\n\
             \x20 data_activity: (RECALL events WHERE subject = $user_id RECENT 20),\n\
             \x20 audit_notes:   (RECALL observations WHERE subject = $user_id\n\
             \x20                  RECENT 15),\n\
             \x20 permissions:   (RECALL facts WHERE subject = $user_id\n\
             \x20                  AND relation IS PERMISSION RECENT 10)\n\
             BUDGET 4000 tokens\n\
             FORMAT markdown\n\
             WITH provenance, include_sources, annotate_relative_time, dedup",
            "Compliance audit context: consents, data events, observations, and permissions for a user",
            &[QueryParam { name: "user_id".to_string(), default: None }],
        );

        // ── 9. Creative Collaborator ──
        // Agent: Creative writing / worldbuilding / game master agent.
        // Layout: world/character facts → plot events → story state → narrative goals → reasoning.
        self.insert_builtin(
            "Creative Collaborator",
            "ASSEMBLE \"Creative Context\" FROM\n\
             \x20 world:     (RECALL facts ABOUT $topic RECENT 25),\n\
             \x20 plot:      (RECALL events ABOUT $topic RECENT 20),\n\
             \x20 state:     (RECALL states ABOUT $topic RECENT 5),\n\
             \x20 goals:     (RECALL goals ABOUT $topic\n\
             \x20              WHERE goal_state = \"active\" RECENT 10),\n\
             \x20 reasoning: (RECALL reasonings ABOUT $topic RECENT 10)\n\
             BUDGET 5000 tokens\n\
             FORMAT markdown\n\
             WITH conflict_resolution, contradiction_detection, dedup",
            "Creative collaborator context: world facts, plot events, story state, narrative goals, and reasoning",
            &[QueryParam { name: "topic".to_string(), default: None }],
        );

        // ── 10. Scoped Namespace Context ──
        // Agent: Multi-tenant / multi-project agent needing context from a specific namespace.
        // Layout: facts → events → tools → state, all scoped by namespace.
        self.insert_builtin(
            "Scoped Namespace Context",
            "ASSEMBLE \"Namespace Context\" FROM\n\
             \x20 knowledge:  (RECALL facts WHERE namespace = $namespace RECENT 20),\n\
             \x20 activity:   (RECALL events WHERE namespace = $namespace RECENT 15),\n\
             \x20 operations: (RECALL tools WHERE namespace = $namespace RECENT 10),\n\
             \x20 snapshots:  (RECALL states WHERE namespace = $namespace RECENT 5)\n\
             BUDGET 4000 tokens\n\
             FORMAT markdown\n\
             WITH dedup, conflict_resolution, recency_weight(0.7)",
            "Scoped namespace context: facts, events, tools, and state filtered by namespace for multi-tenant agents",
            &[QueryParam { name: "namespace".to_string(), default: None }],
        );

        // ── 11. Agent Context (M1-09) ──
        // Default ASSEMBLE for the harness runtime. Cloned per-harness into
        // `agent/<slug>/ctx` once the per-harness customization UI lands;
        // for M1 every harness uses this shared builtin via the Pilot row's
        // default `assemble_query = "agent_context"`.
        //
        // Params come pre-built from the invoker — CAL has no string
        // concat in WHERE, so namespace strings are passed in whole:
        //   $user       — caller user_id
        //   $def_ns     — harnesses/<slug>/def
        //   $session_id — conversation id (= session_id on Event grains)
        //   $query      — current user message (used by SEMANTIC ABOUT)
        //
        // Conversation history is sourced exclusively from Event grains
        // (harness-chat-events ADR, 2026-04-19). Filtering by session_id
        // is sufficient — session_id is user-scoped and hex-random, so
        // there is no cross-user collision risk.
        self.insert_builtin(
            "agent_context",
            "ASSEMBLE \"Agent Context\" FROM\n\
             \x20 goal:       (RECALL goals     WHERE namespace = $def_ns RECENT 1),\n\
             \x20 workflow:   (RECALL workflows WHERE namespace = $def_ns RECENT 1),\n\
             \x20 tools:      (RECALL tools   WHERE namespace = $def_ns LIMIT 20),\n\
             \x20 history:    (RECALL events    WHERE session_id = $session_id RECENT 12),\n\
             \x20 user_facts: (RECALL facts   ABOUT $query RECENT 8)\n\
             BUDGET 4000 tokens\n\
             FORMAT sml\n\
             WITH dedup, conflict_resolution, rerank, provenance, session_affinity(0.9), recency_weight(0.7), min_score(0.55)",
            "Default harness runtime context: harness definition (goal/workflow/tools), recent conversation Events, and rerank-filtered user facts scoped to the active query.",
            &[
                QueryParam { name: "user".to_string(), default: None },
                QueryParam { name: "def_ns".to_string(), default: None },
                QueryParam { name: "query".to_string(), default: None },
                QueryParam { name: "session_id".to_string(), default: None },
            ],
        );

        // ── Enterprise harness template queries (12–18) ─────────────────────

        // ── 12. Account 360 Context ──
        // Harness: Account 360 — latest account state, contacts, interactions,
        // and open threads for a named account.
        self.insert_builtin(
            "Account 360 Context",
            "ASSEMBLE \"Account 360\" FROM\n\
             \x20 account:      (RECALL states WHERE subject = $account_id RECENT 1),\n\
             \x20 contacts:     (RECALL states ABOUT $query\n\
             \x20                WHERE tags CONTAINS \"account\" AND subject = $account_id RECENT 10),\n\
             \x20 interactions: (RECALL events WHERE subject = $account_id RECENT 20),\n\
             \x20 threads:      (RECALL states ABOUT $query\n\
             \x20                WHERE tags CONTAINS \"thread\" AND subject = $account_id RECENT 10)\n\
             BUDGET 5000 tokens\n\
             FORMAT markdown\n\
             WITH dedup, recency_weight(0.8), provenance, rerank, min_score(0.4)",
            "Account 360 context: latest account state, contacts, interactions, and open threads for a named account",
            &[
                QueryParam { name: "account_id".to_string(), default: None },
                QueryParam { name: "query".to_string(), default: None },
            ],
        );

        // ── 13. Meeting Context ──
        // Harness: Meeting Intelligence — meeting event, decisions, active
        // action items, and discussion topics for a specific meeting.
        self.insert_builtin(
            "Meeting Context",
            "ASSEMBLE \"Meeting Context\" FROM\n\
             \x20 meeting:    (RECALL events WHERE subject = $meeting_id RECENT 1),\n\
             \x20 decisions:  (RECALL states WHERE subject = $meeting_id RECENT 20),\n\
             \x20 actions:    (RECALL goals WHERE subject = $meeting_id\n\
             \x20              AND goal_state = \"active\" RECENT 20),\n\
             \x20 topics:     (RECALL observations ABOUT $query\n\
             \x20              WHERE subject = $meeting_id RECENT 15)\n\
             BUDGET 5000 tokens\n\
             FORMAT markdown\n\
             WITH dedup, recency_weight(0.7), provenance, rerank",
            "Meeting context: meeting event, decisions, active action items, and discussion topics",
            &[
                QueryParam {
                    name: "meeting_id".to_string(),
                    default: None,
                },
                QueryParam {
                    name: "query".to_string(),
                    default: None,
                },
            ],
        );

        // ── 14. Meeting Cross Query ──
        // Harness: Meeting Intelligence (cross-meeting) — decisions, active
        // goals, and meeting events matching a query across all meetings.
        self.insert_builtin(
            "Meeting Cross Query",
            "ASSEMBLE \"Meeting Search\" FROM\n\
             \x20 decisions:    (RECALL states ABOUT $query RECENT 25),\n\
             \x20 actions:      (RECALL goals ABOUT $query\n\
             \x20                WHERE goal_state = \"active\" RECENT 15),\n\
             \x20 meetings:     (RECALL events ABOUT $query RECENT 10)\n\
             BUDGET 4000 tokens\n\
             FORMAT markdown\n\
             WITH dedup, contradiction_detection, provenance, rerank, min_score(0.5)",
            "Cross-meeting search: decisions, active goals, and meeting events matching a query",
            &[
                QueryParam {
                    name: "user".to_string(),
                    default: None,
                },
                QueryParam {
                    name: "query".to_string(),
                    default: None,
                },
            ],
        );

        // ── 15. Support CoPilot Context ──
        // Harness: Support CoPilot — KB articles, resolved tickets, and
        // runbooks for a support query.
        self.insert_builtin(
            "Support CoPilot Context",
            "ASSEMBLE \"Support Context\" FROM\n\
             \x20 kb_articles:    (RECALL facts ABOUT $query\n\
             \x20                  WHERE relation = \"documents\" RECENT 15),\n\
             \x20 resolved:       (RECALL events ABOUT $query\n\
             \x20                  WHERE tags CONTAINS \"resolved\" RECENT 10),\n\
             \x20 runbooks:       (RECALL workflows ABOUT $query RECENT 5)\n\
             BUDGET 5000 tokens\n\
             FORMAT markdown\n\
             WITH dedup, recency_weight(0.6), provenance, rerank, score_breakdown, min_score(0.45)",
            "Support co-pilot context: KB articles, resolved tickets, and runbooks for a support query",
            &[
                QueryParam { name: "query".to_string(), default: None },
            ],
        );

        // ── 16. Sales Playbook Context ──
        // Harness: Sales Playbook — competitor profiles, objection counters,
        // case studies, and pricing for battlecard generation.
        self.insert_builtin(
            "Sales Playbook Context",
            "ASSEMBLE \"Sales Playbook\" FROM\n\
             \x20 competitor:   (RECALL states ABOUT $competitor RECENT 3),\n\
             \x20 objections:   (RECALL facts ABOUT $query\n\
             \x20                WHERE relation = \"countered-by\" RECENT 15),\n\
             \x20 cases:        (RECALL facts ABOUT $industry\n\
             \x20                WHERE relation = \"demonstrated\" RECENT 10),\n\
             \x20 pricing:      (RECALL states ABOUT $query\n\
             \x20                WHERE tags CONTAINS \"pricing\" RECENT 5)\n\
             BUDGET 5000 tokens\n\
             FORMAT markdown\n\
             WITH dedup, provenance, rerank, diversity(0.5), min_score(0.4)",
            "Sales playbook context: competitor profiles, objection counters, case studies, and pricing for battlecard generation",
            &[
                QueryParam { name: "competitor".to_string(), default: None },
                QueryParam { name: "industry".to_string(), default: None },
                QueryParam { name: "query".to_string(), default: None },
            ],
        );

        // ── 17. Policy Library Context ──
        // Harness: Policy Library — active policies and user-scoped exceptions
        // for policy questions.
        self.insert_builtin(
            "Policy Library Context",
            "ASSEMBLE \"Policy Context\" FROM\n\
             \x20 policies:    (RECALL states ABOUT $query\n\
             \x20               WHERE tags CONTAINS \"active\" RECENT 10),\n\
             \x20 exceptions:  (RECALL consents WHERE subject = $user RECENT 10)\n\
             BUDGET 4000 tokens\n\
             FORMAT markdown\n\
             WITH dedup, provenance, rerank, include_sources, min_score(0.5)",
            "Policy library context: active policies and user-scoped exceptions for policy questions",
            &[
                QueryParam { name: "user".to_string(), default: None },
                QueryParam { name: "query".to_string(), default: None },
            ],
        );

        // ── 18. RFP Questionnaire Context ──
        // Harness: RFP Questionnaire — existing QA pairs and compliance
        // controls for questionnaire answering.
        self.insert_builtin(
            "RFP Questionnaire Context",
            "ASSEMBLE \"RFP Context\" FROM\n\
             \x20 qa_pairs:   (RECALL facts ABOUT $query\n\
             \x20              WHERE relation = \"answered-by\" RECENT 15),\n\
             \x20 controls:   (RECALL states ABOUT $query\n\
             \x20              WHERE tags CONTAINS \"control\" RECENT 10)\n\
             BUDGET 4000 tokens\n\
             FORMAT markdown\n\
             WITH dedup, provenance, rerank, score_breakdown, recency_weight(0.6), min_score(0.45)",
            "RFP questionnaire context: existing QA pairs and compliance controls for questionnaire answering",
            &[
                QueryParam { name: "query".to_string(), default: None },
            ],
        );
    }

    /// Apply persisted timestamps to an existing entry (including built-ins).
    /// Used on rehydration to restore `last_run_at` from Fjall for built-in
    /// queries that were previously run.
    pub fn apply_timestamps(
        &mut self,
        name: &str,
        last_run_at: Option<u64>,
        updated_at: Option<u64>,
    ) {
        if let Some(entry) = self.queries.get_mut(name) {
            if last_run_at.is_some() {
                entry.last_run_at = last_run_at;
            }
            if updated_at.is_some() && entry.updated_at.is_none() {
                entry.updated_at = updated_at;
            }
        }
    }

    /// Record the current time as the last execution timestamp for a saved query.
    /// Returns `Err` if the query does not exist.
    pub fn update_last_run(&mut self, name: &str) -> Result<(), String> {
        let entry = self
            .queries
            .get_mut(name)
            .ok_or_else(|| format!("saved query \"{name}\" not found"))?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        entry.last_run_at = Some(now);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Number of built-in queries loaded by `load_builtins()`.
    const BUILTIN_COUNT: usize = 18;

    /// All built-in query names.
    const BUILTIN_NAMES: [&str; BUILTIN_COUNT] = [
        "Customer Support Context",
        "Conversation Resume",
        "User Profile Briefing",
        "Knowledge Researcher",
        "Task Executor",
        "Meeting Debrief",
        "Recommendation Context",
        "Compliance Audit",
        "Creative Collaborator",
        "Scoped Namespace Context",
        "agent_context",
        // --- New enterprise template queries ---
        "Account 360 Context",
        "Meeting Context",
        "Meeting Cross Query",
        "Support CoPilot Context",
        "Sales Playbook Context",
        "Policy Library Context",
        "RFP Questionnaire Context",
    ];

    #[test]
    fn test_valid_names() {
        assert!(QueryRegistry::is_valid_name("abc"));
        assert!(QueryRegistry::is_valid_name("a_b_c"));
        assert!(QueryRegistry::is_valid_name("query123"));
        assert!(QueryRegistry::is_valid_name("ABC"));
        assert!(QueryRegistry::is_valid_name("a-b-c"));
        assert!(QueryRegistry::is_valid_name("Customer Support Context"));
        assert!(QueryRegistry::is_valid_name("My Query-1"));
        assert!(!QueryRegistry::is_valid_name(""));
        assert!(!QueryRegistry::is_valid_name("123abc"));
        assert!(!QueryRegistry::is_valid_name("_starts_with_underscore"));
        assert!(!QueryRegistry::is_valid_name("-starts-with-hyphen"));
        assert!(!QueryRegistry::is_valid_name("trailing space "));
        assert!(!QueryRegistry::is_valid_name(" leading space"));
        assert!(!QueryRegistry::is_valid_name("double  space"));
    }

    #[test]
    fn test_agent_namespaced_names() {
        // Permitted agent-namespaced shape.
        assert!(QueryRegistry::is_valid_name("agent/support/ctx"));
        assert!(QueryRegistry::is_valid_name("agent/bot-v2/ctx"));
        assert!(QueryRegistry::is_valid_name("agent/support/tool_summary"));

        // Reject anything that doesn't fit the narrow shape.
        assert!(!QueryRegistry::is_valid_name("agent/")); // missing slug + suffix
        assert!(!QueryRegistry::is_valid_name("agent/support")); // missing suffix
        assert!(!QueryRegistry::is_valid_name("agent/support/")); // empty suffix
        assert!(!QueryRegistry::is_valid_name("agent/SUPPORT/ctx")); // uppercase slug
        assert!(!QueryRegistry::is_valid_name("agent/-bad/ctx")); // bad slug
        assert!(!QueryRegistry::is_valid_name("agent/support/CTX")); // uppercase suffix
        assert!(!QueryRegistry::is_valid_name("agents/support/ctx")); // wrong prefix
        assert!(!QueryRegistry::is_valid_name("agent/support/ctx/extra")); // too many segments
        assert!(!QueryRegistry::is_valid_name(&format!(
            "agent/support/{}",
            "x".repeat(17)
        ))); // suffix too long
    }

    #[test]
    fn test_agent_context_builtin_present() {
        let reg = QueryRegistry::new();
        let agent_ctx = reg.get("agent_context").expect("agent_context builtin");
        assert!(agent_ctx.builtin);
        let names: Vec<_> = agent_ctx.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["user", "def_ns", "query", "session_id"]);
    }

    #[test]
    fn test_builtins_loaded() {
        let reg = QueryRegistry::new();
        assert_eq!(reg.len(), BUILTIN_COUNT);
        for name in BUILTIN_NAMES {
            let entry = reg.get(name).expect(name);
            assert!(entry.builtin);
            assert!(!entry.body.is_empty());
        }
    }

    #[test]
    fn test_builtin_has_params() {
        let reg = QueryRegistry::new();
        let support = reg.get("Customer Support Context").unwrap();
        assert_eq!(support.params.len(), 2);
        assert_eq!(support.params[0].name, "user_id");
        assert!(support.params[0].default.is_none()); // required
        assert_eq!(support.params[1].name, "topic");

        let executor = reg.get("Task Executor").unwrap();
        assert_eq!(executor.params.len(), 1);
        assert_eq!(executor.params[0].name, "user_id");
    }

    #[test]
    fn test_cannot_delete_builtin() {
        let mut reg = QueryRegistry::new();
        let err = reg.delete("Task Executor").unwrap_err();
        assert!(err.contains("cannot delete built-in"));
    }

    #[test]
    fn test_cannot_overwrite_builtin() {
        let mut reg = QueryRegistry::new();
        let err = reg
            .register("Task Executor", "RECALL facts", "", &[])
            .unwrap_err();
        assert!(err.contains("already exists"));
    }

    #[test]
    fn test_is_builtin() {
        let mut reg = QueryRegistry::new();
        assert!(reg.is_builtin("Task Executor"));
        assert!(!reg.is_builtin("nonexistent"));
        reg.register("custom", "RECALL facts", "", &[]).unwrap();
        assert!(!reg.is_builtin("custom"));
    }

    #[test]
    fn test_register_and_get() {
        let mut reg = QueryRegistry::new();
        reg.register("test_query", "RECALL facts RECENT 10", "A test", &[])
            .unwrap();
        let entry = reg.get("test_query").unwrap();
        assert_eq!(entry.body, "RECALL facts RECENT 10");
        assert_eq!(entry.description, "A test");
        assert!(!entry.builtin);
    }

    #[test]
    fn test_duplicate_name_upserts() {
        // User-defined queries are upserted (second DEFINE QUERY overwrites the first).
        let mut reg = QueryRegistry::new();
        reg.register("q", "RECALL facts", "", &[]).unwrap();
        reg.register("q", "RECALL events", "", &[]).unwrap(); // must not error
        let entry = reg.get("q").unwrap();
        assert_eq!(entry.body, "RECALL events"); // second definition wins
    }

    #[test]
    fn test_delete() {
        let mut reg = QueryRegistry::new();
        reg.register("q", "RECALL facts", "", &[]).unwrap();
        reg.delete("q").unwrap();
        assert!(reg.get("q").is_none());
    }

    #[test]
    fn test_delete_not_found() {
        let reg = QueryRegistry::new();
        // All entries are builtins, try deleting a nonexistent name.
        let mut reg = reg;
        assert!(reg.delete("nonexistent").is_err());
    }

    #[test]
    fn test_body_too_large() {
        let mut reg = QueryRegistry::new();
        let body = "x".repeat(MAX_QUERY_BODY_SIZE + 1);
        assert!(reg.register("q", &body, "", &[]).is_err());
    }

    #[test]
    fn test_too_many_params() {
        let mut reg = QueryRegistry::new();
        let params: Vec<QueryParam> = (0..11)
            .map(|i| QueryParam {
                name: format!("p{i}"),
                default: None,
            })
            .collect();
        assert!(reg.register("q", "RECALL facts", "", &params).is_err());
    }

    #[test]
    fn test_max_queries_limit() {
        let mut reg = QueryRegistry::new();
        // Fill up to limit (builtins already count).
        for i in 0..(MAX_QUERIES_PER_NAMESPACE - BUILTIN_COUNT) {
            let name = format!("q{:03}", i);
            reg.register(&name, "RECALL facts", "", &[]).unwrap();
        }
        assert!(reg.register("overflow", "RECALL facts", "", &[]).is_err());
    }

    #[test]
    fn test_list_sorted_includes_builtins() {
        let mut reg = QueryRegistry::new();
        reg.register("zebra", "RECALL facts", "", &[]).unwrap();
        reg.register("alpha", "RECALL events", "desc", &[]).unwrap();
        let list = reg.list();
        // All entries (builtins + custom) present, sorted by updated_at DESC
        // (most-recent first) — see `QueryRegistry::list`.
        assert_eq!(list.len(), BUILTIN_COUNT + 2);
        let ts: Vec<u64> = list.iter().map(|e| e.updated_at.unwrap_or(0)).collect();
        assert!(
            ts.windows(2).all(|w| w[0] >= w[1]),
            "list is not sorted by updated_at DESC: {:?}",
            ts
        );
        // Builtins have builtin=true, customs have builtin=false.
        let builtin_count = list.iter().filter(|e| e.builtin).count();
        assert_eq!(builtin_count, BUILTIN_COUNT);
    }

    #[test]
    fn test_all_builtins_parse() {
        use crate::parser::parse;
        let reg = QueryRegistry::new();
        for name in BUILTIN_NAMES {
            let entry = reg.get(name).unwrap();
            // Substitute all $param references with dummy string values
            // (the RUN executor does the same substitution before parsing).
            let mut body = entry.body.clone();
            for param in &entry.params {
                body = body.replace(
                    &format!("${}", param.name),
                    &format!("\"test_{}\"", param.name),
                );
            }
            let result = parse(&body);
            assert!(
                result.is_ok(),
                "built-in query \"{name}\" failed to parse: {:?}\n--- body ---\n{}",
                result.err(),
                body,
            );
        }
    }

    #[test]
    fn test_new_builtin_params() {
        let reg = QueryRegistry::new();

        // Customer Support Context: $user_id, $topic (required)
        let q = reg.get("Customer Support Context").unwrap();
        assert_eq!(q.params.len(), 2);
        assert_eq!(q.params[0].name, "user_id");
        assert_eq!(q.params[1].name, "topic");

        // Conversation Resume: $user_id, $session_id (both required)
        let q = reg.get("Conversation Resume").unwrap();
        assert_eq!(q.params.len(), 2);
        assert_eq!(q.params[0].name, "user_id");
        assert_eq!(q.params[1].name, "session_id");

        // Task Executor: $user_id (required)
        let q = reg.get("Task Executor").unwrap();
        assert_eq!(q.params.len(), 1);
        assert_eq!(q.params[0].name, "user_id");

        // Meeting Debrief: $user_id, $session_id (both required)
        let q = reg.get("Meeting Debrief").unwrap();
        assert_eq!(q.params.len(), 2);
        assert_eq!(q.params[0].name, "user_id");
        assert_eq!(q.params[1].name, "session_id");

        // Scoped Namespace Context: $namespace (required)
        let q = reg.get("Scoped Namespace Context").unwrap();
        assert_eq!(q.params.len(), 1);
        assert_eq!(q.params[0].name, "namespace");
    }
}
