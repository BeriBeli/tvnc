//! The status line.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use super::theme::{Palette, colour};
use crate::term::Metrics;
use crate::term::input::Command;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct FooterActions {
    pub menu: Rect,
    pub quit: Rect,
}

impl FooterActions {
    pub fn at(&self, col: u16, row: u16) -> Option<Command> {
        let at = ratatui::layout::Position::new(col, row);
        if self.menu.contains(at) {
            Some(Command::Menu)
        } else if self.quit.contains(at) {
            Some(Command::Quit)
        } else {
            None
        }
    }
}

/// In a one-row pane, the modal menu uses the footer itself.
pub fn compact_menu(buf: &mut Buffer, metrics: &Metrics, ink: &Palette) -> FooterActions {
    let row = Rect::new(0, 0, metrics.cols, metrics.rows.min(1));
    buf.set_style(row, Style::new().bg(colour(ink.bar)).fg(colour(ink.text)));
    let quit_label = if row.width >= 8 { "q Quit" } else { "q" };
    let quit = Rect::new(0, 0, row.width.min(quit_label.len() as u16), row.height);
    Line::from(ink.bright(quit_label)).render(quit, buf);
    let free = row.width - quit.width;
    let close = if free >= 10 {
        "Esc Close"
    } else if free >= 1 {
        "x"
    } else {
        ""
    };
    let menu = Rect::new(
        row.width - close.len() as u16,
        0,
        close.len() as u16,
        row.height,
    );
    Line::from(ink.bright(close)).render(menu, buf);
    FooterActions { menu, quit }
}

