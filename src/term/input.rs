//! Terminal input to remote input.
//!
//! Two things make this more than a lookup table.
//!
//! **Releases.** The remote wants a down and an up for every key. The Kitty keyboard
//! protocol provides both; a terminal without it reports only presses, so the
//! release has to be synthesised straight after. Which of the two we are dealing
//! with is settled by the capability probe rather than guessed at from traffic.
//!
//! **Never release what was not pressed.** Every release is checked against the
//! set of keys we actually sent a press for. This keeps local menu actions
//! from sending stray releases to the server.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};

use super::Metrics;
use super::events::KeyAlternates;
use super::keysym::{bitmask, keysym, keysym_for_char};
use crate::cli::{MenuKey, ScaleMode};
use crate::remote::{Input, Key, Pointer};
use crate::render::Layout;
use crate::ui::theme::Theme;

/// Shortest gap between two motion reports, about 60 a second.
///
/// A pixel-reporting terminal can produce motion far faster than any desktop needs,
/// and every report is a packet. noVNC uses the same 17ms. Button and wheel events
/// are never rate limited, and the last position is always flushed, so nothing is
/// lost -- only intermediate positions the remote would not have noticed.
const MOTION_INTERVAL: Duration = Duration::from_millis(17);

/// Pointer button bits, from RFC 6143 section 7.5.5, which is how they cross the
/// seam in [`Pointer`].
mod button {
    pub const LEFT: u8 = 1 << 0;
    pub const MIDDLE: u8 = 1 << 1;
    pub const RIGHT: u8 = 1 << 2;
    pub const WHEEL_UP: u8 = 1 << 3;
    pub const WHEEL_DOWN: u8 = 1 << 4;
    pub const WHEEL_LEFT: u8 = 1 << 5;
    pub const WHEEL_RIGHT: u8 = 1 << 6;
}

/// A local viewer action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Quit,
    FullRefresh,
    Renegotiate,
    /// Step to the next scaling mode.
    CycleMode,
    /// Go straight to one scaling mode. No key reaches this: one binding cannot
    /// name every mode, which is why the key cycles instead. A pointer can name
    /// one, so the menu offers them as a row to choose from.
    Mode(ScaleMode),
    /// Step the percentage zoom up (`+1`) or down (`-1`) through the presets.
    StepZoom(i8),
    Pan(i32, i32),
    ToggleViewOnly,
    ToggleStats,
    /// Toggle between the OS pointer alone and drawing the remote cursor bitmap.
    ToggleRemoteCursor,
    /// Explicitly pick local-only (`false`) or remote-overlay (`true`) cursor.
    RemoteCursor(bool),
    /// Show the command menu, or put it away if it is already up.
    Menu,
    /// Wear a palette. No key reaches this either: it is a choice rather than a step,
    /// so the menu offers the two side by side and a click names one.
    Theme(Theme),
    /// Tap the reserved menu key on the remote, by explicit request.
    SendF8,
}

/// The local lock-key state, as far as the terminal will say.
///
/// `None` means "no idea": a terminal without the Kitty keyboard protocol never
/// mentions lock keys, and an unset bit is then indistinguishable from the key being
/// off. Only when the protocol is in use does absence mean off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LockState {
    pub caps: Option<bool>,
    pub num: Option<bool>,
}

#[derive(Clone, Copy)]
struct PressedKey {
    keysym: u32,
    /// A repeat can omit the alternate that was present on the original press.
    alternates: KeyAlternates,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputFocus {
    Remote,
    Menu,
    ViewOnly,
}

pub struct InputMapper {
    menu_key: MenuKey,
    /// The terminal reports key releases, so we must not invent them.
    expect_releases: bool,
    /// Pointer positions arrive in pixels rather than cell coordinates.
    pixel_mouse: bool,
    /// Keysyms currently pressed as far as the server is concerned.
    held: HashSet<u32>,
    /// The last keysym sent for each non-modifier key, indexed by its base
    /// keysym and whether it belongs to the keypad.
    pressed_keys: HashMap<(u32, bool), PressedKey>,
    /// Modifiers inferred from a chord that lacked a separate modifier press.
    synthesised: KeyModifiers,
    /// Modifiers for which this terminal reports physical transitions, even
    /// if the press of the current chord happened before focus returned.
    reported_modifiers: KeyModifiers,
    /// Pointer buttons currently down.
    buttons: u8,
    /// Last position sent, to drop duplicate motion.
    last_position: Option<(u16, u16)>,
    /// When the last motion report went out.
    last_motion: Option<Instant>,
    /// A position held back by the rate limit, waiting to be flushed.
    pending_motion: Option<(u16, u16)>,
}

impl InputMapper {
    pub fn new(menu_key: MenuKey, expect_releases: bool, pixel_mouse: bool) -> Self {
        Self {
            menu_key,
            expect_releases,
            pixel_mouse,
            held: HashSet::new(),
            pressed_keys: HashMap::new(),
            synthesised: KeyModifiers::NONE,
            reported_modifiers: KeyModifiers::NONE,
            buttons: 0,
            last_position: None,
            last_motion: None,
            pending_motion: None,
        }
    }

    pub fn menu_key(&self) -> MenuKey {
        self.menu_key
    }

    /// The lock-key state this event reports, if the terminal reports any.
    pub fn lock_state(&self, ev: &KeyEvent) -> LockState {
        if !self.expect_releases {
            // No Kitty keyboard protocol, so no lock reporting either, and an absent
            // bit would be read as "off" when it only means "unsaid".
            return LockState::default();
        }
        LockState {
            caps: Some(ev.state.contains(KeyEventState::CAPS_LOCK)),
            num: Some(ev.state.contains(KeyEventState::NUM_LOCK)),
        }
    }

