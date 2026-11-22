//! Linux input backend.
//!
//! - Playback: a virtual keyboard and a virtual mouse created through /dev/uinput
//!   (works on Wayland and X11).
//! - Recording: reads physical keyboards (and mice, when enabled) from /dev/input/event*
//!   (evdev) without grabbing them.
//!
//! Mouse movement is relative (Wayland gives no absolute cursor position): a replayed
//! movement goes through the desktop's pointer acceleration again, so the result may
//! differ slightly from the recording.
//!
//! Needs read/write access to /dev/uinput and read access to /dev/input/event*
//! (usually by being in the "input" group).

use crate::common::{InputError, KEY_MAX, MOUSE_BUTTONS, VIRTUAL_PREFIX, is_keyboard_code, is_mouse_button};
use crate::log;
use crate::recorder::{OnEvent, OnStop, RecordOptions, RecorderCore, StopRequest};
use evdev::raw_stream::RawDevice;
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, InputEvent, KeyCode, RelativeAxisCode};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::os::fd::AsRawFd;
use std::sync::Mutex;
use std::time::{Duration, UNIX_EPOCH};

const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;

const KEYBOARD_NAME: &str = "OpenDeck KeyMacro virtual keyboard";
const MOUSE_NAME: &str = "OpenDeck KeyMacro virtual mouse";

// ---------------------------------------------------------------- playback

#[derive(Default)]
struct Devices {
    keyboard: Option<VirtualDevice>,
    mouse: Option<VirtualDevice>,
    pressed: HashSet<u16>,
}

/// Virtual keyboard + mouse. Separate devices, so the desktop classifies each correctly.
/// Devices are created on first use.
#[derive(Default)]
pub struct VirtualInput {
    devices: Mutex<Devices>,
}

fn uinput_error(e: std::io::Error) -> InputError {
    InputError::new("uinput_failed", e)
}

fn create_keyboard() -> Result<VirtualDevice, InputError> {
    let keys: AttributeSet<KeyCode> =
        (1..=KEY_MAX).filter(|&c| is_keyboard_code(c)).map(KeyCode::new).collect();
    let device = VirtualDevice::builder()
        .map_err(uinput_error)?
        .name(KEYBOARD_NAME)
        .with_keys(&keys)
        .map_err(uinput_error)?
        .build()
        .map_err(uinput_error)?;
    std::thread::sleep(Duration::from_millis(300)); // let the compositor pick it up
    log!("uinput device created: {KEYBOARD_NAME}");
    Ok(device)
}

fn create_mouse() -> Result<VirtualDevice, InputError> {
    let buttons: AttributeSet<KeyCode> = MOUSE_BUTTONS.iter().map(|&c| KeyCode::new(c)).collect();
    let axes: AttributeSet<RelativeAxisCode> =
        [REL_X, REL_Y, REL_WHEEL, REL_HWHEEL].into_iter().map(RelativeAxisCode).collect();
    let device = VirtualDevice::builder()
        .map_err(uinput_error)?
        .name(MOUSE_NAME)
        .with_keys(&buttons)
        .map_err(uinput_error)?
        .with_relative_axes(&axes)
        .map_err(uinput_error)?
        .build()
        .map_err(uinput_error)?;
    std::thread::sleep(Duration::from_millis(300));
    log!("uinput device created: {MOUSE_NAME}");
    Ok(device)
}

impl Devices {
    fn keyboard(&mut self) -> Result<&mut VirtualDevice, InputError> {
        if self.keyboard.is_none() {
            self.keyboard = Some(create_keyboard()?);
        }
        Ok(self.keyboard.as_mut().unwrap())
    }

    fn mouse(&mut self) -> Result<&mut VirtualDevice, InputError> {
        if self.mouse.is_none() {
            self.mouse = Some(create_mouse()?);
        }
        Ok(self.mouse.as_mut().unwrap())
    }
}

impl VirtualInput {
    /// Creates the virtual devices a macro needs before it starts, so that the
    /// creation time (about 300 ms) does not eat into the first delays.
    pub fn prepare(&self, keyboard: bool, mouse: bool) -> Result<(), InputError> {
        let mut d = self.devices.lock().unwrap();
        if keyboard {
            d.keyboard()?;
        }
        if mouse {
            d.mouse()?;
        }
        Ok(())
    }

