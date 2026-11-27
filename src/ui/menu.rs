//! The viewer's local command menu. Its layout also defines the clickable targets.
//!
//! The widgets draw into the shared chrome buffer. The same layout determines
//! which command or option a pointer can select.

use std::ops::Range;

use ratatui::buffer::Buffer;
use ratatui::layout::{Margin, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use super::theme::{Palette, Theme, colour};
use crate::cli::{MenuKey, ScaleMode};
use crate::term::Metrics;
use crate::term::input::Command;

/// Breathing room inside the box, in cells. There is no border to hold the text
/// off the edge, so the padding is the only thing that does.
const PAD_X: u16 = 3;
const PAD_Y: u16 = 1;

/// Least space between a label and the shortcut sitting against the right edge.
const GAP: u16 = 6;

/// Space between two options of a choice row.
const CHOICE_GAP: u16 = 2;

/// One line of the menu.
enum Entry {
    /// The menu title and its clickable close button.
    Title {
        name: &'static str,
        close: &'static str,
    },
    /// A group of commands.
    Section(&'static str),
    /// A command, the keys that reach it, and what a click on it runs.
    Item {
        label: &'static str,
        keys: &'static str,
        command: Command,
    },
    /// A row of alternatives, exactly one of which is in force. Which one that is
    /// arrives at draw time; the row itself only knows what is on offer.
    Choice {
        options: Vec<Option_>,
        keys: &'static str,
    },
    Blank,
}

/// One alternative on a choice row.
///
/// Named awkwardly because `Option` is taken. It is drawn `[name]` when in force
/// and ` name ` when not, so the two are the same width and the geometry does not
/// shift when the selection moves -- which is what lets the hit test agree with the
/// drawing without being told which one is selected.
struct Option_ {
    name: &'static str,
    command: Command,
}

impl Option_ {
    fn width(&self) -> u16 {
        cells(self.name) + 2
    }

    /// Is this the one in force?
    ///
    /// Read off the command rather than stored beside it: an option's command is
    /// precisely the request to be the one in force, so it already says which state
    /// would make it so, and a second copy of that could only disagree.
    fn in_force(&self, state: State) -> bool {
        match self.command {
            Command::Mode(mode) => state.mode == mode,
            Command::Theme(theme) => state.theme == theme,
            _ => false,
        }
    }
}

/// What the menu has to show as being in force.
///
/// Passed in at draw time rather than remembered: the session owns both of these, and
/// a second copy here could only ever drift from the first.
#[derive(Debug, Clone, Copy)]
pub struct State {
    pub mode: ScaleMode,
    pub theme: Theme,
}

/// Every option with the columns it covers, relative to the left of the row.
///
/// The gap after an option belongs to it, so there is no dead strip between two
/// targets; the last one ends where its text does, because everything past it is
/// the shortcut's.
fn option_spans(options: &[Option_]) -> Vec<(&Option_, u16, u16)> {
    let mut out = Vec::with_capacity(options.len());
    let mut x = 0;
    for (i, option) in options.iter().enumerate() {
        let last = i + 1 == options.len();
        let end = x + option.width() + if last { 0 } else { CHOICE_GAP };
        out.push((option, x, end));
        x = end;
    }
    out
}

/// Columns the title's dismissal covers, relative to the left of the row.
///
/// Right-aligned, like every shortcut, so it starts where it ends up rather than at
/// a fixed offset. Drawing and hit testing both come through here, or a click would
/// land next to the button rather than on it.
fn close_span(close: &str, width: u16) -> (u16, u16) {
    (width.saturating_sub(cells(close)), width)
}

/// Columns the options of a choice row occupy together.
fn choice_width(options: &[Option_]) -> u16 {
    match option_spans(options).last() {
        Some((option, start, _)) => start + option.width(),
        None => 0,
    }
}

impl Entry {
    /// Minimum width for the clickable labels, and preferred width with shortcuts.
    fn widths(&self) -> (u16, u16) {
        match self {
            Entry::Blank => (0, 0),
            Entry::Section(text) => (cells(text), cells(text)),
            Entry::Title { name, close } => {
                let width = cells(name) + GAP + cells(close);
                (width, width)
            }
            Entry::Item { label, keys, .. } => (cells(label), cells(label) + GAP + cells(keys)),
            Entry::Choice { options, keys } => {
                let width = choice_width(options);
                (width, width + GAP + cells(keys))
            }
        }
    }

    /// What clicking this line does, `offset` cells in from where its text starts,
    /// on a row `width` cells wide.
    ///
    /// The offset is signed because the padding to the left of the text comes out
    /// negative: it is part of a command's row, which is a target all the way across,
    /// but part of no single option on a row of them. The width is needed because the
    /// title's dismissal is pushed against the right edge, so where it starts depends
    /// on how wide the box turned out.
    fn command_at(&self, offset: i32, width: u16) -> Option<Command> {
        match self {
            Entry::Item { command, .. } => Some(*command),
            Entry::Choice { options, .. } => option_spans(options)
                .into_iter()
                .find(|(_, start, end)| offset >= i32::from(*start) && offset < i32::from(*end))
                .map(|(option, ..)| option.command),
            Entry::Title { close, .. } => {
                let (start, end) = close_span(close, width);
                (offset >= i32::from(start) && offset < i32::from(end)).then_some(Command::Menu)
            }
            _ => None,
        }
    }
}

fn cells(text: &str) -> u16 {
    Span::raw(text).width() as u16
}

/// What sits under the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// Somewhere else entirely: the remote screen, or the status line.
    Outside,
    /// On the menu, but not on anything a click would act on.
    Inside,
    /// On a command.
    Item { index: usize, command: Command },
}

/// The geometry and first entry used to draw one frame of the menu.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    area: Rect,
    inner: Rect,
    first_row: usize,
}

