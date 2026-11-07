//! HerdrConnection: one attached herdr session. Owns the API handle, the
//! event stream, the reconciler, and the herdr_tab/pane ↔ Warp mapping.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use warpui::ModelContext;

use super::layout_convert::HerdrPaneTree;
use super::protocol::*;
use super::reconciler::{Intent, Reconciler, WorkspaceOps};
use super::socket::{EventStreamHandle, HerdrApi};
use crate::send_telemetry_from_ctx;
use crate::server::telemetry::TelemetryEvent;
use crate::terminal::herdr_tty::stream::HerdrStreamSpec;

/// How often the heartbeat watchdog snapshots the server while connected:
/// the event stream alone can't catch a half-alive server that holds the
/// socket open but stops sending events.
const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

pub struct HerdrConnection {
    api: HerdrApi,
    reconciler: Reconciler,
    workspace: warpui::WeakViewHandle<crate::workspace::Workspace>,
    /// Pane close reports from every herdr_tty pane of this session.
    pane_closed_tx: async_channel::Sender<(String, Option<String>)>,
    base_spec: HerdrStreamSpec,
    connected: bool,
    /// Generation of the live event stream, bumped on every (re)subscribe. A
    /// superseded stream's items and late close are ignored: the watchdog can
    /// trigger a resync while a half-alive server still holds the old stream
    /// open, and that stream must not interleave events into — or read as a
    /// fresh disconnect of — the rebuilt mirror.
    event_stream_generation: u64,
    /// Generation of the armed heartbeat watchdog, shared with its background
    /// probes. Bumped on every arm and disarm (disconnect/detach): a probe
    /// that wakes to a changed generation retires itself instead of requesting
    /// alongside its replacement or reporting a death that liveness ownership
    /// has already moved on from.
    watchdog_generation: Arc<AtomicU64>,
    /// Bumped for accepted local and event-stream pane-label mutations. A
    /// watchdog snapshot captures this immediately before its RPC so its
    /// callback cannot overwrite a newer label with an older snapshot.
    pane_label_generation: Arc<AtomicU64>,
    /// Pane rename RPCs queued or in flight. A watchdog snapshot taken while
    /// any rename is outstanding is not authoritative for pane labels because
    /// the snapshot and rename use independent socket connections.
    pane_renames_outstanding: Arc<AtomicU64>,
    /// One serialized rename chain per pane. Each chain has at most one RPC
    /// in flight and one coalesced latest value waiting behind it.
    pane_rename_queue: PaneRenameQueue,
    /// Set once the Workspace detaches (or the connection drops with its
    /// window): every Warp-initiated op becomes a no-op (detach closes tabs
    /// locally WITHOUT touching herdr), including deferred op callbacks still
    /// queued behind the detach. Shared with the reconnect loop so an outage
    /// survived by a detach doesn't leave the loop pinging a dead socket for
    /// the app's lifetime.
    detached: Arc<AtomicBool>,
    /// Shutdown handle for the LIVE event stream. Superseding a stream (or
    /// dropping the connection) closes the old socket so its reader thread,
    /// fd, and server-side subscription don't leak until the server closes.
    event_stream_shutdown: Option<EventStreamHandle>,
    /// Pane ids the LIVE subscription's pane-scoped entries cover (sorted).
    /// A refresh whose filtered set matches this skips the resubscribe —
    /// see refreshed_subscribe_set for the loop this prevents.
    subscribed_pane_ids: Vec<String>,
}

pub struct HerdrBootstrap {
    pub snapshot: SessionSnapshot,
    pub layouts: Vec<(String, HerdrPaneTree)>,
    events_rx: async_channel::Receiver<AnyEvent>,
    shutdown: EventStreamHandle,
    pane_ids: Vec<String>,
}

pub enum HerdrConnectionEvent {
    Disconnected,
    Reconnected,
    /// A reconnect-loop resync reached the server but bootstrap or
    /// re-subscribe failed (e.g. it came back below MIN_HERDR_VERSION).
    /// Without surfacing this, the connection resyncs-and-fails forever
    /// with the actionable reason only in logs.
    ResyncFailed {
        message: String,
    },
}

impl warpui::Entity for HerdrConnection {
    type Event = HerdrConnectionEvent;
}

/// A window can close while attached — no detach runs first. Without this,
/// the watchdog keeps snapshotting every HEARTBEAT_INTERVAL, an outage's
/// reconnect loop keeps pinging every 30s, and the event socket's reader
/// thread parks in read — one leaked set per closed window, for the app's
/// lifetime.
impl Drop for HerdrConnection {
    fn drop(&mut self) {
        self.detached.store(true, Ordering::SeqCst);
        self.disarm_watchdog();
        if let Some(stream) = self.event_stream_shutdown.take() {
            stream.shutdown();
        }
    }
}

impl HerdrConnection {
    /// Blocking bootstrap (call on background executor): ping-gate, snapshot,
    /// per-tab layout export.
    pub fn bootstrap(
        api: &HerdrApi,
    ) -> anyhow::Result<(SessionSnapshot, Vec<(String, HerdrPaneTree)>)> {
        let pong = api.ping()?;
        if !version_at_least(&pong.version, MIN_HERDR_VERSION) {
            anyhow::bail!(
                "herdr {} is older than the minimum supported {}.{}.{}",
                pong.version,
                MIN_HERDR_VERSION.0,
                MIN_HERDR_VERSION.1,
                MIN_HERDR_VERSION.2
            );
        }
        if pong.protocol < MIN_HERDR_PROTOCOL {
            anyhow::bail!(
                "herdr control protocol {} is older than the minimum supported {}",
                pong.protocol,
                MIN_HERDR_PROTOCOL
            );
        }
        let snapshot = api.session_snapshot()?;
        if snapshot.protocol != pong.protocol {
            // Same server one RPC apart; a mismatch means it restarted (or
            // something is proxying) mid-bootstrap. Warn-only — the resync
            // machinery heals actual divergence.
            log::warn!(
                "herdr: snapshot protocol {} differs from pong protocol {}",
                snapshot.protocol,
                pong.protocol
            );
        }
        let mut layouts = Vec::new();
        for tab in &snapshot.tabs {
            match api.layout_export(&tab.tab_id) {
                // One malformed layout must not abort the whole attach:
                // apply_snapshot falls back to a single-leaf layout for tabs
                // missing from `layouts`.
                Ok(desc) => match HerdrPaneTree::from_layout(&desc.root) {
                    Ok(tree) => layouts.push((tab.tab_id.clone(), tree)),
                    Err(err) => log::warn!("herdr layout {} malformed: {err}", tab.tab_id),
                },
                Err(err) => log::warn!("herdr layout.export {} failed: {err}", tab.tab_id),
            }
        }
        Ok((snapshot, layouts))
    }

