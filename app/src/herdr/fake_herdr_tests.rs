use super::*;
use crate::herdr::connection::HerdrConnection;
use crate::herdr::layout_convert::HerdrPaneTree;
use crate::herdr::socket::HerdrApi;

#[test]
fn bootstrap_gates_version_and_exports_layouts() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("herdr.sock");
    let server = serve_scripted(
        &sock,
        vec![
            ("ping", r#"{"type":"pong","version":"0.7.4","protocol":17}"#),
            ("session.snapshot", SNAPSHOT_JSON),
            ("layout.export", LAYOUT_T1_JSON),
            ("layout.export", LAYOUT_T2_JSON),
        ],
    );
    let api = HerdrApi::at(sock);
    let (snapshot, layouts) = HerdrConnection::bootstrap(&api).unwrap();
    assert_eq!(snapshot.tabs.len(), 2);
    assert_eq!(layouts.len(), 2);
    assert_eq!(layouts[0].0, "w1:t1");
    assert!(matches!(layouts[0].1, HerdrPaneTree::Leaf { .. }));
    assert_eq!(layouts[1].1.pane_ids(), vec!["w1:p2", "w1:p3"]);
    server.join().unwrap();
}

#[test]
fn bootstrap_rejects_old_herdr() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("herdr.sock");
    let _server = serve_scripted(
        &sock,
        vec![("ping", r#"{"type":"pong","version":"0.7.3","protocol":16}"#)],
    );
    let api = HerdrApi::at(sock);
    let err = HerdrConnection::bootstrap(&api).unwrap_err();
    assert!(err.to_string().contains("0.7.4"), "{err}");
}

#[test]
fn bootstrap_subscribed_refreshes_snapshot_after_live_only_subscription() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("herdr.sock");
    let pong = r#"{"type":"pong","version":"0.9.5","protocol":22}"#;
    let current_snapshot = SNAPSHOT_JSON.replace("agents", "renamed before subscribe");
    let server = serve_scripted(
        &sock,
        vec![
            ("ping", pong.to_string()),
            ("session.snapshot", SNAPSHOT_JSON.to_string()),
            ("layout.export", LAYOUT_T1_JSON.to_string()),
            ("layout.export", LAYOUT_T2_JSON.to_string()),
            (
                "events.subscribe",
                r#"{"type":"subscription_started"}"#.to_string(),
            ),
            ("ping", pong.to_string()),
            ("session.snapshot", current_snapshot),
            ("layout.export", LAYOUT_T1_JSON.to_string()),
            ("layout.export", LAYOUT_T2_JSON.to_string()),
        ],
    );
    let bootstrap = HerdrConnection::bootstrap_subscribed(&HerdrApi::at(sock)).unwrap();
    assert_eq!(bootstrap.snapshot.tabs[0].label, "renamed before subscribe");
    assert_eq!(bootstrap.layouts.len(), 2);
    server.join().unwrap();
}
