//! Keyboard translation: egui keys -> QEMU key numbers (XT scancodes) + X11 keysyms, and text ->
//! key strokes on a US keyboard (for "type text" / paste).

use egui::Key;
use rfb::keysym as ks;

/// QEMU key number of a physical key (XT set 1; extended keys as `0x80 | low byte`), 0 if unknown.
pub fn qnum(key: Key) -> u32 {
    use Key::*;
    match key {
        Escape => 0x01,
        Num1 => 0x02,
        Num2 => 0x03,
        Num3 => 0x04,
        Num4 => 0x05,
        Num5 => 0x06,
        Num6 => 0x07,
        Num7 => 0x08,
        Num8 => 0x09,
        Num9 => 0x0a,
        Num0 => 0x0b,
        Minus => 0x0c,
        Equals => 0x0d,
        Backspace => 0x0e,
        Tab => 0x0f,
        Q => 0x10,
        W => 0x11,
        E => 0x12,
        R => 0x13,
        T => 0x14,
        Y => 0x15,
        U => 0x16,
        I => 0x17,
        O => 0x18,
        P => 0x19,
        OpenBracket => 0x1a,
        CloseBracket => 0x1b,
        Enter => 0x1c,
        ControlLeft => 0x1d,
        A => 0x1e,
        S => 0x1f,
        D => 0x20,
        F => 0x21,
        G => 0x22,
        H => 0x23,
        J => 0x24,
        K => 0x25,
        L => 0x26,
        Semicolon => 0x27,
        Quote => 0x28,
        Backtick => 0x29,
        ShiftLeft => 0x2a,
        Backslash => 0x2b,
        Z => 0x2c,
        X => 0x2d,
        C => 0x2e,
        V => 0x2f,
        B => 0x30,
        N => 0x31,
        M => 0x32,
        Comma => 0x33,
        Period => 0x34,
        Slash => 0x35,
        ShiftRight => 0x36,
        AltLeft => 0x38,
        Space => 0x39,
        F1 => 0x3b,
        F2 => 0x3c,
        F3 => 0x3d,
        F4 => 0x3e,
        F5 => 0x3f,
        F6 => 0x40,
        F7 => 0x41,
        F8 => 0x42,
        F9 => 0x43,
        F10 => 0x44,
        IntlBackslash => 0x56,
        F11 => 0x57,
        F12 => 0x58,
        F13 => 0x5d,
        F14 => 0x5e,
        F15 => 0x5f,
        F16 => 0x55,
        F17 => 0x83,
        F18 => 0xf7,
        F19 => 0x84,
        F20 => 0x5a,
        F21 => 0x74,
        F22 => 0xf9,
        F23 => 0x6d,
        F24 => 0x6f,
        ControlRight => 0x9d,
        AltRight => 0xb8,
        Home => 0xc7,
        ArrowUp => 0xc8,
        PageUp => 0xc9,
        ArrowLeft => 0xcb,
        ArrowRight => 0xcd,
        End => 0xcf,
        ArrowDown => 0xd0,
        PageDown => 0xd1,
        Insert => 0xd2,
        Delete => 0xd3,
        SuperLeft => 0xdb,
        SuperRight => 0xdc,
        // only the numpad has a physical "+" key
        Plus => 0x4e,
        _ => 0,
    }
}

/// Keysym of the logical key (what the user's layout produced). 0 if none.
pub fn keysym(key: Key, shift: bool) -> u32 {
    use Key::*;
    let named = match key {
        Escape => ks::ESCAPE,
        Tab => ks::TAB,
        Backspace => ks::BACKSPACE,
        Enter => ks::RETURN,
        Space => 0x20,
        Insert => ks::INSERT,
        Delete => ks::DELETE,
        Home => ks::HOME,
        End => ks::END,
        PageUp => ks::PAGE_UP,
        PageDown => ks::PAGE_DOWN,
        ArrowUp => ks::UP,
        ArrowDown => ks::DOWN,
        ArrowLeft => ks::LEFT,
        ArrowRight => ks::RIGHT,
        ShiftLeft => ks::SHIFT_L,
        ShiftRight => ks::SHIFT_R,
        ControlLeft => ks::CONTROL_L,
        ControlRight => ks::CONTROL_R,
        AltLeft => ks::ALT_L,
        AltRight => ks::ALT_R,
        SuperLeft => ks::SUPER_L,
        SuperRight => ks::SUPER_R,
        _ => 0,
    };
    if named != 0 {
        return named;
    }
    if let Some(n) = function_number(key) {
        return ks::function(n);
    }
    match key_char(key) {
        Some(c) if c.is_ascii_alphabetic() => {
            let c = if shift { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() };
            ks::from_char(c)
        }
        Some(c) => ks::from_char(c),
        None => 0,
    }
}

