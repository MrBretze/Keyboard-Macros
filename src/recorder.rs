//! Platform-independent recording logic: turns raw input into macro events.
//!
//! The platform backends capture input and feed it here; this module filters it
//! (auto-repeat, keys pressed before the recording, mouse options), merges mouse
//! movements and computes the delays.

use crate::common::{BTN_LEFT, is_keyboard_code, is_mouse_button};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// Relative movements are merged over this window: mice report up to 1000 times per
/// second, which would make huge, unreadable macros.
const MOVE_MERGE_WINDOW: f64 = 0.012; // seconds

#[derive(Debug, Clone, Copy)]
pub struct RecordOptions {
    pub mouse: bool,
    pub mouse_move: bool,
}

/// Called for every recorded event (live display in the editor).
pub type OnEvent = Box<dyn Fn(&Value) + Send>;
/// Called once with the final list of events when recording stops.
pub type OnStop = Box<dyn FnOnce(Vec<Value>) + Send>;

/// Shared stop request: 0 = running, 1 = stop, 2 = stop requested from the editor.
#[derive(Clone, Default)]
pub struct StopRequest(Arc<AtomicU8>);

impl StopRequest {
    pub fn request(&self, from_editor: bool) {
        self.0.store(if from_editor { 2 } else { 1 }, Ordering::SeqCst);
    }
    #[cfg_attr(windows, allow(dead_code))] // the Windows backend stops through its message loop
    pub fn is_requested(&self) -> bool {
        self.0.load(Ordering::SeqCst) != 0
    }
    pub fn from_editor(&self) -> bool {
        self.0.load(Ordering::SeqCst) == 2
    }
}

pub struct RecorderCore {
    options: RecordOptions,
    on_event: OnEvent,
    on_stop: Option<OnStop>,
    events: Vec<Value>,
    held: Vec<u16>,
    last_t: Option<f64>,
    /// Pending merged movement: (dx, dy, first_t, last_t).
    pending_move: Option<(i32, i32, f64, f64)>,
}

fn is_type(ev: &Value, kind: &str) -> bool {
    ev.get("type").and_then(Value::as_str).unwrap_or("key") == kind
}

impl RecorderCore {
    pub fn new(options: RecordOptions, on_event: OnEvent, on_stop: OnStop) -> Self {
        Self {
            options: RecordOptions { mouse: options.mouse, mouse_move: options.mouse && options.mouse_move },
            on_event,
            on_stop: Some(on_stop),
            events: Vec::new(),
            held: Vec::new(),
            last_t: None,
            pending_move: None,
        }
    }

    fn emit(&mut self, mut ev: Value, t: f64) {
        let delay = match self.last_t {
            Some(last) => ((t - last) * 1000.0).round().max(0.0) as u64,
            None => 0,
        };
        self.last_t = Some(t);
        ev["delay"] = json!(delay);
        (self.on_event)(&ev);
        self.events.push(ev);
    }

    fn flush_move(&mut self) {
        if let Some((dx, dy, _, last_t)) = self.pending_move.take() {
            if dx != 0 || dy != 0 {
                self.emit(json!({"type": "move", "dx": dx, "dy": dy}), last_t);
            }
        }
    }

    /// Key or mouse button (evdev code); `t` is a timestamp in seconds.
    pub fn feed_key(&mut self, code: u16, down: bool, t: f64) {
        if !(is_keyboard_code(code) || (self.options.mouse && is_mouse_button(code))) {
            return;
        }
        let is_held = self.held.contains(&code);
        if down && is_held {
            return; // auto-repeat, or duplicate from a virtual keyboard re-emitting keys
        }
        if !down && !is_held {
            return; // release of a key pressed before the recording started
        }
        if down {
            self.held.push(code);
        } else {
            self.held.retain(|&c| c != code);
        }
        self.flush_move();
        self.emit(json!({"code": code, "down": down}), t);
    }

    pub fn feed_move(&mut self, dx: i32, dy: i32, t: f64) {
        if !self.options.mouse_move || (dx == 0 && dy == 0) {
            return;
        }
        let pending = self.pending_move.get_or_insert((0, 0, t, t));
        pending.0 += dx;
        pending.1 += dy;
        pending.3 = t;
        if t - pending.2 >= MOVE_MERGE_WINDOW {
            self.flush_move();
        }
    }

    pub fn feed_wheel(&mut self, dx: i32, dy: i32, t: f64) {
        if !self.options.mouse || (dx == 0 && dy == 0) {
            return;
        }
        self.flush_move();
        self.emit(json!({"type": "wheel", "dx": dx, "dy": dy}), t);
    }

