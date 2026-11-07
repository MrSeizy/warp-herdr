//! Deferred-spawn and stream-restart tests for the herdr terminal manager:
//! a `defer_stream` pane starts unattached, `ensure_stream_attached` drives
//! the global, sequential spawn worker, and `restart_stream` respawns a dead
//! subprocess through the same queue under a sliding-window budget.
//! Unix-only (gated at the `#[path]` include): the fixture is a `#!/bin/sh`
//! script made executable via `PermissionsExt`.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::time::Duration;

use instant::Instant;
use warpui::platform::WindowStyle;
use warpui::{App, Element, SingletonEntity, TypedActionView};

use super::*;
use crate::herdr::fake_herdr::{test_stream_spec, write_executable_herdr_script};
use crate::server::server_api::ServerApiProvider;
use crate::terminal::event::Event as TerminalEvent;
use crate::terminal::view::TerminalAction;
use crate::test_util::terminal::initialize_app_for_terminal_view;

/// Writes a fake `herdr` that records each spawn, emits one frame carrying
/// an OSC title (proof that frames reached the model), and echoes its stdin
/// to a file for control-command assertions.
fn fake_herdr(dir: &Path) -> PathBuf {
    let script = r#"#!/bin/sh
echo spawn >> "$0.count"
echo '{"type":"terminal.frame","seq":1,"encoding":"ansi","width":80,"height":24,"full":true,"bytes":"G10yO2hlcmRyLWF0dGFjaGVkBw=="}'
cat > "$0.stdin"
echo '{"type":"terminal.closed","reason":"detached"}'
"#;
    write_executable_herdr_script(dir, script)
}

fn spec_for(herdr_bin: PathBuf) -> HerdrStreamSpec {
    test_stream_spec(herdr_bin, "w1:p1")
}

/// One line per `herdr terminal session control` subprocess spawned.
fn spawn_count(bin: &Path) -> usize {
    std::fs::read_to_string(bin.with_file_name("herdr.count"))
        .map(|contents| contents.lines().count())
        .unwrap_or(0)
}

/// Everything written to the subprocess's stdin so far (empty before the
/// fake's `cat` creates the file — i.e. before any spawn).
fn stdin_contents(bin: &Path) -> String {
    std::fs::read_to_string(bin.with_file_name("herdr.stdin")).unwrap_or_default()
}

struct HerdrRootView {
    terminal_view: ViewHandle<TerminalView>,
    manager: ModelHandle<Box<dyn TerminalManagerTrait>>,
}

impl warpui::Entity for HerdrRootView {
    type Event = ();
}

impl warpui::View for HerdrRootView {
    fn ui_name() -> &'static str {
        "HerdrRootView"
    }

    fn render(&self, _app: &AppContext) -> Box<dyn Element> {
        warpui::elements::ChildView::new(&self.terminal_view).finish()
    }
}

impl warpui::TypedActionView for HerdrRootView {
    type Action = ();
}

/// Builds a herdr manager in a real window and returns the boxed manager
/// registered as a model.
fn create_manager(
    app: &mut App,
    spec: HerdrStreamSpec,
    defer_stream: bool,
) -> ModelHandle<Box<dyn TerminalManagerTrait>> {
    initialize_app_for_terminal_view(app);
    let server_api = app.read(|ctx| ServerApiProvider::as_ref(ctx).get());
    let tips_model = app.add_model(|_| Default::default());
    let (_window_id, root_view) = app.add_window(WindowStyle::NotStealFocus, |ctx| {
        let resources = TerminalViewResources {
            tips_completed: tips_model,
            server_api,
            model_event_sender: None,
        };
        let (on_closed_tx, _on_closed_rx) = async_channel::unbounded();
        let init = TerminalManager::create_model(
            resources,
            Vector2F::new(7., 10.5),
            None,
            ctx.window_id(),
            spec,
            on_closed_tx,
            defer_stream,
            ctx,
        )
        .expect("create_model");
        HerdrRootView {
            terminal_view: init.view,
            manager: init.manager,
        }
    });
    root_view.read(app, |view, _| view.manager.clone())
}

