//! Plugin logic: reacts to OpenDeck events, plays and records macros, updates keys.
//!
//! This module is synchronous: playback and recording run on plain threads. It talks to
//! OpenDeck only through `Outgoing` messages, which `host.rs` delivers in order with the
//! `openaction` crate; `host.rs` also calls the `on_*` entry points below for each event.
//!
//! User-facing text is not produced here: errors are sent to the property inspector as
//! codes, and the property inspector translates them.

use crate::common::{
    DEBUG, InputError, MacroEvent, Step, clear_log, log_path, open_folder, parse_steps, plugin_dir,
    read_log_tail,
};
use crate::input::{RecorderHandle, VirtualInput, diagnostics, requirements, start_recording};
use crate::recorder::RecordOptions;
use crate::{debug_log, log};
use base64::Engine;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

pub const ACTION_UUID: &str = "com.mrbretzel.keymacro.macro";

/// A message for OpenDeck. Produced by any thread, delivered in order by `host::deliver`.
#[derive(Debug)]
pub enum Outgoing {
    SetState {
        context: String,
        state: u8,
    },
    /// `image: None` resets the state to the manifest image.
    SetImage {
        context: String,
        image: Option<String>,
        state: u8,
    },
    /// Dial touch-strip layout values.
    SetFeedback {
        context: String,
        payload: Value,
    },
    ShowAlert {
        context: String,
    },
    /// Brief check mark on the key (recording saved).
    ShowOk {
        context: String,
    },
    SetSettings {
        context: String,
        settings: Value,
    },
    /// To the property inspector of one key.
    ToEditor {
        context: String,
        payload: Value,
    },
    /// To the property inspector of every visible key of this plugin.
    ToAllEditors {
        payload: Value,
    },
    SetGlobalSettings {
        settings: Value,
    },
    /// Opens a web page in the user's browser (About tab links).
    OpenUrl {
        url: String,
    },
    /// A line for OpenDeck's own log.
    LogMessage {
        message: String,
    },
}

/// The only pages the editor may ask to open: never a URL coming from the editor itself.
const REPOSITORY_URL: &str = "https://github.com/MrBretze/Keyboard-Macros";

fn setup_section() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macos",
        "windows" => "windows",
        _ => "linux",
    }
}

fn link_url(target: &str) -> Option<String> {
    Some(match target {
        "repo" => REPOSITORY_URL.to_owned(),
        "issues" => format!("{REPOSITORY_URL}/issues/new"),
        "license" => format!("{REPOSITORY_URL}/blob/main/LICENSE"),
        // Setup guide, at the section of the system the plugin runs on.
        "setup" => format!("{REPOSITORY_URL}/blob/main/docs/SETUP.md#{}", setup_section()),
        _ => return None,
    })
}

const STATE_IDLE: u8 = 0;
const STATE_PLAYING: u8 = 1;
const STATE_RECORDING: u8 = 2;

/// Macro slots in the action settings. Keys only use SLOT_PRESS; dials (Stream Deck+
/// encoders) also have one macro per rotation direction.
const SLOT_PRESS: &str = "events";
const SLOT_CW: &str = "eventsCW";
const SLOT_CCW: &str = "eventsCCW";
/// Cap on queued dial notches, so a fast spin cannot run for minutes.
const MAX_PENDING_TICKS: u32 = 50;

/// Upper bound for any single wait (event delay, pause between repeats). Settings are
/// user-editable: without it, a huge value would overflow the duration arithmetic.
const MAX_DELAY: Duration = Duration::from_secs(3600);

/// Converts a delay in milliseconds (scaled by the playback speed) to a bounded duration.
fn bounded_delay(ms: f64, speed: f64) -> Duration {
    Duration::try_from_secs_f64(ms.max(0.0) / 1000.0 / speed).map_or(MAX_DELAY, |d| d.min(MAX_DELAY))
}

/// Time span covered by a recorded mouse move (the recorder's merge window): playback
/// spreads the move's steps over it.
const MOVE_SPAN: Duration = Duration::from_millis(12);

