//! Keyboard Macros: OpenDeck / Stream Deck plugin that records, edits and replays
//! keyboard and mouse macros.
//!
//! A single self-contained executable: OpenDeck starts it with
//! `-port <p> -pluginUUID <id> -registerEvent <event> -info <json>`. The connection to
//! OpenDeck goes through the `openaction` crate (see `host.rs`).

// No console window on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod common;
mod host;
mod input;
mod notify;
mod plugin;
mod recorder;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| {
        let i = args.iter().position(|a| a.eq_ignore_ascii_case(name))?;
        args.get(i + 1).map(String::as_str)
    };
    log!(
        "start {} {}/{} (port {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        flag("-port").unwrap_or("?")
    );

    // `openaction::run` panics on a missing flag: check them first for a clear message.
    if ["-port", "-pluginUUID", "-registerEvent", "-info"].iter().any(|f| flag(f).is_none()) {
        eprintln!("Keyboard Macros is an OpenDeck plugin: it is started by OpenDeck, not by hand.");
        log!("missing -port / -pluginUUID / -registerEvent / -info arguments");
        std::process::exit(2);
    }

    host::init_log_bridge();
    // Two worker threads are plenty: the runtime only carries the connection; playback
    // and recording run on their own threads.
    let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            log!("cannot start the async runtime: {e}");
            std::process::exit(1);
        }
    };

    runtime.block_on(async {
        let (queue, outgoing) = tokio::sync::mpsc::unbounded_channel();
        // From now on, log lines also reach OpenDeck's own log ("[Keyboard Macros] ...").
        let log_queue = queue.clone();
        common::set_log_sink(Box::new(move |line| {
            let _ = log_queue.send(plugin::Outgoing::LogMessage { message: line.to_owned() });
        }));
        // OpenDeck's interface language, for the texts the engine shows itself.
        let host_language = flag("-info")
            .and_then(|info| serde_json::from_str::<serde_json::Value>(info).ok())
            .and_then(|info| info.pointer("/application/language")?.as_str().map(str::to_owned))
            .unwrap_or_default();
        let plugin = plugin::Plugin::new(queue, host_language);
        tokio::spawn(host::deliver(outgoing));
        host::install(plugin.clone()).await;
        // Connects, registers, then handles OpenDeck's events until the connection closes.
        if let Err(e) = openaction::run(args.clone()).await {
            log!("connection to OpenDeck failed: {e}");
        }
        log!("connection closed");
        plugin.shutdown();
    });
}
