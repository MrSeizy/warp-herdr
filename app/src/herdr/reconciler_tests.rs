use std::time::Duration;

use instant::Instant;

use super::*;
use crate::herdr::protocol::*;

#[derive(Default)]
struct RecordingOps {
    calls: Vec<String>,
}

impl WorkspaceOps for RecordingOps {
    fn create_workspace_group(&mut self, ws: &WorkspaceInfo) {
        self.calls
            .push(format!("group+ {} {}", ws.workspace_id, ws.label));
    }
    fn create_tab(&mut self, tab: &TabInfo, layout: &HerdrPaneTree) {
        self.calls
            .push(format!("tab+ {} panes={:?}", tab.tab_id, layout.pane_ids()));
    }
    fn close_tab(&mut self, tab_id: &str) {
        self.calls.push(format!("tab- {tab_id}"));
    }
    fn rename_tab(&mut self, tab_id: &str, label: &str) {
        self.calls.push(format!("tab~ {tab_id} {label}"));
    }
    fn apply_tab_order(&mut self, workspace_id: &str, ordered_tab_ids: &[String]) {
        self.calls.push(format!(
            "order {workspace_id} {}",
            ordered_tab_ids.join(",")
        ));
    }
    fn set_tab_status(&mut self, tab_id: &str, status: AgentStatus) {
        self.calls.push(format!("status {tab_id} {status:?}"));
    }
    fn add_split(&mut self, tab_id: &str, pane: &PaneInfo) {
        self.calls
            .push(format!("pane+ {} in {tab_id}", pane.pane_id));
    }
    fn sync_tab_layout(&mut self, tab_id: &str, layout: &HerdrPaneTree) {
        self.calls
            .push(format!("sync {tab_id} panes={:?}", layout.pane_ids()));
    }
    fn close_pane(&mut self, pane_id: &str) {
        self.calls.push(format!("pane- {pane_id}"));
    }
    fn set_pane_title(&mut self, pane_id: &str, title: &str) {
        self.calls.push(format!("title {pane_id} {title}"));
    }
    fn set_pane_label(&mut self, pane_id: &str, label: Option<&str>) {
        self.calls
            .push(format!("label {pane_id} {}", label.unwrap_or("-")));
    }
    fn set_pane_agent(&mut self, pane_id: &str, agent: Option<&str>) {
        self.calls
            .push(format!("agent {pane_id} {}", agent.unwrap_or("-")));
    }
    fn set_pane_agent_status(&mut self, pane_id: &str, status: AgentStatus) {
        self.calls.push(format!("pane_status {pane_id} {status:?}"));
    }
    fn request_layout_refresh(&mut self, tab_id: &str) {
        self.calls.push(format!("relayout {tab_id}"));
    }
}

fn tab_info(tab_id: &str, ws: &str, label: &str) -> TabInfo {
    TabInfo {
        tab_id: tab_id.into(),
        workspace_id: ws.into(),
        number: 1,
        label: label.into(),
        focused: false,
        pane_count: 1,
        agent_status: AgentStatus::Idle,
    }
}

fn pane_info(pane_id: &str, ws: &str, tab: &str) -> PaneInfo {
    PaneInfo {
        pane_id: pane_id.into(),
        terminal_id: format!("term_{pane_id}"),
        workspace_id: ws.into(),
        tab_id: tab.into(),
        focused: false,
        cwd: None,
        label: None,
        title: None,
        terminal_title: None,
        display_agent: None,
        agent: None,
        agent_status: AgentStatus::Idle,
        revision: 1,
    }
}

fn workspace_info() -> WorkspaceInfo {
    WorkspaceInfo {
        workspace_id: "w1".into(),
        number: 1,
        label: "proj".into(),
        focused: true,
        active_tab_id: "w1:t1".into(),
        agent_status: AgentStatus::Idle,
    }
}

fn one_tab_snapshot() -> (SessionSnapshot, Vec<(String, HerdrPaneTree)>) {
    let snapshot = SessionSnapshot {
        version: "0.7.4".into(),
        protocol: 17,
        focused_workspace_id: Some("w1".into()),
        focused_tab_id: Some("w1:t1".into()),
        focused_pane_id: Some("w1:p1".into()),
        workspaces: vec![workspace_info()],
        tabs: vec![tab_info("w1:t1", "w1", "agents")],
        panes: vec![pane_info("w1:p1", "w1", "w1:t1")],
    };
    let layouts = vec![(
        "w1:t1".to_string(),
        HerdrPaneTree::Leaf {
            pane_id: "w1:p1".into(),
            cwd: None,
            agent: None,
            agent_status: AgentStatus::Unknown,
        },
    )];
    (snapshot, layouts)
}

