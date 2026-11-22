// Minimal translation engine for the property inspector.
//
// Each file in locales/ registers a dictionary: I18N.<lang> = { key: "text", ... }.
// To add a language: copy locales/en.js, translate the values, add a <script> tag in
// index.html and an <option> in the language selector.
//
// Placeholders use braces: t("status.recorded", { n: 3 }) -> "3 events recorded."

const I18N = {};
const FALLBACK_LANG = "en";
const LANG_STORAGE_KEY = "keymacro.lang";
let currentLang = FALLBACK_LANG;

// Picks the language: user override (stored), then the host application language,
// then the browser / system language, then English.
function detectLanguage(info) {
	const candidates = [];
	try {
		const stored = localStorage.getItem(LANG_STORAGE_KEY);
		if (stored && stored !== "auto") candidates.push(stored);
	} catch (e) { /* storage may be unavailable */ }
	if (info && info.application && info.application.language) candidates.push(info.application.language);
	for (const l of navigator.languages || [navigator.language]) candidates.push(l);
	for (const c of candidates) {
		const short = String(c).toLowerCase().split(/[-_]/)[0];
		if (I18N[short]) return short;
	}
	return FALLBACK_LANG;
}

function getStoredLanguage() {
	try { return localStorage.getItem(LANG_STORAGE_KEY) || "auto"; } catch (e) { return "auto"; }
}

function storeLanguage(lang) {
	try { localStorage.setItem(LANG_STORAGE_KEY, lang); } catch (e) { /* ignore */ }
}

function setLanguage(lang) {
	currentLang = I18N[lang] ? lang : FALLBACK_LANG;
	document.documentElement.lang = currentLang;
	applyTranslations(document);
}

function t(key, params) {
	const dict = I18N[currentLang] || {};
	let s = dict[key] ?? (I18N[FALLBACK_LANG] || {})[key] ?? key;
	if (params) s = s.replace(/\{(\w+)\}/g, (m, k) => (k in params ? params[k] : m));
	return s;
}

// Translates static markup: data-i18n (text), data-i18n-title, data-i18n-placeholder.
function applyTranslations(root) {
	root.querySelectorAll("[data-i18n]").forEach((el) => (el.textContent = t(el.dataset.i18n)));
	root.querySelectorAll("[data-i18n-title]").forEach((el) => (el.title = t(el.dataset.i18nTitle)));
	root.querySelectorAll("[data-i18n-placeholder]").forEach((el) => (el.placeholder = t(el.dataset.i18nPlaceholder)));
}
