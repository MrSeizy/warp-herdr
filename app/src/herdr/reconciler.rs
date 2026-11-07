//! Pure herdr↔Warp sync state machine. All effects go through WorkspaceOps.
//! Must stay idempotent: herdr replays up to 512 buffered events on subscribe.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use instant::{Duration, Instant};

use super::layout_convert::HerdrPaneTree;
use super::protocol::{
    AgentStatus, AgentStatusEvent, EventData, PaneInfo, SessionSnapshot, TabInfo, WorkspaceInfo,
};

/// Effects the reconciler can request. Implemented for the real Workspace in
/// connection.rs and by a recording fake in tests.
pub trait WorkspaceOps {
    fn create_workspace_group(&mut self, workspace: &WorkspaceInfo);
    fn create_tab(&mut self, tab: &TabInfo, layout: &HerdrPaneTree);
    /// Brackets a herdr tab materialization (snapshot or refreshed layout) so
    /// the workspace can suppress per-tab activation churn. No-op by default.
    fn set_materializing(&mut self, _active: bool) {}
    fn close_tab(&mut self, tab_id: &str);
    fn rename_tab(&mut self, tab_id: &str, label: &str);
    /// A herdr-side tab move reordered the workspace's tabs; `ordered_tab_ids`
    /// is the full post-move order, so applying it heals any missed moves.
    /// No-op by default so test fakes that don't track tab order are
    /// unaffected.
    fn apply_tab_order(&mut self, _workspace_id: &str, _ordered_tab_ids: &[String]) {}
    fn set_tab_status(&mut self, tab_id: &str, status: AgentStatus);
    fn add_split(&mut self, tab_id: &str, pane: &PaneInfo);
    fn close_pane(&mut self, pane_id: &str);
    fn set_pane_title(&mut self, pane_id: &str, title: &str);
    fn set_pane_label(&mut self, _pane_id: &str, _label: Option<&str>) {}
    /// The agent herdr reports running in the pane changed (started, stopped,
    /// or switched); drives the pane's sidebar agent icon.
    fn set_pane_agent(&mut self, pane_id: &str, agent: Option<&str>);
    /// The pane's agent status changed; drives the pane's live status
    /// indicator. No-op by default so test fakes that don't track pane
    /// status are unaffected.
    fn set_pane_agent_status(&mut self, _pane_id: &str, _status: AgentStatus) {}
    /// A refreshed layout arrived for an already-materialized tab: apply its
    /// direction/ratio/structure to the live pane tree. No-op by default so
    /// test fakes that don't track geometry are unaffected.
    fn sync_tab_layout(&mut self, _tab_id: &str, _layout: &HerdrPaneTree) {}
    /// Layout drifted in a way we can't patch incrementally (pane moved,
    /// layout.updated); connection.rs responds by re-running layout.export.
    fn request_layout_refresh(&mut self, tab_id: &str);
    /// Attaches deferred pane streams for the window's active tab when it
    /// mirrors a herdr tab. Called once at the end of the resync rebuild:
    /// rebuilt tabs defer their streams, and one can end up active without a
    /// fresh activation ever firing the first-view attach trigger. No-op by
    /// default so test fakes that don't track streams are unaffected.
    fn attach_active_tab_streams(&mut self) {}
}

/// Warp-initiated operations, recorded before the API call so the echoed herdr
/// event is not re-applied.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Intent {
    CloseTab(String),
    ClosePane(String),
    /// Keyed by the new label too: a coarse tab-id key would let the intent
    /// swallow a NON-matching echo — another client renaming the same tab
    /// inside the confirmation window would be dropped and the titles would
    /// silently diverge.
    RenameTab {
        tab_id: String,
        label: String,
    },
    MoveTab(String),
}

/// How long an intent waits for its echoed herdr event before expiring: the
/// echo normally lands within a round trip, and a lost echo or failed API
/// call must not swallow the next herdr-initiated event for the same target
/// (the design doc's "short confirmation window").
const INTENT_TTL: Duration = Duration::from_secs(10);

/// Lost-refresh self-heal: an event arriving for a tab still awaiting its
/// first layout re-requests the refresh only when the previous request is
/// at least this old (the round trip normally lands well within it).
const LAYOUT_REFRESH_REREQUEST_AFTER: Duration = Duration::from_secs(2);

/// …and at most this many times. Past the cap the tab stays degraded —
/// events for it keep dropping — until the next full resync heals it.
const MAX_LAYOUT_REFRESH_REREQUESTS: u32 = 3;

