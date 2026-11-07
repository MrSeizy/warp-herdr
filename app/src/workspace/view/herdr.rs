//! herdr control mode: the Workspace side of mirroring a herdr session.
//!
//! Pure code motion out of `workspace/view.rs` — attach/detach, the
//! materialization brackets, tab/pane mirroring, and the connection
//! toasts. Lives here as a child module so it can still reach the
//! parent's private `Workspace` internals.

use super::*;

/// `object_id` of the persistent toast shown while an attached herdr session
/// is unreachable. Keying the toast by `object_id` makes re-showing it
/// idempotent (the stack auto-dismisses the stale copy) and lets the
/// reconnect/detach paths remove it by identifier.
const HERDR_DISCONNECT_TOAST_ID: &str = "herdr_disconnect_toast";

/// Layout-refresh fetch attempts before giving up: transient RPC failures
/// get a short bounded retry inside the background future; permanent
/// outcomes (tab vanished, malformed export) stop on first sight.
const HERDR_LAYOUT_REFRESH_MAX_ATTEMPTS: u32 = 3;

/// Delay between layout-refresh attempts (runs on the background executor).
const HERDR_LAYOUT_REFRESH_RETRY_DELAY: Duration = Duration::from_millis(250);

/// How one layout-refresh fetch attempt failed. Only `Transport` is worth
/// retrying: a vanished tab is owned by the TabClosed/tombstone path, and
/// re-fetching a malformed export would return the same malformed layout.
enum HerdrLayoutRefreshError {
    /// The layout.export / session.snapshot RPC itself failed.
    Transport(anyhow::Error),
    /// The tab no longer appears in the session snapshot.
    Vanished,
    /// HerdrPaneTree::from_layout rejected the exported layout.
    Malformed(anyhow::Error),
}

/// One layout-refresh fetch: the tab's exported layout plus its TabInfo
/// from a fresh session snapshot (apply_layout materializes from TabInfo).
fn herdr_fetch_refreshed_layout(
    api: &crate::herdr::socket::HerdrApi,
    tab_id: &str,
) -> Result<
    (
        crate::herdr::protocol::TabInfo,
        crate::herdr::layout_convert::HerdrPaneTree,
    ),
    HerdrLayoutRefreshError,
> {
    let layout = api
        .layout_export(tab_id)
        .map_err(HerdrLayoutRefreshError::Transport)?;
    let snapshot = api
        .session_snapshot()
        .map_err(HerdrLayoutRefreshError::Transport)?;
    let tab = snapshot
        .tabs
        .into_iter()
        .find(|t| t.tab_id == tab_id)
        .ok_or(HerdrLayoutRefreshError::Vanished)?;
    let tree = crate::herdr::layout_convert::HerdrPaneTree::from_layout(&layout.root)
        .map_err(HerdrLayoutRefreshError::Malformed)?;
    Ok((tab, tree))
}

/// Pure mapping for Workspace::herdr_apply_tab_order: given the herdr tab ids
/// of a group run in their CURRENT order (`run_ids`; None for non-mirrored
/// tabs sharing the run) and herdr's full post-move order, returns the desired
/// run order as indices into `run_ids` — tabs named by herdr's list first, in
/// that order, then the remaining run tabs in their current relative order.
/// Ids not present in the run (unknown, or still awaiting their first layout)
/// are skipped; duplicates collapse to the first match.
pub(super) fn herdr_run_target_order(
    run_ids: &[Option<&str>],
    ordered_tab_ids: &[String],
) -> Vec<usize> {
    let mut placed = vec![false; run_ids.len()];
    let mut target = Vec::with_capacity(run_ids.len());
    for id in ordered_tab_ids {
        if let Some(i) = run_ids.iter().position(|rid| *rid == Some(id.as_str()))
            && !placed[i]
        {
            placed[i] = true;
            target.push(i);
        }
    }
    target.extend((0..run_ids.len()).filter(|i| !placed[*i]));
    target
}

impl Workspace {
    /// Runs `f` against the herdr connection after the current update cycle
    /// completes. Workspace-side interceptions can fire while the connection
    /// model is itself mid-update (reconciler ops apply inside it), and
    /// updating it synchronously then panics with "Circular model update".
    pub(super) fn with_herdr_connection(
        &mut self,
        ctx: &mut ViewContext<Self>,
        f: impl FnOnce(
            &mut crate::herdr::connection::HerdrConnection,
            &mut warpui::ModelContext<crate::herdr::connection::HerdrConnection>,
        ) + Send
        + 'static,
    ) {
        let Some(conn) = self.herdr_connection.clone() else {
            return;
        };
        ctx.spawn(async {}, move |_this: &mut Self, _, ctx| {
            conn.update(ctx, f);
        });
    }

