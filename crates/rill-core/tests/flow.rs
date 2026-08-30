//! FlowGraph structural gate and dependency ordering.

use rill_core::flow::{
    find_flow_structure_issues, topological_sort, FlowEdge, FlowError, FlowGraph, FlowNode,
    MAX_FLOW_NODES,
};

fn node(id: &str, kind: &str) -> FlowNode {
    FlowNode {
        id: id.into(),
        kind: kind.into(),
        config: None,
        inputs: None,
    }
}

fn edge(source: &str, sh: &str, target: &str, th: &str) -> FlowEdge {
    FlowEdge {
        source: source.into(),
        source_handle: sh.into(),
        target: target.into(),
        target_handle: th.into(),
    }
}

/// Cetus swap feeding a Haedal stake — the composed flow the reference supports.
fn swap_then_stake() -> FlowGraph {
    FlowGraph {
        nodes: vec![node("swap", "cetus_swap"), node("stake", "haedal_stake")],
        edges: vec![edge("swap", "coin_out", "stake", "sui_coin")],
    }
}

#[test]
fn a_valid_flow_has_no_issues_and_orders_by_dependency() {
    let flow = swap_then_stake();
    assert!(find_flow_structure_issues(&flow).is_empty());

    let ordered = topological_sort(&flow).expect("should order");
    let ids: Vec<&str> = ordered.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["swap", "stake"],
        "a swap must be built before the stake it funds"
    );
}

#[test]
fn a_cycle_is_reported_not_recursed_into() {
    let flow = FlowGraph {
        nodes: vec![node("a", "cetus_swap"), node("b", "cetus_swap")],
        edges: vec![
            edge("a", "coin_out", "b", "coin_inputs"),
            edge("b", "coin_out", "a", "coin_inputs"),
        ],
    };
    assert!(matches!(
        topological_sort(&flow),
        Err(FlowError::Cycle { .. })
    ));
}

#[test]
fn a_self_loop_is_a_cycle() {
    let flow = FlowGraph {
        nodes: vec![node("a", "cetus_swap")],
        edges: vec![edge("a", "coin_out", "a", "coin_inputs")],
    };
    assert!(matches!(
        topological_sort(&flow),
        Err(FlowError::Cycle { .. })
    ));
}

#[test]
fn a_duplicate_node_id_is_reported() {
    let flow = FlowGraph {
        nodes: vec![node("dup", "cetus_swap"), node("dup", "haedal_stake")],
        edges: vec![],
    };
    let issues = find_flow_structure_issues(&flow);
    assert_eq!(issues.len(), 1);
    assert!(issues[0].message.contains("duplicate node id"));
}

#[test]
fn a_dangling_edge_endpoint_is_reported_on_both_sides() {
    let flow = FlowGraph {
        nodes: vec![node("swap", "cetus_swap")],
        edges: vec![edge("ghost", "coin_out", "swap", "coin_inputs")],
    };
    let issues = find_flow_structure_issues(&flow);
    assert!(issues
        .iter()
        .any(|i| i.message.contains("source \"ghost\"")));
}

/// The handle check is the one that catches a silent funding bug rather than a loud wiring one.
#[test]
fn a_typoed_handle_is_refused() {
    let flow = FlowGraph {
        nodes: vec![node("swap", "cetus_swap"), node("stake", "haedal_stake")],
        // "coin" instead of "sui_coin": the edge would fail to chain, and the stake would draw a
        // second, independent helping of root funding.
        edges: vec![edge("swap", "coin_out", "stake", "coin")],
    };
    let issues = find_flow_structure_issues(&flow);
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert!(issues[0].message.contains("not a valid input handle"));
    assert!(
        issues[0].message.contains("sui_coin"),
        "the error should name what was expected"
    );
}

#[test]
fn an_unregistered_node_type_is_left_unchecked() {
    let flow = FlowGraph {
        nodes: vec![node("x", "some_future_protocol")],
        edges: vec![],
    };
    assert!(
        find_flow_structure_issues(&flow).is_empty(),
        "an unknown node type is a soft warning at adapter lookup, not a hard structural failure"
    );
}

#[test]
fn a_flow_above_the_node_cap_is_refused() {
    let nodes: Vec<FlowNode> = (0..=MAX_FLOW_NODES)
        .map(|i| node(&format!("n{i}"), "ptb"))
        .collect();
    let flow = FlowGraph {
        nodes,
        edges: vec![],
    };
    let issues = find_flow_structure_issues(&flow);
    assert!(issues.iter().any(|i| i.message.contains("more than the")));
}

#[test]
fn every_issue_is_collected_in_one_pass() {
    let flow = FlowGraph {
        nodes: vec![node("dup", "cetus_swap"), node("dup", "cetus_swap")],
        edges: vec![edge("ghost", "coin_out", "dup", "wrong_handle")],
    };
    let issues = find_flow_structure_issues(&flow);
    assert!(
        issues.len() >= 3,
        "a user who mis-wired three things should learn all three at once, got {issues:?}"
    );
}

#[test]
fn ordering_refuses_a_structurally_invalid_flow_before_sorting() {
    let flow = FlowGraph {
        nodes: vec![node("a", "cetus_swap")],
        edges: vec![edge("a", "coin_out", "missing", "coin_inputs")],
    };
    assert!(matches!(
        topological_sort(&flow),
        Err(FlowError::Structural(_))
    ));
}

#[test]
fn independent_nodes_keep_a_stable_order() {
    let flow = FlowGraph {
        nodes: vec![node("a", "ptb"), node("b", "ptb"), node("c", "ptb")],
        edges: vec![],
    };
    let first: Vec<String> = topological_sort(&flow)
        .unwrap()
        .iter()
        .map(|n| n.id.clone())
        .collect();
    for _ in 0..20 {
        let again: Vec<String> = topological_sort(&flow)
            .unwrap()
            .iter()
            .map(|n| n.id.clone())
            .collect();
        assert_eq!(
            first, again,
            "the same graph must compile to the same order every time"
        );
    }
}