fn two_pane_snapshot() -> (SessionSnapshot, Vec<(String, HerdrPaneTree)>) {
    let snapshot = SessionSnapshot {
        version: "0.7.4".into(),
        protocol: 17,
        focused_workspace_id: Some("w1".into()),
        focused_tab_id: Some("w1:t1".into()),
        focused_pane_id: Some("w1:p1".into()),
        workspaces: vec![workspace_info()],
        tabs: vec![tab_info("w1:t1", "w1", "agents")],
        panes: vec![
            pane_info("w1:p1", "w1", "w1:t1"),
            pane_info("w1:p2", "w1", "w1:t1"),
        ],
    };
    let layouts = vec![(
        "w1:t1".to_string(),
        HerdrPaneTree::Split {
            direction: HerdrSplitDirection::Right,
            children: Box::new([
                (
                    0.5,
                    HerdrPaneTree::Leaf {
                        pane_id: "w1:p1".into(),
                        cwd: None,
                        agent: None,
                        agent_status: AgentStatus::Unknown,
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
        },
    )];
    (snapshot, layouts)
}

fn seeded() -> (Reconciler, RecordingOps) {
    let mut rec = Reconciler::default();
    let mut ops = RecordingOps::default();
    let (snapshot, layouts) = one_tab_snapshot();
    rec.apply_snapshot(&snapshot, &layouts, &mut ops);
    (rec, ops)
}

fn seeded_two_panes() -> (Reconciler, RecordingOps) {
    let mut rec = Reconciler::default();
    let mut ops = RecordingOps::default();
    let (snapshot, layouts) = two_pane_snapshot();
    rec.apply_snapshot(&snapshot, &layouts, &mut ops);
    (rec, ops)
}

fn seeded_two_tabs() -> (Reconciler, RecordingOps) {
    let mut rec = Reconciler::default();
    let mut ops = RecordingOps::default();
    let (mut snapshot, mut layouts) = one_tab_snapshot();
    snapshot.tabs.push(tab_info("w1:t2", "w1", "build"));
    snapshot.panes.push(pane_info("w1:p2", "w1", "w1:t2"));
    layouts.push((
        "w1:t2".to_string(),
        HerdrPaneTree::Leaf {
            pane_id: "w1:p2".into(),
            cwd: None,
            agent: None,
            agent_status: AgentStatus::Unknown,
        },
    ));
    rec.apply_snapshot(&snapshot, &layouts, &mut ops);
    (rec, ops)
}

#[test]
fn snapshot_creates_groups_then_tabs() {
    let (_rec, ops) = seeded();
    assert_eq!(ops.calls[0], "group+ w1 proj");
    assert!(ops.calls[1].starts_with("tab+ w1:t1"));
}

#[test]
fn snapshot_pushes_initial_pane_titles() {
    let mut rec = Reconciler::default();
    let mut ops = RecordingOps::default();
    let mut pane = pane_info("w1:p1", "w1", "w1:t1");
    pane.terminal_title = Some("claude".into());
    let snapshot = SessionSnapshot {
        version: "0.7.4".into(),
        protocol: 17,
        focused_workspace_id: None,
        focused_tab_id: None,
        focused_pane_id: None,
        workspaces: vec![workspace_info()],
        tabs: vec![tab_info("w1:t1", "w1", "agents")],
        panes: vec![pane],
    };
    let layouts = vec![(
        "w1:t1".to_string(),
        HerdrPaneTree::Leaf {
            pane_id: "w1:p1".into(),
            cwd: None,
            agent: None,
            agent_status: AgentStatus::Unknown,
        },
    )];
    rec.apply_snapshot(&snapshot, &layouts, &mut ops);
    // Pane titles compose the herdr tab label with the live title.
    assert!(
        ops.calls
            .contains(&"title w1:p1 agents - claude".to_string()),
        "{:?}",
        ops.calls
    );
}

#[test]
fn snapshot_label_is_trimmed_and_emitted_after_tab_creation() {
    let mut rec = Reconciler::default();
    let mut ops = RecordingOps::default();
    let (mut snapshot, layouts) = one_tab_snapshot();
    snapshot.panes[0].label = Some("  reviewer  ".into());

    rec.apply_snapshot(&snapshot, &layouts, &mut ops);

    let tab_created = ops
        .calls
        .iter()
        .position(|call| call.starts_with("tab+ w1:t1"))
        .expect("snapshot creates the tab");
    let label_set = ops
        .calls
        .iter()
        .position(|call| call == "label w1:p1 reviewer")
        .expect("snapshot sets the normalized pane label");
    assert!(tab_created < label_set, "{:?}", ops.calls);
}

#[test]
fn snapshot_label_reconciliation_sets_changes_and_clears_only_labels() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    let mut pane = pane_info("w1:p1", "w1", "w1:t1");
    pane.label = Some("  reviewer  ".into());
    pane.terminal_title = Some("ignored".into());
    pane.agent = Some("ignored".into());
    pane.agent_status = AgentStatus::Blocked;
    pane.revision = 99;

    rec.reconcile_pane_labels(&[pane.clone()], &mut ops);
    pane.label = Some(" builder ".into());
    rec.reconcile_pane_labels(&[pane.clone()], &mut ops);
    pane.label = Some("   ".into());
    rec.reconcile_pane_labels(&[pane], &mut ops);

    assert_eq!(
        ops.calls,
        vec![
            "label w1:p1 reviewer",
            "label w1:p1 builder",
            "label w1:p1 -",
        ]
    );
}

#[test]
fn snapshot_label_reconciliation_ignores_unknown_panes() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    let mut pane = pane_info("w1:p9", "w1", "w1:t1");
    pane.label = Some("reviewer".into());

    rec.reconcile_pane_labels(&[pane], &mut ops);

    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
}

#[test]
fn snapshot_label_reconciliation_does_not_poison_pane_updated_fields() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    let mut snapshot_pane = pane_info("w1:p1", "w1", "w1:t1");
    snapshot_pane.label = Some("reviewer".into());
    snapshot_pane.terminal_title = Some("ignored".into());
    snapshot_pane.agent = Some("ignored".into());
    snapshot_pane.agent_status = AgentStatus::Blocked;
    snapshot_pane.revision = 99;
    rec.reconcile_pane_labels(&[snapshot_pane], &mut ops);
    assert_eq!(ops.calls, vec!["label w1:p1 reviewer"]);

    ops.calls.clear();
    let mut updated = pane_info("w1:p1", "w1", "w1:t1");
    updated.label = Some("reviewer".into());
    updated.terminal_title = Some("vim".into());
    updated.revision = 2;
    rec.apply_event(&EventData::PaneUpdated { pane: updated }, &mut ops);

    assert_eq!(ops.calls, vec!["title w1:p1 agents - vim"]);
}

#[test]
fn panes_without_live_titles_get_the_tab_label() {
    let (_rec, ops) = seeded();
    // seeded()'s pane has no terminal_title; the herdr tab label stands in.
    assert!(
        ops.calls.contains(&"title w1:p1 agents".to_string()),
        "{:?}",
        ops.calls
    );
}

#[test]
fn tab_rename_reprefixes_pane_titles() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    rec.apply_event(
        &EventData::TabRenamed {
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
            label: "build".into(),
        },
        &mut ops,
    );
    assert_eq!(ops.calls, vec!["tab~ w1:t1 build", "title w1:p1 build"]);
}

/// A herdr-side move applies the event's full post-move tab list, not just
/// insert_index, so an earlier missed move heals too.
#[test]
fn tab_moved_applies_full_order() {
    let (mut rec, mut ops) = seeded_two_tabs();
    ops.calls.clear();
    rec.apply_event(
        &EventData::TabMoved {
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
            insert_index: 1,
            tabs: vec![
                tab_info("w1:t2", "w1", "build"),
                tab_info("w1:t1", "w1", "agents"),
            ],
        },
        &mut ops,
    );
    assert_eq!(ops.calls, vec!["order w1 w1:t2,w1:t1"]);
}

/// The echo of a Warp-initiated tab.move is consumed by the MoveTab intent:
/// Warp already reordered the tab locally.
#[test]
fn warp_initiated_move_suppresses_echo() {
    let (mut rec, mut ops) = seeded_two_tabs();
    rec.note_local_intent(Intent::MoveTab("w1:t1".into()));
    ops.calls.clear();
    let echoed = EventData::TabMoved {
        tab_id: "w1:t1".into(),
        workspace_id: "w1".into(),
        insert_index: 1,
        tabs: vec![
            tab_info("w1:t2", "w1", "build"),
            tab_info("w1:t1", "w1", "agents"),
        ],
    };
    rec.apply_event(&echoed, &mut ops);
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
    // The intent was consumed, so a later herdr-side move applies normally.
    rec.apply_event(&echoed, &mut ops);
    assert_eq!(ops.calls, vec!["order w1 w1:t2,w1:t1"]);
}

/// A move for an unknown tab or workspace is ignored (its TabCreated may
/// still be awaiting a layout refresh); materialization heals the order.
#[test]
fn tab_moved_for_unknown_tab_or_workspace_is_ignored() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    rec.apply_event(
        &EventData::TabMoved {
            tab_id: "w1:t9".into(),
            workspace_id: "w1".into(),
            insert_index: 0,
            tabs: vec![tab_info("w1:t9", "w1", "ghost")],
        },
        &mut ops,
    );
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
    rec.apply_event(
        &EventData::TabMoved {
            tab_id: "w1:t1".into(),
            workspace_id: "w9".into(),
            insert_index: 0,
            tabs: vec![tab_info("w1:t1", "w9", "agents")],
        },
        &mut ops,
    );
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
}

#[test]
fn tab_created_event_is_applied_once_and_replay_is_ignored() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    let ev = EventData::TabCreated {
        tab: tab_info("w1:t2", "w1", "tests"),
    };
    rec.apply_event(&ev, &mut ops);
    // TabInfo carries no root pane id → creation defers to a layout refresh.
    assert_eq!(ops.calls, vec!["relayout w1:t2"]);
    let layout = HerdrPaneTree::Leaf {
        pane_id: "w1:p9".into(),
        cwd: None,
        agent: None,
        agent_status: AgentStatus::Unknown,
    };
    rec.apply_layout(&tab_info("w1:t2", "w1", "tests"), &layout, &mut ops);
    rec.apply_event(&ev, &mut ops); // replayed (512-event buffer) — must be a no-op
    rec.apply_layout(&tab_info("w1:t2", "w1", "tests"), &layout, &mut ops); // replayed refresh
    let creates = ops
        .calls
        .iter()
        .filter(|c| c.starts_with("tab+ w1:t2"))
        .count();
    assert_eq!(creates, 1);
}