/// The menu and what the pointer is on.
pub struct Menu {
    entries: Vec<Entry>,
    label_width: u16,
    preferred_width: u16,
    first_row: usize,
    /// The entry under the pointer and the command it would run, which is only ever
    /// one a click would act on. The command is kept because a row of options has
    /// several, and the highlight has to know which.
    hover: Option<(usize, Command)>,
}

impl Menu {
    pub fn new(menu_key: MenuKey) -> Self {
        let item = |label, keys, command| Entry::Item {
            label,
            keys,
            command,
        };
        let entries = vec![
            Entry::Title {
                name: "Command menu",
                close: super::CLOSE,
            },
            Entry::Blank,
            Entry::Section("Session"),
            match menu_key {
                MenuKey::F8 => item("Send F8 to remote", "8", Command::SendF8),
                MenuKey::None => item("Close menu", "Esc", Command::Menu),
            },
            item("Quit", "q", Command::Quit),
            Entry::Blank,
            Entry::Section("Screen"),
            item("Refresh in full", "f", Command::FullRefresh),
            item("Renegotiate the remote size", "r", Command::Renegotiate),
            // Give each direction its own click target.
            item("Pan left", "left", Command::Pan(-1, 0)),
            item("Pan right", "right", Command::Pan(1, 0)),
            item("Pan up", "up", Command::Pan(0, -1)),
            item("Pan down", "down", Command::Pan(0, 1)),
            Entry::Blank,
            Entry::Section("Scaling"),
            // One row rather than four commands: the key steps through the modes
            // because a single binding cannot name one, but a click can, so the
            // options are offered side by side with the one in force marked.
            Entry::Choice {
                options: vec![
                    Option_ {
                        name: "Native",
                        command: Command::Mode(ScaleMode::Native),
                    },
                    Option_ {
                        name: "Fit",
                        command: Command::Mode(ScaleMode::Fit),
                    },
                    Option_ {
                        name: "Integer",
                        command: Command::Mode(ScaleMode::Integer),
                    },
                    Option_ {
                        name: "1:1",
                        command: Command::Mode(ScaleMode::OneToOne),
                    },
                ],
                keys: "m",
            },
            Entry::Blank,
            Entry::Section("Theme"),
            // No keys: two of anything is a choice rather than a cycle, and a click
            // names one where a single binding could only alternate.
            Entry::Choice {
                options: vec![
                    Option_ {
                        name: "Dark",
                        command: Command::Theme(Theme::Dark),
                    },
                    Option_ {
                        name: "Light",
                        command: Command::Theme(Theme::Light),
                    },
                ],
                keys: "",
            },
            Entry::Blank,
            Entry::Section("View"),
            item("Toggle view-only", "v", Command::ToggleViewOnly),
            item("Toggle statistics", "c", Command::ToggleStats),
        ];
        let (min_width, preferred_width) = entries
            .iter()
            .map(Entry::widths)
            .fold((0, 0), |(min, preferred), (label, full)| {
                (min.max(label), preferred.max(full))
            });
        Self {
            entries,
            label_width: min_width,
            preferred_width: preferred_width + 2 * PAD_X,
            first_row: 0,
            hover: None,
        }
    }

    /// Where the box goes, in zero-based screen cells, or `None` when the image
    /// area is too small to hold it.
    ///
    /// Everything -- drawing, erasing and hit testing -- comes through here, so
    /// the three cannot disagree about which cells the menu owns.
    pub fn area(&self, metrics: &Metrics) -> Option<Rect> {
        let rows = metrics.image_rows();
        if metrics.cols == 0 || rows == 0 {
            return None;
        }
        let width = metrics.cols.min(self.preferred_width);
        let height = rows.min(self.entries.len() as u16 + PAD_Y * 2);
        Some(Rect {
            x: (metrics.cols - width) / 2,
            y: (rows - height) / 2,
            width,
            height,
        })
    }

    /// Drop vertical padding before clipping any entries. Drawing and hit
    /// testing use the same slice, including after a resize or wheel event.
    fn visible_rows(&self, area: Rect) -> (Rect, Range<usize>) {
        let count = self.entries.len();
        let pad_y = PAD_Y.min(area.height.saturating_sub(count as u16) / 2);
        let pad_x = PAD_X.min(area.width.saturating_sub(self.label_width) / 2);
        let inner = area.inner(Margin::new(pad_x, pad_y));
        let visible = count.min(usize::from(inner.height));
        let start = self.first_row.min(count - visible);
        (inner, start..start + visible)
    }

    pub fn scroll(&mut self, metrics: &Metrics, down: bool) {
        if let Some(area) = self.area(metrics) {
            let (_, rows) = self.visible_rows(area);
            self.first_row = if down {
                (rows.start + 1).min(self.entries.len() - rows.len())
            } else {
                rows.start.saturating_sub(1)
            };
        }
    }

