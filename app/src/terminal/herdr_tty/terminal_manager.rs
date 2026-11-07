use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;

use async_channel::Sender;
use instant::{Duration, Instant};
use parking_lot::FairMutex;
use pathfinder_geometry::vector::Vector2F;
use warp_errors::report_error;
use warpui::{AppContext, ModelHandle, ViewHandle, WindowId};

use crate::context_chips::prompt_type::PromptType;
use crate::pane_group::TerminalViewResources;
use crate::pane_group::pane::DetachType;
use crate::persistence::ModelEvent;
use crate::terminal::event_listener::ChannelEventListener;
use crate::terminal::herdr_tty::event_loop::EventLoop;
use crate::terminal::herdr_tty::stream::{
    FRAMES_CHANNEL_CAPACITY, HerdrStream, HerdrStreamSpec, StreamEvent, StreamSlot,
    StreamSpawnRequest, attach_spawned_stream, enqueue_stream_spawn,
};
use crate::terminal::model::session::Sessions;
use crate::terminal::model_events::ModelEventDispatcher;
use crate::terminal::shell::{ShellName, ShellType};
use crate::terminal::terminal_manager::BlockSpacing;
use crate::terminal::view::Event as TerminalViewEvent;
use crate::terminal::writeable_pty::terminal_manager_util::{
    init_pty_controller_model, wire_up_pty_controller_with_surface,
};
use crate::terminal::writeable_pty::{self, Message};
use crate::terminal::{
    ShellLaunchState, SizeInfo, TerminalManager as TerminalManagerTrait, TerminalModel,
    TerminalView, terminal_manager,
};

type PtyController = writeable_pty::PtyController<Sender<Message>>;

pub struct TerminalManager {
    model: Arc<FairMutex<TerminalModel>>,

    /// Used to gracefully release the herdr stream when the pane is
    /// permanently closed (see `on_view_detached`).
    event_loop_tx: Sender<Message>,

    /// The late-bound herdr stream shared with the event loop's writer task:
    /// filled by the attach completion (the eager spawn at creation, or the
    /// global spawn worker for deferred panes and restarts), emptied by the
    /// event loop when the subprocess dies (see `restart_stream`).
    stream_slot: StreamSlot,

    /// Everything needed to (re)attach the pane's stream through the global
    /// spawn queue, stored on every manager — eager included — so a dead
    /// stream can be restarted via the same machinery as a deferred first
    /// attach.
    attach_params: AttachParams,

    /// Set while a spawn request sits in the global queue; cleared by the
    /// worker once the request settles so a later `ensure_stream_attached`
    /// or `restart_stream` can queue the next spawn.
    attach_in_flight: Arc<AtomicBool>,

    /// Set by the event loop when the stream ended with a reasoned Closed
    /// record (attach taken over elsewhere, pane gone herdr-side): the empty
    /// slot is deliberate, and `ensure_stream_attached` must not respawn on
    /// mere tab activation — retaking a taken-over terminal is a deliberate
    /// act. An explicit `restart_stream` clears it.
    stream_released: Arc<AtomicBool>,

    /// Bounds restarts after a subprocess death so a crash-looping
    /// `herdr terminal session control` doesn't fork in a tight loop.
    restart_budget: FairMutex<RestartBudget>,

    // Store a reference to the PTYController and EventLoop so the UI framework doesn't end up
    // deallocating them because there are no strong references to the models.
    _pty_controller: ModelHandle<PtyController>,

    _event_loop: ModelHandle<EventLoop>,

    _view: ViewHandle<TerminalView>,
}

/// The pane's stream attach state: the finalized spawn spec plus the ends of
/// the channels that go live once a queued spawn completes. Kept (not
/// consumed) after queueing so a failed spawn — or a dead stream — can be
/// re-queued later.
struct AttachParams {
    spec: HerdrStreamSpec,
    frames_relay_tx: Sender<StreamEvent>,
    pending_resize: Arc<FairMutex<Option<SizeInfo>>>,
}

/// How many restarts `restart_stream` allows within [`RESTART_WINDOW`]
/// before giving up and letting the caller close the pane.
const MAX_STREAM_RESTARTS: usize = 3;

/// Sliding window for [`MAX_STREAM_RESTARTS`]; the count resets once a full
/// window passes with no death.
const RESTART_WINDOW: Duration = Duration::from_secs(60);

/// Sliding-window budget for stream restarts: at most [`MAX_STREAM_RESTARTS`]
/// within [`RESTART_WINDOW`], so a crash-looping `herdr terminal session
/// control` subprocess doesn't keep forking forever (the pane keeps its last
/// frame between a death and its respawn, so restarts are cheap but not
/// free).
struct RestartBudget {
    /// One timestamp per allowed restart inside the current window.
    recent_restarts: Vec<Instant>,
}