    /// Ensures a local tab group mirrors the given herdr workspace.
    pub fn herdr_ensure_tab_group(
        &mut self,
        herdr_ws_id: &str,
        label: &str,
        ctx: &mut ViewContext<Self>,
    ) {
        if !FeatureFlag::GroupedTabs.is_enabled() {
            return;
        }
        // The mapping can dangle when the group was removed outside of detach
        // (e.g. the whole group was closed); recreate the group in that case.
        if let Some(group_id) = self.herdr_group_ids.get(herdr_ws_id)
            && self.tab_groups.contains_key(group_id)
        {
            return;
        }
        let mut group = TabGroup::new();
        group.name = Some(format!("herdr: {label}"));
        let group_id = group.id;
        self.tab_groups.insert(group_id, group);
        self.herdr_group_ids
            .insert(herdr_ws_id.to_string(), group_id);
        ctx.notify();
    }

    /// Adds a local tab mirroring a herdr tab with the given split layout.
    pub fn herdr_add_tab(
        &mut self,
        tab: &crate::herdr::protocol::TabInfo,
        layout: &crate::herdr::layout_convert::HerdrPaneTree,
        base_spec: &crate::terminal::herdr_tty::stream::HerdrStreamSpec,
        pane_closed_tx: async_channel::Sender<(String, Option<String>)>,
        ctx: &mut ViewContext<Self>,
    ) {
        // DECISION RULE: defer spawning each pane's herdr stream subprocess
        // unless the tab being added will be the window's ACTIVE tab.
        // add_tab_with_pane_layout always activates the tab it adds, but the
        // materializing block below flips activation back to the
        // pre-materialization tab, and the reconciler wraps every create_tab
        // (snapshot or live event) in set_materializing — so herdr tabs land
        // inactive and defer, attaching lazily on first view via
        // activate_tab_internal. An add outside materialization (the tab
        // lands active) spawns eagerly.
        let defer_stream = self.herdr_materializing();
        let panes_layout = PanesLayout::Herdr(Box::new(crate::pane_group::HerdrPanesLayout {
            tree: layout.clone(),
            base_spec: base_spec.clone(),
            on_pane_closed: pane_closed_tx,
            defer_stream,
        }));
        let title = (!tab.label.is_empty()).then(|| tab.label.clone());
        self.add_tab_with_pane_layout(panes_layout, Arc::new(HashMap::new()), title, ctx);
        // add_tab_with_pane_layout activates the new tab.
        let idx = self.active_tab_index();
        self.tabs[idx].herdr_tab = Some(tab.tab_id.clone());
        self.tabs[idx].herdr_workspace = Some(tab.workspace_id.clone());
        if let Some(group_id) = self.herdr_group_ids.get(&tab.workspace_id).copied() {
            // Pull the tab to the end of its group's run so group membership
            // stays contiguous, and expand the group so the tab is visible.
            let target_index = self.index_after_group(group_id).unwrap_or(self.tabs.len());
            self.tabs[idx].group_id = Some(group_id);
            self.move_tab_to_index(idx, target_index, ctx);
            self.expand_tab_group(group_id, ctx);
        }
        if self.herdr_materializing() {
            // Bulk materialization (snapshot/layout application) must not
            // steal activation; flip back to the pre-materialization tab.
            let restore_idx = self.herdr_materializing_active_tab.and_then(|active_id| {
                self.tabs
                    .iter()
                    .position(|t| t.pane_group.id() == active_id)
            });
            match restore_idx {
                Some(active_idx) => self.activate_tab_internal(active_idx, ctx),
                None => {
                    // The captured tab was closed inside this bracket (the
                    // resync rebuild tears down every stale tab): adopt this
                    // newly materialized tab — active right now — as the
                    // restore target so the rest of the bracket flips back
                    // to a tab that exists.
                    self.herdr_materializing_active_tab = self
                        .tabs
                        .get(self.active_tab_index)
                        .map(|t| t.pane_group.id());
                }
            }
        }
        if self.herdr_disconnected {
            // A tab materialized mid-outage inherits the read-only marking.
            self.set_herdr_panes_forced_read_only(true, ctx);
        }
        ctx.notify();
    }

    /// Opens (or closes) one herdr materialization bracket. Brackets nest —
    /// the resync rebuild wraps the reconciler's per-tab brackets — and the
    /// guard stays active until the outermost bracket closes. While active,
    /// `herdr_add_tab` keeps the tab that was active at the outermost start
    /// active instead of activating each materialized tab, so ending
    /// materialization leaves that tab in place.
    pub fn set_herdr_materializing(&mut self, active: bool) {
        if active {
            self.herdr_materializing_depth += 1;
            if self.herdr_materializing_depth == 1 {
                self.herdr_materializing_active_tab = self
                    .tabs
                    .get(self.active_tab_index)
                    .map(|t| t.pane_group.id());
            }
        } else {
            self.herdr_materializing_depth = self.herdr_materializing_depth.saturating_sub(1);
            if self.herdr_materializing_depth == 0 {
                self.herdr_materializing_active_tab = None;
            }
        }
    }

