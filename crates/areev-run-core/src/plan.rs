//! The validated plan graph — load-time validation V1–V6's pure parts
//! (governed-agents §6.1) over an OMS §8.4 Workflow grain, plus the derived
//! structure the scheduler walks: indexed nodes and edges, adjacency in
//! declaration order, and the Tarjan condensation V5 needs.
//!
//! Store-dependent validation stays driver-side: V3 (binding hashes resolve
//! to Tool Definitions), V7 (resolution freeze into the manifest), V8 (price
//! table coverage). Everything here is a pure function of the Workflow's
//! content — a foreign OMS file gets exactly the same verdict on every host.

use crate::cond::{self, Cond};
use crate::error::{Result, RunError};
use areev_core::types::Workflow;
use std::collections::BTreeMap;

/// One edge, indexed and with its condition pre-parsed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PlanEdge {
    pub src: usize,
    pub dst: usize,
    /// The raw condition string (journaled in decision records) and its
    /// parsed built-in form. Hosts installing their own evaluator get the
    /// raw string; the built-in gets the parse.
    pub cond_raw: Option<String>,
    pub cond: Option<Cond>,
    pub max_cycles: Option<u32>,
}

/// A validated, indexed plan. Immutable once built — the runtime never
/// mutates a plan, only its own scheduler state.
#[derive(Debug, Clone)]
pub struct PlanGraph {
    pub nodes: Vec<String>,
    pub edges: Vec<PlanEdge>,
    /// Per node: out-edge indices in **declaration order** — the canonical
    /// evaluation order (§6.2).
    pub out_edges: Vec<Vec<usize>>,
    pub in_edges: Vec<Vec<usize>>,
    /// Per node: SCC id (condensation vertex).
    pub scc_of: Vec<usize>,
    /// Per node: re-attempt budget (`retries[n] = n` re-attempts, so up to
    /// n+1 executions — §6.3's pinned reading of the field).
    pub retries: Vec<u32>,
    /// Per node: the raw binding string from the Workflow (a Tool Definition
    /// hash for Bound nodes). Resolution to grains is the driver's V3/V7.
    pub bindings: Vec<Option<String>>,
}

