// Translation catalogues live in i18n/<language>.json. To add a language, add
// its catalogue, list it in SUPPORTED_LANGUAGES and give it a name under
// settings.language.<code>. src/i18n.rs needs it too, for the tray.
import { api } from "./api.js";

const SUPPORTED_LANGUAGES = ["en", "de"];
const FALLBACK_LANGUAGE = "en";
// "auto" follows the system. The choice is a convenience of this computer, so it
// lives in the web view's storage, which may be unavailable.
const CHOICE_KEY = "docsweep.language";

let catalogue = {};
let fallback = {};
let language = FALLBACK_LANGUAGE;

export const languageChoices = ["auto", ...SUPPORTED_LANGUAGES];

export function languageChoice() {
  try {
    const stored = localStorage.getItem(CHOICE_KEY);
    return languageChoices.includes(stored) ? stored : "auto";
  } catch {
    return "auto";
  }
}

export function setLanguageChoice(choice) {
  try {
    if (choice === "auto") localStorage.removeItem(CHOICE_KEY);
    else localStorage.setItem(CHOICE_KEY, choice);
  } catch {
    // Without storage the choice lasts until the window reloads.
  }
}

async function loadCatalogue(code) {
  const response = await fetch(`i18n/${code}.json`);
  return response.json();
}

export async function loadTranslations(choice = languageChoice()) {
  const preferred = choice === "auto" ? navigator.language.split("-")[0] : choice;
  language = SUPPORTED_LANGUAGES.includes(preferred) ? preferred : FALLBACK_LANGUAGE;
  fallback = await loadCatalogue(FALLBACK_LANGUAGE);
  catalogue = language === FALLBACK_LANGUAGE ? fallback : await loadCatalogue(language);
  document.documentElement.lang = language;
  try {
    api.setLanguage(language).catch(() => {});
  } catch {
    // The tray keeps its language; the window is what matters.
  }
}

function fill(template, values) {
  return template.replace(/\{(\w+)\}/g, (match, key) => values[key] ?? match);
}

function lookup(key) {
  return catalogue[key] ?? fallback[key];
}

export function t(key, values = {}) {
  return fill(lookup(key) ?? key, values);
}

export function tn(key, count, values = {}) {
  const form = new Intl.PluralRules(language).select(count) === "one" ? "one" : "other";
  return fill(lookup(`${key}_${form}`) ?? key, { count, ...values });
}

export function formatDate(iso) {
  if (!iso) return "–";
  // A plain date has no time zone; noon keeps it on the same day everywhere.
  const date = new Date(iso.length === 10 ? `${iso}T12:00:00` : iso);
  return new Intl.DateTimeFormat(language, { dateStyle: "medium" }).format(date);
}

export function formatDateTime(iso) {
  return new Intl.DateTimeFormat(language, { dateStyle: "medium", timeStyle: "short" }).format(new Date(iso));
}

// The name of a cron weekday: 0 and 7 are Sunday.
export function weekdayName(number) {
  // 4 January 2026 is a Sunday.
  const date = new Date(2026, 0, 4 + (number % 7));
  return new Intl.DateTimeFormat(language, { weekday: "long" }).format(date);
}

export function formatAmount(amount, currency) {
  if (!amount) return "–";
  const value = Number(amount);
  if (Number.isNaN(value)) return amount;
  if (!currency) return new Intl.NumberFormat(language, { minimumFractionDigits: 2 }).format(value);
  return new Intl.NumberFormat(language, { style: "currency", currency }).format(value);
}
