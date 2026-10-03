//! Colours. Everything is built on the terminal's own 16-colour palette so
//! gitst follows light and dark themes; truecolor is only used for tints
//! mixed from a detected background.

use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Debug)]
pub struct Theme {
    pub accent: Color,
    pub add: Color,
    pub del: Color,
    pub warn: Color,
    pub err: Color,
    /// The possible-secrets band.
    pub leak: Style,
    pub modified: Color,
    pub untracked: Color,
    /// Header band.
    pub band: Style,
    pub select: Style,
    pub hover: Style,
    pub dim: Style,
    pub border: Style,
    pub nerd: bool,
}

type Rgb = (u8, u8, u8);

/// Linear blend from `a` (t = 0) to `b` (t = 1).
pub fn mix(a: Rgb, b: Rgb, t: f32) -> Color {
    let ch = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color::Rgb(ch(a.0, b.0), ch(a.1, b.1), ch(a.2, b.2))
}

impl Theme {
    pub fn ansi() -> Theme {
        Theme {
            accent: Color::Blue,
            add: Color::Green,
            del: Color::Red,
            warn: Color::Yellow,
            err: Color::Red,
            leak: Style::new()
                .fg(Color::White)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
            modified: Color::Yellow,
            untracked: Color::Magenta,
            band: Style::new().add_modifier(Modifier::BOLD),
            select: Style::new().add_modifier(Modifier::REVERSED),
            hover: Style::new().add_modifier(Modifier::UNDERLINED),
            dim: Style::new().add_modifier(Modifier::DIM),
            border: Style::new().add_modifier(Modifier::DIM),
            nerd: false,
        }
    }

    /// ANSI colours plus background tints mixed from the terminal's own
    /// foreground and background.
    pub fn from_palette(fg: Rgb, bg: Rgb) -> Theme {
        Theme {
            band: Style::new().bg(mix(bg, fg, 0.10)),
            select: Style::new().bg(mix(bg, fg, 0.20)),
            hover: Style::new().bg(mix(bg, fg, 0.08)),
            ..Theme::ansi()
        }
    }

    /// Warning glyph for possible secrets: nf-fa-warning with Nerd Font
    /// icons.
    pub fn leak_icon(&self) -> &'static str {
        if self.nerd { "\u{f071}" } else { "⚠" }
    }

    /// Queries the terminal's colours; falls back to plain ANSI styling.
    pub fn detect(nerd: bool) -> Theme {
        let mut opts = terminal_colorsaurus::QueryOptions::default();
        opts.timeout = Duration::from_millis(150);
        let mut theme = match terminal_colorsaurus::color_palette(opts) {
            Ok(p) => {
                Theme::from_palette(p.foreground.scale_to_8bit(), p.background.scale_to_8bit())
            }
            Err(_) => Theme::ansi(),
        };
        theme.nerd = nerd;
        theme
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_uses_named_colours_and_modifiers() {
        let t = Theme::ansi();
        assert_eq!(
            (t.add, t.del, t.warn),
            (Color::Green, Color::Red, Color::Yellow)
        );
        assert!(t.select.add_modifier.contains(Modifier::REVERSED));
        assert!(t.dim.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn palette_tints_mix_toward_foreground() {
        let t = Theme::from_palette((255, 255, 255), (0, 0, 0));
        assert_eq!(t.band.bg, Some(Color::Rgb(26, 26, 26)));
        assert_eq!(t.add, Color::Green);
        let light = Theme::from_palette((0, 0, 0), (255, 255, 255));
        assert_eq!(light.band.bg, Some(Color::Rgb(230, 230, 230)));
    }

    #[test]
    fn leak_band_is_white_on_red_with_an_icon() {
        let t = Theme::ansi();
        assert_eq!(
            (t.leak.fg, t.leak.bg),
            (Some(Color::White), Some(Color::Red))
        );
        assert_eq!(t.leak_icon(), "⚠");
        let nerd = Theme {
            nerd: true,
            ..Theme::ansi()
        };
        assert_eq!(nerd.leak_icon(), "\u{f071}");
    }
}
