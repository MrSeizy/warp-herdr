//! Tests for the herdr tab-order sync in Workspace: applying a herdr-side
//! order within a group run (run-start computation, unknown-id tolerance,
//! extras at the end, focus preservation) and the pure run-order mapping.

use warpui::App;

use super::herdr::herdr_run_target_order;
use super::tests::{initialize_app, mock_workspace};
use super::*;

/// Marks `tabs[index]` as mirroring herdr tab `herdr_id` of workspace
/// `workspace_id` and assigns it to the group backing that workspace,
/// creating the group (and the herdr_group_ids mapping) on first use.
fn mark_herdr_tab_in_workspace(
    workspace: &mut Workspace,
    index: usize,
    workspace_id: &str,
    herdr_id: &str,
) {
    let group_id = *workspace
        .herdr_group_ids
        .entry(workspace_id.to_string())
        .or_insert_with(|| {
            let group = TabGroup::new();
            let id = group.id;
            workspace.tab_groups.insert(id, group);
            id
        });
    workspace.tabs[index].group_id = Some(group_id);
    workspace.tabs[index].herdr_tab = Some(herdr_id.to_string());
    workspace.tabs[index].herdr_workspace = Some(workspace_id.to_string());
}

/// `mark_herdr_tab_in_workspace` for the common "<ws>:<tab>" fixture ids.
fn mark_herdr_tab(workspace: &mut Workspace, index: usize, herdr_id: &str) {
    let (workspace_id, _) = herdr_id.split_once(':').unwrap();
    mark_herdr_tab_in_workspace(workspace, index, workspace_id, herdr_id);
}

fn mark_first_pane_as_herdr(
    workspace: &mut Workspace,
    tab_index: usize,
    herdr_tab_id: &str,
    herdr_pane_id: &str,
    ctx: &mut ViewContext<Workspace>,
) -> PaneViewLocator {
    mark_herdr_tab(workspace, tab_index, herdr_tab_id);
    let pane_group = workspace.tabs[tab_index].pane_group.clone();
    let pane_id = pane_group.as_ref(ctx).pane_ids().next().unwrap();
    pane_group.update(ctx, |panes, _ctx| {
        panes.register_herdr_pane_for_test(herdr_pane_id, pane_id);
    });
    PaneViewLocator {
        pane_group_id: pane_group.id(),
        pane_id,
    }
}

/// Populates `herdr_connection` with a socket-less connection. Detach and the
/// attach guard both key off its presence, and nothing asserted at the
/// Workspace layer needs a live server.
fn attach_test_connection(workspace: &mut Workspace, ctx: &mut ViewContext<Workspace>) {
    let handle = ctx.handle();
    workspace.herdr_connection =
        Some(ctx.add_model(|_| crate::herdr::connection::HerdrConnection::for_test(handle)));
}

/// The terminal model behind a pane, held directly so it stays observable
/// after the tab that owned it is closed.
fn pane_terminal_model(
    workspace: &Workspace,
    locator: &PaneViewLocator,
    ctx: &ViewContext<Workspace>,
) -> Arc<FairMutex<TerminalModel>> {
    let pane_group = workspace
        .tabs
        .iter()
        .map(|tab| tab.pane_group.clone())
        .find(|pg| pg.id() == locator.pane_group_id)
        .expect("pane group still present");
    let pg = pane_group.as_ref(ctx);
    pg.terminal_view_from_pane_id(locator.pane_id, ctx)
        .expect("pane has a terminal view")
        .as_ref(ctx)
        .model
        .clone()
}

fn tab_order(workspace: &Workspace) -> Vec<EntityId> {
    workspace
        .tabs
        .iter()
        .map(|tab| tab.pane_group.id())
        .collect()
}

