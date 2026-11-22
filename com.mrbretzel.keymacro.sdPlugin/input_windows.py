"""
Windows input backend (ctypes only, no third-party package).

- Playback: SendInput with hardware scan codes, so macros replay physical keys
  regardless of the active keyboard layout (same behaviour as the Linux backend).
  Mouse buttons and wheel go through SendInput; movements move the cursor directly.
- Recording: low-level keyboard and mouse hooks (WH_KEYBOARD_LL / WH_MOUSE_LL).
  Injected events, including our own playback, are ignored.

Macro events store Linux evdev codes. For keys of the main block, evdev codes are
identical to PC scan codes (set 1); other keys are translated with the tables below.
"""

import ctypes
import threading
from ctypes import wintypes

from input_common import (
    BTN_EXTRA, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, BTN_SIDE, InputError, RecorderBase, log,
)

user32 = ctypes.WinDLL("user32", use_last_error=True)
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

INPUT_KEYBOARD = 1
KEYEVENTF_EXTENDEDKEY = 0x0001
KEYEVENTF_KEYUP = 0x0002
KEYEVENTF_SCANCODE = 0x0008

WH_KEYBOARD_LL = 13
WM_QUIT = 0x0012
WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP = 0x0100, 0x0101, 0x0104, 0x0105
LLKHF_EXTENDED = 0x01
LLKHF_INJECTED = 0x10

ULONG_PTR = ctypes.c_size_t
LRESULT = ctypes.c_ssize_t

# evdev code -> (scan code, extended) for keys that differ from the plain scan code.
SCAN_EXT = {
    69: (0x45, True),    # NUMLOCK (reported as extended by Windows)
    96: (0x1C, True),    # KPENTER
    97: (0x1D, True),    # RIGHTCTRL
    98: (0x35, True),    # KPSLASH
    99: (0x37, True),    # SYSRQ / Print Screen
    100: (0x38, True),   # RIGHTALT / AltGr
    102: (0x47, True),   # HOME
    103: (0x48, True),   # UP
    104: (0x49, True),   # PAGEUP
    105: (0x4B, True),   # LEFT
    106: (0x4D, True),   # RIGHT
    107: (0x4F, True),   # END
    108: (0x50, True),   # DOWN
    109: (0x51, True),   # PAGEDOWN
    110: (0x52, True),   # INSERT
    111: (0x53, True),   # DELETE
    116: (0x5E, True),   # POWER
    117: (0x59, False),  # KPEQUAL
    125: (0x5B, True),   # LEFTMETA
    126: (0x5C, True),   # RIGHTMETA
    127: (0x5D, True),   # COMPOSE / Menu
    183: (0x64, False), 184: (0x65, False), 185: (0x66, False), 186: (0x67, False),  # F13-F16
    187: (0x68, False), 188: (0x69, False), 189: (0x6A, False), 190: (0x6B, False),  # F17-F20
    191: (0x6C, False), 192: (0x6D, False), 193: (0x6E, False), 194: (0x76, False),  # F21-F24
}

# evdev code -> virtual-key code, for keys that are only reliable as virtual keys.
VK_KEYS = {
    113: 0xAD,  # MUTE
    114: 0xAE,  # VOLUMEDOWN
    115: 0xAF,  # VOLUMEUP
    119: 0x13,  # PAUSE
    140: 0xB7,  # CALC (VK_LAUNCH_APP2)
    158: 0xA6,  # BACK (VK_BROWSER_BACK)
    159: 0xA7,  # FORWARD (VK_BROWSER_FORWARD)
    163: 0xB0,  # NEXTSONG
    164: 0xB3,  # PLAYPAUSE
    165: 0xB1,  # PREVIOUSSONG
    166: 0xB2,  # STOPCD
}
# Not supported on Windows (no standard key): BRIGHTNESSDOWN (224), BRIGHTNESSUP (225).
VK_EXTENDED = {0xAD, 0xAE, 0xAF, 0xB0, 0xB1, 0xB2, 0xB3, 0xA6, 0xA7, 0xB7}

REVERSE_SCAN = {v: k for k, v in SCAN_EXT.items()}
REVERSE_VK = {v: k for k, v in VK_KEYS.items()}
DIRECT_SCAN_MAX = 88  # evdev 1..88 == scan code 0x01..0x58 (non-extended)


