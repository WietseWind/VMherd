//! Keyboard events -> key messages for the consoles (tracks held keys so every press gets a release).

use egui::{Event, Key};

use crate::keys;

/// What an input event turns into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Routed {
    Key {
        keysym: u32,
        qnum: u32,
        down: bool,
    },
    /// Clipboard text to type (held keys have been released first).
    Paste(String),
}

/// A clipboard command from egui-winit. It swallows the key-down, and the key it came from
/// (C / X, or Insert / Delete, or a dedicated Copy/Cut key) only shows up in the release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Clip {
    Copy,
    Cut,
}

#[derive(Default)]
pub struct KeyRouter {
    /// (physical key, qnum, keysym) currently down on the target
    held: Vec<(Key, u32, u32)>,
    pending: Option<Clip>,
}

fn is_modifier(k: Key) -> bool {
    use Key::*;
    matches!(k, ShiftLeft | ShiftRight | ControlLeft | ControlRight | AltLeft | AltRight | SuperLeft | SuperRight)
}

/// egui reports numpad keys at the position of the top-row digit; with NumLock off the logical key
/// says what the key does (arrows, Home, ...), so use that one.
fn physical(key: Key, physical_key: Option<Key>) -> Key {
    use Key::*;
    let phys = physical_key.unwrap_or(key);
    let nav =
        matches!(key, ArrowUp | ArrowDown | ArrowLeft | ArrowRight | Home | End | PageUp | PageDown | Insert | Delete);
    let numpad_pos = matches!(phys, Num0 | Num1 | Num2 | Num3 | Num4 | Num5 | Num6 | Num7 | Num8 | Num9 | Period);
    if nav && numpad_pos { key } else { phys }
}

impl KeyRouter {
    /// Translate one event. `mac`: ⌘ shortcuts stay with the app and are not forwarded.
    pub fn route(&mut self, event: &Event, mac: bool, out: &mut Vec<Routed>) {
        match event {
            Event::Key { key, physical_key, pressed: true, modifiers, .. } => {
                let phys = physical(*key, *physical_key);
                if mac && (modifiers.mac_cmd || matches!(phys, Key::SuperLeft | Key::SuperRight)) {
                    return; // presses only: releases below must still reach the guest
                }
                let (_, qnum, keysym) = match self.held.iter().find(|h| h.0 == phys) {
                    Some(h) => *h, // auto-repeat: same keysym as the first press
                    None => {
                        let h = (phys, keys::qnum(phys), keys::keysym(*key, modifiers.shift));
                        if h.1 == 0 && h.2 == 0 {
                            return;
                        }
                        self.held.push(h);
                        h
                    }
                };
                out.push(Routed::Key { keysym, qnum, down: true });
            }
            Event::Key { key, physical_key, pressed: false, .. } => {
                let phys = physical(*key, *physical_key);
                if let Some(clip) = self.pending.take() {
                    resolve_clip(clip, *key, phys, out);
                }
                if let Some(i) = self.held.iter().position(|h| h.0 == phys) {
                    let (_, qnum, keysym) = self.held.remove(i);
                    out.push(Routed::Key { keysym, qnum, down: false });
                }
            }
            // Windows / Linux: Ctrl+C, Ctrl+X, Ctrl+Insert, Shift+Delete and the Copy/Cut keys arrive as
            // clipboard commands; which key it was is known when it is released.
            Event::Copy if !mac => self.pending = Some(Clip::Copy),
            Event::Cut if !mac => self.pending = Some(Clip::Cut),
            Event::Paste(text) => {
                self.release_all(out);
                out.push(Routed::Paste(text.clone()));
            }
            Event::WindowFocused(false) => self.release_all(out),
            _ => {}
        }
    }

    /// Release everything that is held (focus moved, window lost focus, typing starts).
    pub fn release_all(&mut self, out: &mut Vec<Routed>) {
        self.pending = None;
        for (_, qnum, keysym) in self.held.drain(..) {
            out.push(Routed::Key { keysym, qnum, down: false });
        }
    }
}

