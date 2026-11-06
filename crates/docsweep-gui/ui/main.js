// Translation catalogues live in i18n/<language>.json. To add a language,
// add its catalogue and list it in SUPPORTED_LANGUAGES.
const SUPPORTED_LANGUAGES = ["en"];
const FALLBACK_LANGUAGE = "en";

function pickLanguage() {
  const preferred = navigator.language.split("-")[0];
  return SUPPORTED_LANGUAGES.includes(preferred) ? preferred : FALLBACK_LANGUAGE;
}

async function loadCatalogue(language) {
  const response = await fetch(`i18n/${language}.json`);
  return response.json();
}

function format(template, values) {
  return template.replace(/\{(\w+)\}/g, (_, key) => values[key] ?? "");
}

async function start() {
  const language = pickLanguage();
  const catalogue = await loadCatalogue(language);
  document.documentElement.lang = language;

  for (const element of document.querySelectorAll("[data-i18n]")) {
    element.textContent = catalogue[element.dataset.i18n] ?? element.dataset.i18n;
  }

  const info = await window.__TAURI__.core.invoke("get_app_info");
  document.getElementById("version").textContent = format(catalogue["app.version"], info);
}

start();