/// A Warp-initiated tab.create materializes from the response and the
/// echoed broadcast is a replay no-op; a later herdr-side tab.create in
/// the same workspace must still materialize (regression: the stale
/// CreateTab intent left pending by the Warp-side create swallowed the
/// next herdr-side tab).
#[test]
fn herdr_tab_created_after_warp_initiated_create_materializes() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    let layout = HerdrPaneTree::Leaf {
        pane_id: "w1:p2".into(),
        cwd: None,
        agent: None,
        agent_status: AgentStatus::Unknown,
    };
    // Warp-side create: the tab.create response materializes the tab…
    rec.apply_layout(&tab_info("w1:t2", "w1", "mine"), &layout, &mut ops);
    // …and the echoed broadcast is deduped by the replay guard.
    rec.apply_event(
        &EventData::TabCreated {
            tab: tab_info("w1:t2", "w1", "mine"),
        },
        &mut ops,
    );
    let creates = ops
        .calls
        .iter()
        .filter(|c| c.starts_with("tab+ w1:t2"))
        .count();
    assert_eq!(creates, 1, "{:?}", ops.calls);
    // A herdr-side create in the same workspace must not be swallowed.
    ops.calls.clear();
    rec.apply_event(
        &EventData::TabCreated {
            tab: tab_info("w1:t3", "w1", "theirs"),
        },
        &mut ops,
    );
    assert_eq!(ops.calls, vec!["relayout w1:t3"]);
}

/// resync after a server restart is a full rebuild:
/// take_stale_tabs_for_resync returns the ids to tear down locally and
/// forgets ALL state — including pending intents noted toward the dead
/// server — so the fresh snapshot re-materializes from scratch and no
/// stale intent can swallow its events.
#[test]
fn take_stale_tabs_for_resync_forgets_everything() {
    let (mut rec, mut ops) = seeded();
    rec.note_local_intent(Intent::CloseTab("w1:t1".into()));
    let stale = rec.take_stale_tabs_for_resync();
    assert_eq!(stale, vec!["w1:t1".to_string()]);
    // The formerly known tab is forgotten; its events are ignored…
    ops.calls.clear();
    rec.apply_event(
        &EventData::TabClosed {
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
        },
        &mut ops,
    );
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
    // …the fresh snapshot re-materializes from scratch…
    let (snapshot, layouts) = one_tab_snapshot();
    rec.apply_snapshot(&snapshot, &layouts, &mut ops);
    assert!(
        ops.calls.iter().any(|c| c.starts_with("tab+ w1:t1")),
        "{:?}",
        ops.calls
    );
    // …and a herdr-initiated close of the rebuilt tab fires — the stale
    // CloseTab intent from before the restart would have swallowed it.
    ops.calls.clear();
    rec.apply_event(
        &EventData::TabClosed {
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
        },
        &mut ops,
    );
    assert_eq!(ops.calls, vec!["tab- w1:t1"]);
}

/// Resync forgets known workspaces too, so the fresh snapshot re-issues
/// create_workspace_group for groups that were pruned while their tabs were
/// torn down.
#[test]
fn resync_reissues_create_workspace_group() {
    let (mut rec, mut ops) = seeded();
    assert_eq!(
        ops.calls.iter().filter(|c| c.starts_with("group+")).count(),
        1
    );
    let stale = rec.take_stale_tabs_for_resync();
    assert_eq!(stale, vec!["w1:t1".to_string()]);
    ops.calls.clear();
    let (snapshot, layouts) = one_tab_snapshot();
    rec.apply_snapshot(&snapshot, &layouts, &mut ops);
    assert_eq!(ops.calls[0], "group+ w1 proj");
}

/// Resync teardown order is deterministic (sorted ids), not HashMap
/// iteration order.
#[test]
fn take_stale_tabs_for_resync_returns_sorted_ids() {
    let mut rec = Reconciler::default();
    let mut ops = RecordingOps::default();
    let (mut snapshot, mut layouts) = one_tab_snapshot();
    snapshot.tabs.push(tab_info("w1:t0", "w1", "first"));
    snapshot.panes.push(pane_info("w1:p0", "w1", "w1:t0"));
    layouts.push((
        "w1:t0".to_string(),
        HerdrPaneTree::Leaf {
            pane_id: "w1:p0".into(),
            cwd: None,
            agent: None,
            agent_status: AgentStatus::Unknown,
        },
    ));
    rec.apply_snapshot(&snapshot, &layouts, &mut ops);
    let stale = rec.take_stale_tabs_for_resync();
    assert_eq!(stale, vec!["w1:t0".to_string(), "w1:t1".to_string()]);
}

#[test]
fn tab_closed_removes_and_tolerates_unknown() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    rec.apply_event(
        &EventData::TabClosed {
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
        },
        &mut ops,
    );
    assert_eq!(ops.calls, vec!["tab- w1:t1"]);
    ops.calls.clear();
    rec.apply_event(
        &EventData::TabClosed {
            tab_id: "w1:t9".into(),
            workspace_id: "w1".into(),
        },
        &mut ops,
    );
    assert!(ops.calls.is_empty(), "closing unknown tab must be a no-op");
}

#[test]
fn warp_initiated_close_suppresses_echo() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    rec.note_local_intent(Intent::CloseTab("w1:t1".into()));
    rec.apply_event(
        &EventData::TabClosed {
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
        },
        &mut ops,
    );
    // The tab is forgotten from state but ops.close_tab must NOT fire
    // (Warp already closed it locally).
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
    // ...and the intent is consumed: a second close event for a re-created
    // same-id tab would apply normally (herdr never reuses ids, but be safe).
}

/// An intent whose echo never arrives (failed API call, lost event) expires
/// after INTENT_TTL instead of swallowing the next herdr-initiated event for
/// the same target.
#[test]
fn expired_intent_does_not_swallow_herdr_close() {
    let (mut rec, mut ops) = seeded();
    let backdated = Instant::now()
        .checked_sub(INTENT_TTL + Duration::from_secs(1))
        .expect("test host uptime exceeds the intent TTL");
    rec.pending
        .insert(Intent::CloseTab("w1:t1".into()), backdated);
    ops.calls.clear();
    rec.apply_event(
        &EventData::TabClosed {
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
        },
        &mut ops,
    );
    assert_eq!(ops.calls, vec!["tab- w1:t1"]);
}

/// The connection rolls back the noted intent when the API call fails, so
/// the next herdr-initiated event for the target applies normally.
#[test]
fn remove_intent_cancels_pending_intent() {
    let (mut rec, mut ops) = seeded();
    rec.note_local_intent(Intent::RenameTab {
        tab_id: "w1:t1".into(),
        label: "x".into(),
    });
    rec.remove_intent(Intent::RenameTab {
        tab_id: "w1:t1".into(),
        label: "x".into(),
    });
    ops.calls.clear();
    rec.apply_event(
        &EventData::TabRenamed {
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
            label: "theirs".into(),
        },
        &mut ops,
    );
    assert_eq!(ops.calls, vec!["tab~ w1:t1 theirs", "title w1:p1 theirs"]);
}

/// The herdr echo of a Warp-initiated pane close must not re-close the
/// local pane: the connection notes a ClosePane intent before calling
/// pane.close, and the reconciler consumes it. Without the intent the
/// same event closes the local pane (the herdr-initiated path).
#[test]
fn warp_initiated_pane_close_suppresses_echo() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    rec.note_local_intent(Intent::ClosePane("w1:p1".into()));
    rec.apply_event(
        &EventData::PaneClosed {
            pane_id: "w1:p1".into(),
            workspace_id: "w1".into(),
        },
        &mut ops,
    );
    // The echoed close is swallowed (no `pane-` call), but forgetting the
    // pane still drops the tab's status roll-up to Unknown.
    assert_eq!(ops.calls, vec!["status w1:t1 Unknown"], "{:?}", ops.calls);
    ops.calls.clear();
    // The pane is forgotten, so a stray second event is also a no-op...
    rec.apply_event(
        &EventData::PaneClosed {
            pane_id: "w1:p1".into(),
            workspace_id: "w1".into(),
        },
        &mut ops,
    );
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
    // ...while a herdr-initiated close (no intent) routes to ops.
    rec.apply_event(
        &EventData::PaneCreated {
            pane: pane_info("w1:p2", "w1", "w1:t1"),
        },
        &mut ops,
    );
    ops.calls.clear();
    rec.apply_event(
        &EventData::PaneClosed {
            pane_id: "w1:p2".into(),
            workspace_id: "w1".into(),
        },
        &mut ops,
    );
    assert_eq!(ops.calls, vec!["pane- w1:p2"]);
}