#[derive(Debug, Clone)]
struct PaneRecord {
    tab_id: String,
    revision: u64,
    label: Option<String>,
    terminal_title: Option<String>,
    agent: Option<String>,
    agent_status: AgentStatus,
}

/// Bookkeeping for a tab in `tabs_awaiting_layout`.
#[derive(Debug, Clone)]
struct AwaitingLayout {
    /// When the latest layout-refresh request for this tab was issued.
    last_request: Instant,
    /// Re-requests issued so far, capped at MAX_LAYOUT_REFRESH_REREQUESTS.
    retries: u32,
    /// The tab closed while its refresh was in flight: the entry stays as a
    /// tombstone so the late apply_layout is discarded instead of
    /// resurrecting the tab. Tombstoned tabs never re-request.
    tombstone: bool,
}

#[derive(Default)]
pub struct Reconciler {
    known_workspaces: HashSet<String>,
    known_tabs: HashMap<String, String>, // tab id → workspace id
    tab_labels: HashMap<String, String>, // tab id → herdr tab label
    known_panes: HashMap<String, PaneRecord>,
    tab_status: HashMap<String, AgentStatus>,
    /// Tabs whose TabCreated was seen but whose layout.export follow-up has
    /// not landed yet, plus tombstones for closed tabs. Awaiting tabs stay
    /// in known_tabs so a TabClosed landing mid-refresh is honored; every
    /// close leaves a non-consuming tombstone (materialized tabs included —
    /// geometry refreshes can be in flight) so late apply_layout arrivals
    /// are discarded instead of resurrecting the tab. An event arriving for
    /// a still-awaiting, non-tombstoned tab may re-request the refresh
    /// (throttled and capped — see maybe_rerequest_layout_refresh). Cleared
    /// by resync.
    tabs_awaiting_layout: HashMap<String, AwaitingLayout>,
    /// Warp-initiated intents awaiting their echoed herdr event; entries
    /// expire unmatched after INTENT_TTL.
    pending: HashMap<Intent, Instant>,
    /// Set when a pane id is learned incrementally (split, late tab) after
    /// the event stream subscribed: its pane-scoped agent-status
    /// subscription is missing until the connection re-subscribes.
    pane_subscription_stale: bool,
    /// Set when an intent expired unconsumed (see take_intent_expiry).
    intent_expired: bool,
}

/// Displayed pane title: "<herdr tab label> - <live terminal title>", or
/// whichever half exists.
fn composed_pane_title(label: Option<&str>, title: Option<&str>) -> Option<String> {
    let label = label.filter(|l| !l.trim().is_empty());
    let title = title.filter(|t| !t.trim().is_empty());
    match (label, title) {
        (Some(label), Some(title)) => Some(format!("{label} - {title}")),
        (Some(label), None) => Some(label.to_string()),
        (None, Some(title)) => Some(title.to_string()),
        (None, None) => None,
    }
}

fn normalized_pane_label(label: Option<&str>) -> Option<String> {
    label
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(str::to_string)
}

/// The agent status to push when a pane first materializes (snapshot,
/// layout, or live pane.created). Idle normally pushes nothing — it is the
/// pane's default, and a LIVE Idle transition deliberately never overrides
/// the last real status. The snapshot status needs no coercion: herdr's
/// reported status is seen-aware (a finished-while-unseen agent reports Done
/// directly; Idle means at-prompt and already seen — live-verified against
/// 0.7.5), and pane materialization seeds the mirrored session's initial
/// status from it, so the push here is idempotent insurance for non-idle
/// statuses rather than a correction.
fn initial_status_push(status: AgentStatus) -> Option<AgentStatus> {
    (status != AgentStatus::Idle).then_some(status)
}

/// herdr rolls per-pane agent statuses up to the tab by urgency; mirrored
/// here so multi-pane tabs keep the right color.
fn status_rank(status: AgentStatus) -> u8 {
    match status {
        AgentStatus::Blocked => 4,
        AgentStatus::Working => 3,
        AgentStatus::Done => 2,
        AgentStatus::Idle => 1,
        AgentStatus::Unknown => 0,
    }
}

impl Reconciler {
    pub fn note_local_intent(&mut self, intent: Intent) {
        self.pending.insert(intent, Instant::now());
    }

    /// Rolls back a noted intent whose API call failed, so the next
    /// herdr-initiated event for the target applies normally.
    pub fn remove_intent(&mut self, intent: Intent) {
        self.pending.remove(&intent);
    }

    fn take_intent(&mut self, intent: &Intent) -> bool {
        self.expire_stale_intents();
        self.pending.remove(intent).is_some()
    }

