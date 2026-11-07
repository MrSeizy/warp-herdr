//! The passive "herdr" chip marking a herdr-bound tab (herdr control mode).
//! One renderer shared by the sidebar rows / detail-sidecar sections
//! (`workspace::view::vertical_tabs`) and the horizontal tab bar
//! (`crate::tab`), so the chip's style can't drift between surfaces.

use warp_core::ui::theme::color::internal_colors;
use warpui::Element;
use warpui::elements::{Container, CornerRadius, Padding, Radius, Text};

use crate::appearance::Appearance;

/// Chip label; also pushed into the sidebar's search fragments so typing
/// "herdr" matches herdr-bound rows.
pub const HERDR_BADGE_LABEL: &str = "herdr";

/// Passive chip marking a herdr-bound tab. Complements the herdr agent-icon
/// fallback (the icon still shows the mirrored pane's reported agent) and
/// the agent-status color roll-up (the row background/border) — all three
/// can appear on the same row.
pub fn render_herdr_badge(appearance: &Appearance) -> Box<dyn Element> {
    let theme = appearance.theme();
    Container::new(
        Text::new_inline(HERDR_BADGE_LABEL, appearance.ui_font_family(), 10.)
            .with_color(theme.accent().into())
            .finish(),
    )
    .with_padding(Padding::uniform(1.).with_left(4.).with_right(4.))
    .with_background(internal_colors::fg_overlay_1(theme))
    .with_corner_radius(CornerRadius::with_all(Radius::Pixels(3.)))
    .finish()
}
