//! Connection-lifecycle unit tests over a recording WorkspaceOps fake:
//! intent rollback on API failure and the batched resync rebuild.
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::UnixListener;
#[cfg(windows)]
use std::os::windows::net::UnixListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use super::{
    OutstandingPaneRename, PaneRenameCompletion, PaneRenameQueue, PaneRenameRequest,
    WatchdogSnapshot, apply_stream_event, complete_pane_rename, complete_pane_rename_response,
    note_local_pane_label_mutation, pane_rename_to_schedule, rebuild_from_snapshot,
    reconcile_watchdog_snapshot_labels, reconnect_loop, refreshed_subscribe_set,
    resolve_intent_on_api_result, watchdog_snapshot_probe,
};
use crate::herdr::fake_herdr::{SNAPSHOT_JSON, serve_scripted};
use crate::herdr::layout_convert::HerdrPaneTree;
use crate::herdr::protocol::*;
use crate::herdr::reconciler::{Intent, Reconciler, WorkspaceOps};
use crate::herdr::socket::HerdrApi;

#[derive(Default)]
struct RecordingOps {
    calls: Vec<String>,
}

impl WorkspaceOps for RecordingOps {
    fn create_workspace_group(&mut self, ws: &WorkspaceInfo) {
        self.calls
            .push(format!("group+ {} {}", ws.workspace_id, ws.label));
    }
    fn create_tab(&mut self, tab: &TabInfo, layout: &HerdrPaneTree) {
        self.calls
            .push(format!("tab+ {} panes={:?}", tab.tab_id, layout.pane_ids()));
    }
    fn close_tab(&mut self, tab_id: &str) {
        self.calls.push(format!("tab- {tab_id}"));
    }
    fn rename_tab(&mut self, tab_id: &str, label: &str) {
        self.calls.push(format!("tab~ {tab_id} {label}"));
    }
    fn set_tab_status(&mut self, tab_id: &str, status: AgentStatus) {
        self.calls.push(format!("status {tab_id} {status:?}"));
    }
    fn add_split(&mut self, tab_id: &str, pane: &PaneInfo) {
        self.calls
            .push(format!("pane+ {} in {tab_id}", pane.pane_id));
    }
    fn close_pane(&mut self, pane_id: &str) {
        self.calls.push(format!("pane- {pane_id}"));
    }
    fn set_pane_title(&mut self, pane_id: &str, title: &str) {
        self.calls.push(format!("title {pane_id} {title}"));
    }
    fn set_pane_label(&mut self, pane_id: &str, label: Option<&str>) {
        self.calls
            .push(format!("label {pane_id} {}", label.unwrap_or("-")));
    }
    fn set_pane_agent(&mut self, pane_id: &str, agent: Option<&str>) {
        self.calls
            .push(format!("agent {pane_id} {}", agent.unwrap_or("-")));
    }
    fn set_pane_agent_status(&mut self, pane_id: &str, status: AgentStatus) {
        self.calls.push(format!("pane_status {pane_id} {status:?}"));
    }
    fn request_layout_refresh(&mut self, tab_id: &str) {
        self.calls.push(format!("relayout {tab_id}"));
    }
    fn set_materializing(&mut self, active: bool) {
        self.calls.push(format!("materializing {active}"));
    }
    fn attach_active_tab_streams(&mut self) {
        self.calls.push("attach-active".into());
    }
}

/// A single-workspace ("w1") snapshot with one single-pane tab per
/// (tab_id, label, pane_id) triple.
fn snapshot_for(tabs: &[(&str, &str, &str)]) -> (SessionSnapshot, Vec<(String, HerdrPaneTree)>) {
    let snapshot = SessionSnapshot {
        version: "0.7.4".into(),
        protocol: 17,
        focused_workspace_id: Some("w1".into()),
        focused_tab_id: None,
        focused_pane_id: None,
        workspaces: vec![WorkspaceInfo {
            workspace_id: "w1".into(),
            number: 1,
            label: "proj".into(),
            focused: true,
            active_tab_id: tabs
                .first()
                .map(|(t, _, _)| t.to_string())
                .unwrap_or_default(),
            agent_status: AgentStatus::Idle,
        }],
        tabs: tabs
            .iter()
            .map(|(tab_id, label, _)| TabInfo {
                tab_id: tab_id.to_string(),
                workspace_id: "w1".into(),
                number: 1,
                label: label.to_string(),
                focused: false,
                pane_count: 1,
                agent_status: AgentStatus::Idle,
            })
            .collect(),
        panes: tabs
            .iter()
            .map(|(tab_id, _, pane_id)| PaneInfo {
                pane_id: pane_id.to_string(),
                terminal_id: format!("term_{pane_id}"),
                workspace_id: "w1".into(),
                tab_id: tab_id.to_string(),
                focused: false,
                cwd: None,
                label: None,
                title: None,
                terminal_title: None,
                display_agent: None,
                agent: None,
                agent_status: AgentStatus::Idle,
                revision: 1,
            })
            .collect(),
    };
    let layouts = tabs
        .iter()
        .map(|(tab_id, _, pane_id)| {
            (
                tab_id.to_string(),
                HerdrPaneTree::Leaf {
                    pane_id: pane_id.to_string(),
                    cwd: None,
                    agent: None,
                    agent_status: AgentStatus::Unknown,
                },
            )
        })
        .collect();
    (snapshot, layouts)
}

fn seeded() -> (Reconciler, RecordingOps) {
    let mut rec = Reconciler::default();
    let mut ops = RecordingOps::default();
    let (snapshot, layouts) = snapshot_for(&[("w1:t1", "agents", "w1:p1")]);
    rec.apply_snapshot(&snapshot, &layouts, &mut ops);
    (rec, ops)
}