/// Send the key behind a clipboard command as a press + release, before `released` goes up.
fn resolve_clip(clip: Clip, logical: Key, released: Key, out: &mut Vec<Routed>) {
    let mut tap = |qnum: u32, keysym: u32| {
        out.push(Routed::Key { keysym, qnum, down: true });
        out.push(Routed::Key { keysym, qnum, down: false });
    };
    let command_key = |k: Key| matches!(k, Key::C | Key::X | Key::Insert | Key::Delete);
    if command_key(logical) || command_key(released) {
        tap(keys::qnum(released), keys::keysym(logical, false));
    } else if is_modifier(released) {
        // the modifier came up first while the command key is still down: assume the common C / X
        let (qnum, c) = if clip == Clip::Copy { (0x2e, 'c') } else { (0x2d, 'x') };
        tap(qnum, c as u32);
    }
    // a dedicated Copy/Cut key has no console equivalent: nothing to send
}

/// A plain Esc on a console target: a tap is sent to the consoles when released; holding it for
/// `hold` means "leave" (the caller drops keyboard focus) and nothing is sent.
pub struct EscHold {
    hold: std::time::Duration,
    since: Option<std::time::Instant>,
    swallow: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EscTick {
    Idle,
    /// held, 0..1 of the way to leaving
    Holding(f32),
    /// held long enough: leave the console target
    Leave,
}

impl EscHold {
    pub fn new(hold: std::time::Duration) -> Self {
        Self { hold, since: None, swallow: false }
    }

    pub fn reset(&mut self) {
        self.since = None;
        self.swallow = false;
    }

    /// Handle a plain Esc event (true) or leave the event to the router (false).
    pub fn on_event(&mut self, event: &Event, now: std::time::Instant, out: &mut Vec<Routed>) -> bool {
        let Event::Key { key: Key::Escape, pressed, repeat, modifiers, .. } = event else { return false };
        if !modifiers.is_none() {
            return false;
        }
        match (pressed, repeat) {
            (true, false) => self.since = Some(now),
            (true, true) => {}
            (false, _) => {
                if self.since.take().is_some() && !self.swallow {
                    out.push(Routed::Key { keysym: rfb::keysym::ESCAPE, qnum: 0x01, down: true });
                    out.push(Routed::Key { keysym: rfb::keysym::ESCAPE, qnum: 0x01, down: false });
                }
                self.swallow = false;
            }
        }
        true
    }