    fn local_command(&self, ev: KeyEvent, focus: InputFocus) -> Option<Command> {
        if ev.kind != KeyEventKind::Press {
            return None;
        }
        if self.menu_key == MenuKey::F8 && ev.modifiers.is_empty() && ev.code == KeyCode::F(8) {
            return Some(Command::Menu);
        }
        if focus != InputFocus::Menu {
            return None;
        }
        // `+` arrives with SHIFT on layouts where it shares a key with `=`.
        if matches!(ev.code, KeyCode::Char('+') | KeyCode::Char('='))
            && (ev.modifiers.is_empty() || ev.modifiers == KeyModifiers::SHIFT)
        {
            return Some(Command::StepZoom(1));
        }
        if !ev.modifiers.is_empty() {
            return None;
        }
        Some(match ev.code {
            KeyCode::Esc => Command::Menu,
            KeyCode::Char('8') if self.menu_key == MenuKey::F8 => Command::SendF8,
            KeyCode::Char('q') => Command::Quit,
            KeyCode::Char('f') => Command::FullRefresh,
            KeyCode::Char('r') => Command::Renegotiate,
            KeyCode::Char('m') => Command::CycleMode,
            KeyCode::Char('-') => Command::StepZoom(-1),
            KeyCode::Char('v') => Command::ToggleViewOnly,
            KeyCode::Char('c') => Command::ToggleStats,
            KeyCode::Char('p') => Command::ToggleRemoteCursor,
            KeyCode::Left => Command::Pan(-1, 0),
            KeyCode::Right => Command::Pan(1, 0),
            KeyCode::Up => Command::Pan(0, -1),
            KeyCode::Down => Command::Pan(0, 1),
            _ => return None,
        })
    }

    /// Local commands still work in the menu and in view-only mode. Only a key
    /// whose press reached the remote can later repeat or release there.
    pub fn on_key(
        &mut self,
        mut ev: KeyEvent,
        alternates: KeyAlternates,
        focus: InputFocus,
    ) -> (Vec<Key>, Option<Command>) {
        let mut out = Vec::new();
        if let Some(command) = self.local_command(ev, focus) {
            self.sync_modifiers(&mut out, KeyModifiers::NONE);
            return (out, Some(command));
        }

        if focus != InputFocus::Remote {
            ev.modifiers &= self.synthesised;
        }
        match keysym(ev.code) {
            Some(sym) if matches!(ev.code, KeyCode::Modifier(_)) => {
                self.forward_modifier(&mut out, sym, ev, focus);
            }
            Some(base) if focus == InputFocus::Remote || ev.kind == KeyEventKind::Release => {
                self.forward_key(&mut out, base, ev, alternates);
            }
            _ => self.sync_modifiers(&mut out, self.synthesised & ev.modifiers),
        }
        (out, None)
    }

    fn forward_key(
        &mut self,
        out: &mut Vec<Key>,
        base: u32,
        ev: KeyEvent,
        mut alternates: KeyAlternates,
    ) {
        let keypad = ev.state.contains(KeyEventState::KEYPAD);
        let id = (base, keypad);
        if ev.kind == KeyEventKind::Release {
            if let Some(previous) = self.pressed_keys.remove(&id) {
                // The mask describes the instant of key-up. A modifier that
                // vanished since key-down must already be up on the remote.
                self.sync_modifiers(out, ev.modifiers);
                self.release_if_unowned(out, previous.keysym);
                // A mask-only terminal may send nothing after a chord's last up.
                if self.pressed_keys.is_empty() {
                    self.sync_modifiers(out, self.synthesised & self.reported_modifiers);
                }
            } else {
                self.sync_modifiers(out, self.synthesised & ev.modifiers);
            }
            return;
        }
        let previous = self.pressed_keys.get(&id).copied();
        if ev.kind == KeyEventKind::Repeat && previous.is_none() {
            self.sync_modifiers(out, self.synthesised & ev.modifiers);
            return;
        }

        if ev.kind == KeyEventKind::Repeat
            && let Some(key) = previous
        {
            alternates.shifted = alternates.shifted.or(key.alternates.shifted);
            alternates.base_layout = alternates.base_layout.or(key.alternates.base_layout);
        }
        let sym = match ev.code {
            KeyCode::Char(c) if !keypad => keysym_for_char(character(ev, c, alternates)),
            _ => base,
        };
        if self.expect_releases {
            self.pressed_keys.insert(
                id,
                PressedKey {
                    keysym: sym,
                    alternates,
                },
            );
            if let Some(old) = previous
                && old.keysym != sym
            {
                self.release_if_unowned(out, old.keysym);
            }
        }
        self.sync_modifiers(out, ev.modifiers);
        self.press(out, sym);
        if !self.expect_releases {
            self.release(out, sym);
        }
    }

    fn release_if_unowned(&mut self, out: &mut Vec<Key>, sym: u32) {
        if !self.pressed_keys.values().any(|key| key.keysym == sym) {
            self.release(out, sym);
        }
    }