    /// Sweeps intents older than INTENT_TTL (also done lazily by
    /// take_intent) and remembers that some expired for take_intent_expiry.
    pub fn expire_stale_intents(&mut self) {
        let expired = &mut self.intent_expired;
        self.pending.retain(|noted, at| {
            let fresh = at.elapsed() < INTENT_TTL;
            if !fresh {
                *expired = true;
                log::warn!(
                    "herdr: expiring unconsumed intent {noted:?}; its echoed event never arrived"
                );
            }
            fresh
        });
    }

    /// True (once) when an intent expired unconsumed since the last take. A
    /// pending intent only exists after its API call succeeded, so herdr HAS
    /// broadcast the echoed event — expiry means the event stream is not
    /// delivering, and the connection resyncs on this signal.
    pub fn take_intent_expiry(&mut self) -> bool {
        std::mem::take(&mut self.intent_expired)
    }

    pub fn is_known_tab(&self, tab_id: &str) -> bool {
        self.known_tabs.contains_key(tab_id)
    }

    pub fn tab_of_pane(&self, pane_id: &str) -> Option<&str> {
        self.known_panes.get(pane_id).map(|p| p.tab_id.as_str())
    }

    pub fn note_local_pane_label(&mut self, pane_id: &str, label: Option<&str>) {
        if let Some(rec) = self.known_panes.get_mut(pane_id) {
            rec.label = normalized_pane_label(label);
        }
    }

    pub(super) fn note_local_pane_rename_ack(&mut self, expected_pane_id: &str, pane: &PaneInfo) {
        if pane.pane_id != expected_pane_id {
            log::warn!(
                "herdr: pane.rename for {expected_pane_id} returned pane {}",
                pane.pane_id
            );
            return;
        }
        if let Some(rec) = self.known_panes.get_mut(expected_pane_id) {
            rec.revision = rec.revision.max(pane.revision);
        }
    }

    pub fn event_changes_pane_label(&self, event: &EventData) -> bool {
        let EventData::PaneUpdated { pane } = event else {
            return false;
        };
        self.known_panes.get(&pane.pane_id).is_some_and(|rec| {
            pane.revision > rec.revision
                && rec.label != normalized_pane_label(pane.label.as_deref())
        })
    }

    pub fn reconcile_pane_labels(&mut self, panes: &[PaneInfo], ops: &mut dyn WorkspaceOps) {
        for pane in panes {
            let Some(rec) = self.known_panes.get_mut(&pane.pane_id) else {
                continue;
            };
            let label = normalized_pane_label(pane.label.as_deref());
            if rec.label != label {
                rec.label = label;
                ops.set_pane_label(&pane.pane_id, rec.label.as_deref());
            }
        }
    }

    pub fn known_tab_ids(&self) -> Vec<String> {
        self.known_tabs.keys().cloned().collect()
    }

    /// Prepares for a full rebuild from a fresh snapshot after a server
    /// restart: returns the tab ids currently materialized locally (the
    /// caller closes them) and forgets ALL state, including pending intents —
    /// nothing learned from the dead server may route API calls to the new
    /// one or swallow its events.
    pub fn take_stale_tabs_for_resync(&mut self) -> Vec<String> {
        let mut stale = self.known_tab_ids();
        stale.sort(); // deterministic teardown order
        *self = Self::default();
        stale
    }

    pub fn apply_snapshot(
        &mut self,
        snapshot: &SessionSnapshot,
        layouts: &[(String, HerdrPaneTree)],
        ops: &mut dyn WorkspaceOps,
    ) {
        let layout_by_tab: HashMap<&str, &HerdrPaneTree> =
            layouts.iter().map(|(id, t)| (id.as_str(), t)).collect();
        let mut pane_by_id: HashMap<&str, &PaneInfo> = HashMap::new();
        for pane in &snapshot.panes {
            pane_by_id.entry(pane.pane_id.as_str()).or_insert(pane);
        }
        for ws in &snapshot.workspaces {
            if self.known_workspaces.insert(ws.workspace_id.clone()) {
                ops.create_workspace_group(ws);
            }
        }
        for tab in &snapshot.tabs {
            if self.known_tabs.contains_key(&tab.tab_id) {
                continue;
            }
            let mut layout: HerdrPaneTree = match layout_by_tab.get(tab.tab_id.as_str()) {
                Some(l) => (*l).clone(),
                None => {
                    // No exported layout (shouldn't happen); single leaf from
                    // the first pane in this tab.
                    let first = snapshot.panes.iter().find(|p| p.tab_id == tab.tab_id);
                    let Some(first) = first else {
                        log::warn!(
                            "herdr: snapshot tab {} has no exported layout and no panes; skipping",
                            tab.tab_id
                        );
                        continue;
                    };
                    HerdrPaneTree::Leaf {
                        pane_id: first.pane_id.clone(),
                        cwd: first.cwd.clone(),
                        agent: first.agent.clone(),
                        agent_status: first.agent_status,
                    }
                }
            };
            // Enrich leaves with the agents herdr reports so mirrored panes
            // can surface agent icons.
            layout.enrich_agents(&|pane_id| {
                pane_by_id
                    .get(pane_id)
                    .copied()
                    .map(|pane| (pane.agent.clone(), pane.agent_status))
                    .unwrap_or((None, AgentStatus::Unknown))
            });
            self.materialize_tab(tab, &layout, &pane_by_id, true, ops);
        }
        // The (re)subscribe that accompanies every snapshot apply (bootstrap
        // and resync both subscribe with the snapshot's full pane set) covers
        // everything just learned — including the stale flag materialize_tab
        // set for each registered pane.
        self.pane_subscription_stale = false;
    }

