// Translation catalogues live in i18n/<language>.json. To add a language, add
// its catalogue and list it in SUPPORTED_LANGUAGES.
const SUPPORTED_LANGUAGES = ["en"];
const FALLBACK_LANGUAGE = "en";

let catalogue = {};
let language = FALLBACK_LANGUAGE;

export async function loadTranslations() {
  const preferred = navigator.language.split("-")[0];
  language = SUPPORTED_LANGUAGES.includes(preferred) ? preferred : FALLBACK_LANGUAGE;
  const response = await fetch(`i18n/${language}.json`);
  catalogue = await response.json();
  document.documentElement.lang = language;
}

function fill(template, values) {
  return template.replace(/\{(\w+)\}/g, (match, key) => values[key] ?? match);
}

export function t(key, values = {}) {
  return fill(catalogue[key] ?? key, values);
}

export function tn(key, count, values = {}) {
  const form = new Intl.PluralRules(language).select(count) === "one" ? "one" : "other";
  return fill(catalogue[`${key}_${form}`] ?? key, { count, ...values });
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

export function formatAmount(amount, currency) {
  if (!amount) return "–";
  const value = Number(amount);
  if (Number.isNaN(value)) return amount;
  if (!currency) return new Intl.NumberFormat(language, { minimumFractionDigits: 2 }).format(value);
  return new Intl.NumberFormat(language, { style: "currency", currency }).format(value);
}