fn ordered(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

#[test]
fn test_herdr_apply_tab_order_reorders_within_group_run() {
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            // One local tab in front, so the herdr run starts past index 0.
            workspace.add_terminal_tab(false, ctx);
            workspace.add_terminal_tab(false, ctx);
            workspace.add_terminal_tab(false, ctx);
            assert_eq!(workspace.tab_count(), 4);
            mark_herdr_tab(workspace, 1, "w1:t1");
            mark_herdr_tab(workspace, 2, "w1:t2");
            mark_herdr_tab(workspace, 3, "w1:t3");
            let local = workspace.tabs[0].pane_group.id();
            let t1 = workspace.tabs[1].pane_group.id();
            let t2 = workspace.tabs[2].pane_group.id();
            let t3 = workspace.tabs[3].pane_group.id();

            workspace.herdr_apply_tab_order("w1", &ordered(&["w1:t3", "w1:t1", "w1:t2"]), ctx);

            // The run reorders in place; the local tab keeps its slot.
            assert_eq!(tab_order(workspace), vec![local, t3, t1, t2]);
            // The remote-order guard is released when the apply finishes.
            assert!(!workspace.herdr_applying_remote_order);
        });
    });
}

#[test]
fn test_herdr_apply_tab_order_skips_unknown_and_keeps_extras_at_end() {
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            workspace.add_terminal_tab(false, ctx);
            workspace.add_terminal_tab(false, ctx);
            workspace.add_terminal_tab(false, ctx);
            mark_herdr_tab(workspace, 1, "w1:t1");
            mark_herdr_tab(workspace, 2, "w1:t2");
            mark_herdr_tab(workspace, 3, "w1:t3");
            let local = workspace.tabs[0].pane_group.id();
            let t1 = workspace.tabs[1].pane_group.id();
            let t2 = workspace.tabs[2].pane_group.id();
            let t3 = workspace.tabs[3].pane_group.id();

            // w1:t9 is not materialized locally (still awaiting its layout):
            // skipped. w1:t1 and w1:t3 are missing from the list: they keep
            // their relative order at the end of the run.
            workspace.herdr_apply_tab_order("w1", &ordered(&["w1:t2", "w1:t9"]), ctx);

            assert_eq!(tab_order(workspace), vec![local, t2, t1, t3]);
        });
    });
}

#[test]
fn test_herdr_apply_tab_order_preserves_active_tab() {
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            workspace.add_terminal_tab(false, ctx);
            workspace.add_terminal_tab(false, ctx);
            workspace.add_terminal_tab(false, ctx);
            mark_herdr_tab(workspace, 1, "w1:t1");
            mark_herdr_tab(workspace, 2, "w1:t2");
            mark_herdr_tab(workspace, 3, "w1:t3");
            let local = workspace.tabs[0].pane_group.id();
            let t1 = workspace.tabs[1].pane_group.id();
            let t2 = workspace.tabs[2].pane_group.id();
            let t3 = workspace.tabs[3].pane_group.id();
            workspace.activate_tab(2, ctx); // t2 active

            workspace.herdr_apply_tab_order("w1", &ordered(&["w1:t3", "w1:t2", "w1:t1"]), ctx);

            assert_eq!(tab_order(workspace), vec![local, t3, t2, t1]);
            let active = workspace.active_tab_index();
            assert_eq!(workspace.tabs[active].pane_group.id(), t2);
        });
    });
}

#[test]
fn test_herdr_apply_tab_order_only_touches_addressed_workspace() {
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            workspace.add_terminal_tab(false, ctx);
            workspace.add_terminal_tab(false, ctx);
            workspace.add_terminal_tab(false, ctx);
            // Two herdr workspaces, each a contiguous run.
            mark_herdr_tab(workspace, 0, "w1:t1");
            mark_herdr_tab(workspace, 1, "w1:t2");
            mark_herdr_tab(workspace, 2, "w2:t1");
            mark_herdr_tab(workspace, 3, "w2:t2");
            let w1t1 = workspace.tabs[0].pane_group.id();
            let w1t2 = workspace.tabs[1].pane_group.id();
            let w2t1 = workspace.tabs[2].pane_group.id();
            let w2t2 = workspace.tabs[3].pane_group.id();

            workspace.herdr_apply_tab_order("w2", &ordered(&["w2:t2", "w2:t1"]), ctx);

            // w2's run (starting past w1's run) flips; w1 is untouched.
            assert_eq!(tab_order(workspace), vec![w1t1, w1t2, w2t2, w2t1]);
        });
    });
}

