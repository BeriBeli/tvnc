//! Read input and presentation replies in the order the terminal sent them.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyModifiers};
use rustix::event::{PollFd, PollFlags, poll};
use rustix::fs::{Mode, OFlags, fcntl_getfl, open};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::time::{Instant, sleep_until};

use crate::ui::theme::ColorScheme;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyAlternates {
    pub shifted: Option<char>,
    pub base_layout: Option<char>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameQuery {
    DeviceAttributes,
    KittyGraphics,
}

// This query never places an image. Its id is separate from both startup probes.
const GRAPHICS_QUERY_ID: u32 = 1895;

impl FrameQuery {
    pub fn for_caps(caps: &super::caps::Caps) -> Option<Self> {
        if caps.da1.is_some() {
            Some(Self::DeviceAttributes)
        } else {
            caps.kitty_graphics_reply.then_some(Self::KittyGraphics)
        }
    }

    pub fn request(self) -> &'static [u8] {
        match self {
            Self::DeviceAttributes => b"\x1b[0c",
            Self::KittyGraphics => b"\x1b_Gi=1895,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Report {
    Input(Event, KeyAlternates),
    FramePresented,
    ColorScheme(ColorScheme),
}

pub struct TerminalEvents {
    tty: AsyncFd<File>,
    resize: Signal,
    query: FrameQuery,
    pending: Vec<u8>,
    ready: VecDeque<Report>,
    escape_deadline: Option<Instant>,
}

impl TerminalEvents {
    pub fn new(query: FrameQuery) -> io::Result<Self> {
        let stdin = io::stdin();
        let stdout = io::stdout();
        let source = if stdin.is_terminal() {
            stdin.as_fd()
        } else {
            stdout.as_fd()
        };
        Self::from_terminal(source, query)
    }

    fn from_terminal(source: BorrowedFd<'_>, query: FrameQuery) -> io::Result<Self> {
        // Reopen the actual device when possible: kqueue on macOS rejects /dev/tty.
        let reopened = rustix::termios::ttyname(source, Vec::new()).and_then(|path| {
            open(
                path.as_c_str(),
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOCTTY,
                Mode::empty(),
            )
        });
        let tty = match reopened {
            Ok(fd) => File::from(fd),
            Err(_) => {
                let fd = if fcntl_getfl(source)? & OFlags::ACCMODE == OFlags::WRONLY {
                    // Stdin was redirected and stdout can identify the terminal
                    // but cannot be read. Use the controlling terminal for input.
                    open(
                        "/dev/tty",
                        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOCTTY,
                        Mode::empty(),
                    )?
                } else {
                    source.try_clone_to_owned()?
                };
                inherited_input(File::from(fd))?
            }
        };
        // SAFETY: the owned File keeps this descriptor open and stable until
        // AsyncFd deregisters it and drops the File.
        let tty = unsafe { AsyncFd::register_with_interest(tty, Interest::READABLE)? };
        Ok(Self {
            tty,
            query,
            resize: signal(SignalKind::window_change())?,
            pending: Vec::new(),
            ready: VecDeque::new(),
            escape_deadline: None,
        })
    }

    pub async fn next(&mut self, awaiting_frame_reply: bool) -> io::Result<Report> {
        loop {
            if let Some(report) = self.ready.pop_front() {
                return Ok(report);
            }
            // Every ambiguous prefix, including a lone ESC, can still be the
            // start of the reply we are waiting for. Keep it across PTY reads.
            let deadline = self.escape_deadline.filter(|_| !awaiting_frame_reply);
            tokio::select! {
                guard = self.tty.readable() => {
                    let mut bytes = [0; 4096];
                    let Ok(count) = guard?.try_io(|tty| tty.get_ref().read(&mut bytes)) else {
                        continue;
                    };
                    let count = match count {
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        result => result?,
                    };
                    if count == 0 {
                        return Err(io::ErrorKind::UnexpectedEof.into());
                    }
                    // If the next byte rules out a reply, emit the timed-out
                    // Escape before processing the user's next key.
                    if awaiting_frame_reply && self.pending == b"\x1b"
                        && self.escape_deadline.is_some_and(|at| Instant::now() >= at)
                        && !starts_csi_or_apc(self.query, bytes[0])
                    {
                        finish_escape_key(self.query, &mut self.pending, &mut self.ready);
                    }
                    queue_reports(self.query, &mut self.pending, &mut self.ready, &bytes[..count]);
                    self.escape_deadline = ambiguous_escape(self.query, &self.pending)
                        .then(|| Instant::now() + Duration::from_millis(25));
                }
                _ = self.resize.recv() => {
                    let (cols, rows) = crossterm::terminal::size()?;
                    return Ok(Report::Input(Event::Resize(cols, rows), KeyAlternates::default()));
                }
                _ = async { sleep_until(deadline.unwrap()).await }, if deadline.is_some() => {
                    self.escape_deadline = None;
                    finish_escape_key(self.query, &mut self.pending, &mut self.ready);
                }
            }
        }
    }
}

/// When the device path is unavailable, relay the inherited terminal through a
/// private socket. Setting O_NONBLOCK on a dup would also change the shell's
/// descriptor and, on some terminals, the frame writer's stdout.
fn inherited_input(mut input: File) -> io::Result<File> {
    let (reader, mut relay) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    std::thread::Builder::new()
        .name("tvnc-tty-input".into())
        .spawn(move || -> io::Result<()> {
            let mut buf = [0; 4096];
            loop {
                // Closing the reader wakes poll even if no more input ever arrives.
                let mut fds = [
                    PollFd::new(&input, PollFlags::IN),
                    PollFd::new(&relay, PollFlags::IN),
                ];
                match poll(&mut fds, None) {
                    Err(rustix::io::Errno::INTR) => continue,
                    result => result?,
                };
                if !fds[1].revents().is_empty() {
                    return Ok(());
                }
                match input.read(&mut buf) {
                    Ok(0) => return Ok(()),
                    Ok(n) => relay.write_all(&buf[..n])?,
                    Err(err)
                        if matches!(
                            err.kind(),
                            io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                        ) =>
                    {
                        continue;
                    }
                    Err(err) => return Err(err),
                }
            }
        })?;
    Ok(File::from(OwnedFd::from(reader)))
}

fn ambiguous_escape(query: FrameQuery, bytes: &[u8]) -> bool {
    matches!(bytes, b"\x1b" | b"\x1b\x1b")
        || (query == FrameQuery::KittyGraphics && bytes == b"\x1b_")
}

fn starts_csi_or_apc(query: FrameQuery, byte: u8) -> bool {
    byte == b'[' || (query == FrameQuery::KittyGraphics && byte == b'_')
}

#[derive(PartialEq, Eq)]
enum Reply {
    Incomplete,
    Presented,
    ColorScheme(ColorScheme),
    Other,
}

fn terminal_reply(query: FrameQuery, seq: &[u8]) -> Option<Reply> {
    if b"\x1b[?".starts_with(seq) {
        return Some(Reply::Incomplete);
    }
    if let Some(csi) = seq.strip_prefix(b"\x1b[")
        && csi.starts_with(b"?")
    {
        let (&last, body) = csi.split_last()?;
        if !(0x40..=0x7e).contains(&last) {
            return Some(Reply::Incomplete);
        }
        if last == b'n'
            && let Some(scheme) = super::caps::color_scheme_reply(body)
        {
            return Some(Reply::ColorScheme(scheme));
        }
        let da1 = last == b'c' && super::caps::da1_parameters(body).is_some();
        return Some(if da1 && query == FrameQuery::DeviceAttributes {
            Reply::Presented
        } else {
            Reply::Other
        });
    }
    if query != FrameQuery::KittyGraphics {
        return None;
    }
    // ESC _ can also be Alt+underscore. A Kitty APC starts with G, a
    // single-letter parameter and '='. An error reply confirms the frame too.
    let apc = seq.strip_prefix(b"\x1b_")?;
    match apc {
        [] | [b'G'] => Some(Reply::Incomplete),
        [b'G', name] if name.is_ascii_alphabetic() => Some(Reply::Incomplete),
        [b'G', name, b'=', ..] if name.is_ascii_alphabetic() => {
            let Some(payload) = apc
                .strip_suffix(b"\x1b\\")
                .or_else(|| apc.strip_suffix(&[0x9c]))
            else {
                return Some(Reply::Incomplete);
            };
            let presented = matches!(
                super::caps::graphics_reply(payload),
                Some((GRAPHICS_QUERY_ID, _))
            );
            Some(if presented {
                Reply::Presented
            } else {
                Reply::Other
            })
        }
        _ => None,
    }
}

fn queue_reports(
    query: FrameQuery,
    pending: &mut Vec<u8>,
    ready: &mut VecDeque<Report>,
    bytes: &[u8],
) {
    for &byte in bytes {
        if pending.as_slice() == b"\x1b\x1b" {
            if starts_csi_or_apc(query, byte) {
                // A second escape introduces a CSI or APC. Deliver the user's
                // first Esc and let the regular parser own the entire sequence,
                // including a bracketed paste and its opaque payload.
                ready.push_back(Report::Input(
                    Event::Key(KeyCode::Esc.into()),
                    KeyAlternates::default(),
                ));
                pending.pop();
            } else {
                finish_escape_key(query, pending, ready);
            }
        }
        pending.push(byte);
        if pending.as_slice() == b"\x1b\x1b" {
            continue;
        }
        match terminal_reply(query, pending) {
            Some(Reply::Incomplete) => continue,
            Some(Reply::Presented) => ready.push_back(Report::FramePresented),
            Some(Reply::ColorScheme(scheme)) => ready.push_back(Report::ColorScheme(scheme)),
            Some(Reply::Other) => {}
            None if query == FrameQuery::KittyGraphics && pending.starts_with(b"\x1b_") => {
                finish_escape_key(query, pending, ready);
                continue;
            }
            None => match decode(pending) {
                Ok(None) => continue,
                Ok(Some(report)) => ready.push_back(report),
                Err(_) => {} // Unknown terminal response.
            },
        }
        pending.clear();
    }
}

fn finish_escape_key(query: FrameQuery, pending: &mut Vec<u8>, ready: &mut VecDeque<Report>) {
    let tail = pending.split_off(pending.len().min(2));
    if let Ok(Some(key)) = decode(pending) {
        ready.push_back(key);
    }
    pending.clear();
    queue_reports(query, pending, ready, &tail);
}

fn kitty_release_mask(bytes: &[u8]) -> Option<terminput::KeyModifiers> {
    let (_, modifiers_and_kind) = std::str::from_utf8(bytes).ok()?.split_once(';')?;
    let (modifiers, _) = modifiers_and_kind.split_once(':')?;
    let bits = modifiers.parse::<u8>().ok()?.checked_sub(1)?;
    Some(terminput::KeyModifiers::from_bits_truncate(bits))
}

fn decode(bytes: &mut Vec<u8>) -> io::Result<Option<Report>> {
    // Paste text can itself contain a CSI-u sequence. Parse it only at the terminator.
    if bytes.starts_with(b"\x1b[200~") && !bytes.ends_with(b"\x1b[201~") {
        return Ok(None);
    }
    let mut alternates = KeyAlternates::default();
    if let Some(fields) = bytes
        .strip_prefix(b"\x1b[")
        .and_then(|s| s.strip_suffix(b"u"))
    {
        // Kitty's first field is base[:shifted[:PC-101 key]]. Pass just the base
        // to the general parser, which otherwise folds Shift into the key code.
        let end = fields
            .iter()
            .position(|&b| b == b';')
            .unwrap_or(fields.len());
        if let Some(colon) = fields[..end].iter().position(|&b| b == b':') {
            let mut codes = fields[colon + 1..end]
                .split(|&b| b == b':')
                .map(|s| char::from_u32(std::str::from_utf8(s).ok()?.parse().ok()?));
            alternates.shifted = codes.next().flatten();
            alternates.base_layout = codes.next().flatten();
            bytes.drain(2 + colon..2 + end);
        }
    }
    let Some(event) = terminput::Event::parse_from(bytes)? else {
        return Ok(None);
    };
    let event = match event {
        terminput::Event::Key(mut key) => {
            // terminput adds a modifier's own bit even on release. Kitty's
            // wire mask says whether the opposite side is still held.
            if key.kind == terminput::KeyEventKind::Release
                && matches!(key.code, terminput::KeyCode::Modifier(..))
                && let Some(modifiers) = kitty_release_mask(bytes)
            {
                key.modifiers = modifiers;
            }
            // The adapter maps the other modifiers but omits these Kitty bits.
            let mut mapped = terminput_crossterm::to_crossterm_key(key);
            mapped.modifiers.set(
                KeyModifiers::META,
                key.modifiers.contains(terminput::KeyModifiers::META),
            );
            mapped.modifiers.set(
                KeyModifiers::HYPER,
                key.modifiers.contains(terminput::KeyModifiers::HYPER),
            );
            Event::Key(mapped)
        }
        other => terminput_crossterm::to_crossterm(other).map_err(io::Error::other)?,
    };
    Ok(Some(Report::Input(event, alternates)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

    #[tokio::test]
    async fn inherited_input_without_a_device_path_keeps_kitty_reports_and_fd_flags() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // This readable descriptor has no tty pathname, just as in a chroot
        // without procfs or the original device node.
        let (inherited, terminal) = UnixStream::pair().unwrap();
        terminal.set_nonblocking(true).unwrap();
        let mut terminal = tokio::net::UnixStream::from_std(terminal).unwrap();
        let flags = fcntl_getfl(&inherited).unwrap();
        let mut events =
            TerminalEvents::from_terminal(inherited.as_fd(), FrameQuery::DeviceAttributes).unwrap();
        assert_eq!(fcntl_getfl(&inherited).unwrap(), flags);
        drop(inherited);
        terminal.write_all(b"\x1b[45::54;2u").await.unwrap();
        let Report::Input(event, alternates) =
            tokio::time::timeout(Duration::from_secs(2), events.next(false))
                .await
                .unwrap()
                .unwrap()
        else {
            panic!("expected a key report")
        };
        assert!(
            matches!(event, Event::Key(key) if key.code == KeyCode::Char('-') && key.modifiers == KeyModifiers::SHIFT)
        );
        assert_eq!(alternates.base_layout, Some('6'));
        drop(events);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), terminal.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0,
            "the inherited reader should stop when the session closes"
        );
    }

    #[test]
    fn split_reads_keep_layout_modifiers_and_pasted_escape_sequences_intact() {
        let bytes =
            b"\x1b[118:86:118;6u\x1b[118;1:3u\x1b[200~\x1b[19~\x1b[97:65;6u\x1b[201~\x1b[O\x1b[I";
        let modifiers = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
        let expected = [
            Report::Input(
                Event::Key(KeyEvent::new(KeyCode::Char('v'), modifiers)),
                KeyAlternates {
                    shifted: Some('V'),
                    base_layout: Some('v'),
                },
            ),
            Report::Input(
                Event::Key(KeyEvent::new_with_kind(
                    KeyCode::Char('v'),
                    KeyModifiers::NONE,
                    KeyEventKind::Release,
                )),
                KeyAlternates::default(),
            ),
            Report::Input(
                Event::Paste("\x1b[19~\x1b[97:65;6u".into()),
                KeyAlternates::default(),
            ),
            Report::Input(Event::FocusLost, KeyAlternates::default()),
            Report::Input(Event::FocusGained, KeyAlternates::default()),
        ];
        for split in 0..=bytes.len() {
            let mut pending = Vec::new();
            let mut ready = VecDeque::new();
            queue_reports(
                FrameQuery::DeviceAttributes,
                &mut pending,
                &mut ready,
                &bytes[..split],
            );
            queue_reports(
                FrameQuery::DeviceAttributes,
                &mut pending,
                &mut ready,
                &bytes[split..],
            );
            assert!(pending.is_empty());
            assert_eq!(ready, expected, "split at byte {split}");
            // KeyEvent equality normalises shifted ASCII. The mapper needs
            // the exact base and modifier bits that came off the wire.
            let Some(Report::Input(Event::Key(key), _)) = ready.front() else {
                panic!("missing key at split {split}");
            };
            assert_eq!(key.code, KeyCode::Char('v'));
            assert_eq!(key.modifiers, modifiers);
        }
    }

    #[test]
    fn frame_replies_stay_in_input_order_and_pasted_replies_stay_text() {
        for (query, reply) in [
            (FrameQuery::DeviceAttributes, b"\x1b[?62;22c".as_slice()),
            (
                FrameQuery::KittyGraphics,
                b"\x1b_Gi=1895;OK\x1b\\".as_slice(),
            ),
        ] {
            let bytes = [
                b"\x1b[?1016;2$ya\x1b".as_slice(),
                reply,
                b"\x1b[200~",
                reply,
                b"\x1b[201~b",
            ]
            .concat();
            let key = |c| {
                Report::Input(
                    Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
                    KeyAlternates::default(),
                )
            };
            let expected = [
                key('a'),
                Report::Input(Event::Key(KeyCode::Esc.into()), KeyAlternates::default()),
                Report::FramePresented,
                Report::Input(
                    Event::Paste(String::from_utf8(reply.to_vec()).unwrap()),
                    KeyAlternates::default(),
                ),
                key('b'),
            ];
            for split in 0..=bytes.len() {
                let mut pending = Vec::new();
                let mut ready = VecDeque::new();
                queue_reports(query, &mut pending, &mut ready, &bytes[..split]);
                queue_reports(query, &mut pending, &mut ready, &bytes[split..]);
                assert!(pending.is_empty());
                assert_eq!(ready, expected);
            }
        }
    }

    #[test]
    fn a_legacy_alt_escape_still_works_without_a_frame_reply() {
        let query = FrameQuery::DeviceAttributes;
        let mut pending = Vec::new();
        let mut ready = VecDeque::new();
        queue_reports(query, &mut pending, &mut ready, b"\x1b\x1bz\x1b\x1b");
        assert!(ambiguous_escape(query, &pending));
        finish_escape_key(query, &mut pending, &mut ready);
        let key = |code, modifiers| {
            Report::Input(
                Event::Key(KeyEvent::new(code, modifiers)),
                KeyAlternates::default(),
            )
        };
        assert_eq!(
            ready,
            [
                key(KeyCode::Esc, KeyModifiers::ALT),
                key(KeyCode::Char('z'), KeyModifiers::NONE),
                key(KeyCode::Esc, KeyModifiers::ALT),
            ]
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn graphics_replies_do_not_turn_into_keys_or_eat_alt_underscore() {
        let mut pending = Vec::new();
        let mut ready = VecDeque::new();
        queue_reports(
            FrameQuery::KittyGraphics,
            &mut pending,
            &mut ready,
            b"\x1b_Gi=1893;OK\x1b\\\x1b_Gp=1,i=1895;ENOENT\x1b\\\x1b_G!",
        );
        let key = |c, modifiers| {
            Report::Input(
                Event::Key(KeyEvent::new(KeyCode::Char(c), modifiers)),
                KeyAlternates::default(),
            )
        };
        assert_eq!(
            ready,
            [
                Report::FramePresented,
                key('_', KeyModifiers::ALT),
                key('G', KeyModifiers::SHIFT),
                key('!', KeyModifiers::NONE)
            ]
        );
    }

    #[test]
    fn mode_2031_color_scheme_updates_arrive_as_reports() {
        let mut pending = Vec::new();
        let mut ready = VecDeque::new();
        queue_reports(
            FrameQuery::DeviceAttributes,
            &mut pending,
            &mut ready,
            b"\x1b[?997;2n\x1b[?997;1n",
        );
        assert_eq!(
            ready,
            [
                Report::ColorScheme(ColorScheme::Light),
                Report::ColorScheme(ColorScheme::Dark),
            ]
        );
    }
}