    /// Materializes one tab: registers it and its panes, creates the local
    /// tab inside a materializing bracket, then pushes initial composed pane
    /// titles and non-idle agent statuses (idle is the pane's default; later
    /// changes arrive via pane.updated / the agent-status subscription).
    /// Shared by `apply_snapshot` (pane records seeded from the snapshot's
    /// pane info, statuses pushed) and `apply_layout`'s first
    /// materialization (no pane info yet — records start at defaults and
    /// fill in from later pane.updated events, so `push_pane_statuses` is
    /// off and nothing pushes a meaningless Unknown).
    fn materialize_tab(
        &mut self,
        tab: &TabInfo,
        layout: &HerdrPaneTree,
        pane_by_id: &HashMap<&str, &PaneInfo>,
        push_pane_statuses: bool,
        ops: &mut dyn WorkspaceOps,
    ) {
        self.known_tabs
            .insert(tab.tab_id.clone(), tab.workspace_id.clone());
        self.tab_labels
            .insert(tab.tab_id.clone(), tab.label.clone());
        for pane_id in layout.pane_ids() {
            let info = pane_by_id.get(pane_id.as_str()).copied();
            if let Entry::Vacant(entry) = self.known_panes.entry(pane_id) {
                entry.insert(PaneRecord {
                    tab_id: tab.tab_id.clone(),
                    revision: info.map(|p| p.revision).unwrap_or(0),
                    label: normalized_pane_label(info.and_then(|p| p.label.as_deref())),
                    terminal_title: info.and_then(|p| p.terminal_title.clone()),
                    agent: info.and_then(|p| p.agent.clone()),
                    agent_status: info.map(|p| p.agent_status).unwrap_or(AgentStatus::Unknown),
                });
                self.pane_subscription_stale = true;
            }
        }
        ops.set_materializing(true);
        ops.create_tab(tab, layout);
        ops.set_materializing(false);
        for pane_id in layout.pane_ids() {
            let rec = self.known_panes.get(&pane_id);
            if let Some(label) = rec.and_then(|rec| rec.label.as_deref()) {
                ops.set_pane_label(&pane_id, Some(label));
            }
            let title = rec.and_then(|rec| rec.terminal_title.clone());
            if let Some(composed) = composed_pane_title(Some(&tab.label), title.as_deref()) {
                ops.set_pane_title(&pane_id, &composed);
            }
            // See initial_status_push: non-idle statuses push as-is; idle
            // pushes nothing (the pane's session seeds its initial status).
            let status = rec
                .map(|rec| rec.agent_status)
                .unwrap_or(AgentStatus::Unknown);
            if push_pane_statuses && let Some(status) = initial_status_push(status) {
                ops.set_pane_agent_status(&pane_id, status);
            }
        }
        // Record the initial status for every tab (so later pane.updated
        // status syncs diff correctly); only non-idle needs a color push.
        self.tab_status.insert(tab.tab_id.clone(), tab.agent_status);
        if tab.agent_status != AgentStatus::Idle {
            ops.set_tab_status(&tab.tab_id, tab.agent_status);
        }
    }

    /// Returns the full current pane id set when a pane has been learned
    /// since the event stream last (re)subscribed, clearing the flag.
    /// pane.agent_status_changed subscriptions are pane-scoped and herdr
    /// sends status transitions through no other path, so the connection
    /// must re-subscribe to cover panes born after subscribe time.
    pub fn take_pane_subscription_refresh(&mut self) -> Option<Vec<String>> {
        if !self.pane_subscription_stale {
            return None;
        }
        self.pane_subscription_stale = false;
        let mut pane_ids: Vec<String> = self.known_panes.keys().cloned().collect();
        pane_ids.sort();
        Some(pane_ids)
    }