    /// Keyboard key or mouse button (evdev code).
    pub fn key(&self, code: u16, down: bool) -> Result<(), InputError> {
        let mut d = self.devices.lock().unwrap();
        let event = InputEvent::new(EV_KEY, code, i32::from(down));
        let device = if is_mouse_button(code) { d.mouse()? } else { d.keyboard()? };
        device.emit(&[event]).map_err(uinput_error)?;
        if down {
            d.pressed.insert(code);
        } else {
            d.pressed.remove(&code);
        }
        Ok(())
    }

    pub fn move_by(&self, dx: i32, dy: i32) -> Result<(), InputError> {
        let mut d = self.devices.lock().unwrap();
        let events = [InputEvent::new(EV_REL, REL_X, dx), InputEvent::new(EV_REL, REL_Y, dy)];
        d.mouse()?.emit(&events).map_err(uinput_error)
    }

    pub fn wheel(&self, dx: i32, dy: i32) -> Result<(), InputError> {
        let mut events = Vec::new();
        if dy != 0 {
            events.push(InputEvent::new(EV_REL, REL_WHEEL, dy));
        }
        if dx != 0 {
            events.push(InputEvent::new(EV_REL, REL_HWHEEL, dx));
        }
        if events.is_empty() {
            return Ok(());
        }
        let mut d = self.devices.lock().unwrap();
        d.mouse()?.emit(&events).map_err(uinput_error)
    }

    /// Which virtual devices exist, for the About tab.
    pub fn status(&self) -> String {
        let d = self.devices.lock().unwrap();
        let state = |created: bool| if created { "created" } else { "not created yet" };
        format!("keyboard: {}, mouse: {}", state(d.keyboard.is_some()), state(d.mouse.is_some()))
    }

    /// Releases every key and button this plugin still holds down.
    pub fn release_all(&self) {
        let pressed: Vec<u16> = self.devices.lock().unwrap().pressed.iter().copied().collect();
        for code in pressed {
            let _ = self.key(code, false);
        }
    }
}

// ---------------------------------------------------------------- recording