    fn forward_modifier(&mut self, out: &mut Vec<Key>, sym: u32, ev: KeyEvent, focus: InputFocus) {
        let Some((flag, left, right)) = bitmask::MODIFIERS
            .into_iter()
            .find(|&(_, left, right)| sym == left || sym == right)
        else {
            return;
        };
        if self.expect_releases {
            self.reported_modifiers.insert(flag);
        }
        if focus != InputFocus::Remote && ev.kind != KeyEventKind::Release {
            self.sync_modifiers(out, self.synthesised & ev.modifiers);
            return;
        }
        // The physical transition owns this bit. Other bits describe the state
        // at the time of the event, even when the transition itself is redundant.
        let inferred = self.synthesised.contains(flag);
        let was_forwarded = self.held.contains(&sym) || inferred;
        let want = if ev.kind == KeyEventKind::Press || was_forwarded {
            ev.modifiers
        } else {
            self.synthesised & ev.modifiers
        };
        self.sync_modifiers(out, (want - flag) | (self.synthesised & flag));
        if ev.kind == KeyEventKind::Repeat && !inferred {
            return;
        }
        self.synthesised.remove(flag);
        let other = if sym == left { right } else { left };
        if ev.kind == KeyEventKind::Release {
            // On key-up an active bit identifies the other physical side.
            // Press it first so the remote never briefly loses the modifier.
            if ev.modifiers.contains(flag) {
                self.hold_modifier(out, other);
            } else {
                self.release(out, other);
            }
            self.release(out, sym);
            return;
        }
        // A new physical down while the bit was already active identifies
        // the opposite side as the earlier hold. A repeat identifies the
        // already-held side itself.
        let new_press = ev.kind == KeyEventKind::Press && self.expect_releases;
        if inferred && new_press {
            self.hold_modifier(out, other);
        }
        self.hold_modifier(out, sym);
        if inferred && !new_press && left != sym {
            self.release(out, left);
        }
        if !self.expect_releases {
            self.release(out, sym);
        }
    }

    fn hold_modifier(&mut self, out: &mut Vec<Key>, sym: u32) {
        if !self.held.contains(&sym) {
            self.press(out, sym);
        }
    }

    /// Supply only modifiers that the terminal hasn't already pressed explicitly.
    fn sync_modifiers(&mut self, out: &mut Vec<Key>, want: KeyModifiers) {
        for (flag, left, _) in bitmask::MODIFIERS {
            if self.synthesised.contains(flag) && !want.contains(flag) {
                self.release(out, left);
                self.synthesised.remove(flag);
            }
        }
        for (flag, left, right) in bitmask::MODIFIERS {
            if want.contains(flag) && !self.held.contains(&left) && !self.held.contains(&right) {
                self.press(out, left);
                self.synthesised.insert(flag);
            }
        }
    }

    fn press(&mut self, out: &mut Vec<Key>, sym: u32) {
        self.held.insert(sym);
        out.push(Key {
            keysym: sym,
            down: true,
        });
    }

    /// Release a key, but only if we told the server it was down.
    fn release(&mut self, out: &mut Vec<Key>, sym: u32) {
        if self.held.remove(&sym) {
            out.push(Key {
                keysym: sym,
                down: false,
            });
        }
    }

    /// Release everything still held. Called when leaving, losing focus, or
    /// reconnecting: a modifier left down on the remote outlives us.
    pub fn release_all(&mut self) -> Vec<Key> {
        let mut out: Vec<_> = self
            .held
            .drain()
            .map(|sym| Key {
                keysym: sym,
                down: false,
            })
            .collect();
        // Deterministic order makes the traffic reproducible in tests and logs.
        out.sort_by_key(|e| e.keysym);
        self.synthesised = KeyModifiers::NONE;
        self.pressed_keys.clear();
        out
    }

    /// The terminal pixel this event happened at.
    ///
    /// Mode 1016 reports pixels in the same framing as 1006, so crossterm's column and
    /// row *are* pixels. Without it, the middle of the cell is as close as cell
    /// resolution can get.
    pub fn terminal_pixel(&self, ev: &MouseEvent, metrics: &Metrics) -> (u32, u32) {
        if self.pixel_mouse {
            (u32::from(ev.column), u32::from(ev.row))
        } else {
            (
                u32::from(ev.column) * metrics.cell_w + metrics.cell_w / 2,
                u32::from(ev.row) * metrics.cell_h + metrics.cell_h / 2,
            )
        }
    }

    /// The zero-based cell this event happened on.
    ///
    /// Through the pixel position rather than straight from the event, because with
    /// mode 1016 in force the event carries pixels, and the chrome is addressed in
    /// cells. The round trip is exact for a terminal without it: the middle of a
    /// cell divides back to the cell it came from.
    pub fn terminal_cell(&self, ev: &MouseEvent, metrics: &Metrics) -> (u16, u16) {
        let (x, y) = self.terminal_pixel(ev, metrics);
        (
            (x / metrics.cell_w.max(1)).min(u32::from(u16::MAX)) as u16,
            (y / metrics.cell_h.max(1)).min(u32::from(u16::MAX)) as u16,
        )
    }