#[test]
fn pane_created_in_known_tab_adds_split() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    rec.apply_event(
        &EventData::PaneCreated {
            pane: pane_info("w1:p2", "w1", "w1:t1"),
        },
        &mut ops,
    );
    assert_eq!(
        ops.calls,
        vec![
            "pane+ w1:p2 in w1:t1",
            "relayout w1:t1",
            "title w1:p2 agents"
        ]
    );
}

/// A fresh split gets its composed title and agent icon up front, like
/// apply_snapshot pushes them, instead of waiting for the next pane.updated.
/// Idle pushes nothing — the pane materialization seeds the CLI agent
/// session's initial status from the snapshot status, so an idle-with-agent
/// pane needs no status correction (herdr 0.7.5 reports finished-while-unseen
/// agents as Done directly; idle means genuinely idle).
#[test]
fn pane_created_pushes_initial_title_and_agent() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    let mut pane = pane_info("w1:p2", "w1", "w1:t1");
    pane.terminal_title = Some("claude".into());
    pane.agent = Some("claude".into());
    rec.apply_event(&EventData::PaneCreated { pane }, &mut ops);
    assert_eq!(
        ops.calls,
        vec![
            "pane+ w1:p2 in w1:t1",
            "relayout w1:t1",
            "title w1:p2 agents - claude",
            "agent w1:p2 claude",
        ]
    );
}

#[test]
fn pane_created_emits_initial_label_after_pane_creation() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    let mut pane = pane_info("w1:p2", "w1", "w1:t1");
    pane.label = Some("  reviewer  ".into());

    rec.apply_event(&EventData::PaneCreated { pane }, &mut ops);

    let pane_created = ops
        .calls
        .iter()
        .position(|call| call == "pane+ w1:p2 in w1:t1")
        .expect("pane is created");
    let label_set = ops
        .calls
        .iter()
        .position(|call| call == "label w1:p2 reviewer")
        .expect("initial pane label is set");
    assert!(pane_created < label_set, "{:?}", ops.calls);
}

/// A Warp-initiated split applies pane_created twice: once from the
/// pane.split response and once from the echoed broadcast. The replay
/// guard must dedupe them into exactly one add_split (regression: the
/// former SplitPane intent suppressed the response path instead, so
/// Warp splits never materialized locally).
#[test]
fn pane_created_applied_twice_adds_split_once() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    for _ in 0..2 {
        rec.apply_event(
            &EventData::PaneCreated {
                pane: pane_info("w1:p2", "w1", "w1:t1"),
            },
            &mut ops,
        );
    }
    assert_eq!(
        ops.calls,
        vec![
            "pane+ w1:p2 in w1:t1",
            "relayout w1:t1",
            "title w1:p2 agents"
        ]
    );
}

/// A refreshed layout for an already-materialized tab syncs its
/// direction/ratio/structure onto the live tab instead of being discarded
/// (and does NOT re-materialize the tab).
#[test]
fn apply_layout_on_known_tab_syncs_geometry() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    let layout = HerdrPaneTree::Split {
        direction: HerdrSplitDirection::Right,
        children: Box::new([
            (
                0.5,
                HerdrPaneTree::Leaf {
                    pane_id: "w1:p1".into(),
                    cwd: None,
                    agent: None,
                    agent_status: AgentStatus::Unknown,
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
    rec.apply_layout(&tab_info("w1:t1", "w1", "agents"), &layout, &mut ops);
    assert_eq!(
        ops.calls,
        vec!["sync w1:t1 panes=[\"w1:p1\", \"w1:p2\"]"],
        "{:?}",
        ops.calls
    );
}

/// The first layout for a tab still awaiting it materializes the tab; the
/// geometry sync path is only for already-materialized tabs.
#[test]
fn apply_layout_for_awaiting_tab_materializes_without_sync() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    rec.apply_event(
        &EventData::TabCreated {
            tab: tab_info("w1:t2", "w1", "tests"),
        },
        &mut ops,
    );
    assert_eq!(ops.calls, vec!["relayout w1:t2"]);
    ops.calls.clear();
    let layout = HerdrPaneTree::Leaf {
        pane_id: "w1:p9".into(),
        cwd: None,
        agent: None,
        agent_status: AgentStatus::Unknown,
    };
    rec.apply_layout(&tab_info("w1:t2", "w1", "tests"), &layout, &mut ops);
    assert!(
        ops.calls.iter().any(|c| c.starts_with("tab+ w1:t2")),
        "{:?}",
        ops.calls
    );
    assert!(
        !ops.calls.iter().any(|c| c.starts_with("sync")),
        "{:?}",
        ops.calls
    );
}

/// A new pane in a materialized tab triggers a layout refresh: add_split
/// inserts at the pane-tree root with default sizing, and the refreshed
/// layout re-places the split with herdr's direction/ratio.
#[test]
fn pane_created_in_known_tab_requests_layout_refresh() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    rec.apply_event(
        &EventData::PaneCreated {
            pane: pane_info("w1:p2", "w1", "w1:t1"),
        },
        &mut ops,
    );
    assert!(
        ops.calls.contains(&"relayout w1:t1".to_string()),
        "{:?}",
        ops.calls
    );
}

/// layout.updated targets the tab whose geometry changed; the refreshed
/// layout then lands in apply_layout and syncs onto its live tab.
#[test]
fn layout_updated_refreshes_target_tab_only() {
    let mut rec = Reconciler::default();
    let mut ops = RecordingOps::default();
    let (mut snapshot, mut layouts) = one_tab_snapshot();
    snapshot.tabs.push(tab_info("w1:t2", "w1", "tests"));
    snapshot.panes.push(pane_info("w1:p2", "w1", "w1:t2"));
    layouts.push((
        "w1:t2".to_string(),
        HerdrPaneTree::Leaf {
            pane_id: "w1:p2".into(),
            cwd: None,
            agent: None,
            agent_status: AgentStatus::Unknown,
        },
    ));
    rec.apply_snapshot(&snapshot, &layouts, &mut ops);
    ops.calls.clear();
    rec.apply_event(
        &EventData::LayoutUpdated {
            tab_id: "w1:t2".into(),
        },
        &mut ops,
    );
    let refreshes: Vec<&str> = ops
        .calls
        .iter()
        .filter(|c| c.starts_with("relayout"))
        .map(String::as_str)
        .collect();
    assert_eq!(refreshes, vec!["relayout w1:t2"]);
    assert_eq!(ops.calls.len(), 1, "{:?}", ops.calls);
}

#[test]
fn layout_updated_for_unknown_tab_is_ignored() {
    let mut rec = Reconciler::default();
    let mut ops = RecordingOps::default();

    rec.apply_event(
        &EventData::LayoutUpdated {
            tab_id: "w1:t2".into(),
        },
        &mut ops,
    );

    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
}

#[test]
fn rename_and_status_flow_through() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    rec.apply_event(
        &EventData::TabRenamed {
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
            label: "ai".into(),
        },
        &mut ops,
    );
    rec.apply_agent_status(
        &AgentStatusEvent {
            pane_id: "w1:p1".into(),
            workspace_id: "w1".into(),
            agent_status: AgentStatus::Blocked,
            title: None,
            display_agent: None,
        },
        &mut ops,
    );
    assert_eq!(
        ops.calls,
        vec![
            "tab~ w1:t1 ai",
            "title w1:p1 ai",
            "pane_status w1:p1 Blocked",
            "status w1:t1 Blocked"
        ]
    );
}

#[test]
fn pane_updated_title_change_sets_title() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    let mut p = pane_info("w1:p1", "w1", "w1:t1");
    p.terminal_title = Some("vim".into());
    p.label = Some("reviewer".into());
    p.revision = 2;
    rec.apply_event(&EventData::PaneUpdated { pane: p.clone() }, &mut ops);
    rec.apply_event(&EventData::PaneUpdated { pane: p }, &mut ops); // same revision → no-op
    assert_eq!(
        ops.calls,
        vec!["label w1:p1 reviewer", "title w1:p1 agents - vim",]
    );
}