    pub fn apply_event(&mut self, event: &EventData, ops: &mut dyn WorkspaceOps) {
        self.apply_event_with_pane_label(event, true, ops);
    }

    pub(super) fn apply_event_with_pane_label(
        &mut self,
        event: &EventData,
        apply_pane_label: bool,
        ops: &mut dyn WorkspaceOps,
    ) {
        match event {
            EventData::WorkspaceCreated { workspace } => {
                if self.known_workspaces.insert(workspace.workspace_id.clone()) {
                    ops.create_workspace_group(workspace);
                }
            }
            EventData::WorkspaceClosed { workspace_id } => {
                self.known_workspaces.remove(workspace_id);
                let mut tabs: Vec<String> = self
                    .known_tabs
                    .iter()
                    .filter(|(_, ws)| *ws == workspace_id)
                    .map(|(t, _)| t.clone())
                    .collect();
                tabs.sort(); // deterministic close order
                for tab_id in tabs {
                    // Mirror TabClosed per tab: tombstone every close so a
                    // late layout refresh can't resurrect it, and only close
                    // tabs that actually materialized — one still awaiting
                    // its first layout has nothing local to close.
                    let awaiting = self.tabs_awaiting_layout.contains_key(&tab_id);
                    self.forget_tab(&tab_id);
                    self.tombstone_tab(&tab_id);
                    if !awaiting {
                        ops.close_tab(&tab_id);
                    }
                }
            }
            EventData::WorkspaceRenamed { .. } => {
                // v1: group labels update only on resync; rename events ignored.
            }
            EventData::TabCreated { tab } => {
                if self.known_tabs.contains_key(&tab.tab_id)
                    || self.tabs_awaiting_layout.contains_key(&tab.tab_id)
                {
                    return; // replay (or the tab closed while its refresh was in flight)
                }
                self.known_tabs
                    .insert(tab.tab_id.clone(), tab.workspace_id.clone());
                self.tab_labels
                    .insert(tab.tab_id.clone(), tab.label.clone());
                // No Warp-create intent here: unlike close/rename, the local
                // tab cannot exist before the tab.create response (the new
                // tab id is minted herdr-side), so both the response path and
                // the echoed broadcast must materialize — the replay guards
                // here and in apply_layout dedupe them. (A stale intent used
                // to outlive the Warp-side create and swallow the next
                // herdr-side tab in the same workspace.)
                // A fresh tab's root pane id is not knowable from TabInfo, so
                // connection.rs follows up with layout.export and apply_layout
                // materializes the tab. Until then the tab is marked awaiting
                // so events that can't apply yet defer to the layout, and a
                // TabClosed landing mid-refresh is honored instead of dropped.
                self.tabs_awaiting_layout.insert(
                    tab.tab_id.clone(),
                    AwaitingLayout {
                        last_request: Instant::now(),
                        retries: 0,
                        tombstone: false,
                    },
                );
                ops.request_layout_refresh(&tab.tab_id);
            }
            EventData::TabClosed { tab_id, .. } => {
                let awaiting = self.tabs_awaiting_layout.contains_key(tab_id);
                if !self.known_tabs.contains_key(tab_id) && !awaiting {
                    return;
                }
                self.forget_tab(tab_id);
                // A layout refresh can be in flight for materialized tabs too
                // (geometry syncs re-request layout.export), so every close
                // leaves a tombstone: a late arrival must be discarded, never
                // materialized into a ghost tab herdr no longer owns.
                self.tombstone_tab(tab_id);
                if awaiting {
                    // Closed while the first layout refresh was in flight:
                    // nothing was materialized locally, so there is nothing
                    // to close.
                    return;
                }
                if self.take_intent(&Intent::CloseTab(tab_id.clone())) {
                    return; // Warp closed it locally already
                }
                ops.close_tab(tab_id);
            }
            EventData::TabRenamed { tab_id, label, .. } => {
                if !self.known_tabs.contains_key(tab_id) {
                    return;
                }
                self.tab_labels.insert(tab_id.clone(), label.clone());
                if !self.take_intent(&Intent::RenameTab {
                    tab_id: tab_id.clone(),
                    label: label.clone(),
                }) {
                    ops.rename_tab(tab_id, label);
                }
                // Re-push pane titles so the composed "<label> - <title>"
                // prefix follows the rename (even for Warp-initiated renames).
                let panes: Vec<(String, Option<String>)> = self
                    .known_panes
                    .iter()
                    .filter(|(_, rec)| rec.tab_id == *tab_id)
                    .map(|(id, rec)| (id.clone(), rec.terminal_title.clone()))
                    .collect();
                for (pane_id, title) in panes {
                    if let Some(composed) = composed_pane_title(Some(label), title.as_deref()) {
                        ops.set_pane_title(&pane_id, &composed);
                    }
                }
            }
            EventData::TabMoved {
                tab_id,
                workspace_id,
                tabs,
                ..
            } => {
                if self.take_intent(&Intent::MoveTab(tab_id.clone())) {
                    return; // Warp moved it locally already
                }
                // The moved tab may be unknown (its TabCreated is still
                // awaiting a layout refresh) or its workspace unknown; there
                // is no local order to apply yet — materialization heals it.
                if !self.known_workspaces.contains(workspace_id)
                    || !self.known_tabs.contains_key(tab_id)
                {
                    log::debug!(
                        "herdr: ignoring tab.moved for unknown tab {tab_id} in workspace {workspace_id}"
                    );
                    return;
                }
                // insert_index is redundant with the full post-move order in
                // `tabs`, which also heals any move we missed.
                let ordered_ids: Vec<String> = tabs.iter().map(|tab| tab.tab_id.clone()).collect();
                ops.apply_tab_order(workspace_id, &ordered_ids);
            }
            EventData::TabFocused { .. } => {
                // v1: tab focus is one-way Warp→herdr; ignore herdr-side.
            }
            EventData::PaneCreated { pane } => {
                if self.known_panes.contains_key(&pane.pane_id) {
                    return; // replay
                }
                if !self.known_tabs.contains_key(&pane.tab_id) {
                    return; // unknown tab (closed or never materialized); dropped
                }
                if self.tabs_awaiting_layout.contains_key(&pane.tab_id) {
                    // The tab's layout refresh is still in flight (or failed —
                    // then the tab stays unmaterialized): the pane arrives
                    // with the layout when it lands.
                    log::warn!(
                        "herdr: dropping pane.created {} for tab {} still awaiting layout",
                        pane.pane_id,
                        pane.tab_id
                    );
                    self.maybe_rerequest_for_awaiting_tab(&pane.tab_id, ops);
                    return;
                }
                self.known_panes.insert(
                    pane.pane_id.clone(),
                    PaneRecord {
                        tab_id: pane.tab_id.clone(),
                        revision: pane.revision,
                        label: normalized_pane_label(pane.label.as_deref()),
                        terminal_title: pane.terminal_title.clone(),
                        agent: pane.agent.clone(),
                        agent_status: pane.agent_status,
                    },
                );
                self.pane_subscription_stale = true;
                // No Warp-split intent here: unlike close/rename, the local pane
                // cannot exist before the pane.split response (the new pane id
                // is minted herdr-side), so both the response path and the
                // echoed broadcast must materialize — the replay guard above
                // dedupes them.
                ops.add_split(&pane.tab_id, pane);
                if let Some(label) = normalized_pane_label(pane.label.as_deref()) {
                    ops.set_pane_label(&pane.pane_id, Some(&label));
                }
                // add_split inserts at the pane-tree root with default
                // sizing; the refreshed layout re-places the new split with
                // herdr's direction/ratio (apply_layout syncs geometry onto
                // materialized tabs).
                ops.request_layout_refresh(&pane.tab_id);
                // Push the initial title, agent icon, and agent status up
                // front, like apply_snapshot, instead of waiting for the
                // next pane.updated.
                let label = self.tab_labels.get(&pane.tab_id).map(String::as_str);
                if let Some(composed) = composed_pane_title(label, pane.terminal_title.as_deref()) {
                    ops.set_pane_title(&pane.pane_id, &composed);
                }
                if pane.agent.is_some() {
                    ops.set_pane_agent(&pane.pane_id, pane.agent.as_deref());
                }
                if let Some(status) = initial_status_push(pane.agent_status) {
                    ops.set_pane_agent_status(&pane.pane_id, status);
                }
            }
            EventData::PaneClosed { pane_id, .. } | EventData::PaneExited { pane_id, .. } => {
                let Some(rec) = self.known_panes.remove(pane_id) else {
                    return;
                };
                if !self.take_intent(&Intent::ClosePane(pane_id.clone())) {
                    ops.close_pane(pane_id);
                }
                // The roll-up may drop (e.g. the tab's only Working pane
                // closed); recompute even when the close was Warp-initiated.
                self.recompute_tab_status(&rec.tab_id, ops);
            }
            EventData::PaneUpdated { pane } => {
                let Some(rec) = self.known_panes.get_mut(&pane.pane_id) else {
                    // Unknown pane: if its tab is still awaiting its first
                    // layout, the update defers to the layout (and may
                    // re-request a lost refresh); otherwise truly unknown.
                    self.maybe_rerequest_for_awaiting_tab(&pane.tab_id, ops);
                    return;
                };
                if pane.revision <= rec.revision {
                    // Herdr does not advance the session revision for manual
                    // pane-label changes. An equal revision can therefore be
                    // replayed history and cannot safely override the
                    // authoritative heartbeat snapshot; live label-only
                    // changes wait for the next snapshot.
                    return;
                }
                if apply_pane_label {
                    let label = normalized_pane_label(pane.label.as_deref());
                    if rec.label != label {
                        rec.label = label;
                        ops.set_pane_label(&pane.pane_id, rec.label.as_deref());
                    }
                }
                rec.revision = pane.revision;
                let title_changed = pane.terminal_title != rec.terminal_title;
                if title_changed {
                    rec.terminal_title = pane.terminal_title.clone();
                }
                let agent_changed = pane.agent != rec.agent;
                if agent_changed {
                    rec.agent = pane.agent.clone();
                }
                let status_changed = pane.agent_status != rec.agent_status;
                if status_changed {
                    rec.agent_status = pane.agent_status;
                }
                let tab_id = rec.tab_id.clone();
                if title_changed {
                    let label = self.tab_labels.get(&tab_id).map(String::as_str);
                    if let Some(composed) =
                        composed_pane_title(label, pane.terminal_title.as_deref())
                    {
                        ops.set_pane_title(&pane.pane_id, &composed);
                    }
                }
                // The running agent is usually detected only after the pane is
                // mirrored (the pane spawns as a bare shell, the agent starts
                // later), so icons must follow these updates too. Agent BEFORE
                // status: a single update carrying a newly-detected agent plus
                // a non-idle status must seed the CLI agent session first, or
                // the status push lands on no session and is dropped
                // (add_split orders the same way).
                if agent_changed {
                    ops.set_pane_agent(&pane.pane_id, pane.agent.as_deref());
                }
                if status_changed {
                    ops.set_pane_agent_status(&pane.pane_id, pane.agent_status);
                    // herdr sends no incremental tab-level status event, so
                    // the tab color is a roll-up across its panes' statuses.
                    self.recompute_tab_status(&tab_id, ops);
                }
            }
            EventData::PaneAgentDetected { pane_id, agent, .. } => {
                let Some(rec) = self.known_panes.get_mut(pane_id) else {
                    return;
                };
                if rec.agent != *agent {
                    rec.agent = agent.clone();
                    ops.set_pane_agent(pane_id, agent.as_deref());
                }
            }
            EventData::PaneFocused { .. } => {
                // one-way focus (Warp→herdr); ignore herdr-side focus.
            }
            EventData::LayoutUpdated { tab_id } => {
                if self.known_tabs.contains_key(tab_id) {
                    ops.request_layout_refresh(tab_id);
                }
            }
            EventData::Unknown => {}
        }
    }

