use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_channel::Receiver;
use parking_lot::FairMutex;
use warpui::{Entity, ModelContext};

use crate::herdr::protocol::REASON_ATTACH_TAKEN_OVER;
use crate::terminal::event_listener::ChannelEventListener;
use crate::terminal::herdr_tty::stream::{StreamEvent, StreamSlot};
use crate::terminal::model::ansi::{
    BootstrappedValue, CommandFinishedValue, CompletionMetadata, Handler as _, InitShellValue,
    PrecmdValue, PreexecValue, Processor, PromptMetadata,
};
use crate::terminal::writeable_pty::Message as EventLoopMessage;
use crate::terminal::{SizeInfo, TerminalModel};

/// Parses one frame's bytes into the model and re-asserts the alt screen:
/// mirrored frames routinely carry 1049l (vim, less, … exiting the alt
/// screen), which would flip input back into Warp's block input editor and
/// silently swallow keystrokes meant for the mirrored program.
fn process_frame_bytes(model: &mut TerminalModel, parser: &mut Processor, bytes: &[u8]) {
    parser.parse_bytes(model, bytes, &mut io::sink());
    if !model.is_alt_screen_active() {
        model.enter_alt_screen(true /* save_cursor_and_clear_screen */);
    }
    model.set_foreign_control_released(false);
}

/// Puts a mirrored herdr pane's model into the state Warp requires before it
/// forwards raw keystrokes to the pty: a bootstrapped session sitting at a
/// prompt that then ran a never-finishing command. Warp gates typing on this
/// shell-integration lifecycle, and herdr's foreign shell will never emit the
/// integration sequences itself.
fn prepare_model_for_mirroring(model: &mut TerminalModel, cwd: Option<&str>, agent: Option<&str>) {
    // Must precede init_shell: the PtyController reacts to the InitShell event
    // by injecting Warp's bootstrap script unless the terminal is marked
    // foreign-controlled.
    model.set_foreign_controlled();
    model.set_foreign_agent(agent.map(str::to_owned));
    let session_id = crate::terminal::bootstrap::generate_session_id();
    model.register_session_id(session_id);
    // init_shell creates the pending session info that bootstrapped() consumes
    // to announce the session; without it the session never registers and
    // input gates stay shut. Local hostname keeps the session classified as
    // local (see SessionInfo::determine_session_type).
    let hostname = crate::terminal::model::session::get_local_hostname()
        .unwrap_or_else(|_| "localhost".to_string());
    model.init_shell(InitShellValue {
        session_id,
        // TODO: Add support for other shells within the herdr-backed pty;
        // the mirrored pane runs whatever shell the foreign session uses.
        shell: "zsh".to_owned(),
        hostname,
        ..Default::default()
    });
    model.bootstrapped(BootstrappedValue {
        session_id: Some(session_id.as_u64()),
        // TODO: Add support for other shells within the herdr-backed pty.
        shell: "zsh".to_string(),
        ..Default::default()
    });
    let completion_metadata = CompletionMetadata::default();
    model.command_finished(CommandFinishedValue {
        completion_metadata: completion_metadata.clone(),
        session_id: Some(session_id.as_u64()),
    });
    model.precmd_with_completion_metadata(PrecmdValue {
        completion_metadata,
        prompt_metadata: PromptMetadata {
            session_id: Some(session_id.as_u64()),
            // Surfaces as the pane's working directory (block metadata pwd);
            // without one, tab labels suppress the title as a duplicate.
            pwd: cwd.map(str::to_owned),
            ..Default::default()
        },
    });
    // Use herdr's reported agent as the running command so Warp's CLI-agent
    // detection (icons, badges) recognizes what the pane is running.
    model.preexec(PreexecValue {
        command: agent.unwrap_or("herdr").to_string(),
        session_id: Some(session_id.as_u64()),
    });
}

pub(super) struct EventLoop {
    terminal_model: Arc<FairMutex<TerminalModel>>,
    parser: Processor,
    event_loop_rx: Receiver<EventLoopMessage>,
    channel_event_listener: ChannelEventListener,
}