#[test]
fn same_revision_pane_updated_cannot_override_authoritative_snapshot_label() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    let mut snapshot_pane = pane_info("w1:p1", "w1", "w1:t1");
    snapshot_pane.label = Some(" snapshot ".into());
    rec.reconcile_pane_labels(&[snapshot_pane], &mut ops);
    assert_eq!(ops.calls, vec!["label w1:p1 snapshot"]);
    ops.calls.clear();

    let mut pane = pane_info("w1:p1", "w1", "w1:t1");
    pane.label = Some(" historical ".into());
    pane.terminal_title = Some("ignored".into());
    pane.agent = Some("ignored".into());
    pane.agent_status = AgentStatus::Blocked;

    rec.apply_event(&EventData::PaneUpdated { pane }, &mut ops);

    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
}

#[test]
fn older_pane_updated_cannot_regress_label() {
    let (mut rec, mut ops) = seeded();
    let mut newer = pane_info("w1:p1", "w1", "w1:t1");
    newer.label = Some("newer".into());
    newer.revision = 2;
    rec.apply_event(&EventData::PaneUpdated { pane: newer }, &mut ops);

    ops.calls.clear();
    let mut older = pane_info("w1:p1", "w1", "w1:t1");
    older.label = Some("older".into());
    older.revision = 1;
    rec.apply_event(&EventData::PaneUpdated { pane: older }, &mut ops);

    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
}

#[test]
fn pane_and_tab_labels_produce_independent_effects() {
    let mut rec = Reconciler::default();
    let mut ops = RecordingOps::default();
    let (mut snapshot, layouts) = one_tab_snapshot();
    snapshot.panes[0].label = Some("reviewer".into());
    rec.apply_snapshot(&snapshot, &layouts, &mut ops);

    ops.calls.clear();
    rec.apply_event(
        &EventData::TabRenamed {
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
            label: "build".into(),
        },
        &mut ops,
    );

    assert_eq!(ops.calls, vec!["tab~ w1:t1 build", "title w1:p1 build"]);
}

#[test]
fn authoritative_snapshot_repairs_a_locally_noted_label() {
    let (mut rec, mut ops) = seeded();
    rec.note_local_pane_label("w1:p1", Some(" local "));
    ops.calls.clear();
    let mut pane = pane_info("w1:p1", "w1", "w1:t1");
    pane.label = Some(" authoritative ".into());

    rec.reconcile_pane_labels(&[pane], &mut ops);

    assert_eq!(ops.calls, vec!["label w1:p1 authoritative"]);
}

#[test]
fn pane_updated_status_change_recolors_tab() {
    // pane.updated carries the per-pane agent_status; the tab color pushed
    // is the roll-up across the tab's panes (single pane here).
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    let mut p = pane_info("w1:p1", "w1", "w1:t1");
    p.agent_status = AgentStatus::Working;
    p.revision = 2;
    rec.apply_event(&EventData::PaneUpdated { pane: p.clone() }, &mut ops);
    assert_eq!(
        ops.calls,
        vec!["pane_status w1:p1 Working", "status w1:t1 Working"]
    );
    ops.calls.clear();
    p.revision = 3;
    rec.apply_event(&EventData::PaneUpdated { pane: p }, &mut ops); // status unchanged
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
}

/// Multi-pane regression: the tab status is a roll-up across the tab's
/// panes (Blocked > Working > Done > Idle), pushed only on change — an
/// idle sibling's pane.updated must not clear a Working sibling's color.
#[test]
fn tab_status_rolls_up_across_panes() {
    let (mut rec, mut ops) = seeded_two_panes();
    ops.calls.clear();
    // p2 starts working → tab goes Working.
    let mut p2 = pane_info("w1:p2", "w1", "w1:t1");
    p2.agent_status = AgentStatus::Working;
    p2.revision = 2;
    rec.apply_event(&EventData::PaneUpdated { pane: p2.clone() }, &mut ops);
    assert_eq!(
        ops.calls,
        vec!["pane_status w1:p2 Working", "status w1:t1 Working"]
    );
    // A title-only pane.updated from the idle sibling must not clear it.
    ops.calls.clear();
    let mut p1 = pane_info("w1:p1", "w1", "w1:t1");
    p1.terminal_title = Some("vim".into());
    p1.revision = 2;
    rec.apply_event(&EventData::PaneUpdated { pane: p1.clone() }, &mut ops);
    assert_eq!(ops.calls, vec!["title w1:p1 agents - vim"]);
    // Even a status change on the sibling keeps Working while p2 works: the
    // pane's own status still pushes, but the tab roll-up does not move.
    ops.calls.clear();
    p1.agent_status = AgentStatus::Done;
    p1.revision = 3;
    rec.apply_event(&EventData::PaneUpdated { pane: p1 }, &mut ops);
    assert_eq!(ops.calls, vec!["pane_status w1:p1 Done"], "{:?}", ops.calls);
    // p2 goes idle → the roll-up drops to the sibling's Done.
    ops.calls.clear();
    p2.agent_status = AgentStatus::Idle;
    p2.revision = 3;
    rec.apply_event(&EventData::PaneUpdated { pane: p2 }, &mut ops);
    assert_eq!(
        ops.calls,
        vec!["pane_status w1:p2 Idle", "status w1:t1 Done"]
    );
}

/// The dedicated pane.agent_status_changed subscription feeds the same
/// roll-up as pane.updated.
#[test]
fn agent_status_event_rolls_up_across_panes() {
    let (mut rec, mut ops) = seeded_two_panes();
    ops.calls.clear();
    let ev = |pane_id: &str, agent_status| AgentStatusEvent {
        pane_id: pane_id.into(),
        workspace_id: "w1".into(),
        agent_status,
        title: None,
        display_agent: None,
    };
    rec.apply_agent_status(&ev("w1:p2", AgentStatus::Blocked), &mut ops);
    assert_eq!(
        ops.calls,
        vec!["pane_status w1:p2 Blocked", "status w1:t1 Blocked"]
    );
    // The sibling's transition does not clear Blocked (the pane's own status
    // still pushes; the tab roll-up does not move).
    ops.calls.clear();
    rec.apply_agent_status(&ev("w1:p1", AgentStatus::Done), &mut ops);
    assert_eq!(ops.calls, vec!["pane_status w1:p1 Done"], "{:?}", ops.calls);
    // p2 clears → roll-up drops to the sibling's Done.
    ops.calls.clear();
    rec.apply_agent_status(&ev("w1:p2", AgentStatus::Idle), &mut ops);
    assert_eq!(
        ops.calls,
        vec!["pane_status w1:p2 Idle", "status w1:t1 Done"]
    );
}

/// The dedicated agent-status event drives the pane's own status indicator,
/// not just the tab roll-up, and dedupes on a repeated status.
#[test]
fn agent_status_event_pushes_per_pane_status() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    let ev = || AgentStatusEvent {
        pane_id: "w1:p1".into(),
        workspace_id: "w1".into(),
        agent_status: AgentStatus::Working,
        title: None,
        display_agent: None,
    };
    rec.apply_agent_status(&ev(), &mut ops);
    assert_eq!(
        ops.calls,
        vec!["pane_status w1:p1 Working", "status w1:t1 Working"]
    );
    ops.calls.clear();
    rec.apply_agent_status(&ev(), &mut ops); // same status → no-op
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
}

/// Snapshot seeding pushes each pane's non-Idle agent status (idle is the
/// pane's default, so only non-idle needs a push), like the tab color seed.
#[test]
fn snapshot_seeds_per_pane_agent_status() {
    let mut rec = Reconciler::default();
    let mut ops = RecordingOps::default();
    let (mut snapshot, layouts) = two_pane_snapshot();
    snapshot.panes[0].agent_status = AgentStatus::Blocked;
    // panes[1] stays Idle → no push for it.
    rec.apply_snapshot(&snapshot, &layouts, &mut ops);
    assert!(
        ops.calls.contains(&"pane_status w1:p1 Blocked".to_string()),
        "{:?}",
        ops.calls
    );
    assert!(
        !ops.calls.iter().any(|c| c.starts_with("pane_status w1:p2")),
        "{:?}",
        ops.calls
    );
}

