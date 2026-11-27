//! The cells we own, and the difference between one frame's worth and the last.
//!
//! Everything the client draws in text -- the bar, the command menu, a notification --
//! renders into one buffer the size of the terminal, which is then compared against what
//! is already on screen. Only the cells that changed are written.
//!
//! The rule that makes this safe is the one `ui/mod.rs` states: never touch a cell we did
//! not write. A renderer that owned the whole screen would be wrong here, because it would
//! repaint the cells our graphics placements live in, and those are not its to repaint.
//! Owning one plane and diffing that is the same idea with the boundary in the right place:
//! a cell nothing has drawn in stays blank in both buffers, so it never appears in a diff.
//!
//! Two things fall out of it rather than being arranged:
//!
//! * **Taking chrome off the screen is not a separate operation.** A menu that closed, a
//!   bar whose row moved, a note that expired: each is simply absent from the next frame's
//!   buffer, and the cells it occupied come back as blanks in the diff. There is nothing to
//!   remember having drawn.
//! * **The cells it vacates are damage.** They are handed back from [`Chrome::flush`] so
//!   the tiles underneath can be redrawn, which is what a terminal that treats clearing a
//!   cell as dropping the placement under it needs -- and it is impossible to forget,
//!   because the flush returns them whether the caller wants them or not.
//!
//! What a diff rests on is knowing what is on screen, and there is one moment when we do
//! not: a resize. What a terminal leaves on the alternate screen after the window changed
//! shape is not specified, so [`Chrome::begin`] reports the change and the caller erases,
//! after which the frame says all of its chrome again rather than the difference from a
//! screen it is only assuming.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::paint::write_diff;
use crate::term::Metrics;
use ratatui::style::Color;

/// The chrome's own plane: what is on screen, and what should be.
pub struct Chrome {
    shown: Buffer,
    next: Buffer,
}

impl Chrome {
    pub fn new() -> Self {
        let empty = Rect::new(0, 0, 0, 0);
        Self {
            shown: Buffer::empty(empty),
            next: Buffer::empty(empty),
        }
    }

    /// Start a frame: blank everything, and adopt the terminal's size.
    ///
    /// Returns true when the geometry changed, which means nothing about the screen can be
    /// trusted and the caller owes it an erase -- see [`Chrome::flush`].
    ///
    /// A diff rests on knowing what is on screen, and across a resize it does not. Carrying
    /// the old cells over by coordinate looks right and is a guess: what a terminal does to
    /// the alternate screen when the window changes shape is not specified, and a client
    /// that assumes wrong leaves fragments of the old chrome stranded wherever the guess
    /// missed -- which is exactly what a resize with the menu open looked like. So a resize
    /// forgets the screen instead, and the frame that adopts the new geometry says
    /// everything again.
    pub fn begin(&mut self, metrics: &Metrics) -> bool {
        let area = Rect::new(0, 0, metrics.cols, metrics.rows);
        // An empty `shown` is the first frame, not a resize: nothing has been drawn, so
        // there is nothing on screen to distrust and nothing to erase.
        let resized = self.shown.area != area && !self.shown.area.is_empty();
        if self.shown.area != area {
            self.shown = Buffer::empty(area);
            self.next = Buffer::empty(area);
        }
        self.next.reset();
        resized
    }

    /// The buffer this frame's chrome renders into.
    pub fn buffer(&mut self) -> &mut Buffer {
        &mut self.next
    }

    /// Write the changed cells and return the cells that reveal the desktop again.
    ///
    /// After a [`Chrome::begin`] that reported a resize, the caller has to have erased the
    /// screen first: this then writes the whole of the new chrome, having forgotten the old.
    ///
    /// Nothing is committed here: [`Chrome::commit`] is called once the frame has actually
    /// reached the terminal, because a frame the writer was too busy for did not happen and
    /// the next one has to say all of it again.
    pub fn flush(&mut self, out: &mut Vec<u8>) -> Vec<Rect> {
        let updates = self.shown.diff(&self.next);
        write_diff(out, &updates);

        // Clearing even a blank padding cell can evict the tile underneath it.
        updates
            .iter()
            .filter(|(x, y, cell)| {
                let was = &self.shown[(*x, *y)];
                let erased_text = cell.symbol() == " " && was.symbol() != " ";
                cell.bg == Color::Reset && (was.bg != Color::Reset || erased_text)
            })
            .map(|(x, y, _)| Rect::new(*x, *y, 1, 1))
            .collect()
    }

    /// The frame reached the terminal, so what it drew is what is on screen.
    pub fn commit(&mut self) {
        std::mem::swap(&mut self.shown, &mut self.next);
    }
}

impl Default for Chrome {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    fn metrics(cols: u16, rows: u16) -> Metrics {
        Metrics {
            cols,
            rows,
            px_w: u32::from(cols) * 8,
            px_h: u32::from(rows) * 17,
            cell_w: 8,
            cell_h: 17,
        }
    }

