# AGENTS.md

Guidance for AI coding agents (Claude Code, Codex, Cursor, Gemini CLI, Copilot…) working in this repository.

## Project

Keyboard Macros: an OpenDeck plugin that records, edits and replays keyboard and mouse macros. It also follows the Stream Deck SDK. Two parts:

- **Engine**: Rust executable in `src/`. One background process for all keys, started by OpenDeck and running as long as OpenDeck is open.
- **Editor** (Property Inspector): plain HTML/JS, no build step, in `com.mrbretzel.keymacro.sdPlugin/pi/`.

They never talk directly: OpenDeck relays every message over a local WebSocket. The engine uses the official `openaction` crate for that connection. `docs/ARCHITECTURE.md` is the reference for the lifecycle, threads, message protocol and stored data. Keep it in sync when that behaviour changes.

## Commands

```bash
cargo test                                               # unit tests (host platform)
cargo test recorder::tests::editor_stop_drops_the_stop_click   # a single test
cargo fmt                                                # rustfmt.toml: max_width 110
cargo clippy --all-targets                               # must stay warning-free
cargo clippy --all-targets --target x86_64-pc-windows-gnu      # Windows backend
cargo clippy --all-targets --target aarch64-apple-darwin       # macOS backend (also x86_64-apple-darwin)
cargo build --release --target x86_64-unknown-linux-musl # static Linux executable
./install.sh                                             # build + copy the plugin into OpenDeck (Flatpak or native)
cargo audit                                              # known vulnerabilities in dependencies
```

- **Other platforms.** Windows and macOS code can only be type-checked or linted locally, because there is no linker for them on this machine. Their tests run in CI.
- **CI** (`.github/workflows/release.yml`) runs on every push:
  - tests on all four platforms;
  - a macOS universal binary, made with `lipo`;
  - the release zip.

  A `v*` tag attaches the zip to the matching GitHub release, and the tag must equal `Version` in `manifest.json`.
- **Versions.** When bumping the version, change both `Cargo.toml` and `com.mrbretzel.keymacro.sdPlugin/manifest.json`.
- **Editor.** There is no JS tooling. Check syntax by concatenating `keys.js i18n.js locales/*.js appearance.js` with the inline `<script>` of `index.html`, then run `node --check`.
- **After `./install.sh`**, OpenDeck must be restarted to load the new engine.

## Architecture: the parts that span several files

- **Macro format.** Every event is stored with Linux evdev codes, which name physical keys, on every OS:
  - `{code, down, delay}` for a key or mouse button;
  - `{type:"move", dx, dy, delay}`;
  - `{type:"wheel", dx, dy, delay}`.

  `delay` is in ms since the previous event. The editor table (`pi/keys.js`) and the Windows and macOS backends all translate from these codes. In `src/input/windows.rs` and `macos.rs`, the key tables are `#[rustfmt::skip]` and covered by round-trip unit tests. Keep the three in agreement when adding keys.
- **Platform backends.** `src/input/{linux,windows,macos}.rs` must expose the same API, re-exported by `src/input/mod.rs`:
  - `VirtualInput` (`prepare`, `key`, `move_by`, `wheel`, `release_all`, `status`);
  - `start_recording(options, on_event, on_stop) -> (RecorderHandle, device names)`;
  - `diagnostics()`.

  Filtering, movement merging, delays and the event cap live once in `src/recorder.rs` (`RecorderCore`), fed by each backend.
- **Connection (`src/host.rs`).** It is the only async code (tokio). The `openaction` crate calls `MacroAction` / `GlobalHandler`, which forward to `Plugin::on_*` inside `block_in_place`. `Plugin` and its threads never call the crate: they queue `Outgoing` messages, which `host::deliver` sends in order. A new OpenDeck message type means:
  1. a variant in `Outgoing`;
  2. a case in `deliver_one` and in `describe`.

  Messages for keys that are no longer visible are dropped, because the crate only knows visible instances.
- **Plugin state.** `src/plugin.rs` keeps all mutable state behind one `Mutex<State>`. Playback runs on its own thread per key. Key state changes that must not race (end of playback) go through `set_state_locked`. Delays from settings are user-controlled: always go through `bounded_delay`, and use saturating arithmetic on values from settings or the OS.
- **Errors are codes, not text.** The engine sends `{"type":"error","code":...}`. Every code needs an `error.<code>` entry in both `pi/locales/en.js` and `fr.js`. The engine contains no user-facing strings.
- **Global settings.** They hold `language` and `debug`. OpenDeck replaces the whole object on save, so always change them through `Plugin::set_global`, which saves the full object. A language change is broadcast to every key's editor (`broadcast_to_editors`), because OpenDeck keeps already-shown editors alive without reloading them.
- **Custom key images.** They are rendered by the editor (`pi/appearance.js`, canvas), because the engine has no graphics library. They are stored in the action settings as `images: {"0","1","2"}`, one per state (idle, playing, recording). The engine only forwards them with `setImage`, and `setFeedback` for dials. A missing `images` key means "never customised: do not override OpenDeck's own image".
- **Dials (Stream Deck+).** Three macro slots in the settings: `events` (press/touch), `eventsCW` and `eventsCCW` (one pass per notch, queued up to 50).
- **Editor translations.** `I18N.<lang>` dictionaries; `en.js` and `fr.js` must have identical key sets. Key names can have per-platform variants (`key.LEFTMETA.mac`, `.windows`), resolved by `keyLabel` from the platform the engine reports.
- **Editor safety.**
  - Escape every value interpolated into `innerHTML` (`escapeHtml`), since settings can come from shared profiles.
  - Do not use `window.confirm()`: it may be blocked in OpenDeck's webview. Use `confirmClick`.
  - The editor never sends a URL to open: it sends a `target`, and `link_url` in `plugin.rs` maps it to a known page.
- **Logging.** `log!` writes to `plugin.log` and, once connected, to OpenDeck's log (`logMessage`). `debug_log!` writes to the file only. Use `write_log_file_only` for problems of the connection itself, to avoid a loop.

## Environment notes

- The user runs OpenDeck as a Flatpak (`me.amankhanna.opendeck`). Plugins run inside its sandbox and are installed in `~/.var/app/me.amankhanna.opendeck/config/opendeck/plugins/`. The engine writes `plugin.log` (times in UTC) next to its executable.
- Inside the sandbox, groups show up as `nfsnobody`, yet `/dev/uinput` and `/dev/input/event*` are still accessible.
- OpenDeck does not read the manifest localisation files in `lang/`, and does not provide Python or Node.js. That is why the engine is a static Rust binary.

## Working with this user

- The user writes in French. Code, comments, README, docs and commit messages stay in English.
- Pushing, tagging, creating releases and opening pull requests are public actions. Only do them when the user explicitly asks. "Test only locally" means commit locally and do not push.
- The project is listed in the OpenAction plugin catalogue through a PR to `OpenActionAPI/plugins`, with `"ai_assisted": true`. Its description must equal the GitHub repository description, and the catalogue installs from the latest GitHub release.