/// Opens every evdev device that looks like a keyboard (or a mouse, when asked).
fn open_devices(mouse: bool) -> Vec<(String, RawDevice)> {
    let mut found: Vec<(String, RawDevice)> = evdev::raw_stream::enumerate()
        .filter_map(|(_, device)| {
            let name = device.name().unwrap_or("").to_owned();
            if name.starts_with(VIRTUAL_PREFIX) {
                return None;
            }
            let keys = device.supported_keys();
            let has_key = |c: KeyCode| keys.is_some_and(|k| k.contains(c));
            let rels = device.supported_relative_axes();
            let has_rel = |c: u16| rels.is_some_and(|r| r.contains(RelativeAxisCode(c)));
            // A keyboard has at least a few letter keys; a mouse has X/Y axes and a left button.
            let is_keyboard = has_key(KeyCode::KEY_A) && has_key(KeyCode::KEY_SPACE) && has_key(KeyCode::KEY_ENTER);
            let is_mouse = has_rel(REL_X) && has_rel(REL_Y) && has_key(KeyCode::BTN_LEFT);
            (is_keyboard || (mouse && is_mouse)).then_some((name, device))
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

fn set_nonblocking(device: &RawDevice) {
    let fd = device.as_raw_fd();
    // SAFETY: plain fcntl calls on a file descriptor owned by `device`.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

pub struct RecorderHandle {
    stop: StopRequest,
}

impl RecorderHandle {
    pub fn stop(&self, from_editor: bool) {
        self.stop.request(from_editor);
    }
}

/// Starts recording in a background thread. Returns the names of the devices read.
pub fn start_recording(
    options: RecordOptions,
    on_event: OnEvent,
    on_stop: OnStop,
) -> Result<(RecorderHandle, Vec<String>), InputError> {
    let devices = open_devices(options.mouse);
    if devices.is_empty() {
        return Err(InputError::new("no_keyboards", "/dev/input/event*"));
    }
    let names: Vec<String> = devices.iter().map(|(n, _)| n.clone()).collect();
    log!("recording on {names:?}");
    let stop = StopRequest::default();
    let thread_stop = stop.clone();
    std::thread::spawn(move || record_loop(devices, RecorderCore::new(options, on_event, on_stop), thread_stop));
    Ok((RecorderHandle { stop }, names))
}

fn record_loop(mut devices: Vec<(String, RawDevice)>, mut core: RecorderCore, stop: StopRequest) {
    for (_, device) in &devices {
        set_nonblocking(device);
    }
    while !stop.is_requested() && !devices.is_empty() {
        let mut fds: Vec<libc::pollfd> = devices
            .iter()
            .map(|(_, d)| libc::pollfd { fd: d.as_raw_fd(), events: libc::POLLIN, revents: 0 })
            .collect();
        // SAFETY: `fds` is a valid array of pollfd for the duration of the call.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 50) };
        if ready <= 0 {
            continue;
        }
        let mut unplugged = Vec::new();
        for (index, pfd) in fds.iter().enumerate() {
            if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                unplugged.push(index);
                continue;
            }
            if pfd.revents & libc::POLLIN == 0 {
                continue;
            }
            let events: Vec<InputEvent> = match devices[index].1.fetch_events() {
                Ok(iter) => iter.collect(),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(_) => {
                    unplugged.push(index);
                    continue;
                }
            };
            for ev in events {
                let t = ev.timestamp().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
                match (ev.event_type().0, ev.code(), ev.value()) {
                    (EV_KEY, _, 2) => {} // auto-repeat
                    (EV_KEY, code, value) => core.feed_key(code, value == 1, t),
                    // High-resolution wheel events are ignored: REL_WHEEL / REL_HWHEEL carry the notches.
                    (EV_REL, REL_X, v) => core.feed_move(v, 0, t),
                    (EV_REL, REL_Y, v) => core.feed_move(0, v, t),
                    (EV_REL, REL_WHEEL, v) => core.feed_wheel(0, v, t),
                    (EV_REL, REL_HWHEEL, v) => core.feed_wheel(v, 0, t),
                    _ => {}
                }
            }
        }
        for index in unplugged.into_iter().rev() {
            devices.remove(index);
        }
    }
    core.finish(stop.from_editor());
}

// ---------------------------------------------------------------- diagnostics

fn check(id: &str, ok: Option<bool>, detail: impl Into<String>) -> Value {
    json!({"id": id, "ok": ok, "detail": detail.into()})
}

/// Checks run from the About tab. `ok` is null for information-only lines.
pub fn diagnostics() -> Vec<Value> {
    let mut checks = Vec::new();

    let uinput = std::fs::OpenOptions::new().read(true).write(true).open("/dev/uinput");
    checks.push(match uinput {
        Ok(_) => check("uinput", Some(true), "/dev/uinput: read/write OK"),
        Err(e) => check("uinput", Some(false), format!("/dev/uinput: {e}")),
    });

    let (mut total, mut denied) = (0, 0);
    if let Ok(entries) = std::fs::read_dir("/dev/input") {
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("event") {
                continue;
            }
            total += 1;
            if let Err(e) = std::fs::File::open(entry.path()) {
                if e.kind() == std::io::ErrorKind::PermissionDenied {
                    denied += 1;
                }
            }
        }
    }
    checks.push(check(
        "eventAccess",
        Some(total > 0 && denied == 0),
        format!("/dev/input/event*: {} of {total} readable", total - denied),
    ));

    let keyboards: Vec<String> = open_devices(false).into_iter().map(|(n, _)| n).collect();
    let mice: Vec<String> =
        open_devices(true).into_iter().map(|(n, _)| n).filter(|n| !keyboards.contains(n)).collect();
    let list = |v: &[String]| if v.is_empty() { "none".to_owned() } else { v.join(", ") };
    checks.push(check("keyboards", Some(!keyboards.is_empty()), list(&keyboards)));
    checks.push(check("mice", None, list(&mice)));

    let flatpak = std::path::Path::new("/.flatpak-info").exists();
    let detail = if flatpak {
        format!("yes ({})", std::env::var("FLATPAK_ID").unwrap_or_default())
    } else {
        "no".to_owned()
    };
    checks.push(check("flatpak", None, detail));
    checks
}