/// A failed Warp-initiated API call rolls its intent back: the next
/// herdr-initiated event for the same target must still apply (a stale intent
/// would have swallowed it).
#[test]
fn failed_api_call_rolls_back_intent() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();

    rec.note_local_intent(Intent::CloseTab("w1:t1".into()));
    resolve_intent_on_api_result(
        &mut rec,
        Some(Intent::CloseTab("w1:t1".into())),
        Err(anyhow::anyhow!("boom")),
    );
    rec.apply_event(
        &EventData::TabClosed {
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
        },
        &mut ops,
    );
    assert_eq!(ops.calls, vec!["tab- w1:t1"]);
}

/// A successful call keeps the intent pending: the echoed event is swallowed
/// (Warp already applied the change locally).
#[test]
fn successful_api_call_keeps_intent_swallowing_echo() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();

    rec.note_local_intent(Intent::CloseTab("w1:t1".into()));
    resolve_intent_on_api_result(&mut rec, Some(Intent::CloseTab("w1:t1".into())), Ok(()));
    rec.apply_event(
        &EventData::TabClosed {
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
        },
        &mut ops,
    );
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
}

/// A subscription refresh must only name panes herdr's live snapshot can
/// resolve (one dead id makes herdr reject the whole subscribe), and a
/// filtered set identical to what the live subscription already covers must
/// not resubscribe at all: every resubscribe replays the event backlog, a
/// replayed created/closed pair for a since-dead pane re-marks the
/// subscription stale, and the resubscribe→replay→stale cycle looped once
/// per second with tabs visibly flapping through replayed tab.moved history
/// (live 2026-07-21).
#[test]
fn refresh_subscribe_set_filters_dead_panes_and_skips_no_change() {
    let (snapshot, _) = snapshot_for(&[("w1:t1", "agents", "w1:p1")]);
    assert_eq!(
        refreshed_subscribe_set(
            vec!["w1:p1".to_string(), "w1:pDEAD".to_string()],
            &snapshot,
            &[],
        ),
        Some(vec!["w1:p1".to_string()]),
        "a genuinely new live pane resubscribes, dead ids filtered out"
    );
    assert_eq!(
        refreshed_subscribe_set(
            vec!["w1:p1".to_string(), "w1:pDEAD".to_string()],
            &snapshot,
            &["w1:p1".to_string()],
        ),
        None,
        "already-covered set must not resubscribe"
    );
}

/// Resync rebuilds from a fresh snapshot: every stale tab closes in one batch
/// before anything re-materializes, the whole rebuild sits inside an outer
/// materializing guard, and intents noted toward the dead server are
/// forgotten.
#[test]
fn resync_rebuild_batches_stale_closes_under_one_materializing_guard() {
    let (mut rec, mut ops) = seeded();
    rec.note_local_intent(Intent::RenameTab {
        tab_id: "w1:t1".into(),
        label: "x".into(),
    }); // toward the dead server
    ops.calls.clear();

    // After a restart herdr re-mints w1:t1 and has a new tab w1:t2.
    let (snapshot, layouts) =
        snapshot_for(&[("w1:t1", "agents", "w1:p1"), ("w1:t2", "tests", "w1:p2")]);
    rebuild_from_snapshot(&mut rec, &snapshot, &layouts, &mut ops);

    // One outer materializing window wraps the whole rebuild (the reconciler
    // may add finer per-tab guards inside it); guards stay balanced.
    assert_eq!(
        ops.calls.first().map(String::as_str),
        Some("materializing true"),
        "{:?}",
        ops.calls
    );
    // The rebuild ends by attaching the active tab's deferred streams, after
    // the outer guard has closed (the attach must see materializing off).
    assert_eq!(
        ops.calls.last().map(String::as_str),
        Some("attach-active"),
        "{:?}",
        ops.calls
    );
    // Guards nest (outer rebuild bracket wrapping the reconciler's per-tab
    // brackets): the depth must stay positive across every create and land
    // back at exactly zero.
    let mut depth = 0i32;
    for call in &ops.calls {
        match call.as_str() {
            "materializing true" => depth += 1,
            "materializing false" => depth -= 1,
            "attach-active" => {
                assert_eq!(depth, 0, "attach inside materializing: {:?}", ops.calls)
            }
            c if c.starts_with("tab+ ") => {
                assert!(depth >= 1, "create outside materializing: {:?}", ops.calls)
            }
            _ => {}
        }
        assert!(depth >= 0, "guard depth went negative: {:?}", ops.calls);
    }
    assert_eq!(depth, 0, "unbalanced guards: {:?}", ops.calls);
    // The one stale tab closes before anything is re-created.
    let closes: Vec<&str> = ops
        .calls
        .iter()
        .map(String::as_str)
        .filter(|c| c.starts_with("tab- "))
        .collect();
    assert_eq!(closes, ["tab- w1:t1"], "{:?}", ops.calls);
    let last_close = ops
        .calls
        .iter()
        .rposition(|c| c.starts_with("tab- "))
        .unwrap();
    let first_create = ops
        .calls
        .iter()
        .position(|c| c.starts_with("tab+ "))
        .unwrap();
    assert!(last_close < first_create, "{:?}", ops.calls);
    // Both tabs from the fresh snapshot re-materialize.
    assert!(
        ops.calls.iter().any(|c| c.starts_with("tab+ w1:t1")),
        "{:?}",
        ops.calls
    );
    assert!(
        ops.calls.iter().any(|c| c.starts_with("tab+ w1:t2")),
        "{:?}",
        ops.calls
    );

    // The intent noted toward the dead server was forgotten: a herdr-initiated
    // rename of the rebuilt tab applies.
    ops.calls.clear();
    rec.apply_event(
        &EventData::TabRenamed {
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
            label: "renamed".into(),
        },
        &mut ops,
    );
    assert!(
        ops.calls.contains(&"tab~ w1:t1 renamed".to_string()),
        "{:?}",
        ops.calls
    );
}