    /// One frame: render `text` at `(x, y)` if given, flush, commit.
    fn frame(
        chrome: &mut Chrome,
        m: &Metrics,
        at: Option<(u16, u16, &str)>,
    ) -> (String, Vec<Rect>) {
        chrome.begin(m);
        if let Some((x, y, text)) = at {
            chrome.buffer().set_string(x, y, text, Style::new());
        }
        let mut out = Vec::new();
        let vacated = chrome.flush(&mut out);
        chrome.commit();
        (String::from_utf8(out).unwrap(), vacated)
    }

    #[test]
    fn only_what_changed_is_written() {
        let m = metrics(20, 5);
        let mut chrome = Chrome::new();

        let (first, vacated) = frame(&mut chrome, &m, Some((2, 1, "hello")));
        assert!(first.contains("hello"), "{first:?}");
        assert!(vacated.is_empty(), "nothing was on screen before");

        // The same content again writes nothing at all.
        let (again, vacated) = frame(&mut chrome, &m, Some((2, 1, "hello")));
        assert_eq!(again, "", "an unchanged plane is no bytes");
        assert!(vacated.is_empty());

        // One letter different is one cell.
        let (edit, _) = frame(&mut chrome, &m, Some((2, 1, "hellp")));
        assert!(edit.contains('p'), "{edit:?}");
        assert!(!edit.contains("hell"), "only the last cell moved: {edit:?}");
    }

    #[test]
    fn text_that_goes_away_blanks_its_cells_and_is_reported_as_damage() {
        // What used to be `Menu::clear`, `status::clear` and the popup's own record of
        // where it had drawn: absent from the next frame is all it takes.
        let m = metrics(20, 5);
        let mut chrome = Chrome::new();
        frame(&mut chrome, &m, Some((2, 1, "hi")));

        let (gone, vacated) = frame(&mut chrome, &m, None);
        assert!(
            gone.contains("\x1b[2;3H"),
            "positioned at the cells: {gone:?}"
        );
        assert_eq!(
            vacated,
            vec![Rect::new(2, 1, 1, 1), Rect::new(3, 1, 1, 1)],
            "both cells have to be handed back as damage"
        );
    }

    #[test]
    fn a_resize_is_reported_and_says_the_whole_chrome_again() {
        // The screen's contents do not survive a resize as far as we are concerned, so the
        // frame that adopts the new size writes all of its chrome rather than a diff against
        // where the old chrome used to be. Guessing that instead -- carrying the old cells
        // over by coordinate -- is what stranded fragments of a menu across the screen.
        let narrow = metrics(20, 5);
        let mut chrome = Chrome::new();
        frame(&mut chrome, &narrow, Some((0, 4, "bar")));

        // Unchanged geometry: still a diff, still nothing to say.
        assert!(!chrome.begin(&narrow), "the same size is not a resize");

        let wide = metrics(40, 8);
        assert!(chrome.begin(&wide), "a new size has to be reported");
        chrome.buffer().set_string(0, 7, "bar", Style::new());
        let mut out = Vec::new();
        let vacated = chrome.flush(&mut out);
        chrome.commit();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("\x1b[8;1H") && text.contains("bar"),
            "the bar is written in full, on the new last row: {text:?}"
        );
        assert!(
            !text.contains("\x1b[5;1H"),
            "and nothing is blanked at the old coordinates, which the caller's erase has \
             already taken care of: {text:?}"
        );
        assert!(
            vacated.is_empty(),
            "with the screen erased there is no cell that went blank"
        );
    }

    #[test]
    fn a_dropped_frame_is_said_again() {
        // No commit, so nothing reached the terminal and the next frame owes all of it.
        let m = metrics(20, 5);
        let mut chrome = Chrome::new();
        chrome.begin(&m);
        chrome.buffer().set_string(0, 0, "x", Style::new());
        let mut out = Vec::new();
        chrome.flush(&mut out);
        // ... and no commit.

        chrome.begin(&m);
        chrome.buffer().set_string(0, 0, "x", Style::new());
        let mut again = Vec::new();
        chrome.flush(&mut again);
        assert_eq!(
            again, out,
            "the frame that was thrown away has to be redone"
        );
    }

    #[test]
    fn clearing_blank_padding_damages_only_the_cells_that_reveal_the_desktop() {
        let mut chrome = Chrome::new();
        let m = metrics(12, 6);
        chrome.begin(&m);
        chrome
            .buffer()
            .set_style(Rect::new(4, 2, 3, 1), Style::new().bg(Color::Black));
        chrome.flush(&mut Vec::new());
        chrome.commit();

        chrome.begin(&m);
        chrome
            .buffer()
            .set_style(Rect::new(5, 2, 1, 1), Style::new().bg(Color::White));
        chrome.buffer().set_string(6, 2, "x", Style::new());
        let vacated = chrome.flush(&mut Vec::new());
        assert_eq!(vacated, [Rect::new(4, 2, 1, 1), Rect::new(6, 2, 1, 1)]);
    }
}
