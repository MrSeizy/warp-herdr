//! Serde types for Herdr's JSON socket API (v0.7.4 through v0.9.5) and the
//! `herdr terminal session control` stream bridge. Wire-shape notes:
//! request ids are strings; delivered lifecycle events are snake_case even
//! though subscriptions are registered with dotted names.

use std::borrow::Cow;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};

use crate::terminal::model::mouse::{MouseAction, MouseButton, MouseState};

/// Stream-close reason when another client takes control of the pane.
pub(crate) const REASON_ATTACH_TAKEN_OVER: &str = "terminal attach taken over";

#[derive(Debug, Clone, Serialize)]
pub struct ApiRequest {
    pub id: String,
    pub method: String,
    pub params: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ApiResponse {
    Success {
        id: String,
        result: Box<ResponseResult>,
    },
    Error {
        id: String,
        error: ErrorBody,
    },
}

#[derive(Debug, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseResult {
    Pong(Pong),
    SessionSnapshot {
        snapshot: SessionSnapshot,
    },
    SubscriptionStarted {},
    Ok {},
    TabCreated {
        tab: TabInfo,
        root_pane: PaneInfo,
    },
    TabInfo {
        tab: TabInfo,
    },
    TabList {
        tabs: Vec<TabInfo>,
    },
    PaneInfo {
        pane: PaneInfo,
    },
    // herdr tags this response "layout_export" (not "layout_exported").
    #[serde(rename = "layout_export")]
    LayoutExported {
        layout: LayoutDescription,
    },
    LayoutSplitRatioSet {
        layout: LayoutDescription,
    },
    WorkspaceList {
        workspaces: Vec<WorkspaceInfo>,
    },
    WorkspaceCreated {
        workspace: WorkspaceInfo,
        tab: TabInfo,
        root_pane: PaneInfo,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
pub struct Pong {
    pub version: String,
    pub protocol: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

// Manual Deserialize (`#[serde(other)]` is not valid on externally-tagged
// fieldless enums): any status a future herdr adds maps to Unknown instead of
// failing the parse of the whole containing snapshot or event.
impl<'de> Deserialize<'de> for AgentStatus {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = Cow::<'de, str>::deserialize(de)?;
        Ok(match s.as_ref() {
            "idle" => AgentStatus::Idle,
            "working" => AgentStatus::Working,
            "blocked" => AgentStatus::Blocked,
            "done" => AgentStatus::Done,
            _ => AgentStatus::Unknown,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionSnapshot {
    pub version: String,
    pub protocol: u32,
    pub focused_workspace_id: Option<String>,
    pub focused_tab_id: Option<String>,
    pub focused_pane_id: Option<String>,
    pub workspaces: Vec<WorkspaceInfo>,
    pub tabs: Vec<TabInfo>,
    pub panes: Vec<PaneInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceInfo {
    pub workspace_id: String,
    pub number: usize,
    pub label: String,
    pub focused: bool,
    pub active_tab_id: String,
    pub agent_status: AgentStatus,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TabInfo {
    pub tab_id: String,
    pub workspace_id: String,
    pub number: usize,
    pub label: String,
    pub focused: bool,
    pub pane_count: usize,
    pub agent_status: AgentStatus,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PaneInfo {
    pub pane_id: String,
    pub terminal_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub focused: bool,
    pub cwd: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    pub title: Option<String>,
    pub terminal_title: Option<String>,
    pub display_agent: Option<String>,
    pub agent: Option<String>,
    pub agent_status: AgentStatus,
    pub revision: u64,
}

// ---- layout (BSP tree from layout.export) ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HerdrSplitDirection {
    Right,
    Down,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LayoutNode {
    Pane {
        pane_id: Option<String>,
        cwd: Option<String>,
    },
    Split {
        direction: HerdrSplitDirection,
        ratio: f32,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct LayoutDescription {
    pub workspace_id: String,
    pub tab_id: String,
    pub zoomed: bool,
    /// Parsed but deliberately unused (focus is one-way Warp→herdr);
    /// optional so a null/absent value can't fail the whole layout parse
    /// and degrade the tab to the single-leaf fallback.
    #[serde(default)]
    pub focused_pane_id: Option<String>,
    pub root: LayoutNode,
}

// ---- events ----

#[derive(Debug, Clone, Deserialize)]
pub struct EventEnvelope {
    pub event: String,
    pub data: EventData,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventData {
    PaneCreated {
        pane: PaneInfo,
    },
    PaneClosed {
        pane_id: String,
        workspace_id: String,
    },
    PaneUpdated {
        pane: PaneInfo,
    },
    PaneFocused {
        pane_id: String,
        workspace_id: String,
    },
    PaneExited {
        pane_id: String,
        workspace_id: String,
    },
    /// The process-detected agent in a pane changed (started, exited, or
    /// switched). Unlike agent-status, this does NOT arrive as a pane.updated
    /// revision — it has its own bare subscription. `agent` is omitted on the
    /// wire when the agent exited.
    PaneAgentDetected {
        pane_id: String,
        #[serde(default)]
        workspace_id: String,
        agent: Option<String>,
    },
    TabCreated {
        tab: TabInfo,
    },
    TabClosed {
        tab_id: String,
        workspace_id: String,
    },
    TabRenamed {
        tab_id: String,
        workspace_id: String,
        label: String,
    },
    TabMoved {
        tab_id: String,
        workspace_id: String,
        insert_index: usize,
        tabs: Vec<TabInfo>,
    },
    TabFocused {
        tab_id: String,
        workspace_id: String,
    },
    WorkspaceCreated {
        workspace: WorkspaceInfo,
    },
    WorkspaceRenamed {
        workspace_id: String,
        label: String,
    },
    WorkspaceClosed {
        workspace_id: String,
    },
    LayoutUpdated {
        tab_id: String,
    },
    #[serde(other)]
    Unknown,
}

/// Untagged payload of the dedicated `pane.agent_status_changed` subscription.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentStatusEvent {
    pub pane_id: String,
    pub workspace_id: String,
    pub agent_status: AgentStatus,
    pub title: Option<String>,
    pub display_agent: Option<String>,
}

/// One parsed line from the events.subscribe connection. Lifecycle events are
/// snake_case-tagged; the agent-status subscription is dot-named with untagged
/// data, so we dispatch on the `event` field manually.
#[derive(Debug, Clone)]
pub enum AnyEvent {
    Lifecycle(EventEnvelope),
    AgentStatus(AgentStatusEvent),
    Unknown,
}

impl<'de> Deserialize<'de> for AnyEvent {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(de)?;
        let kind = v
            .get("event")
            .and_then(|e| e.as_str())
            .unwrap_or_default()
            .to_string();
        if kind == "pane.agent_status_changed" {
            let data = v.get("data").cloned().unwrap_or(serde_json::Value::Null);
            return match serde_json::from_value::<AgentStatusEvent>(data) {
                Ok(e) => Ok(AnyEvent::AgentStatus(e)),
                Err(err) => {
                    // A known kind whose payload fails to parse is schema
                    // drift, not an unknown event — log it, never silently
                    // swallow it.
                    log::warn!("herdr: dropping malformed {kind} event: {err:#}");
                    Ok(AnyEvent::Unknown)
                }
            };
        }
        match serde_json::from_value::<EventEnvelope>(v) {
            Ok(env) if !matches!(env.data, EventData::Unknown) => Ok(AnyEvent::Lifecycle(env)),
            // Genuinely unknown (future) event kind: intended forward-compat
            // drop, no log.
            Ok(_) => Ok(AnyEvent::Unknown),
            Err(err) => {
                log::warn!("herdr: dropping malformed {kind} event: {err:#}");
                Ok(AnyEvent::Unknown)
            }
        }
    }
}

// ---- stream bridge records (herdr terminal session control) ----

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum StreamRecord {
    #[serde(rename = "terminal.frame")]
    Frame(TerminalFrame),
    #[serde(rename = "terminal.closed")]
    Closed { reason: Option<String> },
}

// `seq`, `encoding`, `width`, `height`, and `full` are parsed but intentionally
// unused by the consumer: the pipe is ordered, and incremental frames are
// plain ANSI that applies without geometry. They default (rather than being
// required) so a herdr that drops or renames any of them can't make every
// frame unparseable and silently freeze the pane — only `bytes` is essential.
#[derive(Debug, Clone, Deserialize)]
pub struct TerminalFrame {
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub encoding: String,
    #[serde(default)]
    pub width: u16,
    #[serde(default)]
    pub height: u16,
    #[serde(default)]
    pub full: bool,
    pub bytes: String,
}

impl TerminalFrame {
    pub fn decoded(&self) -> anyhow::Result<Vec<u8>> {
        // One frame is one rendered grid — a few hundred KiB at the very
        // largest. The cap keeps a broken or hostile peer from making Warp
        // allocate without bound from a single record.
        const MAX_FRAME_BASE64_BYTES: usize = 16 * 1024 * 1024;
        if self.bytes.len() > MAX_FRAME_BASE64_BYTES {
            anyhow::bail!(
                "herdr frame of {} base64 bytes exceeds the {MAX_FRAME_BASE64_BYTES} byte cap",
                self.bytes.len()
            );
        }
        Ok(STANDARD.decode(&self.bytes)?)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ControlCommand {
    #[serde(rename = "terminal.input")]
    Input(Input),
    #[serde(rename = "terminal.resize")]
    Resize {
        cols: u16,
        rows: u16,
        cell_width_px: u16,
        cell_height_px: u16,
    },
    #[serde(rename = "terminal.scroll")]
    Scroll {
        direction: &'static str,
        // herdr parses this as u16; larger values are silently dropped
        // server-side.
        lines: u16,
        source: &'static str,
        modifiers: u8,
        #[serde(skip_serializing_if = "Option::is_none")]
        column: Option<u16>,
        #[serde(skip_serializing_if = "Option::is_none")]
        row: Option<u16>,
    },
    #[serde(rename = "terminal.mouse")]
    Mouse {
        action: &'static str,
        button: &'static str,
        column: u16,
        row: u16,
        modifiers: u8,
    },
    #[serde(rename = "terminal.release")]
    Release,
}

/// Payload of a `terminal.input` control command. herdr requires exactly one
/// of `text` or `bytes`: both set is rejected, both empty sends empty input.
/// The constructors make that the only representable state, and the fields
/// stay private so it can't be violated after construction.
#[derive(Debug, Clone, Serialize)]
pub struct Input {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    // Base64-encoded on the wire; see Input::bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<String>,
}

impl Input {
    /// UTF-8 text input, sent as-is.
    pub fn text(text: String) -> Self {
        Input {
            text: Some(text),
            bytes: None,
        }
    }

    /// Raw byte input (may not be valid UTF-8), base64-encoded on the wire.
    pub fn bytes(bytes: &[u8]) -> Self {
        Input {
            text: None,
            bytes: Some(STANDARD.encode(bytes)),
        }
    }
}

impl ControlCommand {
    pub fn input_bytes(bytes: &[u8]) -> Self {
        ControlCommand::Input(Input::bytes(bytes))
    }

    /// Herdr encodes the event for the child's actual mouse protocol. Cell
    /// coordinates are zero-based; modifier bits match crossterm, not X10.
    pub fn mouse(state: &MouseState) -> Option<Self> {
        let point = state.maybe_point()?;
        let column = u16::try_from(point.col).ok()?;
        let row = u16::try_from(point.row).ok()?;
        let keys = state.modifiers();
        let modifiers = u8::from(keys.shift)
            | (u8::from(keys.ctrl) << 1)
            | (u8::from(keys.alt) << 2)
            | (u8::from(keys.cmd) << 3);
        if let MouseAction::Scrolled { delta } = state.action() {
            if *delta == 0 {
                return None;
            }
            return Some(Self::Scroll {
                direction: if *delta > 0 { "up" } else { "down" },
                lines: delta.unsigned_abs().min(u32::from(u16::MAX)) as u16,
                source: "wheel",
                column: Some(column),
                row: Some(row),
                modifiers,
            });
        }
        let button = match state.button() {
            MouseButton::Left | MouseButton::LeftDrag | MouseButton::Move => "left",
            MouseButton::Right => "right",
            MouseButton::Wheel => return None,
        };
        let action = match state.action() {
            MouseAction::Released => "up",
            MouseAction::Pressed => match state.button() {
                MouseButton::LeftDrag => "drag",
                MouseButton::Move => "move",
                MouseButton::Left | MouseButton::Right => "down",
                MouseButton::Wheel => return None,
            },
            MouseAction::Scrolled { .. } => unreachable!("scroll handled above"),
        };
        Some(Self::Mouse {
            action,
            button,
            column,
            row,
            modifiers,
        })
    }
}

/// Longest newline-delimited record accepted from a herdr peer — the socket
/// event stream and the control subprocess's stdout share the bound. herdr's
/// largest legitimate payloads (session snapshots, base64 frame records) are
/// single-digit MiB at the extreme; the cap only exists so a broken or
/// hostile peer can't grow one line without bound.
#[cfg(not(target_family = "wasm"))]
pub(crate) const MAX_PEER_LINE_BYTES: u64 = 32 * 1024 * 1024;

/// One capped `read_line` step of a newline-delimited JSON reader loop.
#[cfg(not(target_family = "wasm"))]
pub(crate) enum CappedLine {
    /// `line` holds a complete record (possibly blank).
    Line,
    /// Clean EOF.
    Eof,
    /// The peer exceeded [`MAX_PEER_LINE_BYTES`] without a newline; the
    /// caller must close the stream (resuming mid-record is unsafe).
    Overlong,
}

/// Reads the next line into `line` (cleared first), bounded by
/// [`MAX_PEER_LINE_BYTES`]. Shared by the socket event reader and the
/// control-subprocess stdout reader so the cap logic can't drift.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn read_capped_line(
    reader: &mut impl std::io::BufRead,
    line: &mut String,
) -> std::io::Result<CappedLine> {
    use std::io::{BufRead as _, Read as _};
    line.clear();
    let n = reader.take(MAX_PEER_LINE_BYTES).read_line(line)?;
    Ok(if n == 0 {
        CappedLine::Eof
    } else if n as u64 >= MAX_PEER_LINE_BYTES && !line.ends_with('\n') {
        CappedLine::Overlong
    } else {
        CappedLine::Line
    })
}

/// Minimum herdr version this integration is developed and tested against.
pub const MIN_HERDR_VERSION: (u32, u32, u32) = (0, 7, 4);

/// Minimum herdr control-protocol number this integration understands
/// (herdr 0.7.4 answers 16).
pub const MIN_HERDR_PROTOCOL: u32 = 16;

/// Tolerant gate: 1–3 numeric components (missing ones read as 0) with any
/// pre-release/build suffix on a component ignored, so a NEWER herdr
/// ("0.8", "1.0", "0.7.5-beta") is never rejected as too old. Extra
/// components ("0.7.4.1") are ignored. Unparseable versions fail the gate.
pub fn version_at_least(version: &str, min: (u32, u32, u32)) -> bool {
    fn component(part: Option<&str>) -> Option<u32> {
        let Some(part) = part else {
            return Some(0); // missing component reads as 0 ("0.8" → 0.8.0)
        };
        let digits = part
            .trim()
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap_or("");
        digits.parse().ok()
    }
    let mut parts = version.split('.');
    match (
        component(parts.next()),
        component(parts.next()),
        component(parts.next()),
    ) {
        (Some(a), Some(b), Some(c)) => (a, b, c) >= min,
        _ => false,
    }
}

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod tests;
