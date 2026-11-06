import { api, errorText } from "../lib/api.js";
import { h, icon, replace } from "../lib/dom.js";
import { formatDate, formatDateTime, t, tn } from "../lib/i18n.js";
import { endPickup, startPickup } from "../lib/pickup.js";
import { plannedEffects, reportList, summaryStats } from "../lib/report.js";
import { confirmDialog, emptyState, notice, pageHeader } from "../lib/ui.js";

// Survives switching views, so a preview is not lost.
let current = { phase: "idle" };
// A scan can outlive the view that started it, so results are drawn by
// whichever instance of the view is on screen.
let paintVisible = () => {};

function hasEffects(effects) {
  return Object.values(effects).some((count) => count > 0);
}

function effectItems(effects) {
  const items = [];
  const add = (count, key, iconName, danger) => {
    if (count > 0) items.push({ icon: iconName, text: tn(key, count), danger });
  };
  add(effects.saves, "scan.confirm_saves", "file");
  add(effects.reviews, "scan.confirm_reviews", "flag");
  add(effects.moves, "scan.confirm_moves", "arrow");
  add(effects.trash, "scan.confirm_trash", "trash");
  add(effects.permanent, "scan.confirm_permanent", "trash", true);
  add(effects.fetches, "scan.confirm_fetches", "link");
  add(effects.pickups, "scan.confirm_pickups", "link");
  return items;
}

