use std::borrow::Cow;

use crate::SizeInfo;
use crate::model::mouse::MouseState;

/// Messages that may be sent to the `EventLoop`.
#[derive(Debug)]
pub enum Message {
    /// Data that should be written to the PTY.
    Input(Cow<'static, [u8]>),

    /// Structured mouse input for a backend that owns the child's terminal modes.
    Mouse(MouseState),

    /// Indicates that the `EventLoop` should be shut down.
    Shutdown,

    /// Stops the old shell without terminating the terminal that will host its replacement.
    ShutdownForReplacement,

    /// Indicates that the child process has exited.
    ///
    /// Only used on Windows, as we need to pass this information to the
    /// event loop via the channel (and cannot use the child event token).
    #[cfg_attr(not(windows), allow(dead_code))]
    ChildExited,

    /// Instruction to resize the PTY.
    Resize(SizeInfo),

    /// Scroll request for a backend whose scrollback lives outside Warp's
    /// grid (herdr control mode). Only foreign-controlled panes produce
    /// these; backends with a Warp-side grid (local, remote) ignore them.
    Scroll {
        /// Scroll direction: true scrolls up into history.
        up: bool,
        /// Number of rows to scroll; the receiving backend clamps to its
        /// wire format.
        lines: u32,
        source: ScrollSource,
    },
}

/// The gesture behind a [`Message::Scroll`]. herdr uses it to route the
/// scroll: wheel scrolls go to the inner program when it requested mouse
/// reporting (and to herdr's own scrollback otherwise), while page keys go
/// to the program as escape sequences unless it left them unclaimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollSource {
    Wheel,
    PageKey,
}
