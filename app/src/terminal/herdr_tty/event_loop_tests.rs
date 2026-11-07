//! Feeding decoded ANSI through `Processor::parse_bytes` must update the
//! model — proves the mirrored-pane parse path works without a GUI.

use super::*;

#[test]
fn mirrored_model_accepts_raw_input() {
    let mut model = TerminalModel::mock(None, None);
    super::prepare_model_for_mirroring(&mut model, Some("/tmp/herdr-cwd"), Some("claude"));
    let mut parser = Processor::new();
    parser.parse_bytes(&mut model, b"\x1b[?1049h", &mut std::io::sink());
    // AltScreen input state + a started active block are the gates Warp
    // checks before forwarding keystrokes to the pty.
    assert!(matches!(
        model.terminal_input_state(),
        crate::terminal::model::terminal_model::TerminalInputState::AltScreen
    ));
    assert!(model.block_list().active_block().started());
    // Guards the PtyController against injecting the bootstrap script.
    assert!(model.is_foreign_controlled());
}

#[test]
fn title_osc_flows_into_model() {
    let mut model = TerminalModel::mock(None, None);
    let mut parser = Processor::new();
    parser.parse_bytes(&mut model, b"\x1b]2;herdr-pane\x07", &mut std::io::sink());
    assert_eq!(model.terminal_title().as_deref(), Some("herdr-pane"));
}

#[test]
fn alt_screen_reasserted_after_1049l() {
    // Drives the REAL frame path (process_frame_bytes): a mirrored frame
    // carrying 1049l (vim/less exiting the alt screen) must leave the model
    // back in AltScreen input state, or keystrokes fall into Warp's block
    // editor. Deleting the re-assert must fail this test.
    let mut model = TerminalModel::mock(None, None);
    super::prepare_model_for_mirroring(&mut model, None, None);
    let mut parser = Processor::new();
    super::process_frame_bytes(&mut model, &mut parser, b"\x1b[?1049h");
    super::process_frame_bytes(&mut model, &mut parser, b"vim says bye\x1b[?1049l");
    assert!(matches!(
        model.terminal_input_state(),
        crate::terminal::model::terminal_model::TerminalInputState::AltScreen
    ));
}

#[test]
fn reconnect_frame_preserves_outage_read_only_override() {
    let mut model = TerminalModel::mock(None, None);
    prepare_model_for_mirroring(&mut model, None, None);
    model.set_foreign_control_released(true);
    model.set_forced_read_only(true);
    let mut parser = Processor::new();

    process_frame_bytes(&mut model, &mut parser, b"fresh frame");

    assert!(!model.foreign_control_released());
    assert!(model.is_read_only());
}