    pub fn reset(&mut self) {
        self.first_row = 0;
        self.clear_hover();
    }

    /// The cells to highlight for the hovered command.
    ///
    /// A command's target is its whole row, and a bar across the row is the answer. An
    /// option's is the option itself, that row holding several, and a bar across all of it
    /// would say the pointer is on things it is not. The title's button gets no bar at all.
    fn hover_bar(&self, area: Rect) -> Option<Rect> {
        let (index, command) = self.hover?;
        let (inner, rows) = self.visible_rows(area);
        if !rows.contains(&index) {
            return None;
        }
        let y = inner.y + (index - rows.start) as u16;
        let span = |start: u16, width: u16| {
            (start < inner.width).then(|| Rect {
                x: inner.x + start,
                y,
                width: width.min(inner.width - start),
                height: 1,
            })
        };
        match self.entries.get(index)? {
            Entry::Choice { options, .. } => {
                let (option, start, _) = option_spans(options)
                    .into_iter()
                    .find(|(option, ..)| option.command == command)?;
                span(start, option.width())
            }
            // The one target with no bar: it is a button in the palette's own styling,
            // which lifts by colour alone, and a bar behind it would be a second mark for
            // one thing -- as well as the one difference between this button and the
            // popup's, which has no bar to give it (see `theme::Palette::close_button`).
            Entry::Title { .. } => None,
            _ => Some(Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            }),
        }
    }

    /// Move the highlight to whatever the pointer is on.
    pub fn set_hover(&mut self, hit: Hit) {
        self.hover = match hit {
            Hit::Item { index, command } => Some((index, command)),
            Hit::Outside | Hit::Inside => None,
        };
    }

    /// Forget the highlight, for a menu that is being put away.
    pub fn clear_hover(&mut self) {
        self.hover = None;
    }

    /// What a pointer at zero-based cell `(col, row)` is on.
    pub fn hit(&self, metrics: &Metrics, col: u16, row: u16) -> Hit {
        self.hit_in(self.viewport(metrics), col, row)
    }

    fn viewport(&self, metrics: &Metrics) -> Option<Viewport> {
        let area = self.area(metrics)?;
        let (inner, rows) = self.visible_rows(area);
        Some(Viewport {
            area,
            inner,
            first_row: rows.start,
        })
    }

    pub fn hit_in(&self, viewport: Option<Viewport>, col: u16, row: u16) -> Hit {
        let Some(Viewport {
            area,
            inner,
            first_row,
        }) = viewport
        else {
            return Hit::Outside;
        };
        if !area.contains(Position::new(col, row)) {
            return Hit::Outside;
        }
        // Above the first entry the subtraction underflows, and below the last one
        // there is no entry to find.
        if row < inner.y || row >= inner.bottom() {
            return Hit::Inside;
        }
        let index = first_row + usize::from(row - inner.y);
        let Some(entry) = self.entries.get(index) else {
            return Hit::Inside;
        };
        // Counted from where the text starts, so the padding comes out negative: it
        // belongs to a command's row, but to no single option on a row of them.
        let offset = i32::from(col) - i32::from(inner.x);
        match entry.command_at(offset, inner.width) {
            Some(command) => Hit::Item { index, command },
            None => Hit::Inside,
        }
    }

    /// Draw the menu and its current selections over the remote screen.
    pub fn render(&self, buf: &mut Buffer, metrics: &Metrics, state: State) -> Option<Viewport> {
        let viewport = self.viewport(metrics)?;
        MenuView {
            menu: self,
            state,
            viewport,
        }
        .render(viewport.area, buf);
        Some(viewport)
    }
}

/// The menu together with what it has to show as in force. A `Widget` takes no
/// arguments beyond the area, and this is the one thing the entries cannot know for
/// themselves.
struct MenuView<'a> {
    menu: &'a Menu,
    state: State,
    viewport: Viewport,
}