    /// Translate a mouse report over the drawn image, including the modifiers
    /// in effect when its button or motion reaches the server.
    pub fn on_mouse(&mut self, ev: MouseEvent, layout: &Layout, metrics: &Metrics) -> Vec<Input> {
        // A menu item can close on button-down. Its drag and release still
        // belong to the menu, including any modifier bits they carry.
        if let MouseEventKind::Drag(button) | MouseEventKind::Up(button) = ev.kind
            && self.buttons & button_bit(button) == 0
        {
            return Vec::new();
        }
        let (tx, ty) = self.terminal_pixel(&ev, metrics);
        let mut position = layout.terminal_px_to_src(tx, ty);
        if position.is_none() && matches!(ev.kind, MouseEventKind::Up(_)) {
            // A gesture that began on the desktop can end on the footer.
            position = self.pending_motion.or(self.last_position);
        }
        let Some((x, y)) = position else {
            return Vec::new();
        };

        // SGR mouse has no fields for Super, Hyper or Meta.
        let reported = KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL;
        let mut keys = Vec::new();
        self.sync_modifiers(&mut keys, (self.synthesised - reported) | ev.modifiers);
        let motion = matches!(ev.kind, MouseEventKind::Drag(_) | MouseEventKind::Moved);
        let mut out = Vec::new();
        if (!motion || !keys.is_empty())
            && let Some(pointer) = self.take_pending_motion()
        {
            out.push(Input::Pointer(pointer));
        }
        out.extend(keys.into_iter().map(Input::Key));
        match ev.kind {
            MouseEventKind::Down(b) => self.buttons |= button_bit(b),
            MouseEventKind::Up(b) => self.buttons &= !button_bit(b),
            MouseEventKind::Drag(_) | MouseEventKind::Moved => {
                if self.last_position == Some((x, y)) {
                    self.pending_motion = None;
                    return out;
                }
                if let Some(last) = self.last_motion
                    && last.elapsed() < MOTION_INTERVAL
                {
                    self.pending_motion = Some((x, y));
                    return out;
                }
                self.pending_motion = None;
                self.last_motion = Some(Instant::now());
            }
            MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => {
                let wheel = match ev.kind {
                    MouseEventKind::ScrollUp => button::WHEEL_UP,
                    MouseEventKind::ScrollDown => button::WHEEL_DOWN,
                    MouseEventKind::ScrollLeft => button::WHEEL_LEFT,
                    _ => button::WHEEL_RIGHT,
                };
                out.push(Input::Pointer(Pointer {
                    x,
                    y,
                    buttons: self.buttons | wheel,
                }));
            }
        }

        self.last_position = Some((x, y));
        out.push(Input::Pointer(Pointer {
            x,
            y,
            buttons: self.buttons,
        }));
        out
    }

    /// Send a position the rate limit held back, if enough time has passed.
    pub fn flush_motion(&mut self) -> Option<Pointer> {
        if let Some(last) = self.last_motion
            && last.elapsed() < MOTION_INTERVAL
        {
            return None;
        }
        self.take_pending_motion()
    }

    /// Flush motion before a key or button changes how the remote interprets it.
    pub fn take_pending_motion(&mut self) -> Option<Pointer> {
        let (x, y) = self.pending_motion.take()?;
        self.last_motion = Some(Instant::now());
        self.last_position = Some((x, y));
        Some(Pointer {
            x,
            y,
            buttons: self.buttons,
        })
    }

    /// Let go of every pointer button, for the same reason as `release_all`.
    pub fn release_buttons(&mut self) -> Option<Pointer> {
        self.pending_motion = None;
        if self.buttons == 0 {
            return None;
        }
        self.buttons = 0;
        let (x, y) = self.last_position.unwrap_or((0, 0));
        Some(Pointer { x, y, buttons: 0 })
    }
}

/// Kitty reports the unshifted key in every transition. Some terminals omit
/// alternate key codes even when requested, including for Ctrl+Shift+letter.
fn character(ev: KeyEvent, base: char, alternates: KeyAlternates) -> char {
    let shift = ev.modifiers.contains(KeyModifiers::SHIFT);
    let caps = ev.state.contains(KeyEventState::CAPS_LOCK);
    if shift && let Some(alternate) = alternates.shifted {
        if caps
            && base.is_lowercase()
            && (base.to_uppercase().eq([alternate]) || alternate.to_lowercase().eq([base]))
        {
            return base;
        }
        return alternate;
    }
    if shift ^ caps && base.is_lowercase() {
        // RFB accepts one keysym per key. A multi-character case mapping cannot
        // describe the key (for example, the uppercase expansion of ß).
        let mut upper = base.to_uppercase();
        if let (Some(c), None) = (upper.next(), upper.next()) {
            return c;
        }
    }
    match (shift, base, alternates.base_layout) {
        // Without a shifted code, the PC-101 positions of French AZERTY minus
        // and underscore distinguish its number row from the US minus key.
        (true, '-', Some('6')) => '6',
        (true, '_', Some('8')) => '8',
        (true, '-', None | Some('-')) => '_',
        _ => base,
    }
}

fn button_bit(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => button::LEFT,
        MouseButton::Middle => button::MIDDLE,
        MouseButton::Right => button::RIGHT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::Layout;
    use InputFocus::{Menu, Remote};

    const NO_ALTERNATES: KeyAlternates = KeyAlternates {
        shifted: None,
        base_layout: None,
    };

    fn key(code: KeyCode, kind: KeyEventKind, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        key(code, KeyEventKind::Press, KeyModifiers::NONE)
    }

    fn release(code: KeyCode) -> KeyEvent {
        key(code, KeyEventKind::Release, KeyModifiers::NONE)
    }

    fn ctrl(c: char, kind: KeyEventKind) -> KeyEvent {
        key(KeyCode::Char(c), kind, KeyModifiers::CONTROL)
    }

    fn transitions(keys: Vec<Key>) -> Vec<(u32, bool)> {
        keys.into_iter().map(|k| (k.keysym, k.down)).collect()
    }

    #[test]
    fn a_local_f8_press_cannot_repeat_remotely_after_shift_changes() {
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        let f8 = KeyCode::F(8);
        assert_eq!(
            input.on_key(press(f8), NO_ALTERNATES, Remote),
            (vec![], Some(Command::Menu))
        );
        assert_eq!(
            input.on_key(press(KeyCode::Esc), NO_ALTERNATES, Menu).1,
            Some(Command::Menu)
        );
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            assert_eq!(
                input.on_key(key(f8, kind, KeyModifiers::SHIFT), NO_ALTERNATES, Remote),
                (vec![], None)
            );
        }
        assert!(input.release_all().is_empty());
    }

    #[test]
    fn a_keypad_press_does_not_authorise_a_top_row_repeat() {
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        let mut keypad = press(KeyCode::Char('1'));
        keypad.state.insert(KeyEventState::KEYPAD);
        input.on_key(keypad, NO_ALTERNATES, Remote);
        assert_eq!(
            input.on_key(ctrl('1', KeyEventKind::Repeat), NO_ALTERNATES, Remote),
            (vec![], None)
        );
        keypad.kind = KeyEventKind::Repeat;
        let (keys, _) = input.on_key(keypad, NO_ALTERNATES, Remote);
        assert_eq!(transitions(keys), [(u32::from('1'), true)]);
    }

