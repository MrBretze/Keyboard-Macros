//! Pieces shared by every platform: logging, key code helpers, errors, macro events.
//!
//! Macro event formats (stored in the action settings, identical on every platform):
//!   {"code": 30, "down": true, "delay": 0}              key or mouse button (Linux evdev code)
//!   {"type": "move", "dx": 12, "dy": -3, "delay": 8}    relative mouse movement
//!   {"type": "wheel", "dx": 0, "dy": 1, "delay": 120}   wheel notches (dy > 0 = up, dx > 0 = right)
//! "delay" is in milliseconds since the previous event.

use serde_json::Value;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Prefix of our virtual devices' names, so the recorder can ignore our own output.
#[cfg_attr(windows, allow(dead_code))] // only the Linux backend creates named devices
pub const VIRTUAL_PREFIX: &str = "OpenDeck KeyMacro";

pub const KEY_MAX: u16 = 0x2FF;
const BTN_MISC: u16 = 0x100;
const KEY_OK: u16 = 0x160;

pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;
pub const BTN_SIDE: u16 = 0x113;
pub const BTN_EXTRA: u16 = 0x114;
pub const MOUSE_BUTTONS: [u16; 5] = [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA];

/// Keyboard keys only (mouse, gamepad and touchpad buttons are excluded).
pub fn is_keyboard_code(code: u16) -> bool {
    code > 0 && code <= KEY_MAX && !(BTN_MISC..KEY_OK).contains(&code)
}

pub fn is_mouse_button(code: u16) -> bool {
    MOUSE_BUTTONS.contains(&code)
}

/// Error with a stable code that the property inspector translates.
#[derive(Debug, Clone)]
pub struct InputError {
    pub code: &'static str,
    pub detail: String,
}

impl InputError {
    pub fn new(code: &'static str, detail: impl ToString) -> Self {
        Self { code, detail: detail.to_string() }
    }
}

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

// ---------------------------------------------------------------- macro events

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MacroEvent {
    Key { code: u16, down: bool },
    Move { dx: i32, dy: i32 },
    Wheel { dx: i32, dy: i32 },
}

/// One playable step: an event and the delay before it.
#[derive(Debug, Clone)]
pub struct Step {
    pub event: MacroEvent,
    pub delay_ms: f64,
}

/// Reads the events of one macro from the settings, skipping disabled or invalid ones.
pub fn parse_steps(events: &Value) -> Vec<Step> {
    let Some(list) = events.as_array() else { return Vec::new() };
    list.iter()
        .filter(|e| !e.get("disabled").and_then(Value::as_bool).unwrap_or(false))
        .filter_map(|e| {
            let int = |key: &str| e.get(key).and_then(Value::as_f64).unwrap_or(0.0).round() as i32;
            let event = match e.get("type").and_then(Value::as_str).unwrap_or("key") {
                "move" => MacroEvent::Move { dx: int("dx"), dy: int("dy") },
                "wheel" => MacroEvent::Wheel { dx: int("dx"), dy: int("dy") },
                _ => MacroEvent::Key {
                    code: u16::try_from(e.get("code")?.as_u64()?).ok()?,
                    down: e.get("down").and_then(Value::as_bool).unwrap_or(false),
                },
            };
            let delay_ms = e.get("delay").and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
            Some(Step { event, delay_ms })
        })
        .collect()
}

// ---------------------------------------------------------------- logging

/// Folder of the plugin: the executable sits at its root.
pub fn plugin_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

pub fn write_log(message: &str) {
    let path = LOG_PATH.get_or_init(|| plugin_dir().join("plugin.log"));
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{h:02}:{m:02}:{s:02}Z {message}");
    }
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::common::write_log(&format!($($arg)*)) };
}
