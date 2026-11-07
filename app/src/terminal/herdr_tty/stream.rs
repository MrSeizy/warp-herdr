//! One `herdr terminal session control` subprocess per herdr-backed pane.
//! stdout frames → StreamEvent channel; input/resize/release → stdin JSON.
//!
//! Panes created with `defer_stream` skip the spawn at creation; their first
//! `ensure_stream_attached` queues a [`StreamSpawnRequest`] onto a global,
//! sequentially-drained queue (see [`enqueue_stream_spawn`]) so attaching a
//! many-pane session never forks all its subprocesses at once. A subprocess
//! that later dies (crash, exit 1) is restarted through the same queue —
//! every stream, however spawned, feeds the pane's frames relay, so a
//! respawn always has somewhere to send frames.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use anyhow::Context;
use command::blocking::Command;
use parking_lot::FairMutex;
use warp_core::safe_warn;
use warp_errors::report_error;

use crate::herdr::protocol::{
    CappedLine, ControlCommand, MAX_PEER_LINE_BYTES, StreamRecord, read_capped_line,
};
use crate::terminal::SizeInfo;
use crate::terminal::model::mouse::MouseState;
use crate::terminal::writeable_pty::ScrollSource;

/// Bounds the frames relay so a fast-producing herdr subprocess (e.g. a
/// pane running `yes`) can't pile up frames in memory while the app
/// thread is stalled; the reader thread blocks until the app catches up.
pub(crate) const FRAMES_CHANNEL_CAPACITY: usize = 64;

/// Bounds the pending-spawn queue. Attaching a session queues one request
/// per deferred pane, so this comfortably exceeds any real pane count; a
/// full queue just fails the ensure, which retries on a later view.
const SPAWN_QUEUE_CAPACITY: usize = 256;

#[derive(Debug, Clone)]
pub struct HerdrStreamSpec {
    /// Path to the herdr binary (default: "herdr" on PATH).
    pub herdr_bin: PathBuf,
    /// Named session, if not the default one.
    pub session: Option<String>,
    /// Pane target, e.g. "w1:p1".
    pub target: String,
    /// The pane's working directory as reported by herdr, if known.
    pub cwd: Option<String>,
    /// The agent herdr reports running in this pane (e.g. "claude").
    pub agent: Option<String>,
    /// The pane's agent status as herdr reported it when this spec was
    /// built; seeds the mirrored CLI agent session's initial status.
    pub agent_status: crate::herdr::protocol::AgentStatus,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug)]
pub enum StreamEvent {
    /// One frame's bytes, base64-decoded on the stream's reader thread so
    /// the UI thread only pays for the grid apply.
    Frame(Vec<u8>),
    Closed {
        reason: Option<String>,
    },
    /// Subprocess died without a terminal.closed record (crash, exit 1).
    /// The event loop empties the pane's stream slot before reporting the
    /// death so `TerminalManager::restart_stream` can queue a respawn.
    Died,
}

pub struct HerdrStream {
    child: Mutex<Child>,
    stdin: Mutex<Option<std::process::ChildStdin>>,
    /// Set by `release`/`kill` before they empty the stdin slot: a
    /// `write_command` holding the handle OUT of the mutex mid-write (see
    /// there for why) checks this before restoring it, so a close can't be
    /// undone by a concurrent write putting the handle back.
    stdin_closed: AtomicBool,
}