    #[test]
    fn a_physical_shift_report_releases_stale_inferred_alt_first() {
        use crossterm::event::ModifierKeyCode::LeftShift;
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        input.on_key(
            key(KeyCode::Char('v'), KeyEventKind::Press, KeyModifiers::ALT),
            NO_ALTERNATES,
            Remote,
        );
        let (keys, _) = input.on_key(
            key(
                KeyCode::Modifier(LeftShift),
                KeyEventKind::Press,
                KeyModifiers::SHIFT,
            ),
            NO_ALTERNATES,
            Remote,
        );
        assert_eq!(
            transitions(keys),
            [(bitmask::ALT, false), (bitmask::SHIFT, true)]
        );
    }

    #[test]
    fn a_shared_keysym_stays_down_when_a_keypad_release_clears_control() {
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        let minus = KeyCode::Char('-');
        input.on_key(press(minus), NO_ALTERNATES, Remote);
        let mut keypad = key(minus, KeyEventKind::Press, KeyModifiers::CONTROL);
        keypad.state.insert(KeyEventState::KEYPAD);
        input.on_key(keypad, NO_ALTERNATES, Remote);
        keypad.kind = KeyEventKind::Release;
        keypad.modifiers = KeyModifiers::NONE;
        let (keys, _) = input.on_key(keypad, NO_ALTERNATES, Remote);
        assert_eq!(transitions(keys), [(bitmask::CONTROL, false)]);
        let (keys, _) = input.on_key(release(minus), NO_ALTERNATES, Remote);
        assert_eq!(transitions(keys), [(u32::from('-'), false)]);
        assert!(input.release_all().is_empty());
    }