    pub fn tick(&mut self, now: std::time::Instant) -> EscTick {
        let Some(since) = self.since else { return EscTick::Idle };
        let held = now.duration_since(since);
        if held >= self.hold {
            self.since = None;
            self.swallow = true;
            EscTick::Leave
        } else {
            EscTick::Holding(held.as_secs_f32() / self.hold.as_secs_f32())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Modifiers;
    use rfb::keysym as ks;

    fn key(key: Key, pressed: bool, modifiers: Modifiers) -> Event {
        Event::Key { key, physical_key: Some(key), pressed, repeat: false, modifiers }
    }

    fn run(events: &[Event], mac: bool) -> Vec<Routed> {
        let mut r = KeyRouter::default();
        let mut out = Vec::new();
        for e in events {
            r.route(e, mac, &mut out);
        }
        out
    }

    const fn k(keysym: u32, qnum: u32, down: bool) -> Routed {
        Routed::Key { keysym, qnum, down }
    }

    const CTRL: Modifiers = Modifiers { ctrl: true, command: true, ..Modifiers::NONE };
    const CMD: Modifiers = Modifiers { mac_cmd: true, command: true, ..Modifiers::NONE };

    #[test]
    fn shift_a() {
        let s = Modifiers::SHIFT;
        let out = run(
            &[
                key(Key::ShiftLeft, true, s),
                key(Key::A, true, s),
                key(Key::A, false, s),
                key(Key::ShiftLeft, false, Modifiers::NONE),
            ],
            false,
        );
        assert_eq!(
            out,
            vec![
                k(ks::SHIFT_L, 0x2a, true),
                k('A' as u32, 0x1e, true),
                k('A' as u32, 0x1e, false),
                k(ks::SHIFT_L, 0x2a, false)
            ]
        );
    }

    #[test]
    fn layout_independent_scancode() {
        // AZERTY: the physical Q key produces 'a'
        let e = Event::Key {
            key: Key::A,
            physical_key: Some(Key::Q),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        assert_eq!(run(&[e], false), vec![k('a' as u32, 0x10, true)]);
    }

    #[test]
    fn release_uses_press_keysym_and_ignores_strays() {
        let out = run(
            &[
                key(Key::A, true, Modifiers::SHIFT),
                key(Key::A, false, Modifiers::NONE),
                key(Key::B, false, Modifiers::NONE),
            ],
            false,
        );
        assert_eq!(out, vec![k('A' as u32, 0x1e, true), k('A' as u32, 0x1e, false)]);
    }

    #[test]
    fn autorepeat() {
        let out = run(
            &[
                key(Key::X, true, Modifiers::NONE),
                key(Key::X, true, Modifiers::NONE),
                key(Key::X, false, Modifiers::NONE),
            ],
            false,
        );
        assert_eq!(out, vec![k('x' as u32, 0x2d, true), k('x' as u32, 0x2d, true), k('x' as u32, 0x2d, false)]);
    }

    #[test]
    fn cmd_stays_local_on_mac() {
        let out = run(
            &[
                key(Key::SuperLeft, true, CMD),
                key(Key::W, true, CMD),
                key(Key::W, false, CMD),
                key(Key::SuperLeft, false, Modifiers::NONE),
            ],
            true,
        );
        assert!(out.is_empty(), "{out:?}");
        // but the Windows key is forwarded elsewhere
        assert_eq!(run(&[key(Key::SuperLeft, true, Modifiers::NONE)], false), vec![k(ks::SUPER_L, 0xdb, true)]);
    }

    #[test]
    fn modifier_released_while_cmd_is_down_still_goes_up() {
        let shift_cmd = Modifiers { shift: true, ..CMD };
        let out = run(
            &[
                key(Key::ShiftLeft, true, Modifiers::SHIFT),
                key(Key::SuperLeft, true, shift_cmd),
                key(Key::Num4, true, shift_cmd),
                key(Key::Num4, false, shift_cmd),
                key(Key::ShiftLeft, false, CMD),
                key(Key::SuperLeft, false, Modifiers::NONE),
            ],
            true,
        );
        assert_eq!(out, vec![k(ks::SHIFT_L, 0x2a, true), k(ks::SHIFT_L, 0x2a, false)]);
    }

    #[test]
    fn ctrl_c_on_windows_linux() {
        let out = run(
            &[
                key(Key::ControlLeft, true, CTRL),
                Event::Copy,
                key(Key::C, false, CTRL),
                key(Key::ControlLeft, false, Modifiers::NONE),
            ],
            false,
        );
        assert_eq!(
            out,
            vec![
                k(ks::CONTROL_L, 0x1d, true),
                k('c' as u32, 0x2e, true),
                k('c' as u32, 0x2e, false),
                k(ks::CONTROL_L, 0x1d, false)
            ]
        );
        // on macOS Cmd+C is a local copy
        assert!(run(&[Event::Copy], true).is_empty());
    }

    #[test]
    fn ctrl_insert_is_insert_not_ctrl_c() {
        let out = run(
            &[
                key(Key::ControlLeft, true, CTRL),
                Event::Copy,
                key(Key::Insert, false, CTRL),
                key(Key::ControlLeft, false, Modifiers::NONE),
            ],
            false,
        );
        assert_eq!(
            out,
            vec![
                k(ks::CONTROL_L, 0x1d, true),
                k(ks::INSERT, 0xd2, true),
                k(ks::INSERT, 0xd2, false),
                k(ks::CONTROL_L, 0x1d, false)
            ]
        );
    }

    #[test]
    fn shift_delete_is_delete() {
        let s = Modifiers::SHIFT;
        let out = run(
            &[
                key(Key::ShiftLeft, true, s),
                Event::Cut,
                key(Key::Delete, false, s),
                key(Key::ShiftLeft, false, Modifiers::NONE),
            ],
            false,
        );
        assert_eq!(
            out,
            vec![
                k(ks::SHIFT_L, 0x2a, true),
                k(ks::DELETE, 0xd3, true),
                k(ks::DELETE, 0xd3, false),
                k(ks::SHIFT_L, 0x2a, false)
            ]
        );
    }

    #[test]
    fn ctrl_released_before_c() {
        let out = run(
            &[
                key(Key::ControlLeft, true, CTRL),
                Event::Copy,
                key(Key::ControlLeft, false, Modifiers::NONE),
                key(Key::C, false, Modifiers::NONE),
            ],
            false,
        );
        assert_eq!(
            out,
            vec![
                k(ks::CONTROL_L, 0x1d, true),
                k('c' as u32, 0x2e, true),
                k('c' as u32, 0x2e, false),
                k(ks::CONTROL_L, 0x1d, false)
            ]
        );
    }

    #[test]
    fn dvorak_ctrl_c_uses_the_physical_key() {
        let release =
            Event::Key { key: Key::C, physical_key: Some(Key::I), pressed: false, repeat: false, modifiers: CTRL };
        let out = run(&[key(Key::ControlLeft, true, CTRL), Event::Copy, release], false);
        assert_eq!(out, vec![k(ks::CONTROL_L, 0x1d, true), k('c' as u32, 0x17, true), k('c' as u32, 0x17, false)]);
    }

    #[test]
    fn numpad_navigation_with_numlock_off() {
        let up = Event::Key {
            key: Key::ArrowUp,
            physical_key: Some(Key::Num8),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        assert_eq!(run(&[up], false), vec![k(ks::UP, 0xc8, true)]);
        assert_eq!(run(&[key(Key::Plus, true, Modifiers::NONE)], false), vec![k('+' as u32, 0x4e, true)]);
    }

    #[test]
    fn focus_loss_releases_keys() {
        let out = run(&[key(Key::ShiftLeft, true, Modifiers::SHIFT), Event::WindowFocused(false)], false);
        assert_eq!(out, vec![k(ks::SHIFT_L, 0x2a, true), k(ks::SHIFT_L, 0x2a, false)]);
    }

    #[test]
    fn esc_tap_long_press_and_repeat() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let esc = |pressed, repeat, modifiers| Event::Key {
            key: Key::Escape,
            physical_key: Some(Key::Escape),
            pressed,
            repeat,
            modifiers,
        };
        let mut e = EscHold::new(Duration::from_secs(1));
        let mut out = Vec::new();
        // a tap is sent on release
        assert!(e.on_event(&esc(true, false, Modifiers::NONE), t0, &mut out));
        assert_eq!(e.tick(t0 + Duration::from_millis(300)), EscTick::Holding(0.3));
        assert!(e.on_event(&esc(false, false, Modifiers::NONE), t0, &mut out));
        assert_eq!(out, vec![k(ks::ESCAPE, 0x01, true), k(ks::ESCAPE, 0x01, false)]);
        // held: repeats ignored, leaves after 1 s, the release is swallowed
        out.clear();
        e.on_event(&esc(true, false, Modifiers::NONE), t0, &mut out);
        e.on_event(&esc(true, true, Modifiers::NONE), t0 + Duration::from_millis(500), &mut out);
        assert_eq!(e.tick(t0 + Duration::from_millis(1000)), EscTick::Leave);
        assert_eq!(e.tick(t0 + Duration::from_millis(1100)), EscTick::Idle);
        e.on_event(&esc(false, false, Modifiers::NONE), t0, &mut out);
        assert!(out.is_empty(), "{out:?}");
        // the next tap works again
        e.on_event(&esc(true, false, Modifiers::NONE), t0, &mut out);
        e.on_event(&esc(false, false, Modifiers::NONE), t0, &mut out);
        assert_eq!(out.len(), 2);
        // Shift+Esc is an ordinary key for the router
        assert!(!e.on_event(&esc(true, false, Modifiers::SHIFT), t0, &mut out));
    }

    #[test]
    fn paste_releases_held_keys_first() {
        let out = run(&[key(Key::ControlLeft, true, Modifiers::CTRL), Event::Paste("ls".into())], false);
        assert_eq!(out, vec![k(ks::CONTROL_L, 0x1d, true), k(ks::CONTROL_L, 0x1d, false), Routed::Paste("ls".into())]);
    }
}