impl EventLoop {
    /// Starts the loop around the pane's late-bound stream slot. Every pane
    /// — eager or deferred — consumes the frames relay created in
    /// `TerminalManager::create_model`; each spawn (eager, deferred first
    /// attach, restart) lands in the slot and forwards its frames into the
    /// relay via the shared attach completion in `stream`. Infallible: spawn
    /// failures surface at `create_model` for eager panes and at attach time
    /// for deferred ones.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn start(
        model: Arc<FairMutex<TerminalModel>>,
        stream_slot: StreamSlot,
        frames_relay_rx: async_channel::Receiver<StreamEvent>,
        pending_resize: Arc<FairMutex<Option<SizeInfo>>>,
        stream_released: Arc<AtomicBool>,
        message_receiver: Receiver<EventLoopMessage>,
        channel_event_listener: ChannelEventListener,
        on_closed: async_channel::Sender<Option<String>>, // reason; None = subprocess died
        cwd: Option<String>,
        agent: Option<String>,
        ctx: &mut ModelContext<Self>,
    ) -> Self {
        let mut event_loop = Self {
            terminal_model: model,
            parser: Processor::new(),
            event_loop_rx: message_receiver,
            channel_event_listener,
        };
        // Simulate the shell-integration lifecycle Warp's input gates require,
        // then enter the alt screen so frames render on the grid and
        // keystrokes forward raw to the stream instead of being captured in
        // Warp's block input editor. Runs at loop start for both eager and
        // deferred panes: the model must accept raw input state from birth.
        prepare_model_for_mirroring(
            &mut event_loop.terminal_model.lock(),
            cwd.as_deref(),
            agent.as_deref(),
        );
        event_loop.process_pty_bytes(b"\x1b[?1049h");
        event_loop.start_tasks(
            stream_slot,
            frames_relay_rx,
            pending_resize,
            stream_released,
            on_closed,
            ctx,
        );
        event_loop
    }

    #[allow(clippy::too_many_arguments)]
    fn start_tasks(
        &mut self,
        stream_slot: StreamSlot,
        frames_relay_rx: async_channel::Receiver<StreamEvent>,
        pending_resize: Arc<FairMutex<Option<SizeInfo>>>,
        stream_released: Arc<AtomicBool>,
        on_closed: async_channel::Sender<Option<String>>,
        ctx: &mut ModelContext<Self>,
    ) {
        // INBOUND: herdr frames → TerminalModel (mirrors remote_tty's stream task).
        // Consumes the pane's frames relay; each attach completion connects a
        // freshly spawned stream to the relay's other end.
        let slot_for_inbound = stream_slot.clone();
        ctx.spawn_stream_local(
            frames_relay_rx,
            move |event_loop, event, _| match event {
                // Base64 decode already happened on the stream's reader
                // thread; only the grid apply runs here.
                StreamEvent::Frame(bytes) => event_loop.process_pty_bytes(&bytes),
                StreamEvent::Closed { reason } => {
                    // The subprocess ended after writing a Closed record:
                    // empty the slot so the exited child is reaped (the
                    // dropped Arc kills+waits) and a later restart isn't
                    // refused as already-attached. A reasoned close
                    // (takeover, pane gone herdr-side) also marks the slot
                    // released so tab activation doesn't silently respawn —
                    // retaking a taken-over terminal must stay a deliberate
                    // act.
                    if reason.is_some() {
                        stream_released.store(true, Ordering::SeqCst);
                    }
                    let dead = slot_for_inbound.lock().take();
                    drop(dead);
                    if reason.as_deref() == Some(REASON_ATTACH_TAKEN_OVER) {
                        event_loop
                            .terminal_model
                            .lock()
                            .set_foreign_control_released(true);
                        event_loop.channel_event_listener.send_wakeup_event();
                    }
                    let _ = on_closed.try_send(reason);
                }
                StreamEvent::Died => {
                    // The subprocess died without a Closed record. Empty the
                    // slot BEFORE reporting the death so the restart this
                    // notification triggers sees the slot streamless; the
                    // dropped Arc reaps the dead child (HerdrStream::Drop).
                    // The writer task then behaves as pre-attach — input
                    // dropped with a debug log, resize buffered — and the
                    // pane's last frame stays on the grid until the first
                    // frame of the restarted stream.
                    let dead = slot_for_inbound.lock().take();
                    drop(dead);
                    let _ = on_closed.try_send(None);
                }
            },
            |_, _| {},
        );

        // OUTBOUND: Warp input/resize → stream (mirrors remote_tty's writer task).
        // The stream is late-bound: the slot is empty until a queued spawn
        // completes (deferred first attach, restart after a death).
        // Every arm binds the slot clone in a `let` before touching the
        // stream: `match slot.lock().clone()` keeps the guard alive for the
        // whole arm, so a stalled herdr subprocess blocking a pipe write
        // would wedge the main thread, which locks the same slot on tab
        // activation.
        let receiver = self.event_loop_rx.clone();
        ctx.background_executor()
            .spawn(async move {
                while let Ok(message) = receiver.recv().await {
                    match message {
                        EventLoopMessage::Input(bytes) => {
                            let stream = stream_slot.lock().clone();
                            match stream {
                                Some(stream) => {
                                    log::debug!(
                                        "herdr: forwarding {} input bytes to stream",
                                        bytes.len()
                                    );
                                    stream.send_input(&bytes);
                                }
                                // No attached stream (not yet attached, or
                                // died and awaiting a restart): the mirrored
                                // shell can't receive input, so drop it.
                                None => log::debug!(
                                    "herdr: dropping {} input bytes; pane has no attached stream",
                                    bytes.len()
                                ),
                            }
                        }
                        EventLoopMessage::Resize(size_info) => {
                            // Always record the latest size, live stream or
                            // not: a restart respawns from creation-time
                            // spec dimensions, and the attach completion
                            // flushes this buffer through the new stream so
                            // a respawned pane doesn't render at stale
                            // geometry until the next user resize.
                            *pending_resize.lock() = Some(size_info);
                            let stream = stream_slot.lock().clone();
                            if let Some(stream) = stream {
                                stream.resize(
                                    size_info.columns as u16,
                                    size_info.rows as u16,
                                    size_info.cell_width_px().as_f32() as u16,
                                    size_info.cell_height_px().as_f32() as u16,
                                );
                            }
                        }
                        EventLoopMessage::Mouse(state) => {
                            let stream = stream_slot.lock().clone();
                            if let Some(stream) = stream {
                                stream.mouse(&state);
                            }
                        }
                        EventLoopMessage::Scroll { up, lines, source } => {
                            let stream = stream_slot.lock().clone();
                            match stream {
                                Some(stream) => stream.scroll(up, lines, source),
                                // No attached stream (not yet attached, or
                                // died and awaiting a restart): herdr-side
                                // scrollback is unreachable, so drop it.
                                None => log::debug!(
                                    "herdr: dropping scroll; pane has no attached stream"
                                ),
                            }
                        }
                        EventLoopMessage::Shutdown | EventLoopMessage::ShutdownForReplacement => {
                            let stream = stream_slot.lock().clone();
                            if let Some(stream) = stream {
                                stream.release();
                            }
                            break;
                        }
                        EventLoopMessage::ChildExited => {}
                    }
                }
            })
            .detach();
    }

    /// Processes a byte slice through the `Processor`.
    fn process_pty_bytes(&mut self, bytes: &[u8]) {
        let mut terminal_model = self.terminal_model.lock();
        process_frame_bytes(&mut terminal_model, &mut self.parser, bytes);
        self.channel_event_listener.send_wakeup_event();
    }
}

impl Entity for EventLoop {
    type Event = ();
}

#[cfg(test)]
#[path = "event_loop_tests.rs"]
mod tests;
