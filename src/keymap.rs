//! Key and button codes.
//!
//! These are the kernel's `input-event-codes.h` numbers, which EIS speaks
//! directly: `ei_keyboard.key` and `ei_button.button` both take linux key
//! codes. US layout only — EIS events go into the compositor's input pipeline,
//! so the layout that matters is the one the compositor will apply, and there
//! is no way to ask it. Callers that need other layouts should use the
//! clipboard.

use anyhow::{Context as _, Result, bail};

const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;

pub const KEY_LEFTSHIFT: u16 = 42;

#[derive(Clone, Copy, Debug)]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            "middle" => Ok(Self::Middle),
            other => bail!("unknown button {other:?} (expected left, right or middle)"),
        }
    }

    pub fn code(self) -> u16 {
        match self {
            Self::Left => BTN_LEFT,
            Self::Right => BTN_RIGHT,
            Self::Middle => BTN_MIDDLE,
        }
    }
}

/// US layout: the compositor's own layout is irrelevant here because the events
/// go in below it.
pub fn keycode_for(ch: char) -> Option<(u16, bool)> {
    let (code, shift) = match ch {
        'a'..='z' => (letter(ch)?, false),
        'A'..='Z' => (letter(ch.to_ascii_lowercase())?, true),
        '1' => (2, false),
        '2' => (3, false),
        '3' => (4, false),
        '4' => (5, false),
        '5' => (6, false),
        '6' => (7, false),
        '7' => (8, false),
        '8' => (9, false),
        '9' => (10, false),
        '0' => (11, false),
        '!' => (2, true),
        '@' => (3, true),
        '#' => (4, true),
        '$' => (5, true),
        '%' => (6, true),
        '^' => (7, true),
        '&' => (8, true),
        '*' => (9, true),
        '(' => (10, true),
        ')' => (11, true),
        ' ' => (57, false),
        '\n' => (28, false),
        '\t' => (15, false),
        '-' => (12, false),
        '_' => (12, true),
        '=' => (13, false),
        '+' => (13, true),
        '[' => (26, false),
        '{' => (26, true),
        ']' => (27, false),
        '}' => (27, true),
        '\\' => (43, false),
        '|' => (43, true),
        ';' => (39, false),
        ':' => (39, true),
        '\'' => (40, false),
        '"' => (40, true),
        ',' => (51, false),
        '<' => (51, true),
        '.' => (52, false),
        '>' => (52, true),
        '/' => (53, false),
        '?' => (53, true),
        '`' => (41, false),
        '~' => (41, true),
        _ => return None,
    };
    Some((code, shift))
}

fn letter(lower: char) -> Option<u16> {
    let code = match lower {
        'q' => 16,
        'w' => 17,
        'e' => 18,
        'r' => 19,
        't' => 20,
        'y' => 21,
        'u' => 22,
        'i' => 23,
        'o' => 24,
        'p' => 25,
        'a' => 30,
        's' => 31,
        'd' => 32,
        'f' => 33,
        'g' => 34,
        'h' => 35,
        'j' => 36,
        'k' => 37,
        'l' => 38,
        'z' => 44,
        'x' => 45,
        'c' => 46,
        'v' => 47,
        'b' => 48,
        'n' => 49,
        'm' => 50,
        _ => return None,
    };
    Some(code)
}

/// Named keys accepted by the `key` tool, as `ctrl+s` style chords.
pub fn parse_chord(chord: &str) -> Result<Vec<u16>> {
    let mut codes = Vec::new();
    for part in chord.split('+') {
        let part = part.trim().to_ascii_lowercase();
        let code = match part.as_str() {
            "ctrl" | "control" => 29,
            "shift" => KEY_LEFTSHIFT,
            "alt" => 56,
            "super" | "meta" => 125,
            "enter" | "return" => 28,
            "tab" => 15,
            "esc" | "escape" => 1,
            "backspace" => 14,
            "delete" => 111,
            "space" => 57,
            "up" => 103,
            "down" => 108,
            "left" => 105,
            "right" => 106,
            "home" => 102,
            "end" => 107,
            "pageup" => 104,
            "pagedown" => 109,
            single if single.chars().count() == 1 => {
                keycode_for(single.chars().next().unwrap())
                    .with_context(|| format!("unsupported key {part:?}"))?
                    .0
            }
            other => bail!("unknown key {other:?}"),
        };
        codes.push(code);
    }
    if codes.is_empty() {
        bail!("empty key chord");
    }
    Ok(codes)
}
