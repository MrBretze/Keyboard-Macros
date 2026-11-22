//! Windows input backend.
//!
//! - Playback: SendInput with hardware scan codes, so macros replay physical keys
//!   whatever the keyboard layout (same behaviour as the Linux backend). Mouse buttons
//!   and the wheel go through SendInput; movements move the cursor directly, so they
//!   replay exactly (no pointer acceleration).
//! - Recording: low-level keyboard and mouse hooks (WH_KEYBOARD_LL / WH_MOUSE_LL).
//!   Injected events, including our own playback, are ignored.
//!
//! Macros store Linux evdev codes. For keys of the main block, evdev codes equal PC scan
//! codes (set 1); other keys are translated with the tables below.

use crate::common::{BTN_EXTRA, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, BTN_SIDE, InputError};
use crate::log;
use crate::recorder::{OnEvent, OnStop, RecordOptions, RecorderCore, StopRequest};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetCursorPos, GetMessageW, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED,
    LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, PostThreadMessageW, SetCursorPos, SetWindowsHookExW,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

const XBUTTON1: u32 = 0x0001;
const XBUTTON2: u32 = 0x0002;
const WHEEL_DELTA: i32 = 120;
const VK_NUMLOCK: u32 = 0x90;
/// Keys of the main block: evdev 1..=88 equals scan code 0x01..=0x58 (non-extended).
const DIRECT_SCAN_MAX: u16 = 88;

/// evdev code -> (scan code, extended) for keys that differ from the plain scan code.
#[rustfmt::skip] // aligned table, one key per column
const SCAN_EXT: &[(u16, u16, bool)] = &[
    (69, 0x45, true),   // NUMLOCK (reported as extended by Windows)
    (96, 0x1C, true),   // KPENTER
    (97, 0x1D, true),   // RIGHTCTRL
    (98, 0x35, true),   // KPSLASH
    (99, 0x37, true),   // SYSRQ / Print Screen
    (100, 0x38, true),  // RIGHTALT / AltGr
    (102, 0x47, true),  // HOME
    (103, 0x48, true),  // UP
    (104, 0x49, true),  // PAGEUP
    (105, 0x4B, true),  // LEFT
    (106, 0x4D, true),  // RIGHT
    (107, 0x4F, true),  // END
    (108, 0x50, true),  // DOWN
    (109, 0x51, true),  // PAGEDOWN
    (110, 0x52, true),  // INSERT
    (111, 0x53, true),  // DELETE
    (116, 0x5E, true),  // POWER
    (117, 0x59, false), // KPEQUAL
    (125, 0x5B, true),  // LEFTMETA
    (126, 0x5C, true),  // RIGHTMETA
    (127, 0x5D, true),  // COMPOSE / Menu
    (183, 0x64, false), (184, 0x65, false), (185, 0x66, false), (186, 0x67, false), // F13-F16
    (187, 0x68, false), (188, 0x69, false), (189, 0x6A, false), (190, 0x6B, false), // F17-F20
    (191, 0x6C, false), (192, 0x6D, false), (193, 0x6E, false), (194, 0x76, false), // F21-F24
];

/// evdev code -> virtual-key code, for keys that are only reliable as virtual keys.
/// Not supported on Windows (no standard key): BRIGHTNESSDOWN (224), BRIGHTNESSUP (225).
#[rustfmt::skip] // aligned table, one key per column
const VK_KEYS: &[(u16, u16)] = &[
    (113, 0xAD), // MUTE
    (114, 0xAE), // VOLUMEDOWN
    (115, 0xAF), // VOLUMEUP
    (119, 0x13), // PAUSE
    (140, 0xB7), // CALC (VK_LAUNCH_APP2)
    (158, 0xA6), // BACK (VK_BROWSER_BACK)
    (159, 0xA7), // FORWARD (VK_BROWSER_FORWARD)
    (163, 0xB0), // NEXTSONG
    (164, 0xB3), // PLAYPAUSE
    (165, 0xB1), // PREVIOUSSONG
    (166, 0xB2), // STOPCD
];
const VK_EXTENDED: &[u16] = &[0xAD, 0xAE, 0xAF, 0xB0, 0xB1, 0xB2, 0xB3, 0xA6, 0xA7, 0xB7];

enum Target {
    Vk(u16),
    Scan(u16, bool),
}