export function renderScan(app) {
  const root = h("div", { class: "page" });

  async function start(dryRun) {
    current = { phase: dryRun ? "previewing" : "running" };
    paintVisible();
    try {
      const result = await api.runScan(dryRun);
      current = { phase: dryRun ? "preview" : "done", result };
      if (!dryRun) app.refreshBadges();
    } catch (error) {
      current = { phase: "error", message: errorText(error) };
    }
    paintVisible();
  }
  app.startPreview = () => start(true);

  async function confirmRun(effects) {
    const permanent = effects.permanent > 0;
    const confirmed = await confirmDialog({
      title: t("scan.confirm_title"),
      text: t("scan.confirm_text"),
      items: effectItems(effects),
      confirmLabel: t("scan.confirm_run"),
      danger: permanent,
    });
    if (confirmed) start(false);
  }

  function working(labelKey) {
    return h("div", { class: "working" },
      h("div", { class: "sweep", role: "progressbar", "aria-label": t(labelKey) }, h("span", {})),
      h("p", {}, t(labelKey)));
  }

  function createCompanyFrom(sender) {
    const domain = (sender ?? "").split("@").pop() ?? "";
    app.navigate("companies", { draftDomain: domain.toLowerCase() });
  }

  function resultView(result, dryRun) {
    const { reports, summary } = result;
    const fetches = result.fetches ?? [];
    const anyMail = reports.some((account) => account.mails.length > 0 || account.error);
    if (!anyMail && fetches.length === 0) {
      return emptyState("check", t("scan.empty_title"), t("scan.empty_lead"),
        h("button", { class: "button", onclick: () => start(true) }, t("scan.preview_again")));
    }
    const effects = plannedEffects(reports);
    return [
      result.warning && notice("warning", result.warning),
      summaryStats(summary, dryRun),
      reportList([...reports, ...fetches], { onCreateCompany: createCompanyFrom, done: !dryRun }),
      dryRun && h("div", { class: "action-bar" },
        h("span", { class: "action-note" }, icon("question"),
          hasEffects(effects) ? t("scan.preview_note") : t("scan.nothing_to_run")),
        h("div", { class: "action-buttons" },
          h("button", { class: "button", onclick: () => start(true) }, t("scan.preview_again")),
          h("button", {
            class: "button button-primary",
            disabled: !hasEffects(effects),
            onclick: () => confirmRun(effects),
          }, t("scan.run")),
        ),
      ),
    ];
  }

  function headerFor(phase) {
    if (phase === "done") {
      const failed = current.result.summary.failed > 0;
      return pageHeader(t(failed ? "scan.done_with_failures" : "scan.done_title"), null,
        h("button", { class: "button", onclick: () => start(true) }, t("scan.preview_again")));
    }
    return pageHeader(t("scan.title"), null);
  }

  // What automatic runs are up to, so nobody has to open the history to find out.
  function automationNotices() {
    const problems = app.automaticProblems.length;
    if (!app.automationRunning && problems === 0) return null;
    return h("div", { class: "notices" },
      app.automationRunning && notice("info", t("scan.auto_running")),
      problems > 0 && h("div", { class: "notice notice-danger notice-with-action", role: "alert" },
        icon("alert"),
        h("span", {}, tn("scan.auto_problems", problems)),
        h("button", { class: "button button-small", onclick: () => app.navigate("history") }, t("scan.auto_see_history"))),
    );
  }

  // Invoices waiting for a person, whatever a scan is up to. Showing them is all
  // that automatic runs ever do about them.
  function pickupPanel() {
    const items = app.pickups;
    const session = app.pickupSession?.watching ? app.pickupSession : null;
    if (items.length === 0 && !session && !app.pickupError) return null;
    return h("section", { class: "pickups" },
      h("div", { class: "notices" },
        app.pickupError && notice("danger", app.pickupError),
        session && h("div", { class: "notice notice-info notice-with-action", role: "status" },
          icon("history"),
          h("span", {}, t("pickup.watching", {
            name: session.company_name,
            folder: session.folder,
            time: formatDateTime(session.ends_at),
          })),
          h("button", { class: "button button-small", onclick: () => endPickup(app) }, t("pickup.end"))),
      ),
      items.length > 0 && h("h2", { class: "report-heading" }, t("pickup.title")),
      items.length > 0 && h("p", { class: "pickup-lead" }, t("pickup.lead")),
      items.length > 0 && h("ul", { class: "mail-list" }, items.map((item) =>
        h("li", { class: "mail-row" },
          h("div", { class: "mail-main" },
            h("div", { class: "mail-head" }, h("span", { class: "mail-subject" }, item.company_name)),
            h("div", { class: "mail-from" }, item.mail_date
              ? t("pickup.mail_date", { date: formatDate(item.mail_date) })
              : t("pickup.mail_no_date"))),
          h("div", { class: "mail-side" },
            h("button", {
              class: "button button-small button-primary",
              disabled: Boolean(app.pickupSession),
              onclick: () => startPickup(app, item.company_id),
            }, t("pickup.start")))))),
    );
  }

  function paint() {
    const noCompanies = app.config.companies.length === 0;
    let body;
    switch (current.phase) {
      case "previewing":
        body = working("scan.previewing");
        break;
      case "running":
        body = working("scan.running");
        break;
      case "preview":
        body = resultView(current.result, true);
        break;
      case "done":
        body = resultView(current.result, false);
        break;
      case "error":
        body = [
          notice("danger", current.message),
          h("div", { class: "empty-actions" },
            h("button", { class: "button", onclick: () => start(true) }, t("scan.preview"))),
        ];
        break;
      default:
        body = h("div", { class: "hero" },
          h("div", { class: "hero-lines", "aria-hidden": "true" }, h("span", {}), h("span", {}), h("span", {})),
          h("h2", {}, t("scan.idle_title")),
          h("p", {}, t(noCompanies ? "scan.no_companies" : "scan.idle_lead")),
          noCompanies
            ? h("button", { class: "button button-primary button-large", onclick: () => app.navigate("companies", { create: true }) }, t("scan.add_company"))
            : h("button", { class: "button button-primary button-large", onclick: () => start(true) }, t("scan.preview")),
        );
    }
    replace(root, headerFor(current.phase), automationNotices(), pickupPanel(), body);
  }

  paintVisible = paint;
  app.repaintScan = paint;
  paint();
  return root;
}
