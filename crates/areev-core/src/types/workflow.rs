use std::collections::HashMap;

use super::grain::{Grain, GrainCommon, GrainType};

/// Namespace for the OMS §8.4 execution-record relation.
///
/// When an agent executes a workflow node it records a Tool grain carrying a
/// `related_to` link of type `mg:step_action:<node_id>` whose target is the
/// Workflow grain's hash. That links the execution record to the plan **without
/// modifying the immutable Workflow grain** — the plan is content-addressed, so
/// it cannot accumulate run state; the runs point back at it instead.
///
/// Per OMS §15.3 a `related_to` link is an annotation: a conformant store
/// indexes it for retrieval but MUST NOT let it change the target's
/// supersession state.
pub const STEP_ACTION_PREFIX: &str = "mg:step_action:";

/// Build the execution-record relation for a workflow node.
pub fn step_action_relation(node_id: &str) -> String {
    format!("{STEP_ACTION_PREFIX}{node_id}")
}

/// The node id inside a `mg:step_action:<node_id>` relation, if it is one.
///
/// Returns `None` for every other relation, including a bare
/// `"mg:step_action:"` with no node — an execution record has to say which step
/// it executed.
pub fn step_action_node(relation: &str) -> Option<&str> {
    relation
        .strip_prefix(STEP_ACTION_PREFIX)
        .filter(|n| !n.is_empty())
}

/// A directed edge in a workflow graph.
#[derive(Debug, Clone)]
pub struct WorkflowEdge {
    /// Source node ID (must exist in `nodes`).
    pub src: String,
    /// Destination node ID (must exist in `nodes`).
    pub dst: String,
    /// Opaque condition string (absent = unconditional).
    pub cond: Option<String>,
    /// Maximum traversal count for back-edges (absent = unlimited).
    pub max_cycles: Option<u32>,
}

/// A Workflow grain — directed graph of procedural steps.
#[derive(Debug, Clone)]
pub struct Workflow {
    /// Graph node IDs/labels. Each string is both ID and human-readable label.
    pub nodes: Vec<String>,
    /// Directed edges between nodes.
    pub edges: Vec<WorkflowEdge>,
    /// Node ID → Tool definition grain hash.
    pub bindings: HashMap<String, String>,
    /// Node ID → max repeat count on failure.
    pub retries: HashMap<String, u32>,
    /// Activation condition (optional).
    pub trigger: Option<String>,
    pub common: GrainCommon,
}

impl Workflow {
    pub fn new(nodes: Vec<String>) -> Self {
        Workflow {
            nodes,
            edges: Vec::new(),
            bindings: HashMap::new(),
            retries: HashMap::new(),
            trigger: None,
            common: GrainCommon {
                confidence: 1.0,
                ..Default::default()
            },
        }
    }

    pub fn trigger(mut self, trigger: &str) -> Self {
        self.trigger = Some(trigger.to_string());
        self
    }

    pub fn edge(mut self, src: &str, dst: &str) -> Self {
        self.edges.push(WorkflowEdge {
            src: src.to_string(),
            dst: dst.to_string(),
            cond: None,
            max_cycles: None,
        });
        self
    }

    /// A cycle-bounded edge: fires in at most `max_cycles` generations per
    /// run (the loop-authoring primitive — every cycle must carry at least
    /// one such edge to pass plan validation).
    pub fn edge_with_cycles(mut self, src: &str, dst: &str, max_cycles: u32) -> Self {
        self.edges.push(WorkflowEdge {
            src: src.to_string(),
            dst: dst.to_string(),
            cond: None,
            max_cycles: Some(max_cycles),
        });
        self
    }

    pub fn cond_edge(mut self, src: &str, dst: &str, cond: &str) -> Self {
        self.edges.push(WorkflowEdge {
            src: src.to_string(),
            dst: dst.to_string(),
            cond: Some(cond.to_string()),
            max_cycles: None,
        });
        self
    }

    pub fn bind(mut self, node: &str, hash: &str) -> Self {
        self.bindings.insert(node.to_string(), hash.to_string());
        self
    }

    pub fn retry(mut self, node: &str, max: u32) -> Self {
        self.retries.insert(node.to_string(), max);
        self
    }
}

impl Grain for Workflow {
    fn grain_type(&self) -> GrainType {
        GrainType::Workflow
    }

    fn common(&self) -> &GrainCommon {
        &self.common
    }

    fn common_mut(&mut self) -> &mut GrainCommon {
        &mut self.common
    }

    fn text(&self) -> String {
        // For embedding/indexing: trigger + node labels joined
        let mut parts = Vec::new();
        if let Some(ref t) = self.trigger {
            parts.push(t.clone());
        }
        if !self.nodes.is_empty() {
            parts.push(self.nodes.join(" -> "));
        }
        parts.join(" | ")
    }
}
