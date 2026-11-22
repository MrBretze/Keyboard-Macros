//! Keyboard and mouse simulation / capture, one backend per platform.
//!
//! Each backend provides the same API:
//!   - `VirtualInput` with `prepare`, `key`, `move_by`, `wheel`, `release_all` and `status`;
//!   - `diagnostics()` returning checks for the About tab;
//!   - `start_recording(options, on_event, on_stop)` returning a `RecorderHandle` with `stop`.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{RecorderHandle, VirtualInput, diagnostics, start_recording};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{RecorderHandle, VirtualInput, diagnostics, start_recording};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{RecorderHandle, VirtualInput, diagnostics, start_recording};

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
compile_error!("Keyboard Macros supports Linux, Windows and macOS only.");