fn evdev_to_windows(code: u16) -> Option<Target> {
    if let Some(&(_, vk)) = VK_KEYS.iter().find(|(c, _)| *c == code) {
        return Some(Target::Vk(vk));
    }
    if let Some(&(_, scan, ext)) = SCAN_EXT.iter().find(|(c, _, _)| *c == code) {
        return Some(Target::Scan(scan, ext));
    }
    (1..=DIRECT_SCAN_MAX).contains(&code).then_some(Target::Scan(code, false))
}

fn windows_to_evdev(vk: u32, scan: u32, extended: bool) -> Option<u16> {
    if let Some(&(code, _)) = VK_KEYS.iter().find(|(_, v)| u32::from(*v) == vk) {
        return Some(code);
    }
    if vk == VK_NUMLOCK {
        return Some(69); // Windows reports scan 0x45 extended
    }
    if let Some(&(code, _, _)) = SCAN_EXT.iter().find(|(_, s, e)| u32::from(*s) == scan && *e == extended) {
        return Some(code);
    }
    (!extended && (1..=u32::from(DIRECT_SCAN_MAX)).contains(&scan)).then_some(scan as u16)
}

/// evdev mouse button -> (down flag, up flag, mouseData)
fn mouse_button_flags(code: u16) -> Option<(MOUSE_EVENT_FLAGS, MOUSE_EVENT_FLAGS, u32)> {
    Some(match code {
        BTN_LEFT => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
        BTN_RIGHT => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
        BTN_MIDDLE => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
        BTN_SIDE => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON1),
        BTN_EXTRA => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON2),
        _ => return None,
    })
}

// ---------------------------------------------------------------- playback

#[derive(Default)]
pub struct VirtualInput {
    pressed: Mutex<HashSet<u16>>,
}

fn send(input: INPUT) -> Result<(), InputError> {
    // SAFETY: one fully initialised INPUT structure, with its exact size.
    let sent = unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) };
    if sent == 1 {
        Ok(())
    } else {
        Err(InputError::new("sendinput_failed", std::io::Error::last_os_error().raw_os_error().unwrap_or(0)))
    }
}

