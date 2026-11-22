"""
Linux input backend.

- Playback: a virtual keyboard and a virtual mouse created through /dev/uinput
  (works on Wayland and X11).
- Recording: reads physical keyboards (and mice, when enabled) from /dev/input/event*
  (evdev) without grabbing them.

Mouse movement is relative (Wayland gives no absolute cursor position): a replayed
movement goes through the desktop's pointer acceleration again, so the result may
differ slightly from the recording.

Requires read/write access to /dev/uinput and read access to /dev/input/event*
(usually by being in the "input" group).
"""

import fcntl
import glob
import os
import select
import struct
import threading
import time

from input_common import (
    KEY_MAX, MOUSE_BUTTONS, VIRTUAL_KEYBOARD_NAME, VIRTUAL_MOUSE_NAME, VIRTUAL_PREFIX,
    InputError, RecorderBase, is_keyboard_code, is_mouse_button, log,
)

EV_SYN, EV_KEY, EV_REL = 0x00, 0x01, 0x02
SYN_REPORT = 0
REL_X, REL_Y, REL_HWHEEL, REL_WHEEL = 0x00, 0x01, 0x06, 0x08
BTN_LEFT = 0x110
REL_MAX = 0x0F

INPUT_EVENT = struct.Struct("llHHi")  # struct input_event: timeval, type, code, value


def _IOC(direction, typ, nr, size):
    return (direction << 30) | (size << 16) | (ord(typ) << 8) | nr


UI_SET_EVBIT = _IOC(1, "U", 100, 4)
UI_SET_KEYBIT = _IOC(1, "U", 101, 4)
UI_SET_RELBIT = _IOC(1, "U", 102, 4)
UI_DEV_CREATE = _IOC(0, "U", 1, 0)
UI_DEV_DESTROY = _IOC(0, "U", 2, 0)


def EVIOCGNAME(length):
    return _IOC(2, "E", 0x06, length)


def EVIOCGBIT(ev, length):
    return _IOC(2, "E", 0x20 + ev, length)


class UinputDevice:
    """One virtual evdev device."""

    def __init__(self, name, keys, rels=()):
        self.name, self.keys, self.rels = name, keys, rels
        self.fd = None

    def open(self):
        if self.fd is not None:
            return
        try:
            fd = os.open("/dev/uinput", os.O_WRONLY | os.O_NONBLOCK)
        except OSError as e:
            raise InputError("uinput_failed", str(e))
        fcntl.ioctl(fd, UI_SET_EVBIT, EV_SYN)
        fcntl.ioctl(fd, UI_SET_EVBIT, EV_KEY)
        for code in self.keys:
            fcntl.ioctl(fd, UI_SET_KEYBIT, code)
        if self.rels:
            fcntl.ioctl(fd, UI_SET_EVBIT, EV_REL)
            for code in self.rels:
                fcntl.ioctl(fd, UI_SET_RELBIT, code)
        # struct uinput_user_dev (legacy API, supported everywhere)
        name = self.name.encode()[:79].ljust(80, b"\0")
        input_id = struct.pack("HHHH", 0x06, 0x1234, 0x5678 + len(self.rels), 1)  # BUS_VIRTUAL
        os.write(fd, name + input_id + struct.pack("I", 0) + b"\0" * (64 * 4 * 4))
        fcntl.ioctl(fd, UI_DEV_CREATE)
        self.fd = fd
        time.sleep(0.3)  # give the compositor time to pick up the new device
        log("uinput device created:", self.name)

    def emit(self, events):
        """events: [(type, code, value)], followed by a SYN_REPORT."""
        self.open()
        now = time.time()
        sec, usec = int(now), int((now - int(now)) * 1e6)
        data = b"".join(INPUT_EVENT.pack(sec, usec, t, c, v) for t, c, v in events)
        os.write(self.fd, data + INPUT_EVENT.pack(sec, usec, EV_SYN, SYN_REPORT, 0))

    def close(self):
        if self.fd is not None:
            try:
                fcntl.ioctl(self.fd, UI_DEV_DESTROY)
            except OSError:
                pass
            os.close(self.fd)
            self.fd = None