    pub(super) fn herdr_materializing(&self) -> bool {
        self.herdr_materializing_depth > 0
    }

    /// Attaches any deferred herdr pane streams in the active tab. The resync
    /// rebuild can leave a freshly materialized (deferred-stream) tab active
    /// without a real activation ever firing, so the first-view attach
    /// trigger in `activate_tab_internal` would never run for it and the tab
    /// would render an empty grid until the user switched away and back.
    pub fn herdr_attach_active_tab_streams(&mut self, ctx: &mut ViewContext<Self>) {
        let idx = self.active_tab_index();
        let Some(tab) = self.tabs.get(idx) else {
            return;
        };
        if tab.herdr_tab.is_none() {
            return;
        }
        let pane_group = tab.pane_group.clone();
        pane_group.update(ctx, |pg, ctx| pg.attach_pending_herdr_streams(ctx));
    }

    fn herdr_tab_index(&self, herdr_tab_id: &str) -> Option<usize> {
        self.tabs
            .iter()
            .position(|t| t.herdr_tab.as_deref() == Some(herdr_tab_id))
    }

    pub fn herdr_close_tab(&mut self, herdr_tab_id: &str, ctx: &mut ViewContext<Self>) {
        let Some(idx) = self.herdr_tab_index(herdr_tab_id) else {
            return;
        };
        // Clear the marker up front so close_tabs does not echo the close back
        // to herdr: herdr already closed this tab, and a deferred echo could
        // kill a live tab that re-used the id after a resync.
        self.tabs[idx].herdr_tab = None;
        self.tabs[idx].herdr_workspace = None;
        if self.tabs.len() == 1 {
            self.replace_sole_tab_with_local(idx, ctx);
            return;
        }
        // herdr already closed it; skip confirmation, skip undo stack, and
        // go through close_tabs directly — the close_tab wrapper's CloseTab
        // telemetry and save_app dispatch are user-intent signals a
        // machine-driven close must not emit (remove_tab persists on its
        // own).
        self.close_tabs(
            std::iter::once(idx),
            OpenDialogSource::CloseTab { tab_index: idx },
            true,  // skip_confirmation
            false, // add_to_undo_stack
            ctx,
        );
    }

    /// Closes the herdr tab at `index` when it is the only tab left: closing
    /// it outright would close the whole window (or be refused when the
    /// CloseWindow context is disabled), so a fresh local tab takes its place
    /// first.
    fn replace_sole_tab_with_local(&mut self, index: usize, ctx: &mut ViewContext<Self>) {
        let pane_group_id = self.tabs[index].pane_group.id();
        self.add_new_session_tab_with_default_mode(
            NewSessionSource::Tab,
            Some(ctx.window_id()),
            None,
            None,
            false,
            ctx,
        );
        // Keep the fresh tab out of the mirrored tab's (herdr) group.
        let new_idx = self.active_tab_index();
        if let Some(new_tab) = self.tabs.get_mut(new_idx) {
            new_tab.group_id = None;
        }
        // The fresh tab may have been inserted before `index`; re-locate the
        // mirrored tab before closing it.
        if let Some(idx) = self
            .tabs
            .iter()
            .position(|t| t.pane_group.id() == pane_group_id)
        {
            self.close_tab(idx, true, false, ctx);
        }
    }

    pub fn herdr_rename_tab(
        &mut self,
        herdr_tab_id: &str,
        label: &str,
        ctx: &mut ViewContext<Self>,
    ) {
        if let Some(idx) = self.herdr_tab_index(herdr_tab_id) {
            let pane_group = self.tabs[idx].pane_group.clone();
            pane_group.update(ctx, |pg, ctx| pg.set_title_preserving_focus(label, ctx));
            // sidebar renders from the Workspace view
            ctx.notify();
        }
    }

