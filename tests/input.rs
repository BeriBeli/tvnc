//! Keyboard, pointer and clipboard reaching the server, against a fake server.
//!
//! The terminal is the only input device, and what it reports varies: some terminals
//! answer for the keyboard protocol and owe a release for every press, some report
//! nothing and have their releases synthesised. The viewer also handles local menu
//! shortcuts and synchronises the remote lock keys.

mod common;

use std::time::Duration;

use base64::Engine as _;
use common::server::{
    EXTENDED_CLIPBOARD_ENCODING, Extensions, FakeServer, REMOTE_CLIPBOARD, Request, Resize,
};
use common::session::*;
use common::*;

fn received_keys(server: &FakeServer) -> Vec<(u32, bool)> {
    server
        .requests()
        .into_iter()
        .filter_map(|r| match r {
            Request::Key { keysym, down } => Some((keysym, down)),
            _ => None,
        })
        .collect()
}

fn received_pointers(server: &FakeServer) -> Vec<(u8, u16, u16)> {
    server
        .requests()
        .into_iter()
        .filter_map(|r| match r {
            Request::Pointer { buttons, x, y } => Some((buttons, x, y)),
            _ => None,
        })
        .collect()
}

fn wait_for_key(server: &FakeServer, symbol: char, down: bool) {
    server.wait_for(Duration::from_secs(10), |r| {
        matches!(r, Request::Key { keysym, down: state } if *keysym == u32::from(symbol) && *state == down)
    }).unwrap_or_else(|| panic!("key {symbol:?} (down={down}) missing from {:?}", received_keys(server)));
}

fn click_at(term: &mut FakeTerm, x: u16, y: u16) {
    term.send(format!("\x1b[<0;{x};{y}M\x1b[<0;{x};{y}m").as_bytes());
}

#[test]
fn a_legacy_escape_before_paste_respects_the_clipboard_setting() {
    let timeout = Duration::from_secs(8);
    for clipboard in [false, true] {
        let server = FakeServer::start(240, 224, Resize::Accept);
        let addr = server.addr.to_string();
        let mut args = vec![addr.as_str()];
        if !clipboard {
            args.push("--no-clipboard");
        }
        let mut term = FakeTerm::spawn(30, 15, 240, 240, &args);
        term.answer_probe(NO_KEYBOARD_REPLIES);
        assert!(term.wait_for(LAYOUT_QUERY, timeout));
        term.send(b"\x1b\x1b[200~first\x1b[201~");
        term.send(b"\x1b");
        std::thread::sleep(Duration::from_millis(10));
        term.send(b"\x1b[200~second\x1b[201~z");
        wait_for_key(&server, 'z', false);
        assert_eq!(
            received_keys(&server),
            [
                (0xff1b, true),
                (0xff1b, false),
                (0xff1b, true),
                (0xff1b, false),
                (122, true),
                (122, false)
            ]
        );
        let pastes: Vec<_> = server
            .requests()
            .into_iter()
            .filter_map(|r| match r {
                Request::CutText(text) => Some(text),
                _ => None,
            })
            .collect();
        if clipboard {
            assert_eq!(pastes, ["first", "second"]);
        } else {
            assert!(pastes.is_empty());
        }
        quit(&mut term);
        assert!(term.wait(timeout).unwrap().success());
    }
}

#[test]
fn a_legacy_escape_next_to_a_da1_reply_reaches_vnc_as_one_key() {
    let server = FakeServer::start(240, 224, Resize::Accept);
    let addr = server.addr.to_string();
    let mut term = FakeTerm::spawn(30, 15, 240, 240, &[&addr]);
    let timeout = Duration::from_secs(8);
    term.answer_probe(NO_KEYBOARD_REPLIES);
    assert!(term.wait_for(LAYOUT_QUERY, timeout));
    term.send(b"\x1b\x1b[?62;22cz");
    wait_for_key(&server, 'z', false);
    assert_eq!(
        received_keys(&server),
        [(0xff1b, true), (0xff1b, false), (122, true), (122, false)]
    );
    quit(&mut term);
    assert!(term.wait(timeout).unwrap().success());
}

#[test]
fn a_split_frame_reply_outlives_the_escape_key_timeout() {
    let da1 = b"\x1b[?62;22c".as_slice();
    let kitty = b"\x1b_Gi=1895;OK\x1b\\".as_slice();
    let graphics_only = NO_KEYBOARD_REPLIES.strip_suffix(da1).unwrap();
    let timeout = Duration::from_secs(8);
    for (startup, query, reply, split, user_esc) in [
        (NO_KEYBOARD_REPLIES, LAYOUT_QUERY, da1, 1, false),
        (NO_KEYBOARD_REPLIES, LAYOUT_QUERY, da1, 1, true),
        (NO_KEYBOARD_REPLIES, LAYOUT_QUERY, da1, 2, true),
        (graphics_only, GRAPHICS_LAYOUT_QUERY, kitty, 1, false),
        (graphics_only, GRAPHICS_LAYOUT_QUERY, kitty, 1, true),
        (graphics_only, GRAPHICS_LAYOUT_QUERY, kitty, 2, false),
        (graphics_only, GRAPHICS_LAYOUT_QUERY, kitty, 3, false),
        (graphics_only, GRAPHICS_LAYOUT_QUERY, kitty, 4, true),
    ] {
        let server = FakeServer::start(640, 64, Resize::Accept);
        let addr = server.addr.to_string();
        let mut term = FakeTerm::with_manual_frame_replies(80, 5, 640, 80, &[&addr]);
        term.answer_probe(startup);
        assert!(term.wait_for(query, timeout));
        let footer = Screen::of(&term.output()).row(4);
        let col = footer.find("commands").unwrap() as u16;
        let escape = if user_esc { b"\x1b".as_slice() } else { b"" };
        term.send(&[escape, &reply[..split]].concat());
        std::thread::sleep(Duration::from_millis(100));
        term.send(&reply[split..]);
        if query == GRAPHICS_LAYOUT_QUERY {
            // With the reply complete, the same two bytes alone mean Alt+underscore.
            term.send(b"\x1b_");
            wait_for_key(&server, '_', false);
        }
        click_at(&mut term, col * 8 + 1, 65);
        term.send(b"8z");
        wait_for_key(&server, 'z', false);
        // A clickable footer proves the parser also delivered the acknowledgement.
        let mut expected = Vec::new();
        if user_esc {
            expected.extend([(0xff1b, true), (0xff1b, false)]);
        }
        if query == GRAPHICS_LAYOUT_QUERY {
            expected.extend([(0xffe9, true), (95, true), (95, false), (0xffe9, false)]);
        }
        expected.extend([(0xffc5, true), (0xffc5, false), (122, true), (122, false)]);
        assert_eq!(
            received_keys(&server),
            expected,
            "reply {reply:?}, split {split}, user Escape {user_esc}"
        );
        assert!(
            !server
                .requests()
                .iter()
                .any(|r| matches!(r, Request::Pointer { .. }))
        );
        quit(&mut term);
        assert!(term.wait(timeout).unwrap().success());
    }
}

#[test]
fn a_standalone_legacy_escape_keeps_its_identity_while_a_reply_is_pending() {
    let server = FakeServer::start(640, 64, Resize::Accept);
    let addr = server.addr.to_string();
    let mut term =
        FakeTerm::with_manual_frame_replies(80, 5, 640, 80, &[&addr, "--menu-key", "none"]);
    let timeout = Duration::from_secs(8);
    term.answer_probe(NO_KEYBOARD_REPLIES);
    assert!(term.wait_for(LAYOUT_QUERY, timeout));
    let col = Screen::of(&term.output()).row(4).find("commands").unwrap() as u16;

    term.send(b"\x1b");
    std::thread::sleep(Duration::from_millis(100));
    term.send(b"x");
    wait_for_key(&server, 'x', false);
    term.send(b"\x1b");
    std::thread::sleep(Duration::from_millis(100));
    term.send(b"\x1b");
    std::thread::sleep(Duration::from_millis(100));
    term.send(b"[?62;22cy");
    wait_for_key(&server, 'y', false);
    assert_eq!(
        received_keys(&server),
        [
            (0xff1b, true),
            (0xff1b, false),
            (120, true),
            (120, false),
            (0xff1b, true),
            (0xff1b, false),
            (121, true),
            (121, false)
        ]
    );
    click_at(&mut term, col * 8 + 1, 65);
    term.send(b"q");
    assert!(term.wait(timeout).unwrap().success());
    assert!(received_pointers(&server).is_empty());
}

#[test]
fn f8_reserves_only_the_bare_key_and_can_be_sent_from_the_menu() {
    let (server, mut term) = start(Resize::Accept, (800, 600));
    let timeout = Duration::from_secs(8);
    assert_drew(&term, timeout);
    // One Ctrl+A and its following letter both reach the desktop.
    term.send(b"\x1b[97;5u\x1b[97;5:3up\x1b[112;1:3u");
    term.send(b"\x1b[19;2~\x1b[19;2:3~"); // Shift+F8 passes through.
    term.send(b"\x1b[104u"); // Keep H held as the menu takes focus.
    term.send(b"\x1b[19~\x1b[19;1:2~\x1b[19;1:3~"); // Repeat and up stay local.
    assert!(wait_for_text(&term, "Send F8 to remote", timeout));
    wait_for_key(&server, 'h', false);
    term.send(b"\x1b[200~local only\x1b[201~");
    term.send(b"8\x1b[56;1:3u"); // Explicitly send one F8 and leave the menu.
    term.send(b"\x1b[104;1:2u\x1b[104;1:3u"); // H was released on entry.
    term.send(b"z\x1b[122;1:3u");
    wait_for_key(&server, 'z', false);
    let f8 = 0xffc5;
    assert_eq!(
        received_keys(&server),
        [
            (0xffe3, true),
            (97, true),
            (97, false),
            (0xffe3, false),
            (112, true),
            (112, false),
            (0xffe1, true),
            (f8, true),
            (f8, false),
            (0xffe1, false),
            (104, true),
            (104, false),
            (f8, true),
            (f8, false),
            (122, true),
            (122, false),
        ]
    );
    assert!(
        !server
            .requests()
            .iter()
            .any(|r| matches!(r, Request::CutText(_)))
    );
    quit(&mut term);
    assert!(term.wait(timeout).unwrap().success());
}