#[test]
fn test_herdr_apply_tab_order_unknown_workspace_is_noop() {
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            workspace.add_terminal_tab(false, ctx);
            mark_herdr_tab(workspace, 0, "w1:t1");
            mark_herdr_tab(workspace, 1, "w1:t2");
            let before = tab_order(workspace);

            workspace.herdr_apply_tab_order("w9", &ordered(&["w9:t2", "w9:t1"]), ctx);

            assert_eq!(tab_order(workspace), before);
        });
    });
}

/// tab.move's index space contains only the workspace's own mirrored tabs: a
/// local tab interleaved into the group run must not inflate the index sent
/// to herdr (run [H1, L, H2] — H2 is herdr's index 1, not 2).
#[test]
fn test_herdr_run_position_skips_interleaved_local_tabs() {
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            workspace.add_terminal_tab(false, ctx);
            workspace.add_terminal_tab(false, ctx);
            mark_herdr_tab(workspace, 0, "w1:t1");
            mark_herdr_tab(workspace, 2, "w1:t2");

            assert_eq!(
                workspace.herdr_run_position(0),
                Some(("w1:t1".to_string(), 0))
            );
            assert_eq!(
                workspace.herdr_run_position(2),
                Some(("w1:t2".to_string(), 1))
            );
            // The interleaved local tab itself has no herdr position.
            assert_eq!(workspace.herdr_run_position(1), None);
        });
    });
}

/// The herdr tab id is opaque to Warp: run positions come from the stored
/// workspace binding, not from parsing "<ws>:<tab>" out of the id, so an id
/// in any format herdr chooses still mirrors moves.
#[test]
fn test_herdr_run_position_does_not_parse_the_tab_id_format() {
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            workspace.add_terminal_tab(false, ctx);
            workspace.add_terminal_tab(false, ctx);
            // Opaque ids with no ':' separator; the local tab at index 1
            // stays interleaved so the run index must still skip it.
            mark_herdr_tab_in_workspace(workspace, 0, "w1", "opaque-a");
            mark_herdr_tab_in_workspace(workspace, 2, "w1", "opaque-b");

            assert_eq!(
                workspace.herdr_run_position(0),
                Some(("opaque-a".to_string(), 0))
            );
            assert_eq!(
                workspace.herdr_run_position(2),
                Some(("opaque-b".to_string(), 1))
            );
        });
    });
}

#[test]
fn test_herdr_run_target_order_mapping() {
    let run = [Some("w1:t1"), Some("w1:t2"), None, Some("w1:t3")];
    // Known ids land first in herdr's order; unknown ids are skipped; unlisted
    // tabs (including a non-mirrored None entry) keep relative order at the end.
    assert_eq!(
        herdr_run_target_order(&run, &ordered(&["w1:t3", "w1:tX", "w1:t1"])),
        vec![3, 0, 1, 2]
    );
    // An empty order leaves the run untouched (everything is an extra).
    assert_eq!(
        herdr_run_target_order(&run, &ordered(&[])),
        vec![0, 1, 2, 3]
    );
    // Duplicate ids collapse to the first match.
    assert_eq!(
        herdr_run_target_order(&run, &ordered(&["w1:t2", "w1:t2"])),
        vec![1, 0, 2, 3]
    );
}

/// Live agent-status colors layer above the user's manual tab color without
/// overwriting it: the selection survives status flips and shows again once
/// the status clears.
#[test]
fn test_herdr_tab_status_never_clobbers_manual_color() {
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            mark_herdr_tab(workspace, 0, "w1:t1");
            workspace.set_tab_color(
                0,
                SelectedTabColor::Color(AnsiColorIdentifier::Magenta),
                ctx,
            );

            workspace.herdr_set_tab_status("w1:t1", AgentStatus::Working, ctx);
            assert_eq!(workspace.tabs[0].color(), Some(AnsiColorIdentifier::Blue));
            assert_eq!(
                workspace.tabs[0].selected_color,
                SelectedTabColor::Color(AnsiColorIdentifier::Magenta)
            );

            workspace.herdr_set_tab_status("w1:t1", AgentStatus::Idle, ctx);
            assert_eq!(
                workspace.tabs[0].color(),
                Some(AnsiColorIdentifier::Magenta)
            );
        });
    });
}