    #[test]
    fn an_unforwarded_key_up_can_update_the_modifier_of_a_drag() {
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 10,
            modifiers: KeyModifiers::CONTROL,
        };
        input.on_mouse(click, &native_layout(), &metrics());
        let (keys, _) = input.on_key(ctrl('z', KeyEventKind::Release), NO_ALTERNATES, Remote);
        assert!(keys.is_empty());
        let (keys, _) = input.on_key(release(KeyCode::Char('z')), NO_ALTERNATES, Remote);
        assert_eq!(transitions(keys), [(bitmask::CONTROL, false)]);
    }

    fn pointers(events: Vec<Input>) -> Vec<Pointer> {
        events
            .into_iter()
            .map(|event| match event {
                Input::Pointer(pointer) => pointer,
                other => panic!("unexpected input {other:?}"),
            })
            .collect()
    }

    fn metrics() -> Metrics {
        Metrics {
            cols: 200,
            rows: 50,
            px_w: 1600,
            px_h: 850,
            cell_w: 8,
            cell_h: 17,
        }
    }

    fn native_layout() -> Layout {
        let m = metrics();
        let (w, h) = m.image_area();
        Layout::compute(&m, ScaleMode::Native, w, h, (0, 0))
    }

    #[test]
    fn a_key_press_and_release_pass_straight_through() {
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        assert_eq!(
            input.on_key(press(KeyCode::Char('x')), NO_ALTERNATES, Remote),
            (
                vec![Key {
                    keysym: 0x78,
                    down: true
                }],
                None
            )
        );
        assert_eq!(
            input.on_key(release(KeyCode::Char('x')), NO_ALTERNATES, Remote),
            (
                vec![Key {
                    keysym: 0x78,
                    down: false
                }],
                None
            )
        );
    }

    #[test]
    fn a_terminal_without_releases_gets_a_synthesised_one() {
        let mut input = InputMapper::new(MenuKey::F8, false, true);
        let (events, _) = input.on_key(press(KeyCode::Char('x')), NO_ALTERNATES, Remote);
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(events[0].down);
        assert!(!events[1].down);
        assert_eq!(events[0].keysym, events[1].keysym);
    }

    #[test]
    fn a_press_only_terminal_sends_shift_and_the_shifted_letter() {
        let mut input = InputMapper::new(MenuKey::F8, false, true);
        let (events, _) = input.on_key(
            key(KeyCode::Char('c'), KeyEventKind::Press, KeyModifiers::SHIFT),
            NO_ALTERNATES,
            Remote,
        );
        // Shift down, C down, C up. Shift stays down until it is reported gone.
        assert_eq!(
            events[0],
            Key {
                keysym: bitmask::SHIFT,
                down: true
            }
        );
        assert_eq!(events[1].keysym, u32::from('C'));
        assert!(!events[2].down);

        // Now without shift: it has to be released.
        let (events, _) = input.on_key(press(KeyCode::Char('c')), NO_ALTERNATES, Remote);
        assert_eq!(
            events[0],
            Key {
                keysym: bitmask::SHIFT,
                down: false
            }
        );
    }

    #[test]
    fn a_terminal_with_the_keyboard_protocol_sends_its_own_modifier_keys() {
        // The modifier arrives as a key in its own right, so inferring it from the
        // bitmask as well would press it twice.
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        let (events, _) = input.on_key(
            key(
                KeyCode::Modifier(crossterm::event::ModifierKeyCode::LeftControl),
                KeyEventKind::Press,
                KeyModifiers::CONTROL,
            ),
            NO_ALTERNATES,
            Remote,
        );
        assert_eq!(
            events,
            vec![Key {
                keysym: 0xffe3,
                down: true
            }]
        );

        let (events, _) = input.on_key(
            key(
                KeyCode::Char('c'),
                KeyEventKind::Press,
                KeyModifiers::CONTROL,
            ),
            NO_ALTERNATES,
            Remote,
        );
        assert_eq!(
            events,
            vec![Key {
                keysym: 0x63,
                down: true
            }],
            "the modifier must not be pressed a second time"
        );
    }

    #[test]
    fn a_physical_repeat_identifies_the_side_of_an_inferred_modifier() {
        use crossterm::event::ModifierKeyCode;

        let mut input = InputMapper::new(MenuKey::F8, true, true);
        input.on_key(ctrl('v', KeyEventKind::Press), NO_ALTERNATES, Remote);
        input.on_key(ctrl('c', KeyEventKind::Press), NO_ALTERNATES, Remote);
        assert_eq!(
            input.on_key(ctrl('v', KeyEventKind::Release), NO_ALTERNATES, Remote),
            (
                vec![Key {
                    keysym: u32::from('v'),
                    down: false
                }],
                None
            )
        );
        let right = KeyCode::Modifier(ModifierKeyCode::RightControl);
        assert_eq!(
            input.on_key(
                key(right, KeyEventKind::Repeat, KeyModifiers::CONTROL),
                NO_ALTERNATES,
                Remote
            ),
            (
                vec![
                    Key {
                        keysym: 0xffe4,
                        down: true
                    },
                    Key {
                        keysym: bitmask::CONTROL,
                        down: false
                    },
                ],
                None
            )
        );
        assert_eq!(
            input.on_key(ctrl('c', KeyEventKind::Release), NO_ALTERNATES, Remote),
            (
                vec![Key {
                    keysym: u32::from('c'),
                    down: false
                }],
                None
            )
        );
        assert_eq!(
            input.on_key(release(right), NO_ALTERNATES, Remote),
            (
                vec![Key {
                    keysym: 0xffe4,
                    down: false
                }],
                None
            )
        );
        assert!(input.release_all().is_empty());
    }

    #[test]
    fn caps_lock_inverts_the_shifted_case_of_a_kitty_letter() {
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        for (base, shift, shifted, expected) in [
            ('v', false, None, 'V'),
            ('v', true, None, 'v'),
            ('ä', false, None, 'Ä'),
            ('ä', true, Some('Ä'), 'ä'),
            ('ä', true, None, 'ä'),
            ('ς', true, Some('Σ'), 'ς'),
            ('é', true, Some('2'), '2'),
            ('ß', false, None, 'ß'),
            ('ß', true, Some('ẞ'), 'ß'),
            ('1', false, None, '1'),
        ] {
            let mods = if shift {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            let mut ev = key(KeyCode::Char(base), KeyEventKind::Press, mods);
            ev.state = KeyEventState::CAPS_LOCK;
            let (events, _) = input.on_key(
                ev,
                KeyAlternates {
                    shifted,
                    base_layout: None,
                },
                Remote,
            );
            assert_eq!(events.last().unwrap().keysym, keysym_for_char(expected));
            let (events, _) = input.on_key(release(KeyCode::Char(base)), NO_ALTERNATES, Remote);
            let char_up = Key {
                keysym: keysym_for_char(expected),
                down: false,
            };
            // This release carries an empty mask, so an inferred Shift went
            // up first. The character still releases its original keysym.
            if shift {
                assert_eq!(
                    events,
                    [
                        Key {
                            keysym: bitmask::SHIFT,
                            down: false
                        },
                        char_up
                    ]
                );
            } else {
                assert_eq!(events, [char_up]);
            }
            assert!(input.release_all().is_empty());
        }
    }

    #[test]
    fn a_release_is_never_sent_for_a_key_that_was_not_pressed() {
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        assert_eq!(
            input.on_key(release(KeyCode::Char('z')), NO_ALTERNATES, Remote),
            (vec![], None)
        );
        assert_eq!(
            input.on_key(
                release(KeyCode::Modifier(
                    crossterm::event::ModifierKeyCode::LeftControl
                )),
                NO_ALTERNATES,
                Remote
            ),
            (vec![], None)
        );
    }

    #[test]
    fn command_shortcuts_act_only_inside_the_open_menu() {
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        for (code, command) in [
            (KeyCode::Esc, Command::Menu),
            (KeyCode::Char('8'), Command::SendF8),
            (KeyCode::Char('q'), Command::Quit),
            (KeyCode::Char('f'), Command::FullRefresh),
            (KeyCode::Char('r'), Command::Renegotiate),
            (KeyCode::Char('m'), Command::CycleMode),
            (KeyCode::Char('-'), Command::StepZoom(-1)),
            (KeyCode::Char('='), Command::StepZoom(1)),
            (KeyCode::Char('+'), Command::StepZoom(1)),
            (KeyCode::Char('v'), Command::ToggleViewOnly),
            (KeyCode::Char('c'), Command::ToggleStats),
            (KeyCode::Char('p'), Command::ToggleRemoteCursor),
            (KeyCode::Left, Command::Pan(-1, 0)),
            (KeyCode::Right, Command::Pan(1, 0)),
            (KeyCode::Up, Command::Pan(0, -1)),
            (KeyCode::Down, Command::Pan(0, 1)),
        ] {
            assert_eq!(input.local_command(press(code), Remote), None);
            assert_eq!(
                input.on_key(press(code), NO_ALTERNATES, Menu),
                (vec![], Some(command))
            );
        }
        assert_eq!(
            input.on_key(press(KeyCode::Char('z')), NO_ALTERNATES, Menu),
            (vec![], None)
        );
        // Shifted `+` / `=` still steps zoom in the menu; Ctrl/Alt do not.
        for code in [KeyCode::Char('+'), KeyCode::Char('=')] {
            assert_eq!(
                input.on_key(
                    key(code, KeyEventKind::Press, KeyModifiers::SHIFT),
                    NO_ALTERNATES,
                    Menu
                ),
                (vec![], Some(Command::StepZoom(1)))
            );
            assert_eq!(
                input.on_key(
                    key(code, KeyEventKind::Press, KeyModifiers::CONTROL),
                    NO_ALTERNATES,
                    Menu
                ),
                (vec![], None)
            );
        }
    }

    #[test]
    fn the_menu_swallows_what_would_have_reached_the_remote() {
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        assert_eq!(
            input.on_key(press(KeyCode::Char('x')), NO_ALTERNATES, Menu),
            (vec![], None)
        );
        assert_eq!(
            input.on_key(release(KeyCode::Char('x')), NO_ALTERNATES, Menu),
            (vec![], None)
        );
        // The point of swallowing it here rather than dropping what comes back: a
        // translated press is recorded as held, and would be released later for a key
        // the server was never told about.
        assert!(
            input.release_all().is_empty(),
            "a swallowed press must not be remembered as held"
        );
    }

    #[test]
    fn the_menu_still_lets_go_of_a_key_held_before_it_opened() {
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        assert!(
            !input
                .on_key(press(KeyCode::Char('x')), NO_ALTERNATES, Remote)
                .0
                .is_empty()
        );
        let (events, _) = input.on_key(release(KeyCode::Char('x')), NO_ALTERNATES, Menu);
        assert_eq!(events.len(), 1);
        assert!(!events[0].down);
        assert!(input.release_all().is_empty(), "and it is no longer held");
    }

    #[test]
    fn release_all_lets_go_of_everything_still_down() {
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        input.on_key(press(KeyCode::Char('a')), NO_ALTERNATES, Remote);
        input.on_key(
            key(KeyCode::Char('-'), KeyEventKind::Press, KeyModifiers::SHIFT),
            NO_ALTERNATES,
            Remote,
        );
        input.on_key(
            key(
                KeyCode::Modifier(crossterm::event::ModifierKeyCode::LeftControl),
                KeyEventKind::Press,
                KeyModifiers::CONTROL,
            ),
            NO_ALTERNATES,
            Remote,
        );
        let released = input.release_all();
        assert!(released.iter().all(|e| !e.down));
        let syms: Vec<_> = released.iter().map(|e| e.keysym).collect();
        assert_eq!(syms, [u32::from('_'), u32::from('a'), bitmask::CONTROL]);
        assert!(input.release_all().is_empty(), "and only once");
        assert_eq!(
            input.on_key(press(KeyCode::Char('-')), NO_ALTERNATES, Remote),
            (
                vec![Key {
                    keysym: u32::from('-'),
                    down: true
                }],
                None
            )
        );
    }

    #[test]
    fn pointer_position_is_pixel_exact_with_mode_1016() {
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        let layout = native_layout();
        let events = pointers(input.on_mouse(
            MouseEvent {
                kind: MouseEventKind::Moved,
                column: 37,
                row: 91,
                modifiers: KeyModifiers::NONE,
            },
            &layout,
            &metrics(),
        ));
        assert_eq!(events.len(), 1);
        assert_eq!((events[0].x, events[0].y), (37, 91));
    }

    #[test]
    fn without_mode_1016_the_pointer_aims_at_the_cell_centre() {
        let mut input = InputMapper::new(MenuKey::F8, true, false);
        let layout = native_layout();
        let events = pointers(input.on_mouse(
            MouseEvent {
                kind: MouseEventKind::Moved,
                column: 4,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
            &layout,
            &metrics(),
        ));
        assert_eq!((events[0].x, events[0].y), (4 * 8 + 4, 5 * 17 + 8));
    }

    #[test]
    fn the_cell_under_the_pointer_is_found_either_way_round() {
        // What the menu is hit-tested against. With mode 1016 the event carries
        // pixels and has to be divided back down; without it the event is already
        // cells, and going out to the middle of one and back has to land on the cell
        // it started from.
        let m = metrics();
        let at = |column, row| MouseEvent {
            kind: MouseEventKind::Moved,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let pixels = InputMapper::new(MenuKey::F8, true, true);
        assert_eq!(pixels.terminal_cell(&at(0, 0), &m), (0, 0));
        assert_eq!(
            pixels.terminal_cell(&at(8 * 12 + 3, 17 * 7 + 9), &m),
            (12, 7)
        );

        let cells = InputMapper::new(MenuKey::F8, true, false);
        for (col, row) in [(0, 0), (12, 7), (m.cols - 1, m.rows - 1)] {
            assert_eq!(cells.terminal_cell(&at(col, row), &m), (col, row));
        }
    }

    #[test]
    fn buttons_accumulate_and_clear() {
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        let layout = native_layout();
        let m = metrics();
        let at = |kind| MouseEvent {
            kind,
            column: 10,
            row: 10,
            modifiers: KeyModifiers::NONE,
        };

        let e = pointers(input.on_mouse(at(MouseEventKind::Down(MouseButton::Left)), &layout, &m));
        assert_eq!(e[0].buttons, button::LEFT);
        let e = pointers(input.on_mouse(at(MouseEventKind::Down(MouseButton::Right)), &layout, &m));
        assert_eq!(e[0].buttons, button::LEFT | button::RIGHT);
        let e = pointers(input.on_mouse(at(MouseEventKind::Up(MouseButton::Left)), &layout, &m));
        assert_eq!(e[0].buttons, button::RIGHT);

        let stop = input.release_buttons().unwrap();
        assert_eq!(stop.buttons, 0);
        assert!(input.release_buttons().is_none());
    }

    #[test]
    fn a_remote_button_released_over_the_footer_uses_its_last_position() {
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        let layout = native_layout();
        let m = metrics();
        let mut ev = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 10,
            row: 12,
            modifiers: KeyModifiers::NONE,
        };
        input.on_mouse(ev, &layout, &m);
        ev.row = m.image_area().1 as u16;
        ev.kind = MouseEventKind::Up(MouseButton::Left);
        ev.modifiers = KeyModifiers::CONTROL;
        assert!(input.on_mouse(ev, &layout, &m).is_empty());
        ev.kind = MouseEventKind::Up(MouseButton::Right);
        ev.modifiers = KeyModifiers::NONE;
        assert_eq!(
            pointers(input.on_mouse(ev, &layout, &m)),
            [Pointer {
                x: 10,
                y: 12,
                buttons: 0
            }]
        );
        assert!(input.release_buttons().is_none());
    }

    #[test]
    fn a_wheel_notch_is_a_press_and_a_release() {
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        let layout = native_layout();
        let events = pointers(input.on_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
            &layout,
            &metrics(),
        ));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].buttons, button::WHEEL_UP);
        assert_eq!(events[1].buttons, 0);
    }

    #[test]
    fn a_wheel_notch_while_dragging_keeps_the_held_button() {
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        let layout = native_layout();
        let m = metrics();
        input.on_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
            &layout,
            &m,
        );
        let events = pointers(input.on_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
            &layout,
            &m,
        ));
        assert_eq!(events[0].buttons, button::LEFT | button::WHEEL_DOWN);
        assert_eq!(events[1].buttons, button::LEFT);
    }

    #[test]
    fn motion_is_rate_limited_and_the_last_position_still_arrives() {
        // A pixel-reporting terminal can emit motion far faster than any desktop
        // needs. Intermediate positions may be dropped; the final one may not.
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        let layout = native_layout();
        let m = metrics();
        let at = |col| MouseEvent {
            kind: MouseEventKind::Moved,
            column: col,
            row: 10,
            modifiers: KeyModifiers::NONE,
        };

        assert_eq!(
            input.on_mouse(at(10), &layout, &m).len(),
            1,
            "first move goes"
        );
        assert!(
            input.on_mouse(at(11), &layout, &m).is_empty(),
            "a move straight after must be held back"
        );
        assert!(input.on_mouse(at(12), &layout, &m).is_empty());

        // Nothing to flush until the interval has passed, and then exactly the
        // last position.
        assert!(input.flush_motion().is_none());
        std::thread::sleep(MOTION_INTERVAL + Duration::from_millis(2));
        let flushed = input.flush_motion().expect("the last position must arrive");
        assert_eq!((flushed.x, flushed.y), (12, 10));
        assert!(input.flush_motion().is_none(), "and only once");
    }

    #[test]
    fn a_click_flushes_held_back_motion_first() {
        // Otherwise the button would land wherever the last report happened to be,
        // which is the classic off-by-a-few-pixels click bug.
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        let layout = native_layout();
        let m = metrics();
        let ev = |kind, col| MouseEvent {
            kind,
            column: col,
            row: 20,
            modifiers: KeyModifiers::NONE,
        };

        input.on_mouse(ev(MouseEventKind::Moved, 30), &layout, &m);
        assert!(
            input
                .on_mouse(ev(MouseEventKind::Moved, 44), &layout, &m)
                .is_empty()
        );

        let events =
            pointers(input.on_mouse(ev(MouseEventKind::Down(MouseButton::Left), 44), &layout, &m));
        assert_eq!(
            events.len(),
            2,
            "the held position, then the click: {events:?}"
        );
        assert_eq!((events[0].x, events[0].buttons), (44, 0));
        assert_eq!((events[1].x, events[1].buttons), (44, button::LEFT));
    }

    #[test]
    fn a_wheel_notch_is_never_held_back() {
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        let layout = native_layout();
        let m = metrics();
        input.on_mouse(
            MouseEvent {
                kind: MouseEventKind::Moved,
                column: 5,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
            &layout,
            &m,
        );
        let events = pointers(input.on_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 6,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
            &layout,
            &m,
        ));
        assert!(
            events.iter().any(|e| e.buttons == button::WHEEL_UP),
            "the notch must go out immediately: {events:?}"
        );
    }

    #[test]
    fn releasing_buttons_drops_any_held_back_motion() {
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        let layout = native_layout();
        let m = metrics();
        input.on_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
            &layout,
            &m,
        );
        input.on_mouse(
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 2,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
            &layout,
            &m,
        );
        assert!(input.release_buttons().is_some());
        std::thread::sleep(MOTION_INTERVAL + Duration::from_millis(2));
        assert!(
            input.flush_motion().is_none(),
            "a stale position must not follow the button release"
        );
    }

    #[test]
    fn repeated_motion_at_the_same_pixel_is_dropped() {
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        let layout = native_layout();
        let m = metrics();
        let ev = MouseEvent {
            kind: MouseEventKind::Moved,
            column: 20,
            row: 20,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(input.on_mouse(ev, &layout, &m).len(), 1);
        assert!(input.on_mouse(ev, &layout, &m).is_empty());
    }

    #[test]
    fn a_pointer_outside_the_image_is_not_reported() {
        let m = metrics();
        // A letterboxed layout: the image does not fill the area.
        let layout = Layout::compute(&m, ScaleMode::Fit, 1920, 1080, (0, 0));
        let mut input = InputMapper::new(MenuKey::F8, true, true);
        assert!(
            input
                .on_mouse(
                    MouseEvent {
                        kind: MouseEventKind::Moved,
                        column: 0,
                        row: 0,
                        modifiers: KeyModifiers::NONE,
                    },
                    &layout,
                    &m,
                )
                .is_empty(),
            "the top-left corner is letterbox, not desktop"
        );
    }
}
