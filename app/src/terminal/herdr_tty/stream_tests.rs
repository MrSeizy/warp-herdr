//! `HerdrStream` tests against a fake `herdr` shell script: frame parsing,
//! control-command forwarding, and close signaling. Unix-only (gated at the
//! `#[path]` include): the fixture is a `#!/bin/sh` script made executable
//! via `PermissionsExt`.

use super::*;
use crate::herdr::fake_herdr::{test_stream_spec, write_executable_herdr_script};
use crate::terminal::model::index::Point;
use crate::terminal::model::mouse::{MouseAction, MouseButton};

/// Writes a fake `herdr` that emits two frames then a closed record, and
/// echoes its stdin to a file for assertions.
fn fake_herdr(dir: &std::path::Path) -> std::path::PathBuf {
    let script = r#"#!/bin/sh
echo '{"type":"terminal.frame","seq":1,"encoding":"ansi","width":80,"height":24,"full":true,"bytes":"aGVsbG8="}'
echo '{"type":"terminal.frame","seq":2,"encoding":"ansi","width":80,"height":24,"full":false,"bytes":"IHdvcmxk"}'
cat > "$0.stdin"
echo '{"type":"terminal.closed","reason":"detached"}'
"#;
    write_executable_herdr_script(dir, script)
}

#[test]
fn streams_frames_then_close() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_herdr(dir.path());
    let spec = test_stream_spec(bin, "w1:p1");
    let (tx, rx) = async_channel::bounded(FRAMES_CHANNEL_CAPACITY);
    let stream = HerdrStream::spawn(&spec, tx).unwrap();
    match rx.recv_blocking().unwrap() {
        StreamEvent::Frame(bytes) => assert_eq!(bytes, b"hello"),
        other => panic!("unexpected: {other:?}"),
    }
    match rx.recv_blocking().unwrap() {
        StreamEvent::Frame(bytes) => assert_eq!(bytes, b" world"),
        other => panic!("unexpected: {other:?}"),
    }
    stream.send_input(b"hi");
    stream.release();
    match rx.recv_blocking().unwrap() {
        StreamEvent::Closed { reason } => assert_eq!(reason.as_deref(), Some("detached")),
        other => panic!("unexpected: {other:?}"),
    }
    // stdin got both commands, one JSON per line:
    let stdin_file = spec_stdin_path(&spec);
    let written = std::fs::read_to_string(stdin_file).unwrap();
    assert!(written.contains(r#""type":"terminal.input""#));
    assert!(written.contains(r#""type":"terminal.release""#));
}

#[test]
fn herdr_stream_forwards_mouse_input_with_upstream_records() {
    let dir = tempfile::tempdir().unwrap();
    let bin = write_executable_herdr_script(
        dir.path(),
        r#"#!/bin/sh
echo '{"type":"terminal.frame","bytes":"aGVsbG8="}'
cat > "$0.stdin"
echo '{"type":"terminal.closed","reason":"detached"}'
"#,
    );
    let spec = test_stream_spec(bin, "w1:p1");
    let (tx, rx) = async_channel::bounded(FRAMES_CHANNEL_CAPACITY);
    let stream = HerdrStream::spawn(&spec, tx).unwrap();
    match rx.recv_blocking().unwrap() {
        StreamEvent::Frame(bytes) => assert_eq!(bytes, b"hello"),
        other => panic!("unexpected: {other:?}"),
    }
    stream.mouse(
        &MouseState::new(MouseButton::Left, MouseAction::Pressed, Default::default())
            .set_point(Point::new(2, 6)),
    );
    stream.release();
    assert!(matches!(
        rx.recv_blocking().unwrap(),
        StreamEvent::Closed { .. }
    ));
    assert_eq!(
        std::fs::read_to_string(spec_stdin_path(&spec)).unwrap(),
        "{\"type\":\"terminal.mouse\",\"action\":\"down\",\"button\":\"left\",\"column\":6,\"row\":2,\"modifiers\":0}\n{\"type\":\"terminal.release\"}\n"
    );
}

/// Scroll gestures serialize to herdr's `terminal.scroll` command with an
/// explicit direction, the source herdr routes on, and lines clamped to the
/// protocol's u16. Zero lines never hit the wire (herdr rejects 0).
#[test]
fn scroll_writes_terminal_scroll_commands() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_herdr(dir.path());
    let spec = test_stream_spec(bin, "w1:p1");
    let (tx, rx) = async_channel::bounded(FRAMES_CHANNEL_CAPACITY);
    let stream = HerdrStream::spawn(&spec, tx).unwrap();
    stream.scroll(true, 3, ScrollSource::Wheel);
    stream.scroll(false, u32::from(u16::MAX) + 100, ScrollSource::PageKey);
    stream.scroll(true, 0, ScrollSource::Wheel);
    stream.release();
    // Drain until the fake herdr closes so its stdin copy is flushed.
    loop {
        match rx.recv_blocking().unwrap() {
            StreamEvent::Closed { .. } => break,
            StreamEvent::Frame(_) | StreamEvent::Died => {}
        }
    }
    let written = std::fs::read_to_string(spec_stdin_path(&spec)).unwrap();
    assert!(
        written.contains(
            r#"{"type":"terminal.scroll","direction":"up","lines":3,"source":"wheel","modifiers":0}"#
        ),
        "stdin: {written}"
    );
    assert!(
        written.contains(
            r#"{"type":"terminal.scroll","direction":"down","lines":65535,"source":"page_key","modifiers":0}"#
        ),
        "stdin: {written}"
    );
    assert_eq!(
        written.matches(r#""type":"terminal.scroll""#).count(),
        2,
        "the zero-line scroll must not be sent; stdin: {written}"
    );
}

/// kill() — and thus Drop — must not queue behind a write blocked on a full
/// stdin pipe: a stalled subprocess that never reads its stdin would
/// otherwise wedge pane teardown until the subprocess exits on its own.
/// Killing the child is exactly what unblocks the write (EPIPE), so the
/// lock order matters, not just latency.
#[test]
fn kill_is_not_blocked_by_a_stalled_stdin_write() {
    use instant::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    // Never reads stdin and never exits on its own (within the test).
    let bin = write_executable_herdr_script(dir.path(), "#!/bin/sh\nexec sleep 30\n");
    let spec = test_stream_spec(bin, "w1:p1");
    let (tx, _rx) = async_channel::bounded(FRAMES_CHANNEL_CAPACITY);
    let stream = HerdrStream::spawn(&spec, tx).unwrap();
    let stream = Arc::new(stream);

    // A multi-megabyte input overfills the pipe buffer (~64KiB), so this
    // write blocks inside write_all until the subprocess dies.
    let writer = {
        let stream = stream.clone();
        std::thread::spawn(move || stream.send_input(&vec![b'x'; 4 * 1024 * 1024]))
    };
    // Give the writer time to enter the blocking write before killing.
    std::thread::sleep(Duration::from_millis(300));

    let killer = {
        let stream = stream.clone();
        std::thread::spawn(move || stream.kill())
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !killer.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        killer.is_finished(),
        "kill() wedged behind the blocked stdin write"
    );
    killer.join().unwrap();
    // The SIGKILL broke the pipe, so the blocked write errored out and the
    // writer finished too.
    writer.join().unwrap();
}

fn spec_stdin_path(spec: &HerdrStreamSpec) -> std::path::PathBuf {
    spec.herdr_bin.with_file_name("herdr.stdin")
}