/// Reads through the boxed manager to the concrete herdr manager.
fn read_herdr_manager<R>(
    app: &App,
    manager: &ModelHandle<Box<dyn TerminalManagerTrait>>,
    f: impl FnOnce(&TerminalManager) -> R,
) -> R {
    app.read(|ctx| {
        f(manager
            .as_ref(ctx)
            .as_any()
            .downcast_ref::<TerminalManager>()
            .expect("herdr-backed manager"))
    })
}

/// Polls `cond` while the app's executors make progress; fails after ~10s
/// so a broken attach doesn't hang the test.
async fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
    crate::test_util::assert_eventually!(2000 => cond(), "timed out waiting for {what}");
}

#[test]
fn takeover_makes_mirrored_pane_read_only() {
    App::test((), |mut app| async move {
        let dir = tempfile::tempdir().unwrap();
        let bin = write_executable_herdr_script(
            dir.path(),
            r#"#!/bin/sh
echo '{"type":"terminal.frame","seq":1,"encoding":"ansi","width":80,"height":24,"full":true,"bytes":"G10yO2hlcmRyLWF0dGFjaGVkBw=="}'
echo '{"type":"terminal.closed","reason":"terminal attach taken over"}'
cat > /dev/null
"#,
        );
        let manager = create_manager(&mut app, spec_for(bin), false);
        let model = app.read(|ctx| manager.as_ref(ctx).model());

        wait_until(
            || {
                model.lock().terminal_title().as_deref() == Some("herdr-attached")
                    && read_herdr_manager(&app, &manager, |m| m.has_pending_stream())
            },
            "takeover to release the stream",
        )
        .await;

        assert!(model.lock().is_read_only());
    });
}

#[test]
fn takeover_reconnects_only_after_explicit_action() {
    App::test((), |mut app| async move {
        let dir = tempfile::tempdir().unwrap();
        let bin = write_executable_herdr_script(
            dir.path(),
            r#"#!/bin/sh
echo spawn >> "$0.count"
echo '{"type":"terminal.frame","seq":1,"encoding":"ansi","width":80,"height":24,"full":true,"bytes":"G10yO2hlcmRyLWF0dGFjaGVkBw=="}'
if [ ! -e "$0.taken" ]; then
    touch "$0.taken"
    echo '{"type":"terminal.closed","reason":"terminal attach taken over"}'
fi
cat > "$0.stdin"
"#,
        );
        let manager = create_manager(&mut app, spec_for(bin.clone()), false);
        let model = app.read(|ctx| manager.as_ref(ctx).model());
        wait_until(
            || model.lock().foreign_control_released(),
            "takeover notice",
        )
        .await;

        read_herdr_manager(&app, &manager, |m| m.ensure_stream_attached());
        assert!(read_herdr_manager(&app, &manager, |m| m.has_pending_stream()));
        assert_eq!(spawn_count(&bin), 1);
        assert!(model.lock().is_read_only());

        let view = read_herdr_manager(&app, &manager, |m| m._view.clone());
        view.update(&mut app, |view, ctx| {
            view.handle_action(&TerminalAction::RetakeHerdrControl, ctx);
        });
        wait_until(
            || !model.lock().foreign_control_released(),
            "first frame after reconnect",
        )
        .await;

        assert_eq!(spawn_count(&bin), 2);
        assert!(!model.lock().is_read_only());
        assert!(!read_herdr_manager(&app, &manager, |m| m.has_pending_stream()));
    });
}

#[test]
fn deferred_manager_starts_unattached() {
    App::test((), |mut app| async move {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_herdr(dir.path());
        let manager = create_manager(
            &mut app,
            spec_for(bin.clone()),
            true, /* defer_stream */
        );

        // No subprocess at creation and no frames pending anywhere: the
        // model shows no title, and the manager reports a pending stream.
        assert!(read_herdr_manager(&app, &manager, |m| m.has_pending_stream()));
        assert_eq!(spawn_count(&bin), 0);
        let model = app.read(|ctx| manager.as_ref(ctx).model());
        assert_eq!(model.lock().terminal_title().as_deref(), None);

        // ...but the model was still put into mirrored-input state at birth,
        // ready to accept raw input and frames.
        assert!(matches!(
            model.lock().terminal_input_state(),
            crate::terminal::model::terminal_model::TerminalInputState::AltScreen
        ));
        assert!(model.lock().is_foreign_controlled());
    });
}