    /// Reorders the mirrored tabs of herdr workspace `workspace_id` to match
    /// herdr's post-move order (`ordered_tab_ids` is the workspace's full tab
    /// order, so applying it also heals a missed move). The reorder stays
    /// within the group's contiguous run: ids not materialized locally are
    /// skipped, and mirrored tabs missing from the list keep their relative
    /// order at the end of the run. Bracketed by herdr_applying_remote_order
    /// so the moves don't echo back to herdr via the move interception; the
    /// active tab is preserved across the reorder.
    pub fn herdr_apply_tab_order(
        &mut self,
        workspace_id: &str,
        ordered_tab_ids: &[String],
        ctx: &mut ViewContext<Self>,
    ) {
        let Some(group_id) = self.herdr_group_ids.get(workspace_id).copied() else {
            log::debug!("herdr: apply_tab_order for unknown workspace {workspace_id}");
            return;
        };
        let Some((first, last)) = group_member_index_range(&self.tabs, group_id) else {
            return; // no mirrored tabs (the empty group survives while attached)
        };
        // Desired run order by stable tab identity, so the moves below can
        // re-locate each tab as earlier ones shift positions.
        let desired: Vec<EntityId> = {
            let run = &self.tabs[first..=last];
            let run_ids: Vec<Option<&str>> =
                run.iter().map(|tab| tab.herdr_tab.as_deref()).collect();
            herdr_run_target_order(&run_ids, ordered_tab_ids)
                .into_iter()
                .map(|i| run[i].pane_group.id())
                .collect()
        };
        // Selection-sort pass: after step i the run's first i slots hold their
        // final tabs, so the next desired tab is always at or after its slot.
        self.herdr_applying_remote_order = true;
        for (i, wanted) in desired.iter().enumerate() {
            let slot = first + i;
            let Some(current) = self
                .tabs
                .iter()
                .position(|tab| tab.pane_group.id() == *wanted)
            else {
                continue;
            };
            if current != slot {
                self.move_tab_to_index(current, slot, ctx);
            }
        }
        self.herdr_applying_remote_order = false;
    }

    pub fn herdr_set_tab_status(
        &mut self,
        herdr_tab_id: &str,
        status: AgentStatus,
        ctx: &mut ViewContext<Self>,
    ) {
        let Some(idx) = self.herdr_tab_index(herdr_tab_id) else {
            return;
        };
        let color = match status {
            AgentStatus::Working => Some(AnsiColorIdentifier::Blue),
            AgentStatus::Blocked => Some(AnsiColorIdentifier::Red),
            AgentStatus::Done => Some(AnsiColorIdentifier::Green),
            AgentStatus::Idle | AgentStatus::Unknown => None,
        };
        // Rendered via TabData::herdr_status_color — layered above the user's
        // manual color without ever overwriting it, and set directly rather
        // than via set_tab_color so machine-driven status flips emit no
        // user-intent TabOperations telemetry.
        if self.tabs[idx].herdr_status_color == color {
            return;
        }
        self.tabs[idx].herdr_status_color = color;
        ctx.notify();
    }

    pub fn herdr_add_split(
        &mut self,
        herdr_tab_id: &str,
        pane: &crate::herdr::protocol::PaneInfo,
        base_spec: &crate::terminal::herdr_tty::stream::HerdrStreamSpec,
        pane_closed_tx: async_channel::Sender<(String, Option<String>)>,
        ctx: &mut ViewContext<Self>,
    ) {
        let Some(idx) = self.herdr_tab_index(herdr_tab_id) else {
            return;
        };
        // DECISION RULE (same as herdr_add_tab): defer the new pane's stream
        // subprocess unless the tab it splits into is the window's ACTIVE
        // tab; a split landing in a background tab attaches on first view.
        let defer_stream = self.herdr_materializing() || idx != self.active_tab_index();
        let pane_group = self.tabs[idx].pane_group.clone();
        let pane_id = pane.pane_id.clone();
        let cwd = pane.cwd.clone();
        let agent = pane.agent.clone();
        let agent_status = pane.agent_status;
        pane_group.update(ctx, |pg, ctx| {
            pg.herdr_split_pane(
                &pane_id,
                cwd,
                agent,
                agent_status,
                base_spec,
                &pane_closed_tx,
                defer_stream,
                ctx,
            );
        });
        if self.herdr_disconnected {
            // A pane materialized mid-outage inherits the read-only marking.
            self.set_herdr_panes_forced_read_only(true, ctx);
        }
    }

    /// Applies a refreshed herdr layout (direction/ratio/structure) to a live
    /// mirrored tab. An unknown tab — it may have closed while the refresh
    /// was in flight — is a no-op.
    pub fn herdr_sync_tab_layout(
        &mut self,
        herdr_tab_id: &str,
        tree: &crate::herdr::layout_convert::HerdrPaneTree,
        base_spec: &crate::terminal::herdr_tty::stream::HerdrStreamSpec,
        on_pane_closed: &async_channel::Sender<(String, Option<String>)>,
        ctx: &mut ViewContext<Self>,
    ) {
        let Some(idx) = self.herdr_tab_index(herdr_tab_id) else {
            log::debug!("herdr: sync_tab_layout: unknown tab {herdr_tab_id}; ignoring");
            return;
        };
        // DECISION RULE (same as herdr_add_tab): panes first seen in this
        // refresh materialize now; defer their stream subprocess unless the
        // tab being synced is the window's ACTIVE tab.
        let defer_stream = self.herdr_materializing() || idx != self.active_tab_index();
        let pane_group = self.tabs[idx].pane_group.clone();
        pane_group.update(ctx, |pg, ctx| {
            pg.sync_herdr_layout(tree, base_spec, on_pane_closed, defer_stream, ctx);
        });
        if self.herdr_disconnected {
            // Panes first materialized by this sync inherit the read-only
            // outage marking.
            self.set_herdr_panes_forced_read_only(true, ctx);
        }
    }