impl RestartBudget {
    fn new() -> Self {
        Self {
            recent_restarts: Vec::new(),
        }
    }

    /// Records a restart at `now` and returns whether it fit the budget.
    /// A refused restart is NOT recorded, so the budget recovers as soon as
    /// the window drains rather than being pinned by the refusal itself.
    fn record_restart(&mut self, now: Instant) -> bool {
        self.recent_restarts
            .retain(|&at| now.duration_since(at) < RESTART_WINDOW);
        if self.recent_restarts.len() >= MAX_STREAM_RESTARTS {
            return false;
        }
        self.recent_restarts.push(now);
        true
    }
}

pub struct TerminalManagerInit {
    pub(crate) manager: ModelHandle<Box<dyn TerminalManagerTrait>>,
    pub(crate) view: ViewHandle<TerminalView>,
}

impl TerminalManager {
    /// Creates a terminal manager model that mirrors a herdr pane through a
    /// `herdr terminal session control` subprocess.
    ///
    /// With `defer_stream` the subprocess is NOT spawned here: the pane
    /// starts unattached and spawns on first view via
    /// `ensure_stream_attached`, drained sequentially by the global spawn
    /// worker in `stream`. Spawn failures then surface at attach time
    /// (reported once; a later ensure retries) instead of failing this
    /// call, so deferred creation is infallible with respect to the spawn.
    /// A subprocess that dies after attaching is respawned through the same
    /// queue by `restart_stream` (bounded retries), so every pane keeps its
    /// attach params for the life of the manager.
    #[allow(clippy::too_many_arguments)]
    pub fn create_model(
        resources: TerminalViewResources,
        initial_size: Vector2F,
        model_event_sender: Option<SyncSender<ModelEvent>>,
        window_id: WindowId,
        mut spec: HerdrStreamSpec,
        on_closed: async_channel::Sender<Option<String>>,
        defer_stream: bool,
        ctx: &mut AppContext,
    ) -> anyhow::Result<TerminalManagerInit> {
        // Create all the necessary channels we need for communication.
        let (wakeups_tx, wakeups_rx) = async_channel::unbounded();
        let (events_tx, events_rx) = async_channel::unbounded();
        let (executor_command_tx, executor_command_rx) = async_channel::unbounded();

        // Use an empty pty reads broadcaster since we don't need to broadcast any PTY bytes for the
        // herdr-backed PTY. We use 1 instead of 0 here because `async_broadcast` internally
        // asserts that the capacity is at least 1.
        let (pty_reads_tx, _pty_reads_rx) = async_broadcast::broadcast(1);

        let channel_event_proxy = ChannelEventListener::new(wakeups_tx, events_tx, pty_reads_tx);

        // Initialize the sessions model.
        let sessions: ModelHandle<Sessions> =
            ctx.add_model(|ctx| Sessions::new(executor_command_tx, ctx));

        let model_events =
            ctx.add_model(|ctx| ModelEventDispatcher::new(events_rx, sessions.clone(), ctx));

        // Create the terminal model. herdr panes arrive with a fully
        // bootstrapped foreign shell; Warp must not run its own bootstrapping.
        let model = terminal_manager::create_terminal_model(
            None, /* startup_directory */
            None, /* restored_blocks */
            initial_size,
            channel_event_proxy.clone(),
            // TODO: Add support for other shells within the herdr-backed pty;
            // mirrored panes run whatever shell the foreign session uses.
            ShellLaunchState::ShellSpawned {
                available_shell: None,
                display_name: ShellName::blank(),
                shell_type: ShellType::Zsh,
            },
            BlockSpacing::for_gui(ctx),
            ctx,
        );

        let size_info = *model.block_list().size();
        let colors = model.colors();
        let model = Arc::new(FairMutex::new(model));

        let (event_loop_tx, event_loop_rx) = async_channel::unbounded();

        // Finalize the spawn spec at the pane's real size: frames arrive
        // pre-rendered at whatever --cols/--rows we pass, while the caller's
        // spec only carries a placeholder default. Deferred panes spawn with
        // these same values at attach time.
        spec.cols = size_info.columns as u16;
        spec.rows = size_info.rows as u16;
        let cwd = spec.cwd.clone();
        let agent = spec.agent.clone();
        let spec_agent_status = spec.agent_status;

        // Every spawn — eager, deferred first attach, or restart — delivers
        // frames through this bounded relay into the event loop, so a
        // respawned stream always has somewhere to send frames. Eager panes
        // fail fast: spawn the herdr subprocess before wiring the event
        // loop, pty controller, and view around it, then run the same
        // attach completion the global spawn worker runs for queued spawns.
        // Deferred panes start unattached instead: the relay stays idle
        // until `ensure_stream_attached`'s queued spawn connects a stream
        // to it, and the stream lands in this shared slot.
        let stream_slot: StreamSlot = Arc::new(FairMutex::new(None));
        let pending_resize = Arc::new(FairMutex::new(None));
        let (frames_relay_tx, frames_relay_rx) = async_channel::bounded(FRAMES_CHANNEL_CAPACITY);
        let attach_params = AttachParams {
            spec,
            frames_relay_tx,
            pending_resize: pending_resize.clone(),
        };
        if !defer_stream {
            let stream =
                HerdrStream::spawn(&attach_params.spec, attach_params.frames_relay_tx.clone())?;
            attach_spawned_stream(&stream_slot, stream, &attach_params.pending_resize);
        }
        let model_for_loop = model.clone();
        let listener_for_loop = channel_event_proxy.clone();
        let slot_for_loop = stream_slot.clone();
        let resize_for_loop = pending_resize.clone();
        let agent_for_loop = agent.clone();
        let stream_released = Arc::new(AtomicBool::new(false));
        let released_for_loop = stream_released.clone();
        let event_loop = ctx.add_model(|ctx| {
            EventLoop::start(
                model_for_loop,
                slot_for_loop,
                frames_relay_rx,
                resize_for_loop,
                released_for_loop,
                event_loop_rx,
                listener_for_loop,
                on_closed,
                cwd,
                agent_for_loop,
                ctx,
            )
        });

        // Initialize the PtyController.
        let pty_controller = init_pty_controller_model(
            event_loop_tx.clone(),
            executor_command_rx,
            model_events.clone(),
            sessions.clone(),
            model.clone(),
            ctx,
        );

        let cloned_model = model.clone();
        let prompt_type =
            ctx.add_model(|ctx| PromptType::new_dynamic_from_sessions(sessions.clone(), ctx));
        let view = ctx.add_typed_action_view(window_id, |ctx| {
            TerminalView::new(
                resources,
                wakeups_rx,
                model_events.clone(),
                cloned_model,
                sessions.clone(),
                size_info,
                colors,
                model_event_sender.clone(),
                prompt_type,
                None, // initial_input_config
                None, // conversation_restoration - not used for herdr
                None, // inactive_pty_reads_rx
                false,
                ctx,
            )
        });

        wire_up_pty_controller_with_surface(
            &pty_controller,
            &view,
            model.clone(),
            sessions,
            model_event_sender,
            ctx,
        );

        // Seed the CLI agent session from the agent herdr reports for this
        // pane so the CLI agent footer and Ctrl-G rich input work; mirrored
        // panes never emit the shell-integration hooks local detection uses.
        // The spec carries the snapshot's agent status, which is seen-aware
        // (finished-while-unseen reports Done), so the session's initial
        // status is trustworthy without any correction push.
        view.update(ctx, |view, ctx| {
            view.sync_herdr_cli_agent_session(agent.as_deref(), spec_agent_status, ctx);
        });

        // Clone the view before moving it into the manager so the caller can
        // receive it alongside the boxed manager.
        let terminal_view = view.clone();

        // Create the terminal manager itself.
        let terminal_manager = Self {
            model,
            event_loop_tx,
            stream_slot,
            attach_params,
            attach_in_flight: Arc::new(AtomicBool::new(false)),
            stream_released,
            restart_budget: FairMutex::new(RestartBudget::new()),
            _view: view,
            _pty_controller: pty_controller,
            _event_loop: event_loop,
        };

        let manager_model = ctx.add_model(|_ctx| {
            let manager: Box<dyn TerminalManagerTrait> = Box::new(terminal_manager);
            manager
        });
        manager_model.update(ctx, |_, ctx| {
            ctx.subscribe_to_view(&terminal_view, |manager, _, event, _| {
                if matches!(event, TerminalViewEvent::RetakeHerdrControl)
                    && let Some(manager) = manager.as_any().downcast_ref::<Self>()
                {
                    manager.retake_stream();
                }
            });
        });
        Ok(TerminalManagerInit {
            manager: manager_model,
            view: terminal_view,
        })
    }