    /// Subscribe before taking the authoritative snapshot: newer Herdr servers do not replay
    /// lifecycle events that happened before subscription setup.
    pub fn bootstrap_subscribed(api: &HerdrApi) -> anyhow::Result<HerdrBootstrap> {
        let (mut snapshot, _) = Self::bootstrap(api)?;
        for _ in 0..3 {
            let mut pane_ids: Vec<_> = snapshot
                .panes
                .iter()
                .map(|pane| pane.pane_id.clone())
                .collect();
            pane_ids.sort();
            let (events_rx, shutdown) = api.subscribe_events(&pane_ids)?;
            let (fresh_snapshot, layouts) = Self::bootstrap(api)?;
            let mut fresh_pane_ids: Vec<_> = fresh_snapshot
                .panes
                .iter()
                .map(|pane| pane.pane_id.clone())
                .collect();
            fresh_pane_ids.sort();
            if pane_ids == fresh_pane_ids {
                return Ok(HerdrBootstrap {
                    snapshot: fresh_snapshot,
                    layouts,
                    events_rx,
                    shutdown,
                    pane_ids,
                });
            }
            snapshot = fresh_snapshot;
        }
        anyhow::bail!("herdr panes changed repeatedly while attaching; retry the connection")
    }

    /// An attached connection with no socket, event stream, or watchdog, for
    /// Workspace-level tests that need `herdr_connection` populated without a
    /// live herdr server. `start` cannot serve this: it spawns the stream and
    /// watchdog whose timing is exactly what makes such tests flaky. Ops
    /// routed here reach a dead socket path and fail harmlessly.
    #[cfg(test)]
    pub(crate) fn for_test(workspace: warpui::WeakViewHandle<crate::workspace::Workspace>) -> Self {
        let (pane_closed_tx, pane_closed_rx) = async_channel::unbounded();
        // The receiver is normally owned by the pane-close drain task; leak it
        // so `pane_closed_tx` sends don't fail on a closed channel.
        std::mem::forget(pane_closed_rx);
        Self {
            api: HerdrApi::at(PathBuf::from("/nonexistent/warp-herdr-test.sock")),
            reconciler: Reconciler::default(),
            workspace,
            pane_closed_tx,
            base_spec: crate::herdr::fake_herdr::test_stream_spec("herdr", ""),
            connected: true,
            event_stream_generation: 0,
            watchdog_generation: Arc::new(AtomicU64::new(0)),
            pane_label_generation: Arc::new(AtomicU64::new(0)),
            pane_renames_outstanding: Arc::new(AtomicU64::new(0)),
            pane_rename_queue: PaneRenameQueue::default(),
            detached: Arc::new(AtomicBool::new(false)),
            event_stream_shutdown: None,
            subscribed_pane_ids: Vec::new(),
        }
    }