    pub fn apply_agent_status(&mut self, ev: &AgentStatusEvent, ops: &mut dyn WorkspaceOps) {
        // ev.title and ev.display_agent are intentionally ignored: the pane's
        // sidebar icon is driven by PaneInfo.agent (via pane.updated /
        // pane.agent_detected), not by this event.
        let Some(rec) = self.known_panes.get_mut(&ev.pane_id) else {
            return;
        };
        if rec.agent_status == ev.agent_status {
            return;
        }
        rec.agent_status = ev.agent_status;
        let tab_id = rec.tab_id.clone();
        ops.set_pane_agent_status(&ev.pane_id, ev.agent_status);
        self.recompute_tab_status(&tab_id, ops);
    }

    /// Late layout arrival for a tab (from request_layout_refresh follow-up):
    /// materializes a tab still awaiting its first layout, syncs geometry
    /// onto an already-materialized one.
    pub fn apply_layout(
        &mut self,
        tab: &TabInfo,
        layout: &HerdrPaneTree,
        ops: &mut dyn WorkspaceOps,
    ) {
        if self
            .tabs_awaiting_layout
            .get(&tab.tab_id)
            .is_some_and(|awaiting| awaiting.tombstone)
        {
            // The tab closed while a layout refresh (first materialization
            // or geometry sync) was in flight. The tombstone is deliberately
            // not consumed: re-requested refreshes can land more than once,
            // and every late arrival must be discarded rather than
            // resurrecting a tab herdr no longer owns.
            log::warn!(
                "herdr: discarding layout for tab {} closed while its refresh was in flight",
                tab.tab_id
            );
            return;
        }
        let was_awaiting = self.tabs_awaiting_layout.remove(&tab.tab_id).is_some();
        if self.known_tabs.contains_key(&tab.tab_id) && !was_awaiting {
            // Already materialized: this refresh is a geometry sync — apply
            // the tree's direction/ratio/structure to the live tab
            // (herdr-initiated splits landed at the pane-tree root with
            // default sizing via add_split).
            ops.sync_tab_layout(&tab.tab_id, layout);
            return;
        }
        let pane_by_id: HashMap<&str, &PaneInfo> = HashMap::new();
        self.materialize_tab(tab, layout, &pane_by_id, false, ops);
    }