/// A fresh split with a non-Idle status pushes the pane's status up front,
/// like its title and agent icon, instead of waiting for pane.updated.
#[test]
fn pane_created_pushes_initial_status() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    let mut pane = pane_info("w1:p2", "w1", "w1:t1");
    pane.agent_status = AgentStatus::Working;
    rec.apply_event(&EventData::PaneCreated { pane }, &mut ops);
    assert_eq!(
        ops.calls,
        vec![
            "pane+ w1:p2 in w1:t1",
            "relayout w1:t1",
            "title w1:p2 agents",
            "pane_status w1:p2 Working",
        ]
    );
    // An idle fresh pane pushes no status (idle is the pane's default).
    ops.calls.clear();
    rec.apply_event(
        &EventData::PaneCreated {
            pane: pane_info("w1:p3", "w1", "w1:t1"),
        },
        &mut ops,
    );
    assert!(
        !ops.calls.iter().any(|c| c.starts_with("pane_status")),
        "{:?}",
        ops.calls
    );
}

#[test]
fn agent_detected_event_updates_icon_and_dedupes() {
    // Agent identity changes arrive on the dedicated pane.agent_detected
    // subscription (they don't bump pane.updated revisions).
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    let started = EventData::PaneAgentDetected {
        pane_id: "w1:p1".into(),
        workspace_id: "w1".into(),
        agent: Some("claude".into()),
    };
    rec.apply_event(&started, &mut ops);
    rec.apply_event(&started, &mut ops); // replay → no-op
    assert_eq!(ops.calls, vec!["agent w1:p1 claude"]);
    ops.calls.clear();
    rec.apply_event(
        &EventData::PaneAgentDetected {
            pane_id: "w1:p1".into(),
            workspace_id: "w1".into(),
            agent: None,
        },
        &mut ops,
    );
    assert_eq!(ops.calls, vec!["agent w1:p1 -"]);
    ops.calls.clear();
    rec.apply_event(
        &EventData::PaneAgentDetected {
            pane_id: "w1:p9".into(),
            workspace_id: "w1".into(),
            agent: Some("claude".into()),
        },
        &mut ops,
    );
    assert!(ops.calls.is_empty(), "unknown pane must be a no-op");
}

#[test]
fn pane_updated_agent_change_updates_icon() {
    // The agent typically starts after the pane is mirrored, so the icon
    // must follow pane.updated agent diffs, including back to none.
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    let mut p = pane_info("w1:p1", "w1", "w1:t1");
    p.agent = Some("claude".into());
    p.revision = 2;
    rec.apply_event(&EventData::PaneUpdated { pane: p.clone() }, &mut ops);
    assert_eq!(ops.calls, vec!["agent w1:p1 claude"]);
    ops.calls.clear();
    p.revision = 3;
    rec.apply_event(&EventData::PaneUpdated { pane: p.clone() }, &mut ops); // agent unchanged
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
    p.agent = None;
    p.revision = 4;
    rec.apply_event(&EventData::PaneUpdated { pane: p }, &mut ops); // agent exited
    assert_eq!(ops.calls, vec!["agent w1:p1 -"]);
}

/// A TabClosed landing while the layout refresh is in flight must be
/// honored (not dropped by the unknown-tab guard), and the late layout must
/// not resurrect the tab herdr already destroyed.
#[test]
fn tab_closed_before_layout_refresh_is_not_resurrected() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    rec.apply_event(
        &EventData::TabCreated {
            tab: tab_info("w1:t2", "w1", "tests"),
        },
        &mut ops,
    );
    assert_eq!(ops.calls, vec!["relayout w1:t2"]);
    ops.calls.clear();
    // herdr closes the tab before the layout round trip lands: the close is
    // honored (nothing was materialized, so no close_tab op) and the tab is
    // forgotten.
    rec.apply_event(
        &EventData::TabClosed {
            tab_id: "w1:t2".into(),
            workspace_id: "w1".into(),
        },
        &mut ops,
    );
    assert!(!rec.is_known_tab("w1:t2"));
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
    // The in-flight refresh lands late: it must not materialize the dead tab.
    let layout = HerdrPaneTree::Leaf {
        pane_id: "w1:p9".into(),
        cwd: None,
        agent: None,
        agent_status: AgentStatus::Unknown,
    };
    rec.apply_layout(&tab_info("w1:t2", "w1", "tests"), &layout, &mut ops);
    assert!(!rec.is_known_tab("w1:t2"));
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
}

/// A pane.created arriving while its tab's layout refresh is in flight is
/// deferred to the layout, not applied to the half-known tab.
#[test]
fn pane_created_for_awaiting_tab_is_deferred() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    rec.apply_event(
        &EventData::TabCreated {
            tab: tab_info("w1:t2", "w1", "tests"),
        },
        &mut ops,
    );
    assert_eq!(ops.calls, vec!["relayout w1:t2"]);
    ops.calls.clear();
    rec.apply_event(
        &EventData::PaneCreated {
            pane: pane_info("w1:p9", "w1", "w1:t2"),
        },
        &mut ops,
    );
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
    // The pane arrives for real when the layout lands.
    let layout = HerdrPaneTree::Leaf {
        pane_id: "w1:p9".into(),
        cwd: None,
        agent: None,
        agent_status: AgentStatus::Unknown,
    };
    rec.apply_layout(&tab_info("w1:t2", "w1", "tests"), &layout, &mut ops);
    assert!(
        ops.calls.iter().any(|c| c.starts_with("tab+ w1:t2")),
        "{:?}",
        ops.calls
    );
}

/// apply_layout seeds the tab status from TabInfo.agent_status exactly like
/// apply_snapshot, so a tab created already non-idle is colored before any
/// pane event arrives.
#[test]
fn apply_layout_seeds_tab_status() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    let mut tab = tab_info("w1:t2", "w1", "tests");
    tab.agent_status = AgentStatus::Working;
    let layout = HerdrPaneTree::Leaf {
        pane_id: "w1:p9".into(),
        cwd: None,
        agent: None,
        agent_status: AgentStatus::Unknown,
    };
    rec.apply_layout(&tab, &layout, &mut ops);
    assert!(
        ops.calls.contains(&"status w1:t2 Working".to_string()),
        "{:?}",
        ops.calls
    );
    // An idle tab pushes no color.
    ops.calls.clear();
    let layout = HerdrPaneTree::Leaf {
        pane_id: "w1:p10".into(),
        cwd: None,
        agent: None,
        agent_status: AgentStatus::Unknown,
    };
    rec.apply_layout(&tab_info("w1:t3", "w1", "idle"), &layout, &mut ops);
    assert!(
        !ops.calls.iter().any(|c| c.starts_with("status")),
        "{:?}",
        ops.calls
    );
}

/// Lost-refresh self-heal: a fresh awaiting entry (refresh just requested)
/// must NOT re-request — the initial relayout from TabCreated is the only
/// one inside the throttle window.
#[test]
fn fresh_awaiting_tab_event_does_not_rerequest() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    rec.apply_event(
        &EventData::TabCreated {
            tab: tab_info("w1:t2", "w1", "tests"),
        },
        &mut ops,
    );
    assert_eq!(ops.calls, vec!["relayout w1:t2"]);
    ops.calls.clear();
    rec.apply_event(
        &EventData::PaneCreated {
            pane: pane_info("w1:p9", "w1", "w1:t2"),
        },
        &mut ops,
    );
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
}

