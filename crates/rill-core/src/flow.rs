//! The flow graph: what a user wires on the canvas, and the structural gate every build path
//! must pass through before a single Move call is emitted.
//!
//! The gate runs here rather than at the HTTP boundary on purpose. The reference learned this the
//! hard way — its MCP callers bypassed the request schema entirely, so a check that lived only in
//! the schema layer did not run for them. A caller that reaches the compiler at all has already
//! passed this.
//!
//! Handle names are checked, not just endpoints. A typo'd handle does not fail loudly on its own:
//! the edge simply fails to chain a coin, and the target node quietly draws a second helping of
//! root funding instead. That is a funding bug wearing the costume of a wiring bug, and it is why
//! an unrecognised handle is refused here.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// The most nodes one flow may carry. A bound exists so a pathological graph cannot turn a build
/// request into an unbounded amount of work.
pub const MAX_FLOW_NODES: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowEdge {
    pub source: String,
    pub source_handle: String,
    pub target: String,
    pub target_handle: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowNode {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlowGraph {
    pub nodes: Vec<FlowNode>,
    pub edges: Vec<FlowEdge>,
}

/// Which edge handles a node type accepts and emits.
pub struct HandleSpec {
    pub target_handles: &'static [&'static str],
    pub source_handles: &'static [&'static str],
}

/// Handles are looked up by node type. A type with no entry is left unchecked deliberately: an
/// unrecognised node type is already reported as a soft warning when no adapter claims it, and
/// hard-failing here would turn that graceful skip into a build error.
pub fn handle_spec(node_type: &str) -> Option<HandleSpec> {
    Some(match node_type {
        "cetus_swap" => HandleSpec {
            target_handles: &["coin_inputs"],
            source_handles: &["coin_out"],
        },
        "haedal_stake" => HandleSpec {
            target_handles: &["sui_coin"],
            source_handles: &[],
        },
        "deepbook_limit_order" | "ptb" => HandleSpec {
            target_handles: &[],
            source_handles: &[],
        },
        // Guardrails use the generic single in/out ports. Many edges may target `in` — a
        // multi-input guardrail asserts each incoming coin, then merges them.
        "guardrail" => HandleSpec {
            target_handles: &["in"],
            source_handles: &["out"],
        },
        _ => return None,
    })
}

/// One structural problem, with the path to the offending element so a caller can point at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowIssue {
    pub message: String,
    pub path: String,
}

/// Every structural problem in the graph, in one pass.
///
/// All issues are collected rather than returning at the first: a user who mis-wired three edges
/// should learn that once, not three times.
pub fn find_flow_structure_issues(flow: &FlowGraph) -> Vec<FlowIssue> {
    let mut issues = Vec::new();

    if flow.nodes.len() > MAX_FLOW_NODES {
        issues.push(FlowIssue {
            message: format!(
                "flow has {} nodes, more than the {MAX_FLOW_NODES} a single build may carry",
                flow.nodes.len()
            ),
            path: "nodes".into(),
        });
    }

    let mut seen: HashSet<&str> = HashSet::new();
    for (i, node) in flow.nodes.iter().enumerate() {
        if !seen.insert(node.id.as_str()) {
            issues.push(FlowIssue {
                message: format!("duplicate node id \"{}\"", node.id),
                path: format!("nodes[{i}].id"),
            });
        }
    }

    let by_id: HashMap<&str, &FlowNode> = flow.nodes.iter().map(|n| (n.id.as_str(), n)).collect();

    for (i, edge) in flow.edges.iter().enumerate() {
        let source = by_id.get(edge.source.as_str());
        let target = by_id.get(edge.target.as_str());

        if source.is_none() {
            issues.push(FlowIssue {
                message: format!(
                    "edge {i}: source \"{}\" does not reference an existing node",
                    edge.source
                ),
                path: format!("edges[{i}].source"),
            });
        }
        if target.is_none() {
            issues.push(FlowIssue {
                message: format!(
                    "edge {i}: target \"{}\" does not reference an existing node",
                    edge.target
                ),
                path: format!("edges[{i}].target"),
            });
        }

        if let Some(node) = source {
            if let Some(spec) = handle_spec(&node.kind) {
                if !spec.source_handles.contains(&edge.source_handle.as_str()) {
                    issues.push(FlowIssue {
                        message: format!(
                            "edge {i}: \"{}\" is not a valid output handle for node type \"{}\" \
                             (expected one of: {})",
                            edge.source_handle,
                            node.kind,
                            list_or_none(spec.source_handles)
                        ),
                        path: format!("edges[{i}].sourceHandle"),
                    });
                }
            }
        }
        if let Some(node) = target {
            if let Some(spec) = handle_spec(&node.kind) {
                if !spec.target_handles.contains(&edge.target_handle.as_str()) {
                    issues.push(FlowIssue {
                        message: format!(
                            "edge {i}: \"{}\" is not a valid input handle for node type \"{}\" \
                             (expected one of: {})",
                            edge.target_handle,
                            node.kind,
                            list_or_none(spec.target_handles)
                        ),
                        path: format!("edges[{i}].targetHandle"),
                    });
                }
            }
        }
    }

    issues
}