    /// True while the pane has no attached herdr stream — a deferred pane
    /// still waiting for `ensure_stream_attached`'s queued spawn to
    /// complete, or a pane whose stream died and is awaiting a restart.
    pub fn has_pending_stream(&self) -> bool {
        self.stream_slot.lock().is_none()
    }

    /// Attaches the pane's herdr stream if it isn't already attached or
    /// queued: pushes the finalized spawn request onto the global,
    /// sequentially-drained spawn queue (see `stream::enqueue_stream_spawn`).
    /// Idempotent and non-blocking; a failed spawn leaves the slot empty and
    /// clears the in-flight flag so a later call retries.
    pub fn ensure_stream_attached(&self) {
        if self.stream_released.load(Ordering::SeqCst) {
            // The stream ended with a reasoned Closed (takeover, pane gone):
            // respawning on tab activation would steal the terminal back.
            return;
        }
        if !self.has_pending_stream() {
            return;
        }
        if self.attach_in_flight.swap(true, Ordering::SeqCst) {
            return;
        }
        self.queue_stream_spawn();
    }

    fn retake_stream(&self) {
        if self.model.lock().foreign_control_released() {
            // User-initiated reconnects do not consume the crash-loop retry budget.
            self.stream_released.store(false, Ordering::SeqCst);
            self.ensure_stream_attached();
        }
    }