/// Splits a total into `parts` integer steps whose sum is exactly `total`.
fn split_steps(total: i32, parts: u32, index: u32) -> i32 {
    let total = i64::from(total);
    let parts = i64::from(parts.max(1));
    let index = i64::from(index);
    // Cumulative rounding: step k = round(total*(k+1)/parts) - round(total*k/parts).
    let at = |k: i64| (total * k).div_euclid(parts);
    i32::try_from(at(index + 1) - at(index)).unwrap_or(0)
}

/// How much of the log the About tab shows.
const LOG_TAIL_BYTES: u64 = 48 * 1024;

const STATE_ICONS: [&str; 3] =
    ["icons/state_idle@2x.png", "icons/state_play@2x.png", "icons/state_rec@2x.png"];

/// Interruptible wait: playback sleeps on it so that a stop request is immediate.
struct StopFlag {
    stopped: Mutex<bool>,
    cv: Condvar,
}

impl StopFlag {
    fn new() -> Arc<Self> {
        Arc::new(Self { stopped: Mutex::new(false), cv: Condvar::new() })
    }
    fn set(&self) {
        *self.stopped.lock().unwrap() = true;
        self.cv.notify_all();
    }
    fn is_set(&self) -> bool {
        *self.stopped.lock().unwrap()
    }
    /// Waits up to `duration`; returns true if a stop was requested.
    fn wait(&self, duration: Duration) -> bool {
        let guard = self.stopped.lock().unwrap();
        let (guard, _) = self.cv.wait_timeout_while(guard, duration, |stopped| !*stopped).unwrap();
        *guard
    }
}

#[derive(Clone, Copy, PartialEq)]
enum PlayerKind {
    Press,
    Turn,
}

struct Player {
    stop: Arc<StopFlag>,
    kind: PlayerKind,
}

struct PendingTurns {
    slot: &'static str,
    notches: u32,
}

#[derive(Default)]
struct State {
    settings: HashMap<String, Value>,
    controllers: HashMap<String, String>,
    states: HashMap<String, u8>,
    /// Contexts currently showing a custom image.
    custom_shown: HashSet<String>,
    players: HashMap<String, Player>,
    turns: HashMap<String, PendingTurns>,
    recorder: Option<RecorderHandle>,
    recording_ctx: Option<String>,
    /// Plugin-wide settings shared by every key: {"debug": bool, "language": "auto" | "en" | ...}.
    globals: Value,
    /// Connected decks: device ID -> name.
    decks: BTreeMap<String, String>,
    /// Deck of each key, to stop its playback if the deck is unplugged.
    deck_of: HashMap<String, String>,
}

pub struct Plugin {
    out: UnboundedSender<Outgoing>,
    input: VirtualInput,
    state: Mutex<State>,
    default_icons: Vec<Option<String>>,
}

