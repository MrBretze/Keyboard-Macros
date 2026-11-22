#!/usr/bin/env python3
"""
OpenDeck / Stream Deck plugin: recordable and editable keyboard macros.

Standard library only. The platform-specific input code lives in:
  - input_linux.py   (uinput + evdev)
  - input_windows.py (SendInput + low-level keyboard hook)

User-facing text is not produced here: errors are sent to the property inspector
as codes, and the property inspector translates them.
"""

import base64
import json
import os
import socket
import struct
import sys
import threading
import time
import traceback

# Make sibling modules importable whatever the working directory is.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from input_common import InputError, event_type, log  # noqa: E402

if sys.platform.startswith("win"):
    from input_windows import Recorder, VirtualInput  # noqa: E402
else:
    from input_linux import Recorder, VirtualInput  # noqa: E402

ACTION_UUID = "com.mrbretzel.keymacro.macro"

STATE_IDLE, STATE_PLAYING, STATE_RECORDING = 0, 1, 2


# --------------------------------------------------------------------------
# Minimal WebSocket client (RFC 6455), enough for the Stream Deck protocol
# --------------------------------------------------------------------------
class WebSocket:
    def __init__(self, port):
        self.sock = socket.create_connection(("127.0.0.1", port))
        self.send_lock = threading.Lock()
        key = base64.b64encode(os.urandom(16)).decode()
        req = (
            "GET / HTTP/1.1\r\n"
            f"Host: 127.0.0.1:{port}\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        self.sock.sendall(req.encode())
        resp = b""
        while b"\r\n\r\n" not in resp:
            chunk = self.sock.recv(1024)
            if not chunk:
                raise ConnectionError("handshake failed")
            resp += chunk
        head, self.buf = resp.split(b"\r\n\r\n", 1)
        if b" 101 " not in head.split(b"\r\n")[0]:
            raise ConnectionError("handshake refused: " + head.decode(errors="replace"))

    def _recv_exact(self, n):
        while len(self.buf) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise ConnectionError("closed")
            self.buf += chunk
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    @staticmethod
    def _mask(payload, mask):
        # XOR through big integers: much faster than a per-byte loop for large payloads (images).
        n = len(payload)
        if not n:
            return b""
        key = (mask * (n // 4 + 1))[:n]
        return (int.from_bytes(payload, "big") ^ int.from_bytes(key, "big")).to_bytes(n, "big")

    def _send_frame(self, opcode, payload):
        header = bytearray([0x80 | opcode])
        n = len(payload)
        if n < 126:
            header.append(0x80 | n)
        elif n < 65536:
            header.append(0x80 | 126)
            header += struct.pack(">H", n)
        else:
            header.append(0x80 | 127)
            header += struct.pack(">Q", n)
        mask = os.urandom(4)
        header += mask
        data = bytes(header) + self._mask(payload, mask)
        with self.send_lock:
            self.sock.sendall(data)

    def send(self, obj):
        self._send_frame(0x1, json.dumps(obj).encode())

    def recv(self):
        message, msg_op = b"", None
        while True:
            b1, b2 = self._recv_exact(2)
            fin, opcode = b1 & 0x80, b1 & 0x0F
            n = b2 & 0x7F
            if n == 126:
                n = struct.unpack(">H", self._recv_exact(2))[0]
            elif n == 127:
                n = struct.unpack(">Q", self._recv_exact(8))[0]
            mask = self._recv_exact(4) if b2 & 0x80 else None
            payload = self._recv_exact(n)
            if mask:
                payload = self._mask(payload, mask)
            if opcode == 0x9:  # ping
                self._send_frame(0xA, payload)
                continue
            if opcode == 0xA:  # pong
                continue
            if opcode == 0x8:
                raise ConnectionError("closed by server")
            if opcode != 0:
                msg_op = opcode
            message += payload
            if fin:
                return message.decode() if msg_op == 0x1 else message


# --------------------------------------------------------------------------
# Plugin
# --------------------------------------------------------------------------
# Macro slots stored in the action settings. Keys only use SLOT_PRESS; dials
# (Stream Deck+ encoders) also have one macro per rotation direction.
SLOT_PRESS, SLOT_CW, SLOT_CCW = "events", "eventsCW", "eventsCCW"
SLOTS = (SLOT_PRESS, SLOT_CW, SLOT_CCW)
MAX_PENDING_TICKS = 50  # cap on queued dial notches, so a fast spin cannot run for minutes

STATE_ICONS = ["icons/state_idle@2x.png", "icons/state_play@2x.png", "icons/state_rec@2x.png"]


def load_data_url(rel_path):
    try:
        with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), rel_path), "rb") as f:
            return "data:image/png;base64," + base64.b64encode(f.read()).decode()
    except OSError:
        return None