/// An awaiting entry whose last request is older than the throttle window
/// re-requests exactly once; the timestamp and retry count reset with it.
#[test]
fn stale_awaiting_tab_event_rerequests_refresh() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    rec.apply_event(
        &EventData::TabCreated {
            tab: tab_info("w1:t2", "w1", "tests"),
        },
        &mut ops,
    );
    assert_eq!(ops.calls, vec!["relayout w1:t2"]);
    // Pretend the refresh went out long enough ago to be considered lost.
    let backdated = Instant::now()
        .checked_sub(LAYOUT_REFRESH_REREQUEST_AFTER + Duration::from_secs(1))
        .expect("test host uptime exceeds the re-request window");
    rec.tabs_awaiting_layout
        .get_mut("w1:t2")
        .expect("tab is awaiting layout")
        .last_request = backdated;
    ops.calls.clear();
    rec.apply_event(
        &EventData::PaneCreated {
            pane: pane_info("w1:p9", "w1", "w1:t2"),
        },
        &mut ops,
    );
    assert_eq!(ops.calls, vec!["relayout w1:t2"]);
    assert_eq!(
        rec.tabs_awaiting_layout
            .get("w1:t2")
            .expect("tab is awaiting layout")
            .retries,
        1
    );
    // The timestamp reset, so an immediate follow-up event does not
    // re-request again.
    ops.calls.clear();
    rec.apply_event(
        &EventData::PaneCreated {
            pane: pane_info("w1:p10", "w1", "w1:t2"),
        },
        &mut ops,
    );
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
}

/// A tombstoned tab (closed while its refresh was in flight) never
/// re-requests, however stale the entry. pane.updated reaches the tombstone
/// check: unlike pane.created it is not stopped by the unknown-tab guard.
#[test]
fn tombstoned_awaiting_tab_never_rerequests() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    rec.apply_event(
        &EventData::TabCreated {
            tab: tab_info("w1:t2", "w1", "tests"),
        },
        &mut ops,
    );
    rec.apply_event(
        &EventData::TabClosed {
            tab_id: "w1:t2".into(),
            workspace_id: "w1".into(),
        },
        &mut ops,
    );
    let backdated = Instant::now()
        .checked_sub(LAYOUT_REFRESH_REREQUEST_AFTER + Duration::from_secs(1))
        .expect("test host uptime exceeds the re-request window");
    rec.tabs_awaiting_layout
        .get_mut("w1:t2")
        .expect("tombstone is still tracked")
        .last_request = backdated;
    ops.calls.clear();
    let mut pane = pane_info("w1:p9", "w1", "w1:t2");
    pane.revision = 2;
    rec.apply_event(&EventData::PaneUpdated { pane }, &mut ops);
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
}

/// Once the re-request cap is exhausted the tab stays degraded (no further
/// relayout calls) until the next full resync.
#[test]
fn awaiting_tab_rerequest_stops_at_the_cap() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    rec.apply_event(
        &EventData::TabCreated {
            tab: tab_info("w1:t2", "w1", "tests"),
        },
        &mut ops,
    );
    assert_eq!(ops.calls, vec!["relayout w1:t2"]);
    let backdated = Instant::now()
        .checked_sub(LAYOUT_REFRESH_REREQUEST_AFTER + Duration::from_secs(1))
        .expect("test host uptime exceeds the re-request window");
    let entry = rec
        .tabs_awaiting_layout
        .get_mut("w1:t2")
        .expect("tab is awaiting layout");
    entry.last_request = backdated;
    entry.retries = MAX_LAYOUT_REFRESH_REREQUESTS;
    ops.calls.clear();
    rec.apply_event(
        &EventData::PaneCreated {
            pane: pane_info("w1:p9", "w1", "w1:t2"),
        },
        &mut ops,
    );
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
}

/// pane.updated for a not-yet-known pane of an awaiting tab flows through
/// the same self-heal (PaneInfo carries tab_id, unlike pane.closed).
#[test]
fn pane_updated_for_awaiting_tab_rerequests_stale_refresh() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    rec.apply_event(
        &EventData::TabCreated {
            tab: tab_info("w1:t2", "w1", "tests"),
        },
        &mut ops,
    );
    assert_eq!(ops.calls, vec!["relayout w1:t2"]);
    let backdated = Instant::now()
        .checked_sub(LAYOUT_REFRESH_REREQUEST_AFTER + Duration::from_secs(1))
        .expect("test host uptime exceeds the re-request window");
    rec.tabs_awaiting_layout
        .get_mut("w1:t2")
        .expect("tab is awaiting layout")
        .last_request = backdated;
    ops.calls.clear();
    let mut pane = pane_info("w1:p9", "w1", "w1:t2");
    pane.revision = 2;
    rec.apply_event(&EventData::PaneUpdated { pane }, &mut ops);
    assert_eq!(ops.calls, vec!["relayout w1:t2"]);
}

/// pane.agent_status_changed subscriptions are pane-scoped, so learning a
/// pane after subscribe time must flag a re-subscribe carrying the full
/// pane set — once, and not again for the replayed duplicate (herdr replays
/// up to 512 buffered events on subscribe).
#[test]
fn new_pane_flags_subscription_refresh_once() {
    let (mut rec, mut ops) = seeded();
    assert_eq!(rec.take_pane_subscription_refresh(), None);

    rec.apply_event(
        &EventData::PaneCreated {
            pane: pane_info("w1:p2", "w1", "w1:t1"),
        },
        &mut ops,
    );
    assert_eq!(
        rec.take_pane_subscription_refresh(),
        Some(vec!["w1:p1".to_string(), "w1:p2".to_string()])
    );
    assert_eq!(rec.take_pane_subscription_refresh(), None);

    rec.apply_event(
        &EventData::PaneCreated {
            pane: pane_info("w1:p2", "w1", "w1:t1"),
        },
        &mut ops,
    );
    assert_eq!(rec.take_pane_subscription_refresh(), None);
}

/// A tab materialized after attach (TabCreated → apply_layout) carries new
/// panes that also need status subscriptions.
#[test]
fn late_tab_layout_flags_subscription_refresh() {
    let (mut rec, mut ops) = seeded();
    assert_eq!(rec.take_pane_subscription_refresh(), None);
    rec.apply_layout(
        &tab_info("w1:t2", "w1", "build"),
        &HerdrPaneTree::Leaf {
            pane_id: "w1:p9".into(),
            cwd: None,
            agent: None,
            agent_status: AgentStatus::Unknown,
        },
        &mut ops,
    );
    let ids = rec
        .take_pane_subscription_refresh()
        .expect("new tab's panes need subscriptions");
    assert!(ids.contains(&"w1:p9".to_string()), "{ids:?}");
}

/// Snapshot applies (bootstrap/resync) subscribe with the snapshot's own
/// pane set, so they clear rather than flag the refresh.
#[test]
fn snapshot_apply_clears_subscription_refresh() {
    let (mut rec, _ops) = seeded();
    assert_eq!(rec.take_pane_subscription_refresh(), None);
}

/// A geometry refresh in flight when a MATERIALIZED tab closes must not
/// resurrect it: TabClosed tombstones the id, and the late apply_layout —
/// plus a second late response from a re-requested refresh (tombstones are
/// non-consuming) — is discarded instead of materializing a ghost tab.
#[test]
fn late_layout_after_close_never_resurrects_tab() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    rec.apply_event(
        &EventData::TabClosed {
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
        },
        &mut ops,
    );
    assert_eq!(ops.calls, vec!["tab- w1:t1"]);
    ops.calls.clear();

    let layout = HerdrPaneTree::Leaf {
        pane_id: "w1:p1".into(),
        cwd: None,
        agent: None,
        agent_status: AgentStatus::Unknown,
    };
    rec.apply_layout(&tab_info("w1:t1", "w1", "agents"), &layout, &mut ops);
    rec.apply_layout(&tab_info("w1:t1", "w1", "agents"), &layout, &mut ops);
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
}

/// Double-late-response for an AWAITING tab: created → closed (tombstone) →
/// two late layout arrivals. The first used to consume the tombstone, letting
/// the second materialize the dead tab.
#[test]
fn double_late_layout_for_awaiting_tab_is_discarded() {
    let (mut rec, mut ops) = seeded();
    rec.apply_event(
        &EventData::TabCreated {
            tab: tab_info("w1:t2", "w1", "tests"),
        },
        &mut ops,
    );
    rec.apply_event(
        &EventData::TabClosed {
            tab_id: "w1:t2".into(),
            workspace_id: "w1".into(),
        },
        &mut ops,
    );
    ops.calls.clear();

    let layout = HerdrPaneTree::Leaf {
        pane_id: "w1:p9".into(),
        cwd: None,
        agent: None,
        agent_status: AgentStatus::Unknown,
    };
    rec.apply_layout(&tab_info("w1:t2", "w1", "tests"), &layout, &mut ops);
    rec.apply_layout(&tab_info("w1:t2", "w1", "tests"), &layout, &mut ops);
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
}

