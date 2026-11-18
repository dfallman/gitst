//! A one-line text input for commit and stash messages. Pure: no I/O.
//! The cursor counts graphemes; `view` measures in terminal cells.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

use crate::ui::fmt::width;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineInput {
    pub text: String,
    /// Grapheme index, `0..=graphemes`.
    cursor: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Edit {
    Changed,
    Submit,
    Cancel,
    Ignored,
}

impl LineInput {
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            cursor: text.graphemes(true).count(),
        }
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn len(&self) -> usize {
        self.text.graphemes(true).count()
    }

    /// Byte offset of grapheme index `i` (the text length when past the end).
    fn byte_at(&self, i: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .nth(i)
            .map_or(self.text.len(), |(b, _)| b)
    }

    fn remove(&mut self, from: usize, to: usize) {
        let (a, b) = (self.byte_at(from), self.byte_at(to));
        self.text.replace_range(a..b, "");
    }

    pub fn key(&mut self, k: KeyEvent) -> Edit {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        match k.code {
            KeyCode::Enter => Edit::Submit,
            KeyCode::Esc => Edit::Cancel,
            KeyCode::Char('w') if ctrl => {
                let mut start = self.cursor;
                let gs: Vec<&str> = self.text.graphemes(true).collect();
                while start > 0 && gs[start - 1].trim().is_empty() {
                    start -= 1;
                }
                while start > 0 && !gs[start - 1].trim().is_empty() {
                    start -= 1;
                }
                self.delete_before_cursor(start)
            }
            KeyCode::Char('u') if ctrl => self.delete_before_cursor(0),
            KeyCode::Char(c) if !ctrl && !alt && !c.is_control() => {
                let at = self.byte_at(self.cursor);
                let before = self.len();
                self.text.insert(at, c);
                // A combining mark merges into the previous grapheme.
                if self.len() > before {
                    self.cursor += 1;
                }
                Edit::Changed
            }
            KeyCode::Left if !ctrl && !alt => self.move_to(self.cursor.saturating_sub(1)),
            KeyCode::Right if !ctrl && !alt => self.move_to((self.cursor + 1).min(self.len())),
            KeyCode::Home => self.move_to(0),
            KeyCode::End => self.move_to(self.len()),
            KeyCode::Backspace if self.cursor > 0 => {
                self.remove(self.cursor - 1, self.cursor);
                self.cursor -= 1;
                Edit::Changed
            }
            KeyCode::Delete if self.cursor < self.len() => {
                self.remove(self.cursor, self.cursor + 1);
                Edit::Changed
            }
            _ => Edit::Ignored,
        }
    }

    fn delete_before_cursor(&mut self, start: usize) -> Edit {
        if start == self.cursor {
            return Edit::Ignored;
        }
        self.remove(start, self.cursor);
        self.cursor = start;
        Edit::Changed
    }

    fn move_to(&mut self, to: usize) -> Edit {
        if to == self.cursor {
            return Edit::Ignored;
        }
        self.cursor = to;
        Edit::Changed
    }

    /// Inserts at the cursor. Each line break and any other control
    /// character becomes one space, so nothing unprintable gets in.
    pub fn paste(&mut self, s: &str) {
        let flat = s
            .replace("\r\n", "\n")
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect::<String>();
        let at = self.byte_at(self.cursor);
        let before = self.len();
        self.text.insert_str(at, &flat);
        // Count what was added, tolerating merges at the seam.
        let added = self.len().saturating_sub(before);
        self.cursor = (self.cursor + added).min(self.len());
    }

    /// The visible slice for `width` cells, scrolled to keep the cursor in
    /// view, and the cursor's cell offset. One cell is kept for the cursor.
    pub fn view(&self, cells: usize) -> (String, usize) {
        let gs: Vec<&str> = self.text.graphemes(true).collect();
        let mut start = self.cursor;
        let mut left = 0;
        while start > 0 {
            let w = width(gs[start - 1]);
            if left + w >= cells {
                break;
            }
            left += w;
            start -= 1;
        }
        let mut shown = left;
        let mut end = self.cursor;
        while end < gs.len() {
            let w = width(gs[end]);
            if shown + w > cells {
                break;
            }
            shown += w;
            end += 1;
        }
        (gs[start..end].concat(), left)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(i: &mut LineInput, code: KeyCode) -> Edit {
        i.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl(i: &mut LineInput, c: char) -> Edit {
        i.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    fn type_str(i: &mut LineInput, s: &str) {
        for c in s.chars() {
            press(i, KeyCode::Char(c));
        }
    }

    #[test]
    fn typing_and_cursor_moves() {
        let mut i = LineInput::default();
        type_str(&mut i, "ab");
        press(&mut i, KeyCode::Left);
        type_str(&mut i, "x");
        assert_eq!(i.text, "axb");
        press(&mut i, KeyCode::Home);
        press(&mut i, KeyCode::Delete);
        assert_eq!(i.text, "xb");
        press(&mut i, KeyCode::End);
        press(&mut i, KeyCode::Backspace);
        assert_eq!(i.text, "x");
        assert_eq!(i.cursor(), 1);
    }

    #[test]
    fn ctrl_w_and_ctrl_u() {
        let mut i = LineInput::new("fix the bug");
        ctrl(&mut i, 'w');
        assert_eq!(i.text, "fix the ");
        ctrl(&mut i, 'w');
        assert_eq!(i.text, "fix ");
        ctrl(&mut i, 'u');
        assert_eq!(i.text, "");
        assert_eq!(ctrl(&mut i, 'u'), Edit::Ignored);
    }

    #[test]
    fn graphemes_are_whole() {
        let mut i = LineInput::new("e\u{301}");
        assert_eq!(i.cursor(), 1);
        press(&mut i, KeyCode::Backspace);
        assert_eq!(i.text, "");
    }

    #[test]
    fn paste_flattens_newlines() {
        let mut i = LineInput::default();
        i.paste("a\r\nb\nc");
        assert_eq!(i.text, "a b c");
        i.paste("\td\r");
        assert_eq!(i.text, "a b c d ");
        assert_eq!(i.cursor(), 8);
    }

    #[test]
    fn enter_submits_esc_cancels() {
        let mut i = LineInput::new("x");
        assert_eq!(press(&mut i, KeyCode::Enter), Edit::Submit);
        assert_eq!(press(&mut i, KeyCode::Esc), Edit::Cancel);
        assert_eq!(press(&mut i, KeyCode::Tab), Edit::Ignored);
        assert_eq!(ctrl(&mut i, 'a'), Edit::Ignored);
        assert_eq!(ctrl(&mut i, 'e'), Edit::Ignored);
        assert_eq!(i.text, "x");
    }

    #[test]
    fn view_scrolls_to_cursor() {
        let i = LineInput::new(&"abcdefghij".repeat(4));
        let (s, off) = i.view(10);
        assert!(s.ends_with("ghij"));
        assert!(off <= 9);
        assert_eq!(width(&s), off);
    }

    #[test]
    fn view_never_splits_wide_graphemes() {
        let mut i = LineInput::new("日本語日本語");
        let (s, off) = i.view(7);
        assert_eq!(s, "語日本語".chars().skip(1).collect::<String>());
        assert_eq!(off, 6);
        press(&mut i, KeyCode::Home);
        let (s, off) = i.view(7);
        assert_eq!((s.as_str(), off), ("日本語", 0));
    }

    #[test]
    fn modified_chars_are_not_inserted() {
        let mut i = LineInput::default();
        let alt = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT);
        assert_eq!(i.key(alt), Edit::Ignored);
        let shift = KeyEvent::new(KeyCode::Char('X'), KeyModifiers::SHIFT);
        assert_eq!(i.key(shift), Edit::Changed);
        assert_eq!(i.text, "X");
    }
}
