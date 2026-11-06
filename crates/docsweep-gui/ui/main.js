import { api, errorText, onTrayPreview } from "./lib/api.js";
import { h, icon, mark, replace } from "./lib/dom.js";
import { loadTranslations, t } from "./lib/i18n.js";
import { notice } from "./lib/ui.js";
import { renderCompanies } from "./views/companies.js";
import { renderHistory } from "./views/history.js";
import { renderInvoices } from "./views/invoices.js";
import { renderMailboxes } from "./views/mailboxes.js";
import { renderScan } from "./views/scan.js";
import { renderSettings } from "./views/settings.js";
import { renderBrokenConfig, renderSetup } from "./views/setup.js";

const VIEWS = {
  scan: renderScan,
  invoices: renderInvoices,
  companies: renderCompanies,
  mailboxes: renderMailboxes,
  history: renderHistory,
  settings: renderSettings,
};

const root = document.getElementById("app");
const content = h("main", { class: "content" });
const reviewBadge = h("span", { class: "nav-badge", hidden: true });
let activeView = "scan";
let info = {};

// Shared with the views.
const app = {
  config: null,
  passwords: new Map(),
  setConfig(config) {
    const firstConfig = !app.config;
    app.config = config;
    if (firstConfig) showShell();
    paintNav();
  },
  isPasswordStored: (id) => app.passwords.get(id) === true,
  setPasswordStored: (id, stored) => app.passwords.set(id, stored),
  navigate,
  async refreshBadges() {
    try {
      const invoices = await api.listInvoices();
      const count = invoices.filter((row) => row.status === "needs_review").length;
      reviewBadge.hidden = count === 0;
      reviewBadge.textContent = count;
    } catch {
      reviewBadge.hidden = true;
    }
  },
};

const nav = h("nav", { class: "nav" });

function paintNav() {
  replace(nav, Object.keys(VIEWS).map((name) =>
    h("button", {
      class: `nav-item${name === activeView ? " is-active" : ""}`,
      "aria-current": name === activeView ? "page" : undefined,
      onclick: () => navigate(name),
    }, icon(name), h("span", {}, t(`nav.${name}`)), name === "invoices" && reviewBadge)));
}

function navigate(view, options) {
  activeView = view;
  paintNav();
  replace(content, VIEWS[view](app, options));
  content.scrollTop = 0;
}

function showShell() {
  replace(root,
    h("div", { class: "shell" },
      h("aside", { class: "sidebar" },
        h("div", { class: "brand" }, mark(), h("span", {}, info.name)),
        nav,
        h("div", { class: "sidebar-foot" }, t("nav.version", { version: info.version })),
      ),
      content,
    ));
  navigate(activeView);
  app.refreshBadges();
}

async function start() {
  await loadTranslations();
  let state;
  try {
    state = await api.getState();
  } catch (error) {
    replace(root, h("div", { class: "setup" }, notice("danger", errorText(error))));
    return;
  }
  info = state.info;
  app.info = state.info;
  app.configPath = state.config_path;
  app.passwords = new Map(state.stored_passwords);

  onTrayPreview(() => {
    if (!app.config) return;
    navigate("scan");
    app.startPreview?.();
  });

  if (state.config_error) {
    replace(root, renderBrokenConfig(state.config_error, state.config_path));
  } else if (state.config) {
    app.setConfig(state.config);
  } else {
    replace(root, renderSetup(app));
  }
}

start();
