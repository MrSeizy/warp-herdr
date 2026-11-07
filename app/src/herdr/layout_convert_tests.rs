use super::{HerdrPaneTree, MAX_SPLIT_FRACTION, MIN_SPLIT_FRACTION, warp_axis};
use crate::herdr::protocol::{AgentStatus, HerdrSplitDirection, LayoutNode};
use crate::pane_group::tree::SplitDirection;

fn leaf(id: &str) -> LayoutNode {
    LayoutNode::Pane {
        pane_id: Some(id.to_string()),
        cwd: None,
    }
}

#[test]
fn converts_single_pane() {
    let tree = HerdrPaneTree::from_layout(&leaf("w1:p1")).unwrap();
    assert_eq!(
        tree,
        HerdrPaneTree::Leaf {
            pane_id: "w1:p1".into(),
            cwd: None,
            agent: None,
            agent_status: AgentStatus::Unknown,
        }
    );
    assert_eq!(tree.pane_ids(), vec!["w1:p1"]);
}

#[test]
fn converts_nested_splits_with_ratio_as_first_fraction() {
    let node = LayoutNode::Split {
        direction: HerdrSplitDirection::Right,
        ratio: 0.65,
        first: Box::new(leaf("w1:p1")),
        second: Box::new(LayoutNode::Split {
            direction: HerdrSplitDirection::Down,
            ratio: 0.5,
            first: Box::new(leaf("w1:p2")),
            second: Box::new(leaf("w1:p3")),
        }),
    };
    let tree = HerdrPaneTree::from_layout(&node).unwrap();
    match &tree {
        HerdrPaneTree::Split {
            direction,
            children,
        } => {
            assert_eq!(*direction, HerdrSplitDirection::Right);
            assert!((children[0].0 - 0.65).abs() < 1e-6);
            assert!((children[1].0 - 0.35).abs() < 1e-6);
            assert!(matches!(children[1].1, HerdrPaneTree::Split { .. }));
        }
        other => panic!("unexpected: {other:?}"),
    }
    assert_eq!(tree.pane_ids(), vec!["w1:p1", "w1:p2", "w1:p3"]);
}

#[test]
fn rejects_pane_without_id() {
    assert!(
        HerdrPaneTree::from_layout(&LayoutNode::Pane {
            pane_id: None,
            cwd: None,
        })
        .is_err()
    );
}

#[test]
fn rejects_duplicate_pane_ids() {
    let node = LayoutNode::Split {
        direction: HerdrSplitDirection::Right,
        ratio: 0.5,
        first: Box::new(leaf("w1:p1")),
        second: Box::new(leaf("w1:p1")),
    };
    assert!(HerdrPaneTree::from_layout(&node).is_err());
}

#[test]
fn clamps_degenerate_ratios() {
    let node = LayoutNode::Split {
        direction: HerdrSplitDirection::Down,
        ratio: 0.0,
        first: Box::new(leaf("w1:p1")),
        second: Box::new(leaf("w1:p2")),
    };
    let tree = HerdrPaneTree::from_layout(&node).unwrap();
    match &tree {
        HerdrPaneTree::Split { children, .. } => {
            assert!((children[0].0 - MIN_SPLIT_FRACTION).abs() < 1e-6);
            assert!((children[1].0 - MAX_SPLIT_FRACTION).abs() < 1e-6);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn maps_herdr_directions_to_warp_axes() {
    assert_eq!(
        warp_axis(HerdrSplitDirection::Right),
        SplitDirection::Horizontal
    );
    assert_eq!(
        warp_axis(HerdrSplitDirection::Down),
        SplitDirection::Vertical
    );
}

#[test]
fn clamps_ratio_upper_bound() {
    let node = LayoutNode::Split {
        direction: HerdrSplitDirection::Right,
        ratio: 1.0,
        first: Box::new(leaf("w1:p1")),
        second: Box::new(leaf("w1:p2")),
    };
    let tree = HerdrPaneTree::from_layout(&node).unwrap();
    match &tree {
        HerdrPaneTree::Split { children, .. } => {
            assert!((children[0].0 - MAX_SPLIT_FRACTION).abs() < 1e-6);
            assert!((children[1].0 - MIN_SPLIT_FRACTION).abs() < 1e-6);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// enrich_agents fills only leaves that lack an agent (ones herdr already
/// annotated are never overwritten) and stamps each leaf's agent status so
/// pane materialization can seed the mirrored session's initial status.
#[test]
fn enrich_agents_fills_only_missing_leaves() {
    let mut tree = HerdrPaneTree::Split {
        direction: HerdrSplitDirection::Right,
        children: Box::new([
            (
                0.5,
                HerdrPaneTree::Leaf {
                    pane_id: "w1:p1".into(),
                    cwd: None,
                    agent: Some("codex".into()),
                    agent_status: AgentStatus::Working,
                },
            ),
            (
                0.5,
                HerdrPaneTree::Leaf {
                    pane_id: "w1:p2".into(),
                    cwd: None,
                    agent: None,
                    agent_status: AgentStatus::Unknown,
                },
            ),
        ]),
    };
    tree.enrich_agents(&|pane_id| {
        (pane_id == "w1:p2")
            .then(|| ("claude".to_string(), AgentStatus::Done))
            .map(|(agent, status)| (Some(agent), status))
            .unwrap_or((None, AgentStatus::Unknown))
    });
    let HerdrPaneTree::Split { children, .. } = &tree else {
        panic!("unexpected: {tree:?}");
    };
    let HerdrPaneTree::Leaf {
        agent: first,
        agent_status: first_status,
        ..
    } = &children[0].1
    else {
        panic!("expected leaf");
    };
    let HerdrPaneTree::Leaf {
        agent: second,
        agent_status: second_status,
        ..
    } = &children[1].1
    else {
        panic!("expected leaf");
    };
    assert_eq!(first.as_deref(), Some("codex"));
    // The lookup missed w1:p1: its herdr-annotated agent and status stay.
    assert_eq!(*first_status, AgentStatus::Working);
    assert_eq!(second.as_deref(), Some("claude"));
    assert_eq!(*second_status, AgentStatus::Done);
}