#[test]
fn test_herdr_set_pane_label_routes_to_exact_pane_without_renaming_tab() {
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            let locator = mark_first_pane_as_herdr(workspace, 0, "w1:t1", "w1:p1", ctx);
            let pane_group = workspace.tabs[0].pane_group.clone();
            let tab_display_title = pane_group.as_ref(ctx).display_title(ctx);

            workspace.herdr_set_pane_label("w1:p1", Some("reviewer"), ctx);

            assert_eq!(
                pane_group
                    .as_ref(ctx)
                    .pane_by_id(locator.pane_id)
                    .unwrap()
                    .pane_configuration()
                    .as_ref(ctx)
                    .custom_vertical_tabs_title(),
                Some("reviewer")
            );
            assert_eq!(pane_group.as_ref(ctx).display_title(ctx), tab_display_title);
        });
    });
}

#[test]
fn test_mirrored_pane_rename_sets_custom_title_without_renaming_tab() {
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            let locator = mark_first_pane_as_herdr(workspace, 0, "w1:t1", "w1:p1", ctx);
            let pane_group = workspace.tabs[0].pane_group.clone();
            let tab_display_title = pane_group.as_ref(ctx).display_title(ctx);

            workspace.set_custom_pane_name(locator, "reviewer".into(), ctx);

            let pane_custom_title = pane_group
                .as_ref(ctx)
                .pane_by_id(locator.pane_id)
                .unwrap()
                .pane_configuration()
                .as_ref(ctx)
                .custom_vertical_tabs_title();
            assert_eq!(
                (pane_custom_title, pane_group.as_ref(ctx).display_title(ctx)),
                (Some("reviewer"), tab_display_title)
            );
        });
    });
}

#[test]
fn test_mirrored_pane_clear_clears_only_the_pane_name() {
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            let locator = mark_first_pane_as_herdr(workspace, 0, "w1:t1", "w1:p1", ctx);
            let pane_group = workspace.tabs[0].pane_group.clone();
            let tab_display_title = pane_group.as_ref(ctx).display_title(ctx);

            workspace.set_custom_pane_name(locator, "reviewer".into(), ctx);
            workspace.clear_pane_name(locator, ctx);

            assert_eq!(
                pane_group
                    .as_ref(ctx)
                    .pane_by_id(locator.pane_id)
                    .unwrap()
                    .pane_configuration()
                    .as_ref(ctx)
                    .custom_vertical_tabs_title(),
                None
            );
            assert_eq!(pane_group.as_ref(ctx).display_title(ctx), tab_display_title);
        });
    });
}

#[test]
fn test_local_pane_rename_and_clear_behavior_is_unchanged() {
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            mark_herdr_tab(workspace, 0, "w1:t1");
            let pane_group = workspace.tabs[0].pane_group.clone();
            let pane_id = pane_group.as_ref(ctx).pane_ids().next().unwrap();
            let locator = PaneViewLocator {
                pane_group_id: pane_group.id(),
                pane_id,
            };
            let tab_display_title = pane_group.as_ref(ctx).display_title(ctx);

            workspace.set_custom_pane_name(locator, "local fallback".into(), ctx);
            assert_eq!(
                pane_group
                    .as_ref(ctx)
                    .pane_by_id(locator.pane_id)
                    .unwrap()
                    .pane_configuration()
                    .as_ref(ctx)
                    .custom_vertical_tabs_title(),
                Some("local fallback")
            );
            assert_eq!(pane_group.as_ref(ctx).display_title(ctx), tab_display_title);

            workspace.clear_pane_name(locator, ctx);
            assert_eq!(
                pane_group
                    .as_ref(ctx)
                    .pane_by_id(locator.pane_id)
                    .unwrap()
                    .pane_configuration()
                    .as_ref(ctx)
                    .custom_vertical_tabs_title(),
                None
            );
            assert_eq!(pane_group.as_ref(ctx).display_title(ctx), tab_display_title);
        });
    });
}

