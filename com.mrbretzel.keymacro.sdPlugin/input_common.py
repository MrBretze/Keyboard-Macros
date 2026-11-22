"""
Platform-independent pieces shared by the input backends.

Macro event formats (stored in the action settings, identical on every platform):
  {"code": 30, "down": true, "delay": 0}              key or mouse button (evdev code)
  {"type": "move", "dx": 12, "dy": -3, "delay": 8}    relative mouse movement
  {"type": "wheel", "dx": 0, "dy": 1, "delay": 120}   wheel notches (dy > 0 = up, dx > 0 = right)
"delay" is in milliseconds since the previous event.

Key codes are Linux evdev codes (physical keys). On Linux they are used as-is; the
Windows backend translates them to/from PC scan codes and mouse button flags.
"""

import os
import sys
import time

# Prefix of our virtual devices' names, so the recorder can ignore our own output.
VIRTUAL_PREFIX = "OpenDeck KeyMacro"
VIRTUAL_KEYBOARD_NAME = VIRTUAL_PREFIX + " virtual keyboard"
VIRTUAL_MOUSE_NAME = VIRTUAL_PREFIX + " virtual mouse"

KEY_MAX = 0x2FF
BTN_MISC, KEY_OK = 0x100, 0x160  # mouse / gamepad / touchpad button range

# Mouse buttons (evdev codes)
BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA = 0x110, 0x111, 0x112, 0x113, 0x114
MOUSE_BUTTONS = (BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA)

# Relative movements are merged over this window: mice report up to 1000 times per
# second, which would make huge, unreadable macros.
MOVE_MERGE_WINDOW = 0.012  # seconds


def is_keyboard_code(code):
    """True for keyboard keys (mouse, gamepad and touchpad buttons are excluded)."""
    return 0 < code <= KEY_MAX and not (BTN_MISC <= code < KEY_OK)


def is_mouse_button(code):
    return code in MOUSE_BUTTONS


def event_type(ev):
    return ev.get("type", "key")


def base_dir():
    """Plugin folder, also when frozen into an executable (PyInstaller)."""
    if getattr(sys, "frozen", False):
        return os.path.dirname(sys.executable)
    return os.path.dirname(os.path.abspath(__file__))


LOG_PATH = os.path.join(base_dir(), "plugin.log")


def log(*args):
    try:
        with open(LOG_PATH, "a", encoding="utf-8") as f:
            f.write(time.strftime("%H:%M:%S ") + " ".join(str(a) for a in args) + "\n")
    except OSError:
        pass


class InputError(Exception):
    """Error with a stable code that the property inspector can translate."""

    def __init__(self, code, detail=""):
        super().__init__(f"{code}: {detail}" if detail else code)
        self.code = code
        self.detail = detail


class RecorderBase:
    """
    Turns raw input into macro events (see the module docstring for the formats).

    Subclasses capture input and call _feed_key() / _feed_move() / _feed_wheel(), then
    _finish() when capture stops. With mouse=False, mouse input is ignored; with
    mouse_move=False, only mouse buttons and the wheel are kept.
    """

    def __init__(self, on_event, on_stop, mouse=False, mouse_move=True):
        self.on_event = on_event
        self.on_stop = on_stop
        self.mouse = mouse
        self.mouse_move = mouse and mouse_move
        self.events = []
        self._held = set()
        self._last_t = None
        self._move = None  # pending merged movement: [dx, dy, first_t, last_t]
        self.trim_stop_click = False

    def _emit(self, ev, t):
        delay = 0 if self._last_t is None else max(0, round((t - self._last_t) * 1000))
        self._last_t = t
        ev["delay"] = delay
        self.events.append(ev)
        self.on_event(ev)

    def _flush_move(self):
        if self._move:
            dx, dy, _, last_t = self._move
            self._move = None
            if dx or dy:
                self._emit({"type": "move", "dx": dx, "dy": dy}, last_t)

    def _feed_key(self, code, down, t):
        """code: evdev code (key or mouse button), down: bool, t: timestamp in seconds."""
        if not (is_keyboard_code(code) or (self.mouse and is_mouse_button(code))):
            return
        if down and code in self._held:
            return  # auto-repeat, or duplicate from a virtual keyboard re-emitting keys
        if not down and code not in self._held:
            return  # release of a key pressed before the recording started
        if down:
            self._held.add(code)
        else:
            self._held.discard(code)
        self._flush_move()
        self._emit({"code": code, "down": down}, t)

    def _feed_move(self, dx, dy, t):
        if not self.mouse_move or not (dx or dy):
            return
        if self._move is None:
            self._move = [0, 0, t, t]
        self._move[0] += dx
        self._move[1] += dy
        self._move[3] = t
        if t - self._move[2] >= MOVE_MERGE_WINDOW:
            self._flush_move()

    def _feed_wheel(self, dx, dy, t):
        if not self.mouse or not (dx or dy):
            return
        self._flush_move()
        self._emit({"type": "wheel", "dx": dx, "dy": dy}, t)

    def stop_from_editor(self):
        """Stop requested with the editor's button: the click on that button (and the
        movement to reach it) must not end up in the macro."""
        self.trim_stop_click = self.mouse
        self.stop()

    def _trim_stop_click(self):
        def pop_moves():
            while self.events and event_type(self.events[-1]) == "move":
                self.events.pop()

        def pop_left(down):
            last = self.events[-1] if self.events else None
            if last and event_type(last) == "key" and last.get("code") == BTN_LEFT and bool(last.get("down")) == down:
                self.events.pop()
                return True
            return False

        # The browser fires "click" on release, so the macro ends with: moves, left down,
        # (moves), left up, (moves). Remove that tail.
        pop_moves()
        pop_left(False)
        pop_moves()
        pop_left(True)
        pop_moves()

    def _finish(self):
        self._flush_move()
        if self.trim_stop_click:
            self._trim_stop_click()
        # Keys and buttons still held when recording stops get a release event.
        held = []
        for ev in self.events:
            if event_type(ev) == "key":
                if ev["down"]:
                    held.append(ev["code"])
                elif ev["code"] in held:
                    held.remove(ev["code"])
        for code in sorted(set(held)):
            self.events.append({"code": code, "down": False, "delay": 0})
        self.on_stop(self.events)

    def stop(self):
        raise NotImplementedError