impl Widget for MenuView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let ink = self.state.theme.palette();
        let inner = self.viewport.inner;
        let rows = self.viewport.first_row..self.menu.entries.len();
        buf.set_style(area, Style::new().bg(colour(ink.paper)).fg(colour(ink.ink)));

        if let Some(bar) = self.menu.hover_bar(area) {
            buf.set_style(bar, Style::new().bg(colour(ink.hover)));
        }

        for (index, row) in rows.zip(inner.rows()) {
            let entry = &self.menu.entries[index];
            let hovered = |command| self.menu.hover == Some((index, command));

            match entry {
                Entry::Blank => {}
                Entry::Section(text) => {
                    Line::from(Span::styled(*text, Style::new().fg(colour(ink.accent))))
                        .render(row, buf);
                }
                Entry::Title { name, close } => {
                    Line::from(Span::styled(
                        *name,
                        Style::new()
                            .fg(colour(ink.ink))
                            .add_modifier(Modifier::BOLD),
                    ))
                    .render(row, buf);
                    // A target, so it lifts under the pointer like everything else
                    // rather than sitting there as a label that happens to work. The
                    // styling is the palette's rather than this file's, the popup
                    // carrying the same button (see `toast`).
                    let style = ink.close_button(hovered(Command::Menu));
                    Line::from(Span::styled(*close, style))
                        .right_aligned()
                        .render(row, buf);
                }
                Entry::Item {
                    label,
                    keys,
                    command,
                } => {
                    let style = if hovered(*command) {
                        Style::new()
                            .fg(colour(ink.accent))
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::new().fg(colour(ink.ink))
                    };
                    split(buf, row, label, keys, style, ink);
                }
                Entry::Choice { options, keys } => {
                    for (option, start, _) in option_spans(options) {
                        if start >= row.width {
                            break;
                        }
                        // Brackets on the one in force, blanks on the rest, so every
                        // option is the same width whichever is selected and the
                        // options do not shuffle sideways as it changes.
                        let selected = option.in_force(self.state);
                        let mut style = if selected {
                            Style::new()
                                .fg(colour(ink.ink))
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::new().fg(colour(ink.muted))
                        };
                        if hovered(option.command) {
                            style = style
                                .fg(colour(ink.accent))
                                .add_modifier(Modifier::UNDERLINED);
                        }
                        let text = if selected {
                            format!("[{}]", option.name)
                        } else {
                            format!(" {} ", option.name)
                        };
                        let at = Rect {
                            x: row.x + start,
                            width: option.width().min(row.width - start),
                            ..row
                        };
                        Line::from(Span::styled(text, style)).render(at, buf);
                    }
                    if !keys.is_empty() && choice_width(options) + cells(keys) < row.width {
                        Line::from(Span::styled(*keys, Style::new().fg(colour(ink.muted))))
                            .right_aligned()
                            .render(row, buf);
                    }
                }
            }
        }
    }
}

