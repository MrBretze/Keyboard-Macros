//! macOS input backend (CoreGraphics, called directly through its C API).
//!
//! - Playback: CGEvent keyboard, mouse and scroll events posted to the HID level. The
//!   cursor is moved to an absolute position (current position + recorded delta), so
//!   movements replay exactly, like on Windows.
//! - Recording: a listen-only event tap. Events posted by this process are ignored.
//!
//! Permissions (System Settings → Privacy & Security), granted to OpenDeck:
//!   - Accessibility: needed to post events (playback);
//!   - Input Monitoring: needed to listen to events (recording).
//!
//! Macros store Linux evdev codes; they are translated to macOS virtual key codes, which
//! also identify physical keys (ANSI / ISO positions).
//! Not supported on macOS (no equivalent key): Scroll Lock, Print Screen, Pause, Power, media
//! and browser keys, brightness keys and F21-F24.

use crate::common::{BTN_EXTRA, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, BTN_SIDE, InputError};
use crate::log;
use crate::recorder::{OnEvent, OnStop, RecordOptions, RecorderCore, StopRequest};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::{Mutex, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------- CoreGraphics FFI

type CGEventRef = *mut c_void;
type CGEventSourceRef = *mut c_void;
type CFMachPortRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFRunLoopRef = *mut c_void;
type CFStringRef = *const c_void;
type CGEventTapCallBack = unsafe extern "C" fn(
    proxy: *mut c_void,
    kind: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceCreate(state: i32) -> CGEventSourceRef;
    fn CGEventCreate(source: CGEventSourceRef) -> CGEventRef;
    fn CGEventCreateKeyboardEvent(source: CGEventSourceRef, keycode: u16, key_down: bool) -> CGEventRef;
    fn CGEventCreateMouseEvent(
        source: CGEventSourceRef,
        kind: u32,
        position: CGPoint,
        button: u32,
    ) -> CGEventRef;
    fn CGEventCreateScrollWheelEvent2(
        source: CGEventSourceRef,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> CGEventRef;
    fn CGEventPost(tap: u32, event: CGEventRef);
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CGEventGetFlags(event: CGEventRef) -> u64;
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGPreflightPostEventAccess() -> bool;
    fn CGRequestPostEventAccess() -> bool;
    fn CGPreflightListenEventAccess() -> bool;
    fn CGRequestListenEventAccess() -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopDefaultMode: CFStringRef;
    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;
    fn CFMachPortInvalidate(port: CFMachPortRef);
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(run_loop: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRemoveSource(run_loop: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRunInMode(mode: CFStringRef, seconds: f64, return_after_source_handled: bool) -> i32;
    fn CFRelease(object: *const c_void);
}

const HID_SYSTEM_STATE: i32 = 1; // kCGEventSourceStateHIDSystemState
const HID_EVENT_TAP: u32 = 0; // kCGHIDEventTap
const SESSION_EVENT_TAP: u32 = 1; // kCGSessionEventTap
const HEAD_INSERT: u32 = 0; // kCGHeadInsertEventTap
const LISTEN_ONLY: u32 = 1; // kCGEventTapOptionListenOnly
const SCROLL_UNIT_LINE: u32 = 1; // kCGScrollEventUnitLine

// Event types (CGEventType)
const LEFT_DOWN: u32 = 1;
const LEFT_UP: u32 = 2;
const RIGHT_DOWN: u32 = 3;
const RIGHT_UP: u32 = 4;
const MOUSE_MOVED: u32 = 5;
const LEFT_DRAGGED: u32 = 6;
const RIGHT_DRAGGED: u32 = 7;
const KEY_DOWN: u32 = 10;
const KEY_UP: u32 = 11;
const FLAGS_CHANGED: u32 = 12;
const SCROLL_WHEEL: u32 = 22;
const OTHER_DOWN: u32 = 25;
const OTHER_UP: u32 = 26;
const OTHER_DRAGGED: u32 = 27;
const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

// Event fields (CGEventField)
const FIELD_CLICK_STATE: u32 = 1; // kCGMouseEventClickState
const FIELD_BUTTON_NUMBER: u32 = 3; // kCGMouseEventButtonNumber
const FIELD_KEYCODE: u32 = 9; // kCGKeyboardEventKeycode
const FIELD_SCROLL_AXIS1: u32 = 11; // kCGScrollWheelEventDeltaAxis1 (vertical)
const FIELD_SCROLL_AXIS2: u32 = 12; // kCGScrollWheelEventDeltaAxis2 (horizontal)
const FIELD_SOURCE_PID: u32 = 41; // kCGEventSourceUnixProcessID

// Modifier flags (CGEventFlags)
const FLAG_CAPS: u64 = 0x0001_0000;
const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_OPTION: u64 = 0x0008_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;

// ---------------------------------------------------------------- key mapping

/// evdev code -> macOS virtual key code (kVK_*).
#[rustfmt::skip] // aligned table, one key per column
const KEYS: &[(u16, u16)] = &[
    (1, 0x35),                                                                // ESC
    (2, 0x12), (3, 0x13), (4, 0x14), (5, 0x15), (6, 0x17),                    // 1-5
    (7, 0x16), (8, 0x1A), (9, 0x1C), (10, 0x19), (11, 0x1D),                  // 6-0
    (12, 0x1B), (13, 0x18), (14, 0x33), (15, 0x30),                           // - = Backspace Tab
    (16, 0x0C), (17, 0x0D), (18, 0x0E), (19, 0x0F), (20, 0x11),               // Q W E R T
    (21, 0x10), (22, 0x20), (23, 0x22), (24, 0x1F), (25, 0x23),               // Y U I O P
    (26, 0x21), (27, 0x1E), (28, 0x24), (29, 0x3B),                           // [ ] Enter LeftCtrl
    (30, 0x00), (31, 0x01), (32, 0x02), (33, 0x03), (34, 0x05),               // A S D F G
    (35, 0x04), (36, 0x26), (37, 0x28), (38, 0x25),                           // H J K L
    (39, 0x29), (40, 0x27), (41, 0x32), (42, 0x38), (43, 0x2A),               // ; ' ` LeftShift \
    (44, 0x06), (45, 0x07), (46, 0x08), (47, 0x09), (48, 0x0B),               // Z X C V B
    (49, 0x2D), (50, 0x2E), (51, 0x2B), (52, 0x2F), (53, 0x2C),               // N M , . /
    (54, 0x3C), (55, 0x43), (56, 0x3A), (57, 0x31), (58, 0x39),               // RightShift KP* LeftAlt Space Caps
    (59, 0x7A), (60, 0x78), (61, 0x63), (62, 0x76), (63, 0x60),               // F1-F5
    (64, 0x61), (65, 0x62), (66, 0x64), (67, 0x65), (68, 0x6D),               // F6-F10
    (69, 0x47),                                                               // NUMLOCK -> keypad Clear
    (71, 0x59), (72, 0x5B), (73, 0x5C), (74, 0x4E),                           // KP7 KP8 KP9 KP-
    (75, 0x56), (76, 0x57), (77, 0x58), (78, 0x45),                           // KP4 KP5 KP6 KP+
    (79, 0x53), (80, 0x54), (81, 0x55), (82, 0x52), (83, 0x41),               // KP1 KP2 KP3 KP0 KP.
    (86, 0x0A), (87, 0x67), (88, 0x6F),                                       // ISO key (102ND) F11 F12
    (96, 0x4C), (97, 0x3E), (98, 0x4B), (100, 0x3D),                          // KPEnter RightCtrl KP/ RightAlt
    (102, 0x73), (103, 0x7E), (104, 0x74), (105, 0x7B),                       // Home Up PageUp Left
    (106, 0x7C), (107, 0x77), (108, 0x7D), (109, 0x79),                       // Right End Down PageDown
    (110, 0x72), (111, 0x75),                                                 // Insert -> Help, Delete
    (113, 0x4A), (114, 0x49), (115, 0x48),                                    // Mute VolumeDown VolumeUp
    (117, 0x51), (125, 0x37), (126, 0x36), (127, 0x6E),                       // KP= LeftCmd RightCmd Menu
    (183, 0x69), (184, 0x6B), (185, 0x71), (186, 0x6A),                       // F13-F16
    (187, 0x40), (188, 0x4F), (189, 0x50), (190, 0x5A),                       // F17-F20
];

fn evdev_to_mac(code: u16) -> Option<u16> {
    KEYS.iter().find(|(c, _)| *c == code).map(|&(_, k)| k)
}

fn mac_to_evdev(keycode: u16) -> Option<u16> {
    KEYS.iter().find(|(_, k)| *k == keycode).map(|&(c, _)| c)
}

/// Modifier flag carried by a macOS key code, if it is a modifier key.
fn modifier_flag(keycode: u16) -> Option<u64> {
    match keycode {
        0x38 | 0x3C => Some(FLAG_SHIFT),
        0x3B | 0x3E => Some(FLAG_CONTROL),
        0x3A | 0x3D => Some(FLAG_OPTION),
        0x37 | 0x36 => Some(FLAG_COMMAND),
        0x39 => Some(FLAG_CAPS),
        _ => None,
    }
}

/// evdev mouse button -> CoreGraphics button number (0 left, 1 right, 2+ others).
fn mouse_button_number(code: u16) -> Option<u32> {
    Some(match code {
        BTN_LEFT => 0,
        BTN_RIGHT => 1,
        BTN_MIDDLE => 2,
        BTN_SIDE => 3,
        BTN_EXTRA => 4,
        _ => return None,
    })
}

// ---------------------------------------------------------------- playback

struct Inner {
    source: usize, // CGEventSourceRef, stored as an integer so the struct is Send
    pressed: HashSet<u16>,
    /// Mouse buttons held down (CoreGraphics button numbers), for drag events.
    buttons: HashSet<u32>,
}

pub struct VirtualInput {
    inner: Mutex<Inner>,
}

impl Default for VirtualInput {
    fn default() -> Self {
        // SAFETY: creates an event source; it lives as long as the process.
        let source = unsafe { CGEventSourceCreate(HID_SYSTEM_STATE) } as usize;
        Self { inner: Mutex::new(Inner { source, pressed: HashSet::new(), buttons: HashSet::new() }) }
    }
}

/// Posts an event and releases it.
fn post(event: CGEventRef) -> Result<(), InputError> {
    if event.is_null() {
        return Err(InputError::new("play_failed", "CGEvent creation failed"));
    }
    // SAFETY: `event` is a valid CGEvent we own.
    unsafe {
        CGEventPost(HID_EVENT_TAP, event);
        CFRelease(event);
    }
    Ok(())
}

fn ensure_post_permission() -> Result<(), InputError> {
    // SAFETY: plain permission queries; the request shows the system prompt once.
    if unsafe { CGPreflightPostEventAccess() } {
        return Ok(());
    }
    unsafe { CGRequestPostEventAccess() };
    Err(InputError::new("permission_accessibility", ""))
}

impl Inner {
    fn src(&self) -> CGEventSourceRef {
        self.source as CGEventSourceRef
    }

    fn cursor(&self) -> CGPoint {
        // SAFETY: an empty event carries the current cursor position.
        unsafe {
            let event = CGEventCreate(std::ptr::null_mut());
            if event.is_null() {
                return CGPoint { x: 0.0, y: 0.0 };
            }
            let point = CGEventGetLocation(event);
            CFRelease(event);
            point
        }
    }

    /// Modifier flags of the modifier keys this plugin currently holds.
    fn modifier_flags(&self) -> u64 {
        self.pressed
            .iter()
            .filter_map(|&c| evdev_to_mac(c).and_then(modifier_flag))
            .filter(|&f| f != FLAG_CAPS)
            .fold(0, |acc, f| acc | f)
    }
}

impl VirtualInput {
    /// Checks the Accessibility permission before a macro starts.
    pub fn prepare(&self, _keyboard: bool, _mouse: bool) -> Result<(), InputError> {
        ensure_post_permission()
    }

    /// Keyboard key or mouse button (evdev code).
    pub fn key(&self, code: u16, down: bool) -> Result<(), InputError> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(button) = mouse_button_number(code) {
            let kind = match (button, down) {
                (0, true) => LEFT_DOWN,
                (0, false) => LEFT_UP,
                (1, true) => RIGHT_DOWN,
                (1, false) => RIGHT_UP,
                (_, true) => OTHER_DOWN,
                (_, false) => OTHER_UP,
            };
            let at = inner.cursor();
            // SAFETY: creates a mouse event at the current cursor position.
            let event = unsafe { CGEventCreateMouseEvent(inner.src(), kind, at, button) };
            if !event.is_null() {
                // SAFETY: valid event; a single click.
                unsafe {
                    CGEventSetIntegerValueField(event, FIELD_CLICK_STATE, 1);
                    CGEventSetIntegerValueField(event, FIELD_BUTTON_NUMBER, i64::from(button));
                }
            }
            post(event)?;
            if down {
                inner.buttons.insert(button);
            } else {
                inner.buttons.remove(&button);
            }
        } else {
            let Some(keycode) = evdev_to_mac(code) else {
                log!("unsupported key on macOS: {code}");
                return Ok(());
            };
            if down {
                inner.pressed.insert(code);
            } else {
                inner.pressed.remove(&code);
            }
            // SAFETY: creates a keyboard event; flags carry the held modifiers so that
            // shortcuts (e.g. Cmd+C) are seen by applications.
            let event = unsafe { CGEventCreateKeyboardEvent(inner.src(), keycode, down) };
            if !event.is_null() {
                unsafe { CGEventSetFlags(event, inner.modifier_flags()) };
            }
            post(event)?;
            return Ok(());
        }
        if down {
            inner.pressed.insert(code);
        } else {
            inner.pressed.remove(&code);
        }
        Ok(())
    }

    pub fn move_by(&self, dx: i32, dy: i32) -> Result<(), InputError> {
        let inner = self.inner.lock().unwrap();
        let from = inner.cursor();
        let to = CGPoint { x: from.x + f64::from(dx), y: from.y + f64::from(dy) };
        // While a button is held, applications expect "dragged" events.
        let (kind, button) = if inner.buttons.contains(&0) {
            (LEFT_DRAGGED, 0)
        } else if inner.buttons.contains(&1) {
            (RIGHT_DRAGGED, 1)
        } else if let Some(&other) = inner.buttons.iter().next() {
            (OTHER_DRAGGED, other)
        } else {
            (MOUSE_MOVED, 0)
        };
        // SAFETY: creates a mouse event at the target position.
        post(unsafe { CGEventCreateMouseEvent(inner.src(), kind, to, button) })
    }

    pub fn wheel(&self, dx: i32, dy: i32) -> Result<(), InputError> {
        if dx == 0 && dy == 0 {
            return Ok(());
        }
        let inner = self.inner.lock().unwrap();
        // Macro format: dy > 0 = up, dx > 0 = right. CoreGraphics: axis 1 positive scrolls
        // up, axis 2 positive scrolls left, hence -dx.
        // SAFETY: creates a two-axis scroll event, in lines.
        post(unsafe {
            CGEventCreateScrollWheelEvent2(inner.src(), SCROLL_UNIT_LINE, 2, dy, dx.saturating_neg(), 0)
        })
    }

    /// Status line for the About tab.
    pub fn status(&self) -> String {
        // SAFETY: plain permission query.
        let allowed = unsafe { CGPreflightPostEventAccess() };
        format!("CGEvent ({})", if allowed { "Accessibility allowed" } else { "Accessibility not allowed" })
    }

    pub fn release_all(&self) {
        let pressed: Vec<u16> = self.inner.lock().unwrap().pressed.iter().copied().collect();
        for code in pressed {
            let _ = self.key(code, false);
        }
    }
}

// ---------------------------------------------------------------- recording

struct TapState {
    core: RecorderCore,
    tap: CFMachPortRef,
    last_pt: Option<(f64, f64)>,
    /// Modifier keys currently down (macOS key codes), to tell press from release.
    modifiers_down: HashSet<u16>,
}

thread_local! {
    static TAP_STATE: RefCell<Option<TapState>> = const { RefCell::new(None) };
}

fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

unsafe extern "C" fn tap_callback(
    _proxy: *mut c_void,
    kind: u32,
    event: CGEventRef,
    _info: *mut c_void,
) -> CGEventRef {
    TAP_STATE.with(|cell| {
        let mut guard = cell.borrow_mut();
        let Some(s) = guard.as_mut() else { return };
        // SAFETY: `event` is the valid event handed to the tap callback.
        unsafe {
            if kind == TAP_DISABLED_BY_TIMEOUT || kind == TAP_DISABLED_BY_USER_INPUT {
                CGEventTapEnable(s.tap, true); // macOS disables slow taps: turn it back on
                return;
            }
            if CGEventGetIntegerValueField(event, FIELD_SOURCE_PID) == i64::from(std::process::id()) {
                return; // our own playback
            }
            let t = now_secs();
            match kind {
                KEY_DOWN | KEY_UP => {
                    let keycode =
                        u16::try_from(CGEventGetIntegerValueField(event, FIELD_KEYCODE)).unwrap_or(u16::MAX);
                    if let Some(code) = mac_to_evdev(keycode) {
                        s.core.feed_key(code, kind == KEY_DOWN, t);
                    }
                }
                FLAGS_CHANGED => {
                    // Modifier keys only report "flags changed": compare with what was down.
                    let keycode =
                        u16::try_from(CGEventGetIntegerValueField(event, FIELD_KEYCODE)).unwrap_or(u16::MAX);
                    let Some(code) = mac_to_evdev(keycode) else { return };
                    if keycode == 0x39 {
                        // Caps Lock reports each toggle once: record it as a full tap.
                        s.core.feed_key(code, true, t);
                        s.core.feed_key(code, false, t);
                    } else if s.modifiers_down.remove(&keycode) {
                        s.core.feed_key(code, false, t);
                    } else if modifier_flag(keycode).is_some_and(|f| CGEventGetFlags(event) & f != 0) {
                        s.modifiers_down.insert(keycode);
                        s.core.feed_key(code, true, t);
                    }
                }
                MOUSE_MOVED | LEFT_DRAGGED | RIGHT_DRAGGED | OTHER_DRAGGED => {
                    let p = CGEventGetLocation(event);
                    if let Some((x, y)) = s.last_pt {
                        s.core.feed_move((p.x - x).round() as i32, (p.y - y).round() as i32, t);
                    }
                    s.last_pt = Some((p.x, p.y));
                }
                LEFT_DOWN | LEFT_UP | RIGHT_DOWN | RIGHT_UP | OTHER_DOWN | OTHER_UP => {
                    let number = CGEventGetIntegerValueField(event, FIELD_BUTTON_NUMBER);
                    let code = match number {
                        0 => BTN_LEFT,
                        1 => BTN_RIGHT,
                        2 => BTN_MIDDLE,
                        3 => BTN_SIDE,
                        4 => BTN_EXTRA,
                        _ => return,
                    };
                    let down = matches!(kind, LEFT_DOWN | RIGHT_DOWN | OTHER_DOWN);
                    s.core.feed_key(code, down, t);
                }
                SCROLL_WHEEL => {
                    let axis = |field| i32::try_from(CGEventGetIntegerValueField(event, field)).unwrap_or(0);
                    let (dy, dx) = (axis(FIELD_SCROLL_AXIS1), axis(FIELD_SCROLL_AXIS2).saturating_neg());
                    s.core.feed_wheel(dx, dy, t);
                }
                _ => {}
            }
        }
    });
    event
}

pub struct RecorderHandle {
    stop: StopRequest,
}

impl RecorderHandle {
    pub fn stop(&self, from_editor: bool) {
        self.stop.request(from_editor);
    }
}

fn mask(kinds: &[u32]) -> u64 {
    kinds.iter().fold(0, |acc, &k| acc | (1u64 << k))
}

/// Starts recording in a background thread running its own run loop.
pub fn start_recording(
    options: RecordOptions,
    on_event: OnEvent,
    on_stop: OnStop,
) -> Result<(RecorderHandle, Vec<String>), InputError> {
    // SAFETY: plain permission queries; the request shows the system prompt once.
    if !unsafe { CGPreflightListenEventAccess() } {
        unsafe { CGRequestListenEventAccess() };
        return Err(InputError::new("permission_input_monitoring", ""));
    }
    let mut kinds = vec![KEY_DOWN, KEY_UP, FLAGS_CHANGED];
    if options.mouse {
        kinds.extend([LEFT_DOWN, LEFT_UP, RIGHT_DOWN, RIGHT_UP, OTHER_DOWN, OTHER_UP, SCROLL_WHEEL]);
        if options.mouse_move {
            kinds.extend([MOUSE_MOVED, LEFT_DRAGGED, RIGHT_DRAGGED, OTHER_DRAGGED]);
        }
    }
    let events_mask = mask(&kinds);
    let stop = StopRequest::default();
    let thread_stop = stop.clone();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    std::thread::spawn(move || {
        // SAFETY: the tap, its run loop source and the thread-local state are created,
        // used and released on this thread only.
        unsafe {
            let tap = CGEventTapCreate(
                SESSION_EVENT_TAP,
                HEAD_INSERT,
                LISTEN_ONLY,
                events_mask,
                tap_callback,
                std::ptr::null_mut(),
            );
            if tap.is_null() {
                let _ = ready_tx.send(Err("CGEventTapCreate failed".to_owned()));
                return;
            }
            let source = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
            let run_loop = CFRunLoopGetCurrent();
            CFRunLoopAddSource(run_loop, source, kCFRunLoopDefaultMode);
            TAP_STATE.with(|s| {
                *s.borrow_mut() = Some(TapState {
                    core: RecorderCore::new(options, on_event, on_stop),
                    tap,
                    last_pt: None,
                    modifiers_down: HashSet::new(),
                })
            });
            CGEventTapEnable(tap, true);
            let _ = ready_tx.send(Ok(()));
            log!("event tap installed");
            while !thread_stop.is_requested() {
                CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.05, false);
            }
            CGEventTapEnable(tap, false);
            CFRunLoopRemoveSource(run_loop, source, kCFRunLoopDefaultMode);
            CFMachPortInvalidate(tap);
            CFRelease(source);
            CFRelease(tap);
            if let Some(mut state) = TAP_STATE.with(|s| s.borrow_mut().take()) {
                state.core.finish(thread_stop.was_from_editor());
            }
        }
    });
    match ready_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(())) => Ok((RecorderHandle { stop }, vec!["macOS event tap".to_owned()])),
        Ok(Err(e)) => Err(InputError::new("record_failed", e)),
        Err(_) => Err(InputError::new("record_failed", "timeout")),
    }
}

// ---------------------------------------------------------------- diagnostics

/// Checks run from the About tab. `ok` is null for information-only lines.
pub fn diagnostics() -> Vec<Value> {
    // SAFETY: plain permission queries, without prompting.
    let (post, listen) = unsafe { (CGPreflightPostEventAccess(), CGPreflightListenEventAccess()) };
    vec![
        json!({"id": "accessibility", "ok": post, "detail": if post { "OK" } else { "not allowed" }}),
        json!({"id": "inputMonitoring", "ok": listen, "detail": if listen { "OK" } else { "not allowed" }}),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_mapping_round_trips_and_is_unique() {
        let mut mac_codes = HashSet::new();
        for &(code, mac) in KEYS {
            assert!(mac_codes.insert(mac), "macOS key code {mac:#x} used twice");
            assert_eq!(mac_to_evdev(evdev_to_mac(code).unwrap()), Some(code));
        }
    }
}