impl PlanGraph {
    /// Build and validate. Every violation is an error, never a warning —
    /// V1/V2 shape errors surface as `RUN-E019`, unbounded cycles as
    /// `RUN-E002`, unreachable nodes as `RUN-E003`, bad conditions as
    /// `RUN-E005`.
    pub fn build(wf: &Workflow) -> Result<Self> {
        // V1 — nodes: non-empty list, unique ids, sane bytes.
        if wf.nodes.is_empty() {
            return Err(RunError::InvalidPlan { why: "workflow has no nodes".into() });
        }
        let mut index: BTreeMap<&str, usize> = BTreeMap::new();
        for (i, n) in wf.nodes.iter().enumerate() {
            if n.trim().is_empty() || n.len() > 256 || n.chars().any(|c| c.is_control()) {
                return Err(RunError::InvalidPlan {
                    why: format!("node id {n:?} is empty, oversized, or has control chars"),
                });
            }
            if index.insert(n.as_str(), i).is_some() {
                return Err(RunError::InvalidPlan { why: format!("duplicate node id '{n}'") });
            }
        }

        // V2 — edge referential integrity (re-validated here even though the
        // local ADD path checks it: OMS files arrive from other
        // implementations). V4 — conditions parse.
        let mut edges = Vec::with_capacity(wf.edges.len());
        for e in &wf.edges {
            let (Some(&src), Some(&dst)) = (index.get(e.src.as_str()), index.get(e.dst.as_str()))
            else {
                return Err(RunError::InvalidPlan {
                    why: format!("edge {} -> {} references an unknown node", e.src, e.dst),
                });
            };
            let parsed = match &e.cond {
                Some(raw) => Some(cond::parse(raw).map_err(|why| RunError::InvalidCondition {
                    edge: format!("{} -> {}", e.src, e.dst),
                    why,
                })?),
                None => None,
            };
            edges.push(PlanEdge {
                src,
                dst,
                cond_raw: e.cond.clone(),
                cond: parsed,
                max_cycles: e.max_cycles,
            });
        }

        // Shape checks on the auxiliary maps (the pure half of V3).
        for key in wf.bindings.keys() {
            if !index.contains_key(key.as_str()) {
                return Err(RunError::InvalidPlan {
                    why: format!("bindings names unknown node '{key}'"),
                });
            }
        }
        for key in wf.retries.keys() {
            if !index.contains_key(key.as_str()) {
                return Err(RunError::InvalidPlan {
                    why: format!("retries names unknown node '{key}'"),
                });
            }
        }

        let n = wf.nodes.len();
        let mut out_edges = vec![Vec::new(); n];
        let mut in_edges = vec![Vec::new(); n];
        for (i, e) in edges.iter().enumerate() {
            out_edges[e.src].push(i);
            in_edges[e.dst].push(i);
        }

        // V6 — reachability from the entry (nodes[0], OMS §8.4). Unreachable
        // nodes are almost always authoring typos; erroring here catches
        // them before the first journal write.
        let mut reachable = vec![false; n];
        let mut stack = vec![0usize];
        reachable[0] = true;
        while let Some(v) = stack.pop() {
            for &ei in &out_edges[v] {
                let d = edges[ei].dst;
                if !reachable[d] {
                    reachable[d] = true;
                    stack.push(d);
                }
            }
        }
        let unreachable: Vec<String> = (0..n)
            .filter(|&i| !reachable[i])
            .map(|i| wf.nodes[i].clone())
            .collect();
        if !unreachable.is_empty() {
            return Err(RunError::Unreachable { nodes: unreachable });
        }

        // V5 — every cycle carries a bound: for each SCC that contains a
        // cycle (size > 1, or a self-loop), at least one intra-SCC edge must
        // set max_cycles. Tarjan, iterative (plans are small, but a deep
        // chain must not blow the stack).
        let scc_of = tarjan_scc(n, &out_edges, &edges);
        let mut scc_nodes: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (v, &c) in scc_of.iter().enumerate() {
            scc_nodes.entry(c).or_default().push(v);
        }
        for (c, members) in &scc_nodes {
            let cyclic = members.len() > 1
                || members.iter().any(|&v| {
                    out_edges[v].iter().any(|&ei| edges[ei].dst == v)
                });
            if !cyclic {
                continue;
            }
            let bounded = edges.iter().any(|e| {
                scc_of[e.src] == *c && scc_of[e.dst] == *c && e.max_cycles.is_some()
            });
            if !bounded {
                return Err(RunError::UnboundedCycle {
                    nodes: members.iter().map(|&v| wf.nodes[v].clone()).collect(),
                });
            }
        }

        let retries = wf
            .nodes
            .iter()
            .map(|id| wf.retries.get(id).copied().unwrap_or(0))
            .collect();
        let bindings = wf.nodes.iter().map(|id| wf.bindings.get(id).cloned()).collect();

        Ok(PlanGraph {
            nodes: wf.nodes.clone(),
            edges,
            out_edges,
            in_edges,
            scc_of,
            retries,
            bindings,
        })
    }

    /// Terminal nodes: no out-edges (OMS §8.4).
    pub fn terminals(&self) -> Vec<usize> {
        (0..self.nodes.len())
            .filter(|&v| self.out_edges[v].is_empty())
            .collect()
    }
}