fn mouse_input(flags: MOUSE_EVENT_FLAGS, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: data as _, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn keyboard_input(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

impl VirtualInput {
    /// Nothing to create on Windows: SendInput needs no device.
    pub fn prepare(&self, _keyboard: bool, _mouse: bool) -> Result<(), InputError> {
        Ok(())
    }

    /// Keyboard key or mouse button (evdev code).
    pub fn key(&self, code: u16, down: bool) -> Result<(), InputError> {
        let mut pressed = self.pressed.lock().unwrap();
        if let Some((down_flag, up_flag, data)) = mouse_button_flags(code) {
            send(mouse_input(if down { down_flag } else { up_flag }, data as i32))?;
        } else {
            let Some(target) = evdev_to_windows(code) else {
                log!("unsupported key on Windows: {code}");
                return Ok(());
            };
            let mut flags = if down { 0 } else { KEYEVENTF_KEYUP };
            let input = match target {
                Target::Vk(vk) => {
                    if VK_EXTENDED.contains(&vk) {
                        flags |= KEYEVENTF_EXTENDEDKEY;
                    }
                    keyboard_input(vk, 0, flags)
                }
                Target::Scan(scan, extended) => {
                    flags |= KEYEVENTF_SCANCODE;
                    if extended {
                        flags |= KEYEVENTF_EXTENDEDKEY;
                    }
                    keyboard_input(0, scan, flags)
                }
            };
            send(input)?;
        }
        if down {
            pressed.insert(code);
        } else {
            pressed.remove(&code);
        }
        Ok(())
    }

    pub fn move_by(&self, dx: i32, dy: i32) -> Result<(), InputError> {
        let _guard = self.pressed.lock().unwrap();
        let mut pt = POINT { x: 0, y: 0 };
        // SAFETY: valid pointer to a POINT; plain cursor calls.
        unsafe {
            GetCursorPos(&mut pt);
            SetCursorPos(pt.x.saturating_add(dx), pt.y.saturating_add(dy));
        }
        Ok(())
    }

    pub fn wheel(&self, dx: i32, dy: i32) -> Result<(), InputError> {
        let _guard = self.pressed.lock().unwrap();
        if dy != 0 {
            send(mouse_input(MOUSEEVENTF_WHEEL, dy.saturating_mul(WHEEL_DELTA)))?;
        }
        if dx != 0 {
            send(mouse_input(MOUSEEVENTF_HWHEEL, dx.saturating_mul(WHEEL_DELTA)))?;
        }
        Ok(())
    }

    /// Status line for the About tab.
    pub fn status(&self) -> String {
        "SendInput (no device to create)".to_owned()
    }

    pub fn release_all(&self) {
        let pressed: Vec<u16> = self.pressed.lock().unwrap().iter().copied().collect();
        for code in pressed {
            let _ = self.key(code, false);
        }
    }
}

// ---------------------------------------------------------------- recording

/// State used by the hook callbacks, which run on the recording thread.
struct HookState {
    core: RecorderCore,
    last_pt: Option<(i32, i32)>,
    wheel_rest: [i32; 2], // sub-notch amounts (high-resolution wheels): [horizontal, vertical]
}

thread_local! {
    static HOOK_STATE: RefCell<Option<HookState>> = const { RefCell::new(None) };
}

fn with_state(f: impl FnOnce(&mut HookState)) {
    HOOK_STATE.with(|s| {
        if let Some(state) = s.borrow_mut().as_mut() {
            f(state);
        }
    });
}

unsafe extern "system" fn keyboard_proc(n_code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    let message = w_param as u32;
    if n_code == 0 && matches!(message, WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP) {
        // SAFETY: for WH_KEYBOARD_LL, lParam points to a KBDLLHOOKSTRUCT.
        let k = unsafe { &*(l_param as *const KBDLLHOOKSTRUCT) };
        if k.flags & LLKHF_INJECTED == 0
            && let Some(code) = windows_to_evdev(k.vkCode, k.scanCode, k.flags & LLKHF_EXTENDED != 0)
        {
            let down = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
            with_state(|s| s.core.feed_key(code, down, f64::from(k.time) / 1000.0));
        }
    }
    // SAFETY: forwards the event to the next hook, as required.
    unsafe { CallNextHookEx(std::ptr::null_mut(), n_code, w_param, l_param) }
}

unsafe extern "system" fn mouse_proc(n_code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    if n_code == 0 {
        // SAFETY: for WH_MOUSE_LL, lParam points to an MSLLHOOKSTRUCT.
        let m = unsafe { &*(l_param as *const MSLLHOOKSTRUCT) };
        if m.flags & LLMHF_INJECTED == 0 {
            let t = f64::from(m.time) / 1000.0;
            let high = (m.mouseData >> 16) as u16 as i16 as i32; // signed high word
            with_state(|s| match w_param as u32 {
                WM_MOUSEMOVE => {
                    if let Some((x, y)) = s.last_pt {
                        s.core.feed_move(m.pt.x.saturating_sub(x), m.pt.y.saturating_sub(y), t);
                    }
                    s.last_pt = Some((m.pt.x, m.pt.y));
                }
                WM_LBUTTONDOWN => s.core.feed_key(BTN_LEFT, true, t),
                WM_LBUTTONUP => s.core.feed_key(BTN_LEFT, false, t),
                WM_RBUTTONDOWN => s.core.feed_key(BTN_RIGHT, true, t),
                WM_RBUTTONUP => s.core.feed_key(BTN_RIGHT, false, t),
                WM_MBUTTONDOWN => s.core.feed_key(BTN_MIDDLE, true, t),
                WM_MBUTTONUP => s.core.feed_key(BTN_MIDDLE, false, t),
                msg @ (WM_XBUTTONDOWN | WM_XBUTTONUP) => {
                    let code = if high as u32 == XBUTTON1 { BTN_SIDE } else { BTN_EXTRA };
                    s.core.feed_key(code, msg == WM_XBUTTONDOWN, t);
                }
                msg @ (WM_MOUSEWHEEL | WM_MOUSEHWHEEL) => {
                    let axis = usize::from(msg == WM_MOUSEWHEEL);
                    s.wheel_rest[axis] = s.wheel_rest[axis].saturating_add(high);
                    let notches = s.wheel_rest[axis] / WHEEL_DELTA;
                    if notches != 0 {
                        s.wheel_rest[axis] =
                            s.wheel_rest[axis].saturating_sub(notches.saturating_mul(WHEEL_DELTA));
                        let (dx, dy) = if axis == 1 { (0, notches) } else { (notches, 0) };
                        s.core.feed_wheel(dx, dy, t);
                    }
                }
                _ => {}
            });
        }
    }
    // SAFETY: forwards the event to the next hook, as required.
    unsafe { CallNextHookEx(std::ptr::null_mut(), n_code, w_param, l_param) }
}

pub struct RecorderHandle {
    stop: StopRequest,
    thread_id: Arc<AtomicU32>,
}

impl RecorderHandle {
    pub fn stop(&self, from_editor: bool) {
        self.stop.request(from_editor);
        // Posting can fail if the thread's message queue is busy: retry for up to a second.
        for _ in 0..100 {
            // SAFETY: posts WM_QUIT to the recording thread's message loop.
            if unsafe { PostThreadMessageW(self.thread_id.load(Ordering::SeqCst), WM_QUIT, 0, 0) } != 0 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        log!("could not stop the recording thread");
    }
}

/// Starts recording in a background thread with its own message loop.
pub fn start_recording(
    options: RecordOptions,
    on_event: OnEvent,
    on_stop: OnStop,
) -> Result<(RecorderHandle, Vec<String>), InputError> {
    let stop = StopRequest::default();
    let thread_id = Arc::new(AtomicU32::new(0));
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    let (thread_stop, thread_tid) = (stop.clone(), thread_id.clone());
    std::thread::spawn(move || {
        // SAFETY: Win32 calls on the current thread; hooks are removed before it exits.
        unsafe {
            thread_tid.store(GetCurrentThreadId(), Ordering::SeqCst);
            HOOK_STATE.with(|s| {
                *s.borrow_mut() = Some(HookState {
                    core: RecorderCore::new(options, on_event, on_stop),
                    last_pt: None,
                    wheel_rest: [0, 0],
                })
            });
            let module = GetModuleHandleW(std::ptr::null());
            let mut hooks = vec![SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), module, 0)];
            if options.mouse {
                hooks.push(SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0));
            }
            if hooks.iter().any(|h| h.is_null()) {
                let error = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                for h in hooks.iter().filter(|h| !h.is_null()) {
                    UnhookWindowsHookEx(*h);
                }
                HOOK_STATE.with(|s| s.borrow_mut().take());
                let _ = ready_tx.send(Err(error.to_string()));
                return;
            }
            let _ = ready_tx.send(Ok(()));
            log!("input hooks installed: {}", hooks.len());
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {}
            for h in hooks {
                UnhookWindowsHookEx(h);
            }
            if let Some(mut state) = HOOK_STATE.with(|s| s.borrow_mut().take()) {
                state.core.finish(thread_stop.was_from_editor());
            }
        }
    });
    match ready_rx.recv_timeout(std::time::Duration::from_secs(2)) {
        Ok(Ok(())) => {
            let mut names = vec!["Windows keyboard hook".to_owned()];
            if options.mouse {
                names.push("Windows mouse hook".to_owned());
            }
            Ok((RecorderHandle { stop, thread_id }, names))
        }
        Ok(Err(error)) => Err(InputError::new("hook_failed", error)),
        Err(_) => Err(InputError::new("hook_failed", "timeout")),
    }
}

