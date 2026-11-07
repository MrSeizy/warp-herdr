//! Neutral representation of a herdr tab's split tree, decoupled from both
//! herdr wire types and Warp GUI types.
//!
//! `LayoutDescription.zoomed` and `LayoutDescription.focused_pane_id` are
//! intentionally ignored in v1: zoom is unsupported, and focus syncs later,
//! once the mirrored panes exist.

use std::collections::HashSet;

use anyhow::anyhow;

use super::protocol::{AgentStatus, HerdrSplitDirection, LayoutNode};
use crate::pane_group::tree::{Direction, SplitDirection};

/// Clamp bounds for a split's first-child fraction, keeping both children
/// visible and the divider grabbable even when herdr reports a degenerate
/// ratio.
const MIN_SPLIT_FRACTION: f32 = 0.05;
const MAX_SPLIT_FRACTION: f32 = 0.95;

/// Maps a herdr split direction onto the Warp split axis it renders on.
pub fn warp_axis(direction: HerdrSplitDirection) -> SplitDirection {
    match direction {
        // herdr "right" = panes side-by-side = the axis Warp's Direction::Right splits on.
        HerdrSplitDirection::Right => Direction::Right.axis(),
        HerdrSplitDirection::Down => Direction::Down.axis(),
    }
}

/// Neutral BSP tree between herdr's wire types and Warp's pane tree: parsed
/// from a herdr [`LayoutNode`], then materialized into Warp panes by the
/// reconciler. Each split stores its ratio as the first child's fraction;
/// the second child gets `1.0 - ratio`.
#[derive(Debug, Clone, PartialEq)]
pub enum HerdrPaneTree {
    Leaf {
        pane_id: String,
        /// The pane's working directory as reported by herdr, if known.
        cwd: Option<String>,
        /// The agent herdr reports running in this pane (e.g. "claude").
        agent: Option<String>,
        /// The pane's agent status as reported by herdr; seeds the mirrored
        /// CLI agent session's initial status at materialization.
        agent_status: AgentStatus,
    },
    Split {
        direction: HerdrSplitDirection,
        /// (flex weight, child); binary — exactly two children, enforced at
        /// the type level. Boxed to break the type's recursion.
        children: Box<[(f32, HerdrPaneTree); 2]>,
    },
}

impl HerdrPaneTree {
    pub fn from_layout(node: &LayoutNode) -> anyhow::Result<Self> {
        let tree = Self::from_layout_node(node)?;
        // The socket is an external trust boundary: reject layouts that reuse
        // a pane_id, which would corrupt the pane lookup maps downstream.
        let mut seen = HashSet::new();
        for pane_id in tree.pane_ids() {
            if !seen.insert(pane_id.clone()) {
                return Err(anyhow!(
                    "herdr layout contains duplicate pane_id {pane_id:?}"
                ));
            }
        }
        Ok(tree)
    }

    fn from_layout_node(node: &LayoutNode) -> anyhow::Result<Self> {
        match node {
            LayoutNode::Pane {
                pane_id: Some(id),
                cwd,
            } => Ok(HerdrPaneTree::Leaf {
                pane_id: id.clone(),
                cwd: cwd.clone(),
                // layout.export doesn't report agents; enriched from snapshot
                // pane info by the reconciler.
                agent: None,
                agent_status: AgentStatus::Unknown,
            }),
            LayoutNode::Pane { pane_id: None, .. } => {
                Err(anyhow!("herdr layout pane without pane_id"))
            }
            LayoutNode::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let clamped = ratio.clamp(MIN_SPLIT_FRACTION, MAX_SPLIT_FRACTION);
                if clamped != *ratio {
                    log::debug!("herdr layout split ratio {ratio} clamped to {clamped}");
                }
                Ok(HerdrPaneTree::Split {
                    direction: *direction,
                    children: Box::new([
                        (clamped, Self::from_layout_node(first)?),
                        (1.0 - clamped, Self::from_layout_node(second)?),
                    ]),
                })
            }
        }
    }

    /// Left-to-right, top-to-bottom pane ids.
    pub fn pane_ids(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.collect_ids(&mut out);
        out
    }

    fn collect_ids(&self, out: &mut Vec<String>) {
        match self {
            HerdrPaneTree::Leaf { pane_id, .. } => out.push(pane_id.clone()),
            HerdrPaneTree::Split { children, .. } => {
                for (_, child) in children.iter() {
                    child.collect_ids(out);
                }
            }
        }
    }

    /// Fills each leaf's agent and agent status from a per-pane lookup
    /// (snapshot pane info). An agent herdr already annotated is never
    /// overwritten; the status fills only from Unknown so a leaf built
    /// directly from pane info keeps its authoritative value.
    pub fn enrich_agents(&mut self, lookup: &dyn Fn(&str) -> (Option<String>, AgentStatus)) {
        match self {
            HerdrPaneTree::Leaf {
                pane_id,
                agent,
                agent_status,
                ..
            } => {
                let (looked_up_agent, looked_up_status) = lookup(pane_id);
                if agent.is_none() {
                    *agent = looked_up_agent;
                }
                if *agent_status == AgentStatus::Unknown {
                    *agent_status = looked_up_status;
                }
            }
            HerdrPaneTree::Split { children, .. } => {
                for (_, child) in children.iter_mut() {
                    child.enrich_agents(lookup);
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "layout_convert_tests.rs"]
mod tests;