/// Answers every connection with a pong and counts the pings. The thread
/// parks on accept once the reconnect test stops pinging and leaks until
/// process exit.
fn serve_pongs(
    path: &std::path::Path,
    ping_count: Arc<AtomicUsize>,
) -> std::thread::JoinHandle<()> {
    let listener = UnixListener::bind(path).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            let response = serde_json::json!({
                "id": req["id"],
                "result": {"type":"pong","version":"0.7.4","protocol":17},
            });
            let mut writer = stream;
            writer.write_all(response.to_string().as_bytes()).unwrap();
            writer.write_all(b"\n").unwrap();
            ping_count.fetch_add(1, Ordering::SeqCst);
        }
    })
}

#[test]
fn watchdog_snapshot_probe_reports_unreachable_when_snapshot_fails() {
    let api = HerdrApi::at(PathBuf::from("/nonexistent/herdr.sock"));
    let generation = Arc::new(AtomicU64::new(1));
    let result = warpui::r#async::block_on(watchdog_snapshot_probe(
        api,
        generation,
        1,
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
        Duration::from_millis(1),
    ));
    assert!(result.is_err());
}

#[test]
fn watchdog_snapshot_probe_retires_before_request() {
    let api = HerdrApi::at(PathBuf::from("/nonexistent/herdr.sock"));
    let generation = Arc::new(AtomicU64::new(2));
    let result = warpui::r#async::block_on(watchdog_snapshot_probe(
        api,
        generation,
        1,
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
        Duration::from_millis(1),
    ));
    assert!(result.unwrap().is_none());
}

#[test]
fn watchdog_snapshot_probe_returns_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("herdr.sock");
    let server = serve_scripted(&sock, vec![("session.snapshot", SNAPSHOT_JSON)]);
    let generation = Arc::new(AtomicU64::new(3));
    let snapshot = warpui::r#async::block_on(watchdog_snapshot_probe(
        HerdrApi::at(sock),
        generation,
        3,
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
        Duration::from_millis(1),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(snapshot.snapshot.protocol, 17);
    assert_eq!(snapshot.snapshot.panes.len(), 3);
    assert_eq!(snapshot.pane_label_generation, 0);
    server.join().unwrap();
}

fn watchdog_snapshot_with_label(label: &str, pane_label_generation: u64) -> WatchdogSnapshot {
    let (mut snapshot, _) = snapshot_for(&[("w1:t1", "agents", "w1:p1")]);
    snapshot.panes[0].label = Some(label.into());
    WatchdogSnapshot {
        snapshot,
        pane_label_generation,
        pane_rename_outstanding: false,
    }
}

#[test]
fn watchdog_snapshot_label_reconciliation_skips_newer_local_rename() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    let pane_label_generation = AtomicU64::new(0);
    let stale_snapshot = watchdog_snapshot_with_label("stale", 0);

    note_local_pane_label_mutation(&mut rec, &pane_label_generation, "w1:p1", Some("local"));
    assert_eq!(pane_label_generation.load(Ordering::SeqCst), 1);
    assert!(!reconcile_watchdog_snapshot_labels(
        &mut rec,
        &stale_snapshot,
        pane_label_generation.load(Ordering::SeqCst),
        false,
        &mut ops,
    ));
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);

    let authoritative_snapshot = watchdog_snapshot_with_label("authoritative", 1);
    assert!(reconcile_watchdog_snapshot_labels(
        &mut rec,
        &authoritative_snapshot,
        pane_label_generation.load(Ordering::SeqCst),
        false,
        &mut ops,
    ));
    assert_eq!(ops.calls, vec!["label w1:p1 authoritative"]);
}

#[test]
fn watchdog_snapshot_label_reconciliation_skips_newer_label_event() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    let pane_label_generation = AtomicU64::new(0);
    let stale_snapshot = watchdog_snapshot_with_label("stale", 0);
    let mut pane = snapshot_for(&[("w1:t1", "agents", "w1:p1")])
        .0
        .panes
        .remove(0);
    pane.label = Some("event".into());
    pane.revision = 2;

    apply_stream_event(
        &mut rec,
        &pane_label_generation,
        &EventData::PaneUpdated { pane },
        false,
        &mut ops,
    );
    assert_eq!(pane_label_generation.load(Ordering::SeqCst), 1);
    assert_eq!(ops.calls, vec!["label w1:p1 event"]);
    ops.calls.clear();
    assert!(!reconcile_watchdog_snapshot_labels(
        &mut rec,
        &stale_snapshot,
        pane_label_generation.load(Ordering::SeqCst),
        false,
        &mut ops,
    ));
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);

    let authoritative_snapshot = watchdog_snapshot_with_label("authoritative", 1);
    assert!(reconcile_watchdog_snapshot_labels(
        &mut rec,
        &authoritative_snapshot,
        pane_label_generation.load(Ordering::SeqCst),
        false,
        &mut ops,
    ));
    assert_eq!(ops.calls, vec!["label w1:p1 authoritative"]);
}

#[test]
fn stream_event_does_not_advance_label_generation_for_unrelated_update() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    let pane_label_generation = AtomicU64::new(4);
    let mut pane = snapshot_for(&[("w1:t1", "agents", "w1:p1")])
        .0
        .panes
        .remove(0);
    pane.revision = 2;
    pane.terminal_title = Some("vim".into());

    apply_stream_event(
        &mut rec,
        &pane_label_generation,
        &EventData::PaneUpdated { pane },
        false,
        &mut ops,
    );

    assert_eq!(pane_label_generation.load(Ordering::SeqCst), 4);
    assert_eq!(ops.calls, vec!["title w1:p1 agents - vim"]);
}