// ---------------------------------------------------------------- diagnostics

/// Checks run from the About tab: tries to install (and immediately remove) the hooks
/// used for recording. `ok` is null for information-only lines.
pub fn diagnostics() -> Vec<Value> {
    let mut checks = Vec::new();
    for (id, kind, proc_) in [
        (
            "keyboardHook",
            WH_KEYBOARD_LL,
            keyboard_proc as unsafe extern "system" fn(i32, WPARAM, LPARAM) -> LRESULT,
        ),
        ("mouseHook", WH_MOUSE_LL, mouse_proc),
    ] {
        // SAFETY: installs a hook and removes it right away; the procedures are valid.
        let hook = unsafe { SetWindowsHookExW(kind, Some(proc_), GetModuleHandleW(std::ptr::null()), 0) };
        if hook.is_null() {
            let error = std::io::Error::last_os_error();
            checks.push(json!({"id": id, "ok": false, "detail": error.to_string()}));
        } else {
            // SAFETY: the hook was just installed by this function.
            unsafe { UnhookWindowsHookEx(hook) };
            checks.push(json!({"id": id, "ok": true, "detail": "OK"}));
        }
    }
    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_mapping_round_trips() {
        for code in (1..=88).chain(SCAN_EXT.iter().map(|e| e.0)).chain(VK_KEYS.iter().map(|e| e.0)) {
            let back = match evdev_to_windows(code).unwrap() {
                Target::Vk(vk) => windows_to_evdev(u32::from(vk), 0, false),
                Target::Scan(scan, ext) => {
                    windows_to_evdev(if code == 69 { VK_NUMLOCK } else { 0 }, u32::from(scan), ext)
                }
            };
            assert_eq!(back, Some(code), "evdev code {code}");
        }
    }
}