    /// Re-request hook for events dropped because their tab is still
    /// awaiting its first layout (pane.created, pane.updated for a
    /// not-yet-known pane): the refresh may have failed silently.
    fn maybe_rerequest_for_awaiting_tab(&mut self, tab_id: &str, ops: &mut dyn WorkspaceOps) {
        if let Some(awaiting) = self.tabs_awaiting_layout.get_mut(tab_id) {
            Self::maybe_rerequest_layout_refresh(awaiting, tab_id, ops);
        }
    }

    /// Self-heal for a silently-failed layout refresh: re-request it,
    /// throttled to one re-request per LAYOUT_REFRESH_REREQUEST_AFTER and
    /// capped at MAX_LAYOUT_REFRESH_REREQUESTS — past the cap the tab stays
    /// degraded until the next full resync, the final healer. Tombstoned
    /// tabs never re-request: their close owns cleanup.
    fn maybe_rerequest_layout_refresh(
        awaiting: &mut AwaitingLayout,
        tab_id: &str,
        ops: &mut dyn WorkspaceOps,
    ) {
        if awaiting.tombstone {
            return;
        }
        if awaiting.last_request.elapsed() < LAYOUT_REFRESH_REREQUEST_AFTER {
            return;
        }
        if awaiting.retries >= MAX_LAYOUT_REFRESH_REREQUESTS {
            log::warn!(
                "herdr: tab {tab_id} still awaiting layout after {MAX_LAYOUT_REFRESH_REREQUESTS} re-requests; leaving degraded until the next resync"
            );
            return;
        }
        awaiting.retries += 1;
        awaiting.last_request = Instant::now();
        log::debug!(
            "herdr: re-requesting layout refresh for tab {tab_id} (re-request {})",
            awaiting.retries
        );
        ops.request_layout_refresh(tab_id);
    }