/// A single pane.updated carrying a newly-detected agent AND a non-idle
/// status must push the agent first: the status push seeds/updates the CLI
/// agent session, which exists only once the agent is known (add_split
/// orders the same way).
#[test]
fn pane_updated_pushes_agent_before_status() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();

    let mut pane = pane_info("w1:p1", "w1", "w1:t1");
    pane.revision = 2;
    pane.agent = Some("claude".into());
    pane.agent_status = AgentStatus::Blocked;
    rec.apply_event(&EventData::PaneUpdated { pane }, &mut ops);

    let agent_idx = ops
        .calls
        .iter()
        .position(|c| c.starts_with("agent w1:p1"))
        .expect("agent pushed");
    let status_idx = ops
        .calls
        .iter()
        .position(|c| c.starts_with("pane_status w1:p1"))
        .expect("status pushed");
    assert!(agent_idx < status_idx, "{:?}", ops.calls);
}

/// WorkspaceClosed mirrors TabClosed per tab: materialized tabs close,
/// awaiting tabs don't (nothing local exists), and every id is tombstoned so
/// late layout refreshes can't resurrect them.
#[test]
fn workspace_closed_skips_awaiting_tabs_and_tombstones_all() {
    let (mut rec, mut ops) = seeded_two_tabs();
    rec.apply_event(
        &EventData::TabCreated {
            tab: tab_info("w1:t3", "w1", "new"),
        },
        &mut ops,
    );
    ops.calls.clear();

    rec.apply_event(
        &EventData::WorkspaceClosed {
            workspace_id: "w1".into(),
        },
        &mut ops,
    );
    let closes: Vec<&str> = ops
        .calls
        .iter()
        .map(String::as_str)
        .filter(|c| c.starts_with("tab- "))
        .collect();
    assert_eq!(closes, ["tab- w1:t1", "tab- w1:t2"], "{:?}", ops.calls);

    // Late layouts for every tab of the closed workspace — the awaiting one
    // included — are discarded.
    ops.calls.clear();
    for (tab, pane) in [("w1:t1", "w1:p1"), ("w1:t3", "w1:p9")] {
        rec.apply_layout(
            &tab_info(tab, "w1", "x"),
            &HerdrPaneTree::Leaf {
                pane_id: pane.into(),
                cwd: None,
                agent: None,
                agent_status: AgentStatus::Unknown,
            },
            &mut ops,
        );
    }
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
}

/// WorkspaceCreated is idempotent under herdr's event replay; a genuinely
/// new workspace creates its group, and WorkspaceRenamed stays a v1 no-op.
#[test]
fn workspace_created_replay_is_idempotent() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();

    rec.apply_event(
        &EventData::WorkspaceCreated {
            workspace: workspace_info(),
        },
        &mut ops,
    );
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);

    let mut ws2 = workspace_info();
    ws2.workspace_id = "w2".into();
    ws2.label = "other".into();
    rec.apply_event(&EventData::WorkspaceCreated { workspace: ws2 }, &mut ops);
    assert_eq!(ops.calls, vec!["group+ w2 other"]);

    ops.calls.clear();
    rec.apply_event(
        &EventData::WorkspaceRenamed {
            workspace_id: "w1".into(),
            label: "renamed".into(),
        },
        &mut ops,
    );
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
}

/// pane.exited tears the pane down exactly like pane.closed.
#[test]
fn pane_exited_closes_pane() {
    let (mut rec, mut ops) = seeded_two_panes();
    ops.calls.clear();
    rec.apply_event(
        &EventData::PaneExited {
            pane_id: "w1:p2".into(),
            workspace_id: "w1".into(),
        },
        &mut ops,
    );
    assert!(
        ops.calls.contains(&"pane- w1:p2".to_string()),
        "{:?}",
        ops.calls
    );
}

/// A Warp-initiated rename's intent swallows only the MATCHING echo: another
/// client renaming the same tab to something else inside the confirmation
/// window must still apply (a coarse tab-id key used to drop it).
#[test]
fn rename_intent_swallows_only_matching_echo() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();

    rec.note_local_intent(Intent::RenameTab {
        tab_id: "w1:t1".into(),
        label: "ours".into(),
    });
    rec.apply_event(
        &EventData::TabRenamed {
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
            label: "theirs".into(),
        },
        &mut ops,
    );
    assert!(
        ops.calls.contains(&"tab~ w1:t1 theirs".to_string()),
        "{:?}",
        ops.calls
    );

    ops.calls.clear();
    rec.note_local_intent(Intent::RenameTab {
        tab_id: "w1:t1".into(),
        label: "ours".into(),
    });
    rec.apply_event(
        &EventData::TabRenamed {
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
            label: "ours".into(),
        },
        &mut ops,
    );
    assert!(
        !ops.calls.iter().any(|c| c.starts_with("tab~")),
        "{:?}",
        ops.calls
    );
}

/// herdr's snapshot status is seen-aware (finished-while-unseen reports Done
/// directly; Idle means at-prompt and already seen), and pane materialization
/// seeds the CLI agent session's initial status from it — so an idle pane
/// pushes NO status at materialization, with or without an agent. Coercing
/// idle-with-agent to Done here would over-mark agents herdr itself renders
/// as plain idle (live-verified against 0.7.5, 2026-07-22).
#[test]
fn materialization_pushes_nothing_for_idle_agent_panes() {
    let mut rec = Reconciler::default();
    let mut ops = RecordingOps::default();
    let (mut snapshot, layouts) = two_pane_snapshot();
    snapshot.panes[0].agent = Some("claude".into()); // idle + agent
    rec.apply_snapshot(&snapshot, &layouts, &mut ops);

    assert!(
        !ops.calls.iter().any(|c| c.starts_with("pane_status w1:p1")),
        "{:?}",
        ops.calls
    );
    assert!(
        !ops.calls.iter().any(|c| c.starts_with("pane_status w1:p2")),
        "{:?}",
        ops.calls
    );
}

/// Same seed rule for a live pane.created: an idle pane with an agent pushes
/// no status — the pane's session seeds its initial status from the pane
/// info, so there is nothing to correct.
#[test]
fn pane_created_pushes_nothing_for_idle_agent_pane() {
    let mut rec = Reconciler::default();
    let mut ops = RecordingOps::default();
    let (snapshot, layouts) = one_tab_snapshot();
    rec.apply_snapshot(&snapshot, &layouts, &mut ops);
    ops.calls.clear();

    let mut pane = pane_info("w1:p9", "w1", "w1:t1");
    pane.agent = Some("claude".into()); // idle + agent
    rec.apply_event(&EventData::PaneCreated { pane }, &mut ops);

    assert!(
        !ops.calls.iter().any(|c| c.starts_with("pane_status w1:p9")),
        "{:?}",
        ops.calls
    );
}

/// A pending intent only exists after its API call succeeded, so herdr HAS
/// broadcast the echoed event — expiry therefore means the event stream is
/// not delivering, and the connection must learn about it (it resyncs).
#[test]
fn expired_intent_flags_the_stream_as_suspect() {
    let mut rec = Reconciler::default();
    rec.note_local_intent(Intent::MoveTab("w1:t1".into()));
    assert!(!rec.take_intent_expiry(), "nothing expired yet");

    let backdated = Instant::now()
        .checked_sub(Duration::from_secs(11))
        .expect("test host uptime exceeds the intent TTL");
    for at in rec.pending.values_mut() {
        *at = backdated;
    }
    rec.expire_stale_intents();

    assert!(rec.take_intent_expiry());
    assert!(!rec.take_intent_expiry(), "the flag is take-once");
    assert!(rec.pending.is_empty(), "the expired intent was swept");
}
