//! Keyboard and mouse simulation / capture, one backend per platform.
//!
//! Each backend provides the same API:
//!   - `VirtualInput` with `prepare`, `key`, `move_by`, `wheel`, `release_all` and `status`;
//!   - `diagnostics()` returning checks for the About tab;
//!   - `requirements()` returning setup problems as error codes (empty when all is well);
//!     this module wraps it to support the `KEYMACRO_SIMULATE_SETUP` developer variable.
//!   - `start_recording(options, on_event, on_stop)` returning a `RecorderHandle` with `stop`.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux::requirements as platform_requirements;
#[cfg(target_os = "linux")]
pub use linux::{RecorderHandle, VirtualInput, diagnostics, start_recording};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows::requirements as platform_requirements;
#[cfg(windows)]
pub use windows::{RecorderHandle, VirtualInput, diagnostics, start_recording};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos::requirements as platform_requirements;
#[cfg(target_os = "macos")]
pub use macos::{RecorderHandle, VirtualInput, diagnostics, start_recording};

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
compile_error!("Keyboard Macros supports Linux, Windows and macOS only.");

use crate::common::InputError;
use std::sync::OnceLock;

/// Setup error codes the plugin knows how to explain (editor banner, docs/SETUP.md).
const SETUP_CODES: [&str; 5] = [
    "uinput_missing",
    "uinput_denied",
    "no_keyboards",
    "permission_accessibility",
    "permission_input_monitoring",
];

/// Developer aid: `KEYMACRO_SIMULATE_SETUP=uinput_denied,no_keyboards` makes the plugin behave
/// as if these setup problems were present (banner, errors on key press and on record),
/// without touching the system's permissions. Unknown codes are ignored.
pub fn simulated_problems() -> &'static [&'static str] {
    static SIMULATED: OnceLock<Vec<&'static str>> = OnceLock::new();
    SIMULATED.get_or_init(|| {
        let wanted = std::env::var("KEYMACRO_SIMULATE_SETUP").unwrap_or_default();
        SETUP_CODES.iter().copied().filter(|code| wanted.split(',').any(|w| w.trim() == *code)).collect()
    })
}

/// Setup problems that prevent playback or recording, as error codes (empty when all is well).
pub fn requirements() -> Vec<&'static str> {
    let simulated = simulated_problems();
    if simulated.is_empty() { platform_requirements() } else { simulated.to_vec() }
}

fn simulated_error(codes: &[&str]) -> Option<InputError> {
    simulated_problems().iter().find(|c| codes.contains(c)).map(|c| InputError::new(c, "simulated"))
}

/// The error a playback would hit under `KEYMACRO_SIMULATE_SETUP`, if any.
pub fn simulated_playback_error() -> Option<InputError> {
    simulated_error(&["uinput_missing", "uinput_denied", "permission_accessibility"])
}

/// The error a recording would hit under `KEYMACRO_SIMULATE_SETUP`, if any.
pub fn simulated_recording_error() -> Option<InputError> {
    simulated_error(&["no_keyboards", "permission_input_monitoring"])
}