    pub fn start(
        session_name: String,
        socket_path: PathBuf,
        bootstrap: HerdrBootstrap,
        workspace: warpui::WeakViewHandle<crate::workspace::Workspace>,
        ctx: &mut ModelContext<Self>,
    ) -> Self {
        let HerdrBootstrap {
            snapshot,
            layouts,
            events_rx,
            shutdown,
            pane_ids,
        } = bootstrap;
        let api = HerdrApi::at(socket_path);
        // Data-plane sanity check, off the main thread (running `herdr
        // --version` forks): the control plane talks to the discovered
        // socket directly, but pane streams spawn whatever `herdr` PATH
        // resolves (sparse on macOS GUI launches) — a missing binary fails
        // every pane spawn, and a mismatched one speaks a different stream
        // protocol than the server this connection just version-gated.
        let server_version = snapshot.version.clone();
        ctx.spawn(
            async move { stream_binary_mismatch_warning(&server_version) },
            |_: &mut Self, warning, _| {
                if let Some(warning) = warning {
                    log::warn!("{warning}");
                }
            },
        );
        let (pane_closed_tx, pane_closed_rx) = async_channel::unbounded();
        // Stream subprocesses resolve their session from the environment
        // first (an attach pointed at a named session via HERDR_SOCKET_PATH
        // would otherwise spawn streams against the wrong session), else
        // mirror the attached session name ("default" → no --session flag).
        let session = std::env::var("HERDR_SESSION_NAME")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                std::env::var("HERDR_SESSION")
                    .ok()
                    .filter(|s| !s.is_empty())
            })
            .or_else(|| (session_name != "default").then(|| session_name.clone()));
        let base_spec = HerdrStreamSpec {
            herdr_bin: PathBuf::from("herdr"),
            session,
            target: String::new(),
            cwd: None,
            agent: None,
            agent_status: AgentStatus::Unknown,
            // Placeholder size only: the herdr_tty TerminalManager overrides
            // cols/rows per pane from the pane's real size_info at spawn time.
            cols: 120,
            rows: 40,
        };
        let mut this = Self {
            api: api.clone(),
            reconciler: Reconciler::default(),
            workspace,
            pane_closed_tx,
            base_spec,
            connected: true,
            event_stream_generation: 0,
            watchdog_generation: Arc::new(AtomicU64::new(0)),
            pane_label_generation: Arc::new(AtomicU64::new(0)),
            pane_renames_outstanding: Arc::new(AtomicU64::new(0)),
            pane_rename_queue: PaneRenameQueue::default(),
            detached: Arc::new(AtomicBool::new(false)),
            event_stream_shutdown: None,
            subscribed_pane_ids: Vec::new(),
        };

        // 1. Apply the bootstrap snapshot through real workspace ops — but
        // deferred: start() runs inside ctx.add_model during a Workspace
        // update, and applying synchronously would re-enter the Workspace view
        // (panic: "Circular view update"). The spawn callback fires after the
        // current update cycle completes.
        ctx.spawn(
            async move { (snapshot, layouts) },
            |this: &mut Self, (snapshot, layouts), ctx| {
                this.with_ops(ctx, |rec, ops| {
                    ops.set_materializing(true);
                    rec.apply_snapshot(&snapshot, &layouts, ops);
                    ops.set_materializing(false);
                });
                this.consume_events(events_rx, shutdown, pane_ids, ctx);
            },
        );

        // 2. Per-pane stream deaths (takeover, pane gone, subprocess crash).
        // A bare crash (reason None) restarts the stream in place with
        // bounded retries via on_pane_stream_closed; only a refused restart
        // or a real herdr-side close tears the pane down.
        ctx.spawn_stream_local(
            pane_closed_rx,
            |this: &mut Self, (pane_id, reason): (String, Option<String>), ctx| {
                this.on_pane_stream_closed(&pane_id, reason.as_deref(), ctx);
            },
            |_, _| {},
        );

        // 3. Heartbeat watchdog: a half-alive server can hold the event
        // socket open yet stop sending — without periodic snapshots the
        // mirror looks live but stays silently stale.
        this.arm_watchdog(ctx);

        this
    }

    /// Wires consumption of a (re)subscribed event stream, superseding any
    /// previous one: the generation guard keeps a lingering stale stream (a
    /// half-alive server can hold it open past the watchdog's resync) from
    /// interleaving events or reporting a fresh disconnect when it finally
    /// closes.
    fn consume_events(
        &mut self,
        rx: async_channel::Receiver<AnyEvent>,
        shutdown: EventStreamHandle,
        subscribed_pane_ids: Vec<String>,
        ctx: &mut ModelContext<Self>,
    ) {
        // Record what the new subscription's pane-scoped entries cover, so
        // a refresh that would change nothing can skip resubscribing.
        self.subscribed_pane_ids = subscribed_pane_ids;
        self.subscribed_pane_ids.sort();
        // Close the superseded stream's socket: its reader thread parks in
        // read until the peer closes, leaking a thread + fd + server-side
        // subscription per resubscribe otherwise.
        if let Some(old) = self.event_stream_shutdown.replace(shutdown) {
            old.shutdown();
        }
        self.event_stream_generation += 1;
        let generation = self.event_stream_generation;
        ctx.spawn_stream_local(
            rx,
            move |this: &mut Self, event: AnyEvent, ctx| {
                if this.event_stream_generation != generation {
                    // Superseded stream: the current one owns the mirror (its
                    // subscribe replay covers anything this one delivered).
                    return;
                }
                match event {
                    AnyEvent::Lifecycle(env) => {
                        let suppress_pane_label = match &env.data {
                            EventData::PaneUpdated { pane } => {
                                this.pane_rename_queue.contains_pane(&pane.pane_id)
                            }
                            _ => false,
                        };
                        let pane_label_generation = this.pane_label_generation.clone();
                        this.with_ops(ctx, |rec, ops| {
                            apply_stream_event(
                                rec,
                                &pane_label_generation,
                                &env.data,
                                suppress_pane_label,
                                ops,
                            )
                        });
                        if this.check_intent_expiry(ctx) {
                            return;
                        }
                        this.refresh_pane_subscriptions(ctx);
                    }
                    AnyEvent::AgentStatus(ev) => {
                        this.with_ops(ctx, |rec, ops| rec.apply_agent_status(&ev, ops))
                    }
                    AnyEvent::Unknown => {}
                }
            },
            move |this: &mut Self, ctx| {
                if this.event_stream_generation != generation {
                    log::debug!("herdr: superseded event stream closed; ignoring");
                    return;
                }
                log::warn!("herdr: event stream closed; scheduling reconnect");
                this.note_disconnected(ctx);
            },
        );
    }

    /// Re-subscribes the event stream when the reconciler has learned pane
    /// ids the current subscription does not cover: pane.agent_status_changed
    /// is pane-scoped, and herdr sends status transitions through no other
    /// path, so a pane born after subscribe time would freeze at its
    /// creation-time status. The fresh stream supersedes the old one via
    /// consume_events' generation guard; replayed events are absorbed by the
    /// reconciler's idempotency.
    fn refresh_pane_subscriptions(&mut self, ctx: &mut ModelContext<Self>) {
        let Some(pane_ids) = self.reconciler.take_pane_subscription_refresh() else {
            return;
        };
        if !self.connected || self.is_detached() {
            // The reconnect/resync path subscribes with the full fresh set.
            return;
        }
        let api = self.api.clone();
        let currently_subscribed = self.subscribed_pane_ids.clone();
        ctx.spawn(
            async move {
                // Subscribe only to panes the server can resolve RIGHT NOW,
                // and not at all when the covered set wouldn't change — see
                // refreshed_subscribe_set for the two loops this prevents. A
                // failed snapshot falls through to the error arm: the server
                // really is unreachable then.
                let snapshot = api.session_snapshot()?;
                let Some(pane_ids) =
                    refreshed_subscribe_set(pane_ids, &snapshot, &currently_subscribed)
                else {
                    return anyhow::Ok(None);
                };
                let stream = api.subscribe_events(&pane_ids)?;
                Ok(Some((stream, pane_ids)))
            },
            |this: &mut Self, result, ctx| match result {
                Ok(Some(((events_rx, shutdown), pane_ids))) => {
                    this.consume_events(events_rx, shutdown, pane_ids, ctx)
                }
                Ok(None) => {} // covered set unchanged; keep the live stream
                Err(err) => {
                    log::error!("herdr: pane-subscription refresh failed: {err}");
                    this.note_disconnected(ctx);
                }
            },
        );
    }

    /// Marks the connection down and schedules reconnect. Disconnected (and
    /// its telemetry) is emitted — and the reconnect loop scheduled — only on
    /// an actual connected→disconnected transition: a duplicate close/failure
    /// report for the same outage (e.g. the watchdog and the stream close
    /// noticing together) must not re-emit or spawn a second loop.
    fn note_disconnected(&mut self, ctx: &mut ModelContext<Self>) {
        // The reconnect loop's own ping takes over liveness from here.
        self.disarm_watchdog();
        if self.connected {
            self.connected = false;
            send_telemetry_from_ctx!(TelemetryEvent::HerdrDisconnected, ctx);
            ctx.emit(HerdrConnectionEvent::Disconnected);
            self.schedule_reconnect(ctx);
        }
    }

    /// Detach begins: the Workspace drops this model right after, so every
    /// Warp-initiated op — including deferred callbacks still queued — must
    /// become a no-op (detach closes tabs locally WITHOUT touching herdr).
    pub fn mark_detached(&mut self) {
        self.detached.store(true, Ordering::SeqCst);
        self.disarm_watchdog();
        if let Some(stream) = self.event_stream_shutdown.take() {
            stream.shutdown();
        }
    }

    fn is_detached(&self) -> bool {
        self.detached.load(Ordering::SeqCst)
    }

    /// Detach gate shared by every Warp-initiated operation: a deferred
    /// call arriving after detach is dropped visibly rather than silently.
    fn ignore_if_detached(&self, op: std::fmt::Arguments) -> bool {
        if self.is_detached() {
            log::debug!("herdr: detached; ignoring {op}");
            return true;
        }
        false
    }

    /// Bridges the pure reconciler to the real Workspace. Builds a WorkspaceOps
    /// impl borrowing the workspace view for the duration of one event.
    fn with_ops(
        &mut self,
        ctx: &mut ModelContext<Self>,
        f: impl FnOnce(&mut Reconciler, &mut dyn WorkspaceOps),
    ) {
        let Some(workspace) = self.workspace.upgrade(ctx) else {
            return;
        };
        let api = self.api.clone();
        let base_spec = self.base_spec.clone();
        let pane_closed_tx = self.pane_closed_tx.clone();
        let reconciler = &mut self.reconciler;
        workspace.update(ctx, |ws, ctx| {
            let mut ops = RealWorkspaceOps {
                ws,
                ctx,
                api,
                base_spec,
                pane_closed_tx,
            };
            f(reconciler, &mut ops);
        });
    }

    // Warp-initiated operations (called from Workspace interception):
    pub fn close_tab(&mut self, herdr_tab_id: &str, ctx: &mut ModelContext<Self>) {
        if self.ignore_if_detached(format_args!("close_tab {herdr_tab_id}")) {
            return;
        }
        if !self.reconciler.is_known_tab(herdr_tab_id) {
            return; // already gone herdr-side; this local close is the echo
        }
        let intent = Intent::CloseTab(herdr_tab_id.to_string());
        self.reconciler.note_local_intent(intent.clone());
        let api = self.api.clone();
        let id = herdr_tab_id.to_string();
        self.call_api(Some(intent), ctx, move || api.tab_close(&id));
    }

    pub fn close_pane(&mut self, herdr_pane_id: &str, ctx: &mut ModelContext<Self>) {
        if self.ignore_if_detached(format_args!("close_pane {herdr_pane_id}")) {
            return;
        }
        if self.reconciler.tab_of_pane(herdr_pane_id).is_none() {
            return; // already gone herdr-side; this local close is the echo
        }
        let intent = Intent::ClosePane(herdr_pane_id.to_string());
        self.reconciler.note_local_intent(intent.clone());
        let api = self.api.clone();
        let id = herdr_pane_id.to_string();
        self.call_api(Some(intent), ctx, move || api.pane_close(&id));
    }

    pub fn rename_tab(&mut self, herdr_tab_id: &str, label: &str, ctx: &mut ModelContext<Self>) {
        if self.ignore_if_detached(format_args!("rename_tab {herdr_tab_id}")) {
            return;
        }
        let intent = Intent::RenameTab {
            tab_id: herdr_tab_id.to_string(),
            label: label.to_string(),
        };
        self.reconciler.note_local_intent(intent.clone());
        let api = self.api.clone();
        let id = herdr_tab_id.to_string();
        let label = label.to_string();
        self.call_api(Some(intent), ctx, move || api.tab_rename(&id, &label));
    }

    pub fn rename_pane(
        &mut self,
        herdr_pane_id: &str,
        label: Option<&str>,
        ctx: &mut ModelContext<Self>,
    ) {
        if self.ignore_if_detached(format_args!("rename_pane {herdr_pane_id}")) {
            return;
        }
        if self.reconciler.tab_of_pane(herdr_pane_id).is_none() {
            return;
        }
        // Mark every rename outstanding before changing the label epoch. A
        // coalesced rename drops this temporary guard while the active chain's
        // guard remains held, so the counter never reaches zero between
        // queued values.
        let outstanding_rename = OutstandingPaneRename::new(self.pane_renames_outstanding.clone());
        note_local_pane_label_mutation(
            &mut self.reconciler,
            &self.pane_label_generation,
            herdr_pane_id,
            label,
        );
        let generation = self.watchdog_generation.load(Ordering::SeqCst);
        let request = PaneRenameRequest {
            pane_id: herdr_pane_id.to_string(),
            label: label.map(str::to_string),
        };
        if let Some(request) = self.pane_rename_queue.enqueue(request, generation) {
            self.schedule_pane_rename(request, generation, outstanding_rename, ctx);
        }
    }

    fn schedule_pane_rename(
        &mut self,
        request: PaneRenameRequest,
        generation: u64,
        outstanding_rename: OutstandingPaneRename,
        ctx: &mut ModelContext<Self>,
    ) {
        let api = self.api.clone();
        let pane_id = request.pane_id.clone();
        ctx.spawn(
            async move {
                let result = api.pane_rename(&request.pane_id, request.label.as_deref());
                (outstanding_rename, result)
            },
            move |this: &mut Self, (outstanding_rename, result), ctx| {
                let current_generation = this.watchdog_generation.load(Ordering::SeqCst);
                let detached = this.is_detached();
                let completion = complete_pane_rename_response(
                    &mut this.reconciler,
                    &mut this.pane_rename_queue,
                    &pane_id,
                    generation,
                    generation == current_generation && !detached,
                    result,
                );
                if let Some((request, next_generation)) = pane_rename_to_schedule(
                    &mut this.pane_rename_queue,
                    &pane_id,
                    completion,
                    current_generation,
                    detached,
                ) {
                    this.schedule_pane_rename(request, next_generation, outstanding_rename, ctx);
                }
            },
        );
    }

    pub fn create_tab(&mut self, herdr_workspace_id: &str, ctx: &mut ModelContext<Self>) {
        if self.ignore_if_detached(format_args!("create_tab")) {
            return;
        }
        // The tab.create response carries the new tab + root pane; materialize
        // directly from the response instead of waiting for events.
        let api = self.api.clone();
        let ws_id = herdr_workspace_id.to_string();
        // ctx.spawn runs the future on the background executor and routes the
        // output back to this model on the main thread.
        ctx.spawn(
            async move { api.tab_create(&ws_id, None) },
            move |this: &mut Self, result, ctx| match result {
                Ok((tab, root_pane)) => {
                    let layout = HerdrPaneTree::Leaf {
                        pane_id: root_pane.pane_id.clone(),
                        cwd: root_pane.cwd.clone(),
                        agent: root_pane.agent.clone(),
                        agent_status: root_pane.agent_status,
                    };
                    this.with_ops(ctx, |rec, ops| {
                        ops.set_materializing(true);
                        rec.apply_layout(&tab, &layout, ops);
                        ops.set_materializing(false);
                    });
                }
                Err(err) => log::error!("herdr tab.create failed: {err}"),
            },
        );
    }

    pub fn split_pane(
        &mut self,
        herdr_pane_id: &str,
        direction: HerdrSplitDirection,
        ctx: &mut ModelContext<Self>,
    ) {
        if self.ignore_if_detached(format_args!("split_pane {herdr_pane_id}")) {
            return;
        }
        let api = self.api.clone();
        let id = herdr_pane_id.to_string();
        ctx.spawn(
            async move { api.pane_split(&id, direction, None) },
            move |this: &mut Self, result, ctx| match result {
                Ok(pane) => {
                    // Local split: add the herdr-backed pane beside the target.
                    this.with_ops(ctx, |rec, ops| {
                        rec.apply_event(&EventData::PaneCreated { pane }, ops)
                    });
                }
                Err(err) => log::error!("herdr pane.split failed: {err}"),
            },
        );
    }

    pub fn focus_tab(&mut self, herdr_tab_id: &str, ctx: &mut ModelContext<Self>) {
        if self.ignore_if_detached(format_args!("focus_tab {herdr_tab_id}")) {
            return;
        }
        let api = self.api.clone();
        let id = herdr_tab_id.to_string();
        self.call_api(None, ctx, move || api.tab_focus(&id));
    }

    /// Mirrors a Warp-side tab reorder to herdr: `insert_index` is the tab's
    /// new 0-based index within its herdr workspace's tab list. The echoed
    /// tab.moved is swallowed by the recorded intent; a failed call rolls the
    /// intent back so a later herdr-initiated move still applies.
    pub fn move_tab(
        &mut self,
        herdr_tab_id: String,
        insert_index: usize,
        ctx: &mut ModelContext<Self>,
    ) {
        if self.ignore_if_detached(format_args!("move_tab {herdr_tab_id}")) {
            return;
        }
        if !self.reconciler.is_known_tab(&herdr_tab_id) {
            return; // already gone herdr-side; nothing to move
        }
        let intent = Intent::MoveTab(herdr_tab_id.clone());
        self.reconciler.note_local_intent(intent.clone());
        let api = self.api.clone();
        self.call_api(Some(intent), ctx, move || {
            api.tab_move(&herdr_tab_id, insert_index)
        });
    }

    /// Late layout arrival from a request_layout_refresh follow-up. Deferred
    /// for the same reason as the bootstrap snapshot: the caller may hold the
    /// Workspace view (re-entering it would panic with "Circular view update").
    pub fn apply_refreshed_layout(
        &mut self,
        tab: &TabInfo,
        layout: &HerdrPaneTree,
        ctx: &mut ModelContext<Self>,
    ) {
        if self.ignore_if_detached(format_args!("apply_refreshed_layout")) {
            return;
        }
        let tab = tab.clone();
        let layout = layout.clone();
        ctx.spawn(
            async move { (tab, layout) },
            |this: &mut Self, (tab, layout), ctx| {
                this.with_ops(ctx, |rec, ops| {
                    ops.set_materializing(true);
                    rec.apply_layout(&tab, &layout, ops);
                    ops.set_materializing(false);
                });
            },
        );
    }

    /// Runs a blocking API call on the background executor. `intent` is the
    /// pending intent recorded for the call (if any): on failure it is rolled
    /// back so it can't swallow the next herdr-initiated event for the target.
    fn call_api(
        &self,
        intent: Option<Intent>,
        ctx: &mut ModelContext<Self>,
        f: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
    ) {
        ctx.spawn(async move { f() }, |this: &mut Self, result, ctx| {
            resolve_intent_on_api_result(&mut this.reconciler, intent, result);
            // Sweep here too: on a fully quiet stream no event ever runs the
            // lazy sweep, but the user keeps acting — this is what catches a
            // dead stream while its socket sits open (seen live 2026-07-21:
            // five tab.moves completed "ok" with zero echoed broadcasts).
            this.reconciler.expire_stale_intents();
            this.check_intent_expiry(ctx);
        });
    }

    /// Resyncs when an intent expired unconsumed: pending intents only
    /// survive successful API calls, so herdr HAS broadcast the echoed
    /// event — expiry proves the event stream is silently not delivering
    /// (its socket can sit open while nothing arrives; the watchdog's
    /// snapshots only reconcile pane labels). True when a resync was
    /// scheduled.
    fn check_intent_expiry(&mut self, ctx: &mut ModelContext<Self>) -> bool {
        if !self.reconciler.take_intent_expiry() {
            return false;
        }
        log::warn!(
            "herdr: a local intent expired with no echoed event; \
             the event stream missed broadcasts — resyncing"
        );
        self.note_disconnected(ctx);
        true
    }

    fn on_pane_stream_closed(
        &mut self,
        pane_id: &str,
        reason: Option<&str>,
        ctx: &mut ModelContext<Self>,
    ) {
        if !self.connected {
            // Late-draining death of a stream from the unreachable server.
            // The resync rebuild re-creates every pane, so applying this now
            // could only tear down a freshly rebuilt pane.
            log::info!("herdr pane {pane_id}: stream close while disconnected; ignoring");
            return;
        }
        match reason {
            Some(REASON_ATTACH_TAKEN_OVER) => {
                log::info!("herdr pane {pane_id}: control taken over elsewhere");
                // The pane stays open with its takeover notice. Only an explicit reconnect
                // should retake control, otherwise clients would continually evict each other.
            }
            None => {
                // Subprocess died while the server is alive: the pane may
                // still exist herdr-side, so restart the stream in place
                // (bounded retries on the manager; the pane keeps its last
                // frame — no tab churn). Only a refused restart — budget
                // exhausted, or no mirrored tab tracks the pane (e.g. a
                // local-shell fallback pane) — falls through to a close.
                let restarted = self
                    .workspace
                    .upgrade(ctx)
                    .map(|workspace| {
                        workspace.update(ctx, |ws, ctx| ws.herdr_restart_stream(pane_id, ctx))
                    })
                    .unwrap_or(false);
                if restarted {
                    log::info!("herdr pane {pane_id}: stream died; restart queued");
                    return;
                }
                log::info!("herdr pane {pane_id}: stream died; restart refused, closing pane");
                self.close_pane_locally(pane_id, ctx);
            }
            Some(_) => {
                // Pane truly gone herdr-side: reflect as pane close.
                self.close_pane_locally(pane_id, ctx);
            }
        }
    }

    /// Reflects a herdr-side pane loss (stream dead past the restart budget,
    /// or the pane closed herdr-side) as a local pane close.
    fn close_pane_locally(&mut self, pane_id: &str, ctx: &mut ModelContext<Self>) {
        self.with_ops(ctx, |rec, ops| {
            rec.apply_event(
                &EventData::PaneClosed {
                    pane_id: pane_id.to_string(),
                    workspace_id: String::new(),
                },
                ops,
            )
        });
    }

    /// Arms the heartbeat watchdog: a snapshot every HEARTBEAT_INTERVAL on
    /// the background executor, with a failure taking the same path as an
    /// event-stream death. Arming bumps the generation, retiring any previous
    /// probe chain, so at most one watchdog is live at a time.
    fn arm_watchdog(&mut self, ctx: &mut ModelContext<Self>) {
        let generation = self.watchdog_generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.schedule_watchdog_probe(generation, ctx);
    }

    fn schedule_watchdog_probe(&mut self, armed_generation: u64, ctx: &mut ModelContext<Self>) {
        let api = self.api.clone();
        let shared_generation = self.watchdog_generation.clone();
        let pane_label_generation = self.pane_label_generation.clone();
        let pane_renames_outstanding = self.pane_renames_outstanding.clone();
        ctx.spawn(
            async move {
                watchdog_snapshot_probe(
                    api,
                    shared_generation,
                    armed_generation,
                    pane_label_generation,
                    pane_renames_outstanding,
                    HEARTBEAT_INTERVAL,
                )
                .await
            },
            move |this: &mut Self, result, ctx| {
                if this.watchdog_generation.load(Ordering::SeqCst) != armed_generation {
                    // Disarmed/re-armed while the report crossed to the main
                    // thread: liveness ownership has already moved on.
                    log::debug!("herdr: ignoring stale watchdog snapshot");
                    return;
                }
                match result {
                    Ok(Some(snapshot)) => {
                        let current_pane_label_generation =
                            this.pane_label_generation.load(Ordering::SeqCst);
                        let pane_rename_outstanding =
                            this.pane_renames_outstanding.load(Ordering::SeqCst) != 0;
                        this.with_ops(ctx, |reconciler, ops| {
                            reconcile_watchdog_snapshot_labels(
                                reconciler,
                                &snapshot,
                                current_pane_label_generation,
                                pane_rename_outstanding,
                                ops,
                            );
                        });
                        this.schedule_watchdog_probe(armed_generation, ctx);
                    }
                    Ok(None) => {}
                    Err(err) => {
                        log::warn!(
                            "herdr: heartbeat snapshot failed; treating as disconnected: {err}"
                        );
                        this.note_disconnected(ctx);
                    }
                }
            },
        );
    }

    /// Stops the heartbeat watchdog by retiring its generation: on disconnect
    /// the reconnect loop's own ping takes over liveness, and on detach the
    /// whole connection is dropped right after.
    fn disarm_watchdog(&mut self) {
        self.watchdog_generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Capped exponential backoff, retried indefinitely (design doc): the
    /// connection never gives up on its own — only a successful resync
    /// recovers it.
    fn schedule_reconnect(&mut self, ctx: &mut ModelContext<Self>) {
        if self.is_detached() {
            log::debug!("herdr: detached; not scheduling reconnect");
            return;
        }
        let api = self.api.clone();
        let detached = self.detached.clone();
        ctx.spawn(
            async move {
                let backoff = [1u64, 2, 4, 8, 15]
                    .map(std::time::Duration::from_secs)
                    .to_vec();
                reconnect_loop(api, detached, backoff, std::time::Duration::from_secs(30)).await
            },
            |this: &mut Self, reachable, ctx| {
                if reachable {
                    log::info!("herdr: reachable again; resyncing");
                    this.resync(ctx);
                }
            },
        );
    }

    /// Full rebuild after a server restart: bootstrap + connect/subscribe run
    /// inside the background future (socket.rs blocks and must never run on
    /// the UI thread); the main thread only applies the rebuild and wires up
    /// event consumption.
    fn resync(&mut self, ctx: &mut ModelContext<Self>) {
        if self.is_detached() {
            log::debug!("herdr: detached; skipping resync");
            return;
        }
        let api = self.api.clone();
        ctx.spawn(
            async move { Self::bootstrap_subscribed(&api) },
            |this: &mut Self, result, ctx| match result {
                Ok(HerdrBootstrap {
                    snapshot,
                    layouts,
                    events_rx,
                    shutdown,
                    pane_ids,
                }) => {
                    if this.is_detached() {
                        // Detached while the bootstrap was in flight: don't
                        // re-materialize tabs in a workspace with no session.
                        log::debug!("herdr: detached during resync; discarding rebuild");
                        return;
                    }
                    this.with_ops(ctx, move |rec, ops| {
                        rebuild_from_snapshot(rec, &snapshot, &layouts, ops)
                    });
                    log::info!("herdr: rebuilt from snapshot and re-subscribed");
                    if !this.connected {
                        this.connected = true;
                        send_telemetry_from_ctx!(TelemetryEvent::HerdrReconnected, ctx);
                        ctx.emit(HerdrConnectionEvent::Reconnected);
                    }
                    this.consume_events(events_rx, shutdown, pane_ids, ctx);
                    // Re-arm the heartbeat watchdog (disarmed when the
                    // connection went down): the reconnect loop has handed
                    // liveness back.
                    this.arm_watchdog(ctx);
                }
                Err(err) => {
                    // Bootstrap or re-subscribe failed: without a live event
                    // stream the mirror is dead; go back through the
                    // reconnect loop — and surface the reason, or a server
                    // that came back below the version gate fails here
                    // forever with the error visible only in logs.
                    log::error!("herdr resync failed: {err}; retrying");
                    ctx.emit(HerdrConnectionEvent::ResyncFailed {
                        message: format!("herdr resync failed: {err}"),
                    });
                    this.schedule_reconnect(ctx);
                }
            },
        );
    }
}

