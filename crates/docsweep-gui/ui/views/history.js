import { api, errorText } from "../lib/api.js";
import { h, icon, replace } from "../lib/dom.js";
import { formatDateTime, t } from "../lib/i18n.js";
import { reportList, summaryStats } from "../lib/report.js";
import { badge, emptyState, notice, pageHeader } from "../lib/ui.js";

function runCard(run) {
  const summary = run.summary ?? {};
  const failed = summary.failed > 0;
  const hasContent = Object.values(summary).some((value) => value > 0);
  return h("details", { class: `run${failed ? " has-failed" : ""}` },
    h("summary", { class: "run-summary" },
      h("span", { class: "run-chevron" }, icon("chevron")),
      h("span", { class: "run-time" }, formatDateTime(run.finished_at)),
      run.automatic && badge("neutral", t("history.automatic")),
      hasContent ? summaryStats(summary, false) : h("span", { class: "muted" }, t("history.nothing")),
    ),
    h("div", { class: "run-body" }, Array.isArray(run.reports) ? reportList(run.reports) : null),
  );
}

export function renderHistory(app) {
  const root = h("div", { class: "page" });
  const header = pageHeader(t("history.title"), t("history.lead"));
  replace(root, header);
  api.listRuns().then((runs) => {
    app.markHistorySeen(runs);
    replace(root, header, runs.length === 0
      ? emptyState("history", t("history.empty_title"), t("history.empty_lead"))
      : h("div", { class: "runs" }, runs.map(runCard)));
  }).catch((error) => {
    replace(root, header, notice("danger", errorText(error)));
  });
  return root;
}
