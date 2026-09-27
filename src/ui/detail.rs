//! Full-pane detail views and the help overlay.

use ratatui::Frame;
use ratatui::layout::Rect;

use super::layout::Density;
use super::theme::Theme;
use crate::app::App;

/// Draws the top detail view into `area`; returns the selected line's rect.
pub fn draw(
    _f: &mut Frame,
    _app: &mut App,
    _theme: &Theme,
    _area: Rect,
    _d: Density,
) -> Option<Rect> {
    None
}

pub fn draw_help(_f: &mut Frame, _app: &mut App, _theme: &Theme, _area: Rect) {}
