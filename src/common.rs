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
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Prefix of our virtual devices' names, so the recorder can ignore our own output.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // only the Linux backend creates named devices
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

/// Log files are rotated past this size: plugin.log becomes plugin.log.old.
const LOG_MAX_BYTES: u64 = 1024 * 1024;

/// Verbose logging (every message exchanged with OpenDeck, every replayed event).
/// Toggled from the editor's About tab and saved in the plugin's global settings.
pub static DEBUG: AtomicBool = AtomicBool::new(false);

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

pub fn log_path() -> &'static Path {
    LOG_PATH.get_or_init(|| plugin_dir().join("plugin.log"))
}

pub fn write_log(message: &str) {
    let path = log_path();
    if std::fs::metadata(path).is_ok_and(|m| m.len() > LOG_MAX_BYTES) {
        let _ = std::fs::rename(path, path.with_extension("log.old"));
    }
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{h:02}:{m:02}:{s:02}Z {message}");
    }
}

/// Last `max_bytes` of the log, starting at a line boundary.
pub fn read_log_tail(max_bytes: u64) -> String {
    let Ok(bytes) = std::fs::read(log_path()) else { return String::new() };
    let start = bytes.len().saturating_sub(max_bytes as usize);
    let mut tail = &bytes[start..];
    if start > 0 {
        if let Some(newline) = tail.iter().position(|&b| b == b'\n') {
            tail = &tail[newline + 1..];
        }
    }
    String::from_utf8_lossy(tail).into_owned()
}

pub fn clear_log() -> std::io::Result<()> {
    let path = log_path();
    let _ = std::fs::remove_file(path.with_extension("log.old"));
    std::fs::write(path, b"")
}

/// Opens a folder in the system file manager (through the desktop portal in a Flatpak).
pub fn open_folder(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    let mut command = std::process::Command::new("explorer");
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(not(any(windows, target_os = "macos")))]
    let mut command = std::process::Command::new("xdg-open");
    command.arg(path).spawn().map(|_| ())
}

/// Shortens long values (images are sent as data URLs) before they go into the log.
pub fn shorten_for_log(text: &str) -> String {
    const MAX: usize = 600;
    let mut out = String::with_capacity(text.len().min(MAX + 32));
    for part in text.split("data:image/") {
        if !out.is_empty() {
            // Keep only the image type, not its base64 data.
            let kind: String = part.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '+').collect();
            let rest = part.find('"').map_or("", |i| &part[i..]);
            out.push_str(&format!("data:image/{kind};…"));
            out.push_str(rest);
        } else {
            out.push_str(part);
        }
    }
    if out.chars().count() > MAX {
        out = out.chars().take(MAX).collect::<String>() + "…";
    }
    out
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::common::write_log(&format!($($arg)*)) };
}

/// Logs only when verbose logging is enabled.
#[macro_export]
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if $crate::common::DEBUG.load(std::sync::atomic::Ordering::Relaxed) {
            $crate::common::write_log(&format!("[debug] {}", format!($($arg)*)))
        }
    };
}
