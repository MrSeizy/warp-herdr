use super::*;
use crate::herdr::fake_herdr::{serve_raw, serve_raw_holding_open, serve_scripted};
use crate::herdr::protocol::AnyEvent;

#[test]
fn call_sends_one_line_and_parses_response() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("herdr.sock");
    // serve_scripted asserts the method and echoes the request id back.
    let server = serve_scripted(
        &sock,
        vec![("ping", r#"{"type":"pong","version":"0.7.4","protocol":17}"#)],
    );
    let api = HerdrApi::at(sock);
    let pong = api.ping().unwrap();
    assert_eq!(pong.version, "0.7.4");
    server.join().unwrap();
}

#[test]
fn call_bails_on_response_id_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("herdr.sock");
    let server = serve_raw(
        &sock,
        vec![
            r#"{"id":"bogus","result":{"type":"pong","version":"0.7.4","protocol":17}}"#
                .to_string(),
        ],
    );
    let api = HerdrApi::at(sock);
    let err = api.ping().unwrap_err();
    assert!(err.to_string().contains("does not match"), "{err}");
    server.join().unwrap();
}

#[test]
fn error_response_becomes_err_with_code() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("herdr.sock");
    let server = serve_raw(
        &sock,
        vec![r#"{"id":"$ID","error":{"code":"tab_create_failed","message":"boom"}}"#.to_string()],
    );
    let api = HerdrApi::at(sock);
    let err = api.tab_close("w1:t9").unwrap_err();
    assert!(err.to_string().contains("tab_create_failed"), "{err}");
    server.join().unwrap();
}

/// tab.move's wire shape is exactly `{"tab_id": ..., "insert_index": ...}` —
/// no workspace_id — matching herdr's TabMoveParams (insert_index is 0-based
/// and may equal the tab count for append-at-end).
#[test]
fn tab_move_sends_exact_wire_json() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("herdr.sock");
    let server = serve_raw(
        &sock,
        vec![r#"{"id":"$ID","result":{"type":"tab_list","tabs":[]}}"#.to_string()],
    );
    let api = HerdrApi::at(sock);
    api.tab_move("w1:t1", 2).unwrap();
    let request = server.join().unwrap();
    let req: serde_json::Value = serde_json::from_str(&request).unwrap();
    assert_eq!(req["method"], "tab.move");
    assert_eq!(
        req["params"],
        serde_json::json!({"tab_id": "w1:t1", "insert_index": 2})
    );
}

#[test]
fn pane_rename_sends_exact_wire_json_when_setting_label() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("herdr.sock");
    let server = serve_raw(
        &sock,
        vec![r#"{"id":"$ID","result":{"type":"pane_info","pane":{"pane_id":"w1:p1","terminal_id":"term_1","workspace_id":"w1","tab_id":"w1:t1","focused":true,"agent_status":"idle","revision":1,"label":"reviewer"}}}"#.to_string()],
    );
    let api = HerdrApi::at(sock);

    let pane = api.pane_rename("w1:p1", Some("reviewer")).unwrap();

    assert_eq!(pane.label.as_deref(), Some("reviewer"));
    let request = server.join().unwrap();
    let req: serde_json::Value = serde_json::from_str(&request).unwrap();
    assert_eq!(req["method"], "pane.rename");
    assert_eq!(
        req["params"],
        serde_json::json!({"pane_id": "w1:p1", "label": "reviewer"})
    );
}

#[test]
fn pane_rename_omits_label_when_clearing() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("herdr.sock");
    let server = serve_raw(
        &sock,
        vec![r#"{"id":"$ID","result":{"type":"pane_info","pane":{"pane_id":"w1:p1","terminal_id":"term_1","workspace_id":"w1","tab_id":"w1:t1","focused":true,"agent_status":"idle","revision":1}}}"#.to_string()],
    );
    let api = HerdrApi::at(sock);

    let pane = api.pane_rename("w1:p1", None).unwrap();

    assert_eq!(pane.label, None);
    let request = server.join().unwrap();
    let req: serde_json::Value = serde_json::from_str(&request).unwrap();
    assert_eq!(req["method"], "pane.rename");
    assert_eq!(req["params"], serde_json::json!({"pane_id": "w1:p1"}));
}

#[test]
fn subscribe_streams_ack_then_events() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("herdr.sock");
    let server = serve_raw(
        &sock,
        vec![
            r#"{"id":"$ID","result":{"type":"subscription_started"}}"#.to_string(),
            r#"{"event":"tab_renamed","data":{"type":"tab_renamed","tab_id":"w1:t1","workspace_id":"w1","label":"x"}}"#.to_string(),
            r#"{"event":"pane.agent_status_changed","data":{"pane_id":"w1:p1","workspace_id":"w1","agent_status":"blocked"}}"#.to_string(),
        ],
    );
    let api = HerdrApi::at(sock);
    let (rx, _shutdown) = api.subscribe_events(&[]).unwrap();
    let first = rx.recv_blocking().unwrap();
    assert!(matches!(first, AnyEvent::Lifecycle(_)));
    let second = rx.recv_blocking().unwrap();
    assert!(matches!(second, AnyEvent::AgentStatus(_)));
    // server closes → channel closes
    assert!(rx.recv_blocking().is_err());
    server.join().unwrap();
}