fn load_data_url(relative: &str) -> Option<String> {
    let bytes = std::fs::read(plugin_dir().join(relative)).ok()?;
    Some(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
}

fn number(s: &Value, key: &str) -> Option<f64> {
    s.get(key).and_then(Value::as_f64)
}

fn has_events(s: Option<&Value>, slot: &str) -> bool {
    s.and_then(|s| s.get(slot)).is_some_and(|v| !parse_steps(v).is_empty())
}

impl Plugin {
    pub fn new(out: UnboundedSender<Outgoing>) -> Arc<Self> {
        Arc::new(Self {
            out,
            input: VirtualInput::default(),
            state: Mutex::new(State::default()),
            default_icons: STATE_ICONS.iter().map(|p| load_data_url(p)).collect(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    // ------------------------------------------------------------ messages to OpenDeck

    fn send(&self, message: Outgoing) {
        // Fails only once the connection is gone, when there is nobody left to tell.
        let _ = self.out.send(message);
    }

    fn to_pi(&self, context: &str, payload: Value) {
        self.send(Outgoing::ToEditor { context: context.to_owned(), payload });
    }

    fn error(&self, context: &str, code: &str, detail: &str) {
        self.to_pi(context, json!({"type": "error", "code": code, "detail": detail}));
    }

    fn alert(&self, context: &str) {
        self.send(Outgoing::ShowAlert { context: context.to_owned() });
    }

    fn set_state(&self, context: &str, state: u8) {
        let mut st = self.lock();
        self.set_state_locked(&mut st, context, state);
    }

    /// Same as `set_state`, for callers that already hold the state lock.
    fn set_state_locked(&self, st: &mut State, context: &str, state: u8) {
        st.states.insert(context.to_owned(), state);
        self.send(Outgoing::SetState { context: context.to_owned(), state });
        self.apply_image(st, context);
    }

    /// Shows the custom image rendered by the property inspector for the current state,
    /// or resets to the manifest image when there is none.
    fn apply_image(&self, st: &mut State, context: &str) {
        let state = st.states.get(context).copied().unwrap_or(STATE_IDLE);
        let settings = st.settings.get(context);
        let image = settings
            .and_then(|s| s.get("images"))
            .and_then(|i| i.get(state.to_string()))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        if st.controllers.get(context).map(String::as_str) == Some("Encoder") {
            // The dial's touch-strip segment uses a layout: its icon is set through feedback.
            if let Some(icon) = image.clone().or_else(|| self.default_icons[state as usize].clone()) {
                self.send(Outgoing::SetFeedback {
                    context: context.to_owned(),
                    payload: json!({"icon": icon}),
                });
            }
        }
        let managed = settings.is_some_and(|s| s.get("images").is_some());
        if image.is_none() && !st.custom_shown.contains(context) && !managed {
            // The appearance editor was never used: leave any image set in OpenDeck's own UI alone.
            return;
        }
        if image.is_some() {
            st.custom_shown.insert(context.to_owned());
        } else {
            st.custom_shown.remove(context);
        }
        self.send(Outgoing::SetImage { context: context.to_owned(), image, state });
    }

    /// Changes one plugin-wide setting and saves them all. OpenDeck replaces the whole
    /// global settings object on save, so the other values must be sent along.
    fn set_global(&self, key: &str, value: Value) {
        let globals = {
            let mut st = self.lock();
            if !st.globals.is_object() {
                st.globals = json!({});
            }
            st.globals[key] = value;
            st.globals.clone()
        };
        self.send(Outgoing::SetGlobalSettings { settings: globals });
    }

    /// Sends a message to the editor of every visible key and dial of this plugin.
    /// OpenDeck keeps the editors it has already shown, so plugin-wide changes (the
    /// language) must reach them too, not only the editor where the change was made.
    /// Editors of keys that are not visible catch up when they are shown again (they ask
    /// for `status` whenever they become visible).
    fn broadcast_to_editors(&self, payload: Value) {
        self.send(Outgoing::ToAllEditors { payload });
    }

    fn settings_of(&self, context: &str) -> Value {
        self.lock().settings.get(context).cloned().unwrap_or_else(|| json!({}))
    }

    // ------------------------------------------------------------ playback

    /// Plays the steps once. Returns Ok(false) if interrupted.
    fn play_pass(&self, s: &Value, steps: &[Step], stop: &StopFlag) -> Result<bool, InputError> {
        let speed = number(s, "speed").filter(|v| *v > 0.0).unwrap_or(100.0) / 100.0;
        let speed = speed.max(0.01);
        let fixed = number(s, "fixedDelay").unwrap_or(0.0).max(0.0);
        let use_fixed = s.get("useFixedDelay").and_then(Value::as_bool).unwrap_or(false);
        let needs_mouse = steps.iter().any(|st| match st.event {
            MacroEvent::Key { code, .. } => crate::common::is_mouse_button(code),
            _ => true,
        });
        let needs_keyboard = steps.iter().any(
            |st| matches!(st.event, MacroEvent::Key { code, .. } if !crate::common::is_mouse_button(code)),
        );
        self.input.prepare(needs_keyboard, needs_mouse)?;
        // Absolute deadlines, so that the time spent sending input does not accumulate.
        debug_log!("play {} steps (speed {:.0}%, fixed delay: {use_fixed})", steps.len(), speed * 100.0);
        let mut deadline = Instant::now();
        for (i, step) in steps.iter().enumerate() {
            // The fixed delay replaces key / button / wheel delays; mouse movements keep
            // theirs, otherwise a recorded gesture would become very slow.
            let is_move = matches!(step.event, MacroEvent::Move { .. });
            let delay = if use_fixed && i > 0 && !is_move { fixed } else { step.delay_ms };
            let gap = bounded_delay(delay, speed);
            deadline += gap;
            match step.event {
                MacroEvent::Move { dx, dy, reports } if reports > 1 => {
                    // A recorded move merges `reports` small device steps made during the
                    // MOVE_SPAN before its time. Replay them the same way, ending at the
                    // event's time: the desktop then sees the original speed and applies
                    // the same pointer acceleration, so the cursor lands where it did.
                    let span = MOVE_SPAN.mul_f64(1.0 / speed).min(gap);
                    let start = deadline.checked_sub(span).unwrap_or(deadline);
                    for k in 0..reports {
                        let at = start + span.mul_f64(f64::from(k + 1) / f64::from(reports));
                        if !self.wait_until(at, stop) {
                            return Ok(false);
                        }
                        let (sx, sy) = (split_steps(dx, reports, k), split_steps(dy, reports, k));
                        if sx != 0 || sy != 0 {
                            self.input.move_by(sx, sy)?;
                        }
                    }
                }
                event => {
                    if !self.wait_until(deadline, stop) {
                        return Ok(false);
                    }
                    match event {
                        MacroEvent::Key { code, down } => self.input.key(code, down)?,
                        MacroEvent::Move { dx, dy, .. } => self.input.move_by(dx, dy)?,
                        MacroEvent::Wheel { dx, dy } => self.input.wheel(dx, dy)?,
                    }
                }
            }
        }
        Ok(!stop.is_set())
    }

    /// Waits until `at`. Returns false if a stop was requested meanwhile.
    fn wait_until(&self, at: Instant, stop: &StopFlag) -> bool {
        let wait = at.saturating_duration_since(Instant::now());
        wait.is_zero() || !stop.wait(wait)
    }

    /// Registers a player (the caller holds the state lock) and starts its thread.
    fn start_player(self: &Arc<Self>, st: &mut State, context: &str, kind: PlayerKind) {
        let stop = StopFlag::new();
        st.players.insert(context.to_owned(), Player { stop: stop.clone(), kind });
        let this = self.clone();
        let context = context.to_owned();
        std::thread::spawn(move || this.player_thread(&context, kind, &stop));
    }

    fn player_thread(&self, context: &str, kind: PlayerKind, stop: &StopFlag) {
        self.set_state(context, STATE_PLAYING);
        self.to_pi(context, json!({"type": "playing", "playing": true}));
        let result = match kind {
            PlayerKind::Press => self.press_loop(context, stop),
            PlayerKind::Turn => self.turn_loop(context, stop),
        };
        if let Err(e) = result {
            log!("play error: {e}");
            self.error(context, e.code, &e.detail);
            self.alert(context);
        }
        self.input.release_all();
        // Unregister and reset the key under one lock: a new playback can only start once
        // this one is gone, so its "playing" state cannot be overwritten by this "idle".
        let mut st = self.lock();
        st.players.remove(context);
        st.turns.remove(context);
        let recording = st.recording_ctx.as_deref() == Some(context);
        self.set_state_locked(&mut st, context, if recording { STATE_RECORDING } else { STATE_IDLE });
        self.to_pi(context, json!({"type": "playing", "playing": false}));
    }

    fn stop_playing(&self, context: &str) {
        if let Some(player) = self.lock().players.get(context) {
            player.stop.set();
        }
    }

    /// Key press, dial press or touch tap: plays the main macro, or stops any playback.
    fn play(self: &Arc<Self>, context: &str) {
        let mut st = self.lock();
        if let Some(player) = st.players.get(context) {
            player.stop.set();
            return;
        }
        if !has_events(st.settings.get(context), SLOT_PRESS) {
            drop(st);
            self.alert(context);
            self.error(context, "empty_macro", "");
            return;
        }
        self.start_player(&mut st, context, PlayerKind::Press);
    }

    fn press_loop(&self, context: &str, stop: &StopFlag) -> Result<(), InputError> {
        let s = self.settings_of(context);
        let steps = parse_steps(&s[SLOT_PRESS]);
        let repeat = number(&s, "repeat").map(|v| v.max(0.0) as u64).unwrap_or(1); // 0 = loop
        let repeat_delay = bounded_delay(number(&s, "repeatDelay").unwrap_or(0.0), 1.0);
        let mut n = 0;
        while !stop.is_set() && (repeat == 0 || n < repeat) {
            if n > 0 && stop.wait(repeat_delay) {
                break;
            }
            if !self.play_pass(&s, &steps, stop)? {
                break;
            }
            n += 1;
        }
        Ok(())
    }

    /// Dial rotation: queues one run of the direction's macro per notch.
    fn rotate(self: &Arc<Self>, context: &str, ticks: i64) {
        if ticks == 0 {
            return;
        }
        let slot = if ticks > 0 { SLOT_CW } else { SLOT_CCW };
        let mut st = self.lock();
        if !has_events(st.settings.get(context), slot) {
            return;
        }
        let player_kind = st.players.get(context).map(|p| p.kind);
        if player_kind == Some(PlayerKind::Press) {
            return; // the main macro is running: ignore the dial until it ends
        }
        let pending = st.turns.entry(context.to_owned()).or_insert(PendingTurns { slot, notches: 0 });
        if pending.slot != slot {
            *pending = PendingTurns { slot, notches: 0 }; // changing direction drops the other queue
        }
        let added = u32::try_from(ticks.unsigned_abs()).unwrap_or(u32::MAX);
        pending.notches = pending.notches.saturating_add(added).min(MAX_PENDING_TICKS);
        if player_kind.is_none() {
            self.start_player(&mut st, context, PlayerKind::Turn);
        }
    }

    fn turn_loop(&self, context: &str, stop: &StopFlag) -> Result<(), InputError> {
        while !stop.is_set() {
            let slot = {
                let mut st = self.lock();
                match st.turns.get_mut(context) {
                    Some(p) if p.notches > 0 => {
                        p.notches -= 1;
                        p.slot
                    }
                    _ => return Ok(()),
                }
            };
            let s = self.settings_of(context);
            if !self.play_pass(&s, &parse_steps(&s[slot]), stop)? {
                return Ok(());
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ recording

    fn start_recording(
        self: &Arc<Self>,
        context: &str,
        append: bool,
        slot: &'static str,
        options: RecordOptions,
    ) {
        let mut st = self.lock();
        if st.recorder.is_some() {
            drop(st);
            self.error(context, "already_recording", "");
            return;
        }
        // A macro playing on this key would fight with the recording over the key state.
        if let Some(player) = st.players.get(context) {
            player.stop.set();
        }
        let (this, ctx) = (self.clone(), context.to_owned());
        let on_event =
            Box::new(move |ev: &Value| this.to_pi(&ctx, json!({"type": "recordEvent", "event": ev})));
        let (this, ctx) = (self.clone(), context.to_owned());
        let on_stop = Box::new(move |events: Vec<Value>| this.finish_recording(&ctx, append, slot, events));
        // The state lock is held until the handle is stored, so finish_recording (another
        // thread) always runs after this.
        match start_recording(options, on_event, on_stop) {
            Ok((handle, devices)) => {
                st.recorder = Some(handle);
                st.recording_ctx = Some(context.to_owned());
                drop(st);
                self.set_state(context, STATE_RECORDING);
                self.to_pi(context, json!({"type": "recording", "devices": devices, "slot": slot}));
            }
            Err(e) => {
                drop(st);
                log!("record error: {e}");
                self.error(context, e.code, &e.detail);
            }
        }
    }

    fn finish_recording(&self, context: &str, append: bool, slot: &str, mut events: Vec<Value>) {
        let settings = {
            let mut st = self.lock();
            st.recorder = None;
            st.recording_ctx = None;
            let mut s = st.settings.get(context).cloned().unwrap_or_else(|| json!({}));
            if !s.is_object() {
                s = json!({});
            }
            let existing = s.get(slot).and_then(Value::as_array).cloned().unwrap_or_default();
            if append && !existing.is_empty() && !events.is_empty() {
                // Small gap between the existing macro and the appended part.
                events[0]["delay"] = json!(200);
                events = existing.into_iter().chain(events).collect();
            }
            s[slot] = json!(events);
            st.settings.insert(context.to_owned(), s.clone());
            s
        };
        self.send(Outgoing::SetSettings { context: context.to_owned(), settings });
        self.set_state(context, STATE_IDLE);
        if !events.is_empty() {
            self.send(Outgoing::ShowOk { context: context.to_owned() });
        }
        log!("recorded {} events into {slot}", events.len());
        self.to_pi(context, json!({"type": "recorded", "events": events, "slot": slot}));
    }

    pub fn stop_recording(&self, from_editor: bool) {
        if let Some(recorder) = &self.lock().recorder {
            recorder.stop(from_editor); // drops the click on the editor's Stop button
        }
    }

    fn is_recording(&self, context: &str) -> bool {
        self.lock().recording_ctx.as_deref() == Some(context)
    }

    // ------------------------------------------------------------ events from OpenDeck
    // Called by `host.rs`, one event at a time, in the order OpenDeck sends them.

    fn store_settings(&self, context: &str, settings: &Value) {
        self.lock().settings.insert(context.to_owned(), settings.clone());
    }

    /// A key or dial of this plugin becomes visible (`willAppear`).
    pub fn on_appear(&self, context: &str, device: &str, controller: &str, settings: &Value) {
        let mut st = self.lock();
        st.settings.insert(context.to_owned(), settings.clone());
        st.controllers.insert(context.to_owned(), controller.to_owned());
        st.deck_of.insert(context.to_owned(), device.to_owned());
        let busy = st.players.contains_key(context) || st.recording_ctx.as_deref() == Some(context);
        if !busy {
            st.states.insert(context.to_owned(), STATE_IDLE);
        }
        self.apply_image(&mut st, context);
    }

    /// The key is no longer visible (`willDisappear`): stop its playback.
    pub fn on_disappear(&self, context: &str) {
        self.stop_playing(context);
    }

    /// New settings saved by the property inspector (`didReceiveSettings`).
    pub fn on_settings(&self, context: &str, settings: &Value) {
        let mut st = self.lock();
        st.settings.insert(context.to_owned(), settings.clone());
        self.apply_image(&mut st, context); // the appearance may have changed
    }

    /// Key press, dial press or touch tap.
    pub fn on_press(self: &Arc<Self>, context: &str, settings: &Value) {
        self.store_settings(context, settings);
        if self.is_recording(context) {
            self.stop_recording(false); // pressing the deck key / dial ends the recording
        } else {
            self.play(context);
        }
    }

    /// Dial rotation, `ticks` > 0 to the right.
    pub fn on_rotate(self: &Arc<Self>, context: &str, settings: &Value, ticks: i16) {
        self.store_settings(context, settings);
        if !self.is_recording(context) {
            self.rotate(context, i64::from(ticks));
        }
    }

    /// The property inspector of a key is shown (`propertyInspectorDidAppear`): send it the
    /// current state, so an editor OpenDeck kept from earlier is up to date (language...).
    pub fn on_editor_appear(&self, context: &str) {
        self.send_status(context);
    }

    /// Decks connected at startup (`openaction::get_connected_devices`).
    pub fn set_decks(&self, decks: BTreeMap<String, String>) {
        self.lock().decks = decks;
    }

    pub fn on_deck_connected(&self, device: &str, name: &str) {
        self.lock().decks.insert(device.to_owned(), name.to_owned());
    }

    /// A deck was unplugged: stop the macros started from it. Otherwise a looping macro
    /// could not be stopped any more, since pressing its key is how it stops.
    pub fn on_deck_disconnected(&self, device: &str) {
        let mut st = self.lock();
        let mut stopped = 0;
        for (context, player) in &st.players {
            if st.deck_of.get(context).map(String::as_str) == Some(device) {
                player.stop.set();
                stopped += 1;
            }
        }
        st.decks.remove(device);
        drop(st);
        log!("deck {device} disconnected: {stopped} running macro(s) stopped");
    }

    /// Plugin-wide settings (`didReceiveGlobalSettings`).
    pub fn on_global_settings(&self, settings: &Value) {
        let globals = if settings.is_object() { settings.clone() } else { json!({}) };
        DEBUG.store(globals.get("debug").and_then(Value::as_bool).unwrap_or(false), Ordering::Relaxed);
        self.lock().globals = globals;
    }

    /// Commands sent by the property inspector (`sendToPlugin`).
    pub fn on_command(self: &Arc<Self>, context: &str, payload: &Value) {
        let slot = match payload.get("slot").and_then(Value::as_str) {
            Some(SLOT_CW) => SLOT_CW,
            Some(SLOT_CCW) => SLOT_CCW,
            _ => SLOT_PRESS,
        };
        let flag = |key: &str| payload.get(key).and_then(Value::as_bool);
        match payload.get("cmd").and_then(Value::as_str).unwrap_or("") {
            "record" => {
                let options = RecordOptions {
                    mouse: flag("mouse").unwrap_or(false),
                    mouse_move: flag("mouseMove") != Some(false),
                };
                self.start_recording(context, flag("append").unwrap_or(false), slot, options);
            }
            "stopRecord" => {
                if self.is_recording(context) {
                    self.stop_recording(true);
                } else {
                    // Nothing to stop (already stopped, or another key records): let the
                    // editor resynchronise instead of waiting for a "recorded" that won't come.
                    self.send_status(context);
                }
            }
            "play" => {
                if let Some(settings) = payload.get("settings") {
                    self.lock().settings.insert(context.to_owned(), settings.clone());
                }
                match slot {
                    SLOT_CW => self.rotate(context, 1),
                    SLOT_CCW => self.rotate(context, -1),
                    _ => self.play(context),
                }
            }
            "stopPlay" => self.stop_playing(context),
            // ---- About tab
            "about" => {
                // Read under one lock, before building the reply: two `self.lock()` calls in
                // the same statement would deadlock (temporaries live to the end of it).
                let (controller, decks) = {
                    let st = self.lock();
                    let controller =
                        st.controllers.get(context).cloned().unwrap_or_else(|| "Keypad".to_owned());
                    (controller, st.decks.values().cloned().collect::<Vec<_>>())
                };
                let about = json!({
                    "type": "about",
                    "version": env!("CARGO_PKG_VERSION"),
                    "os": std::env::consts::OS,
                    "arch": std::env::consts::ARCH,
                    "pluginDir": plugin_dir().to_string_lossy(),
                    "logPath": log_path().to_string_lossy(),
                    "debug": DEBUG.load(Ordering::Relaxed),
                    "devices": self.input.status(),
                    "controller": controller,
                    "decks": decks,
                    "context": context,
                });
                self.to_pi(context, about);
            }
            "diagnostics" => {
                let checks = diagnostics();
                self.to_pi(context, json!({"type": "diagnostics", "checks": checks}));
            }
            "readLog" => {
                let text = read_log_tail(LOG_TAIL_BYTES);
                self.to_pi(context, json!({"type": "log", "text": text}));
            }
            "clearLog" => {
                if let Err(e) = clear_log() {
                    self.error(context, "log_failed", &e.to_string());
                }
                log!("log cleared");
                self.to_pi(context, json!({"type": "log", "text": read_log_tail(LOG_TAIL_BYTES)}));
            }
            "openUrl" => {
                if let Some(url) = payload.get("target").and_then(Value::as_str).and_then(link_url) {
                    self.send(Outgoing::OpenUrl { url });
                }
            }
            "openLogFolder" => {
                if let Err(e) = open_folder(&plugin_dir()) {
                    self.error(context, "open_failed", &e.to_string());
                }
            }
            "setDebug" => {
                let enabled = flag("enabled").unwrap_or(false);
                DEBUG.store(enabled, Ordering::Relaxed);
                log!("verbose logging {}", if enabled { "enabled" } else { "disabled" });
                self.set_global("debug", json!(enabled));
            }
            "setLanguage" => {
                // "auto" or a short language code ("en", "fr", "pt-BR"...): anything else is ignored.
                let language = payload.get("language").and_then(Value::as_str).unwrap_or("");
                let valid = language == "auto"
                    || (2..=8).contains(&language.len())
                        && language.chars().all(|c| c.is_ascii_alphabetic() || c == '-' || c == '_');
                if valid {
                    self.set_global("language", json!(language));
                    self.broadcast_to_editors(json!({"type": "language", "language": language}));
                }
            }
            "status" => self.send_status(context),
            _ => {}
        }
    }

    /// Sends the key's state to its property inspector: recording / playing, controller
    /// type, OS (for key names) and the plugin-wide language.
    fn send_status(&self, context: &str) {
        let st = self.lock();
        let status = json!({
            "type": "status",
            "recording": st.recording_ctx.as_deref() == Some(context),
            "playing": st.players.contains_key(context),
            "controller": st.controllers.get(context).map(String::as_str).unwrap_or("Keypad"),
            "platform": std::env::consts::OS,
            "language": st.globals.get("language").and_then(Value::as_str).unwrap_or("auto"),
        });
        drop(st);
        let mut status = status;
        // Missing setup (permissions, module): the editor shows what to do and the guide link.
        status["problems"] = json!(requirements());
        self.to_pi(context, status);
    }

    /// At startup: report missing setup in OpenDeck's log, before anyone presses a key.
    pub fn log_requirements(&self) {
        let problems = requirements();
        if !problems.is_empty() {
            log!("setup needed ({}): see {}", problems.join(", "), link_url("setup").unwrap_or_default());
        }
    }

    /// Called when the connection ends: stop everything and release held keys.
    pub fn shutdown(&self) {
        self.stop_recording(false);
        for player in self.lock().players.values() {
            player.stop.set();
        }
        self.input.release_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_from_settings_are_bounded() {
        assert_eq!(bounded_delay(120.0, 1.0), Duration::from_millis(120));
        assert_eq!(bounded_delay(120.0, 2.0), Duration::from_millis(60)); // speed 200 %
        assert_eq!(bounded_delay(-5.0, 1.0), Duration::ZERO);
        assert_eq!(bounded_delay(1e300, 0.01), MAX_DELAY); // would overflow without the bound
        assert_eq!(bounded_delay(f64::INFINITY, 1.0), MAX_DELAY);
    }

    #[test]
    fn split_moves_add_up_exactly() {
        for (total, parts) in [(25, 12), (-7, 3), (1, 12), (0, 5), (100, 1), (-1, 7), (i32::MAX, 2)] {
            let steps: Vec<i32> = (0..parts).map(|k| split_steps(total, parts, k)).collect();
            assert_eq!(steps.iter().map(|&v| i64::from(v)).sum::<i64>(), i64::from(total), "{total}/{parts}");
            let (min, max) = (steps.iter().min().unwrap(), steps.iter().max().unwrap());
            assert!(max - min <= 1, "steps should be even: {steps:?}");
        }
    }

    #[test]
    fn only_known_links_can_be_opened() {
        assert_eq!(link_url("repo").as_deref(), Some(REPOSITORY_URL));
        assert!(link_url("issues").unwrap().starts_with(REPOSITORY_URL));
        assert!(link_url("license").unwrap().starts_with(REPOSITORY_URL));
        assert!(link_url("setup").unwrap().contains("/docs/SETUP.md#"));
        assert_eq!(link_url("https://example.com"), None);
        assert_eq!(link_url(""), None);
    }

    #[test]
    fn invalid_events_are_skipped() {
        let steps = parse_steps(&json!([
            {"code": 30, "down": true, "delay": 0},
            {"code": "<img src=x>", "down": true},           // not a number
            {"code": 70000, "down": true},                    // out of range
            {"code": 31, "down": true, "disabled": true},     // disabled
            {"type": "move", "dx": 1e12, "dy": -3, "delay": 8},
        ]));
        assert_eq!(steps.len(), 2);
        assert!(matches!(steps[1].event, MacroEvent::Move { dx: i32::MAX, dy: -3, reports: 1 }));
    }
}
