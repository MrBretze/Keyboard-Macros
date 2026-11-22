//! Keyboard and mouse simulation / capture, one backend per platform.
//!
//! Each backend provides the same API:
//!   - `VirtualInput` with `prepare`, `key`, `move_by`, `wheel` and `release_all`;
//!   - `start_recording(options, on_event, on_stop)` returning a `RecorderHandle` with `stop`.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{RecorderHandle, VirtualInput, start_recording};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{RecorderHandle, VirtualInput, start_recording};

#[cfg(not(any(target_os = "linux", windows)))]
compile_error!("Keyboard Macros supports Linux and Windows only.");