#[test]
fn watchdog_snapshot_label_reconciliation_skips_rename_outstanding_at_capture() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    let pane_label_generation = AtomicU64::new(0);
    note_local_pane_label_mutation(&mut rec, &pane_label_generation, "w1:p1", Some("local"));
    let pane_rename_outstanding = Arc::new(AtomicU64::new(0));
    let rename = OutstandingPaneRename::new(pane_rename_outstanding.clone());
    let mut stale_snapshot = watchdog_snapshot_with_label("stale", 1);
    stale_snapshot.pane_rename_outstanding = true;

    assert!(!reconcile_watchdog_snapshot_labels(
        &mut rec,
        &stale_snapshot,
        pane_label_generation.load(Ordering::SeqCst),
        pane_rename_outstanding.load(Ordering::SeqCst) != 0,
        &mut ops,
    ));
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);

    drop(rename);
    let authoritative_snapshot = watchdog_snapshot_with_label("authoritative", 1);
    assert!(reconcile_watchdog_snapshot_labels(
        &mut rec,
        &authoritative_snapshot,
        pane_label_generation.load(Ordering::SeqCst),
        pane_rename_outstanding.load(Ordering::SeqCst) != 0,
        &mut ops,
    ));
    assert_eq!(ops.calls, vec!["label w1:p1 authoritative"]);
}

#[test]
fn outstanding_pane_rename_decrements_after_success_and_failure() {
    let pane_rename_outstanding = Arc::new(AtomicU64::new(0));
    for result in [Ok(()), Err(anyhow::anyhow!("rename failed"))] {
        let rename = OutstandingPaneRename::new(pane_rename_outstanding.clone());
        assert_eq!(pane_rename_outstanding.load(Ordering::SeqCst), 1);
        let run = move || {
            let _rename = rename;
            result
        };
        let _ = run();
        assert_eq!(pane_rename_outstanding.load(Ordering::SeqCst), 0);
    }
}

fn rename_request(pane_id: &str, label: Option<&str>) -> PaneRenameRequest {
    PaneRenameRequest {
        pane_id: pane_id.into(),
        label: label.map(str::to_string),
    }
}

fn pane_response(pane_id: &str, label: Option<&str>, revision: u64) -> PaneInfo {
    let mut pane = snapshot_for(&[("w1:t1", "agents", pane_id)])
        .0
        .panes
        .remove(0);
    pane.label = label.map(str::to_string);
    pane.revision = revision;
    pane
}

#[test]
fn pane_rename_queue_serializes_and_coalesces_latest_set() {
    let mut queue = PaneRenameQueue::default();
    assert_eq!(
        queue.enqueue(rename_request("w1:p1", Some("a")), 7),
        Some(rename_request("w1:p1", Some("a")))
    );
    assert_eq!(queue.enqueue(rename_request("w1:p1", Some("b")), 7), None);

    assert_eq!(
        complete_pane_rename(&mut queue, "w1:p1", 7, Ok(())),
        PaneRenameCompletion::Next {
            request: rename_request("w1:p1", Some("b")),
            generation: 7,
        }
    );
    assert_eq!(
        complete_pane_rename(&mut queue, "w1:p1", 7, Ok(())),
        PaneRenameCompletion::Done
    );
}

#[test]
fn pane_rename_queue_coalesces_set_to_clear() {
    let mut queue = PaneRenameQueue::default();
    assert_eq!(
        queue.enqueue(rename_request("w1:p1", Some("a")), 7),
        Some(rename_request("w1:p1", Some("a")))
    );
    assert_eq!(queue.enqueue(rename_request("w1:p1", None), 7), None);

    assert_eq!(
        complete_pane_rename(&mut queue, "w1:p1", 7, Ok(())),
        PaneRenameCompletion::Next {
            request: rename_request("w1:p1", None),
            generation: 7,
        }
    );
    assert_eq!(
        complete_pane_rename(&mut queue, "w1:p1", 7, Ok(())),
        PaneRenameCompletion::Done
    );
}

#[test]
fn pane_rename_queue_advances_latest_after_error() {
    let mut queue = PaneRenameQueue::default();
    assert!(
        queue
            .enqueue(rename_request("w1:p1", Some("a")), 7)
            .is_some()
    );
    assert_eq!(queue.enqueue(rename_request("w1:p1", Some("b")), 7), None);

    assert_eq!(
        complete_pane_rename(
            &mut queue,
            "w1:p1",
            7,
            Err(anyhow::anyhow!("first rename failed")),
        ),
        PaneRenameCompletion::Next {
            request: rename_request("w1:p1", Some("b")),
            generation: 7,
        }
    );
}