def evdev_to_windows(code):
    """Returns ("vk", vk) or ("scan", scan, extended), or None if unsupported."""
    if code in VK_KEYS:
        return ("vk", VK_KEYS[code])
    if code in SCAN_EXT:
        scan, ext = SCAN_EXT[code]
        return ("scan", scan, ext)
    if 1 <= code <= DIRECT_SCAN_MAX:
        return ("scan", code, False)
    return None


def windows_to_evdev(vk, scan, extended):
    if vk in REVERSE_VK:
        return REVERSE_VK[vk]
    if vk == 0x90:  # VK_NUMLOCK: Windows reports scan 0x45 extended
        return 69
    if (scan, extended) in REVERSE_SCAN:
        return REVERSE_SCAN[(scan, extended)]
    if not extended and 1 <= scan <= DIRECT_SCAN_MAX:
        return scan
    return None


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [("wVk", wintypes.WORD), ("wScan", wintypes.WORD), ("dwFlags", wintypes.DWORD),
                ("time", wintypes.DWORD), ("dwExtraInfo", ULONG_PTR)]


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [("dx", wintypes.LONG), ("dy", wintypes.LONG), ("mouseData", wintypes.DWORD),
                ("dwFlags", wintypes.DWORD), ("time", wintypes.DWORD), ("dwExtraInfo", ULONG_PTR)]


class HARDWAREINPUT(ctypes.Structure):
    _fields_ = [("uMsg", wintypes.DWORD), ("wParamL", wintypes.WORD), ("wParamH", wintypes.WORD)]


class _INPUTUNION(ctypes.Union):
    _fields_ = [("ki", KEYBDINPUT), ("mi", MOUSEINPUT), ("hi", HARDWAREINPUT)]


class INPUT(ctypes.Structure):
    _fields_ = [("type", wintypes.DWORD), ("u", _INPUTUNION)]


class KBDLLHOOKSTRUCT(ctypes.Structure):
    _fields_ = [("vkCode", wintypes.DWORD), ("scanCode", wintypes.DWORD), ("flags", wintypes.DWORD),
                ("time", wintypes.DWORD), ("dwExtraInfo", ULONG_PTR)]


HOOKPROC = ctypes.WINFUNCTYPE(LRESULT, ctypes.c_int, wintypes.WPARAM, wintypes.LPARAM)

user32.SendInput.argtypes = (wintypes.UINT, ctypes.POINTER(INPUT), ctypes.c_int)
user32.SendInput.restype = wintypes.UINT
user32.SetWindowsHookExW.argtypes = (ctypes.c_int, HOOKPROC, wintypes.HINSTANCE, wintypes.DWORD)
user32.SetWindowsHookExW.restype = wintypes.HHOOK
user32.CallNextHookEx.argtypes = (wintypes.HHOOK, ctypes.c_int, wintypes.WPARAM, wintypes.LPARAM)
user32.CallNextHookEx.restype = LRESULT
user32.UnhookWindowsHookEx.argtypes = (wintypes.HHOOK,)
user32.GetMessageW.argtypes = (ctypes.POINTER(wintypes.MSG), wintypes.HWND, wintypes.UINT, wintypes.UINT)
user32.PostThreadMessageW.argtypes = (wintypes.DWORD, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM)
kernel32.GetModuleHandleW.argtypes = (wintypes.LPCWSTR,)
kernel32.GetModuleHandleW.restype = wintypes.HMODULE


class POINT(ctypes.Structure):
    _fields_ = [("x", wintypes.LONG), ("y", wintypes.LONG)]


class MSLLHOOKSTRUCT(ctypes.Structure):
    _fields_ = [("pt", POINT), ("mouseData", wintypes.DWORD), ("flags", wintypes.DWORD),
                ("time", wintypes.DWORD), ("dwExtraInfo", ULONG_PTR)]


user32.GetCursorPos.argtypes = (ctypes.POINTER(POINT),)
user32.SetCursorPos.argtypes = (ctypes.c_int, ctypes.c_int)

