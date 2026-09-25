//! X11 keysyms used by consoles (from X11/keysymdef.h).

pub const BACKSPACE: u32 = 0xff08;
pub const TAB: u32 = 0xff09;
pub const RETURN: u32 = 0xff0d;
pub const PAUSE: u32 = 0xff13;
pub const SCROLL_LOCK: u32 = 0xff14;
pub const SYS_REQ: u32 = 0xff15;
pub const ESCAPE: u32 = 0xff1b;
pub const DELETE: u32 = 0xffff;

pub const HOME: u32 = 0xff50;
pub const LEFT: u32 = 0xff51;
pub const UP: u32 = 0xff52;
pub const RIGHT: u32 = 0xff53;
pub const DOWN: u32 = 0xff54;
pub const PAGE_UP: u32 = 0xff55;
pub const PAGE_DOWN: u32 = 0xff56;
pub const END: u32 = 0xff57;
pub const PRINT: u32 = 0xff61;
pub const INSERT: u32 = 0xff63;
pub const MENU: u32 = 0xff67;
pub const NUM_LOCK: u32 = 0xff7f;
pub const KP_ENTER: u32 = 0xff8d;

/// F1; F2..F35 follow consecutively.
pub const F1: u32 = 0xffbe;

pub const SHIFT_L: u32 = 0xffe1;
pub const SHIFT_R: u32 = 0xffe2;
pub const CONTROL_L: u32 = 0xffe3;
pub const CONTROL_R: u32 = 0xffe4;
pub const CAPS_LOCK: u32 = 0xffe5;
pub const META_L: u32 = 0xffe7;
pub const META_R: u32 = 0xffe8;
pub const ALT_L: u32 = 0xffe9;
pub const ALT_R: u32 = 0xffea;
pub const SUPER_L: u32 = 0xffeb;
pub const SUPER_R: u32 = 0xffec;
pub const ISO_LEVEL3_SHIFT: u32 = 0xfe03;

/// Keysym of a character: Latin-1 maps one to one, everything else to the Unicode keysym range.
pub fn from_char(c: char) -> u32 {
    match c {
        '\n' | '\r' => RETURN,
        '\t' => TAB,
        ' '..='~' | '\u{a0}'..='\u{ff}' => c as u32,
        _ => 0x0100_0000 | c as u32,
    }
}

/// Function key `n` (1-based).
pub const fn function(n: u32) -> u32 {
    F1 + n - 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chars() {
        assert_eq!(from_char('a'), 0x61);
        assert_eq!(from_char('~'), 0x7e);
        assert_eq!(from_char('é'), 0xe9);
        assert_eq!(from_char('€'), 0x0100_20ac);
        assert_eq!(from_char('\n'), RETURN);
        assert_eq!(function(12), 0xffc9);
    }
}
