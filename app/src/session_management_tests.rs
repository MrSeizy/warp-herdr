use super::*;
use crate::pane_group::pane::TerminalPaneId;
use crate::workspace::util::PaneViewLocator;

fn session(running_command: &str, is_foreign_controlled: bool) -> SessionNavigationData {
    SessionNavigationData::new(
        "prompt".to_string(),
        SessionNavigationPromptElements {
            ps1_prompt_grid: None,
            prompt_chip_snapshot: None,
        },
        CommandContext::RunningCommand {
            running_command: running_command.to_string(),
        },
        PaneViewLocator {
            pane_group_id: EntityId::new(),
            pane_id: TerminalPaneId::dummy_terminal_pane_id().into(),
        },
        None,
        false,
        is_foreign_controlled,
        WindowId::new(),
        SharedSessionStatus::NotShared,
    )
}

/// Mirror panes survive Warp closing, so they must not count toward
/// "N processes running" quit/logout warnings.
#[test]
fn running_summary_excludes_foreign_controlled_sessions() {
    let sessions = vec![session("cargo build", false), session("claude", true)];
    let summary = RunningSessionSummary::new(&sessions);
    assert_eq!(
        summary.long_running_cmds.len(),
        1,
        "mirror pane must be excluded"
    );
    assert!(!summary.long_running_cmds[0].is_foreign_controlled());
}
