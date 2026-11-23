//! Connection to OpenDeck, through the `openaction` crate (the OpenAction plugin API).
//!
//! - Incoming: the crate decodes OpenDeck's events and calls `MacroAction` (per-key
//!   events) and `GlobalHandler` (plugin-wide events). They forward to the synchronous
//!   `Plugin` entry points, inside `block_in_place` because some of them briefly block
//!   (starting a recording, running diagnostics).
//! - Outgoing: `Plugin` and its threads queue `Outgoing` messages; `deliver` sends them
//!   one after the other, so OpenDeck receives them in the order they were produced.

use crate::common::{DEBUG, shorten_for_log, write_log_file_only};
use crate::debug_log;
use crate::plugin::{ACTION_UUID, Outgoing, Plugin};
use openaction::global_events::{
    DeviceDidConnectEvent, DeviceDidDisconnectEvent, DidReceiveGlobalSettingsEvent, GlobalEventHandler,
};
use openaction::{Action, Instance, OpenActionResult, async_trait};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc::UnboundedReceiver;

/// Runs a synchronous `Plugin` call from an async handler without stalling the runtime.
fn blocking<T>(f: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(f)
}

fn log_event(event: &str, instance: &Instance, settings: &Value) {
    if DEBUG.load(Ordering::Relaxed) {
        let detail = shorten_for_log(&settings.to_string());
        write_log_file_only(&format!("[debug] <- {event} {} {detail}", instance.instance_id));
    }
}

/// The plugin's only action ("Keyboard macro"), for keys and dials.
struct MacroAction(Arc<Plugin>);

#[async_trait]
impl Action for MacroAction {
    const UUID: &'static str = ACTION_UUID;
    type Settings = Value;

    async fn will_appear(&self, instance: &Instance, settings: &Value) -> OpenActionResult<()> {
        log_event("willAppear", instance, settings);
        blocking(|| {
            self.0.on_appear(&instance.instance_id, &instance.device_id, &instance.controller, settings)
        });
        Ok(())
    }

    async fn will_disappear(&self, instance: &Instance, settings: &Value) -> OpenActionResult<()> {
        log_event("willDisappear", instance, settings);
        blocking(|| self.0.on_disappear(&instance.instance_id));
        Ok(())
    }

    async fn did_receive_settings(&self, instance: &Instance, settings: &Value) -> OpenActionResult<()> {
        log_event("didReceiveSettings", instance, settings);
        blocking(|| self.0.on_settings(&instance.instance_id, settings));
        Ok(())
    }

    async fn key_down(&self, instance: &Instance, settings: &Value) -> OpenActionResult<()> {
        log_event("keyDown", instance, settings);
        blocking(|| self.0.on_press(&instance.instance_id, settings));
        Ok(())
    }

    async fn dial_down(&self, instance: &Instance, settings: &Value) -> OpenActionResult<()> {
        log_event("dialDown", instance, settings);
        blocking(|| self.0.on_press(&instance.instance_id, settings));
        Ok(())
    }

    async fn touch_tap(
        &self,
        instance: &Instance,
        settings: &Value,
        _position: (u16, u16),
        _hold: bool,
    ) -> OpenActionResult<()> {
        log_event("touchTap", instance, settings);
        blocking(|| self.0.on_press(&instance.instance_id, settings));
        Ok(())
    }

    async fn dial_rotate(
        &self,
        instance: &Instance,
        settings: &Value,
        ticks: i16,
        _pressed: bool,
    ) -> OpenActionResult<()> {
        log_event("dialRotate", instance, settings);
        blocking(|| self.0.on_rotate(&instance.instance_id, settings, ticks));
        Ok(())
    }

    async fn property_inspector_did_appear(
        &self,
        instance: &Instance,
        settings: &Value,
    ) -> OpenActionResult<()> {
        log_event("propertyInspectorDidAppear", instance, settings);
        blocking(|| self.0.on_editor_appear(&instance.instance_id));
        Ok(())
    }

    async fn send_to_plugin(
        &self,
        instance: &Instance,
        _settings: &Value,
        payload: &Value,
    ) -> OpenActionResult<()> {
        log_event("sendToPlugin", instance, payload);
        blocking(|| self.0.on_command(&instance.instance_id, payload));
        Ok(())
    }
}

/// Plugin-wide events.
struct GlobalHandler(Arc<Plugin>);

#[async_trait]
impl GlobalEventHandler for GlobalHandler {
    /// Registered with OpenDeck: ask for the saved global settings (language, verbose log)
    /// and note the decks already connected (from the `-info` launch argument).
    async fn plugin_ready(&self) -> OpenActionResult<()> {
        let decks =
            openaction::get_connected_devices().await.into_iter().map(|(id, d)| (id, d.name)).collect();
        blocking(|| {
            self.0.set_decks(decks);
            self.0.log_requirements();
        });
        openaction::get_global_settings().await
    }

    async fn device_did_connect(&self, event: DeviceDidConnectEvent) -> OpenActionResult<()> {
        blocking(|| self.0.on_deck_connected(&event.device, &event.deviceInfo.name));
        Ok(())
    }

    async fn device_did_disconnect(&self, event: DeviceDidDisconnectEvent) -> OpenActionResult<()> {
        blocking(|| self.0.on_deck_disconnected(&event.device));
        Ok(())
    }