/// Cross-window transfer rebuilds TabData in the target window and would
/// sever the herdr binding, so mirrored tabs refuse the handoff and stay in
/// the window owning the connection.
#[test]
fn test_herdr_tab_refuses_cross_window_transfer() {
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            workspace.add_terminal_tab(false, ctx);
            mark_herdr_tab(workspace, 1, "w1:t1");

            assert!(workspace.get_tab_transfer_info_for_attach(0, ctx).is_some());
            assert!(workspace.get_tab_transfer_info_for_attach(1, ctx).is_none());
        });
    });
}

/// Ungrouping a herdr-backed group must drop its herdr_group_ids entry, or
/// later herdr tabs are stamped with the dead group id and render
/// mis-grouped for the rest of the attach.
#[test]
fn test_ungroup_tabs_drops_herdr_group_mapping() {
    let _grouped_tabs = FeatureFlag::GroupedTabs.override_enabled(true);
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            workspace.add_terminal_tab(false, ctx);
            mark_herdr_tab(workspace, 0, "w1:t1");
            mark_herdr_tab(workspace, 1, "w1:t2");
            let group_id = workspace.tabs[0].group_id.unwrap();

            workspace.ungroup_tabs(group_id, ctx);

            assert!(!workspace.tab_groups.contains_key(&group_id));
            assert!(workspace.herdr_group_ids.is_empty());
        });
    });
}

/// herdr renames (incl. replayed backlog events) must not move focus:
/// set_title's refocus would strand it in a background mirror pane.
#[test]
fn test_herdr_rename_tab_does_not_steal_focus() {
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            // Tab 1 mirrors a herdr tab; tab 0 (local) stays active+focused.
            workspace.add_terminal_tab(false, ctx);
            mark_herdr_tab(workspace, 1, "w1:t1");
            workspace.activate_tab_internal(0, ctx);
        });
        let focused_before =
            workspace.update(&mut app, |_w, ctx| ctx.focused_view_id(ctx.window_id()));
        workspace.update(&mut app, |workspace, ctx| {
            workspace.herdr_rename_tab("w1:t1", "renamed by herdr", ctx);
        });
        let focused_after =
            workspace.update(&mut app, |_w, ctx| ctx.focused_view_id(ctx.window_id()));
        assert_eq!(
            focused_before, focused_after,
            "renaming a background mirror tab must not move focus"
        );
        // The rename itself still lands.
        workspace.update(&mut app, |workspace, ctx| {
            let pane_group = workspace.tabs[1].pane_group.clone();
            assert_eq!(
                pane_group.as_ref(ctx).custom_title(ctx).as_deref(),
                Some("renamed by herdr")
            );
        });
    });
}

/// Detach closes every mirrored tab and drops the group mappings behind them.
/// A surviving `herdr_group_ids` entry stamps the next attach's tabs with a
/// dead group id, which renders mis-grouped for the rest of that session.
#[test]
fn test_detach_closes_mirrored_tabs_and_drops_group_mappings() {
    let _grouped_tabs = FeatureFlag::GroupedTabs.override_enabled(true);
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            // Tab 0 stays local; tabs 1 and 2 mirror one herdr workspace.
            workspace.add_terminal_tab(false, ctx);
            workspace.add_terminal_tab(false, ctx);
            mark_herdr_tab(workspace, 1, "w1:t1");
            mark_herdr_tab(workspace, 2, "w1:t2");
            let local_group = workspace.tabs[0].pane_group.id();
            let herdr_group = workspace.tabs[1].group_id.unwrap();
            attach_test_connection(workspace, ctx);

            workspace.detach_herdr_session(ctx);

            assert!(workspace.herdr_connection.is_none());
            assert_eq!(
                tab_order(workspace),
                vec![local_group],
                "only the local tab survives detach"
            );
            assert!(
                workspace.herdr_group_ids.is_empty(),
                "workspace→group mapping must not outlive the session"
            );
            assert!(!workspace.tab_groups.contains_key(&herdr_group));
        });
    });
}