    /// Finds the mirrored tab hosting herdr pane `herdr_pane_id`: the tab's
    /// index and its pane group. Callers pick their own miss handling —
    /// `herdr_close_pane`'s silent no-op is deliberate echo-safety, while
    /// the setters warn.
    fn herdr_pane_host(
        &self,
        herdr_pane_id: &str,
        ctx: &ViewContext<Self>,
    ) -> Option<(usize, ViewHandle<PaneGroup>)> {
        self.tabs.iter().enumerate().find_map(|(idx, tab)| {
            tab.herdr_tab.as_ref()?;
            let pane_group = tab.pane_group.clone();
            pane_group
                .as_ref(ctx)
                .herdr_has_pane(herdr_pane_id)
                .then_some((idx, pane_group))
        })
    }

    pub fn herdr_close_pane(&mut self, herdr_pane_id: &str, ctx: &mut ViewContext<Self>) {
        // No-op on a miss: herdr-initiated closes funnel back through here
        // after the reconciler already forgot the pane.
        let Some((idx, pane_group)) = self.herdr_pane_host(herdr_pane_id, ctx) else {
            return;
        };
        if pane_group.as_ref(ctx).pane_count() == 1 {
            // herdr closing the tab's last pane empties the tab: the
            // Exited cascade closes the local tab, and clearing the
            // marker first keeps close_tabs from echoing a spurious
            // tab.close back to herdr (herdr's own tab.closed event
            // follows on its own).
            self.tabs[idx].herdr_tab = None;
            self.tabs[idx].herdr_workspace = None;
        }
        pane_group.update(ctx, |pg, ctx| pg.herdr_close_pane(herdr_pane_id, ctx));
    }

    /// Restarts the dead stream subprocess of the pane mirroring
    /// `herdr_pane_id` in whichever mirrored tab hosts it. False when no
    /// mirrored tab tracks the pane (or the pane gave up restarting) — the
    /// caller then closes the pane instead.
    pub fn herdr_restart_stream(
        &mut self,
        herdr_pane_id: &str,
        ctx: &mut ViewContext<Self>,
    ) -> bool {
        let Some((_, pane_group)) = self.herdr_pane_host(herdr_pane_id, ctx) else {
            return false;
        };
        pane_group.update(ctx, |pg, ctx| pg.restart_herdr_stream(herdr_pane_id, ctx))
    }

    pub fn herdr_set_pane_title(
        &mut self,
        herdr_pane_id: &str,
        title: &str,
        ctx: &mut ViewContext<Self>,
    ) {
        let Some((_, pane_group)) = self.herdr_pane_host(herdr_pane_id, ctx) else {
            log::warn!("herdr: set_pane_title: no tab hosts pane {herdr_pane_id}");
            return;
        };
        pane_group.update(ctx, |pg, ctx| {
            pg.herdr_set_pane_title(herdr_pane_id, title, ctx)
        });
    }

    pub fn herdr_set_pane_label(
        &mut self,
        herdr_pane_id: &str,
        label: Option<&str>,
        ctx: &mut ViewContext<Self>,
    ) {
        let Some((_, pane_group)) = self.herdr_pane_host(herdr_pane_id, ctx) else {
            log::warn!("herdr: set_pane_label: no tab hosts pane {herdr_pane_id}");
            return;
        };
        pane_group.update(ctx, |pg, ctx| {
            pg.herdr_set_pane_label(herdr_pane_id, label, ctx)
        });
        ctx.notify();
    }

    pub fn herdr_set_pane_agent(
        &mut self,
        herdr_pane_id: &str,
        agent: Option<&str>,
        ctx: &mut ViewContext<Self>,
    ) {
        let Some((_, pane_group)) = self.herdr_pane_host(herdr_pane_id, ctx) else {
            log::warn!("herdr: set_pane_agent: no tab hosts pane {herdr_pane_id}");
            return;
        };
        pane_group.update(ctx, |pg, ctx| {
            pg.herdr_set_pane_agent(herdr_pane_id, agent, ctx)
        });
        // The sidebar renders from the Workspace view; without this the
        // icon change waits for an unrelated redraw (e.g. focus).
        ctx.notify();
    }

