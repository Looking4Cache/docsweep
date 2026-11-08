import { api, errorText, onAutomationStatus, onTrayPreview } from "./lib/api.js";
import { h, icon, mark, replace } from "./lib/dom.js";
import { loadTranslations, t } from "./lib/i18n.js";
import { watchPickups } from "./lib/pickup.js";
import { notice, watchPortalConfirmations } from "./lib/ui.js";
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
const problemBadge = h("span", { class: "nav-badge nav-badge-danger", hidden: true });
const pickupBadge = h("span", { class: "nav-badge", hidden: true });
const NAV_BADGES = { scan: pickupBadge, invoices: reviewBadge, history: problemBadge };
// The newest run someone has seen in the history. Only a convenience: without
// storage, old problems simply show again.
const HISTORY_SEEN_KEY = "docsweep.history-seen";
let activeView = "scan";
let info = {};

function historySeen() {
  try {
    return Number(localStorage.getItem(HISTORY_SEEN_KEY)) || 0;
  } catch {
    return 0;
  }
}

// Shared with the views.
const app = {
  config: null,
  passwords: new Map(),
  setConfig(config) {
    const firstConfig = !app.config;
    app.config = config;
    if (firstConfig) showShell();
    else app.refreshBadges();
    paintNav();
  },
  isPasswordStored: (id) => app.passwords.get(id) === true,
  setPasswordStored: (id, stored) => app.passwords.set(id, stored),
  portalSecrets: new Map(),
  portalSecretsOf: (id) => app.portalSecrets.get(id) ?? { credentials: false, totp: false },
  setPortalSecretStored: (id, kind) => app.portalSecrets.set(id, { ...app.portalSecretsOf(id), [kind]: true }),
  navigate,
  // Whether an automatic run is active, and those with problems nobody has seen.
  automationRunning: false,
  automaticProblems: [],
  repaintScan: null,
  // Invoices to pick up by hand, and the pick-up going on right now.
  pickups: [],
  pickupSession: null,
  pickupError: null,
  setPickupSession(session) {
    app.pickupSession = session;
    if (activeView === "scan") app.repaintScan?.();
  },
  async refreshBadges() {
    try {
      showPickups(await api.listPickups());
    } catch {
      showPickups([]);
    }
    try {
      const invoices = await api.listInvoices();
      const count = invoices.filter((row) => row.status === "needs_review").length;
      reviewBadge.hidden = count === 0;
      reviewBadge.textContent = count;
    } catch {
      reviewBadge.hidden = true;
    }
    try {
      const seen = historySeen();
      const runs = await api.listRuns();
      showProblems(runs.filter((run) => run.automatic && run.summary?.failed > 0 && run.id > seen));
    } catch {
      showProblems([]);
    }
  },
  // Opens the company editor with assisted mode filled in, for the person to check.
  suggestAssisted(companyId, portalUrl) {
    navigate("companies", { suggestAssisted: { companyId, portalUrl } });
  },
  markHistorySeen(runs) {
    try {
      if (runs.length > 0) localStorage.setItem(HISTORY_SEEN_KEY, String(runs[0].id));
    } catch {
      // Without storage, the badge simply comes back at the next refresh.
    }
    showProblems([]);
  },
};

function showPickups(items) {
  app.pickups = items;
  pickupBadge.hidden = items.length === 0;
  pickupBadge.textContent = items.length;
  if (activeView === "scan") app.repaintScan?.();
}

function showProblems(runs) {
  app.automaticProblems = runs;
  problemBadge.hidden = runs.length === 0;
  problemBadge.textContent = runs.length;
  if (activeView === "scan") app.repaintScan?.();
}

const nav = h("nav", { class: "nav" });

function paintNav() {
  replace(nav, Object.keys(VIEWS).map((name) =>
    h("button", {
      class: `nav-item${name === activeView ? " is-active" : ""}`,
      "aria-current": name === activeView ? "page" : undefined,
      onclick: () => navigate(name),
    }, icon(name), h("span", {}, t(`nav.${name}`)), NAV_BADGES[name])));
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
  app.portalSecrets = new Map(state.stored_portal_secrets);
  app.llmKeyStored = state.llm_key_stored;
  app.anthropicDefaultModel = state.anthropic_default_model;
  app.systemDownloadsDir = state.system_downloads_dir;
  watchPortalConfirmations();
  watchPickups(app);
  onAutomationStatus(({ running }) => {
    app.automationRunning = running;
    if (activeView === "scan") app.repaintScan?.();
    if (!running && app.config) app.refreshBadges();
  });
  api.automationStatus().then(({ running }) => {
    app.automationRunning = running;
    if (activeView === "scan") app.repaintScan?.();
  });

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
