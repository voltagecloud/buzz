//! What a key press sends to the shell, as xterm.js encodes it for
//! buzz-app's terminal: xterm sequences, Control letters as C0 bytes,
//! and Alt as an Escape prefix (Option composes characters on macOS).

use gpui_kit::Modifiers;

/// The terminal modes that change what keys send.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyModes {
    /// DECCKM: arrows, Home and End send `ESC O x`.
    pub app_cursor: bool,
}

/// The bytes for a key press, or `None` when the terminal leaves the key
/// to the app (Command chords, modifier keys alone).
pub fn encode(
    key: &str,
    key_char: Option<&str>,
    modifiers: Modifiers,
    modes: KeyModes,
    apple: bool,
) -> Option<Vec<u8>> {
    if modifiers.platform {
        return None;
    }
    let Modifiers {
        control,
        alt,
        shift,
        ..
    } = modifiers;
    // xterm's modifier parameter: 1 + Shift + 2·Alt + 4·Control.
    let parameter = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(control);
    let cursor = |last: char| -> Vec<u8> {
        if parameter > 1 {
            format!("\x1b[1;{parameter}{last}")
        } else if modes.app_cursor {
            format!("\x1bO{last}")
        } else {
            format!("\x1b[{last}")
        }
        .into_bytes()
    };
    let tilde = |number: u8| -> Vec<u8> {
        if parameter > 1 {
            format!("\x1b[{number};{parameter}~")
        } else {
            format!("\x1b[{number}~")
        }
        .into_bytes()
    };
    let function = |last: char| -> Vec<u8> {
        if parameter > 1 {
            format!("\x1b[1;{parameter}{last}")
        } else {
            format!("\x1bO{last}")
        }
        .into_bytes()
    };
    let meta = |bytes: &[u8]| -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        if alt {
            out.push(0x1b);
        }
        out.extend_from_slice(bytes);
        out
    };
    let named = match key {
        "up" => cursor('A'),
        "down" => cursor('B'),
        "right" => cursor('C'),
        "left" => cursor('D'),
        "home" => cursor('H'),
        "end" => cursor('F'),
        "insert" => tilde(2),
        "delete" => tilde(3),
        "pageup" => tilde(5),
        "pagedown" => tilde(6),
        "f1" => function('P'),
        "f2" => function('Q'),
        "f3" => function('R'),
        "f4" => function('S'),
        "f5" => tilde(15),
        "f6" => tilde(17),
        "f7" => tilde(18),
        "f8" => tilde(19),
        "f9" => tilde(20),
        "f10" => tilde(21),
        "f11" => tilde(23),
        "f12" => tilde(24),
        "enter" => meta(b"\r"),
        "escape" => meta(b"\x1b"),
        "tab" if shift => b"\x1b[Z".to_vec(),
        "tab" => meta(b"\t"),
        "backspace" if control => meta(b"\x08"),
        "backspace" => meta(b"\x7f"),
        "space" if control => meta(b"\0"),
        "shift" | "control" | "alt" | "platform" | "function" | "capslock" | "numlock" => {
            return None;
        }
        _ => {
            return character(key, key_char, control, alt && !apple);
        }
    };
    Some(named)
}

/// A printable key: its character, a C0 byte under Control, and an
/// Escape prefix under Alt.
fn character(key: &str, key_char: Option<&str>, control: bool, alt: bool) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    if alt {
        out.push(0x1b);
    }
    if control {
        let mut chars = key.chars();
        let (Some(first), None) = (chars.next(), chars.next()) else {
            return None;
        };
        let byte = match first.to_ascii_lowercase() {
            letter @ 'a'..='z' => letter as u8 - b'a' + 1,
            '2' | '@' | ' ' => 0,
            '3' | '[' => 0x1b,
            '4' | '\\' => 0x1c,
            '5' | ']' => 0x1d,
            '6' | '^' => 0x1e,
            '7' | '-' | '_' | '/' => 0x1f,
            '8' | '?' => 0x7f,
            _ => return None,
        };
        out.push(byte);
        return Some(out);
    }
    let text = match key_char {
        Some(text) if !text.is_empty() => text,
        _ if key == "space" => " ",
        // GPUI reports the key's own name for unshifted characters.
        _ if key.chars().count() == 1 => key,
        _ => return None,
    };
    out.extend_from_slice(text.as_bytes());
    Some(out)
}