INPUT_MOUSE = 0
MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP = 0x0002, 0x0004
MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP = 0x0008, 0x0010
MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP = 0x0020, 0x0040
MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP = 0x0080, 0x0100
MOUSEEVENTF_WHEEL, MOUSEEVENTF_HWHEEL = 0x0800, 0x1000
XBUTTON1, XBUTTON2 = 0x0001, 0x0002
WHEEL_DELTA = 120

WH_MOUSE_LL = 14
WM_MOUSEMOVE = 0x0200
WM_LBUTTONDOWN, WM_LBUTTONUP = 0x0201, 0x0202
WM_RBUTTONDOWN, WM_RBUTTONUP = 0x0204, 0x0205
WM_MBUTTONDOWN, WM_MBUTTONUP = 0x0207, 0x0208
WM_MOUSEWHEEL, WM_XBUTTONDOWN, WM_XBUTTONUP, WM_MOUSEHWHEEL = 0x020A, 0x020B, 0x020C, 0x020E
LLMHF_INJECTED = 0x01

# evdev mouse button -> (down flag, up flag, mouseData)
MOUSE_BUTTON_FLAGS = {
    BTN_LEFT: (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
    BTN_RIGHT: (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
    BTN_MIDDLE: (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
    BTN_SIDE: (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON1),
    BTN_EXTRA: (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON2),
}
# Hook message -> (evdev button, down); X buttons are resolved from mouseData.
MOUSE_MESSAGES = {
    WM_LBUTTONDOWN: (BTN_LEFT, True), WM_LBUTTONUP: (BTN_LEFT, False),
    WM_RBUTTONDOWN: (BTN_RIGHT, True), WM_RBUTTONUP: (BTN_RIGHT, False),
    WM_MBUTTONDOWN: (BTN_MIDDLE, True), WM_MBUTTONUP: (BTN_MIDDLE, False),
}


class VirtualInput:
    """Keyboard and mouse simulation through SendInput."""

    def __init__(self):
        self.lock = threading.Lock()
        self.pressed = set()

    def _send(self, inp):
        if user32.SendInput(1, ctypes.byref(inp), ctypes.sizeof(INPUT)) != 1:
            raise InputError("sendinput_failed", str(ctypes.get_last_error()))

    def _mouse(self, flags, data=0):
        inp = INPUT(type=INPUT_MOUSE)
        inp.u.mi.dwFlags = flags
        inp.u.mi.mouseData = data & 0xFFFFFFFF  # DWORD: negative wheel values wrap
        self._send(inp)

    def key(self, code, down):
        """Keyboard key or mouse button (evdev code)."""
        if code in MOUSE_BUTTON_FLAGS:
            down_flag, up_flag, data = MOUSE_BUTTON_FLAGS[code]
            with self.lock:
                self._mouse(down_flag if down else up_flag, data)
                (self.pressed.add if down else self.pressed.discard)(code)
            return
        target = evdev_to_windows(code)
        if target is None:
            log("unsupported key on Windows:", code)
            return
        inp = INPUT(type=INPUT_KEYBOARD)
        flags = 0 if down else KEYEVENTF_KEYUP
        if target[0] == "vk":
            inp.u.ki.wVk = target[1]
            if target[1] in VK_EXTENDED:
                flags |= KEYEVENTF_EXTENDEDKEY
        else:
            inp.u.ki.wScan = target[1]
            flags |= KEYEVENTF_SCANCODE
            if target[2]:
                flags |= KEYEVENTF_EXTENDEDKEY
        inp.u.ki.dwFlags = flags
        with self.lock:
            self._send(inp)
            (self.pressed.add if down else self.pressed.discard)(code)

    def move(self, dx, dy):
        # The cursor is moved directly (no pointer acceleration), so a replay lands
        # exactly where the recording went.
        with self.lock:
            pt = POINT()
            user32.GetCursorPos(ctypes.byref(pt))
            user32.SetCursorPos(pt.x + int(dx), pt.y + int(dy))

    def wheel(self, dx, dy):
        with self.lock:
            if dy:
                self._mouse(MOUSEEVENTF_WHEEL, int(dy) * WHEEL_DELTA)
            if dx:
                self._mouse(MOUSEEVENTF_HWHEEL, int(dx) * WHEEL_DELTA)

    def release_all(self):
        for code in list(self.pressed):
            try:
                self.key(code, False)
            except InputError:
                pass

    def close(self):
        self.release_all()


class Recorder(RecorderBase):
    def __init__(self, on_event, on_stop, mouse=False, mouse_move=True):
        super().__init__(on_event, on_stop, mouse, mouse_move)
        self._thread_id = None
        self._ready = threading.Event()
        self._error = None
        self._last_pt = None
        self._wheel_rest = [0, 0]  # sub-notch wheel amounts (high-resolution wheels)

    def start(self):
        threading.Thread(target=self._run, daemon=True).start()
        self._ready.wait(2)
        if self._error:
            raise InputError("hook_failed", self._error)
        return ["Windows keyboard hook"] + (["Windows mouse hook"] if self.mouse else [])

    def stop(self):
        if self._thread_id:
            user32.PostThreadMessageW(self._thread_id, WM_QUIT, 0, 0)

    def _on_key(self, n_code, w_param, l_param):
        if n_code == 0 and w_param in (WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP):
            k = ctypes.cast(l_param, ctypes.POINTER(KBDLLHOOKSTRUCT)).contents
            if not k.flags & LLKHF_INJECTED:
                code = windows_to_evdev(k.vkCode, k.scanCode, bool(k.flags & LLKHF_EXTENDED))
                if code is not None:
                    self._feed_key(code, w_param in (WM_KEYDOWN, WM_SYSKEYDOWN), k.time / 1000.0)

    def _on_mouse(self, n_code, w_param, l_param):
        if n_code != 0:
            return
        m = ctypes.cast(l_param, ctypes.POINTER(MSLLHOOKSTRUCT)).contents
        if m.flags & LLMHF_INJECTED:
            return
        t = m.time / 1000.0
        high = ctypes.c_short((m.mouseData >> 16) & 0xFFFF).value  # signed high word
        if w_param == WM_MOUSEMOVE:
            if self._last_pt is not None:
                self._feed_move(m.pt.x - self._last_pt[0], m.pt.y - self._last_pt[1], t)
            self._last_pt = (m.pt.x, m.pt.y)
        elif w_param in MOUSE_MESSAGES:
            code, down = MOUSE_MESSAGES[w_param]
            self._feed_key(code, down, t)
        elif w_param in (WM_XBUTTONDOWN, WM_XBUTTONUP):
            code = BTN_SIDE if high == XBUTTON1 else BTN_EXTRA
            self._feed_key(code, w_param == WM_XBUTTONDOWN, t)
        elif w_param in (WM_MOUSEWHEEL, WM_MOUSEHWHEEL):
            axis = 1 if w_param == WM_MOUSEWHEEL else 0
            self._wheel_rest[axis] += high
            notches = int(self._wheel_rest[axis] / WHEEL_DELTA)
            if notches:
                self._wheel_rest[axis] -= notches * WHEEL_DELTA
                self._feed_wheel(notches if axis == 0 else 0, notches if axis == 1 else 0, t)

    def _run(self):
        self._thread_id = kernel32.GetCurrentThreadId()

        def make_proc(handler):
            def proc(n_code, w_param, l_param):
                try:
                    handler(n_code, w_param, l_param)
                except Exception as e:  # never let an exception escape a hook
                    log("hook error", e)
                return user32.CallNextHookEx(None, n_code, w_param, l_param)
            return HOOKPROC(proc)

        # Keep references to the callbacks, or they get garbage collected.
        self._procs = [make_proc(self._on_key)]
        module = kernel32.GetModuleHandleW(None)
        hooks = [user32.SetWindowsHookExW(WH_KEYBOARD_LL, self._procs[0], module, 0)]
        if self.mouse:
            self._procs.append(make_proc(self._on_mouse))
            hooks.append(user32.SetWindowsHookExW(WH_MOUSE_LL, self._procs[1], module, 0))
        if not all(hooks):
            self._error = str(ctypes.get_last_error())
            for h in hooks:
                if h:
                    user32.UnhookWindowsHookEx(h)
            self._ready.set()
            return
        self._ready.set()
        log("input hooks installed:", len(hooks))
        try:
            msg = wintypes.MSG()
            while user32.GetMessageW(ctypes.byref(msg), None, 0, 0) > 0:
                pass
        finally:
            for h in hooks:
                user32.UnhookWindowsHookEx(h)
            self._finish()
