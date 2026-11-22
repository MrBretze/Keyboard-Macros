// Renders the key images (one per state) from the appearance settings.
//
// The plugin backend has no graphics library, so the images are drawn here with a
// canvas and stored in the action settings as PNG data URLs (settings.images).
// The backend then simply sends the right one to the device for each state.

const ICON_SIZE = 144; // 2x of the 72 px Stream Deck key, downscaled by the host if needed
const STATE_COUNT = 3; // 0 = idle, 1 = playing, 2 = recording
const DEFAULT_ICONS = ["../icons/state_idle@2x.png", "../icons/state_play@2x.png", "../icons/state_rec@2x.png"];
const FONT_FAMILIES = {
	sans: "system-ui, 'Segoe UI', 'Noto Sans', sans-serif",
	serif: "Georgia, 'Noto Serif', serif",
	mono: "'Cascadia Mono', 'DejaVu Sans Mono', monospace",
};

const APPEARANCE_DEFAULTS = {
	base: "default", // default | image | color
	image: null, // uploaded image, already downscaled (data URL)
	fit: "cover", // cover | contain
	zoom: 100,
	bg: "#1f2430",
	text: "",
	textSize: 30,
	textColor: "#ffffff",
	textPos: "bottom", // top | middle | bottom
	font: "sans",
	bold: true,
	outline: true,
	badge: true,
};

const imageCache = new Map();
function loadImage(src) {
	if (!imageCache.has(src)) {
		imageCache.set(src, new Promise((resolve, reject) => {
			const img = new Image();
			img.onload = () => resolve(img);
			img.onerror = () => { imageCache.delete(src); reject(new Error("load " + src)); };
			img.src = src;
		}));
	}
	return imageCache.get(src);
}

// True when the appearance is the stock one, so no custom image is needed.
function isDefaultAppearance(a) {
	return a.base === "default" && !String(a.text || "").trim();
}

function drawImageFit(ctx, img, fit, zoom) {
	const S = ICON_SIZE;
	const ratio = fit === "contain"
		? Math.min(S / img.width, S / img.height)
		: Math.max(S / img.width, S / img.height);
	const scale = ratio * (zoom / 100);
	const w = img.width * scale, h = img.height * scale;
	ctx.drawImage(img, (S - w) / 2, (S - h) / 2, w, h);
}

function drawBadge(ctx, state) {
	if (state === 0) return;
	const S = ICON_SIZE, r = S * 0.11, cx = S * 0.83, cy = S * 0.17;
	ctx.save();
	ctx.beginPath();
	ctx.arc(cx, cy, r * 1.35, 0, Math.PI * 2);
	ctx.fillStyle = "rgba(0,0,0,0.55)";
	ctx.fill();
	if (state === 1) {
		ctx.beginPath();
		ctx.moveTo(cx - r * 0.6, cy - r * 0.8);
		ctx.lineTo(cx - r * 0.6, cy + r * 0.8);
		ctx.lineTo(cx + r * 0.85, cy);
		ctx.closePath();
		ctx.fillStyle = "#50dc8c";
	} else {
		ctx.beginPath();
		ctx.arc(cx, cy, r * 0.8, 0, Math.PI * 2);
		ctx.fillStyle = "#f0464b";
	}
	ctx.fill();
	ctx.restore();
}

function drawText(ctx, a) {
	const lines = String(a.text || "").replace(/\r/g, "").split("\n");
	if (!lines.join("").trim()) return;
	const S = ICON_SIZE, size = Math.max(6, +a.textSize || 30);
	const lineH = size * 1.15, pad = S * 0.07;
	ctx.save();
	ctx.font = `${a.bold ? "bold " : ""}${size}px ${FONT_FAMILIES[a.font] || FONT_FAMILIES.sans}`;
	ctx.textAlign = "center";
	ctx.textBaseline = "middle";
	ctx.lineJoin = "round";
	const blockH = lineH * lines.length;
	let y0;
	if (a.textPos === "top") y0 = pad + lineH / 2;
	else if (a.textPos === "middle") y0 = (S - blockH) / 2 + lineH / 2;
	else y0 = S - pad - blockH + lineH / 2;
	lines.forEach((line, i) => {
		const y = y0 + i * lineH;
		if (a.outline) {
			ctx.strokeStyle = "rgba(0,0,0,0.85)";
			ctx.lineWidth = Math.max(2, size / 5);
			ctx.strokeText(line, S / 2, y, S - pad);
		}
		ctx.fillStyle = a.textColor || "#ffffff";
		ctx.fillText(line, S / 2, y, S - pad);
	});
	ctx.restore();
}

// Draws one state into the given canvas.
async function renderState(canvas, a, state) {
	canvas.width = canvas.height = ICON_SIZE;
	const ctx = canvas.getContext("2d");
	ctx.clearRect(0, 0, ICON_SIZE, ICON_SIZE);
	if (a.base === "default") {
		ctx.drawImage(await loadImage(DEFAULT_ICONS[state]), 0, 0, ICON_SIZE, ICON_SIZE);
	} else {
		ctx.fillStyle = a.bg || "#000000";
		ctx.fillRect(0, 0, ICON_SIZE, ICON_SIZE);
		if (a.base === "image" && a.image) drawImageFit(ctx, await loadImage(a.image), a.fit, +a.zoom || 100);
	}
	drawText(ctx, a);
	// The default icons already include their own state indicator.
	if (a.base !== "default" && a.badge) drawBadge(ctx, state);
}

// Returns {"0": dataURL, "1": ..., "2": ...}, or {} for the stock appearance.
async function renderAllStates(a) {
	if (isDefaultAppearance(a)) return {};
	const out = {};
	const canvas = document.createElement("canvas");
	for (let s = 0; s < STATE_COUNT; s++) {
		await renderState(canvas, a, s);
		out[String(s)] = canvas.toDataURL("image/png");
	}
	return out;
}

// Reads an image file and downscales it so the settings stay small.
function readImageFile(file, maxSize = 288) {
	return new Promise((resolve, reject) => {
		const reader = new FileReader();
		reader.onerror = () => reject(reader.error);
		reader.onload = async () => {
			try {
				const img = await loadImage(reader.result);
				const scale = Math.min(1, maxSize / Math.max(img.width, img.height));
				const c = document.createElement("canvas");
				c.width = Math.round(img.width * scale);
				c.height = Math.round(img.height * scale);
				c.getContext("2d").drawImage(img, 0, 0, c.width, c.height);
				imageCache.delete(reader.result);
				resolve(c.toDataURL("image/png"));
			} catch (e) { reject(e); }
		};
		reader.readAsDataURL(file);
	});
}