/// Pasted text: newlines as Enter, wrapped in bracketed-paste markers when
/// the shell asked for them. Inside the bracket every ESC is dropped, so
/// no marker survives and a paste cannot end the bracket early: removing
/// only the markers would let `ESC[2ESC[201~01~` rebuild one.
pub fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
    if !bracketed {
        return normalized.into_bytes();
    }
    let inner = normalized.replace('\x1b', "");
    format!("\x1b[200~{inner}\x1b[201~").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn press(key: &str, key_char: Option<&str>, modifiers: Modifiers) -> Option<Vec<u8>> {
        encode(key, key_char, modifiers, KeyModes::default(), false)
    }

    fn with(control: bool, alt: bool, shift: bool) -> Modifiers {
        Modifiers {
            control,
            alt,
            shift,
            ..Default::default()
        }
    }

    #[test]
    fn plain_and_shifted_characters_send_themselves() {
        let none = Modifiers::default();
        assert_eq!(press("a", Some("a"), none), Some(b"a".to_vec()));
        assert_eq!(
            press("a", Some("A"), with(false, false, true)),
            Some(b"A".to_vec())
        );
        assert_eq!(press("space", Some(" "), none), Some(b" ".to_vec()));
        assert_eq!(press("e", Some("é"), none), Some("é".as_bytes().to_vec()));
    }

    #[test]
    fn escape_enter_tab_and_backspace() {
        let none = Modifiers::default();
        assert_eq!(press("escape", None, none), Some(b"\x1b".to_vec()));
        assert_eq!(press("enter", None, none), Some(b"\r".to_vec()));
        assert_eq!(press("tab", None, none), Some(b"\t".to_vec()));
        assert_eq!(
            press("tab", None, with(false, false, true)),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(press("backspace", None, none), Some(b"\x7f".to_vec()));
        assert_eq!(
            press("backspace", None, with(true, false, false)),
            Some(b"\x08".to_vec())
        );
    }

    #[test]
    fn control_sends_c0_bytes_and_alt_an_escape_prefix() {
        let control = with(true, false, false);
        assert_eq!(press("c", None, control), Some(vec![0x03]));
        assert_eq!(press("d", None, control), Some(vec![0x04]));
        assert_eq!(press("[", None, control), Some(vec![0x1b]));
        assert_eq!(press("space", None, control), Some(vec![0]));
        assert_eq!(
            press("b", Some("b"), with(false, true, false)),
            Some(b"\x1bb".to_vec())
        );
        assert_eq!(
            press("x", None, with(true, true, false)),
            Some(vec![0x1b, 0x18])
        );
        // Option composes characters on macOS instead.
        assert_eq!(
            encode(
                "e",
                Some("´"),
                with(false, true, false),
                KeyModes::default(),
                true
            ),
            Some("´".as_bytes().to_vec())
        );
    }

    #[test]
    fn arrows_follow_the_cursor_mode_and_modifiers() {
        let none = Modifiers::default();
        assert_eq!(press("up", None, none), Some(b"\x1b[A".to_vec()));
        let app = KeyModes { app_cursor: true };
        assert_eq!(
            encode("up", None, none, app, false),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            press("left", None, with(true, false, false)),
            Some(b"\x1b[1;5D".to_vec())
        );
        assert_eq!(press("pageup", None, none), Some(b"\x1b[5~".to_vec()));
        assert_eq!(press("delete", None, none), Some(b"\x1b[3~".to_vec()));
        assert_eq!(press("f1", None, none), Some(b"\x1bOP".to_vec()));
        assert_eq!(press("f5", None, none), Some(b"\x1b[15~".to_vec()));
    }

    #[test]
    fn the_app_keeps_command_chords_and_lone_modifiers() {
        let command = Modifiers {
            platform: true,
            ..Default::default()
        };
        assert_eq!(press("c", None, command), None);
        assert_eq!(press("shift", None, with(false, false, true)), None);
    }

    #[test]
    fn pastes_are_bracketed_when_asked() {
        assert_eq!(paste("a\nb", false), b"a\rb".to_vec());
        assert_eq!(
            paste("x\x1b[201~y", true),
            b"\x1b[200~x[201~y\x1b[201~".to_vec()
        );
        // Removing the marker in one pass would rebuild it from the halves.
        assert_eq!(
            paste("\x1b[2\x1b[201~01~", true),
            b"\x1b[200~[2[201~01~\x1b[201~".to_vec()
        );
    }
}