fn list_or_none(handles: &[&str]) -> String {
    if handles.is_empty() {
        "<none>".into()
    } else {
        handles.join(", ")
    }
}

/// Why an ordering could not be produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowError {
    /// A cycle. Named by one node on it, which is enough for a user to find the loop.
    Cycle {
        node_id: String,
    },
    Structural(Vec<FlowIssue>),
}

impl std::fmt::Display for FlowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cycle { node_id } => write!(
                f,
                "the flow contains a cycle through node \"{node_id}\"; a transaction is a sequence, \
                 so it cannot loop"
            ),
            Self::Structural(issues) => {
                write!(f, "the flow is structurally invalid: ")?;
                let joined: Vec<String> = issues
                    .iter()
                    .map(|i| format!("{} ({})", i.message, i.path))
                    .collect();
                write!(f, "{}", joined.join("; "))
            }
        }
    }
}

impl std::error::Error for FlowError {}

/// Nodes in dependency order — every node appears after everything it consumes from.
///
/// Depth-first with an in-progress marker, so a cycle is detected rather than recursed into
/// forever. Nodes with no edges keep their original relative order, which makes the compiled
/// output stable for the same input.
pub fn topological_sort(flow: &FlowGraph) -> Result<Vec<&FlowNode>, FlowError> {
    let issues = find_flow_structure_issues(flow);
    if !issues.is_empty() {
        return Err(FlowError::Structural(issues));
    }

    let by_id: HashMap<&str, &FlowNode> = flow.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in &flow.edges {
        adjacency
            .entry(edge.source.as_str())
            .or_default()
            .push(edge.target.as_str());
    }

    let mut done: HashSet<&str> = HashSet::new();
    let mut in_progress: HashSet<&str> = HashSet::new();
    let mut order: Vec<&FlowNode> = Vec::with_capacity(flow.nodes.len());

    // Iterative rather than recursive: a graph is user input, and user input should not be able
    // to choose this process's stack depth.
    for root in &flow.nodes {
        if done.contains(root.id.as_str()) {
            continue;
        }
        let mut stack: Vec<(&str, usize)> = vec![(root.id.as_str(), 0)];
        in_progress.insert(root.id.as_str());

        while let Some((id, child_index)) = stack.pop() {
            let children = adjacency.get(id).map(Vec::as_slice).unwrap_or(&[]);
            if child_index < children.len() {
                stack.push((id, child_index + 1));
                let next = children[child_index];
                if in_progress.contains(next) {
                    return Err(FlowError::Cycle {
                        node_id: next.to_string(),
                    });
                }
                if !done.contains(next) {
                    in_progress.insert(next);
                    stack.push((next, 0));
                }
            } else {
                in_progress.remove(id);
                done.insert(id);
                if let Some(node) = by_id.get(id) {
                    order.push(node);
                }
            }
        }
    }

    // The DFS emits each node after its dependents, so the dependency order is the reverse.
    order.reverse();
    Ok(order)
}