#[test]
fn cross_generation_pane_rename_waits_for_active_request() {
    let mut queue = PaneRenameQueue::default();
    let pane_rename_outstanding = Arc::new(AtomicU64::new(0));
    let rename_guard = OutstandingPaneRename::new(pane_rename_outstanding.clone());
    let mut started = vec![
        queue
            .enqueue(rename_request("w1:p1", Some("old")), 7)
            .expect("old-generation request starts"),
    ];
    assert_eq!(pane_rename_outstanding.load(Ordering::SeqCst), 1);
    assert_eq!(
        queue.enqueue(rename_request("w1:p1", Some("new")), 9),
        None,
        "new generation must not overlap the active old-generation request"
    );
    assert_eq!(pane_rename_outstanding.load(Ordering::SeqCst), 1);

    let PaneRenameCompletion::Next {
        request,
        generation,
    } = complete_pane_rename(&mut queue, "w1:p1", 7, Ok(()))
    else {
        panic!("old-generation completion must promote the queued request");
    };
    assert_eq!(generation, 9);
    started.push(request);
    assert_eq!(pane_rename_outstanding.load(Ordering::SeqCst), 1);
    assert_eq!(
        complete_pane_rename(&mut queue, "w1:p1", 9, Ok(())),
        PaneRenameCompletion::Done
    );
    assert_eq!(
        started,
        vec![
            rename_request("w1:p1", Some("old")),
            rename_request("w1:p1", Some("new")),
        ],
        "writes must remain ordered across watchdog generations"
    );
    assert_eq!(pane_rename_outstanding.load(Ordering::SeqCst), 1);
    drop(rename_guard);
    assert_eq!(pane_rename_outstanding.load(Ordering::SeqCst), 0);
}

#[test]
fn pane_rename_queue_cancels_only_the_active_generation() {
    let mut queue = PaneRenameQueue::default();
    assert!(
        queue
            .enqueue(rename_request("w1:p1", Some("old")), 7)
            .is_some()
    );
    assert_eq!(queue.enqueue(rename_request("w1:p1", Some("new")), 9), None);

    queue.cancel("w1:p1", 9);
    assert!(queue.contains_pane("w1:p1"));
    assert_eq!(
        complete_pane_rename(&mut queue, "w1:p1", 7, Ok(())),
        PaneRenameCompletion::Next {
            request: rename_request("w1:p1", Some("new")),
            generation: 9,
        }
    );
    assert_eq!(
        complete_pane_rename(&mut queue, "w1:p1", 7, Ok(())),
        PaneRenameCompletion::Stale
    );
    queue.cancel("w1:p1", 7);
    assert!(queue.contains_pane("w1:p1"));
    queue.cancel("w1:p1", 9);
    assert!(!queue.contains_pane("w1:p1"));
}

#[test]
fn stale_or_detached_promoted_pane_rename_cancels_the_chain() {
    for (current_generation, detached) in [(10, false), (9, true)] {
        let mut queue = PaneRenameQueue::default();
        assert!(
            queue
                .enqueue(rename_request("w1:p1", Some("old")), 7)
                .is_some()
        );
        assert_eq!(queue.enqueue(rename_request("w1:p1", Some("new")), 9), None);
        let completion = complete_pane_rename(&mut queue, "w1:p1", 7, Ok(()));

        assert_eq!(
            pane_rename_to_schedule(
                &mut queue,
                "w1:p1",
                completion,
                current_generation,
                detached,
            ),
            None
        );
        assert!(!queue.contains_pane("w1:p1"));
    }
}

#[test]
fn pane_rename_queue_keeps_different_pane_ids_independent() {
    let mut queue = PaneRenameQueue::default();
    assert_eq!(
        queue.enqueue(rename_request("w1:p1", Some("a")), 7),
        Some(rename_request("w1:p1", Some("a")))
    );
    assert_eq!(
        queue.enqueue(rename_request("w1:p2", Some("x")), 7),
        Some(rename_request("w1:p2", Some("x")))
    );
    assert_eq!(queue.enqueue(rename_request("w1:p1", Some("b")), 7), None);

    assert_eq!(
        complete_pane_rename(&mut queue, "w1:p2", 7, Ok(())),
        PaneRenameCompletion::Done
    );
    assert_eq!(
        complete_pane_rename(&mut queue, "w1:p1", 7, Ok(())),
        PaneRenameCompletion::Next {
            request: rename_request("w1:p1", Some("b")),
            generation: 7,
        }
    );
}

#[test]
fn stream_event_during_pane_rename_preserves_newest_local_label() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    let pane_label_generation = AtomicU64::new(0);
    let mut queue = PaneRenameQueue::default();
    assert!(
        queue
            .enqueue(rename_request("w1:p1", Some("a")), 7)
            .is_some()
    );
    assert_eq!(queue.enqueue(rename_request("w1:p1", Some("b")), 7), None);
    note_local_pane_label_mutation(&mut rec, &pane_label_generation, "w1:p1", Some("b"));

    let mut event_pane = snapshot_for(&[("w1:t1", "agents", "w1:p1")])
        .0
        .panes
        .remove(0);
    event_pane.label = Some("a".into());
    event_pane.terminal_title = Some("vim".into());
    event_pane.agent_status = AgentStatus::Working;
    event_pane.revision = 2;
    apply_stream_event(
        &mut rec,
        &pane_label_generation,
        &EventData::PaneUpdated { pane: event_pane },
        queue.contains_pane("w1:p1"),
        &mut ops,
    );

    assert_eq!(pane_label_generation.load(Ordering::SeqCst), 1);
    assert_eq!(
        ops.calls,
        vec![
            "title w1:p1 agents - vim",
            "pane_status w1:p1 Working",
            "status w1:t1 Working",
        ]
    );

    assert!(matches!(
        complete_pane_rename(&mut queue, "w1:p1", 7, Ok(())),
        PaneRenameCompletion::Next { .. }
    ));
    assert_eq!(
        complete_pane_rename(&mut queue, "w1:p1", 7, Ok(())),
        PaneRenameCompletion::Done
    );
    ops.calls.clear();
    let mut equal_revision_pane = snapshot_for(&[("w1:t1", "agents", "w1:p1")])
        .0
        .panes
        .remove(0);
    equal_revision_pane.label = Some("peer-equal".into());
    equal_revision_pane.terminal_title = Some("vim".into());
    equal_revision_pane.agent_status = AgentStatus::Working;
    equal_revision_pane.revision = 2;
    apply_stream_event(
        &mut rec,
        &pane_label_generation,
        &EventData::PaneUpdated {
            pane: equal_revision_pane,
        },
        queue.contains_pane("w1:p1"),
        &mut ops,
    );
    assert_eq!(pane_label_generation.load(Ordering::SeqCst), 1);
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);

    let mut peer_pane = snapshot_for(&[("w1:t1", "agents", "w1:p1")])
        .0
        .panes
        .remove(0);
    peer_pane.label = Some("peer".into());
    peer_pane.terminal_title = Some("vim".into());
    peer_pane.agent_status = AgentStatus::Working;
    peer_pane.revision = 3;
    apply_stream_event(
        &mut rec,
        &pane_label_generation,
        &EventData::PaneUpdated { pane: peer_pane },
        queue.contains_pane("w1:p1"),
        &mut ops,
    );
    assert_eq!(pane_label_generation.load(Ordering::SeqCst), 2);
    assert_eq!(ops.calls, vec!["label w1:p1 peer"]);
}

