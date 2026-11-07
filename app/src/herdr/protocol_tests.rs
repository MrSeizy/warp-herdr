use warpui::event::ModifiersState;

use super::*;
use crate::terminal::model::index::Point;

#[test]
fn pane_info_deserializes_label_when_present() {
    let pane: PaneInfo = serde_json::from_str(
        r#"{"pane_id":"w1:p1","terminal_id":"term_1","workspace_id":"w1","tab_id":"w1:t1","focused":true,"agent_status":"idle","revision":1,"label":"reviewer"}"#,
    )
    .unwrap();

    assert_eq!(pane.label.as_deref(), Some("reviewer"));
}

#[test]
fn pane_info_missing_label_defaults_to_none() {
    let pane: PaneInfo = serde_json::from_str(
        r#"{"pane_id":"w1:p1","terminal_id":"term_1","workspace_id":"w1","tab_id":"w1:t1","focused":true,"agent_status":"idle","revision":1}"#,
    )
    .unwrap();

    assert_eq!(pane.label, None);
}

#[test]
fn deserializes_pong() {
    let line = r#"{"id":"r1","result":{"type":"pong","version":"0.7.4","protocol":17,"capabilities":{"live_handoff":true}}}"#;
    let resp: ApiResponse = serde_json::from_str(line).unwrap();
    match resp {
        ApiResponse::Success { id, result } => match *result {
            ResponseResult::Pong(p) => {
                assert_eq!(id, "r1");
                assert_eq!(p.version, "0.7.4");
                assert_eq!(p.protocol, 17);
            }
            other => panic!("unexpected: {other:?}"),
        },
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn deserializes_error() {
    let line = r#"{"id":"r2","error":{"code":"pane_not_found","message":"nope"}}"#;
    let resp: ApiResponse = serde_json::from_str(line).unwrap();
    match resp {
        ApiResponse::Error { id, error } => {
            assert_eq!(id, "r2");
            assert_eq!(error.code, "pane_not_found");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn serializes_request_with_string_id_and_flat_method() {
    let req = ApiRequest {
        id: "req_7".into(),
        method: "tab.rename".into(),
        params: serde_json::json!({"tab_id": "w1:t2", "label": "build"}),
    };
    let line = serde_json::to_string(&req).unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["id"], "req_7");
    assert_eq!(v["method"], "tab.rename");
    assert_eq!(v["params"]["label"], "build");
}

#[test]
fn deserializes_snapshot_core_fields() {
    let line = r#"{"id":"r3","result":{"type":"session_snapshot","snapshot":{
        "version":"0.7.4","protocol":17,
        "focused_workspace_id":"w1","focused_tab_id":"w1:t1","focused_pane_id":"w1:p1",
        "workspaces":[{"workspace_id":"w1","number":1,"label":"proj","focused":true,
            "pane_count":2,"tab_count":1,"active_tab_id":"w1:t1","agent_status":"working"}],
        "tabs":[{"tab_id":"w1:t1","workspace_id":"w1","number":1,"label":"agents",
            "focused":true,"pane_count":2,"agent_status":"blocked"}],
        "panes":[{"pane_id":"w1:p1","terminal_id":"term_ab","workspace_id":"w1","tab_id":"w1:t1",
            "focused":true,"terminal_title":"claude","agent_status":"working","revision":3},
           {"pane_id":"w1:p2","terminal_id":"term_cd","workspace_id":"w1","tab_id":"w1:t1",
            "focused":false,"agent_status":"idle","revision":1}],
        "layouts":[],"agents":[]}}}"#;
    let resp: ApiResponse = serde_json::from_str(line).unwrap();
    let snap = match resp {
        ApiResponse::Success { result, .. } => match *result {
            ResponseResult::SessionSnapshot { snapshot } => snapshot,
            other => panic!("unexpected: {other:?}"),
        },
        other => panic!("unexpected: {other:?}"),
    };
    assert_eq!(snap.tabs[0].agent_status, AgentStatus::Blocked);
    assert_eq!(snap.panes.len(), 2);
    assert_eq!(snap.panes[0].terminal_title.as_deref(), Some("claude"));
}

#[test]
fn deserializes_lifecycle_event_snake_case() {
    let line = r#"{"event":"tab_renamed","data":{"type":"tab_renamed","tab_id":"w1:t1","workspace_id":"w1","label":"new name"}}"#;
    let ev: EventEnvelope = serde_json::from_str(line).unwrap();
    match ev.data {
        EventData::TabRenamed { tab_id, label, .. } => {
            assert_eq!(tab_id, "w1:t1");
            assert_eq!(label, "new name");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn deserializes_layout_updated_lifecycle_event_with_tab_id() {
    let line = r#"{"event":"layout_updated","data":{"type":"layout_updated","tab_id":"w1:t2"}}"#;
    let ev: EventEnvelope = serde_json::from_str(line).unwrap();
    match ev.data {
        EventData::LayoutUpdated { tab_id } => assert_eq!(tab_id, "w1:t2"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn deserializes_agent_status_subscription_event_untagged() {
    let line = r#"{"event":"pane.agent_status_changed","data":{"pane_id":"w1:p1","workspace_id":"w1","agent_status":"done"}}"#;
    let ev: AnyEvent = serde_json::from_str(line).unwrap();
    match ev {
        AnyEvent::AgentStatus(e) => assert_eq!(e.agent_status, AgentStatus::Done),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn any_event_also_parses_lifecycle() {
    let line = r#"{"event":"pane_closed","data":{"type":"pane_closed","pane_id":"w1:p3","workspace_id":"w1"}}"#;
    let ev: AnyEvent = serde_json::from_str(line).unwrap();
    assert!(matches!(
        ev,
        AnyEvent::Lifecycle(EventEnvelope {
            data: EventData::PaneClosed { .. },
            ..
        })
    ));
}

#[test]
fn deserializes_pane_agent_detected_with_and_without_agent() {
    // Agent started: `agent` present.
    let line = r#"{"event":"pane_agent_detected","data":{"type":"pane_agent_detected","pane_id":"w1:p1","workspace_id":"w1","agent":"claude"}}"#;
    let ev: AnyEvent = serde_json::from_str(line).unwrap();
    match ev {
        AnyEvent::Lifecycle(EventEnvelope {
            data: EventData::PaneAgentDetected { pane_id, agent, .. },
            ..
        }) => {
            assert_eq!(pane_id, "w1:p1");
            assert_eq!(agent.as_deref(), Some("claude"));
        }
        other => panic!("unexpected: {other:?}"),
    }
    // Agent exited: herdr omits `agent` entirely.
    let line = r#"{"event":"pane_agent_detected","data":{"type":"pane_agent_detected","pane_id":"w1:p1","workspace_id":"w1"}}"#;
    let ev: AnyEvent = serde_json::from_str(line).unwrap();
    match ev {
        AnyEvent::Lifecycle(EventEnvelope {
            data: EventData::PaneAgentDetected { agent, .. },
            ..
        }) => assert_eq!(agent, None),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn deserializes_layout_node() {
    let json = r#"{"type":"split","direction":"right","ratio":0.65,
        "first":{"type":"pane","pane_id":"w1:p1"},
        "second":{"type":"split","direction":"down","ratio":0.5,
            "first":{"type":"pane","pane_id":"w1:p2"},
            "second":{"type":"pane","pane_id":"w1:p3"}}}"#;
    let node: LayoutNode = serde_json::from_str(json).unwrap();
    match node {
        LayoutNode::Split {
            direction,
            ratio,
            first,
            ..
        } => {
            assert_eq!(direction, HerdrSplitDirection::Right);
            assert!((ratio - 0.65).abs() < f32::EPSILON);
            assert!(matches!(*first, LayoutNode::Pane { .. }));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn parses_stream_records() {
    let frame = r#"{"type":"terminal.frame","seq":9,"encoding":"ansi","width":120,"height":40,"full":true,"bytes":"aGVsbG8="}"#;
    let rec: StreamRecord = serde_json::from_str(frame).unwrap();
    match rec {
        StreamRecord::Frame(f) => {
            assert_eq!(f.seq, 9);
            assert_eq!(f.decoded().unwrap(), b"hello");
        }
        other => panic!("unexpected: {other:?}"),
    }
    let closed = r#"{"type":"terminal.closed","reason":"terminal attach taken over"}"#;
    let rec: StreamRecord = serde_json::from_str(closed).unwrap();
    assert!(
        matches!(rec, StreamRecord::Closed { reason: Some(r) } if r == "terminal attach taken over")
    );
}

#[test]
fn serializes_control_commands() {
    assert_eq!(
        serde_json::to_string(&ControlCommand::input_bytes(b"ls\r")).unwrap(),
        r#"{"type":"terminal.input","bytes":"bHMN"}"#
    );
    assert_eq!(
        serde_json::to_string(&ControlCommand::Input(Input::text("ls".into()))).unwrap(),
        r#"{"type":"terminal.input","text":"ls"}"#
    );
    assert_eq!(
        serde_json::to_string(&ControlCommand::Resize {
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0
        })
        .unwrap(),
        r#"{"type":"terminal.resize","cols":80,"rows":24,"cell_width_px":0,"cell_height_px":0}"#
    );
    assert_eq!(
        serde_json::to_string(&ControlCommand::Release).unwrap(),
        r#"{"type":"terminal.release"}"#
    );
}

#[test]
fn herdr_mouse_commands_preserve_cells_and_modifiers() {
    let state = MouseState::new(
        MouseButton::Right,
        MouseAction::Pressed,
        ModifiersState {
            ctrl: true,
            alt: true,
            ..Default::default()
        },
    )
    .set_point(Point::new(0, 65535));
    assert_eq!(
        serde_json::to_value(ControlCommand::mouse(&state).unwrap()).unwrap(),
        serde_json::json!({"type":"terminal.mouse","action":"down","button":"right","column":65535,"row":0,"modifiers":6})
    );
}

#[test]
fn herdr_mouse_commands_encode_release_drag_and_move() {
    let release = MouseState::new(MouseButton::Left, MouseAction::Released, Default::default())
        .set_point(Point::new(1, 2));
    let drag = MouseState::new(
        MouseButton::LeftDrag,
        MouseAction::Pressed,
        Default::default(),
    )
    .set_point(Point::new(3, 4));
    let motion = MouseState::new(MouseButton::Move, MouseAction::Pressed, Default::default())
        .set_point(Point::new(5, 6));
    assert_eq!(
        serde_json::to_value(ControlCommand::mouse(&release).unwrap()).unwrap(),
        serde_json::json!({"type":"terminal.mouse","action":"up","button":"left","column":2,"row":1,"modifiers":0})
    );
    assert_eq!(
        serde_json::to_value(ControlCommand::mouse(&drag).unwrap()).unwrap(),
        serde_json::json!({"type":"terminal.mouse","action":"drag","button":"left","column":4,"row":3,"modifiers":0})
    );
    assert_eq!(
        serde_json::to_value(ControlCommand::mouse(&motion).unwrap()).unwrap(),
        serde_json::json!({"type":"terminal.mouse","action":"move","button":"left","column":6,"row":5,"modifiers":0})
    );
}

#[test]
fn herdr_mouse_rejects_missing_or_unrepresentable_cells() {
    let state = MouseState::new(MouseButton::Left, MouseAction::Pressed, Default::default());
    assert!(ControlCommand::mouse(&state).is_none());
    assert!(ControlCommand::mouse(&state.set_point(Point::new(65536, 0))).is_none());
    assert!(ControlCommand::mouse(&state.set_point(Point::new(0, 65536))).is_none());
}

#[test]
fn input_command_serializes_exactly_one_of_text_or_bytes() {
    // herdr rejects terminal.input with both fields set and treats both-empty
    // as empty input, so neither state is constructible; every serialized
    // input command carries exactly one payload key.
    for cmd in [
        ControlCommand::Input(Input::text("pwd".into())),
        ControlCommand::input_bytes(b"\x00\xff"),
    ] {
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&cmd).unwrap()).unwrap();
        let payload_keys = ["text", "bytes"]
            .into_iter()
            .filter(|k| v.get(k).is_some())
            .count();
        assert_eq!(payload_keys, 1, "expected exactly one payload key in {v}");
    }
}

#[test]
fn unknown_event_kinds_are_tolerated() {
    // Forward compat: an event kind added by a future herdr must not fail the stream.
    let line =
        r#"{"event":"pane_hyperspaced","data":{"type":"pane_hyperspaced","pane_id":"w1:p9"}}"#;
    let ev: AnyEvent = serde_json::from_str(line).unwrap();
    assert!(matches!(ev, AnyEvent::Unknown));
}

#[test]
fn version_gate() {
    assert!(version_at_least("0.7.4", (0, 7, 4)));
    assert!(version_at_least("0.8.0", (0, 7, 4)));
    assert!(version_at_least("1.0.0", (0, 7, 4)));
    assert!(!version_at_least("0.7.3", (0, 7, 4)));
    assert!(!version_at_least("garbage", (0, 7, 4)));
    // Newer versions with fewer components or suffixes must not be rejected
    // as too old; missing components read as 0.
    assert!(version_at_least("0.8", (0, 7, 4)));
    assert!(version_at_least("1.0", (0, 7, 4)));
    assert!(version_at_least("0.7.5-beta", (0, 7, 4)));
    assert!(version_at_least("0.7.4.1", (0, 7, 4)));
    assert!(!version_at_least("0.7", (0, 7, 4)));
    assert!(!version_at_least("0.7.4-beta", (0, 7, 5)));
}

#[test]
fn unrecognized_agent_status_maps_to_unknown() {
    // Forward compat: a status added by a future herdr must not fail parsing
    // of the containing struct.
    let pane: PaneInfo = serde_json::from_str(
        r#"{"pane_id":"w1:p1","terminal_id":"term_ab","workspace_id":"w1","tab_id":"w1:t1",
            "focused":true,"agent_status":"hyperspacing","revision":1}"#,
    )
    .unwrap();
    assert_eq!(pane.agent_status, AgentStatus::Unknown);
}

#[test]
fn malformed_known_event_is_tolerated() {
    // A known kind whose payload fails to parse (schema drift) drops to
    // Unknown — and logs at warn — instead of failing the stream.
    let line = r#"{"event":"pane_closed","data":{"type":"pane_closed","pane_id":"w1:p3"}}"#;
    let ev: AnyEvent = serde_json::from_str(line).unwrap();
    assert!(matches!(ev, AnyEvent::Unknown));
    let line = r#"{"event":"pane.agent_status_changed","data":{"pane_id":"w1:p1"}}"#;
    let ev: AnyEvent = serde_json::from_str(line).unwrap();
    assert!(matches!(ev, AnyEvent::Unknown));
}

/// tab.moved's newest wire shape: tab_id + workspace_id + insert_index plus
/// the full post-move tab order.
#[test]
fn tab_moved_event_parses_wire_shape() {
    let line = r#"{"event":"tab_moved","data":{"type":"tab_moved","tab_id":"w1:t2","workspace_id":"w1","insert_index":0,"tabs":[
        {"tab_id":"w1:t2","workspace_id":"w1","number":2,"label":"b","focused":false,"pane_count":1,"agent_status":"idle"},
        {"tab_id":"w1:t1","workspace_id":"w1","number":1,"label":"a","focused":true,"pane_count":1,"agent_status":"idle"}]}}"#;
    let ev: AnyEvent = serde_json::from_str(line).unwrap();
    let AnyEvent::Lifecycle(env) = ev else {
        panic!("expected lifecycle event");
    };
    let EventData::TabMoved {
        tab_id,
        workspace_id,
        insert_index,
        tabs,
    } = env.data
    else {
        panic!("expected TabMoved, got {:?}", env.data);
    };
    assert_eq!(tab_id, "w1:t2");
    assert_eq!(workspace_id, "w1");
    assert_eq!(insert_index, 0);
    assert_eq!(
        tabs.iter().map(|t| t.tab_id.as_str()).collect::<Vec<_>>(),
        ["w1:t2", "w1:t1"]
    );
}