/// A label on the left, its keys pushed against the right edge.
///
/// The colour goes on the spans rather than on the lines, because a `Line` styles
/// the whole row it is given and the second one would repaint the first.
fn split(buf: &mut Buffer, row: Rect, left: &str, right: &str, style: Style, ink: &Palette) {
    Line::from(Span::styled(left, style)).render(row, buf);
    if !right.is_empty() && cells(left) + cells(right) < row.width {
        Line::from(Span::styled(right, Style::new().fg(colour(ink.muted))))
            .right_aligned()
            .render(row, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::chrome::Chrome;

    /// Render the menu the way a frame does -- into the chrome's plane -- and serialise
    /// what came out, so the assertions below can go on reading escapes.
    fn draw_menu(menu: &Menu, out: &mut Vec<u8>, m: &Metrics, state: State) {
        let mut chrome = Chrome::new();
        chrome.begin(m);
        menu.render(chrome.buffer(), m, state);
        chrome.flush(out);
    }
    use crate::cli::ScaleMode;
    use crate::ui::CLOSE;
    use crate::ui::theme::probe::{bg, fg};

    /// A settled state, for the tests that reason about geometry rather than marks.
    fn state() -> State {
        State {
            mode: ScaleMode::Fit,
            theme: Theme::Dark,
        }
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

    /// The menu as it lands on screen: one string per positioned row, with the
    /// escapes and the image payloads taken out.
    fn rendered(buf: &[u8]) -> Vec<String> {
        let text = String::from_utf8(buf.to_vec()).unwrap();
        let mut lines: Vec<String> = Vec::new();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\x1b' => match chars.next() {
                    // CSI: parameters then a letter. `H` starts a fresh row.
                    Some('[') => {
                        for c in chars.by_ref() {
                            if c.is_ascii_alphabetic() {
                                if c == 'H' {
                                    lines.push(String::new());
                                }
                                break;
                            }
                        }
                    }
                    // APC: an image, terminated by ESC backslash.
                    Some('_') => {
                        while let Some(c) = chars.next() {
                            if c == '\x1b' {
                                chars.next();
                                break;
                            }
                        }
                    }
                    _ => {}
                },
                c => {
                    if let Some(line) = lines.last_mut() {
                        line.push(c);
                    }
                }
            }
        }
        while lines.first().is_some_and(String::is_empty) {
            lines.remove(0);
        }
        lines
    }

    /// The rows and columns a buffer moved the cursor to, in order.
    fn positions(buf: &[u8]) -> Vec<(u16, u16)> {
        let text = String::from_utf8(buf.to_vec()).unwrap();
        let mut out = Vec::new();
        for part in text.split("\x1b[").skip(1) {
            if let Some((coords, _)) = part.split_once('H')
                && let Some((r, c)) = coords.split_once(';')
                && let (Ok(r), Ok(c)) = (r.parse::<u16>(), c.parse::<u16>())
            {
                out.push((r, c));
            }
        }
        out
    }

    #[test]
    fn menu_fits_or_is_skipped() {
        let menu = Menu::new(MenuKey::F8);
        let mut out = Vec::new();
        draw_menu(&menu, &mut out, &metrics(100, 40), state());
        assert!(!out.is_empty());

        out.clear();
        draw_menu(&menu, &mut out, &metrics(20, 1), state());
        assert!(out.is_empty(), "must not draw a menu that does not fit");
    }

    #[test]
    fn a_narrow_menu_keeps_the_last_scaling_option_visible_and_clickable() {
        let m = metrics(45, 24);
        let menu = Menu::new(MenuKey::F8);
        let area = menu.area(&m).unwrap();
        let mut out = Vec::new();
        draw_menu(&menu, &mut out, &m, state());
        let lines = rendered(&out);
        let (row, choices) = lines
            .iter()
            .enumerate()
            .find(|(_, line)| line.contains("Native"))
            .unwrap();
        let col = choices
            .find("1:1")
            .expect("shortcut covered the last option");
        assert!(matches!(
            menu.hit(&m, area.x + col as u16, area.y + row as u16),
            Hit::Item {
                command: Command::Mode(ScaleMode::OneToOne),
                ..
            }
        ));
    }

    fn assert_menu_background(buf: &Buffer, area: Rect, hover: Option<Rect>, theme: Theme) {
        let palette = theme.palette();
        for pos in buf.area.positions() {
            let expected = if hover.is_some_and(|rect| rect.contains(pos)) {
                colour(palette.hover)
            } else if area.contains(pos) {
                colour(palette.paper)
            } else {
                ratatui::style::Color::Reset
            };
            assert_eq!(buf[pos].bg, expected, "cell at {pos:?}");
        }
    }

    #[test]
    fn every_cell_inside_the_menu_covers_the_desktop_in_both_themes() {
        let m = Metrics {
            cols: 138,
            rows: 40,
            px_w: 2484,
            px_h: 1600,
            cell_w: 18,
            cell_h: 40,
        };
        let menu = Menu::new(MenuKey::F8);
        let area = menu.area(&m).unwrap();
        for theme in [Theme::Dark, Theme::Light] {
            let mut buf = Buffer::empty(Rect::new(0, 0, m.cols, m.rows));
            menu.render(&mut buf, &m, State { theme, ..state() });
            assert_menu_background(&buf, area, None, theme);
        }
    }

    #[test]
    fn menu_is_padded_and_right_aligns_its_shortcuts() {
        let menu = Menu::new(MenuKey::F8);
        let mut out = Vec::new();
        draw_menu(&menu, &mut out, &metrics(100, 40), state());
        let lines = rendered(&out);

        assert!(!lines.is_empty(), "nothing was drawn");
        let width = lines[0].chars().count();
        assert!(
            lines.iter().all(|l| l.chars().count() == width),
            "every row must fill the box, or the backdrop shows through the gaps"
        );
        assert!(
            !lines.iter().any(|l| l.contains(['┌', '─', '│', '└'])),
            "the box is borderless"
        );
        // Padded top and bottom, and held off both edges.
        assert!(lines[0].trim().is_empty(), "no padding above");
        assert!(lines[lines.len() - 1].trim().is_empty(), "no padding below");
        for line in lines.iter().filter(|l| !l.trim().is_empty()) {
            let left = line.len() - line.trim_start().len();
            let right = line.len() - line.trim_end().len();
            assert!(left >= usize::from(PAD_X), "row is not indented: {line:?}");
            assert!(
                right >= usize::from(PAD_X),
                "row runs into the right edge: {line:?}"
            );
        }
        // Every shortcut ends in the same column.
        for (row, entry) in menu.entries.iter().enumerate() {
            if let Entry::Item { keys, .. } | Entry::Choice { keys, .. } = entry
                && !keys.is_empty()
            {
                let line = lines[row + usize::from(PAD_Y)].trim_end();
                assert_eq!(line.split_whitespace().last(), Some(*keys));
                assert_eq!(line.chars().count(), width - usize::from(PAD_X));
            }
        }
    }

    #[test]
    fn a_menu_absent_from_the_next_frame_blanks_every_cell_it_drew() {
        // Repainting the image cannot undraw the menu, because the image is composited
        // below the text: the cells have to be blanked, over exactly the rectangle that was
        // drawn, or a frame of it survives. Nothing asks for that any more -- the menu is
        // simply not rendered, and the plane's diff works out the rest.
        let mut menu = Menu::new(MenuKey::F8);
        let m = metrics(100, 40);
        // Include a highlighted row in the area to clear.
        let area = menu.area(&m).expect("the menu did not fit");
        menu.set_hover(menu.hit(&m, area.x + PAD_X, area.y + PAD_Y + 3));
        let mut chrome = Chrome::new();

        chrome.begin(&m);
        let mut drawn = Vec::new();
        menu.render(chrome.buffer(), &m, state());
        chrome.flush(&mut drawn);
        chrome.commit();

        // The next frame has no menu in it.
        chrome.begin(&m);
        let mut cleared = Vec::new();
        let vacated = chrome.flush(&mut cleared);
        chrome.commit();

        assert!(!cleared.is_empty(), "nothing was erased");
        let cells = |buf: &[u8]| {
            let mut v = positions(buf);
            v.sort_unstable();
            v.dedup();
            v
        };
        assert_eq!(
            cells(&drawn),
            cells(&cleared),
            "the erase must cover the same cells the menu drew"
        );
        assert_eq!(
            vacated,
            area.positions()
                .map(|p| Rect::new(p.x, p.y, 1, 1))
                .collect::<Vec<_>>(),
            "every menu cell, including the blank padding, should reveal the desktop"
        );
        let text = String::from_utf8(cleared).unwrap();
        let written: String = text
            .split("\x1b[")
            .skip(1)
            .filter_map(|chunk| chunk.split_once('H').map(|(_, body)| body.to_string()))
            .map(|body| body.split('\x1b').next().unwrap_or("").to_string())
            .collect();
        assert!(
            written.chars().all(|c| c == ' '),
            "the erase wrote something other than blanks: {written:?}"
        );
    }

    #[test]
    fn a_menu_that_does_not_fit_leaves_nothing_to_blank() {
        // Otherwise the frame after it would blank a rectangle that was never drawn.
        let m = metrics(20, 1);
        let mut chrome = Chrome::new();
        chrome.begin(&m);
        let mut out = Vec::new();
        Menu::new(MenuKey::F8).render(chrome.buffer(), &m, state());
        chrome.flush(&mut out);
        chrome.commit();

        chrome.begin(&m);
        let mut after = Vec::new();
        assert!(chrome.flush(&mut after).is_empty());
        assert!(after.is_empty(), "nothing was drawn, so nothing is erased");
    }

    #[test]
    fn menu_never_touches_the_status_row() {
        let m = metrics(80, 24);
        let mut out = Vec::new();
        draw_menu(&Menu::new(MenuKey::F8), &mut out, &m, state());
        assert!(!out.is_empty());
        let text = String::from_utf8(out).unwrap();
        // Every cursor move must stay above the last row. Only sequences that
        // actually terminate in `H` are positions: the colour set-up ends in `m`,
        // and its parameters would otherwise be read as a row number.
        for part in text.split("\x1b[").skip(1) {
            if let Some((coords, _)) = part.split_once('H')
                && let Some((r, _)) = coords.split_once(';')
                && let Ok(row) = r.parse::<u16>()
            {
                assert!(row < m.rows, "menu wrote to row {row}");
            }
        }
    }

    /// The cell a label's row sits on, found by drawing and looking.
    fn row_of(menu: &Menu, m: &Metrics, label: &str) -> u16 {
        let area = menu.area(m).expect("the menu must fit");
        let index = menu
            .entries
            .iter()
            .position(|e| matches!(e, Entry::Item { label: l, .. } if *l == label))
            .expect("no such item");
        area.y + PAD_Y + u16::try_from(index).unwrap()
    }

    /// The row of scaling options, and the column its first option starts at.
    fn choice_row(menu: &Menu, m: &Metrics) -> (u16, u16) {
        let area = menu.area(m).expect("the menu must fit");
        let index = menu
            .entries
            .iter()
            .position(|e| matches!(e, Entry::Choice { .. }))
            .expect("no row of options");
        (
            area.y + PAD_Y + u16::try_from(index).unwrap(),
            area.x + PAD_X,
        )
    }

    #[test]
    fn a_click_on_a_row_finds_that_rows_command() {
        let menu = Menu::new(MenuKey::F8);
        let m = metrics(100, 40);
        let area = menu.area(&m).unwrap();
        // The first action sends the reserved F8 to the server.
        assert_eq!(
            menu.hit(&m, area.x, area.y + PAD_Y + 3),
            Hit::Item {
                index: 3,
                command: Command::SendF8
            },
            "the first item must send F8"
        );

        for (label, want) in [
            ("Send F8 to remote", Command::SendF8),
            ("Quit", Command::Quit),
            ("Toggle view-only", Command::ToggleViewOnly),
            ("Refresh in full", Command::FullRefresh),
            // One direction each: a click can only mean one of the four.
            ("Pan left", Command::Pan(-1, 0)),
            ("Pan down", Command::Pan(0, 1)),
            ("Toggle statistics", Command::ToggleStats),
        ] {
            let row = row_of(&menu, &m, label);
            // The whole width of the box is the target, padding included.
            for col in [area.x, area.x + area.width / 2, area.x + area.width - 1] {
                match menu.hit(&m, col, row) {
                    Hit::Item { command, .. } => assert_eq!(command, want, "{label} at {col}"),
                    other => panic!("{label} at column {col} hit {other:?}"),
                }
            }
        }
    }

    #[test]
    fn the_button_in_the_title_is_a_target_and_the_name_beside_it_is_not() {
        let mut menu = Menu::new(MenuKey::F8);
        let m = metrics(100, 40);
        let area = menu.area(&m).unwrap();
        let row = area.y + PAD_Y;
        // The button is pushed against the right edge of the padded area, so it ends
        // there.
        let end = area.x + area.width - PAD_X;
        let dismiss = Hit::Item {
            index: 0,
            command: Command::Menu,
        };
        // One cell of mark and one cell of target: what a click acts on is the cell the
        // button is drawn on, with nothing either side of it standing in.
        assert_eq!(menu.hit(&m, end - 1, row), dismiss, "the button");
        // Neither the name at the other end nor the space between them.
        assert_eq!(menu.hit(&m, area.x + PAD_X, row), Hit::Inside, "the name");
        assert_eq!(menu.hit(&m, end - 2, row), Hit::Inside, "the gap");

        // And it lifts by colour alone: no bar behind it, which would be a second mark for
        // one thing and the one way this button and the popup's -- which has no bar to be
        // given -- could look unlike each other.
        menu.set_hover(menu.hit(&m, end - 1, row));
        assert_eq!(
            menu.hover_bar(area),
            None,
            "the title's button should have no bar"
        );

        let mut out = Vec::new();
        draw_menu(&menu, &mut out, &m, state());
        let text = String::from_utf8(out).unwrap();
        let ink = state().theme.palette();
        assert!(
            !text.contains(&bg(ink.hover)),
            "nothing else is hovered, so nothing should carry the lift: {text:?}"
        );
        // The accent is the sections' colour too, so it is the run carrying the button
        // that has to be found rather than the first run in that ink.
        let lit = text.split(&fg(ink.accent)).any(|run| {
            run.split("\x1b[0m")
                .next()
                .is_some_and(|run| run.contains(CLOSE))
        });
        assert!(lit, "the button should be in the accent: {text:?}");
    }

    #[test]
    fn the_padding_and_the_blank_rows_are_not_commands() {
        let menu = Menu::new(MenuKey::F8);
        let m = metrics(100, 40);
        let area = menu.area(&m).unwrap();
        let col = area.x + area.width / 2;

        // The title, the section headings and the blank lines are inside the box
        // but nothing to click.
        assert_eq!(menu.hit(&m, col, area.y), Hit::Inside, "top padding");
        assert_eq!(
            menu.hit(&m, col, area.bottom() - 1),
            Hit::Inside,
            "bottom padding"
        );
        assert_eq!(menu.hit(&m, col, area.y + PAD_Y), Hit::Inside, "the title");

        // A blank line and a section heading, each a row of its own among the
        // commands, and neither of them anything to click.
        for (kind, is_kind) in [
            (
                "blank",
                (|e| matches!(e, Entry::Blank)) as fn(&Entry) -> bool,
            ),
            ("section", |e| matches!(e, Entry::Section(_))),
        ] {
            let index = menu
                .entries
                .iter()
                .position(is_kind)
                .unwrap_or_else(|| panic!("the menu has no {kind} row"));
            let row = area.y + PAD_Y + u16::try_from(index).unwrap();
            assert_eq!(menu.hit(&m, col, row), Hit::Inside, "the {kind} row");
        }
    }

    #[test]
    fn everything_off_the_box_is_outside() {
        let menu = Menu::new(MenuKey::F8);
        let m = metrics(100, 40);
        let area = menu.area(&m).unwrap();
        for (col, row) in [
            (0, 0),
            (area.x - 1, area.y + PAD_Y + 3),
            (area.right(), area.y + PAD_Y + 3),
            (area.x, area.y - 1),
            (area.x, area.bottom()),
            (area.x, m.rows - 1), // the status line
        ] {
            assert_eq!(menu.hit(&m, col, row), Hit::Outside, "{col},{row}");
        }
        // A menu that does not fit was never drawn, so no cell belongs to it.
        assert_eq!(menu.hit(&metrics(20, 1), 10, 0), Hit::Outside);
    }

    #[test]
    fn moving_or_leaving_a_command_highlights_only_its_current_row() {
        let m = metrics(100, 40);
        let mut menu = Menu::new(MenuKey::F8);
        let area = menu.area(&m).unwrap();
        let mut chrome = Chrome::new();
        for target in [Some("Pan left"), Some("Pan right"), Some("Pan up"), None] {
            let row = target.map(|label| row_of(&menu, &m, label));
            let hit = row.map_or(Hit::Outside, |y| menu.hit(&m, m.cols / 2, y));
            menu.set_hover(hit);
            chrome.begin(&m);
            menu.render(chrome.buffer(), &m, state());
            let bar = row.map(|y| Rect::new(area.x, y, area.width, 1));
            assert_menu_background(chrome.buffer(), area, bar, state().theme);
            let mut out = Vec::new();
            assert!(
                chrome.flush(&mut out).is_empty(),
                "the menu still covers the desktop"
            );
            assert!(
                !out.is_empty(),
                "the changed highlight must reach the terminal"
            );
            chrome.commit();
        }
    }

    /// Every option of the scaling row with the column its text starts at.
    fn options(menu: &Menu, m: &Metrics) -> Vec<(Command, u16, u16)> {
        let index = menu
            .entries
            .iter()
            .position(|e| matches!(e, Entry::Choice { .. }))
            .expect("no row of options");
        options_of(menu, m, index)
    }

    /// The same for whichever row of options is asked for.
    fn options_of(menu: &Menu, m: &Metrics, index: usize) -> Vec<(Command, u16, u16)> {
        let left = menu.area(m).expect("the menu must fit").x + PAD_X;
        let Some(Entry::Choice { options, .. }) = menu.entries.get(index) else {
            panic!("entry {index} is not a row of options");
        };
        option_spans(options)
            .into_iter()
            .map(|(option, start, end)| (option.command, left + start, left + end))
            .collect()
    }

    #[test]
    fn each_scaling_option_is_its_own_target() {
        let menu = Menu::new(MenuKey::F8);
        let m = metrics(100, 40);
        let (row, left) = choice_row(&menu, &m);

        for (want, start, end) in options(&menu, &m) {
            for col in [start, end - 1] {
                match menu.hit(&m, col, row) {
                    Hit::Item { command, .. } => {
                        assert_eq!(command, want, "column {col} of the scaling row");
                    }
                    other => panic!("column {col} of the scaling row hit {other:?}"),
                }
            }
        }

        // The padding to the left of the first option belongs to no option, and
        // neither does the stretch on the right where the shortcut lives. A row of
        // choices is not a target all the way across the way a command's row is.
        assert_eq!(menu.hit(&m, left - 1, row), Hit::Inside, "the left padding");
        let last = options(&menu, &m).last().copied().unwrap();
        assert_eq!(
            menu.hit(&m, last.2, row),
            Hit::Inside,
            "past the last option"
        );
    }

    #[test]
    fn the_option_in_force_is_the_marked_one() {
        let m = metrics(100, 40);
        let menu = Menu::new(MenuKey::F8);
        let (row, _) = choice_row(&menu, &m);

        // Whichever mode is in force wears the brackets, and only that one.
        for (mode, name) in [
            (ScaleMode::Native, "Native"),
            (ScaleMode::Fit, "Fit"),
            (ScaleMode::Integer, "Integer"),
            (ScaleMode::OneToOne, "1:1"),
        ] {
            let mut out = Vec::new();
            draw_menu(&menu, &mut out, &m, State { mode, ..state() });
            let lines = rendered(&out);
            let drawn = &lines[usize::from(row - menu.area(&m).unwrap().y)];
            assert!(drawn.contains(&format!("[{name}]")), "{mode:?}: {drawn:?}");
            assert_eq!(drawn.matches('[').count(), 1, "{mode:?}: {drawn:?}");
            // And the row is the same width whichever it is, or the options would
            // shuffle sideways as the mode changed and the hit test would be looking
            // in the wrong place.
            assert_eq!(
                drawn.chars().count(),
                usize::from(menu.area(&m).unwrap().width)
            );
            assert!(
                drawn.split_whitespace().last() == Some("m"),
                "the key still cycles: {drawn:?}"
            );
        }
    }

    #[test]
    fn the_theme_row_marks_the_one_in_force_and_a_click_names_the_other() {
        let menu = Menu::new(MenuKey::F8);
        let m = metrics(100, 40);
        let index = menu
            .entries
            .iter()
            .rposition(|e| matches!(e, Entry::Choice { .. }))
            .expect("no theme row");
        let area = menu.area(&m).unwrap();
        let row = area.y + PAD_Y + u16::try_from(index).unwrap();

        // Two options, each its own target, and each naming the theme it would put on.
        for (want, start, end) in options_of(&menu, &m, index) {
            for col in [start, end - 1] {
                match menu.hit(&m, col, row) {
                    Hit::Item { command, .. } => {
                        assert_eq!(command, want, "column {col} of the theme row");
                    }
                    other => panic!("column {col} of the theme row hit {other:?}"),
                }
            }
        }

        // The brackets follow whichever is in force, and the row is drawn in it too:
        // choosing a theme shows you the theme.
        for (theme, name) in [(Theme::Dark, "Dark"), (Theme::Light, "Light")] {
            let mut out = Vec::new();
            draw_menu(&menu, &mut out, &m, State { theme, ..state() });
            let text = String::from_utf8(out).unwrap();
            let drawn = &rendered(text.as_bytes())[usize::from(row - area.y)];
            assert!(drawn.contains(&format!("[{name}]")), "{theme:?}: {drawn:?}");
            assert_eq!(drawn.matches('[').count(), 1, "{theme:?}: {drawn:?}");
            assert!(
                text.contains(&bg(theme.palette().paper)),
                "{theme:?}: the box is not in the theme it says is in force"
            );
        }
    }

    #[test]
    fn the_highlight_covers_one_option_not_the_whole_row() {
        // A row of four targets cannot take a bar across all of it: that would say
        // the pointer is on things it is not.
        let m = metrics(100, 40);
        let mut menu = Menu::new(MenuKey::F8);
        let (row, _) = choice_row(&menu, &m);
        let area = menu.area(&m).unwrap();
        let (command, start, _) = options(&menu, &m)[2];

        menu.set_hover(menu.hit(&m, start, row));
        let mut out = Vec::new();
        draw_menu(&menu, &mut out, &m, state());
        let text = String::from_utf8(out).unwrap();

        let bar = menu.hover_bar(area).expect("nothing highlighted");
        assert_eq!(bar.x, start, "the bar does not start at the option");
        assert_eq!(bar.width, cells("Integer") + 2);
        assert!(bar.width < area.width, "the bar spans the whole row");
        assert_eq!(command, Command::Mode(ScaleMode::Integer));
        let mut buf = Buffer::empty(Rect::new(0, 0, m.cols, m.rows));
        menu.render(&mut buf, &m, state());
        assert_menu_background(&buf, area, Some(bar), state().theme);

        // The option is underlined as well as barred. Nothing else in the box
        // underlines anything, so this is the one cue that survives a terminal with
        // no graphics protocol *and* no colour.
        assert!(text.contains("\x1b[4m"), "the option is not underlined");
        let mut plain = Vec::new();
        menu.clear_hover();
        draw_menu(&menu, &mut plain, &m, state());
        assert!(!String::from_utf8(plain).unwrap().contains("\x1b[4m"));
    }

    #[test]
    fn only_commands_can_be_hovered() {
        let m = metrics(100, 40);
        let mut menu = Menu::new(MenuKey::F8);
        let col = m.cols / 2;
        menu.set_hover(menu.hit(&m, col, row_of(&menu, &m, "Quit")));
        assert!(menu.hover.is_some());
        // A heading under the pointer clears it rather than highlighting a row no
        // click would act on.
        menu.set_hover(menu.hit(&m, col, menu.area(&m).unwrap().y + PAD_Y));
        assert_eq!(menu.hover, None);
    }
}