    /// Routes a herdr-reported agent status change for `herdr_pane_id` to the
    /// mirrored pane's terminal view, which drives the pane's live status
    /// indicator.
    pub fn herdr_set_pane_agent_status(
        &mut self,
        herdr_pane_id: &str,
        status: AgentStatus,
        ctx: &mut ViewContext<Self>,
    ) {
        let Some((_, pane_group)) = self.herdr_pane_host(herdr_pane_id, ctx) else {
            log::warn!("herdr: set_pane_agent_status: no tab hosts pane {herdr_pane_id}");
            return;
        };
        let view = {
            let pg = pane_group.as_ref(ctx);
            pg.pane_ids()
                .find(|pane_id| pg.herdr_pane_id_of(*pane_id) == Some(herdr_pane_id))
                .and_then(|pane_id| pg.terminal_view_from_pane_id(pane_id, ctx))
        };
        let Some(view) = view else {
            log::debug!("herdr: set_pane_agent_status: pane {herdr_pane_id} has no terminal view");
            return;
        };
        view.update(ctx, |view, ctx| {
            view.sync_herdr_cli_agent_session_status(status, ctx)
        });
    }

    /// Sets the forced read-only override on every pane mirrored from herdr.
    ///
    /// Outage UX: while the herdr connection is down the disconnect banner
    /// shows AND mirrored panes are read-only — herdr owns their input, and
    /// with the connection down anything typed would go nowhere. Input gates
    /// already consult `TerminalModel::is_read_only`, which folds in this
    /// override, so no separate gating is needed. Panes materialized
    /// mid-outage are marked by the herdr_add_tab / herdr_add_split /
    /// herdr_sync_tab_layout callers.
    pub(super) fn set_herdr_panes_forced_read_only(
        &mut self,
        read_only: bool,
        ctx: &mut ViewContext<Self>,
    ) {
        for tab in &self.tabs {
            if tab.herdr_tab.is_none() {
                continue;
            }
            let pane_group = tab.pane_group.clone();
            let pane_ids: Vec<_> = {
                let pg = pane_group.as_ref(ctx);
                // Only panes registered as herdr mirrors: the local-shell
                // fallback pane in a mirrored tab is the user's own shell
                // and must stay writable.
                pg.pane_ids()
                    .filter(|pane_id| pg.herdr_pane_id_of(*pane_id).is_some())
                    .collect()
            };
            for pane_id in pane_ids {
                if let Some(view) = pane_group
                    .as_ref(ctx)
                    .terminal_view_from_pane_id(pane_id, ctx)
                {
                    view.update(ctx, |view, ctx| {
                        view.model.lock().set_forced_read_only(read_only);
                        // Repaint now: input-box visibility reads this flag,
                        // and without a notify it can stay visible while
                        // keystrokes drop (or stay hidden after reconnect).
                        ctx.notify();
                    });
                }
            }
        }
    }

    /// Re-exports a tab's layout in the background and applies it through
    /// the connection's reconciler once it arrives. Transient RPC failures
    /// retry a bounded number of times inside the background future (a
    /// dropped refresh leaves the tab unmaterialized until resync — the
    /// reconciler separately re-requests when later events for the tab
    /// arrive); permanent outcomes (the tab vanished, or herdr exported a
    /// malformed layout) are not retried.
    pub fn herdr_request_layout_refresh(
        &mut self,
        api: crate::herdr::socket::HerdrApi,
        tab_id: String,
        ctx: &mut ViewContext<Self>,
    ) {
        let Some(conn) = self.herdr_connection.clone() else {
            return;
        };
        ctx.spawn(
            async move {
                let mut attempt = 1;
                loop {
                    match herdr_fetch_refreshed_layout(&api, &tab_id) {
                        Ok(refreshed) => break anyhow::Ok(refreshed),
                        Err(HerdrLayoutRefreshError::Transport(err)) => {
                            if attempt == HERDR_LAYOUT_REFRESH_MAX_ATTEMPTS {
                                break Err(err.context(format!(
                                    "giving up on tab {tab_id} after {HERDR_LAYOUT_REFRESH_MAX_ATTEMPTS} attempts"
                                )));
                            }
                            log::debug!(
                                "herdr: layout refresh for tab {tab_id} attempt {attempt}/{HERDR_LAYOUT_REFRESH_MAX_ATTEMPTS} failed: {err:#}; retrying"
                            );
                            attempt += 1;
                            warpui::r#async::Timer::after(HERDR_LAYOUT_REFRESH_RETRY_DELAY).await;
                        }
                        Err(HerdrLayoutRefreshError::Vanished) => {
                            // Permanent: the tab closed before or during the
                            // fetch; the TabClosed/tombstone path owns cleanup.
                            break Err(anyhow::anyhow!(
                                "herdr tab {tab_id} vanished (attempt {attempt}/{HERDR_LAYOUT_REFRESH_MAX_ATTEMPTS})"
                            ));
                        }
                        Err(HerdrLayoutRefreshError::Malformed(err)) => {
                            // Permanent, mirroring bootstrap's tolerance: warn
                            // once and stop rather than re-fetch the same
                            // malformed layout.
                            break Err(err.context(format!(
                                "malformed layout for tab {tab_id} (attempt {attempt}/{HERDR_LAYOUT_REFRESH_MAX_ATTEMPTS})"
                            )));
                        }
                    }
                }
            },
            move |_this: &mut Self, result, ctx| match result {
                Ok((tab, tree)) => conn.update(ctx, |conn, ctx| {
                    conn.apply_refreshed_layout(&tab, &tree, ctx);
                }),
                Err(err) => log::warn!("herdr layout refresh failed: {err:#}"),
            },
        );
    }