#[test]
fn meta_and_hyper_f8_reach_the_remote_with_their_modifiers() {
    let (server, mut term) = start(Resize::Accept, (800, 600));
    let timeout = Duration::from_secs(8);
    assert_drew(&term, timeout);
    let f8 = 0xffc5;
    let mut expected = Vec::new();
    // A mask-only terminal reports whether the modifier is still down on F8-up.
    for (mask, modifier) in [(33, 0xffe7), (17, 0xffed)] {
        for release_mask in [mask, 1] {
            term.send(
                format!("\x1b[19;{mask}~\x1b[19;{mask}:2~\x1b[19;{release_mask}:3~").as_bytes(),
            );
            expected.extend([(modifier, true), (f8, true), (f8, true)]);
            if release_mask == mask {
                expected.extend([(f8, false), (modifier, false)]);
            } else {
                expected.extend([(modifier, false), (f8, false)]);
            }
        }
    }
    term.send(b"z\x1b[122;1:3u");
    wait_for_key(&server, 'z', false);
    expected.extend([(u32::from('z'), true), (u32::from('z'), false)]);
    assert_eq!(received_keys(&server), expected);
    quit(&mut term);
    assert!(term.wait(timeout).unwrap().success());
}

#[test]
fn physical_meta_and_hyper_f8_keep_both_release_orders() {
    let (server, mut term) = start(Resize::Accept, (800, 600));
    let timeout = Duration::from_secs(8);
    assert_drew(&term, timeout);
    let f8 = 0xffc5;
    let mut expected = Vec::new();
    // Explicit left Meta and right Hyper also retain the side of the key.
    for (code, mask, modifier) in [(57446, 33, 0xffe7), (57451, 17, 0xffee)] {
        for modifier_first in [false, true] {
            term.send(format!("\x1b[{code};{mask}u\x1b[19;{mask}~\x1b[19;{mask}:2~").as_bytes());
            expected.extend([(modifier, true), (f8, true), (f8, true)]);
            if modifier_first {
                term.send(format!("\x1b[{code};1:3u\x1b[19;1:3~").as_bytes());
                expected.extend([(modifier, false), (f8, false)]);
            } else {
                term.send(format!("\x1b[19;{mask}:3~\x1b[{code};1:3u").as_bytes());
                expected.extend([(f8, false), (modifier, false)]);
            }
        }
    }
    term.send(b"z\x1b[122;1:3u");
    wait_for_key(&server, 'z', false);
    expected.extend([(122, true), (122, false)]);
    assert_eq!(received_keys(&server), expected);
    quit(&mut term);
    assert!(term.wait(timeout).unwrap().success());
}

#[test]
fn an_observed_physical_modifier_survives_f8_up_after_focus_returns() {
    let (server, mut term) = start(Resize::Accept, (800, 600));
    let timeout = Duration::from_secs(8);
    assert_reports_size(&term, EXPECTED_SIZE, timeout);
    let mut expected = Vec::new();
    for (code, mask, sym) in [(57446, 33, 0xffe7), (57445, 17, 0xffed)] {
        term.send(format!("\x1b[{code};{mask}u\x1b[{code};1:3u\x1b[O\x1b[I").as_bytes());
        // The user pressed the modifier outside the window. F8-up still reports it.
        term.send(format!("\x1b[19;{mask}~\x1b[19;{mask}:3~").as_bytes());
        click_at(&mut term, 101, 101);
        term.send(format!("\x1b[{code};1:3u").as_bytes());
        click_at(&mut term, 101, 101);
        expected.extend([vec![sym], vec![]]);
    }
    term.send(b"z\x1b[122;1:3u");
    wait_for_key(&server, 'z', false);
    assert_eq!(held_keys_at_clicks(&server), expected);
    quit(&mut term);
    assert!(term.wait(timeout).unwrap().success());
}

#[test]
fn both_sides_of_an_inferred_modifier_reach_clicks_in_all_f8_release_orders() {
    let (server, mut term) = start(Resize::Accept, (800, 600));
    let timeout = Duration::from_secs(8);
    assert_reports_size(&term, EXPECTED_SIZE, timeout);
    let mut expected = Vec::new();
    let f8_sym = 0xffc5;
    for (first_code, second_code, mask, first_sym, second_sym) in [
        (57446, 57452, 33, 0xffe7, 0xffe8),
        (57451, 57445, 17, 0xffee, 0xffed),
    ] {
        let first = (first_code, first_sym, 'u');
        let second = (second_code, second_sym, 'u');
        let f8 = (19, f8_sym, '~');
        for order in [
            [first, second, f8],
            [first, f8, second],
            [second, first, f8],
            [second, f8, first],
            [f8, first, second],
            [f8, second, first],
        ] {
            term.send(b"\x1b[O\x1b[I");
            term.send(format!("\x1b[19;{mask}~\x1b[{second_code};{mask}u").as_bytes());
            let mut held = std::collections::BTreeSet::from([f8_sym, first_sym, second_sym]);
            click_at(&mut term, 101, 101);
            expected.push(held.iter().copied().collect::<Vec<_>>());
            for (code, sym, suffix) in order {
                held.remove(&sym);
                let modifier = if held.contains(&first_sym) || held.contains(&second_sym) {
                    mask
                } else {
                    1
                };
                term.send(format!("\x1b[{code};{modifier}:3{suffix}").as_bytes());
                click_at(&mut term, 101, 101);
                expected.push(held.iter().copied().collect());
            }
        }
    }
    term.send(b"z\x1b[122;1:3u");
    wait_for_key(&server, 'z', false);
    assert_eq!(held_keys_at_clicks(&server), expected);
    quit(&mut term);
    assert!(term.wait(timeout).unwrap().success());
}

fn held_keys_at_clicks(server: &FakeServer) -> Vec<Vec<u32>> {
    let mut held = std::collections::BTreeSet::new();
    let mut clicks = Vec::new();
    for request in server.requests() {
        match request {
            Request::Key { keysym, down } => {
                if down {
                    assert!(held.insert(keysym));
                } else {
                    assert!(held.remove(&keysym));
                }
            }
            Request::Pointer { buttons: 1, .. } => {
                clicks.push(held.iter().copied().collect::<Vec<_>>())
            }
            _ => {}
        }
    }
    assert!(held.is_empty());
    clicks
}

#[test]
fn a_click_that_dismisses_the_menu_keeps_its_drag_and_release_local() {
    let (server, mut term) = start(Resize::Accept, (800, 600));
    let timeout = Duration::from_secs(8);
    assert_reports_size(&term, EXPECTED_SIZE, timeout);
    assert_pixel_exact(&term, timeout);
    let before_open = term.output().len();
    term.send(F8);
    let label = "Send F8 to remote";
    assert!(wait_for_text(&term, label, timeout));
    let label_end = term
        .wait_for_after(before_open, label.as_bytes(), timeout)
        .unwrap();
    let open_query = term
        .wait_for_after(label_end, LAYOUT_QUERY, timeout)
        .unwrap();
    let screen = Screen::of(&term.output());
    let (row, col) = (0..usize::from(ROWS))
        .find_map(|r| screen.row(r).find(label).map(|c| (r, c)))
        .unwrap();
    term.send(
        format!(
            "\x1b[<16;{};{}M\x1b[<48;101;101M\x1b[<16;101;101m",
            col * 8 + 1,
            row * 17 + 1
        )
        .as_bytes(),
    );
    term.send(b"z\x1b[122;1:3u");
    wait_for_key(&server, 'z', false);
    assert_eq!(
        received_keys(&server),
        [(0xffc5, true), (0xffc5, false), (122, true), (122, false)]
    );
    assert!(
        !server
            .requests()
            .iter()
            .any(|r| matches!(r, Request::Pointer { .. }))
    );
    // A fresh press of the same button works once the desktop is visible.
    term.wait_for_after(open_query, LAYOUT_QUERY, timeout)
        .unwrap();
    click_at(&mut term, 101, 101);
    assert!(
        server
            .wait_for(timeout, |r| matches!(
                r,
                Request::Pointer {
                    x: 100,
                    y: 100,
                    buttons: 0
                }
            ))
            .is_some()
    );
    quit(&mut term);
    assert!(term.wait(timeout).unwrap().success());
}

#[test]
fn disabling_the_menu_key_leaves_f8_remote_and_the_footer_clickable() {
    let server = FakeServer::start(800, 600, Resize::Accept);
    let addr = server.addr.to_string();
    let mut term = FakeTerm::spawn(30, 15, 240, 240, &[&addr, "--menu-key", "none"]);
    term.answer_probe(NO_KEYBOARD_REPLIES);
    let timeout = Duration::from_secs(8);
    assert!(wait_for_text(&term, "commands", timeout));
    // On a press-only terminal the mapper supplies the key-up. A leading
    // Ctrl+A cannot capture either key.
    term.send(b"\x01\x1b[19~z");
    wait_for_key(&server, 'z', false);
    assert_eq!(
        received_keys(&server),
        [
            (0xffe3, true),
            (97, true),
            (97, false),
            (0xffe3, false),
            (0xffc5, true),
            (0xffc5, false),
            (122, true),
            (122, false),
        ]
    );
    let footer = Screen::of(&term.output()).row(14);
    let x = footer.find("commands").unwrap() as u16 * 8 + 1;
    click_at(&mut term, x, 14 * 16 + 1);
    assert!(wait_for_text(&term, "Command menu", timeout));
    term.send(b"q");
    assert!(term.wait(timeout).unwrap().success());
}

#[test]
fn a_graphics_error_reply_allows_a_mouse_only_menu_without_da1() {
    let server = FakeServer::start(240, 224, Resize::Accept);
    let addr = server.addr.to_string();
    let mut term = FakeTerm::spawn(30, 15, 240, 240, &[&addr, "--force", "--menu-key", "none"]);
    let timeout = Duration::from_secs(8);
    term.answer_probe(b"\x1b_Gi=1893;ENOTSUP\x1b\\");
    assert!(term.wait_for(GRAPHICS_LAYOUT_QUERY, timeout));
    let footer = Screen::of(&term.output()).row(14);
    let col = footer.find("commands").unwrap() as u16 + 1;
    click_at(&mut term, col, 15);
    assert!(wait_for_text(&term, "Command menu", timeout));
    term.send(b"q");
    assert!(term.wait(timeout).unwrap().success());
}