class Plugin:
    def __init__(self, ws):
        self.ws = ws
        self.kbd = VirtualInput()
        self.lock = threading.Lock()
        self.settings = {}  # context -> settings
        self.controllers = {}  # context -> "Keypad" | "Encoder"
        self.states = {}  # context -> current state index
        self.custom_shown = set()  # contexts currently displaying a custom image
        self.players = {}  # context -> {"stop": Event, "kind": "press" | "turn"}
        self.turns = {}  # context -> {"slot": str, "n": pending notches}
        self.recorder = None
        self.recording_ctx = None
        self.default_icons = [load_data_url(p) for p in STATE_ICONS]

    # --- SDK helpers ---
    def send(self, event, context, payload=None, **extra):
        msg = {"event": event, "context": context, **extra}
        if payload is not None:
            msg["payload"] = payload
        self.ws.send(msg)

    def to_pi(self, context, payload):
        self.send("sendToPropertyInspector", context, payload, action=ACTION_UUID)

    def error(self, context, code, detail=""):
        self.to_pi(context, {"type": "error", "code": code, "detail": detail})

    def is_encoder(self, context):
        return self.controllers.get(context) == "Encoder"

    def set_state(self, context, state):
        self.states[context] = state
        self.send("setState", context, {"state": state})
        self.apply_image(context)

    def apply_image(self, context):
        """Show the custom image rendered by the property inspector for the current state,
        or reset to the manifest image when there is none."""
        state = self.states.get(context, STATE_IDLE)
        settings = self.settings.get(context, {})
        images = settings.get("images") or {}
        image = images.get(str(state))
        if self.is_encoder(context):
            # The dial's touch-strip segment uses a layout: its icon is set through feedback.
            icon = image or self.default_icons[state]
            if icon:
                self.send("setFeedback", context, {"icon": icon})
        if not image and context not in self.custom_shown and "images" not in settings:
            # The appearance editor was never used: leave any image set in OpenDeck's own UI alone.
            return
        payload = {"target": 0, "state": state}
        if image:
            payload["image"] = image
            self.custom_shown.add(context)
        else:
            self.custom_shown.discard(context)
        self.send("setImage", context, payload)

    # --- playback ---
    def _play_pass(self, s, events, stop):
        """Plays the events once. Returns False if interrupted."""
        speed = max(float(s.get("speed", 100) or 100) / 100.0, 0.01)
        fixed = float(s.get("fixedDelay") or 0)
        use_fixed = bool(s.get("useFixedDelay"))
        # Absolute deadlines so that key-sending latency does not accumulate.
        deadline = time.monotonic()
        for i, ev in enumerate(events):
            if ev.get("disabled"):
                continue
            kind = event_type(ev)
            # The fixed delay replaces key / button / wheel delays; mouse movements keep
            # theirs, otherwise a recorded gesture would become very slow.
            if use_fixed and i > 0 and kind != "move":
                d = fixed
            else:
                d = float(ev.get("delay", 0) or 0)
            deadline += max(0.0, d) / 1000.0 / speed
            wait = deadline - time.monotonic()
            if wait > 0 and stop.wait(wait):
                return False
            if kind == "move":
                self.kbd.move(int(ev.get("dx", 0)), int(ev.get("dy", 0)))
            elif kind == "wheel":
                self.kbd.wheel(int(ev.get("dx", 0)), int(ev.get("dy", 0)))
            else:
                self.kbd.key(int(ev["code"]), bool(ev.get("down")))
        return not stop.is_set()

    def _start_player(self, context, kind, target):
        """Registers a player (caller holds self.lock) and starts its thread."""
        stop = threading.Event()
        self.players[context] = {"stop": stop, "kind": kind}
        threading.Thread(target=self._player_wrapper, args=(context, target, stop), daemon=True).start()

    def _player_wrapper(self, context, target, stop):
        try:
            self.set_state(context, STATE_PLAYING)
            self.to_pi(context, {"type": "playing", "playing": True})
            target(context, stop)
        except InputError as e:
            log("play error", e)
            self.error(context, e.code, e.detail)
            self.send("showAlert", context)
        except Exception as e:
            log("play error", traceback.format_exc())
            self.error(context, "play_failed", str(e))
            self.send("showAlert", context)
        finally:
            self.kbd.release_all()
            with self.lock:
                self.players.pop(context, None)
                self.turns.pop(context, None)
            self.set_state(context, STATE_IDLE)
            self.to_pi(context, {"type": "playing", "playing": False})

    def stop_playing(self, context):
        with self.lock:
            player = self.players.get(context)
        if player:
            player["stop"].set()

    def play(self, context):
        """Key press, dial press or touch tap: plays the main macro, or stops any playback."""
        with self.lock:
            if context in self.players:
                self.players[context]["stop"].set()
                return
            s = self.settings.get(context, {})
            if not s.get(SLOT_PRESS):
                self.send("showAlert", context)
                self.error(context, "empty_macro")
                return
            self._start_player(context, "press", self._press_loop)

    def _press_loop(self, context, stop):
        s = self.settings.get(context, {})
        events = s.get(SLOT_PRESS) or []
        repeat = int(s.get("repeat", 1) or 0)  # 0 = loop until pressed again
        repeat_delay = max(0, float(s.get("repeatDelay", 0) or 0)) / 1000.0
        n = 0
        while not stop.is_set() and (repeat == 0 or n < repeat):
            if n > 0 and stop.wait(repeat_delay):
                break
            if not self._play_pass(s, events, stop):
                break
            n += 1

    def rotate(self, context, ticks):
        """Dial rotation: queues one run of the direction's macro per notch."""
        if not ticks:
            return
        slot = SLOT_CW if ticks > 0 else SLOT_CCW
        if not self.settings.get(context, {}).get(slot):
            return
        with self.lock:
            player = self.players.get(context)
            if player and player["kind"] == "press":
                return  # the main macro is running: ignore the dial until it ends
            pending = self.turns.get(context)
            if not pending or pending["slot"] != slot:
                pending = {"slot": slot, "n": 0}  # changing direction drops the other queue
                self.turns[context] = pending
            pending["n"] = min(pending["n"] + abs(ticks), MAX_PENDING_TICKS)
            if not player:
                self._start_player(context, "turn", self._turn_loop)

    def _turn_loop(self, context, stop):
        while not stop.is_set():
            with self.lock:
                pending = self.turns.get(context)
                if not pending or pending["n"] <= 0:
                    return
                pending["n"] -= 1
                slot = pending["slot"]
            s = self.settings.get(context, {})
            if not self._play_pass(s, s.get(slot) or [], stop):
                return

    # --- recording ---
    def start_recording(self, context, append=False, slot=SLOT_PRESS, mouse=False, mouse_move=True):
        if slot not in SLOTS:
            slot = SLOT_PRESS
        if self.recorder:
            self.error(context, "already_recording")
            return

        def on_event(ev):
            self.to_pi(context, {"type": "recordEvent", "event": ev})

        def on_stop(events):
            self.recorder = None
            self.recording_ctx = None
            s = dict(self.settings.get(context, {}))
            if append and s.get(slot) and events:
                # Small gap between the existing macro and the appended part.
                events = list(s[slot]) + [dict(events[0], delay=200)] + events[1:]
            s[slot] = events
            self.settings[context] = s
            self.send("setSettings", context, s)
            self.set_state(context, STATE_IDLE)
            self.to_pi(context, {"type": "recorded", "events": events, "slot": slot})
            log("recorded", len(events), "events into", slot)

        rec = Recorder(on_event, on_stop, mouse=mouse, mouse_move=mouse_move)
        try:
            names = rec.start()
        except InputError as e:
            self.error(context, e.code, e.detail)
            return
        except Exception as e:
            log("record error", traceback.format_exc())
            self.error(context, "record_failed", str(e))
            return
        self.recorder = rec
        self.recording_ctx = context
        self.set_state(context, STATE_RECORDING)
        self.to_pi(context, {"type": "recording", "devices": names, "slot": slot})

    def stop_recording(self, from_editor=False):
        if self.recorder:
            if from_editor:
                self.recorder.stop_from_editor()  # drops the click on the Stop button
            else:
                self.recorder.stop()

    # --- event dispatch ---
    def handle(self, msg):
        event = msg.get("event")
        context = msg.get("context")
        payload = msg.get("payload") or {}

        if "settings" in payload and event != "sendToPlugin":
            self.settings[context] = payload["settings"]
        if payload.get("controller"):
            self.controllers[context] = payload["controller"]

        if event == "willAppear":
            busy = context in self.players or self.recording_ctx == context
            if not busy:
                self.states[context] = STATE_IDLE
            self.apply_image(context)
        elif event == "didReceiveSettings":
            self.apply_image(context)  # appearance may have changed
        elif event in ("keyDown", "dialDown", "touchTap"):
            if self.recorder and self.recording_ctx == context:
                self.stop_recording()  # pressing the deck key / dial ends the recording
            else:
                self.play(context)
        elif event == "dialRotate":
            if not (self.recorder and self.recording_ctx == context):
                self.rotate(context, int(payload.get("ticks", 0) or 0))
        elif event == "willDisappear":
            self.stop_playing(context)
        elif event == "sendToPlugin":
            cmd = payload.get("cmd")
            if cmd == "record":
                self.start_recording(
                    context, bool(payload.get("append")), payload.get("slot", SLOT_PRESS),
                    mouse=bool(payload.get("mouse")), mouse_move=payload.get("mouseMove", True) is not False,
                )
            elif cmd == "stopRecord":
                self.stop_recording(from_editor=True)
            elif cmd == "play":
                if "settings" in payload:
                    self.settings[context] = payload["settings"]
                slot = payload.get("slot", SLOT_PRESS)
                if slot == SLOT_CW:
                    self.rotate(context, 1)
                elif slot == SLOT_CCW:
                    self.rotate(context, -1)
                else:
                    self.play(context)
            elif cmd == "stopPlay":
                self.stop_playing(context)
            elif cmd == "status":
                self.to_pi(context, {
                    "type": "status",
                    "recording": self.recording_ctx == context,
                    "playing": context in self.players,
                    "controller": self.controllers.get(context, "Keypad"),
                    "platform": sys.platform,
                })

    def run(self):
        while True:
            raw = self.ws.recv()
            try:
                self.handle(json.loads(raw))
            except Exception:
                log("handler error", traceback.format_exc())


def main():
    args = {}
    argv = sys.argv[1:]
    for i in range(0, len(argv) - 1, 2):
        args[argv[i].lstrip("-")] = argv[i + 1]
    log("start", sys.platform, {k: v for k, v in args.items() if k != "info"})
    ws = WebSocket(int(args["port"]))
    ws.send({"event": args["registerEvent"], "uuid": args["pluginUUID"]})
    plugin = Plugin(ws)
    try:
        plugin.run()
    except ConnectionError as e:
        log("connection closed:", e)
    finally:
        plugin.stop_recording()
        plugin.kbd.close()


if __name__ == "__main__":
    main()