    pub fn attach_herdr_session(
        &mut self,
        session_name: String,
        socket_path: PathBuf,
        ctx: &mut ViewContext<Self>,
    ) {
        if !FeatureFlag::HerdrControlMode.is_enabled() {
            log::warn!("herdr: attach attempted with HerdrControlMode disabled");
            self.show_herdr_error_toast("herdr control mode is not enabled".to_string(), ctx);
            return;
        }
        if self.herdr_connection.is_some() {
            log::warn!("herdr: already attached in this window");
            self.show_herdr_error_toast(
                "A herdr session is already attached in this window".to_string(),
                ctx,
            );
            return;
        }
        if self.herdr_attach_in_progress {
            self.show_herdr_error_toast("A herdr attach is already in progress".to_string(), ctx);
            return;
        }
        self.herdr_attach_in_progress = true;
        log::info!("herdr: attaching session '{session_name}' at {socket_path:?}");
        let api = crate::herdr::socket::HerdrApi::at(socket_path.clone());
        let workspace = ctx.handle();
        ctx.spawn(
            async move { crate::herdr::connection::HerdrConnection::bootstrap_subscribed(&api) },
            move |this: &mut Self, result, ctx| {
                this.herdr_attach_in_progress = false;
                match result {
                    Ok(bootstrap) => {
                        let tab_count = bootstrap.snapshot.tabs.len();
                        let pane_count = bootstrap.snapshot.panes.len();
                        log::info!(
                            "herdr: bootstrap ok — {} workspaces, {} tabs, {} layouts",
                            bootstrap.snapshot.workspaces.len(),
                            tab_count,
                            bootstrap.layouts.len()
                        );
                        let conn = ctx.add_model(|ctx| {
                            crate::herdr::connection::HerdrConnection::start(
                                session_name,
                                socket_path,
                                bootstrap,
                                workspace,
                                ctx,
                            )
                        });
                        ctx.subscribe_to_model(&conn, |me, _, event, ctx| {
                            // Detach drops the connection while queued events
                            // can still drain; with no session there is
                            // nothing to report.
                            if me.herdr_connection.is_none() {
                                return;
                            }
                            match event {
                                HerdrConnectionEvent::Disconnected => {
                                    // Outage UX: alongside the banner,
                                    // mirrored panes go read-only.
                                    me.herdr_disconnected = true;
                                    me.set_herdr_panes_forced_read_only(true, ctx);
                                    me.show_herdr_disconnect_toast(ctx);
                                }
                                HerdrConnectionEvent::Reconnected => {
                                    me.herdr_disconnected = false;
                                    me.set_herdr_panes_forced_read_only(false, ctx);
                                    me.show_herdr_reconnected_toast(ctx);
                                }
                                HerdrConnectionEvent::ResyncFailed { message } => {
                                    // Same object id as the disconnect banner
                                    // so the actionable reason replaces
                                    // "reconnecting…" instead of stacking; a
                                    // later Reconnected dismisses it.
                                    me.show_herdr_resync_failed_toast(message.clone(), ctx);
                                }
                            }
                        });
                        this.herdr_connection = Some(conn);
                        send_telemetry_from_ctx!(
                            TelemetryEvent::HerdrSessionAttached {
                                tab_count,
                                pane_count,
                            },
                            ctx
                        );
                        ctx.notify();
                    }
                    Err(err) => {
                        let message = format!("Failed to attach herdr session: {err}");
                        report_error!(err.context("herdr attach failed"));
                        this.show_herdr_error_toast(message, ctx);
                    }
                }
            },
        );
    }

    /// Persistent, replacing (same object id as the disconnect banner): a
    /// resync that keeps failing — e.g. the server came back below the
    /// minimum supported version — surfaces its reason instead of retrying
    /// forever with the error only in logs.
    fn show_herdr_resync_failed_toast(&mut self, message: String, ctx: &mut ViewContext<Self>) {
        self.toast_stack.update(ctx, |toast_stack, ctx| {
            toast_stack.add_persistent_toast(
                DismissibleToast::error(message)
                    .with_object_id(HERDR_DISCONNECT_TOAST_ID.to_string()),
                ctx,
            );
        });
    }