#[test]
fn eager_manager_spawns_at_creation() {
    App::test((), |mut app| async move {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_herdr(dir.path());
        let manager = create_manager(
            &mut app,
            spec_for(bin.clone()),
            false, /* defer_stream */
        );

        assert!(!read_herdr_manager(&app, &manager, |m| m.has_pending_stream()));
        // The spawn marker is written by the child after fork, so poll for it.
        wait_until(|| spawn_count(&bin) == 1, "subprocess to spawn").await;

        // ensure is a no-op for an already-attached pane.
        read_herdr_manager(&app, &manager, |m| m.ensure_stream_attached());
        warpui::r#async::Timer::after(Duration::from_millis(300)).await;
        assert_eq!(spawn_count(&bin), 1);

        // Frames flow without any ensure.
        let model = app.read(|ctx| manager.as_ref(ctx).model());
        wait_until(
            || model.lock().terminal_title().as_deref() == Some("herdr-attached"),
            "frames to reach the model",
        )
        .await;
    });
}

#[test]
fn ensure_stream_attached_spawns_and_frames_flow() {
    App::test((), |mut app| async move {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_herdr(dir.path());
        let manager = create_manager(
            &mut app,
            spec_for(bin.clone()),
            true, /* defer_stream */
        );

        read_herdr_manager(&app, &manager, |m| m.ensure_stream_attached());
        wait_until(
            || read_herdr_manager(&app, &manager, |m| !m.has_pending_stream()),
            "stream to attach",
        )
        .await;
        // The spawn marker is written by the child after fork, so poll for it.
        wait_until(|| spawn_count(&bin) == 1, "subprocess to spawn").await;

        // The fake herdr's title frame flowed through the relay into the model.
        let model = app.read(|ctx| manager.as_ref(ctx).model());
        wait_until(
            || model.lock().terminal_title().as_deref() == Some("herdr-attached"),
            "frames to reach the model",
        )
        .await;
    });
}

#[test]
fn ensure_stream_attached_is_idempotent() {
    App::test((), |mut app| async move {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_herdr(dir.path());
        let manager = create_manager(
            &mut app,
            spec_for(bin.clone()),
            true, /* defer_stream */
        );

        // Back-to-back ensures queue a single spawn; a post-attach ensure is
        // a no-op as well.
        read_herdr_manager(&app, &manager, |m| {
            m.ensure_stream_attached();
            m.ensure_stream_attached();
        });
        wait_until(
            || read_herdr_manager(&app, &manager, |m| !m.has_pending_stream()),
            "stream to attach",
        )
        .await;
        // The spawn marker is written by the child after fork, so poll for it.
        wait_until(|| spawn_count(&bin) == 1, "subprocess to spawn").await;
        read_herdr_manager(&app, &manager, |m| m.ensure_stream_attached());
        // Give any erroneous second spawn a chance to appear.
        warpui::r#async::Timer::after(Duration::from_millis(300)).await;
        assert_eq!(spawn_count(&bin), 1);
    });
}

