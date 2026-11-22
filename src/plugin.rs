//! Plugin logic: reacts to OpenDeck events, plays and records macros, updates keys.
//!
//! User-facing text is not produced here: errors are sent to the property inspector as
//! codes, and the property inspector translates them.

use crate::common::{
    DEBUG, InputError, MacroEvent, Step, clear_log, log_path, open_folder, parse_steps, plugin_dir, read_log_tail,
    shorten_for_log,
};
use crate::input::{RecorderHandle, VirtualInput, diagnostics, start_recording};
use crate::{debug_log, log};
use crate::recorder::RecordOptions;
use base64::Engine;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const ACTION_UUID: &str = "com.mrbretzel.keymacro.macro";

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

/// How much of the log the About tab shows.
const LOG_TAIL_BYTES: u64 = 48 * 1024;

const STATE_ICONS: [&str; 3] = ["icons/state_idle@2x.png", "icons/state_play@2x.png", "icons/state_rec@2x.png"];

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
}

pub struct Plugin {
    out: Sender<String>,
    /// UUID given by OpenDeck at launch; also the context of global settings.
    plugin_uuid: String,
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
    pub fn new(out: Sender<String>, plugin_uuid: String) -> Arc<Self> {
        Arc::new(Self {
            out,
            plugin_uuid,
            input: VirtualInput::default(),
            state: Mutex::new(State::default()),
            default_icons: STATE_ICONS.iter().map(|p| load_data_url(p)).collect(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    // ------------------------------------------------------------ SDK helpers

    fn send(&self, mut message: Value) {
        if let Value::Object(map) = &mut message {
            map.retain(|_, v| !v.is_null());
        }
        let text = message.to_string();
        debug_log!("-> {}", shorten_for_log(&text));
        let _ = self.out.send(text);
    }

    fn send_event(&self, event: &str, context: &str, payload: Value) {
        self.send(json!({"event": event, "context": context, "payload": payload}));
    }

    fn to_pi(&self, context: &str, payload: Value) {
        self.send(json!({"event": "sendToPropertyInspector", "context": context, "action": ACTION_UUID, "payload": payload}));
    }

    fn error(&self, context: &str, code: &str, detail: &str) {
        self.to_pi(context, json!({"type": "error", "code": code, "detail": detail}));
    }

    fn alert(&self, context: &str) {
        self.send(json!({"event": "showAlert", "context": context}));
    }

    fn set_state(&self, context: &str, state: u8) {
        let mut st = self.lock();
        st.states.insert(context.to_owned(), state);
        self.send_event("setState", context, json!({"state": state}));
        self.apply_image(&mut st, context);
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
                self.send_event("setFeedback", context, json!({"icon": icon}));
            }
        }
        let managed = settings.is_some_and(|s| s.get("images").is_some());
        if image.is_none() && !st.custom_shown.contains(context) && !managed {
            // The appearance editor was never used: leave any image set in OpenDeck's own UI alone.
            return;
        }
        let mut payload = json!({"target": 0, "state": state});
        match image {
            Some(image) => {
                payload["image"] = json!(image);
                st.custom_shown.insert(context.to_owned());
            }
            None => {
                st.custom_shown.remove(context);
            }
        }
        self.send_event("setImage", context, payload);
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
        let needs_keyboard = steps.iter().any(|st| matches!(st.event, MacroEvent::Key { code, .. } if !crate::common::is_mouse_button(code)));
        self.input.prepare(needs_keyboard, needs_mouse)?;
        // Absolute deadlines, so that the time spent sending input does not accumulate.
        debug_log!("play {} steps (speed {:.0}%, fixed delay: {use_fixed})", steps.len(), speed * 100.0);
        let mut deadline = Instant::now();
        for (i, step) in steps.iter().enumerate() {
            // The fixed delay replaces key / button / wheel delays; mouse movements keep
            // theirs, otherwise a recorded gesture would become very slow.
            let is_move = matches!(step.event, MacroEvent::Move { .. });
            let delay = if use_fixed && i > 0 && !is_move { fixed } else { step.delay_ms };
            deadline += Duration::from_secs_f64(delay / 1000.0 / speed);
            let wait = deadline.saturating_duration_since(Instant::now());
            if !wait.is_zero() && stop.wait(wait) {
                return Ok(false);
            }
            match step.event {
                MacroEvent::Key { code, down } => self.input.key(code, down)?,
                MacroEvent::Move { dx, dy } => self.input.move_by(dx, dy)?,
                MacroEvent::Wheel { dx, dy } => self.input.wheel(dx, dy)?,
            }
        }
        Ok(!stop.is_set())
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
        {
            let mut st = self.lock();
            st.players.remove(context);
            st.turns.remove(context);
        }
        self.set_state(context, STATE_IDLE);
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
        let repeat_delay = Duration::from_secs_f64(number(&s, "repeatDelay").unwrap_or(0.0).max(0.0) / 1000.0);
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
        pending.notches = (pending.notches + ticks.unsigned_abs() as u32).min(MAX_PENDING_TICKS);
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

    fn start_recording(self: &Arc<Self>, context: &str, append: bool, slot: &'static str, options: RecordOptions) {
        let mut st = self.lock();
        if st.recorder.is_some() {
            drop(st);
            self.error(context, "already_recording", "");
            return;
        }
        let (this, ctx) = (self.clone(), context.to_owned());
        let on_event = Box::new(move |ev: &Value| this.to_pi(&ctx, json!({"type": "recordEvent", "event": ev})));
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
        self.send_event("setSettings", context, settings);
        self.set_state(context, STATE_IDLE);
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

    // ------------------------------------------------------------ event dispatch

    pub fn handle(self: &Arc<Self>, msg: &Value) {
        let event = msg.get("event").and_then(Value::as_str).unwrap_or("");
        let context = msg.get("context").and_then(Value::as_str).unwrap_or("");
        let empty = json!({});
        let payload = msg.get("payload").unwrap_or(&empty);
        debug_log!("<- {}", shorten_for_log(&msg.to_string()));

        {
            let mut st = self.lock();
            if event != "sendToPlugin" {
                if let Some(settings) = payload.get("settings") {
                    st.settings.insert(context.to_owned(), settings.clone());
                }
            }
            if let Some(controller) = payload.get("controller").and_then(Value::as_str) {
                st.controllers.insert(context.to_owned(), controller.to_owned());
            }
        }

        match event {
            "willAppear" => {
                let mut st = self.lock();
                let busy = st.players.contains_key(context) || st.recording_ctx.as_deref() == Some(context);
                if !busy {
                    st.states.insert(context.to_owned(), STATE_IDLE);
                }
                self.apply_image(&mut st, context);
            }
            "didReceiveSettings" => {
                let mut st = self.lock();
                self.apply_image(&mut st, context); // appearance may have changed
            }
            "keyDown" | "dialDown" | "touchTap" => {
                if self.is_recording(context) {
                    self.stop_recording(false); // pressing the deck key / dial ends the recording
                } else {
                    self.play(context);
                }
            }
            "dialRotate" => {
                if !self.is_recording(context) {
                    self.rotate(context, payload.get("ticks").and_then(Value::as_i64).unwrap_or(0));
                }
            }
            "willDisappear" => self.stop_playing(context),
            "didReceiveGlobalSettings" => {
                let debug = payload.pointer("/settings/debug").and_then(Value::as_bool).unwrap_or(false);
                DEBUG.store(debug, Ordering::Relaxed);
            }
            "sendToPlugin" => self.handle_command(context, payload),
            _ => {}
        }
    }

    /// Commands sent by the property inspector.
    fn handle_command(self: &Arc<Self>, context: &str, payload: &Value) {
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
            "stopRecord" => self.stop_recording(true),
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
                let about = json!({
                    "type": "about",
                    "version": env!("CARGO_PKG_VERSION"),
                    "os": std::env::consts::OS,
                    "arch": std::env::consts::ARCH,
                    "pluginDir": plugin_dir().to_string_lossy(),
                    "logPath": log_path().to_string_lossy(),
                    "debug": DEBUG.load(Ordering::Relaxed),
                    "devices": self.input.status(),
                    "controller": self.lock().controllers.get(context).cloned().unwrap_or_else(|| "Keypad".to_owned()),
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
            "openLogFolder" => {
                if let Err(e) = open_folder(&plugin_dir()) {
                    self.error(context, "open_failed", &e.to_string());
                }
            }
            "setDebug" => {
                let enabled = flag("enabled").unwrap_or(false);
                DEBUG.store(enabled, Ordering::Relaxed);
                log!("verbose logging {}", if enabled { "enabled" } else { "disabled" });
                self.send(json!({"event": "setGlobalSettings", "context": self.plugin_uuid, "payload": {"debug": enabled}}));
            }
            "status" => {
                let st = self.lock();
                let status = json!({
                    "type": "status",
                    "recording": st.recording_ctx.as_deref() == Some(context),
                    "playing": st.players.contains_key(context),
                    "controller": st.controllers.get(context).map(String::as_str).unwrap_or("Keypad"),
                    "platform": std::env::consts::OS,
                });
                drop(st);
                self.to_pi(context, status);
            }
            _ => {}
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