fn function_number(key: Key) -> Option<u32> {
    use Key::*;
    let all = [
        F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12, F13, F14, F15, F16, F17, F18, F19, F20, F21, F22, F23, F24,
        F25, F26, F27, F28, F29, F30, F31, F32, F33, F34, F35,
    ];
    all.iter().position(|k| *k == key).map(|i| i as u32 + 1)
}

/// The character a (logical) key stands for, if it is a printable one.
fn key_char(key: Key) -> Option<char> {
    use Key::*;
    Some(match key {
        Colon => ':',
        Comma => ',',
        Backslash => '\\',
        Slash => '/',
        Pipe => '|',
        Questionmark => '?',
        Exclamationmark => '!',
        OpenBracket => '[',
        CloseBracket => ']',
        OpenCurlyBracket => '{',
        CloseCurlyBracket => '}',
        Backtick => '`',
        Minus => '-',
        Period => '.',
        Plus => '+',
        Equals => '=',
        Semicolon => ';',
        Quote => '\'',
        _ => {
            let name = key.name();
            let mut chars = name.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                // "Num1" etc.
                return name.strip_prefix("Num").and_then(|d| d.chars().next());
            }
            c
        }
    })
}

/// One character typed on a US keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stroke {
    pub qnum: u32,
    pub keysym: u32,
    pub shift: bool,
}

/// `c` as a key stroke on a US keyboard, or `None` if the layout has no such key.
pub fn us_stroke(c: char) -> Option<Stroke> {
    const ROWS: [(&str, &str, [u32; 13]); 4] = [
        (
            "`1234567890-=",
            "~!@#$%^&*()_+",
            [0x29, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d],
        ),
        (
            "qwertyuiop[]\\",
            "QWERTYUIOP{}|",
            [0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x2b],
        ),
        ("asdfghjkl;'", "ASDFGHJKL:\"", [0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0, 0]),
        ("zxcvbnm,./", "ZXCVBNM<>?", [0x2c, 0x2d, 0x2e, 0x2f, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0, 0, 0]),
    ];
    let keysym = ks::from_char(c);
    match c {
        ' ' => return Some(Stroke { qnum: 0x39, keysym, shift: false }),
        '\n' => return Some(Stroke { qnum: 0x1c, keysym: ks::RETURN, shift: false }),
        '\t' => return Some(Stroke { qnum: 0x0f, keysym: ks::TAB, shift: false }),
        _ => {}
    }
    for (plain, shifted, codes) in ROWS {
        if let Some(i) = plain.chars().position(|p| p == c) {
            return Some(Stroke { qnum: codes[i], keysym, shift: false });
        }
        if let Some(i) = shifted.chars().position(|p| p == c) {
            return Some(Stroke { qnum: codes[i], keysym, shift: true });
        }
    }
    None
}

/// Text as key strokes; returns the strokes and how many characters had no US key.
pub fn text_strokes(text: &str) -> (Vec<Stroke>, usize) {
    let mut out = Vec::with_capacity(text.len());
    let mut skipped = 0;
    for c in text.replace("\r\n", "\n").replace('\r', "\n").chars() {
        match us_stroke(c) {
            Some(s) => out.push(s),
            None => skipped += 1,
        }
    }
    (out, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scancodes() {
        assert_eq!(qnum(Key::A), 0x1e);
        assert_eq!(qnum(Key::ArrowUp), 0xc8);
        assert_eq!(qnum(Key::ControlLeft), 0x1d);
        assert_eq!(qnum(Key::F12), 0x58);
    }

    #[test]
    fn keysyms() {
        assert_eq!(keysym(Key::A, false), 'a' as u32);
        assert_eq!(keysym(Key::A, true), 'A' as u32);
        assert_eq!(keysym(Key::Num7, false), '7' as u32);
        assert_eq!(keysym(Key::Exclamationmark, true), '!' as u32);
        assert_eq!(keysym(Key::F5, false), ks::function(5));
        assert_eq!(keysym(Key::Enter, false), ks::RETURN);
    }

    #[test]
    fn us_text() {
        let (s, skipped) = text_strokes("Hi |~\"\n€");
        assert_eq!(skipped, 1);
        assert_eq!(s[0], Stroke { qnum: 0x23, keysym: 'H' as u32, shift: true });
        assert_eq!(s[1], Stroke { qnum: 0x17, keysym: 'i' as u32, shift: false });
        assert_eq!(s[2].qnum, 0x39);
        assert_eq!(s[3], Stroke { qnum: 0x2b, keysym: '|' as u32, shift: true });
        assert_eq!(s[4], Stroke { qnum: 0x29, keysym: '~' as u32, shift: true });
        assert_eq!(s[5], Stroke { qnum: 0x28, keysym: '"' as u32, shift: true });
        assert_eq!(s[6].qnum, 0x1c);
    }
}