/// Iterative Tarjan SCC. Returns the SCC id per vertex (ids are arbitrary
/// but deterministic for a given graph).
fn tarjan_scc(n: usize, out_edges: &[Vec<usize>], edges: &[PlanEdge]) -> Vec<usize> {
    const UNSET: usize = usize::MAX;
    let mut idx = vec![UNSET; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut scc_of = vec![UNSET; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut next_index = 0usize;
    let mut next_scc = 0usize;

    // Explicit DFS frames: (vertex, next out-edge position).
    for root in 0..n {
        if idx[root] != UNSET {
            continue;
        }
        let mut frames: Vec<(usize, usize)> = vec![(root, 0)];
        idx[root] = next_index;
        low[root] = next_index;
        next_index += 1;
        stack.push(root);
        on_stack[root] = true;

        while let Some(&mut (v, ref mut ei_pos)) = frames.last_mut() {
            if *ei_pos < out_edges[v].len() {
                let ei = out_edges[v][*ei_pos];
                *ei_pos += 1;
                let w = edges[ei].dst;
                if idx[w] == UNSET {
                    idx[w] = next_index;
                    low[w] = next_index;
                    next_index += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    frames.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(idx[w]);
                }
            } else {
                frames.pop();
                if let Some(&mut (parent, _)) = frames.last_mut() {
                    low[parent] = low[parent].min(low[v]);
                }
                if low[v] == idx[v] {
                    loop {
                        let w = stack.pop().expect("tarjan stack invariant");
                        on_stack[w] = false;
                        scc_of[w] = next_scc;
                        if w == v {
                            break;
                        }
                    }
                    next_scc += 1;
                }
            }
        }
    }
    scc_of
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wf(nodes: &[&str]) -> Workflow {
        Workflow::new(nodes.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn a_diamond_validates() {
        let w = wf(&["a", "b", "c", "d"])
            .edge("a", "b")
            .edge("a", "c")
            .edge("b", "d")
            .edge("c", "d");
        let p = PlanGraph::build(&w).unwrap();
        assert_eq!(p.terminals(), vec![3]);
        assert_eq!(p.in_edges[3].len(), 2, "d is an AND-join");
        // Four distinct SCCs — no cycles.
        let distinct: std::collections::BTreeSet<_> = p.scc_of.iter().collect();
        assert_eq!(distinct.len(), 4);
    }

    #[test]
    fn bounded_cycles_pass_unbounded_are_refused() {
        // Multi-node cycle with a bound on the back-edge: fine.
        let mut e = wf(&["a", "b"]).edge("a", "b");
        e.edges.push(areev_core::types::WorkflowEdge {
            src: "b".into(),
            dst: "a".into(),
            cond: None,
            max_cycles: Some(3),
        });
        assert!(PlanGraph::build(&e).is_ok());

        // Same cycle, no bound anywhere: RUN-E002.
        let mut u = wf(&["a", "b"]).edge("a", "b");
        u.edges.push(areev_core::types::WorkflowEdge {
            src: "b".into(),
            dst: "a".into(),
            cond: None,
            max_cycles: None,
        });
        let err = PlanGraph::build(&u).unwrap_err();
        assert_eq!(err.code(), "RUN-E002");

        // A self-loop is a cycle too.
        let mut s = wf(&["a", "z"]).edge("a", "z");
        s.edges.push(areev_core::types::WorkflowEdge {
            src: "a".into(),
            dst: "a".into(),
            cond: None,
            max_cycles: None,
        });
        assert_eq!(PlanGraph::build(&s).unwrap_err().code(), "RUN-E002");
    }

    #[test]
    fn unreachable_nodes_are_refused() {
        let w = wf(&["a", "b", "orphan"]).edge("a", "b");
        let err = PlanGraph::build(&w).unwrap_err();
        assert_eq!(err.code(), "RUN-E003");
        assert!(err.to_string().contains("orphan"));
    }

    #[test]
    fn shape_violations_are_refused_with_reasons() {
        // Duplicate node id.
        assert!(PlanGraph::build(&wf(&["a", "a"])).is_err());
        // Empty node list.
        assert!(PlanGraph::build(&wf(&[])).is_err());
        // Edge to an unknown node.
        let w = wf(&["a"]).edge("a", "ghost");
        assert!(PlanGraph::build(&w).unwrap_err().to_string().contains("ghost"));
        // bindings / retries naming unknown nodes.
        let b = wf(&["a"]).bind("ghost", "deadbeef");
        assert!(PlanGraph::build(&b).unwrap_err().to_string().contains("ghost"));
        let r = wf(&["a"]).retry("ghost", 2);
        assert!(PlanGraph::build(&r).unwrap_err().to_string().contains("ghost"));
    }

    #[test]
    fn bad_conditions_name_their_edge() {
        let w = wf(&["a", "b"]).cond_edge("a", "b", "x === 1");
        let err = PlanGraph::build(&w).unwrap_err();
        assert_eq!(err.code(), "RUN-E005");
        assert!(err.to_string().contains("a -> b"), "{err}");
    }

    #[test]
    fn retries_and_bindings_index_by_node_position() {
        let w = wf(&["a", "b"]).edge("a", "b").retry("b", 4).bind("a", "cafe");
        let p = PlanGraph::build(&w).unwrap();
        assert_eq!(p.retries, vec![0, 4]);
        assert_eq!(p.bindings[0].as_deref(), Some("cafe"));
        assert_eq!(p.bindings[1], None);
    }
}
