//! Test double for a herdr session: a real unix socket serving scripted
//! responses, for exercising bootstrap and the socket client end to end.

use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::UnixListener;
#[cfg(windows)]
use std::os::windows::net::UnixListener;
use std::path::{Path, PathBuf};

use crate::terminal::herdr_tty::stream::HerdrStreamSpec;

/// Writes `script` as an executable fake `herdr` at `dir/herdr` and returns
/// its path. Each test file supplies its own script; the chmod-and-return
/// plumbing lives here once.
#[cfg(unix)]
pub fn write_executable_herdr_script(dir: &Path, script: &str) -> PathBuf {
    let path = dir.join("herdr");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// An all-defaults stream spec for tests: default session, no cwd/agent,
/// 80x24, targeting herdr pane `target`.
pub fn test_stream_spec(herdr_bin: impl Into<PathBuf>, target: &str) -> HerdrStreamSpec {
    HerdrStreamSpec {
        herdr_bin: herdr_bin.into(),
        session: None,
        target: target.to_string(),
        cwd: None,
        agent: None,
        agent_status: crate::herdr::protocol::AgentStatus::Unknown,
        cols: 80,
        rows: 24,
    }
}

/// Accepts one connection per scripted entry (herdr allows one request per
/// connection). Asserts each request's method matches the script order and
/// replies with the canned result wrapped in a response envelope.
pub fn serve_scripted(
    path: &Path,
    script: Vec<(&'static str, impl AsRef<str> + Send + 'static)>,
) -> std::thread::JoinHandle<()> {
    let listener = UnixListener::bind(path).unwrap();
    std::thread::spawn(move || {
        for (method, result) in script {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(req["method"], method, "unexpected method order");
            let response = serde_json::json!({
                "id": req["id"],
                "result": serde_json::from_str::<serde_json::Value>(result.as_ref()).unwrap(),
            });
            let mut w = stream;
            w.write_all(response.to_string().as_bytes()).unwrap();
            w.write_all(b"\n").unwrap();
        }
    })
}

/// Accepts a single connection, captures the request line, writes each of
/// `response_lines` verbatim (newline-terminated), and closes. The token
/// `$ID` in a response line is replaced with the request's id so responses
/// can echo it. Returns the captured request line on join. Use this for flows
/// serve_scripted can't express: error envelopes and the long-lived
/// events.subscribe stream.
pub fn serve_raw(path: &Path, response_lines: Vec<String>) -> std::thread::JoinHandle<String> {
    let listener = UnixListener::bind(path).unwrap();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let id = req["id"].as_str().unwrap().to_string();
        let mut w = stream;
        for response in response_lines {
            w.write_all(response.replace("$ID", &id).as_bytes())
                .unwrap();
            w.write_all(b"\n").unwrap();
        }
        line.trim().to_string()
    })
}

/// Like serve_raw, but keeps the connection open after the responses instead
/// of closing it — a half-alive server, from the client's point of view. The
/// thread parks until process exit; tests must not join it.
pub fn serve_raw_holding_open(
    path: &Path,
    response_lines: Vec<String>,
) -> std::thread::JoinHandle<()> {
    let listener = UnixListener::bind(path).unwrap();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let id = req["id"].as_str().unwrap().to_string();
        let mut w = stream.try_clone().unwrap();
        for response in response_lines {
            w.write_all(response.replace("$ID", &id).as_bytes())
                .unwrap();
            w.write_all(b"\n").unwrap();
        }
        // Hold the socket open: park on a read that never completes
        // (the client never sends a second line).
        let _ = reader.read_line(&mut line);
    })
}

pub const SNAPSHOT_JSON: &str = r#"{"type":"session_snapshot","snapshot":{
    "version":"0.7.4","protocol":17,
    "focused_workspace_id":"w1","focused_tab_id":"w1:t1","focused_pane_id":"w1:p1",
    "workspaces":[{"workspace_id":"w1","number":1,"label":"proj","focused":true,
        "active_tab_id":"w1:t1","agent_status":"idle"}],
    "tabs":[
        {"tab_id":"w1:t1","workspace_id":"w1","number":1,"label":"agents",
            "focused":true,"pane_count":1,"agent_status":"idle"},
        {"tab_id":"w1:t2","workspace_id":"w1","number":2,"label":"build",
            "focused":false,"pane_count":2,"agent_status":"working"}],
    "panes":[
        {"pane_id":"w1:p1","terminal_id":"term_a","workspace_id":"w1","tab_id":"w1:t1",
            "focused":true,"agent_status":"idle","revision":1},
        {"pane_id":"w1:p2","terminal_id":"term_b","workspace_id":"w1","tab_id":"w1:t2",
            "focused":false,"agent_status":"working","revision":1},
        {"pane_id":"w1:p3","terminal_id":"term_c","workspace_id":"w1","tab_id":"w1:t2",
            "focused":false,"agent_status":"idle","revision":1}]}}"#;

pub const LAYOUT_T1_JSON: &str = r#"{"type":"layout_export","layout":{
    "workspace_id":"w1","tab_id":"w1:t1","zoomed":false,"focused_pane_id":"w1:p1",
    "root":{"type":"pane","pane_id":"w1:p1"}}}"#;

pub const LAYOUT_T2_JSON: &str = r#"{"type":"layout_export","layout":{
    "workspace_id":"w1","tab_id":"w1:t2","zoomed":false,"focused_pane_id":"w1:p2",
    "root":{"type":"split","direction":"right","ratio":0.5,
        "first":{"type":"pane","pane_id":"w1:p2"},
        "second":{"type":"pane","pane_id":"w1:p3"}}}}"#;

#[cfg(test)]
#[path = "fake_herdr_tests.rs"]
mod tests;