/// Detaching a window whose only tab is mirrored must leave a usable local
/// tab behind rather than closing the last tab out from under the user.
#[test]
fn test_detach_replaces_a_sole_mirrored_tab_with_a_local_one() {
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            mark_herdr_tab(workspace, 0, "w1:t1");
            let mirrored_group = workspace.tabs[0].pane_group.id();
            attach_test_connection(workspace, ctx);

            workspace.detach_herdr_session(ctx);

            assert_eq!(workspace.tabs.len(), 1, "the window must not be left empty");
            assert_ne!(
                workspace.tabs[0].pane_group.id(),
                mirrored_group,
                "the mirrored tab is replaced by a fresh local one, not reused"
            );
            assert!(workspace.tabs[0].herdr_tab.is_none());
            assert!(
                workspace.tabs[0].group_id.is_none(),
                "the replacement tab must not inherit the herdr group"
            );
        });
    });
}

/// Detaching during an outage clears the forced read-only marking: with the
/// session gone the panes are plain local terminals, and leaving the flag set
/// would strand them unwritable for the rest of their life.
#[test]
fn test_detach_clears_outage_forced_read_only() {
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            workspace.add_terminal_tab(false, ctx);
            let locator = mark_first_pane_as_herdr(workspace, 1, "w1:t1", "w1:p1", ctx);
            let model = pane_terminal_model(workspace, &locator, ctx);
            attach_test_connection(workspace, ctx);

            // The connection reports an outage.
            workspace.herdr_disconnected = true;
            workspace.set_herdr_panes_forced_read_only(true, ctx);
            assert!(
                model.lock().is_read_only(),
                "a mirrored pane is read-only while the session is unreachable"
            );

            workspace.detach_herdr_session(ctx);

            assert!(!workspace.herdr_disconnected);
            assert!(
                !model.lock().is_read_only(),
                "detach must release the outage read-only marking"
            );
        });
    });
}

/// A full attach → mirrored mutations → detach → re-attach cycle must start
/// the second session from a clean slate. Event-driven state (labels, status
/// colors, tab markers) is the kind that silently survives a teardown and
/// then misattributes itself to the next session.
#[test]
fn test_reattach_after_detach_starts_from_clean_state() {
    let _grouped_tabs = FeatureFlag::GroupedTabs.override_enabled(true);
    App::test((), |mut app| async move {
        initialize_app(&mut app);
        let workspace = mock_workspace(&mut app);
        workspace.update(&mut app, |workspace, ctx| {
            workspace.add_terminal_tab(false, ctx);
            mark_herdr_tab(workspace, 1, "w1:t1");
            attach_test_connection(workspace, ctx);

            // Herdr-side events reach the Workspace as plain method calls.
            workspace.herdr_rename_tab("w1:t1", "named by herdr", ctx);
            workspace.herdr_set_tab_status(
                "w1:t1",
                crate::herdr::protocol::AgentStatus::Working,
                ctx,
            );
            assert!(
                workspace.tabs[1].herdr_status_color.is_some(),
                "status color seeded, so its teardown is worth asserting"
            );

            workspace.detach_herdr_session(ctx);
            assert!(workspace.herdr_connection.is_none());

            attach_test_connection(workspace, ctx);

            assert!(workspace.herdr_group_ids.is_empty());
            assert!(
                workspace.tabs.iter().all(|tab| tab.herdr_tab.is_none()
                    && tab.herdr_workspace.is_none()
                    && tab.herdr_status_color.is_none()),
                "no tab may still look herdr-backed at the second attach"
            );
        });
    });
}