    /// Restarts the pane's dead herdr stream: queues a respawn through the
    /// same global, sequentially-drained queue as a deferred first attach.
    /// The event loop has already emptied the slot (it does so before
    /// reporting a death), so the writer task drops input and buffers
    /// resizes until the respawn publishes — the pane and its last frame
    /// survive; there is no tab churn.
    ///
    /// Returns true when a spawn is coming — queued by this call, already in
    /// flight, or unnecessary because the stream in the slot is still alive
    /// (a restart only makes sense after a death, so a live stream is left
    /// alone rather than SIGKILLed). Returns false once the restart budget
    /// ([`MAX_STREAM_RESTARTS`] within [`RESTART_WINDOW`]) is exhausted, or
    /// when the spawn request could not be queued (worker gone, queue full);
    /// the caller then closes the pane.
    pub fn restart_stream(&self) -> bool {
        // An explicit restart re-arms auto-attach after a reasoned release.
        self.stream_released.store(false, Ordering::SeqCst);
        if self.stream_slot.lock().is_some() {
            log::debug!("herdr: restart_stream with a live stream attached; leaving it alone");
            return true;
        }
        if self.attach_in_flight.swap(true, Ordering::SeqCst) {
            // A spawn — first attach or an earlier restart — is already
            // queued and doubles as this restart.
            return true;
        }
        if !self.restart_budget.lock().record_restart(Instant::now()) {
            self.attach_in_flight.store(false, Ordering::SeqCst);
            log::warn!(
                "herdr: refusing to restart pane {} stream: over {} restarts within {}s",
                self.attach_params.spec.target,
                MAX_STREAM_RESTARTS,
                RESTART_WINDOW.as_secs(),
            );
            return false;
        }
        self.queue_stream_spawn()
    }

    /// Pushes the pane's spawn request onto the global spawn queue and
    /// returns whether it was actually queued. Callers must have checked the
    /// slot is empty and claimed the in-flight flag; on a queueing failure
    /// the flag is cleared here so a later ensure or restart can retry.
    fn queue_stream_spawn(&self) -> bool {
        let request = StreamSpawnRequest {
            spec: self.attach_params.spec.clone(),
            slot: Arc::downgrade(&self.stream_slot),
            frames_relay_tx: self.attach_params.frames_relay_tx.clone(),
            pending_resize: self.attach_params.pending_resize.clone(),
            attach_in_flight: self.attach_in_flight.clone(),
        };
        if let Err(err) = enqueue_stream_spawn(request) {
            self.attach_in_flight.store(false, Ordering::SeqCst);
            report_error!(
                err.context("Failed to queue herdr stream spawn"),
                warp_errors::ReportErrorLogMode::OncePerRun
            );
            return false;
        }
        true
    }
}

impl super::super::TerminalManager for TerminalManager {
    fn model(&self) -> Arc<FairMutex<TerminalModel>> {
        self.model.clone()
    }

    /// Permanently closing the pane gracefully releases the herdr stream (so
    /// herdr keeps the pane running) via the event loop's writer task, which
    /// maps `Message::Shutdown` to `stream.release()`; a deferred pane that
    /// never attached has an empty slot, so there is nothing to release.
    /// `HiddenForClose` (undo-close grace window) and `Moved` (this manager
    /// is re-attached in the target pane group) keep the stream alive.
    fn on_view_detached(&self, detach_type: DetachType, _app: &mut AppContext) {
        match detach_type {
            DetachType::Closed => {
                // If the writer task is already gone, HerdrStream::Drop
                // falls back to SIGKILL.
                let _ = self.event_loop_tx.try_send(Message::Shutdown);
            }
            DetachType::HiddenForClose | DetachType::Moved => {}
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

// The fake-herdr fixture is a #!/bin/sh script, so these only run on Unix.
#[cfg(all(test, unix))]
#[path = "terminal_manager_tests.rs"]
mod tests;
