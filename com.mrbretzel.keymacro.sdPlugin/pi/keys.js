// Linux evdev key codes (physical keys), used as the macro format on every platform.
// Names follow the kernel's US QWERTY naming (mouse buttons keep their BTN_ prefix); AZERTY_LABELS gives the label printed
// on a French keyboard. Friendly names of special keys come from the translations (key.*).
const KEYS = {
	1: "ESC", 2: "1", 3: "2", 4: "3", 5: "4", 6: "5", 7: "6", 8: "7", 9: "8", 10: "9", 11: "0",
	12: "MINUS", 13: "EQUAL", 14: "BACKSPACE", 15: "TAB",
	16: "Q", 17: "W", 18: "E", 19: "R", 20: "T", 21: "Y", 22: "U", 23: "I", 24: "O", 25: "P",
	26: "LEFTBRACE", 27: "RIGHTBRACE", 28: "ENTER", 29: "LEFTCTRL",
	30: "A", 31: "S", 32: "D", 33: "F", 34: "G", 35: "H", 36: "J", 37: "K", 38: "L",
	39: "SEMICOLON", 40: "APOSTROPHE", 41: "GRAVE", 42: "LEFTSHIFT", 43: "BACKSLASH",
	44: "Z", 45: "X", 46: "C", 47: "V", 48: "B", 49: "N", 50: "M",
	51: "COMMA", 52: "DOT", 53: "SLASH", 54: "RIGHTSHIFT", 55: "KPASTERISK", 56: "LEFTALT",
	57: "SPACE", 58: "CAPSLOCK",
	59: "F1", 60: "F2", 61: "F3", 62: "F4", 63: "F5", 64: "F6", 65: "F7", 66: "F8", 67: "F9", 68: "F10",
	69: "NUMLOCK", 70: "SCROLLLOCK",
	71: "KP7", 72: "KP8", 73: "KP9", 74: "KPMINUS", 75: "KP4", 76: "KP5", 77: "KP6", 78: "KPPLUS",
	79: "KP1", 80: "KP2", 81: "KP3", 82: "KP0", 83: "KPDOT", 86: "102ND", 87: "F11", 88: "F12",
	96: "KPENTER", 97: "RIGHTCTRL", 98: "KPSLASH", 99: "SYSRQ", 100: "RIGHTALT",
	102: "HOME", 103: "UP", 104: "PAGEUP", 105: "LEFT", 106: "RIGHT", 107: "END", 108: "DOWN",
	109: "PAGEDOWN", 110: "INSERT", 111: "DELETE", 113: "MUTE", 114: "VOLUMEDOWN", 115: "VOLUMEUP",
	116: "POWER", 117: "KPEQUAL", 119: "PAUSE", 125: "LEFTMETA", 126: "RIGHTMETA", 127: "COMPOSE",
	140: "CALC", 158: "BACK", 159: "FORWARD",
	163: "NEXTSONG", 164: "PLAYPAUSE", 165: "PREVIOUSSONG", 166: "STOPCD",
	183: "F13", 184: "F14", 185: "F15", 186: "F16", 187: "F17", 188: "F18",
	189: "F19", 190: "F20", 191: "F21", 192: "F22", 193: "F23", 194: "F24",
	224: "BRIGHTNESSDOWN", 225: "BRIGHTNESSUP",
	// Mouse buttons: recorded and replayed like keys (press / release events).
	272: "BTN_LEFT", 273: "BTN_RIGHT", 274: "BTN_MIDDLE", 275: "BTN_SIDE", 276: "BTN_EXTRA",
};

const AZERTY_LABELS = {
	2: "& 1", 3: "é 2", 4: "\" 3", 5: "' 4", 6: "( 5", 7: "- 6", 8: "è 7", 9: "_ 8", 10: "ç 9", 11: "à 0",
	12: ") °", 13: "= +", 16: "A", 17: "Z", 26: "^ ¨", 27: "$ £", 30: "Q", 39: "M", 40: "ù %",
	41: "²", 43: "* µ", 44: "W", 50: ", ?", 51: "; .", 52: ": /", 53: "! §", 86: "< >",
};

// Operating system the macro runs on: "linux", "windows" or "mac". Some keys have a
// platform-specific name (Super / Win / ⌘ Command...), looked up as "key.<NAME>.<platform>"
// in the translations before the generic "key.<NAME>".
let currentPlatform = "linux";

function normalizePlatform(value) {
	const v = String(value || "").toLowerCase();
	if (v.startsWith("mac") || v === "darwin" || v === "osx") return "mac";
	if (v.startsWith("win")) return "windows";
	return "linux";
}

function setPlatform(value) {
	currentPlatform = normalizePlatform(value);
	document.documentElement.dataset.platform = currentPlatform;
}

function keyLabel(code, layout, platform = currentPlatform) {
	const name = KEYS[code];
	if (!name) return "#" + code;
	if (layout === "azerty" && AZERTY_LABELS[code]) return AZERTY_LABELS[code];
	if (typeof t !== "function") return name;
	for (const key of ["key." + name + "." + platform, "key." + name]) {
		const label = t(key);
		if (label !== key) return label;
	}
	return name;
}

function keyName(code) {
	const name = KEYS[code];
	if (!name) return "KEY_" + code;
	return name.startsWith("BTN_") ? name : "KEY_" + name;
}

// Resolves user input ("a", "KEY_A", "Enter", "30", "Left Ctrl"...) to a key code, or null.
function parseKey(text, layout) {
	const input = String(text).trim();
	if (!input) return null;
	if (/^\d+$/.test(input)) return parseInt(input, 10);
	const up = input.toUpperCase().replace(/^KEY_/, "");
	const codes = Object.keys(KEYS).map(Number);
	const wanted = input.toUpperCase();
	// Current platform's names first, then the other platforms' ("Alt" still works on a Mac).
	const platforms = [currentPlatform, ...["linux", "windows", "mac"].filter((p) => p !== currentPlatform)];
	const byLabel = (platform) => codes.find((c) => keyLabel(c, layout, platform).toUpperCase() === wanted);
	return (
		platforms.reduce((found, platform) => found ?? byLabel(platform), undefined) ??
		codes.find((c) => layout === "azerty" && AZERTY_LABELS[c] && AZERTY_LABELS[c].split(" ").some((p) => p.toUpperCase() === input.toUpperCase())) ??
		codes.find((c) => KEYS[c] === up) ??
		null
	);
}