    /// Recomputes a tab's agent-status roll-up across its panes (Blocked >
    /// Working > Done > Idle, mirroring herdr) and pushes the tab status only
    /// when the roll-up changes.
    fn recompute_tab_status(&mut self, tab_id: &str, ops: &mut dyn WorkspaceOps) {
        let rolled_up = self
            .known_panes
            .values()
            .filter(|rec| rec.tab_id == tab_id)
            .map(|rec| rec.agent_status)
            .max_by_key(|status| status_rank(*status))
            .unwrap_or(AgentStatus::Unknown);
        let prev = self.tab_status.insert(tab_id.to_string(), rolled_up);
        if prev != Some(rolled_up) {
            ops.set_tab_status(tab_id, rolled_up);
        }
    }

    fn forget_tab(&mut self, tab_id: &str) {
        self.known_tabs.remove(tab_id);
        self.tab_labels.remove(tab_id);
        self.known_panes.retain(|_, rec| rec.tab_id != tab_id);
        self.tab_status.remove(tab_id);
        // tabs_awaiting_layout is intentionally left alone: close paths
        // follow up with tombstone_tab so late layout arrivals are discarded.
    }

    /// Marks a closed tab id so any late layout arrival for it is discarded
    /// instead of resurrecting the tab. Non-consuming — re-requested
    /// refreshes can land more than once, and each must be discarded. herdr
    /// never reuses tab ids within a server lifetime; resync clears the
    /// entries.
    fn tombstone_tab(&mut self, tab_id: &str) {
        self.tabs_awaiting_layout
            .entry(tab_id.to_string())
            .and_modify(|awaiting| awaiting.tombstone = true)
            .or_insert(AwaitingLayout {
                last_request: Instant::now(),
                retries: MAX_LAYOUT_REFRESH_REREQUESTS,
                tombstone: true,
            });
    }
}

#[cfg(test)]
#[path = "reconciler_tests.rs"]
mod tests;