#[test]
fn pane_rename_response_fences_delayed_equal_revision_event() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    let pane_label_generation = AtomicU64::new(0);
    let mut queue = PaneRenameQueue::default();
    note_local_pane_label_mutation(&mut rec, &pane_label_generation, "w1:p1", Some("b"));
    assert!(
        queue
            .enqueue(rename_request("w1:p1", Some("b")), 7)
            .is_some()
    );

    let response_pane = pane_response("w1:p1", Some("b"), 2);
    assert_eq!(
        complete_pane_rename_response(&mut rec, &mut queue, "w1:p1", 7, true, Ok(response_pane)),
        PaneRenameCompletion::Done
    );

    let delayed_pane = pane_response("w1:p1", Some("a"), 2);
    apply_stream_event(
        &mut rec,
        &pane_label_generation,
        &EventData::PaneUpdated { pane: delayed_pane },
        queue.contains_pane("w1:p1"),
        &mut ops,
    );

    assert_eq!(pane_label_generation.load(Ordering::SeqCst), 1);
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);

    let peer_pane = pane_response("w1:p1", Some("peer"), 3);
    apply_stream_event(
        &mut rec,
        &pane_label_generation,
        &EventData::PaneUpdated { pane: peer_pane },
        queue.contains_pane("w1:p1"),
        &mut ops,
    );
    assert_eq!(pane_label_generation.load(Ordering::SeqCst), 2);
    assert_eq!(ops.calls, vec!["label w1:p1 peer"]);
}

#[test]
fn delayed_event_before_pane_rename_response_preserves_local_label() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    rec.note_local_pane_label("w1:p1", Some("a"));
    let pane_label_generation = AtomicU64::new(0);
    let mut queue = PaneRenameQueue::default();
    note_local_pane_label_mutation(&mut rec, &pane_label_generation, "w1:p1", Some("b"));
    assert!(
        queue
            .enqueue(rename_request("w1:p1", Some("b")), 7)
            .is_some()
    );

    let delayed_pane = pane_response("w1:p1", Some("a"), 2);
    apply_stream_event(
        &mut rec,
        &pane_label_generation,
        &EventData::PaneUpdated { pane: delayed_pane },
        queue.contains_pane("w1:p1"),
        &mut ops,
    );
    assert_eq!(pane_label_generation.load(Ordering::SeqCst), 1);
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);

    let response_pane = pane_response("w1:p1", Some("b"), 2);
    assert_eq!(
        complete_pane_rename_response(&mut rec, &mut queue, "w1:p1", 7, true, Ok(response_pane)),
        PaneRenameCompletion::Done
    );
    assert_eq!(pane_label_generation.load(Ordering::SeqCst), 1);
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
}

#[test]
fn intermediate_pane_rename_response_advances_revision_without_regressing_latest_label() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    rec.note_local_pane_label("w1:p1", Some("a"));
    let pane_label_generation = AtomicU64::new(0);
    let mut queue = PaneRenameQueue::default();
    assert!(
        queue
            .enqueue(rename_request("w1:p1", Some("a")), 7)
            .is_some()
    );
    note_local_pane_label_mutation(&mut rec, &pane_label_generation, "w1:p1", Some("b"));
    assert_eq!(queue.enqueue(rename_request("w1:p1", Some("b")), 7), None);

    let response_pane = pane_response("w1:p1", Some("a"), 2);
    assert_eq!(
        complete_pane_rename_response(&mut rec, &mut queue, "w1:p1", 7, true, Ok(response_pane)),
        PaneRenameCompletion::Next {
            request: rename_request("w1:p1", Some("b")),
            generation: 7,
        }
    );
    assert!(!rec.event_changes_pane_label(&EventData::PaneUpdated {
        pane: pane_response("w1:p1", Some("b"), 3),
    }));

    queue.cancel("w1:p1", 7);
    let delayed_pane = pane_response("w1:p1", Some("a"), 2);
    apply_stream_event(
        &mut rec,
        &pane_label_generation,
        &EventData::PaneUpdated { pane: delayed_pane },
        queue.contains_pane("w1:p1"),
        &mut ops,
    );
    assert_eq!(pane_label_generation.load(Ordering::SeqCst), 1);
    assert!(ops.calls.is_empty(), "{:?}", ops.calls);
}