class VirtualInput:
    """Virtual keyboard + mouse. Separate devices, so the desktop classifies each correctly."""

    def __init__(self):
        self.lock = threading.Lock()
        self.pressed = set()
        self.keyboard = UinputDevice(
            VIRTUAL_KEYBOARD_NAME, [c for c in range(1, KEY_MAX + 1) if is_keyboard_code(c)])
        self.mouse = UinputDevice(
            VIRTUAL_MOUSE_NAME, MOUSE_BUTTONS, (REL_X, REL_Y, REL_WHEEL, REL_HWHEEL))

    def key(self, code, down):
        """Keyboard key or mouse button (evdev code)."""
        dev = self.mouse if is_mouse_button(code) else self.keyboard
        with self.lock:
            dev.emit([(EV_KEY, code, 1 if down else 0)])
            if down:
                self.pressed.add(code)
            else:
                self.pressed.discard(code)

    def move(self, dx, dy):
        with self.lock:
            self.mouse.emit([(EV_REL, REL_X, int(dx)), (EV_REL, REL_Y, int(dy))])

    def wheel(self, dx, dy):
        events = []
        if dy:
            events.append((EV_REL, REL_WHEEL, int(dy)))
        if dx:
            events.append((EV_REL, REL_HWHEEL, int(dx)))
        if events:
            with self.lock:
                self.mouse.emit(events)

    def release_all(self):
        for code in list(self.pressed):
            try:
                self.key(code, False)
            except OSError:
                pass

    def close(self):
        self.release_all()
        self.keyboard.close()
        self.mouse.close()


def list_devices(mouse=False):
    """Open every evdev device that looks like a keyboard (or a mouse, when asked).
    Returns [(fd, name)]."""
    devices = []
    for path in sorted(glob.glob("/dev/input/event*")):
        try:
            fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK)
        except OSError:
            continue
        try:
            buf = bytearray(256)
            fcntl.ioctl(fd, EVIOCGNAME(len(buf)), buf, True)
            name = bytes(buf).split(b"\0", 1)[0].decode(errors="replace")
            keys = bytearray((KEY_MAX + 8) // 8)
            fcntl.ioctl(fd, EVIOCGBIT(EV_KEY, len(keys)), keys, True)
            rels = bytearray((REL_MAX + 8) // 8)
            fcntl.ioctl(fd, EVIOCGBIT(EV_REL, len(rels)), rels, True)

            def has(bits, code):
                return bool(bits[code // 8] & (1 << (code % 8)))

            # A keyboard has at least a few letter keys (KEY_A, KEY_SPACE, KEY_ENTER);
            # a mouse has relative X/Y axes and a left button.
            is_kbd = has(keys, 30) and has(keys, 57) and has(keys, 28)
            is_mouse = has(rels, REL_X) and has(rels, REL_Y) and has(keys, BTN_LEFT)
            if not name.startswith(VIRTUAL_PREFIX) and (is_kbd or (mouse and is_mouse)):
                devices.append((fd, name))
                continue
        except OSError:
            pass
        os.close(fd)
    return devices


class Recorder(RecorderBase):
    def __init__(self, on_event, on_stop, mouse=False, mouse_move=True):
        super().__init__(on_event, on_stop, mouse, mouse_move)
        self._stop = threading.Event()
        self.devices = []

    def start(self):
        self.devices = list_devices(self.mouse)
        if not self.devices:
            raise InputError("no_keyboards", "/dev/input/event*")
        log("recording on", [d[1] for d in self.devices])
        threading.Thread(target=self._run, daemon=True).start()
        return [d[1] for d in self.devices]

    def stop(self):
        self._stop.set()

    def _run(self):
        fds = [d[0] for d in self.devices]
        try:
            while not self._stop.is_set() and fds:
                ready, _, _ = select.select(fds, [], [], 0.05)
                for fd in ready:
                    try:
                        data = os.read(fd, INPUT_EVENT.size * 64)
                    except OSError:
                        fds.remove(fd)  # device unplugged
                        continue
                    for off in range(0, len(data) - INPUT_EVENT.size + 1, INPUT_EVENT.size):
                        sec, usec, typ, code, value = INPUT_EVENT.unpack_from(data, off)
                        t = sec + usec / 1e6
                        if typ == EV_KEY and value != 2:  # value 2 = auto-repeat
                            self._feed_key(code, value == 1, t)
                        elif typ == EV_REL:
                            # High-resolution wheel events are ignored: REL_WHEEL /
                            # REL_HWHEEL carry the same notches.
                            if code == REL_X:
                                self._feed_move(value, 0, t)
                            elif code == REL_Y:
                                self._feed_move(0, value, t)
                            elif code == REL_WHEEL:
                                self._feed_wheel(0, value, t)
                            elif code == REL_HWHEEL:
                                self._feed_wheel(value, 0, t)
        finally:
            for fd, _ in self.devices:
                try:
                    os.close(fd)
                except OSError:
                    pass
            self._finish()