    /// Surfaces a herdr failure to the user as an ephemeral error toast.
    fn show_herdr_error_toast(&mut self, message: String, ctx: &mut ViewContext<Self>) {
        self.toast_stack.update(ctx, |toast_stack, ctx| {
            toast_stack.add_ephemeral_toast(DismissibleToast::error(message), ctx);
        });
    }

    /// Shows the persistent disconnect banner while the attached herdr
    /// session is unreachable. The connection retries on its own, so the
    /// neutral flavor fits better than an error; the `object_id` key makes
    /// re-showing replace the existing banner instead of stacking a second.
    fn show_herdr_disconnect_toast(&mut self, ctx: &mut ViewContext<Self>) {
        self.toast_stack.update(ctx, |toast_stack, ctx| {
            toast_stack.add_persistent_toast(
                DismissibleToast::default("herdr disconnected — reconnecting…".to_string())
                    .with_object_id(HERDR_DISCONNECT_TOAST_ID.to_string()),
                ctx,
            );
        });
    }

    /// Clears the disconnect banner and confirms the recovery ephemerally.
    fn show_herdr_reconnected_toast(&mut self, ctx: &mut ViewContext<Self>) {
        self.dismiss_herdr_disconnect_toast(ctx);
        self.toast_stack.update(ctx, |toast_stack, ctx| {
            toast_stack.add_ephemeral_toast(
                DismissibleToast::success("herdr reconnected".to_string()),
                ctx,
            );
        });
    }

    /// Removes the disconnect banner if it is showing.
    fn dismiss_herdr_disconnect_toast(&mut self, ctx: &mut ViewContext<Self>) {
        self.toast_stack.update(ctx, |toast_stack, ctx| {
            toast_stack.dismiss_older_toasts(HERDR_DISCONNECT_TOAST_ID, ctx);
        });
    }

    pub fn attach_herdr_default(&mut self, ctx: &mut ViewContext<Self>) {
        let sessions = crate::herdr::socket::discover_sessions();
        match sessions.len() {
            0 => {
                log::warn!("herdr: no running sessions found (is `herdr` running?)");
                self.show_herdr_error_toast(
                    "No running herdr sessions found (is `herdr` running?)".to_string(),
                    ctx,
                );
            }
            1 => {
                let s = &sessions[0];
                self.attach_herdr_session(s.name.clone(), s.socket_path.clone(), ctx);
            }
            _ => {
                // v1: attach the default session if present, else the first.
                let s = sessions
                    .iter()
                    .find(|s| s.name == "default")
                    .unwrap_or(&sessions[0]);
                log::info!("herdr: multiple sessions; attaching '{}'", s.name);
                self.attach_herdr_session(s.name.clone(), s.socket_path.clone(), ctx);
            }
        }
    }

    pub fn detach_herdr_session(&mut self, ctx: &mut ViewContext<Self>) {
        let Some(conn) = self.herdr_connection.clone() else {
            self.show_herdr_error_toast("No herdr session is attached".to_string(), ctx);
            return;
        };
        // Invalidate in-flight deferred work (rename/close/focus/layout
        // refresh) before tearing down; after detach herdr keeps its tabs.
        conn.update(ctx, |conn, _| conn.mark_detached());
        self.herdr_connection = None;
        send_telemetry_from_ctx!(TelemetryEvent::HerdrSessionDetached, ctx);
        self.dismiss_herdr_disconnect_toast(ctx);
        // Clear the outage marking: with the session gone, mirrored panes are
        // plain local terminals and must not stay read-only.
        self.herdr_disconnected = false;
        self.set_herdr_panes_forced_read_only(false, ctx);
        // Close all herdr tabs locally WITHOUT herdr-side close (streams release
        // via TerminalManager Shutdown when panes drop).
        let herdr_tabs: Vec<usize> = (0..self.tabs.len())
            .rev()
            .filter(|i| self.tabs[*i].herdr_tab.is_some())
            .collect();
        for idx in herdr_tabs {
            if self.tabs.len() == 1 {
                self.replace_sole_tab_with_local(idx, ctx);
            } else {
                self.close_tab(idx, true, false, ctx);
            }
        }
        // Strip the marker from any herdr tab that could not be closed: with
        // the connection gone it is a plain local tab.
        for tab in &mut self.tabs {
            tab.herdr_tab = None;
            tab.herdr_workspace = None;
            tab.herdr_status_color = None;
        }
        // Drop empty herdr groups:
        let group_ids: Vec<TabGroupId> = self.herdr_group_ids.drain().map(|(_, id)| id).collect();
        for gid in group_ids {
            if !self.tabs.iter().any(|t| t.group_id == Some(gid)) {
                self.tab_groups.remove(&gid);
            }
        }
        ctx.notify();
    }
}