#[test]
fn mismatched_pane_rename_response_does_not_advance_another_pane_revision() {
    let mut rec = Reconciler::default();
    let mut ops = RecordingOps::default();
    let (snapshot, layouts) =
        snapshot_for(&[("w1:t1", "agents", "w1:p1"), ("w1:t2", "logs", "w1:p2")]);
    rec.apply_snapshot(&snapshot, &layouts, &mut ops);
    ops.calls.clear();
    let pane_label_generation = AtomicU64::new(0);
    let mut queue = PaneRenameQueue::default();
    assert!(
        queue
            .enqueue(rename_request("w1:p1", Some("local")), 7)
            .is_some()
    );

    let mismatched_response = pane_response("w1:p2", Some("wrong"), 99);
    assert_eq!(
        complete_pane_rename_response(
            &mut rec,
            &mut queue,
            "w1:p1",
            7,
            true,
            Ok(mismatched_response),
        ),
        PaneRenameCompletion::Done
    );
    let peer_pane = pane_response("w1:p2", Some("peer"), 2);
    apply_stream_event(
        &mut rec,
        &pane_label_generation,
        &EventData::PaneUpdated { pane: peer_pane },
        false,
        &mut ops,
    );

    assert_eq!(pane_label_generation.load(Ordering::SeqCst), 1);
    assert_eq!(ops.calls, vec!["label w1:p2 peer"]);
}

#[test]
fn failed_pane_rename_response_provides_no_revision_fence() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    let pane_label_generation = AtomicU64::new(0);
    let mut queue = PaneRenameQueue::default();
    note_local_pane_label_mutation(&mut rec, &pane_label_generation, "w1:p1", Some("b"));
    assert!(
        queue
            .enqueue(rename_request("w1:p1", Some("b")), 7)
            .is_some()
    );

    assert_eq!(
        complete_pane_rename_response(
            &mut rec,
            &mut queue,
            "w1:p1",
            7,
            true,
            Err(anyhow::anyhow!("rename failed")),
        ),
        PaneRenameCompletion::Done
    );
    let peer_pane = pane_response("w1:p1", Some("peer"), 2);
    apply_stream_event(
        &mut rec,
        &pane_label_generation,
        &EventData::PaneUpdated { pane: peer_pane },
        queue.contains_pane("w1:p1"),
        &mut ops,
    );

    assert_eq!(pane_label_generation.load(Ordering::SeqCst), 2);
    assert_eq!(ops.calls, vec!["label w1:p1 peer"]);
}

#[test]
fn stale_generation_pane_rename_response_does_not_fence_rebuilt_pane() {
    // The queue survives a reconnect while the reconciler is rebuilt from a
    // fresh low-revision snapshot for the same pane id.
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    rec.note_local_pane_label("w1:p1", Some("current"));
    let pane_label_generation = AtomicU64::new(0);
    let mut queue = PaneRenameQueue::default();
    assert!(
        queue
            .enqueue(rename_request("w1:p1", Some("old")), 7)
            .is_some()
    );
    assert_eq!(
        queue.enqueue(rename_request("w1:p1", Some("current")), 9),
        None
    );

    let current_generation = 9;
    let stale_response = pane_response("w1:p1", Some("old"), 99);
    assert_eq!(
        complete_pane_rename_response(
            &mut rec,
            &mut queue,
            "w1:p1",
            7,
            7 == current_generation,
            Ok(stale_response),
        ),
        PaneRenameCompletion::Next {
            request: rename_request("w1:p1", Some("current")),
            generation: 9,
        }
    );
    queue.cancel("w1:p1", 9);

    let current_event = pane_response("w1:p1", Some("peer"), 2);
    apply_stream_event(
        &mut rec,
        &pane_label_generation,
        &EventData::PaneUpdated {
            pane: current_event,
        },
        queue.contains_pane("w1:p1"),
        &mut ops,
    );

    assert_eq!(pane_label_generation.load(Ordering::SeqCst), 1);
    assert_eq!(ops.calls, vec!["label w1:p1 peer"]);
}

#[test]
fn detached_pane_rename_response_does_not_fence_pane() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    rec.note_local_pane_label("w1:p1", Some("local"));
    let pane_label_generation = AtomicU64::new(0);
    let mut queue = PaneRenameQueue::default();
    assert!(
        queue
            .enqueue(rename_request("w1:p1", Some("local")), 7)
            .is_some()
    );

    let detached = true;
    let detached_response = pane_response("w1:p1", Some("local"), 99);
    assert_eq!(
        complete_pane_rename_response(
            &mut rec,
            &mut queue,
            "w1:p1",
            7,
            !detached,
            Ok(detached_response),
        ),
        PaneRenameCompletion::Done
    );
    let current_event = pane_response("w1:p1", Some("peer"), 2);
    apply_stream_event(
        &mut rec,
        &pane_label_generation,
        &EventData::PaneUpdated {
            pane: current_event,
        },
        queue.contains_pane("w1:p1"),
        &mut ops,
    );

    assert_eq!(pane_label_generation.load(Ordering::SeqCst), 1);
    assert_eq!(ops.calls, vec!["label w1:p1 peer"]);
}

#[test]
fn queue_stale_pane_rename_response_does_not_fence_pane() {
    let (mut rec, mut ops) = seeded();
    ops.calls.clear();
    rec.note_local_pane_label("w1:p1", Some("current"));
    let pane_label_generation = AtomicU64::new(0);
    let mut queue = PaneRenameQueue::default();
    assert!(
        queue
            .enqueue(rename_request("w1:p1", Some("current")), 9)
            .is_some()
    );

    let stale_response = pane_response("w1:p1", Some("old"), 99);
    assert_eq!(
        complete_pane_rename_response(&mut rec, &mut queue, "w1:p1", 7, true, Ok(stale_response),),
        PaneRenameCompletion::Stale
    );
    queue.cancel("w1:p1", 9);
    let current_event = pane_response("w1:p1", Some("peer"), 2);
    apply_stream_event(
        &mut rec,
        &pane_label_generation,
        &EventData::PaneUpdated {
            pane: current_event,
        },
        queue.contains_pane("w1:p1"),
        &mut ops,
    );

    assert_eq!(pane_label_generation.load(Ordering::SeqCst), 1);
    assert_eq!(ops.calls, vec!["label w1:p1 peer"]);
}