    /// The browser fires "click" on release, so a stop from the editor's button leaves:
    /// moves, left down, (moves), left up, (moves). That tail is not part of the macro.
    fn trim_stop_click(&mut self) {
        fn pop_moves(events: &mut Vec<Value>) {
            while events.last().is_some_and(|e| is_type(e, "move")) {
                events.pop();
            }
        }
        fn pop_left(events: &mut Vec<Value>, down: bool) {
            let is_left = events.last().is_some_and(|e| {
                is_type(e, "key")
                    && e.get("code").and_then(Value::as_u64) == Some(BTN_LEFT as u64)
                    && e.get("down").and_then(Value::as_bool) == Some(down)
            });
            if is_left {
                events.pop();
            }
        }
        pop_moves(&mut self.events);
        pop_left(&mut self.events, false);
        pop_moves(&mut self.events);
        pop_left(&mut self.events, true);
        pop_moves(&mut self.events);
    }

    /// Ends the recording and hands the events to the plugin.
    pub fn finish(&mut self, stopped_from_editor: bool) {
        self.flush_move();
        if stopped_from_editor && self.options.mouse {
            self.trim_stop_click();
        }
        // Keys and buttons still held at the end get a release event.
        let mut held: Vec<u16> = Vec::new();
        for ev in &self.events {
            if !is_type(ev, "key") {
                continue;
            }
            let code = ev.get("code").and_then(Value::as_u64).unwrap_or(0) as u16;
            if ev.get("down").and_then(Value::as_bool).unwrap_or(false) {
                held.push(code);
            } else {
                held.retain(|&c| c != code);
            }
        }
        held.sort_unstable();
        held.dedup();
        for code in held {
            self.events.push(json!({"code": code, "down": false, "delay": 0}));
        }
        if let Some(on_stop) = self.on_stop.take() {
            on_stop(std::mem::take(&mut self.events));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn run(mouse: bool, mouse_move: bool, editor_stop: bool) -> Vec<Value> {
        let out = Arc::new(Mutex::new(Vec::new()));
        let out2 = out.clone();
        let mut r = RecorderCore::new(
            RecordOptions { mouse, mouse_move },
            Box::new(|_| {}),
            Box::new(move |evs| *out2.lock().unwrap() = evs),
        );
        r.feed_key(30, true, 0.0);
        r.feed_key(30, true, 0.02); // auto-repeat: ignored
        r.feed_key(30, false, 0.05);
        r.feed_key(31, false, 0.06); // released but never pressed: ignored
        for i in 0..20 {
            r.feed_move(2, 1, 0.1 + i as f64 * 0.002);
        }
        r.feed_key(BTN_LEFT, true, 0.2);
        r.feed_key(BTN_LEFT, false, 0.25);
        r.feed_wheel(0, -1, 0.3);
        r.feed_key(57, true, 0.35); // held at the end: release appended
        r.feed_move(50, 20, 0.4); // travel to the Stop button + click
        r.feed_key(BTN_LEFT, true, 0.5);
        r.feed_move(1, 0, 0.52);
        r.feed_key(BTN_LEFT, false, 0.55);
        r.finish(editor_stop);
        let events = out.lock().unwrap().clone();
        events
    }

    #[test]
    fn keyboard_only_ignores_mouse_and_repeats() {
        let evs = run(false, true, false);
        let codes: Vec<_> = evs.iter().map(|e| (e["code"].as_u64().unwrap(), e["down"].as_bool().unwrap())).collect();
        assert_eq!(codes, vec![(30, true), (30, false), (57, true), (57, false)]);
        assert_eq!(evs[1]["delay"], 50);
    }

    #[test]
    fn moves_are_merged_without_losing_distance() {
        let evs = run(true, true, false);
        let moves: Vec<_> = evs.iter().filter(|e| is_type(e, "move")).collect();
        let (dx, dy): (i64, i64) = moves.iter().fold((0, 0), |a, e| (a.0 + e["dx"].as_i64().unwrap(), a.1 + e["dy"].as_i64().unwrap()));
        assert_eq!((dx, dy), (40 + 51, 20 + 20));
        assert!(moves.len() < 20, "movement reports should be merged");
    }

    #[test]
    fn mouse_without_moves_keeps_buttons_and_wheel() {
        let evs = run(true, false, false);
        assert!(!evs.iter().any(|e| is_type(e, "move")));
        assert!(evs.iter().any(|e| is_type(e, "wheel")));
        assert!(evs.iter().any(|e| e["code"] == BTN_LEFT));
    }

    #[test]
    fn editor_stop_drops_the_stop_click() {
        let evs = run(true, true, true);
        // Ends with: Space down (57), then its automatic release; the final click and moves are gone.
        let tail: Vec<_> = evs.iter().rev().take(2).map(|e| (e["code"].as_u64(), e["down"].as_bool())).collect();
        assert_eq!(tail, vec![(Some(57), Some(false)), (Some(57), Some(true))]);
    }
}