struct WatchdogSnapshot {
    snapshot: SessionSnapshot,
    pane_label_generation: u64,
    pane_rename_outstanding: bool,
}

struct OutstandingPaneRename {
    counter: Arc<AtomicU64>,
}

impl OutstandingPaneRename {
    fn new(counter: Arc<AtomicU64>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self { counter }
    }
}

impl Drop for OutstandingPaneRename {
    fn drop(&mut self) {
        let previous = self.counter.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(previous > 0, "outstanding pane rename counter underflow");
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneRenameRequest {
    pane_id: String,
    label: Option<String>,
}

struct PaneRenameState {
    active_generation: u64,
    pending: Option<(PaneRenameRequest, u64)>,
}

#[derive(Default)]
struct PaneRenameQueue {
    by_pane: HashMap<String, PaneRenameState>,
}

impl PaneRenameQueue {
    /// Returns the request to start now, or None when it was coalesced behind
    /// the pane's active request.
    fn enqueue(
        &mut self,
        request: PaneRenameRequest,
        generation: u64,
    ) -> Option<PaneRenameRequest> {
        match self.by_pane.entry(request.pane_id.clone()) {
            Entry::Occupied(mut entry) => {
                entry.get_mut().pending = Some((request, generation));
                None
            }
            Entry::Vacant(entry) => {
                entry.insert(PaneRenameState {
                    active_generation: generation,
                    pending: None,
                });
                Some(request)
            }
        }
    }

    fn complete(&mut self, pane_id: &str, generation: u64) -> PaneRenameCompletion {
        let Some(state) = self.by_pane.get_mut(pane_id) else {
            return PaneRenameCompletion::Stale;
        };
        if state.active_generation != generation {
            return PaneRenameCompletion::Stale;
        }
        if let Some((request, generation)) = state.pending.take() {
            state.active_generation = generation;
            return PaneRenameCompletion::Next {
                request,
                generation,
            };
        }
        self.by_pane.remove(pane_id);
        PaneRenameCompletion::Done
    }

    fn cancel(&mut self, pane_id: &str, generation: u64) {
        if self.is_active_generation(pane_id, generation) {
            self.by_pane.remove(pane_id);
        }
    }

    fn is_active_generation(&self, pane_id: &str, generation: u64) -> bool {
        self.by_pane
            .get(pane_id)
            .is_some_and(|state| state.active_generation == generation)
    }

    fn contains_pane(&self, pane_id: &str) -> bool {
        self.by_pane.contains_key(pane_id)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PaneRenameCompletion {
    Next {
        request: PaneRenameRequest,
        generation: u64,
    },
    Done,
    Stale,
}

fn complete_pane_rename(
    queue: &mut PaneRenameQueue,
    pane_id: &str,
    generation: u64,
    result: anyhow::Result<()>,
) -> PaneRenameCompletion {
    if let Err(err) = result {
        log::error!("herdr pane.rename {pane_id} failed: {err}");
    }
    queue.complete(pane_id, generation)
}

fn complete_pane_rename_response(
    reconciler: &mut Reconciler,
    queue: &mut PaneRenameQueue,
    pane_id: &str,
    generation: u64,
    allow_ack: bool,
    result: anyhow::Result<PaneInfo>,
) -> PaneRenameCompletion {
    let allow_ack = allow_ack && queue.is_active_generation(pane_id, generation);
    let result = result.map(|pane| {
        if allow_ack {
            reconciler.note_local_pane_rename_ack(pane_id, &pane);
        }
    });
    complete_pane_rename(queue, pane_id, generation, result)
}

fn pane_rename_to_schedule(
    queue: &mut PaneRenameQueue,
    pane_id: &str,
    completion: PaneRenameCompletion,
    current_generation: u64,
    detached: bool,
) -> Option<(PaneRenameRequest, u64)> {
    let PaneRenameCompletion::Next {
        request,
        generation,
    } = completion
    else {
        return None;
    };
    if detached || generation != current_generation {
        queue.cancel(pane_id, generation);
        return None;
    }
    Some((request, generation))
}

fn note_local_pane_label_mutation(
    reconciler: &mut Reconciler,
    pane_label_generation: &AtomicU64,
    pane_id: &str,
    label: Option<&str>,
) {
    reconciler.note_local_pane_label(pane_id, label);
    pane_label_generation.fetch_add(1, Ordering::SeqCst);
}

fn apply_stream_event(
    reconciler: &mut Reconciler,
    pane_label_generation: &AtomicU64,
    event: &EventData,
    suppress_pane_label: bool,
    ops: &mut dyn WorkspaceOps,
) {
    if !suppress_pane_label && reconciler.event_changes_pane_label(event) {
        pane_label_generation.fetch_add(1, Ordering::SeqCst);
    }
    reconciler.apply_event_with_pane_label(event, !suppress_pane_label, ops);
}

fn reconcile_watchdog_snapshot_labels(
    reconciler: &mut Reconciler,
    snapshot: &WatchdogSnapshot,
    current_pane_label_generation: u64,
    pane_rename_outstanding: bool,
    ops: &mut dyn WorkspaceOps,
) -> bool {
    if snapshot.pane_label_generation != current_pane_label_generation
        || snapshot.pane_rename_outstanding
        || pane_rename_outstanding
    {
        return false;
    }
    reconciler.reconcile_pane_labels(&snapshot.snapshot.panes, ops);
    true
}

/// Pings with capped backoff until the server answers (→ true) or the
/// connection detaches (→ false). The detach check on every wake matters: a
/// detach (or window close) during an outage must stop the loop, or it pings
/// the dead socket every `cap` for the app's lifetime.
async fn reconnect_loop(
    api: HerdrApi,
    detached: Arc<AtomicBool>,
    initial_delays: Vec<std::time::Duration>,
    cap: std::time::Duration,
) -> bool {
    for delay in initial_delays {
        warpui::r#async::Timer::after(delay).await;
        if detached.load(Ordering::SeqCst) {
            return false;
        }
        if api.ping().is_ok() {
            return true;
        }
    }
    loop {
        warpui::r#async::Timer::after(cap).await;
        if detached.load(Ordering::SeqCst) {
            return false;
        }
        if api.ping().is_ok() {
            return true;
        }
    }
}

async fn watchdog_snapshot_probe(
    api: HerdrApi,
    generation: Arc<AtomicU64>,
    armed_generation: u64,
    pane_label_generation: Arc<AtomicU64>,
    pane_renames_outstanding: Arc<AtomicU64>,
    interval: std::time::Duration,
) -> anyhow::Result<Option<WatchdogSnapshot>> {
    warpui::r#async::Timer::after(interval).await;
    if generation.load(Ordering::SeqCst) != armed_generation {
        return Ok(None);
    }
    let pane_label_generation = pane_label_generation.load(Ordering::SeqCst);
    let pane_rename_outstanding = pane_renames_outstanding.load(Ordering::SeqCst) != 0;
    let snapshot = api.session_snapshot();
    // Re-check AFTER the (blocking, possibly slow) request: a disarm landing
    // while it was in flight means liveness ownership moved on, so neither a
    // stale snapshot nor a stale failure may affect the replacement.
    if generation.load(Ordering::SeqCst) != armed_generation {
        return Ok(None);
    }
    snapshot.map(|snapshot| {
        Some(WatchdogSnapshot {
            snapshot,
            pane_label_generation,
            pane_rename_outstanding,
        })
    })
}

/// Completes a Warp-initiated API call: on failure, roll the recorded intent
/// back — a stale intent would otherwise swallow the next herdr-initiated
/// event for the same target.
/// The pane set a subscription refresh should subscribe with, or None when
/// no resubscribe is needed. Two defenses, both proven live (2026-07-21):
///
/// 1. Only panes the fresh snapshot still has are named: herdr rejects the
///    ENTIRE events.subscribe over one pane id it cannot resolve ("failed
///    to decode pane get error"), and treating that as an outage looped
///    disconnect→resync→replay→disconnect. Dead ids reach the refresh set
///    via closes missed on a quiet stream, or via a replayed pane.created
///    applied an instant before its pane.closed replay.
/// 2. A filtered set identical to what the live subscription already
///    covers does not resubscribe at all: every resubscribe replays up to
///    512 backlog events, a replayed created/closed pair for a since-dead
///    pane re-marks the subscription stale, and the resulting
///    resubscribe→replay→stale cycle looped once per second — with the
///    replayed tab.moved history visibly flapping the tab order each pass.
///
/// Only the subscribe set is computed here; the mirror's own pane state
/// still heals through events and resyncs.
fn refreshed_subscribe_set(
    refresh_ids: Vec<String>,
    snapshot: &SessionSnapshot,
    currently_subscribed: &[String],
) -> Option<Vec<String>> {
    let live: std::collections::HashSet<&str> =
        snapshot.panes.iter().map(|p| p.pane_id.as_str()).collect();
    let filtered: Vec<String> = refresh_ids
        .into_iter()
        .filter(|id| live.contains(id.as_str()))
        .collect();
    (filtered != currently_subscribed).then_some(filtered)
}

fn resolve_intent_on_api_result(
    reconciler: &mut Reconciler,
    intent: Option<Intent>,
    result: anyhow::Result<()>,
) {
    if let Err(err) = result {
        if let Some(intent) = intent {
            reconciler.remove_intent(intent);
        }
        log::error!("herdr api call failed: {err}");
    }
}

/// Full rebuild from a fresh snapshot after a server restart: every pane
/// stream died with the old server, so local mirror state is stale no matter
/// how much of the teardown cascade already ran. Forget ALL state first —
/// pending intents included, so nothing learned from the dead server can
/// route API calls to the new one or swallow its events — close every locally
/// materialized tab in one batch, then re-materialize exactly like a fresh
/// attach, all behind a single materializing guard.
fn rebuild_from_snapshot(
    reconciler: &mut Reconciler,
    snapshot: &SessionSnapshot,
    layouts: &[(String, HerdrPaneTree)],
    ops: &mut dyn WorkspaceOps,
) {
    let stale = reconciler.take_stale_tabs_for_resync();
    ops.set_materializing(true);
    for tab_id in &stale {
        ops.close_tab(tab_id);
    }
    reconciler.apply_snapshot(snapshot, layouts, ops);
    ops.set_materializing(false);
    // The teardown can close the tab the materializing guard captured as its
    // restore target, leaving a rebuilt (deferred-stream) tab active with no
    // fresh activation ever firing the first-view attach trigger — attach the
    // active tab's streams explicitly so it doesn't sit on an empty grid.
    ops.attach_active_tab_streams();
}

/// PATH lookup for the stream data plane's `herdr` binary, via the shared
/// shell-matching resolver (executability check, PATHEXT on Windows,
/// deduped PATH entries).
#[cfg(not(target_family = "wasm"))]
fn herdr_bin_on_path() -> Option<PathBuf> {
    crate::util::path::resolve_executable("herdr").map(|path| path.into_owned())
}

/// wasm has no PATH or subprocesses; the data plane never runs there.
#[cfg(target_family = "wasm")]
fn herdr_bin_on_path() -> Option<PathBuf> {
    None
}

/// Warn-only data-plane check (blocking; run on the background executor):
/// resolves the `herdr` binary pane streams will spawn and compares its
/// `--version` against the attached server's. Returns the warning to log,
/// or None when everything matches (or the version can't be read — the
/// spawn path reports its own failures).
fn stream_binary_mismatch_warning(server_version: &str) -> Option<String> {
    let Some(bin) = herdr_bin_on_path() else {
        return Some("herdr: `herdr` binary not found on PATH; pane streams will fail".to_string());
    };
    let output = command::blocking::Command::new(&bin)
        .arg("--version")
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let bin_version = stdout.split_whitespace().last()?.to_string();
    (bin_version != server_version).then(|| {
        format!(
            "herdr: PATH binary {} is v{bin_version} but the attached server is v{server_version}; pane streams may misbehave",
            bin.display()
        )
    })
}

/// The ONLY place reconciler effects touch the GUI.
struct RealWorkspaceOps<'a, 'b> {
    ws: &'a mut crate::workspace::Workspace,
    ctx: &'a mut warpui::ViewContext<'b, crate::workspace::Workspace>,
    api: HerdrApi,
    base_spec: HerdrStreamSpec,
    pane_closed_tx: async_channel::Sender<(String, Option<String>)>,
}

impl WorkspaceOps for RealWorkspaceOps<'_, '_> {
    fn create_workspace_group(&mut self, workspace: &WorkspaceInfo) {
        self.ws
            .herdr_ensure_tab_group(&workspace.workspace_id, &workspace.label, self.ctx);
    }
    fn create_tab(&mut self, tab: &TabInfo, layout: &HerdrPaneTree) {
        self.ws.herdr_add_tab(
            tab,
            layout,
            &self.base_spec,
            self.pane_closed_tx.clone(),
            self.ctx,
        );
    }
    fn close_tab(&mut self, tab_id: &str) {
        self.ws.herdr_close_tab(tab_id, self.ctx);
    }
    fn rename_tab(&mut self, tab_id: &str, label: &str) {
        self.ws.herdr_rename_tab(tab_id, label, self.ctx);
    }
    fn apply_tab_order(&mut self, workspace_id: &str, ordered_tab_ids: &[String]) {
        self.ws
            .herdr_apply_tab_order(workspace_id, ordered_tab_ids, self.ctx);
    }
    fn set_tab_status(&mut self, tab_id: &str, status: AgentStatus) {
        self.ws.herdr_set_tab_status(tab_id, status, self.ctx);
    }
    fn add_split(&mut self, tab_id: &str, pane: &PaneInfo) {
        self.ws.herdr_add_split(
            tab_id,
            pane,
            &self.base_spec,
            self.pane_closed_tx.clone(),
            self.ctx,
        );
    }
    fn sync_tab_layout(&mut self, tab_id: &str, layout: &HerdrPaneTree) {
        self.ws.herdr_sync_tab_layout(
            tab_id,
            layout,
            &self.base_spec,
            &self.pane_closed_tx,
            self.ctx,
        );
    }
    fn close_pane(&mut self, pane_id: &str) {
        self.ws.herdr_close_pane(pane_id, self.ctx);
    }
    fn set_pane_title(&mut self, pane_id: &str, title: &str) {
        log::debug!("herdr: set_pane_title {pane_id}");
        self.ws.herdr_set_pane_title(pane_id, title, self.ctx);
    }
    fn set_pane_label(&mut self, pane_id: &str, label: Option<&str>) {
        log::debug!("herdr: set_pane_label {pane_id}");
        self.ws.herdr_set_pane_label(pane_id, label, self.ctx);
    }
    fn set_pane_agent(&mut self, pane_id: &str, agent: Option<&str>) {
        log::debug!("herdr: set_pane_agent {pane_id}");
        self.ws.herdr_set_pane_agent(pane_id, agent, self.ctx);
    }
    fn set_pane_agent_status(&mut self, pane_id: &str, status: AgentStatus) {
        self.ws
            .herdr_set_pane_agent_status(pane_id, status, self.ctx);
    }
    fn request_layout_refresh(&mut self, tab_id: &str) {
        // Fire-and-forget: fresh export applied on completion via the model.
        self.ws
            .herdr_request_layout_refresh(self.api.clone(), tab_id.to_string(), self.ctx);
    }
    fn attach_active_tab_streams(&mut self) {
        self.ws.herdr_attach_active_tab_streams(self.ctx);
    }
    fn set_materializing(&mut self, active: bool) {
        self.ws.set_herdr_materializing(active);
    }
}

#[cfg(test)]
#[path = "connection_tests.rs"]
mod tests;