/// Draw the status line on the bottom row, in `ink`.
///
/// Reserve the clickable shortcut first, then fit the name and statistics into
/// the remaining cells. The returned rectangle covers the visible shortcut.
pub fn render(
    buf: &mut Buffer,
    metrics: &Metrics,
    ink: &Palette,
    left: Vec<Span>,
    right: Vec<Span>,
    shortcut: Vec<Span>,
) -> Rect {
    if metrics.cols == 0 || metrics.rows == 0 {
        return Rect::default();
    }
    let area = Rect {
        x: 0,
        y: metrics.rows - 1,
        width: metrics.cols,
        height: 1,
    };

    // Coloured before anything is written on it, so the whole row carries the bar
    // whether or not the text reaches the end.
    buf.set_style(area, Style::new().bg(colour(ink.bar)).fg(colour(ink.text)));

    let shortcut = Line::from(shortcut);
    let width = shortcut.width();
    let mut content = area;
    let mut hit = Rect::default();
    if width > 0 && width <= usize::from(area.width) {
        let width = width as u16;
        let start = (area.width - width).saturating_sub(1);
        hit = Rect::new(start, area.y, width, 1);
        shortcut.render(hit, buf);
        content.width = start;
    }
    let left = Line::from(left);
    let right = Line::from(right).right_aligned();
    // Widths as they will be drawn rather than counts of characters: a server that
    // names itself in something wider than Latin-1 would otherwise be measured short
    // and push the statistics off the end.
    let both = left.width() + right.width() < usize::from(content.width);

    // Rendered into the same row, one aligned each way. Whatever does not fit is
    // clipped at the edge, so the row is exactly its own width however long the text.
    let left_width = content.width.saturating_sub(u16::from(!hit.is_empty()));
    left.render(
        Rect {
            width: left_width,
            ..content
        },
        buf,
    );
    if both {
        right.render(content, buf);
    }
    hit
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::ColorScheme;
    use crate::ui::theme::probe::{bg, fg};

    /// The palette the tests draw in, unless they are about the palettes themselves.
    fn ink() -> &'static Palette {
        ColorScheme::Dark.palette()
    }

    fn metrics(cols: u16, rows: u16) -> Metrics {
        Metrics {
            cols,
            rows,
            px_w: u32::from(cols) * 8,
            px_h: u32::from(rows) * 16,
            cell_w: 8,
            cell_h: 16,
        }
    }

    /// Render the bar and write it out, which is what the chrome's diff does with it. The
    /// buffer is just the bar's row, so what lands in `out` is the row and nothing else.
    fn draw(out: &mut Vec<u8>, m: &Metrics, ink: &Palette, left: Vec<Span>, right: Vec<Span>) {
        let mut buf = Buffer::empty(Rect::new(0, m.rows.saturating_sub(1), m.cols, 1));
        render(&mut buf, m, ink, left, right, vec![]);
        super::super::paint::write_cells(out, &buf);
    }

    /// Draw a plain two-sided bar, for the tests that reason about layout not ink.
    fn draw_str(out: &mut Vec<u8>, m: &Metrics, left: &str, right: &str) {
        let ink = ink();
        draw(out, m, ink, vec![ink.text(left)], vec![ink.text(right)]);
    }

    /// The row as it lands on screen, with the escapes taken out.
    fn body(buf: &[u8], row: u16) -> String {
        let text = String::from_utf8(buf.to_vec()).unwrap();
        let head = format!("\x1b[{row};1H");
        assert!(
            text.starts_with(&head),
            "the line must start at column one of row {row}: {text:?}"
        );
        assert!(
            text.contains(&bg(ink().bar)) && text.contains(&fg(ink().text)),
            "the row carries the bar's own colours, not the terminal's: {text:?}"
        );
        assert!(
            !text.contains("\x1b[7m"),
            "reverse video would follow the theme instead: {text:?}"
        );
        assert!(
            text.ends_with("\x1b[0m"),
            "the attributes must not outlive the row: {text:?}"
        );
        // Everything that is not an escape sequence. They all end in a letter here:
        // a cursor move in `H`, a colour or an attribute in `m`.
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn status_line_fills_the_row_exactly() {
        let m = metrics(40, 10);
        let mut out = Vec::new();
        draw_str(&mut out, &m, "left", "right");
        let body = body(&out, 10);
        assert_eq!(body.chars().count(), 40);
        assert!(body.starts_with("left"));
        assert!(body.ends_with("right"));
    }

    #[test]
    fn status_line_truncates_rather_than_wrapping() {
        let m = metrics(10, 5);
        let mut out = Vec::new();
        draw_str(&mut out, &m, "a very long left side indeed", "stats");
        let body = body(&out, 5);
        assert_eq!(body.chars().count(), 10, "must never exceed the row width");
        assert_eq!(body, "a very lon", "the left side is what survives");
    }

    #[test]
    fn the_statistics_go_before_the_left_side_does() {
        // Both sides fit at forty columns and neither does at twenty, where the left
        // is still short enough to be drawn whole.
        let mut out = Vec::new();
        draw_str(&mut out, &metrics(40, 10), " a-server  1600x832", "60 fps ");
        assert!(body(&out, 10).ends_with("60 fps "));

        out.clear();
        draw_str(&mut out, &metrics(20, 10), " a-server  1600x832", "60 fps ");
        let body = body(&out, 10);
        assert_eq!(body, " a-server  1600x832 ");
        assert!(
            !body.contains("fps"),
            "the statistics should have gone first"
        );
    }

    #[test]
    fn a_wide_glyph_is_measured_by_what_it_covers() {
        // Two cells each, so this name is twelve columns wide and not six. Counting
        // characters would leave room for a right-hand side that cannot fit.
        let m = metrics(20, 10);
        let mut out = Vec::new();
        draw_str(&mut out, &m, "日本語のデス", "12345678");
        let body = body(&out, 10);
        assert!(
            !body.contains("12345678"),
            "twelve columns and eight leave nothing between them: {body:?}"
        );
    }

    #[test]
    fn the_bar_is_grey_but_for_what_is_worth_a_glance() {
        // Two inks and no more: the bar should read as one line with a couple of
        // things lifted out of it, not as a row of competing colours.
        let m = metrics(60, 10);
        let ink = ink();
        let mut out = Vec::new();
        draw(
            &mut out,
            &m,
            ink,
            vec![ink.bright(" a-server"), ink.text("  1600x832  native 1:1")],
            vec![ink.text("23ms  "), ink.bright("F8"), ink.text(" commands ")],
        );
        let text_of = String::from_utf8(out.clone()).unwrap();
        let inks: Vec<&str> = text_of
            .split("\x1b[38;2;")
            .skip(1)
            .filter_map(|s| s.split('m').next())
            .collect();
        assert_eq!(
            inks.iter().collect::<std::collections::BTreeSet<_>>().len(),
            2,
            "expected exactly two inks: {inks:?}"
        );
        assert!(inks.contains(&"128;128;128"), "{inks:?}");
        assert!(inks.contains(&"238;238;238"), "{inks:?}");

        // The name and the key are the bright ones, and they are the only ones.
        let body = body(&out, 10);
        assert!(body.starts_with(" a-server"));
        assert!(body.ends_with("F8 commands "));
    }

    #[test]
    fn degenerate_geometry_draws_nothing() {
        let mut out = Vec::new();
        draw_str(&mut out, &metrics(0, 0), "x", "y");
        assert!(out.is_empty());
    }
}