#[test]
fn an_empty_da1_selects_a_working_graphics_reply_or_stops_before_connecting() {
    let timeout = Duration::from_secs(5);
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let mut term = FakeTerm::with_manual_frame_replies(
        30,
        15,
        240,
        240,
        &[&addr, "--force", "--menu-key", "none"],
    );
    term.answer_probe(b"\x1b[?c");
    let status = term.wait(timeout);
    assert!(
        matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "an unusable mouse-only configuration opened a VNC connection"
    );
    assert!(!status.expect("missing startup diagnostic").success());
    assert!(contains(&term.output(), b"--menu-key f8"));

    let server = FakeServer::start(240, 224, Resize::Accept);
    let addr = server.addr.to_string();
    let mut term = FakeTerm::with_manual_frame_replies(
        30,
        15,
        240,
        240,
        &[&addr, "--force", "--menu-key", "none"],
    );
    term.answer_probe(b"\x1b[?c");
    // An invalid DA1 must not end the probe before this separate reply arrives.
    std::thread::sleep(Duration::from_millis(30));
    term.send(b"\x1b_Gi=1893;OK\x1b\\");
    assert!(term.wait_for(GRAPHICS_LAYOUT_QUERY, timeout));
    term.send(b"\x1b_Gi=1895;OK\x1b\\z");
    wait_for_key(&server, 'z', false);
    let col = Screen::of(&term.output()).row(14).find("commands").unwrap() as u16 + 1;
    click_at(&mut term, col, 15);
    assert!(wait_for_text(&term, "Command menu", timeout));
    term.send(b"q");
    assert!(term.wait(timeout).unwrap().success());
    assert_eq!(received_keys(&server), [(122, true), (122, false)]);
    assert!(
        !server
            .requests()
            .iter()
            .any(|r| matches!(r, Request::Pointer { .. }))
    );
}

#[test]
fn f8_operates_the_menu_without_a_presentation_reply() {
    let server = FakeServer::start(240, 224, Resize::Accept);
    let addr = server.addr.to_string();
    let mut term = FakeTerm::spawn(30, 15, 240, 240, &[&addr, "--force", "--menu-key", "f8"]);
    let timeout = Duration::from_secs(8);
    term.answer_probe(b"");
    term.with_reader_paused(|term| {
        assert!(
            server
                .wait_for(timeout, |r| matches!(r, Request::SetEncodings(_)))
                .is_some()
        );
        term.send(b"\x1b[19~8z");
        wait_for_key(&server, 'z', false);
    });
    assert_eq!(
        received_keys(&server),
        [(0xffc5, true), (0xffc5, false), (122, true), (122, false)]
    );
    quit(&mut term);
    assert!(term.wait(timeout).unwrap().success());
}

#[test]
fn a_silent_terminal_resumes_mouse_reporting_after_the_keyboard_menu_is_erased() {
    let timeout = Duration::from_secs(8);
    let server = FakeServer::start(240, 224, Resize::Accept);
    let addr = server.addr.to_string();
    let mut term = FakeTerm::with_manual_frame_replies(30, 15, 240, 240, &[&addr, "--force"]);
    term.answer_probe(b"");
    let initial = term.wait_for_after(0, LAYOUT_QUERY, timeout).unwrap();
    term.send(F8);
    let label = term
        .wait_for_after(initial, b"Command menu", timeout)
        .unwrap();
    assert!(contains(&term.output()[initial..label], b"\x1b[?1003l"));
    let opened = term.wait_for_after(label, LAYOUT_QUERY, timeout).unwrap();
    click_at(&mut term, 4, 4);
    term.send(F8);
    let closed = term.wait_for_after(opened, LAYOUT_QUERY, timeout).unwrap();
    term.wait_for_after(closed, b"\x1b[?1003h", timeout)
        .unwrap();
    click_at(&mut term, 4, 4);
    term.send(b"z");
    wait_for_key(&server, 'z', false);
    assert_eq!(received_pointers(&server), [(1, 28, 56), (0, 28, 56)]);
    term.send(b"\x1b");
    assert!(
        server
            .wait_for(timeout, |r| matches!(
                r,
                Request::Key {
                    keysym: 0xff1b,
                    down: false
                }
            ))
            .is_some(),
        "a terminal that never answers frame queries must still deliver a lone Escape"
    );
    quit(&mut term);
    assert!(term.wait(timeout).unwrap().success());
}