/// Shutting down the stream handle unblocks the reader thread (EOF) and
/// closes the channel — without it, a superseded stream's thread and fd
/// leak until the server happens to close the socket.
#[test]
fn subscribe_shutdown_handle_closes_stream() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("herdr.sock");
    let _server = serve_raw_holding_open(
        &sock,
        vec![r#"{"id":"$ID","result":{"type":"subscription_started"}}"#.to_string()],
    );
    let api = HerdrApi::at(sock);
    let (rx, shutdown) = api.subscribe_events(&[]).unwrap();
    shutdown.shutdown();
    // The reader thread sees EOF and drops the sender; the channel closes
    // even though the server still holds its end open.
    assert!(rx.recv_blocking().is_err());
}

#[test]
fn dropping_subscribe_handle_closes_stream() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("herdr.sock");
    let _server = serve_raw_holding_open(
        &sock,
        vec![r#"{"id":"$ID","result":{"type":"subscription_started"}}"#.to_string()],
    );
    let api = HerdrApi::at(sock);
    let (rx, shutdown) = api.subscribe_events(&[]).unwrap();
    drop(shutdown);
    // The reader thread sees EOF and drops the sender; the channel closes
    // even though the server still holds its end open.
    assert!(rx.recv_blocking().is_err());
}

#[test]
fn subscribe_registers_per_pane_agent_status() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("herdr.sock");
    let server = serve_raw(
        &sock,
        vec![r#"{"id":"$ID","result":{"type":"subscription_started"}}"#.to_string()],
    );
    let api = HerdrApi::at(sock);
    let (rx, _shutdown) = api
        .subscribe_events(&["w1:p1".to_string(), "w1:p2".to_string()])
        .unwrap();
    drop(rx);
    let request = server.join().unwrap();
    let req: serde_json::Value = serde_json::from_str(&request).unwrap();
    assert_eq!(req["method"], "events.subscribe");
    let subs = req["params"]["subscriptions"].as_array().unwrap();
    assert!(
        subs.contains(
            &serde_json::json!({"type": "pane.agent_status_changed", "pane_id": "w1:p1"})
        )
    );
    assert!(
        subs.contains(
            &serde_json::json!({"type": "pane.agent_status_changed", "pane_id": "w1:p2"})
        )
    );
    // Focus is one-way Warp→herdr, so no workspace.focused subscription.
    assert!(!subs.contains(&serde_json::json!({"type": "workspace.focused"})));
}

#[test]
fn subscribe_rejection_returns_server_error() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("herdr.sock");
    let server = serve_raw(
        &sock,
        vec![
            r#"{"id":"$ID","error":{"code":"bad_subscription","message":"unknown subscription type"}}"#
                .to_string(),
        ],
    );
    let api = HerdrApi::at(sock);
    let err = api.subscribe_events(&[]).unwrap_err();
    assert!(
        err.to_string().contains("unknown subscription type"),
        "{err}"
    );
    server.join().unwrap();
}

#[cfg(unix)]
#[test]
fn discovery_lists_default_and_named_sessions() {
    use std::os::unix::net::UnixListener;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("herdr.sock.placeholder"), b"").unwrap();
    // Real discovery checks for actual sockets; create fake sockets:
    let _l1 = UnixListener::bind(dir.path().join("herdr.sock")).unwrap();
    std::fs::create_dir_all(dir.path().join("sessions/alpha")).unwrap();
    let _l2 = UnixListener::bind(dir.path().join("sessions/alpha/herdr.sock")).unwrap();
    let found = discover_sessions_in(dir.path());
    let names: Vec<_> = found.iter().map(|s| s.name.clone()).collect();
    assert!(names.contains(&"default".to_string()));
    assert!(names.contains(&"alpha".to_string()));
}

// Single test for all env-var-driven behavior so it cannot race with itself
// when run under plain `cargo test` (nextest isolates processes already).
#[cfg(unix)]
#[test]
fn env_overrides_restrict_discovery() {
    use std::os::unix::net::UnixListener;

    let dir = tempfile::tempdir().unwrap();
    let orig_xdg = std::env::var_os("XDG_CONFIG_HOME");

    // HERDR_SOCKET_PATH is honored verbatim as the single candidate, even
    // when the path is not (yet) a socket.
    let bogus = dir.path().join("not-a-socket.sock");
    // SAFETY: This is the only app test that mutates Herdr discovery variables.
    unsafe { std::env::set_var("HERDR_SOCKET_PATH", &bogus) };
    let found = discover_sessions();
    assert_eq!(
        found,
        vec![DiscoveredSession {
            name: "default".into(),
            socket_path: bogus,
        }]
    );
    // SAFETY: This is the only app test that mutates Herdr discovery variables.
    unsafe { std::env::remove_var("HERDR_SOCKET_PATH") };

    // HERDR_SESSION restricts discovery to the named session.
    let config_dir = dir.path().join("herdr");
    std::fs::create_dir_all(config_dir.join("sessions/alpha")).unwrap();
    let _l1 = UnixListener::bind(config_dir.join("herdr.sock")).unwrap();
    let _l2 = UnixListener::bind(config_dir.join("sessions/alpha/herdr.sock")).unwrap();
    // SAFETY: This is the only app test that mutates Herdr discovery variables.
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", dir.path());
        std::env::set_var("HERDR_SESSION", "alpha");
    }
    let found = discover_sessions();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "alpha");
    // SAFETY: This is the only app test that mutates Herdr discovery variables.
    unsafe { std::env::remove_var("HERDR_SESSION") };
    let found = discover_sessions();
    assert_eq!(found.len(), 2);

    // SAFETY: This is the only app test that mutates Herdr discovery variables.
    unsafe {
        match orig_xdg {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }
}
