//! Keyboard Macros: OpenDeck / Stream Deck plugin that records, edits and replays
//! keyboard and mouse macros.
//!
//! A single self-contained executable: OpenDeck starts it with
//! `-port <p> -pluginUUID <id> -registerEvent <event> -info <json>`.

// No console window on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod common;
mod input;
mod plugin;
mod recorder;
mod ws;

use serde_json::{Value, json};
use std::collections::HashMap;

fn main() {
    // Arguments come in "-name value" pairs.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args: HashMap<&str, &str> = argv
        .chunks(2)
        .filter_map(|pair| match pair {
            [name, value] => Some((name.trim_start_matches('-'), value.as_str())),
            _ => None,
        })
        .collect();
    log!("start {} {}/{} (port {})", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH, args.get("port").unwrap_or(&"?"));

    let (Some(port), Some(uuid), Some(register)) = (
        args.get("port").and_then(|p| p.parse::<u16>().ok()),
        args.get("pluginUUID"),
        args.get("registerEvent"),
    ) else {
        eprintln!("Keyboard Macros is an OpenDeck plugin: it is started by OpenDeck, not by hand.");
        log!("missing -port / -pluginUUID / -registerEvent arguments");
        std::process::exit(2);
    };

    let (out, incoming) = match ws::connect(port) {
        Ok(channels) => channels,
        Err(e) => {
            log!("cannot connect to OpenDeck: {e}");
            std::process::exit(1);
        }
    };
    let _ = out.send(json!({"event": register, "uuid": uuid}).to_string());
    // Global settings hold the "verbose logging" switch of the About tab.
    let _ = out.send(json!({"event": "getGlobalSettings", "context": uuid}).to_string());

    let plugin = plugin::Plugin::new(out, uuid.to_string());
    for text in incoming {
        match serde_json::from_str::<Value>(&text) {
            Ok(msg) => plugin.handle(&msg),
            Err(e) => log!("invalid message from OpenDeck: {e}"),
        }
    }
    log!("connection closed");
    plugin.shutdown();
}