#[test]
fn input_reaches_the_server_with_pixel_exact_coordinates() {
    let (server, mut term) = start(Resize::Accept, (1024, 768));
    // The negotiated size first, and the mapping only after it. Pixel-exact is true of
    // the 1024x768 desktop the session opens on as well -- drawn 1:1, letterboxed in the
    // middle of the area -- so on its own it says nothing about where a terminal pixel
    // lands. A click aimed at the desktop that fills the area falls in that letterbox
    // instead, outside the image, where the client drops it rather than reporting it as a
    // click on the nearest edge.
    assert_reports_size(&term, EXPECTED_SIZE, Duration::from_secs(10));
    assert_pixel_exact(&term, Duration::from_secs(10));

    // SGR-pixel mouse report: button 0 pressed at pixel 137,229. In native
    // mapping that is exactly where it should land on the remote desktop.
    term.send(b"\x1b[<0;137;229M");
    let pointer = server
        .wait_for(Duration::from_secs(10), |r| {
            matches!(r, Request::Pointer { buttons: 1, .. })
        })
        .expect("the click never arrived");
    match pointer {
        Request::Pointer { x, y, buttons } => {
            // Reports are one-based, so the pixel is 136,228.
            assert_eq!((x, y), (136, 228), "pointer landed on the wrong pixel");
            assert_eq!(buttons, 1);
        }
        other => panic!("unexpected request {other:?}"),
    }
    term.send(b"\x1b[<0;137;229m");

    // The terminal answered the keyboard-protocol probe, so it owes us a release
    // for every press and the client must not invent one.
    term.send(b"\x1b[120u");
    assert!(
        server
            .wait_for(Duration::from_secs(10), |r| matches!(
                r,
                Request::Key {
                    keysym: 0x78,
                    down: true
                }
            ))
            .is_some(),
        "the keystroke never arrived"
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !server.requests().iter().any(|r| matches!(
            r,
            Request::Key {
                keysym: 0x78,
                down: false
            }
        )),
        "a terminal that reports releases must not have them synthesised too"
    );

    // Now the release the terminal owes us.
    term.send(b"\x1b[120;1:3u");
    assert!(
        server
            .wait_for(Duration::from_secs(10), |r| matches!(
                r,
                Request::Key {
                    keysym: 0x78,
                    down: false
                }
            ))
            .is_some(),
        "the release was not forwarded"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn shift_minus_sends_underscore_and_plain_minus_still_sends_minus() {
    let (server, mut term) = start(Resize::Accept, (800, 600));
    assert_drew(&term, Duration::from_secs(10));

    term.send(b"\x1b[57441;2u"); // left Shift down
    term.send(b"\x1b[45;2u\x1b[45;2:2u"); // minus pressed and repeated with Shift
    term.send(b"\x1b[57412;2u\x1b[57412;2:3u"); // keypad subtract while Shift is down
    // Letting go of Shift first means the final release only names the base key.
    term.send(b"\x1b[57441;1:3u\x1b[45;1:3u");
    term.send(b"\x1b[45u\x1b[45;1:3u");
    // A later event marks the end of the sequence on the server.
    term.send(b"\x1b[120u");
    server
        .wait_for(Duration::from_secs(10), |r| {
            matches!(r, Request::Key { keysym: 120, .. })
        })
        .expect("the server did not receive the last key");

    assert_eq!(
        received_keys(&server),
        [
            (0xffe1, true),
            (u32::from('_'), true),
            (u32::from('_'), true),
            (u32::from('-'), true),
            (u32::from('-'), false),
            (0xffe1, false),
            (u32::from('_'), false),
            (u32::from('-'), true),
            (u32::from('-'), false),
            (120, true),
        ]
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn ctrl_shift_shortcuts_reach_the_remote_with_both_modifiers() {
    let (server, mut term) = start(Resize::Accept, (800, 600));
    assert_drew(&term, Duration::from_secs(10));

    // Rex can report both modifiers on the letter alone, including on its release.
    term.send(b"\x1b[99;6u\x1b[99;6:3u"); // Ctrl+Shift+C
    term.send(b"\x1b[118;6u\x1b[118;6:3u"); // Ctrl+Shift+V
    term.send(b"\x1b[118;5u\x1b[118;5:3u"); // Ctrl+V
    // A terminal can also report each physical modifier. Release Shift before
    // repeating and releasing A with just Ctrl held.
    term.send(b"\x1b[57448;5u\x1b[57447;6u\x1b[97;6u");
    term.send(b"\x1b[57447;5:3u\x1b[97;5:2u\x1b[97;5:3u\x1b[57448;1:3u");
    term.send(b"\x1b[122u\x1b[122;1:3u");
    server
        .wait_for(Duration::from_secs(10), |r| {
            matches!(
                r,
                Request::Key {
                    keysym: 122,
                    down: false
                }
            )
        })
        .unwrap_or_else(|| panic!("the key sequence stopped at {:?}", received_keys(&server)));

    let shift = 0xffe1;
    let ctrl = 0xffe3;
    let right_shift = 0xffe2;
    let right_ctrl = 0xffe4;
    assert_eq!(
        received_keys(&server),
        [
            (shift, true),
            (ctrl, true),
            (u32::from('C'), true),
            (u32::from('C'), false),
            (shift, false),
            (ctrl, false),
            (shift, true),
            (ctrl, true),
            (u32::from('V'), true),
            (u32::from('V'), false),
            (shift, false),
            (ctrl, false),
            (ctrl, true),
            (u32::from('v'), true),
            (u32::from('v'), false),
            (ctrl, false),
            (right_ctrl, true),
            (right_shift, true),
            (u32::from('A'), true),
            (right_shift, false),
            (u32::from('A'), false),
            (u32::from('a'), true),
            (u32::from('a'), false),
            (right_ctrl, false),
            (u32::from('z'), true),
            (u32::from('z'), false),
        ]
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn azerty_minus_and_underscore_keep_their_layout_and_release_independently() {
    let (server, mut term) = start(Resize::Accept, (800, 600));
    assert_drew(&term, Duration::from_secs(10));

    // On AZERTY these are two base keys, at the US positions for 6 and 8.
    term.send(b"\x1b[45::54u\x1b[95::56u\x1b[45::54;1:3u\x1b[95::56;1:3u");
    term.send(b"\x1b[57441;2u\x1b[45:54:54;2u\x1b[95:56:56;2u");
    term.send(b"\x1b[45;2:2u"); // Same Shift state, alternate omitted on this repeat.
    // A repeat after Shift-up produces minus. The other key is released with
    // no repeat, so its up must still match the 8 that went down.
    term.send(b"\x1b[57441;1:3u\x1b[45::54;1:2u\x1b[95::56;1:3u\x1b[45::54;1:3u");
    // Only the PC-101 position is reported on these fresh presses. The first
    // key-up clears the mask, so the inferred Shift is already up at that point.
    term.send(b"\x1b[45::54;2u\x1b[95::56;2u\x1b[45;1:3u\x1b[95;1:3u");
    term.send(b"\x1b[120u");
    wait_for_key(&server, 'x', true);

    let minus = u32::from('-');
    let underscore = u32::from('_');
    let six = u32::from('6');
    let eight = u32::from('8');
    assert_eq!(
        received_keys(&server),
        [
            (minus, true),
            (underscore, true),
            (minus, false),
            (underscore, false),
            (0xffe1, true),
            (six, true),
            (eight, true),
            (six, true),
            (0xffe1, false),
            (six, false),
            (minus, true),
            (eight, false),
            (minus, false),
            (0xffe1, true),
            (six, true),
            (eight, true),
            (0xffe1, false),
            (six, false),
            (eight, false),
            (120, true),
        ]
    );
    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn mask_only_control_matches_the_mouse_report_at_each_click() {
    let (server, mut term) = start(Resize::Accept, (1024, 768));
    assert_pixel_exact(&term, Duration::from_secs(10));
    // Release Ctrl first. The unmodified click is the first report of it.
    term.send(b"\x1b[118;5u\x1b[<0;100;100M\x1b[<0;100;100m\x1b[118;1:3u");
    // Release V first, then click with Ctrl still held.
    term.send(b"\x1b[118;5u\x1b[118;5:3u\x1b[<16;100;100M\x1b[<0;100;100m");
    term.send(b"\x1b[120u");
    wait_for_key(&server, 'x', true);
    let mut ctrl = false;
    let mut clicks = Vec::new();
    for event in server.requests() {
        match event {
            Request::Key {
                keysym: 0xffe3,
                down,
            } => ctrl = down,
            Request::Pointer { buttons, .. } => clicks.push((buttons, ctrl)),
            _ => {}
        }
    }
    assert_eq!(clicks, [(1, false), (0, false), (1, true), (0, false)]);
    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn only_an_authenticated_view_only_password_locks_the_input_toggle() {
    use std::io::{Seek, Write};
    use std::os::unix::fs::OpenOptionsExt;
    let full = [0x6e, 0x09, 0x7a, 0x8e, 0x12, 0x69, 0x58, 0xd8];
    let watch = [0xf6, 0xda, 0xc8, 0xd0, 0x2c, 0xad, 0x5c, 0x60];
    let two = [full, watch].concat();
    // DES responses to the fake server's fixed challenge for these vncpasswd entries.
    let watch_response = [
        0x94, 0xb1, 0xe4, 0xae, 0xa9, 0x28, 0x93, 0x92, 0xcc, 0xc3, 0x74, 0xb1, 0xb9, 0x9f, 0x96,
        0xb7,
    ];
    let full_response = [
        0x56, 0x9f, 0xea, 0x3c, 0xef, 0x68, 0x56, 0x39, 0x62, 0x97, 0xb7, 0x12, 0xc1, 0x29, 0xc7,
        0xc6,
    ];
    let path = std::env::temp_dir().join(format!("tvnc-test-passwd-{}", std::process::id()));
    let mut passwd = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    let timeout = Duration::from_secs(8);
    for (file, auth_response, locked) in [
        (two.as_slice(), Some(watch_response), true),
        (two.as_slice(), None, false),
        (full.as_slice(), Some(full_response), false),
    ] {
        passwd.rewind().unwrap();
        passwd.set_len(0).unwrap();
        passwd.write_all(file).unwrap();
        let (server, mut term) = start_with(
            Extensions {
                auth_response,
                ..Default::default()
            },
            (800, 600),
            &["--view-only", "--passwd", path.to_str().unwrap()],
        );
        assert!(wait_for_text(&term, "view-only", timeout));
        term.send(b"\x1b[19~v");
        let notice = if locked {
            "View-only password"
        } else {
            "input enabled"
        };
        assert!(wait_for_text(&term, notice, timeout));
        term.send(b"\x1b[27ux\x1b[120;1:3u");
        if !locked {
            wait_for_key(&server, 'x', false);
        }
        quit(&mut term);
        assert!(term.wait(timeout).unwrap().success());
        assert_eq!(received_keys(&server).is_empty(), locked);
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn control_held_in_view_only_is_sent_when_a_remote_shortcut_needs_it() {
    let (server, mut term) = start_with(Extensions::default(), (800, 600), &["--view-only"]);
    let timeout = Duration::from_secs(10);
    let ctrl_click = |term: &mut FakeTerm, label: &str| {
        assert!(wait_for_text(term, label, timeout));
        let screen = Screen::of(&term.output());
        let (col, row) = (0..ROWS)
            .find_map(|row| {
                screen
                    .row(usize::from(row))
                    .find(label)
                    .map(|col| (col as u16, row))
            })
            .expect(label);
        let (x, y) = (col * (PIXELS.0 / COLS) + 1, row * (PIXELS.1 / ROWS) + 1);
        term.send(format!("\x1b[<16;{x};{y}M\x1b[<16;{x};{y}m").as_bytes());
    };
    assert_drew(&term, timeout);
    // Keep physical Ctrl held through the local menu and the first remote Ctrl+A.
    term.send(b"\x1b[57442;5u\x1b[97;5u\x1b[97;5:3u");
    ctrl_click(&mut term, "F8 commands");
    ctrl_click(&mut term, "Toggle view-only");
    ctrl_click(&mut term, "F8 commands");
    term.send(b"\x1b[97;5u\x1b[97;5:3u\x1b[57442;1:3u\x1b[120u\x1b[120;1:3u");
    wait_for_key(&server, 'x', false);
    assert_eq!(
        received_keys(&server),
        [
            (0xffe3, true),
            (u32::from('a'), true),
            (u32::from('a'), false),
            (0xffe3, false),
            (120, true),
            (120, false),
        ]
    );
    quit(&mut term);
    assert!(term.wait(timeout).unwrap().success());
}

#[test]
fn clicking_the_status_hint_toggles_the_command_menu() {
    let server = FakeServer::start(800, 600, Resize::Accept);
    let addr = server.addr.to_string();
    let (cols, rows) = (80, 24);
    let (cw, ch) = (8, 16);
    let mut term = FakeTerm::spawn(cols, rows, cols * cw, rows * ch, &[&addr]);
    term.answer_probe(GHOSTTY_REPLIES);
    let timeout = Duration::from_secs(6);
    let hint = "F8 commands";
    assert!(
        wait_for_text(&term, hint, timeout),
        "the status hint was never drawn"
    );
    let footer = Screen::of(&term.output()).row(usize::from(rows - 1));
    let start = footer
        .find(hint)
        .expect("missing command hint on bottom row") as u16;
    let x = |col: u16| col * cw + 1;
    let y = (rows - 1) * ch + 1;
    click_at(&mut term, x(start - 1), y); // gap after the latency figure
    click_at(&mut term, x(start), y);
    assert!(
        wait_for_text(&term, "Command menu", timeout),
        "clicking the shortcut did not open the menu"
    );
    assert!(
        wait_for_text(&term, "Toggle statistics", timeout),
        "the last command must fit above the footer"
    );
    click_at(&mut term, x(start + hint.len() as u16 - 1), y);
    wait_for_screen(&term, timeout, |screen| !screen.contains("Command menu"))
        .expect("clicking the label did not close the menu");
    assert!(
        !server
            .requests()
            .iter()
            .any(|r| matches!(r, Request::Pointer { .. } | Request::Key { .. }))
    );
    // The abbreviated button remains clickable. Its former first cell is now
    // plain status text and must no longer take keyboard focus.
    let mark = term.output().len();
    term.send(b"\x1b[19~c\x1b[27u");
    term.drawn_after(mark, 3, timeout)
        .expect("no frames after enabling statistics");
    let footer = Screen::of(&term.output()).row(usize::from(rows - 1));
    assert!(footer.trim_end().ends_with("F8"), "{footer:?}");
    click_at(&mut term, x(start), y);
    term.send(b"\x1b[122u\x1b[122;1:3u");
    wait_for_key(&server, 'z', false);
    quit(&mut term);
    assert!(term.wait(timeout).expect("client did not exit").success());
}

#[test]
fn a_one_row_menu_keeps_keyboard_and_clicks_local() {
    let timeout = Duration::from_secs(8);
    let (server, mut term) = start(Resize::Accept, (800, 600));
    assert_pixel_exact(&term, timeout);
    term.send(F8);
    assert!(wait_for_text(&term, "Command menu", timeout));
    term.resize(40, 1, 320, 16);
    assert!(wait_for_text(&term, "q Quit", timeout));
    term.send(b"\x1b[27u");
    assert!(wait_for_text(&term, "F8 commands", timeout));
    quit(&mut term);
    assert!(term.wait(timeout).unwrap().success());
    assert!(received_keys(&server).is_empty());

    // Each fake server accepts one connection; exercise the initially collapsed case too.
    drop(term);
    drop(server);
    let server = FakeServer::start(800, 600, Resize::Accept);
    let addr = server.addr.to_string();
    let mut term = FakeTerm::spawn(40, 1, 320, 16, &[&addr, "--menu-key", "none"]);
    term.answer_probe(GHOSTTY_REPLIES);
    assert!(
        wait_for_text(&term, "commands", timeout),
        "one-row startup: {}",
        show(&term.output())
    );
    let click_label = |term: &mut FakeTerm, label: &str| {
        let col = Screen::of(&term.output()).row(0).find(label).unwrap();
        click_at(term, col as u16 * 8 + 1, 1);
    };
    click_label(&mut term, "commands");
    assert!(
        wait_for_text(&term, "Esc Close", timeout),
        "one-row footer after click: {:?}, {}",
        Screen::of(&term.output()).row(0).trim_end(),
        tail(&term.output())
    );
    click_label(&mut term, "Close");
    assert!(wait_for_text(&term, "commands", timeout));
    click_label(&mut term, "commands");
    assert!(wait_for_text(&term, "q Quit", timeout));
    term.resize(40, 15, 320, 240);
    assert!(wait_for_text(&term, "Command menu", timeout));
    assert!(
        server
            .wait_for(timeout, |r| matches!(
                r,
                Request::SetDesktopSize {
                    width: 320,
                    height: 224,
                    ..
                }
            ))
            .is_some()
    );
    term.resize(40, 1, 320, 16);
    assert!(wait_for_text(&term, "q Quit", timeout));
    click_label(&mut term, "Quit");
    assert!(term.wait(timeout).unwrap().success());
    assert!(
        !server
            .requests()
            .iter()
            .any(|r| matches!(r, Request::Key { .. } | Request::Pointer { .. }))
    );
}

#[test]
fn scrolling_the_menu_changes_the_click_target_only_after_redraw() {
    let server = FakeServer::start(640, 176, Resize::Accept);
    let addr = server.addr.to_string();
    let mut term = FakeTerm::spawn(80, 12, 640, 192, &[&addr, "--fps", "30"]);
    term.answer_probe(GHOSTTY_REPLIES);
    let timeout = Duration::from_secs(8);
    assert!(wait_for_text(&term, "F8 commands", timeout));
    term.send(F8);
    let label = "Send F8 to remote";
    assert!(wait_for_text(&term, label, timeout));
    let screen = Screen::of(&term.output());
    let (row, col) = (0..11)
        .find_map(|r| screen.row(r).find(label).map(|c| (r, c)))
        .unwrap();
    let (x, y) = (col as u16 * 8 + 1, row as u16 * 16 + 1);
    let wheel = format!("\x1b[<65;{x};{y}M");

    // Hold the visible frame still while the viewer processes a wheel event.
    // Even a scrolled frame already queued for output cannot be clicked yet.
    let tapped = term.with_output_stalled(|term| {
        term.send(wheel.as_bytes());
        std::thread::sleep(Duration::from_millis(150));
        click_at(term, x, y);
        server.wait_for(timeout, |r| {
            matches!(
                r,
                Request::Key {
                    keysym: 0xffc5,
                    down: false
                }
            )
        })
    });
    assert!(
        tapped.is_some(),
        "clicking the visible Send F8 row sent no F8"
    );
    assert!(wait_for_screen(&term, timeout, |s| !s.contains(label)).is_some());

    // Once the new row really is visible, the same click must execute Quit.
    term.send(F8);
    assert!(wait_for_screen(&term, timeout, |s| s.row(row).contains(label)).is_some());
    term.send(wheel.as_bytes());
    assert!(wait_for_screen(&term, timeout, |s| s.row(row).contains("Quit")).is_some());
    click_at(&mut term, x, y);
    assert!(
        term.wait(timeout)
            .expect("the displayed Quit row did not quit")
            .success()
    );
    assert_eq!(received_keys(&server), [(0xffc5, true), (0xffc5, false)]);
}

/// A single wheel notch replaces the bottom visible command, Send F8, with Quit.
fn five_row_menu(replies: &[u8]) -> (FakeServer, FakeTerm, u16, u16) {
    let server = FakeServer::start(640, 64, Resize::Accept);
    let addr = server.addr.to_string();
    let mut term = FakeTerm::spawn(80, 5, 640, 80, &[&addr, "--fps", "30"]);
    let timeout = Duration::from_secs(8);
    term.answer_probe(replies);
    assert!(wait_for_text(&term, "F8 commands", timeout));
    term.send(F8);
    let label = "Send F8 to remote";
    let end = term.wait_for_after(0, label.as_bytes(), timeout).unwrap();
    let complete = term.wait_for_after(end, END_SYNC, timeout).unwrap();
    term.wait_for_after(complete, FRAME, timeout).unwrap();
    let screen = Screen::of(&term.output());
    let (row, col) = (0..4)
        .find_map(|r| screen.row(r).find(label).map(|c| (r, c)))
        .unwrap();
    (server, term, col as u16 * 8 + 1, row as u16 * 16 + 1)
}

#[test]
fn a_scroll_frame_waiting_in_a_nonfull_pty_cannot_change_the_visible_click_target() {
    const MARKER: &[u8] = b"\0\0\0";
    let timeout = Duration::from_secs(8);
    let graphics_only = GHOSTTY_REPLIES.strip_suffix(b"\x1b[?62;22c").unwrap();
    for replies in [GHOSTTY_REPLIES, NO_KEYBOARD_REPLIES, graphics_only] {
        let (server, mut term, x, y) = five_row_menu(replies);
        let before = term.output().len();
        let tapped = term
            .with_reader_paused(|term| {
                let buffered = term.unread_bytes();
                term.send(format!("\x1b[<65;{x};{y}M").as_bytes());
                let deadline = std::time::Instant::now() + timeout;
                // More than an idle frame is waiting on the master. The old
                // command is still on screen since the reader has not advanced.
                while term.unread_bytes() < buffered + 100 {
                    if std::time::Instant::now() >= deadline {
                        return Err("no scroll frame reached the PTY");
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                if !term.try_write_output(MARKER) {
                    return Err("the PTY had no room after the scroll frame");
                }
                click_at(term, x, y);
                Ok(server
                    .wait_for(timeout, |r| {
                        matches!(
                            r,
                            Request::Key {
                                keysym: 0xffc5,
                                down: false
                            }
                        )
                    })
                    .is_some())
            })
            .unwrap();
        let end = term.wait_for_after(before, MARKER, timeout).unwrap() - MARKER.len();
        let output = term.output();
        assert!(
            frame_containing(&output[before..end], b"Quit").is_some(),
            "the complete scroll frame must precede the marker and click"
        );
        assert!(
            tapped,
            "the unread Quit row replaced the visible Send F8 target"
        );
        assert!(wait_for_screen(&term, timeout, |s| !s.contains("Send F8 to remote")).is_some());
        assert_eq!(received_keys(&server), [(0xffc5, true), (0xffc5, false)]);
        quit(&mut term);
        assert!(term.wait(timeout).unwrap().success());
    }
}

#[test]
fn a_second_click_on_the_closing_menu_cannot_reach_the_remote_desktop() {
    let (server, mut term, x, y) = five_row_menu(GHOSTTY_REPLIES);
    let timeout = Duration::from_secs(8);
    let before = term.output().len();
    // The label is three 8-pixel cells inside the panel. Its entire row is clickable.
    let edge = x - 3 * 8;
    let processed = term.with_reader_paused(|term| {
        click_at(term, edge, y);
        click_at(term, edge - 1, y);
        click_at(term, edge, y);
        term.send(b"z\x1b[122;1:3u");
        server
            .wait_for(timeout, |r| {
                matches!(
                    r,
                    Request::Key {
                        keysym: 122,
                        down: false
                    }
                )
            })
            .is_some()
    });
    assert!(
        processed,
        "the key after the clicks did not reach the server"
    );
    assert_eq!(
        received_keys(&server),
        [(0xffc5, true), (0xffc5, false), (122, true), (122, false)]
    );
    assert!(
        !server
            .requests()
            .iter()
            .any(|r| matches!(r, Request::Pointer { .. })),
        "a click reached the remote before the menu dismissal was acknowledged"
    );
    assert!(wait_for_screen(&term, timeout, |s| !s.contains("Send F8 to remote")).is_some());
    term.wait_for_after(before, LAYOUT_QUERY, timeout).unwrap();
    click_at(&mut term, edge - 1, y);
    assert!(server.wait_for(timeout, |r| matches!(r, Request::Pointer { buttons: 0, x: rx, y: ry } if (*rx, *ry) == (edge-2, y-1))).is_some());
    quit(&mut term);
    assert!(term.wait(timeout).unwrap().success());
}

#[test]
fn an_old_footer_cannot_click_the_desktop_until_the_resized_frame_is_acknowledged() {
    let timeout = Duration::from_secs(8);
    let reply = b"\x1b[?62;22c";
    // Grow the window with more rows, then with taller cells on the same grid.
    for (rows, desktop_height) in [(10, 144), (5, 128)] {
        let server = FakeServer::start(640, 64, Resize::Accept);
        let addr = server.addr.to_string();
        let mut term = FakeTerm::with_manual_frame_replies(80, 5, 640, 80, &[&addr]);
        term.answer_probe(NO_KEYBOARD_REPLIES);
        let initial = term.wait_for_after(0, LAYOUT_QUERY, timeout).unwrap();
        let footer = Screen::of(&term.output()).row(4);
        let x = footer.find("commands").unwrap() as u16 * 8 + 1;
        term.send(reply);
        term.send(b"\x1b[<0;9;9M\x1b[<32;17;17M");
        term.send(b"x");
        wait_for_key(&server, 'x', false);

        term.with_reader_paused(|term| {
            term.resize(80, rows, 640, 160);
            server.wait_for(timeout, |r| {
                matches!(r, Request::SetDesktopSize { width: 640, height, .. } if *height == desktop_height)
            }).expect("the client did not adopt the new terminal dimensions");
            term.send(b"\x1b[<0;17;17m");
            click_at(term, x, 65);
            term.send(b"y");
            wait_for_key(&server, 'y', false);
        });
        let mut expected = vec![(1, 8, 8), (1, 16, 16), (0, 16, 16)];
        assert_eq!(
            received_pointers(&server),
            expected,
            "only the drag and its release may reach the desktop"
        );

        term.wait_for_after(initial, LAYOUT_QUERY, timeout).unwrap();
        assert!(wait_for_text(
            &term,
            &format!("640x{desktop_height}"),
            timeout
        ));
        term.send(reply);
        click_at(&mut term, x, 65);
        term.send(b"z");
        wait_for_key(&server, 'z', false);
        expected.extend([(1, x - 1, 64), (0, x - 1, 64)]);
        assert_eq!(received_pointers(&server), expected);
        quit(&mut term);
        assert!(term.wait(timeout).unwrap().success());
    }
}

#[test]
fn a_menu_with_an_unacknowledged_open_frame_owns_clicks_until_its_close_reply() {
    let server = FakeServer::start(240, 224, Resize::Accept);
    let addr = server.addr.to_string();
    let mut term = FakeTerm::with_manual_frame_replies(30, 15, 240, 240, &[&addr]);
    let timeout = Duration::from_secs(8);
    let da1 = b"\x1b[?62;22c";
    term.answer_probe(NO_KEYBOARD_REPLIES);
    let initial = term.wait_for_after(0, LAYOUT_QUERY, timeout).unwrap();
    term.send(da1);
    term.send(b"\x1b[<0;9;9M\x1b[<32;17;17M"); // Start a remote drag before the menu opens.
    term.send(F8);
    let label = "Send F8 to";
    let drawn = term
        .wait_for_after(initial, label.as_bytes(), timeout)
        .unwrap();
    let opened = term.wait_for_after(drawn, LAYOUT_QUERY, timeout).unwrap();
    let screen = Screen::of(&term.output());
    let (row, col) = (0..14)
        .find_map(|r| screen.row(r).find(label).map(|c| (r, c)))
        .unwrap();
    let (x, y) = (col as u16 * 8 + 1, row as u16 * 16 + 1);
    term.with_reader_paused(|term| {
        term.send(F8);
        term.send(b"\x1b[<0;17;17m");
        click_at(term, x, y);
        term.send(b"z");
        wait_for_key(&server, 'z', false);
    });
    term.wait_for_after(opened, LAYOUT_QUERY, timeout).unwrap();
    term.send(da1); // Only the open frame has been acknowledged.
    click_at(&mut term, x, y);
    term.send(b"y");
    wait_for_key(&server, 'y', false);
    let mut expected = vec![(1, 8, 8), (1, 16, 16), (0, 16, 16)];
    assert_eq!(
        received_pointers(&server),
        expected,
        "only the original drag and its release"
    );
    term.send(da1);
    click_at(&mut term, x, y);
    term.send(b"x");
    wait_for_key(&server, 'x', false);
    expected.extend([(1, x - 1, y - 1), (0, x - 1, y - 1)]);
    assert_eq!(received_pointers(&server), expected);
    assert_eq!(
        received_keys(&server),
        [
            (122, true),
            (122, false),
            (121, true),
            (121, false),
            (120, true),
            (120, false)
        ]
    );
    quit(&mut term);
    assert!(term.wait(timeout).unwrap().success());
}

#[test]
fn a_cell_mouse_can_use_the_abbreviated_shortcut_in_a_short_view_only_pane() {
    let server = FakeServer::start(800, 600, Resize::Accept);
    let addr = server.addr.to_string();
    let (cols, rows) = (200, 12);
    let mut term = FakeTerm::spawn(cols, rows, cols * 8, rows * 16, &[&addr, "--view-only"]);
    let replies = std::str::from_utf8(GHOSTTY_REPLIES)
        .unwrap()
        .replace("?1016;2", "?1016;0");
    term.answer_probe(replies.as_bytes());
    let timeout = Duration::from_secs(6);
    assert!(wait_for_text(&term, "F8 commands", timeout));
    term.send(b"\x1b[19~c\x1b[27u"); // enable statistics and close the menu
    assert!(wait_for_text(&term, "dropped  F8", timeout));
    let footer = Screen::of(&term.output()).row(usize::from(rows - 1));
    let col = footer.find("F8").unwrap() as u16 + 1;
    click_at(&mut term, col, rows);
    assert!(wait_for_text(&term, "Command menu", timeout));

    // The last command starts below the viewport. Scroll to it and click the
    // displayed row to switch statistics off again.
    let wheel = format!("\x1b[<65;{};5M", cols / 2);
    term.send(wheel.repeat(24).as_bytes());
    assert!(wait_for_text(&term, "Toggle statistics", timeout));
    click_at(&mut term, cols / 2, rows - 1);
    assert!(wait_for_text(&term, "F8 commands", timeout));
    assert!(
        !server
            .requests()
            .iter()
            .any(|r| matches!(r, Request::Pointer { .. } | Request::Key { .. }))
    );
    term.send(b"q");
    assert!(term.wait(timeout).expect("client did not exit").success());
}

#[test]
fn the_command_menu_holds_the_focus_until_escape() {
    // An unbound key stays inside the menu. Escape returns focus to the desktop.
    let (server, mut term) = start(Resize::Accept, (1024, 768));
    assert_pixel_exact(&term, Duration::from_secs(10));

    term.send(F8);
    assert!(
        term.wait_for(b"Renegotiate the remote size", Duration::from_secs(10)),
        "the menu never appeared"
    );

    // An ordinary key changes nothing, so the box is neither taken down nor said again:
    // the chrome is diffed, and a menu that has not changed is not written. So what settles
    // it is that frames kept coming and none of them took the box down.
    //
    // The wait is not a deadline for an answer -- there is no answer to wait for -- but
    // long enough for a wrong one to have gone out in.
    term.send(b"\x1b[120u");
    std::thread::sleep(Duration::from_millis(500));
    term.drawn_after(term.output().len(), 2, Duration::from_secs(10))
        .expect("the client stopped drawing with the menu up");
    assert!(
        Screen::of(&term.output()).contains("Command menu"),
        "an ordinary key put the menu away"
    );

    // And it went nowhere else either: with the focus in the menu there is nothing
    // behind it to type into.
    assert!(
        !server.requests().iter().any(|r| matches!(
            r,
            Request::Key {
                keysym: 0x78,
                down: true
            }
        )),
        "a key typed at the menu was forwarded to the server"
    );

    term.send(b"\x1b");
    let cleared = wait_for_screen(&term, Duration::from_secs(10), |screen| {
        !screen.contains("Command menu")
    })
    .unwrap_or_else(|| {
        panic!(
            "the menu was never cleared, so it is still on screen: {}",
            tail(&term.output())
        )
    });

    // And it stays away. Watched from the clearing rather than from the keystroke,
    // because the frame in flight when the escape went out had been composed before the
    // client had seen it, and holds the box for that reason alone.
    let since = term
        .drawn_after(cleared, 2, Duration::from_secs(10))
        .expect("the client stopped drawing once the menu was dismissed");
    assert!(
        !contains(&since, b"Renegotiate the remote size"),
        "escape did not put the menu away"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn clicking_a_scaling_option_selects_that_one() {
    // The menu is the only way to reach a scaling mode by name. The key cannot: one
    // binding for four modes can only step to the next.
    //
    // The server refuses to resize, so the modes are told apart by what they do to a
    // 1024x768 desktop in a 1600x833 area. Native falls back, integer scales by one
    // and 1:1 is itself -- all three pixel-exact. Only fit resamples, so "scaled" is
    // proof that the click landed on fit itself: one step on from the mode native fell
    // back to would have been native again.
    let (server, mut term) = start(Resize::Refuse, (1024, 768));
    assert!(
        term.wait_for(b"1:1", Duration::from_secs(10)),
        "never fell back from native: {}",
        tail(&term.output())
    );

    term.send(F8);
    assert!(
        term.wait_for(b"Scaling", Duration::from_secs(10)),
        "the menu never appeared: {}",
        tail(&term.output())
    );

    assert!(wait_for_text(&term, "Integer", Duration::from_secs(10)));
    let screen = Screen::of(&term.output());
    let (row, col) = (0..usize::from(ROWS))
        .find_map(|row| {
            let line = screen.row(row);
            line.contains("Native")
                .then(|| (row, line.find("Fit").unwrap()))
        })
        .expect("missing scaling choices");
    click_at(&mut term, col as u16 * 8 + 1, row as u16 * 17 + 1);

    // Read the screen rather than the stream: the chrome is diffed, so a note replacing
    // another reaches the terminal as the cells that differ and a cursor move, and the
    // phrase is nowhere in the bytes even though it is on screen.
    assert!(
        wait_for_text(&term, "scaling: scaled", Duration::from_secs(10)),
        "the click did not select fit: {}",
        Screen::of(&term.output()).row(1)
    );
    // Where the stream had got to with the choice made, for the next claim to watch from.
    let selected = term.output().len();

    // The menu stays up, and shows the choice it just made: the brackets mark the mode
    // in force, so they move to fit. Only the dismissal takes the box down, which is
    // what makes the row a control you can watch rather than one that ends the session
    // with the menu.
    //
    // Watched from the selection on, so that a frame composed before the click cannot
    // stand in for the box having stayed.
    term.drawn_after(selected, 2, Duration::from_secs(10))
        .expect("the client stopped drawing after the click");
    assert!(
        Screen::of(&term.output()).contains("Command menu"),
        "the menu went away on a click that was not the dismissal"
    );
    // The brackets are read off the screen for the same reason the note above them is: they
    // moved when the click landed, and a window that starts after that is a window they have
    // already gone out in.
    assert!(
        wait_for_text(&term, "[Fit]", Duration::from_secs(10)),
        "the option in force is not the one that was clicked: {}",
        tail(&term.output())
    );

    // And the click went to the menu alone. A button that reached the remote as well
    // would have pressed whatever the box was covering.
    assert!(
        server
            .wait_for(Duration::from_secs(1), |r| matches!(
                r,
                Request::Pointer { buttons: 1, .. }
            ))
            .is_none(),
        "the click was forwarded to the server as well"
    );

    term.send(b"q"); // The menu still owns the keyboard.
    assert!(term.wait(Duration::from_secs(10)).unwrap().success());
}

#[test]
fn clicking_the_close_button_puts_the_notification_away() {
    // A click on the popup's `[x]` has to take
    // the box off the screen before its linger is up -- and take it off properly, the
    // message being text that no repaint of the tiles under it would erase.
    let (server, mut term) = start(Resize::Accept, (1024, 768));
    assert_drew(&term, Duration::from_secs(10));

    // Refresh through the menu, then dismiss it to test the notification alone.
    term.send(b"\x1b[19~f\x1b[27u");
    assert!(
        term.wait_for(b"full refresh requested", Duration::from_secs(10)),
        "the notification never appeared: {}",
        tail(&term.output())
    );

    // Where the button is. The message is 22 columns and the box adds eight -- three of
    // gap, one for the `x`, two of padding either side -- so it is 30 wide, held two
    // columns off the right of 200 and one row down. That puts the `x` at cell 195,2,
    // which in 8x17 cells is pixel 1560,34; mouse reports are one-based.
    //
    // Checked rather than assumed, because a click aimed at a box that has moved would
    // fail somewhere far less obvious than here.
    let out = term.output();
    let at = find(&out, b"\x1b[3;169H").unwrap_or_else(|| {
        panic!(
            "the notification does not start at cell 169,3 any more: {}",
            tail(&out)
        )
    });
    assert!(
        contains(
            &out[at..(at + 120).min(out.len())],
            b"full refresh requested"
        ),
        "cell 169,3 is no longer the message's row, so the click would miss"
    );

    term.send(b"\x1b[<0;1561;35M");
    term.send(b"\x1b[<0;1561;35m");

    // Two seconds is far more than the frame or two the client needs and still well
    // inside the note's four-second linger, so what took the box off was the click
    // rather than time running out on it.
    let cleared = wait_for_screen(&term, Duration::from_secs(2), |screen| {
        !screen.contains("full refresh requested")
    })
    .unwrap_or_else(|| {
        panic!(
            "the notification is still visible after clicking close: {}",
            tail(&term.output())
        )
    });
    let since = term
        .drawn_after(cleared, 2, Duration::from_secs(10))
        .expect("the client stopped drawing once the note was closed");
    assert!(
        !contains(&since, b"full refresh requested"),
        "the message is still being redrawn: {}",
        tail(&since)
    );

    // And the click went to the popup alone: a press that reached the remote as well
    // would have landed on whatever the box was covering.
    assert!(
        server
            .wait_for(Duration::from_secs(1), |r| matches!(
                r,
                Request::Pointer { buttons: 1, .. }
            ))
            .is_none(),
        "the click was forwarded to the server as well"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn the_close_button_answers_while_the_menu_is_up() {
    // Which is most of the time a note is on screen: half the commands that raise one
    // are reached through the menu, and a click on a menu item leaves the box up. The
    // popup is drawn over the menu, so it has to be clickable over it too -- otherwise
    // the menu, which takes the whole pointer while it is up, eats the click.
    let (_server, mut term) = start(Resize::Accept, (1024, 768));
    assert_drew(&term, Duration::from_secs(10));

    term.send(F8);
    assert!(
        term.wait_for(b"Renegotiate the remote size", Duration::from_secs(10)),
        "the menu never appeared: {}",
        tail(&term.output())
    );

    // A local command still works with the menu up, and this one puts a note on screen.
    term.send(b"f");
    assert!(
        term.wait_for(b"full refresh requested", Duration::from_secs(10)),
        "the notification never appeared: {}",
        tail(&term.output())
    );

    // The button, as in `clicking_the_close_button_puts_the_notification_away`: cell
    // 195,2 of an 8x17 grid, and mouse reports are one-based.
    term.send(b"\x1b[<0;1561;35M");
    term.send(b"\x1b[<0;1561;35m");

    // As in `clicking_the_close_button_puts_the_notification_away`: inside the linger, so
    // the click must dismiss the note before it expires.
    let cleared = wait_for_screen(&term, Duration::from_secs(2), |screen| {
        !screen.contains("full refresh requested")
    })
    .unwrap_or_else(|| {
        panic!(
            "the menu swallowed the click on the popup drawn over it: {}",
            tail(&term.output())
        )
    });
    let since = term
        .drawn_after(cleared, 2, Duration::from_secs(10))
        .expect("the client stopped drawing once the note was closed");
    assert!(
        !contains(&since, b"full refresh requested"),
        "the message is still being redrawn: {}",
        tail(&since)
    );
    // And the menu is still up: the cross closes the note, not the box behind it.
    assert!(
        Screen::of(&term.output()).contains("Command menu"),
        "closing the notification put the menu away too: {}",
        tail(&since)
    );

    term.send(b"q");
    assert!(term.wait(Duration::from_secs(10)).unwrap().success());
}

#[test]
fn a_terminal_without_key_releases_gets_them_synthesised() {
    // Otherwise the remote would hold every key down for ever.
    let server = FakeServer::start(1024, 768, Resize::Accept);
    let addr = server.addr.to_string();
    let mut term = FakeTerm::spawn(COLS, ROWS, PIXELS.0, PIXELS.1, &[&addr, "--fps", "10"]);
    term.answer_probe(NO_KEYBOARD_REPLIES);
    assert_drew(&term, Duration::from_secs(15));

    // A legacy terminal sends the character produced by Shift+minus directly.
    term.send(b"_");
    assert!(
        server
            .wait_for(Duration::from_secs(10), |r| matches!(
                r,
                Request::Key {
                    keysym: 0x5f,
                    down: true
                }
            ))
            .is_some(),
        "the keystroke never arrived"
    );
    assert!(
        server
            .wait_for(Duration::from_secs(10), |r| matches!(
                r,
                Request::Key {
                    keysym: 0x5f,
                    down: false
                }
            ))
            .is_some(),
        "no release followed the press"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn view_only_sends_nothing() {
    let server = FakeServer::start(800, 600, Resize::Accept);
    let addr = server.addr.to_string();
    let mut term = FakeTerm::spawn(
        COLS,
        ROWS,
        PIXELS.0,
        PIXELS.1,
        &[&addr, "--view-only", "--scale", "fit", "--fps", "10"],
    );
    term.answer_probe(GHOSTTY_REPLIES);
    assert!(term.wait_for(b"view-only", Duration::from_secs(10)));

    term.send(b"\x1b[<0;100;100M");
    term.send(b"hello");
    std::thread::sleep(Duration::from_millis(500));

    let sent_input = server.requests().iter().any(|r| {
        matches!(
            r,
            Request::Key { .. } | Request::Pointer { .. } | Request::CutText(_)
        )
    });
    assert!(
        !sent_input,
        "view-only forwarded input: {:?}",
        server.requests()
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn a_pasted_selection_goes_to_the_remote_clipboard() {
    let (server, mut term) = start(Resize::Accept, (800, 600));
    assert_drew(&term, Duration::from_secs(10));

    // Bracketed paste, as a terminal delivers it.
    let cut = paste(&mut term, "hello there", &server, |r| {
        matches!(r, Request::CutText(_))
    });
    match cut {
        Request::CutText(text) => assert_eq!(text, "hello there"),
        other => panic!("unexpected request {other:?}"),
    }

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn the_extended_clipboard_brings_cyrillic_back_from_the_remote() {
    // The point of the extension. Over legacy cut text this arrives as a row of
    // question marks, because Latin-1 has nowhere to put a Cyrillic letter.
    let (server, mut term) = start_with(
        Extensions {
            extended_clipboard: true,
            announce_clipboard: true,
            ..Default::default()
        },
        (800, 600),
        &["--scale", "fit"],
    );
    assert_drew(&term, Duration::from_secs(10));

    // The server announced a clipboard without sending it, so the client has to ask.
    assert!(
        server
            .wait_for(Duration::from_secs(10), |r| matches!(
                r,
                Request::ClipboardRequest
            ))
            .is_some(),
        "the client never asked for the clipboard it was told about"
    );

    // And what came back went to the local clipboard through OSC 52, in UTF-8.
    let encoded = base64::engine::general_purpose::STANDARD.encode(REMOTE_CLIPBOARD);
    assert!(
        term.wait_for(
            format!("\x1b]52;c;{encoded}").as_bytes(),
            Duration::from_secs(10)
        ),
        "the remote clipboard never reached the local one: {}",
        tail(&term.output())
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn a_pasted_selection_is_announced_first_and_sent_when_asked() {
    // With the server advertising a zero unsolicited size -- which is what the spec
    // recommends and what TigerVNC does -- a paste says "I have text" and the bytes
    // follow only when something over there asks for them.
    let (server, mut term) = start_with(
        Extensions {
            extended_clipboard: true,
            ..Default::default()
        },
        (800, 600),
        &["--scale", "fit"],
    );
    assert_drew(&term, Duration::from_secs(10));

    // Our own capabilities go out during the handshake, before any of this.
    let caps = server
        .wait_for(Duration::from_secs(10), |r| {
            matches!(r, Request::ClipboardCaps { .. })
        })
        .expect("the client never sent its clipboard capabilities");
    match caps {
        Request::ClipboardCaps { text_size, .. } => assert_eq!(
            text_size, 0,
            "a nonzero size invites the server to push every remote selection"
        ),
        other => panic!("unexpected request {other:?}"),
    }

    // The server's own capabilities have arrived by now, which is what decides how a paste
    // goes out: without them the client falls back to Latin-1 cut text, correctly, and
    // nothing would ever announce anything. `assert_drew` above is the wait for them --
    // they answer the `SetEncodings`, so no frame can have overtaken them.
    let sent = paste(&mut term, "Привет, мир", &server, |r| {
        // Either way it went, so that a fallback is caught below rather than taken for a
        // paste that never arrived and said again.
        matches!(r, Request::ClipboardNotify | Request::CutText(_))
    });
    assert!(
        matches!(sent, Request::ClipboardNotify),
        "the paste went out as {sent:?} instead of being announced, which is the fallback \
         for a server that never offered the extension"
    );
    // The fake server answers a notify with a request, so the text should follow --
    // whole, which is the other half of what the extension is for.
    let provided = server
        .wait_for(Duration::from_secs(10), |r| {
            matches!(r, Request::ClipboardProvide(_))
        })
        .expect("the announced text never followed");
    match provided {
        Request::ClipboardProvide(text) => assert_eq!(text, "Привет, мир"),
        other => panic!("unexpected request {other:?}"),
    }
    // And none of it went out as Latin-1, which is where the question marks came from.
    assert!(
        !server
            .requests()
            .iter()
            .any(|r| matches!(r, Request::CutText(_))),
        "the paste also went out as legacy cut text"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn no_clipboard_never_offers_the_extension() {
    // The pseudo-encoding is a standing offer to exchange clipboards. A session that
    // wants none should not make it, whatever the server would have done with it.
    let (server, mut term) = start_with(Extensions::default(), (800, 600), &["--no-clipboard"]);
    assert_drew(&term, Duration::from_secs(10));

    let encodings = server
        .wait_for(Duration::from_secs(10), |r| {
            matches!(r, Request::SetEncodings(_))
        })
        .expect("no encodings were sent");
    match encodings {
        Request::SetEncodings(ids) => assert!(
            !ids.contains(&EXTENDED_CLIPBOARD_ENCODING),
            "offered the extended clipboard with --no-clipboard: {ids:?}"
        ),
        other => panic!("unexpected request {other:?}"),
    }
    // And nothing extended was sent either, capabilities included.
    assert!(
        !server
            .requests()
            .iter()
            .any(|r| matches!(r, Request::ClipboardCaps { .. })),
        "sent clipboard capabilities for an encoding it never asked for"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn a_paste_outside_latin1_is_substituted_not_silently_shortened() {
    // RFB clipboard traffic is Latin-1 only. Dropping the rest moves every character
    // after it; a question mark keeps the text the same shape and is visibly a
    // substitution, which is what noVNC does too.
    let (server, mut term) = start(Resize::Accept, (800, 600));
    assert_drew(&term, Duration::from_secs(10));

    let cut = paste(&mut term, "caf\u{e9} \u{2615} tea", &server, |r| {
        matches!(r, Request::CutText(_))
    });
    match cut {
        Request::CutText(text) => {
            // The e-acute is Latin-1 and survives; the coffee cup is not and becomes
            // '?', leaving the length and the spaces where they were.
            assert_eq!(text, "caf\u{e9} ? tea", "got {text:?}");
        }
        other => panic!("unexpected request {other:?}"),
    }
    // And the user is told, rather than left wondering what happened to it.
    assert!(
        term.wait_for(b"not Latin-1", Duration::from_secs(5)),
        "the substitution was not reported: {}",
        show(&term.output())
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

/// Related: `live::a_real_server_reports_its_lock_key_state` checks the precondition,
/// that a real server sends the LED state this correction compares against at all.
#[test]
fn a_disagreeing_caps_lock_is_corrected_before_the_keystroke() {
    // The server says its caps lock is on; the terminal reports a key pressed with it
    // off. Sent as-is the letter would arrive in the wrong case, so a caps lock tap has
    // to go first.
    let ext = Extensions {
        led_caps_on: true,
        ..Extensions::default()
    };
    let (server, mut term) = start_with(ext, (800, 600), &["--scale", "fit"]);
    assert_drew(&term, Duration::from_secs(10));

    // A Kitty-protocol press of 'x' with no lock modifiers: caps lock is off locally.
    term.send(b"\x1b[120u");

    let keys = || -> Vec<(u32, bool)> {
        server
            .requests()
            .iter()
            .filter_map(|r| match r {
                Request::Key { keysym, down } => Some((*keysym, *down)),
                _ => None,
            })
            .collect()
    };
    let began = std::time::Instant::now();
    while began.elapsed() < Duration::from_secs(10) {
        if keys().iter().any(|(k, _)| *k == 0x78) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let sent = keys();
    let caps = 0xffe5u32;
    let caps_at = sent.iter().position(|(k, down)| *k == caps && *down);
    let x_at = sent.iter().position(|(k, down)| *k == 0x78 && *down);
    assert!(
        caps_at.is_some(),
        "no caps lock correction was sent: {sent:x?}"
    );
    assert!(
        caps_at < x_at,
        "the correction has to precede the keystroke: {sent:x?}"
    );
    // And it is a tap, not a key left down.
    assert!(
        sent.iter().any(|(k, down)| *k == caps && !*down),
        "caps lock was pressed and never released: {sent:x?}"
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn a_matching_caps_lock_is_left_alone() {
    // Nothing to fix, so nothing should be sent: a spurious tap would turn the remote
    // caps lock on and make every later keystroke wrong.
    let (server, mut term) = start_with(Extensions::default(), (800, 600), &["--scale", "fit"]);
    assert_drew(&term, Duration::from_secs(10));

    term.send(b"\x1b[120u");
    std::thread::sleep(Duration::from_millis(600));

    let touched_caps = server
        .requests()
        .iter()
        .any(|r| matches!(r, Request::Key { keysym: 0xffe5, .. }));
    assert!(
        !touched_caps,
        "sent a lock-key correction with no LED state to justify it: {:?}",
        server.requests()
    );

    quit(&mut term);
    term.wait(Duration::from_secs(10));
}

#[test]
fn focus_loss_releases_remote_input_while_terminal_output_is_stalled() {
    let (server, mut term) = start(Resize::Accept, (1024, 768));
    let timeout = Duration::from_secs(8);
    assert_reports_size(&term, EXPECTED_SIZE, timeout);
    assert_pixel_exact(&term, timeout);
    term.send(b"\x1b[120u\x1b[<0;101;101M");
    assert!(
        server
            .wait_for(timeout, |r| matches!(
                r,
                Request::Pointer { buttons: 1, .. }
            ))
            .is_some()
    );

    let offset = term.output().len();
    let released = term.with_output_stalled(|term| {
        // One status frame blocks in write and the next fills the writer's single slot.
        std::thread::sleep(Duration::from_millis(1300));
        term.send(b"\x1b[O");
        server
            .wait_for(timeout, |r| {
                matches!(
                    r,
                    Request::Pointer {
                        buttons: 0,
                        x: 100,
                        y: 100
                    }
                )
            })
            .is_some()
    });
    let keys = received_keys(&server);
    assert!(
        released && keys.contains(&(u32::from('x'), false)),
        "input still held while output was paused: {keys:?}"
    );
    assert!(
        term.wait_for_after(offset, b"\x1b[?1003l", timeout)
            .is_some()
    );
    quit(&mut term);
    assert!(term.wait(timeout).unwrap().success());
}

#[test]
fn focus_loss_pauses_mouse_tracking_until_focus_returns() {
    // Some terminals keep delivering mode-1003 motion while unfocused (for
    // instance behind another window). Focus loss has to disarm 1003 and ignore
    // any pointer reports still in flight, then resume once focus comes back.
    let (server, mut term) = start(Resize::Accept, (1024, 768));
    let timeout = Duration::from_secs(10);
    assert_reports_size(&term, EXPECTED_SIZE, timeout);
    assert_pixel_exact(&term, timeout);

    // Hold a key and a pointer button before switching away.
    term.send(b"\x1b[120u\x1b[<0;101;101M");
    assert!(
        server
            .wait_for(timeout, |r| matches!(
                r,
                Request::Pointer {
                    x: 100,
                    y: 100,
                    buttons: 1
                }
            ))
            .is_some(),
        "initial press never reached the server"
    );

    let before_lost = term.output().len();
    term.send(b"\x1b[O");
    assert!(
        term.wait_for_after(before_lost, b"\x1b[?1003l", timeout)
            .is_some(),
        "focus loss did not disable mouse motion reporting: {}",
        tail(&term.output()[before_lost..])
    );
    wait_for_key(&server, 'x', false);
    assert!(
        server
            .wait_for(timeout, |r| matches!(
                r,
                Request::Pointer {
                    x: 100,
                    y: 100,
                    buttons: 0
                }
            ))
            .is_some(),
        "focus loss did not release the held pointer button"
    );

    // A late repeat, a key release and unfocused mouse reports must neither re-arm
    // tracking nor reach the remote desktop.
    let after_lost = term.output().len();
    term.send(b"\x1b[120;1:2u\x1b[120;1:3u\x1b[<35;201;201M\x1b[<35;301;301M\x1b[<0;301;301M\x1b[<0;301;301m");
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !contains(&term.output()[after_lost..], b"\x1b[?1003h"),
        "a stray key release or unfocused mouse event re-armed tracking"
    );

    // Focus returns: mode 1003 is re-armed and pointer reports resume.
    term.send(b"\x1b[I");
    assert!(
        term.wait_for_after(after_lost, b"\x1b[?1003h", timeout)
            .is_some(),
        "focus gain did not re-enable mouse motion reporting"
    );
    term.send(b"\x1b[<35;401;401M");
    assert!(
        server
            .wait_for(timeout, |r| matches!(
                r,
                Request::Pointer {
                    x: 400,
                    y: 400,
                    buttons: 0
                }
            ))
            .is_some(),
        "mouse motion did not resume after focus returned"
    );

    let unfocused_pointer = server.requests().into_iter().any(|r| {
        matches!(
            r,
            Request::Pointer { x: 200, y: 200, .. } | Request::Pointer { x: 300, y: 300, .. }
        )
    });
    assert!(
        !unfocused_pointer,
        "pointer events while unfocused reached the server: {:?}",
        server.requests()
    );

    // A terminal that drops FocusGained still recovers on a real key press.
    let before_second_loss = term.output().len();
    term.send(b"\x1b[O");
    assert!(
        term.wait_for_after(before_second_loss, b"\x1b[?1003l", timeout)
            .is_some(),
        "second focus loss did not disable mouse motion reporting"
    );
    let after_second_loss = term.output().len();
    term.send(b"\x1b[122u\x1b[122;1:3u");
    wait_for_key(&server, 'z', false);
    assert!(
        term.wait_for_after(after_second_loss, b"\x1b[?1003h", timeout)
            .is_some(),
        "a key press while unfocused did not re-arm mouse motion reporting"
    );
    term.send(b"\x1b[<35;501;501M");
    assert!(
        server
            .wait_for(timeout, |r| matches!(
                r,
                Request::Pointer {
                    x: 500,
                    y: 500,
                    buttons: 0
                }
            ))
            .is_some(),
        "mouse motion did not resume after keyboard recovery"
    );

    quit(&mut term);
    assert!(term.wait(timeout).unwrap().success());
}