    async fn did_receive_global_settings(
        &self,
        event: DidReceiveGlobalSettingsEvent,
    ) -> OpenActionResult<()> {
        blocking(|| self.0.on_global_settings(&event.payload.settings));
        Ok(())
    }
}

/// Registers the action and the global event handler. Call before `openaction::run`.
pub async fn install(plugin: Arc<Plugin>) {
    // The handler must live for the whole process: leak one small box.
    openaction::global_events::set_global_event_handler(Box::leak(Box::new(GlobalHandler(plugin.clone()))));
    openaction::register_action(MacroAction(plugin)).await;
}

/// Sends the queued messages to OpenDeck, in order, until the queue closes.
pub async fn deliver(mut queue: UnboundedReceiver<Outgoing>) {
    while let Some(message) = queue.recv().await {
        debug_log!("-> {}", shorten_for_log(&describe(&message)));
        if let Err(e) = deliver_one(message).await {
            // File only: reporting through the failing connection would loop.
            write_log_file_only(&format!("could not send a message to OpenDeck: {e}"));
        }
    }
}

/// One readable log line for a message: event name, key ID and compact JSON payload.
fn describe(message: &Outgoing) -> String {
    match message {
        Outgoing::SetState { context, state } => format!("setState {context} {state}"),
        Outgoing::SetImage { context, image, state } => {
            format!("setImage {context} state {state} {}", image.as_deref().unwrap_or("(manifest image)"))
        }
        Outgoing::SetFeedback { context, payload } => format!("setFeedback {context} {payload}"),
        Outgoing::ShowAlert { context } => format!("showAlert {context}"),
        Outgoing::SetSettings { context, settings } => format!("setSettings {context} {settings}"),
        Outgoing::ToEditor { context, payload } => format!("sendToPropertyInspector {context} {payload}"),
        Outgoing::ToAllEditors { payload } => format!("sendToPropertyInspector (all visible) {payload}"),
        Outgoing::SetGlobalSettings { settings } => format!("setGlobalSettings {settings}"),
        Outgoing::ShowOk { context } => format!("showOk {context}"),
        Outgoing::OpenUrl { url } => format!("openUrl {url}"),
        Outgoing::LogMessage { message } => format!("logMessage {message}"),
        Outgoing::SetupNotification { problems, .. } => format!("setup notification {}", problems.join(", ")),
    }
}

async fn deliver_one(message: Outgoing) -> OpenActionResult<()> {
    let context = match &message {
        Outgoing::SetGlobalSettings { settings } => return openaction::set_global_settings(settings).await,
        Outgoing::OpenUrl { url } => return openaction::open_url(url.clone()).await,
        Outgoing::LogMessage { message } => return openaction::log_message(message.clone()).await,
        Outgoing::SetupNotification { problems, french } => {
            // Runs on its own: it waits for clicks for the rest of the session.
            let url = crate::plugin::link_url("setup").unwrap_or_default();
            tokio::spawn(crate::notify::show_setup(problems.clone(), *french, move || {
                let url = url.clone();
                tokio::spawn(async move {
                    let _ = openaction::open_url(url).await;
                });
            }));
            return Ok(());
        }
        Outgoing::ToAllEditors { payload } => {
            for instance in openaction::visible_instances(ACTION_UUID).await {
                instance.send_to_property_inspector(payload).await?;
            }
            return Ok(());
        }
        Outgoing::SetState { context, .. }
        | Outgoing::SetImage { context, .. }
        | Outgoing::SetFeedback { context, .. }
        | Outgoing::ShowAlert { context }
        | Outgoing::ShowOk { context }
        | Outgoing::SetSettings { context, .. }
        | Outgoing::ToEditor { context, .. } => context.clone(),
    };
    // A key that has disappeared (page or profile change) has nothing left to update.
    let Some(instance) = openaction::get_instance(context).await else { return Ok(()) };
    match message {
        Outgoing::SetState { state, .. } => instance.set_state(u16::from(state)).await,
        Outgoing::SetImage { image, state, .. } => instance.set_image(image, Some(u16::from(state))).await,
        Outgoing::SetFeedback { payload, .. } => instance.set_feedback(&payload).await,
        Outgoing::ShowAlert { .. } => instance.show_alert().await,
        Outgoing::ShowOk { .. } => instance.show_ok().await,
        Outgoing::SetSettings { settings, .. } => instance.set_settings(&settings).await,
        Outgoing::ToEditor { payload, .. } => instance.send_to_property_inspector(payload).await,
        Outgoing::SetGlobalSettings { .. }
        | Outgoing::ToAllEditors { .. }
        | Outgoing::OpenUrl { .. }
        | Outgoing::LogMessage { .. }
        | Outgoing::SetupNotification { .. } => Ok(()),
    }
}

/// Routes the `log` records of the `openaction` crate (warnings and errors, such as an
/// event it could not decode) to `plugin.log`.
struct LogBridge;

impl log::Log for LogBridge {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            // File only: these records can come from the connection itself.
            write_log_file_only(&format!(
                "[{}] {}",
                record.level(),
                shorten_for_log(&record.args().to_string())
            ));
        }
    }

    fn flush(&self) {}
}

static LOG_BRIDGE: LogBridge = LogBridge;

pub fn init_log_bridge() {
    if log::set_logger(&LOG_BRIDGE).is_ok() {
        log::set_max_level(log::LevelFilter::Warn);
    }
}