/// Detaching during an outage stops the reconnect loop — it used to ping the
/// dead socket every 30s for the app's lifetime.
#[test]
fn reconnect_loop_exits_on_detach() {
    let api = HerdrApi::at(PathBuf::from("/nonexistent/herdr.sock"));
    let detached = Arc::new(AtomicBool::new(false));
    let flag = detached.clone();
    let handle = std::thread::spawn(move || {
        warpui::r#async::block_on(reconnect_loop(
            api,
            flag,
            vec![Duration::from_millis(1)],
            Duration::from_millis(5),
        ))
    });
    std::thread::sleep(Duration::from_millis(20));
    detached.store(true, Ordering::SeqCst);
    assert!(
        !handle.join().unwrap(),
        "loop must report detached, not reachable"
    );
}

/// A server answering pings resolves the loop reachable (the callback then
/// resyncs).
#[test]
fn reconnect_loop_reports_reachable() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("herdr.sock");
    let ping_count = Arc::new(AtomicUsize::new(0));
    let _server = serve_pongs(&sock, ping_count.clone());
    let reachable = warpui::r#async::block_on(reconnect_loop(
        HerdrApi::at(sock),
        Arc::new(AtomicBool::new(false)),
        vec![Duration::from_millis(1)],
        Duration::from_millis(5),
    ));
    assert!(reachable);
}

enum BlockedSnapshotResponse {
    Snapshot,
    Drop,
}

/// Accepts one snapshot request and holds it unanswered until `release`
/// fires, then either answers or drops the socket. The returned receiver
/// reports that the request has been read so tests can disarm deterministically
/// while the RPC is in flight.
fn serve_one_blocked_snapshot(
    path: &std::path::Path,
    release: std::sync::mpsc::Receiver<()>,
    response: BlockedSnapshotResponse,
) -> (std::thread::JoinHandle<()>, std::sync::mpsc::Receiver<()>) {
    let listener = UnixListener::bind(path).unwrap();
    let (request_received_tx, request_received_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(request["method"], "session.snapshot");
        request_received_tx.send(()).unwrap();
        release.recv().unwrap();
        match response {
            BlockedSnapshotResponse::Snapshot => {
                let response = serde_json::json!({
                    "id": request["id"],
                    "result": serde_json::from_str::<serde_json::Value>(SNAPSHOT_JSON).unwrap(),
                });
                let mut writer = stream;
                writer.write_all(response.to_string().as_bytes()).unwrap();
                writer.write_all(b"\n").unwrap();
            }
            BlockedSnapshotResponse::Drop => drop(stream),
        }
    });
    (server, request_received_rx)
}

#[test]
fn watchdog_snapshot_probe_retires_after_in_flight_success() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("herdr.sock");
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (server, request_received_rx) =
        serve_one_blocked_snapshot(&sock, release_rx, BlockedSnapshotResponse::Snapshot);

    let generation = Arc::new(AtomicU64::new(7));
    let probe_generation = generation.clone();
    let handle = std::thread::spawn(move || {
        warpui::r#async::block_on(watchdog_snapshot_probe(
            HerdrApi::at(sock),
            probe_generation,
            7,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Duration::from_millis(1),
        ))
    });
    request_received_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    generation.fetch_add(1, Ordering::SeqCst);
    release_tx.send(()).unwrap();
    assert!(handle.join().unwrap().unwrap().is_none());
    server.join().unwrap();
}

#[test]
fn watchdog_snapshot_probe_captures_label_state_before_request() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("herdr.sock");
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (server, request_received_rx) =
        serve_one_blocked_snapshot(&sock, release_rx, BlockedSnapshotResponse::Snapshot);

    let pane_label_generation = Arc::new(AtomicU64::new(4));
    let probe_label_generation = pane_label_generation.clone();
    let pane_rename_outstanding = Arc::new(AtomicU64::new(1));
    let probe_rename_outstanding = pane_rename_outstanding.clone();
    let handle = std::thread::spawn(move || {
        warpui::r#async::block_on(watchdog_snapshot_probe(
            HerdrApi::at(sock),
            Arc::new(AtomicU64::new(7)),
            7,
            probe_label_generation,
            probe_rename_outstanding,
            Duration::from_millis(1),
        ))
    });
    request_received_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    pane_label_generation.fetch_add(1, Ordering::SeqCst);
    pane_rename_outstanding.fetch_sub(1, Ordering::SeqCst);
    release_tx.send(()).unwrap();

    let snapshot = handle.join().unwrap().unwrap().unwrap();
    assert_eq!(snapshot.pane_label_generation, 4);
    assert!(snapshot.pane_rename_outstanding);
    server.join().unwrap();
}

#[test]
fn watchdog_snapshot_probe_retires_after_in_flight_failure() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("herdr.sock");
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (server, request_received_rx) =
        serve_one_blocked_snapshot(&sock, release_rx, BlockedSnapshotResponse::Drop);

    let generation = Arc::new(AtomicU64::new(7));
    let probe_generation = generation.clone();
    let handle = std::thread::spawn(move || {
        warpui::r#async::block_on(watchdog_snapshot_probe(
            HerdrApi::at(sock),
            probe_generation,
            7,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Duration::from_millis(1),
        ))
    });
    request_received_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    generation.fetch_add(1, Ordering::SeqCst);
    release_tx.send(()).unwrap();
    assert!(handle.join().unwrap().unwrap().is_none());
    server.join().unwrap();
}