impl HerdrStream {
    /// Spawns the control subprocess and starts its reader threads. The
    /// reader parses, decodes, and sends events straight into `events_tx` —
    /// the pane's frames relay — blocking on the bounded channel when the
    /// app is stalled (the backpressure point).
    pub fn spawn(
        spec: &HerdrStreamSpec,
        events_tx: async_channel::Sender<StreamEvent>,
    ) -> anyhow::Result<Self> {
        let mut cmd = Command::new(&spec.herdr_bin);
        if let Some(session) = &spec.session {
            cmd.arg("--session").arg(session);
        }
        cmd.args(["terminal", "session", "control"])
            .arg(&spec.target)
            .arg("--takeover")
            .args(["--cols", &spec.cols.to_string()])
            .args(["--rows", &spec.rows.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn {:?}", spec.herdr_bin))?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stdin = child.stdin.take().expect("piped stdin");
        let stderr = child.stderr.take().expect("piped stderr");
        let tx = events_tx;
        let stream = Self {
            child: Mutex::new(child),
            stdin: Mutex::new(Some(stdin)),
            stdin_closed: AtomicBool::new(false),
        };
        // Keep the subprocess's diagnostics visible: when attach fails
        // herdr-side, stderr is otherwise the only signal we get.
        if let Err(err) = std::thread::Builder::new()
            .name(format!("herdr-stream-stderr-{}", spec.target))
            .spawn(move || {
                for line in BufReader::new(stderr).lines() {
                    let Ok(line) = line else { break };
                    safe_warn!(
                        safe: ("herdr stream: control subprocess wrote to stderr"),
                        full: ("herdr stream stderr: {line}")
                    );
                }
            })
        {
            // Reap the child so the error path doesn't leave a zombie.
            stream.kill();
            return Err(err).context("spawn herdr-stream stderr thread");
        }
        if let Err(err) = std::thread::Builder::new()
            .name(format!("herdr-stream-{}", spec.target))
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut saw_close = false;
                let mut line = String::new();
                loop {
                    match read_capped_line(&mut reader, &mut line) {
                        Ok(CappedLine::Line) => {}
                        Ok(CappedLine::Eof) => break,
                        Ok(CappedLine::Overlong) => {
                            log::warn!(
                                "herdr stream: record exceeded {MAX_PEER_LINE_BYTES} bytes; closing"
                            );
                            break;
                        }
                        Err(_) => break,
                    }
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<StreamRecord>(&line) {
                        // block_on(send) on a bounded channel parks this
                        // reader thread when full — the backpressure point
                        // (send_blocking is repo-disallowed: no wasm).
                        Ok(StreamRecord::Frame(f)) => match f.decoded() {
                            Ok(bytes) => {
                                if warpui::r#async::block_on(tx.send(StreamEvent::Frame(bytes)))
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            Err(err) => report_error!(
                                err.context("Failed to decode herdr terminal frame"),
                                warp_errors::ReportErrorLogMode::OncePerRun
                            ),
                        },
                        Ok(StreamRecord::Closed { reason }) => {
                            saw_close = true;
                            let _ =
                                warpui::r#async::block_on(tx.send(StreamEvent::Closed { reason }));
                        }
                        Err(err) => log::warn!("herdr stream: bad record: {err}"),
                    }
                }
                if !saw_close {
                    let _ = warpui::r#async::block_on(tx.send(StreamEvent::Died));
                }
            })
        {
            // Reap the child so the error path doesn't leave a zombie.
            stream.kill();
            return Err(err).context("spawn herdr-stream thread");
        }
        Ok(stream)
    }

    fn write_command(&self, cmd: &ControlCommand) {
        // Take stdin OUT of the mutex for the write: a stalled subprocess
        // (full pipe, nothing reading) blocks write_all indefinitely, and
        // holding the lock across it would wedge `kill` — the very call
        // whose SIGKILL breaks the pipe and unblocks the write — behind it.
        let Some(mut stdin) = self.stdin.lock().unwrap().take() else {
            return;
        };
        let line = serde_json::to_string(cmd).expect("serialize control command");
        if stdin
            .write_all(line.as_bytes())
            .and_then(|_| stdin.write_all(b"\n"))
            .is_err()
        {
            // stdin gone; reader thread reports Died/Closed. Dropping the
            // taken handle leaves the slot empty, as before.
            return;
        }
        let _ = stdin.flush();
        let mut guard = self.stdin.lock().unwrap();
        if self.stdin_closed.load(Ordering::SeqCst) {
            // release()/kill() closed the stream mid-write: restoring the
            // handle would hold the pipe open and swallow the detach EOF.
            return;
        }
        *guard = Some(stdin);
    }

    pub fn send_input(&self, bytes: &[u8]) {
        self.write_command(&ControlCommand::input_bytes(bytes));
    }

    pub fn resize(&self, cols: u16, rows: u16, cell_width_px: u16, cell_height_px: u16) {
        self.write_command(&ControlCommand::Resize {
            cols,
            rows,
            cell_width_px,
            cell_height_px,
        });
    }

    /// Scrolls the pane herdr-side. herdr owns the scrollback — including
    /// pre-attach history the mirrored grid never had — and routes the
    /// scroll by `source`: wheel scrolls reach a program that requested
    /// mouse reporting (SGR wheel) or alt-screen alternate scroll (arrow
    /// keys), page keys reach a program that claimed them, and anything
    /// else scrolls herdr's own scrollback. Lines clamp to the u16 the
    /// protocol parses; herdr rejects 0, so it is skipped here.
    pub fn scroll(&self, up: bool, lines: u32, source: ScrollSource) {
        let lines = lines.min(u32::from(u16::MAX)) as u16;
        if lines == 0 {
            return;
        }
        self.write_command(&ControlCommand::Scroll {
            direction: if up { "up" } else { "down" },
            lines,
            source: match source {
                ScrollSource::Wheel => "wheel",
                ScrollSource::PageKey => "page_key",
            },
            modifiers: 0,
            column: None,
            row: None,
        });
    }

    pub fn mouse(&self, state: &MouseState) {
        if let Some(command) = ControlCommand::mouse(state) {
            self.write_command(&command);
        }
    }

    /// Graceful detach: herdr keeps the pane running.
    pub fn release(&self) {
        self.write_command(&ControlCommand::Release);
        // Closing our stdin also signals detach on EOF.
        self.stdin_closed.store(true, Ordering::SeqCst);
        *self.stdin.lock().unwrap() = None;
    }

    /// Hard stop of the subprocess: SIGKILL, then reap. `kill` without
    /// `wait` would leave a zombie per pane attach/detach cycle.
    pub fn kill(&self) {
        self.stdin_closed.store(true, Ordering::SeqCst);
        *self.stdin.lock().unwrap() = None;
        let mut child = self.child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for HerdrStream {
    fn drop(&mut self) {
        // SIGKILLs unless the child already exited (e.g. after release()),
        // and reaps it either way.
        self.kill();
    }
}

/// The late-bound herdr stream shared between a pane's TerminalManager, its
/// event loop's writer task, and the spawn worker: filled by the attach
/// completion (the eager spawn at creation, or the worker for deferred
/// panes and restarts), emptied by the event loop when the subprocess dies
/// so a restart can be queued.
pub(crate) type StreamSlot = Arc<FairMutex<Option<Arc<HerdrStream>>>>;

/// Everything the global spawn worker needs to attach a pane's stream,
/// captured by `TerminalManager::ensure_stream_attached` (deferred first
/// attach) and `TerminalManager::restart_stream` (respawn after a death).
pub(crate) struct StreamSpawnRequest {
    /// The finalized spec: cols/rows were overridden to the pane's real size
    /// at creation, so the attach-time spawn matches an eager one.
    pub spec: HerdrStreamSpec,
    /// Destination for the spawned stream; a dead slot means the pane closed
    /// before its turn and the request is skipped.
    pub slot: Weak<FairMutex<Option<Arc<HerdrStream>>>>,
    /// Connects the new stream's frames to the event loop's INBOUND relay,
    /// which every pane — eager or deferred — has consumed since creation.
    pub frames_relay_tx: async_channel::Sender<StreamEvent>,
    /// Latest resize buffered while the slot was empty; flushed through the
    /// new stream just before it goes live in the slot.
    pub pending_resize: Arc<FairMutex<Option<SizeInfo>>>,
    /// In-flight flag shared with the manager; cleared once the request
    /// settles (published, failed, or skipped) so a later ensure or restart
    /// can queue the next spawn.
    pub attach_in_flight: Arc<AtomicBool>,
}

/// Global queue bounding herdr control-subprocess spawns: a single lazily
/// started worker drains it SEQUENTIALLY (concurrency 1), so attaching a
/// many-pane session never forks all its subprocesses at once. Raising the
/// bound means adding a semaphore around the spawn, not more workers.
static SPAWN_QUEUE: OnceLock<Option<async_channel::Sender<StreamSpawnRequest>>> = OnceLock::new();

/// The global spawn queue's sender, starting the worker thread on first use.
/// `None` when the worker thread could not be spawned.
fn spawn_queue() -> &'static Option<async_channel::Sender<StreamSpawnRequest>> {
    SPAWN_QUEUE.get_or_init(|| {
        let (tx, rx) = async_channel::bounded(SPAWN_QUEUE_CAPACITY);
        match std::thread::Builder::new()
            .name("herdr-stream-spawn".to_owned())
            .spawn(move || run_spawn_worker(rx))
        {
            Ok(_) => Some(tx),
            Err(err) => {
                report_error!(
                    anyhow::Error::new(err).context("Failed to spawn herdr stream worker thread")
                );
                None
            }
        }
    })
}

/// Queues a stream spawn for the global worker — a deferred pane's first
/// attach or a restart after the subprocess died. Fails only when the
/// worker thread is unavailable or the queue is full; the caller clears the
/// request's in-flight flag so a later ensure/restart can retry.
pub(crate) fn enqueue_stream_spawn(request: StreamSpawnRequest) -> anyhow::Result<()> {
    let queue = spawn_queue()
        .as_ref()
        .context("herdr stream spawn worker unavailable")?;
    if queue.try_send(request).is_err() {
        anyhow::bail!("herdr stream spawn queue is full or its worker is gone");
    }
    Ok(())
}

/// Test-only: closes the global spawn queue, simulating a dead worker, so
/// enqueue-failure paths can be exercised. Irreversible for the process —
/// only safe under process-per-test isolation (nextest, the repo's runner).
#[cfg(test)]
pub(crate) fn close_spawn_queue_for_tests() {
    if let Some(queue) = spawn_queue() {
        queue.close();
    }
}

/// Drains spawn requests one at a time; runs for the life of the process
/// (the queue's sender lives in a `static`).
fn run_spawn_worker(rx: async_channel::Receiver<StreamSpawnRequest>) {
    while let Ok(request) = rx.recv_blocking() {
        let Some(slot) = request.slot.upgrade() else {
            log::debug!("herdr: skipping queued spawn; pane closed before its turn");
            request.attach_in_flight.store(false, Ordering::SeqCst);
            continue;
        };
        let failed = match HerdrStream::spawn(&request.spec, request.frames_relay_tx.clone()) {
            Ok(stream) => {
                attach_spawned_stream(&slot, stream, &request.pending_resize);
                false
            }
            Err(err) => {
                report_error!(
                    err.context("Failed to spawn herdr stream"),
                    warp_errors::ReportErrorLogMode::OncePerRun
                );
                true
            }
        };
        // Whatever the outcome, the request has settled: clear the in-flight
        // flag so a failure (which leaves the slot empty) can be retried by
        // a later ensure_stream_attached/restart_stream, and a success does
        // not block a restart after a later death.
        request.attach_in_flight.store(false, Ordering::SeqCst);
        if failed {
            // A failed spawn must not strand an already-active pane on its
            // last frame with no signal (nothing re-runs the attach until
            // the next tab activation): report it as a death so the
            // budget-bounded restart path retries, then closes the pane once
            // the budget is spent. Sent after the in-flight flag clears so
            // the restart isn't refused as already-queued.
            let _ = request.frames_relay_tx.try_send(StreamEvent::Died);
        }
    }
}

/// Puts a freshly spawned stream live: flushes any resize buffered while the
/// slot was empty, then publishes the stream into the slot. Shared by the
/// spawn worker (deferred attach, restart) and the eager spawn at
/// `create_model`. The stream's reader thread already sends frames straight
/// into the pane's relay (see `HerdrStream::spawn`), so every stream —
/// initial, deferred, or restarted — delivers frames the event loop
/// consumes.
pub(crate) fn attach_spawned_stream(
    slot: &StreamSlot,
    stream: HerdrStream,
    pending_resize: &FairMutex<Option<SizeInfo>>,
) {
    let stream = Arc::new(stream);
    // Flush the resize buffered while the slot was empty BEFORE publishing,
    // so the writer task can't reorder a newer resize ahead of it. A resize
    // racing the flush stays buffered until the next resize — self-healing.
    if let Some(size_info) = pending_resize.lock().take() {
        stream.resize(
            size_info.columns as u16,
            size_info.rows as u16,
            size_info.cell_width_px().as_f32() as u16,
            size_info.cell_height_px().as_f32() as u16,
        );
    }
    *slot.lock() = Some(stream);
}

// The fake-herdr fixture is a #!/bin/sh script, so these only run on Unix.
#[cfg(all(test, unix))]
#[path = "stream_tests.rs"]
mod tests;