#[test]
fn input_before_attach_is_dropped() {
    App::test((), |mut app| async move {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_herdr(dir.path());
        let manager = create_manager(
            &mut app,
            spec_for(bin.clone()),
            true, /* defer_stream */
        );

        read_herdr_manager(&app, &manager, |m| {
            m.event_loop_tx
                .try_send(Message::Input(Cow::Borrowed(b"echo hi")))
                .unwrap();
        });
        // Let the writer task drain (and drop) the message before attaching
        // so the test doesn't race the slot.
        warpui::r#async::Timer::after(Duration::from_millis(200)).await;
        assert!(read_herdr_manager(&app, &manager, |m| m.has_pending_stream()));
        assert_eq!(spawn_count(&bin), 0);

        read_herdr_manager(&app, &manager, |m| m.ensure_stream_attached());
        wait_until(
            || read_herdr_manager(&app, &manager, |m| !m.has_pending_stream()),
            "stream to attach",
        )
        .await;
        // Release so the fake herdr flushes our stdin copy to disk.
        read_herdr_manager(&app, &manager, |m| {
            m.event_loop_tx.try_send(Message::Shutdown).unwrap();
        });
        wait_until(
            || stdin_contents(&bin).contains(r#""type":"terminal.release""#),
            "stdin copy to flush",
        )
        .await;
        // The pre-attach input never reached the subprocess.
        assert!(!stdin_contents(&bin).contains(r#""type":"terminal.input""#));
    });
}

/// A resize sent while the pane is unattached must be flushed through the
/// stream when the attach completes. Exercises the event loop and the spawn
/// worker directly (no TerminalView) so view-generated resizes can't race
/// the assertion.
#[test]
fn resize_before_attach_is_flushed_on_attach() {
    App::test((), |mut app| async move {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_herdr(dir.path());

        let model = Arc::new(FairMutex::new(TerminalModel::mock(None, None)));
        let (wakeups_tx, _wakeups_rx) = async_channel::unbounded();
        let (events_tx, _events_rx) = async_channel::unbounded::<TerminalEvent>();
        let (pty_reads_tx, _pty_reads_rx) = async_broadcast::broadcast(1);
        let listener = ChannelEventListener::new(wakeups_tx, events_tx, pty_reads_tx);
        let (msg_tx, msg_rx) = async_channel::unbounded();
        let (on_closed_tx, _on_closed_rx) = async_channel::unbounded();
        let (relay_tx, relay_rx) = async_channel::bounded(FRAMES_CHANNEL_CAPACITY);
        let stream_slot: StreamSlot = Arc::new(FairMutex::new(None));
        let pending_resize = Arc::new(FairMutex::new(None));
        let _event_loop = app.add_model(|ctx| {
            EventLoop::start(
                model.clone(),
                stream_slot.clone(),
                relay_rx,
                pending_resize.clone(),
                Arc::new(AtomicBool::new(false)),
                msg_rx,
                listener,
                on_closed_tx,
                None,
                None,
                ctx,
            )
        });

        // Resize while unattached; the writer task buffers it.
        msg_tx
            .try_send(Message::Resize(SizeInfo::new_without_font_metrics(43, 132)))
            .unwrap();
        // Let the writer task drain the message into the buffer before
        // attaching so the test doesn't race the slot.
        warpui::r#async::Timer::after(Duration::from_millis(200)).await;

        // Attach through the global worker with the same payload
        // ensure_stream_attached sends.
        enqueue_stream_spawn(StreamSpawnRequest {
            spec: spec_for(bin.clone()),
            slot: Arc::downgrade(&stream_slot),
            frames_relay_tx: relay_tx,
            pending_resize: pending_resize.clone(),
            attach_in_flight: Arc::new(AtomicBool::new(true)),
        })
        .unwrap();
        wait_until(|| stream_slot.lock().is_some(), "stream to attach").await;

        // Frames flow through the relay into the model.
        wait_until(
            || model.lock().terminal_title().as_deref() == Some("herdr-attached"),
            "frames to reach the model",
        )
        .await;

        // Release so the fake herdr flushes our stdin copy to disk.
        msg_tx.try_send(Message::Shutdown).unwrap();
        wait_until(
            || stdin_contents(&bin).contains(r#""type":"terminal.release""#),
            "stdin copy to flush",
        )
        .await;
        let stdin = stdin_contents(&bin);
        // The buffered 132x43 resize (with its 1x1 test cells) was flushed
        // through the new stream on attach.
        assert!(
            stdin.contains(r#""cols":132,"rows":43,"cell_width_px":1,"cell_height_px":1"#),
            "stdin: {stdin}"
        );
    });
}

#[test]
fn shutdown_before_attach_is_a_no_op() {
    App::test((), |mut app| async move {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_herdr(dir.path());
        let manager = create_manager(
            &mut app,
            spec_for(bin.clone()),
            true, /* defer_stream */
        );

        // Permanently closing a never-attached pane has no stream to
        // release; the writer task treats the empty slot as
        // nothing-to-release and exits.
        read_herdr_manager(&app, &manager, |m| {
            m.event_loop_tx.try_send(Message::Shutdown).unwrap();
        });
        warpui::r#async::Timer::after(Duration::from_millis(200)).await;
        assert!(read_herdr_manager(&app, &manager, |m| m.has_pending_stream()));
        assert_eq!(spawn_count(&bin), 0);
    });
}

/// Writes a fake `herdr` like `fake_herdr`, but its title frame carries the
/// spawn ordinal (`herdr-attached-N`) so a test can tell a respawn's frames
/// apart from the previous stream's. Deliberately has NO trailing closed
/// record: these tests SIGKILL the subprocess to simulate a crash, and a
/// `cat`-EOF-then-echo-closed tail would race the SIGKILL (kill() closes
/// stdin first), masking the crash as a clean close and never emitting
/// Died. Without it every death — SIGKILL or stdin EOF — produces Died.
fn fake_herdr_with_spawn_titles(dir: &Path) -> PathBuf {
    let script = r#"#!/bin/sh
echo spawn >> "$0.count"
n=$(wc -l < "$0.count" | tr -d ' ')
title=$(printf '\033]2;herdr-attached-%s\007' "$n" | base64)
echo "{\"type\":\"terminal.frame\",\"seq\":1,\"encoding\":\"ansi\",\"width\":80,\"height\":24,\"full\":true,\"bytes\":\"$title\"}"
cat > "$0.stdin"
"#;
    write_executable_herdr_script(dir, script)
}

/// SIGKILLs the pane's attached stream subprocess — a crash, so no
/// terminal.closed record — and waits for the event loop to register the
/// death by emptying the stream slot.
async fn kill_stream_and_wait(app: &App, manager: &ModelHandle<Box<dyn TerminalManagerTrait>>) {
    read_herdr_manager(app, manager, |m| {
        m.stream_slot
            .lock()
            .as_ref()
            .expect("stream attached")
            .kill();
    });
    wait_until(
        || read_herdr_manager(app, manager, |m| m.has_pending_stream()),
        "stream death to empty the slot",
    )
    .await;
}

/// Kills the attached stream, asserts `restart_stream` accepts the restart,
/// and waits for the respawn to publish.
async fn kill_and_restart(
    app: &App,
    manager: &ModelHandle<Box<dyn TerminalManagerTrait>>,
    bin: &Path,
    expected_spawns: usize,
) {
    kill_stream_and_wait(app, manager).await;
    assert!(read_herdr_manager(app, manager, |m| m.restart_stream()));
    wait_until(|| spawn_count(bin) == expected_spawns, "stream to respawn").await;
    wait_until(
        || read_herdr_manager(app, manager, |m| !m.has_pending_stream()),
        "stream to reattach",
    )
    .await;
}

#[test]
fn restart_stream_respawns_dead_stream() {
    App::test((), |mut app| async move {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_herdr_with_spawn_titles(dir.path());
        let manager = create_manager(
            &mut app,
            spec_for(bin.clone()),
            false, /* defer_stream */
        );
        // The spawn marker is written by the child after fork, so poll for it.
        wait_until(|| spawn_count(&bin) == 1, "subprocess to spawn").await;
        let model = app.read(|ctx| manager.as_ref(ctx).model());
        wait_until(
            || model.lock().terminal_title().as_deref() == Some("herdr-attached-1"),
            "frames to reach the model",
        )
        .await;

        kill_stream_and_wait(&app, &manager).await;
        assert!(read_herdr_manager(&app, &manager, |m| m.restart_stream()));

        // The spawn marker is written by the child after fork, so poll for it.
        wait_until(|| spawn_count(&bin) == 2, "stream to respawn").await;
        wait_until(
            || read_herdr_manager(&app, &manager, |m| !m.has_pending_stream()),
            "stream to reattach",
        )
        .await;
        // The respawn's frames flowed through the same relay into the model.
        wait_until(
            || model.lock().terminal_title().as_deref() == Some("herdr-attached-2"),
            "respawned frames to reach the model",
        )
        .await;
    });
}

#[test]
fn restart_stream_gives_up_after_budget() {
    App::test((), |mut app| async move {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_herdr_with_spawn_titles(dir.path());
        let manager = create_manager(
            &mut app,
            spec_for(bin.clone()),
            false, /* defer_stream */
        );
        // The spawn marker is written by the child after fork, so poll for it.
        wait_until(|| spawn_count(&bin) == 1, "subprocess to spawn").await;
        wait_until(
            || read_herdr_manager(&app, &manager, |m| !m.has_pending_stream()),
            "stream to attach",
        )
        .await;

        // Three deaths inside the window → three restarts (spawns 2..=4).
        for expected_spawns in 2..=4 {
            kill_and_restart(&app, &manager, &bin, expected_spawns).await;
        }

        // A fourth death inside the window exhausts the budget: the restart
        // is refused and no fifth subprocess is spawned.
        kill_stream_and_wait(&app, &manager).await;
        assert!(!read_herdr_manager(&app, &manager, |m| m.restart_stream()));
        // Give any erroneous respawn a chance to appear.
        warpui::r#async::Timer::after(Duration::from_millis(300)).await;
        assert_eq!(spawn_count(&bin), 4);
    });
}

#[test]
fn restart_budget_recovers_after_window() {
    App::test((), |mut app| async move {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_herdr_with_spawn_titles(dir.path());
        let manager = create_manager(
            &mut app,
            spec_for(bin.clone()),
            false, /* defer_stream */
        );
        // The spawn marker is written by the child after fork, so poll for it.
        wait_until(|| spawn_count(&bin) == 1, "subprocess to spawn").await;
        wait_until(
            || read_herdr_manager(&app, &manager, |m| !m.has_pending_stream()),
            "stream to attach",
        )
        .await;

        // Spend the whole budget: three restarts (spawns 2..=4).
        for expected_spawns in 2..=4 {
            kill_and_restart(&app, &manager, &bin, expected_spawns).await;
        }
        kill_stream_and_wait(&app, &manager).await;
        assert!(!read_herdr_manager(&app, &manager, |m| m.restart_stream()));

        // Backdate the recorded restarts beyond the window: with no death
        // for a full window the count resets and the restart is allowed.
        read_herdr_manager(&app, &manager, |m| {
            let mut budget = m.restart_budget.lock();
            for at in &mut budget.recent_restarts {
                *at = Instant::now()
                    .checked_sub(RESTART_WINDOW + Duration::from_secs(1))
                    .expect("test host uptime exceeds the restart window");
            }
        });
        assert!(read_herdr_manager(&app, &manager, |m| m.restart_stream()));
        wait_until(|| spawn_count(&bin) == 5, "stream to respawn").await;
    });
}

/// A restart requested while the stream is alive and attached must not
/// SIGKILL the healthy subprocess: it's treated as already handled.
#[test]
fn restart_stream_with_live_stream_is_a_no_op() {
    App::test((), |mut app| async move {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_herdr(dir.path());
        let manager = create_manager(
            &mut app,
            spec_for(bin.clone()),
            false, /* defer_stream */
        );
        // The spawn marker is written by the child after fork, so poll for it.
        wait_until(|| spawn_count(&bin) == 1, "subprocess to spawn").await;
        wait_until(
            || read_herdr_manager(&app, &manager, |m| !m.has_pending_stream()),
            "stream to attach",
        )
        .await;

        assert!(read_herdr_manager(&app, &manager, |m| m.restart_stream()));
        // Give any erroneous respawn a chance to appear.
        warpui::r#async::Timer::after(Duration::from_millis(300)).await;
        assert_eq!(spawn_count(&bin), 1);
        assert!(read_herdr_manager(&app, &manager, |m| !m.has_pending_stream()));
        let model = app.read(|ctx| manager.as_ref(ctx).model());
        assert_eq!(
            model.lock().terminal_title().as_deref(),
            Some("herdr-attached")
        );
    });
}

/// Restarting a never-attached deferred pane goes through the same queue as
/// its first attach (a queued spawn whose stream died is indistinguishable).
#[test]
fn restart_stream_attaches_never_attached_deferred_pane() {
    App::test((), |mut app| async move {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_herdr(dir.path());
        let manager = create_manager(
            &mut app,
            spec_for(bin.clone()),
            true, /* defer_stream */
        );
        assert_eq!(spawn_count(&bin), 0);

        assert!(read_herdr_manager(&app, &manager, |m| m.restart_stream()));
        // The spawn marker is written by the child after fork, so poll for it.
        wait_until(|| spawn_count(&bin) == 1, "subprocess to spawn").await;
        wait_until(
            || read_herdr_manager(&app, &manager, |m| !m.has_pending_stream()),
            "stream to attach",
        )
        .await;
        let model = app.read(|ctx| manager.as_ref(ctx).model());
        wait_until(
            || model.lock().terminal_title().as_deref() == Some("herdr-attached"),
            "frames to reach the model",
        )
        .await;
    });
}

#[test]
fn restart_budget_caps_and_resets() {
    let mut budget = RestartBudget::new();
    let start = Instant::now();
    assert!(budget.record_restart(start));
    assert!(budget.record_restart(start + Duration::from_secs(1)));
    assert!(budget.record_restart(start + Duration::from_secs(2)));
    // A fourth restart inside the window is refused, and the refusal itself
    // is not recorded.
    assert!(!budget.record_restart(start + Duration::from_secs(3)));
    assert!(!budget.record_restart(start + Duration::from_secs(59)));
    // Once a full window has passed since the early deaths, they expire out
    // of the sliding window and restarts are allowed again.
    let later = start + Duration::from_secs(61);
    assert!(budget.record_restart(later));
    assert!(budget.record_restart(later + Duration::from_secs(1)));
}

/// A restart whose spawn request cannot even be QUEUED (spawn worker gone)
/// must report failure — not promise a spawn that is never coming — and
/// clear the in-flight flag so a later restart can retry. Closing the global
/// queue is irreversible in-process, which is safe only under nextest's
/// process-per-test isolation (the repo's runner).
#[test]
fn restart_stream_reports_failure_when_spawn_queue_unavailable() {
    App::test((), |mut app| async move {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_herdr(dir.path());
        let manager = create_manager(
            &mut app,
            spec_for(bin.clone()),
            true, /* defer_stream */
        );
        crate::terminal::herdr_tty::stream::close_spawn_queue_for_tests();

        read_herdr_manager(&app, &manager, |m| {
            assert!(
                !m.restart_stream(),
                "restart_stream promised a spawn with the queue closed"
            );
            assert!(
                !m.attach_in_flight.load(Ordering::SeqCst),
                "a failed queueing must clear attach_in_flight so a retry can queue"
            );
        });
        assert_eq!(spawn_count(&bin), 0);
    });
}

/// A queued spawn that FAILS (herdr binary gone, attach failure) must clear
/// the in-flight flag AND report a death through the frames relay: without
/// the death event, nothing re-ran the attach for an already-active tab, so
/// the pane sat dead on its last frame indefinitely with no signal.
#[test]
fn failed_spawn_clears_in_flight_and_reports_death() {
    let (relay_tx, relay_rx) = async_channel::bounded(FRAMES_CHANNEL_CAPACITY);
    let stream_slot: StreamSlot = Arc::new(FairMutex::new(None));
    let pending_resize = Arc::new(FairMutex::new(None));
    let attach_in_flight = Arc::new(AtomicBool::new(true));

    enqueue_stream_spawn(StreamSpawnRequest {
        spec: spec_for(PathBuf::from("/nonexistent/herdr-binary")),
        slot: Arc::downgrade(&stream_slot),
        frames_relay_tx: relay_tx,
        pending_resize,
        attach_in_flight: attach_in_flight.clone(),
    })
    .unwrap();

    let died = relay_rx.recv_blocking().unwrap();
    assert!(matches!(died, StreamEvent::Died), "{died:?}");
    assert!(
        !attach_in_flight.load(std::sync::atomic::Ordering::SeqCst),
        "a settled failure must clear the in-flight flag for retries"
    );
    assert!(stream_slot.lock().is_none());
}

/// on_view_detached: HiddenForClose (undo-close grace window) and Moved
/// (this manager is re-hosted in the target group) must PRESERVE the
/// stream — only Closed releases it.
#[test]
fn on_view_detached_preserves_stream_for_hidden_and_moved() {
    App::test((), |mut app| async move {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_herdr(dir.path());
        let manager = create_manager(
            &mut app,
            spec_for(bin.clone()),
            false, /* defer_stream */
        );
        wait_until(
            || read_herdr_manager(&app, &manager, |m| !m.has_pending_stream()),
            "stream to attach",
        )
        .await;

        manager.update(&mut app, |m, ctx| {
            m.on_view_detached(DetachType::HiddenForClose, ctx);
            m.on_view_detached(DetachType::Moved, ctx);
        });
        // Give the writer task a beat: nothing must be released.
        warpui::r#async::Timer::after(Duration::from_millis(200)).await;
        assert!(!stdin_contents(&bin).contains(r#""type":"terminal.release""#));

        manager.update(&mut app, |m, ctx| {
            m.on_view_detached(DetachType::Closed, ctx);
        });
        wait_until(
            || stdin_contents(&bin).contains(r#""type":"terminal.release""#),
            "closed detach to release the stream",
        )
        .await;
    });
}
