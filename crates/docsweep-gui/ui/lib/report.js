// Turns scan reports into the grouped mail list, for previews, results and the
// history. Steps of a failed mail are never worded as done.

import { fileName, h, icon } from "./dom.js";
import { t } from "./i18n.js";
import { badge } from "./ui.js";

const GROUP_ORDER = ["failed", "review", "save", "portal", "unknown", "other"];

function groupOf(mail) {
  if (mail.outcome.kind === "failed") return "failed";
  const action = mail.plan.action;
  switch (action.kind) {
    case "invoices":
      return action.decision.kind === "keep_and_flag" ? "review" : "save";
    case "portal_fetch_required":
      return "portal";
    case "unknown_invoice":
      return "unknown";
    default:
      return "other";
  }
}

export function groupReports(reports) {
  const groups = Object.fromEntries(GROUP_ORDER.map((name) => [name, []]));
  const accountErrors = [];
  for (const account of reports) {
    if (account.error) accountErrors.push({ account: account.account_id, message: account.error });
    for (const mail of account.mails) groups[groupOf(mail)].push(mail);
  }
  return { groups, accountErrors };
}

// What a real run would do, for the confirmation dialog.
export function plannedEffects(reports) {
  const effects = { saves: 0, reviews: 0, moves: 0, trash: 0, permanent: 0, notes: 0 };
  for (const account of reports) {
    for (const mail of account.mails) {
      const action = mail.plan.action;
      if (action.kind === "portal_fetch_required" && !action.already_recorded) effects.notes += 1;
      if (action.kind !== "invoices") continue;
      for (const document of action.documents) {
        if (document.disposition.kind === "store") effects.saves += 1;
        if (document.disposition.kind === "store_for_review") effects.reviews += 1;
      }
      if (action.decision.kind !== "dispose") continue;
      const disposal = action.decision.value.kind;
      if (disposal === "move_to") effects.moves += 1;
      if (disposal === "trash") effects.trash += 1;
      if (disposal === "delete_permanently") effects.permanent += 1;
    }
  }
  return effects;
}

function documentLine(document, failed) {
  const disposition = document.disposition;
  const name = fileName(document.target_path);
  switch (disposition.kind) {
    case "store":
      return h("div", { class: "doc" }, icon("file"),
        h("span", { class: "doc-name" }, name),
        failed && h("span", { class: "doc-note" }, t("row.tried")));
    case "store_for_review":
      return h("div", { class: "doc" }, icon("file"),
        h("span", { class: "doc-name" }, name),
        h("span", { class: "doc-note" }, t(disposition.value.kind === "number_not_found" ? "row.review_number" : "row.review_conflict")));
    case "already_stored":
      return h("div", { class: "doc is-muted" }, icon("check"),
        h("span", { class: "doc-name" }, fileName(disposition.value.path) || document.attachment_name),
        h("span", { class: "doc-note" }, t("row.already")));
    default:
      return h("div", { class: "doc is-muted" }, icon("file"),
        h("span", { class: "doc-name" }, document.attachment_name),
        h("span", { class: "doc-note" }, `${t("row.unresolved")}: ${disposition.value.reason}`));
  }
}

function destination(action, done) {
  if (action.kind !== "invoices") return badge("neutral", t("row.dest_stays"));
  if (action.decision.kind === "keep_and_flag") return badge("warning", t("row.dest_keep"), "flag");
  const disposal = action.decision.value;
  if (disposal.kind === "move_to") {
    return badge("neutral", t(done ? "row.dest_moved" : "row.dest_move", { folder: disposal.value }), "arrow");
  }
  if (disposal.kind === "trash") return badge("neutral", t(done ? "row.dest_trashed" : "row.dest_trash"), "trash");
  return badge("danger", t(done ? "row.dest_permanent_done" : "row.dest_permanent"), "trash");
}

function mailRow(mail, { onCreateCompany, done }) {
  const { plan, outcome } = mail;
  const action = plan.action;
  const failed = outcome.kind === "failed";
  const details = [];

  if (action.kind === "invoices") {
    details.push(action.documents.map((document) => documentLine(document, failed)));
  } else if (action.kind === "portal_fetch_required") {
    details.push(h("div", { class: "doc is-muted" }, icon("link"), h("span", {}, t("row.portal"))));
  } else if (action.kind === "no_document") {
    details.push(h("div", { class: "doc is-muted" }, icon("dot"), h("span", {}, t("row.no_document"))));
  } else if (action.kind === "unknown_invoice") {
    details.push(action.attachment_names.map((name) =>
      h("div", { class: "doc is-muted" }, icon("file"), h("span", { class: "doc-name" }, name))));
  }
  if (failed) details.push(h("p", { class: "row-failure" }, outcome.message));

  return h("li", { class: "mail-row" },
    h("div", { class: "mail-main" },
      h("div", { class: "mail-head" },
        h("span", { class: "mail-subject" }, plan.subject || "–"),
        action.company_name && h("span", { class: "mail-company" }, action.company_name),
      ),
      h("div", { class: "mail-from" }, t("row.from", { sender: plan.from || "–" })),
      details,
    ),
    h("div", { class: "mail-side" },
      !failed && destination(action, done),
      action.kind === "unknown_invoice" && onCreateCompany && h("button", {
        class: "button button-small",
        onclick: () => onCreateCompany(plan.from),
      }, t("row.make_company")),
    ),
  );
}

// Renders groups in order of urgency. Empty groups do not appear.
export function reportList(reports, { onCreateCompany, done = true } = {}) {
  const { groups, accountErrors } = groupReports(reports);
  const options = { onCreateCompany, done };
  const sections = [];
  for (const name of GROUP_ORDER) {
    const mails = groups[name];
    const problems = name === "failed" ? accountErrors : [];
    const count = mails.length + problems.length;
    if (count === 0) continue;
    sections.push(h("section", { class: `group group-${name}` },
      h("h2", { class: "group-title" }, t(`group.${name}`), h("span", { class: "group-count" }, count)),
      h("ul", { class: "mail-list" },
        problems.map((problem) =>
          h("li", { class: "mail-row" },
            h("div", { class: "mail-main" },
              h("span", { class: "mail-subject" }, t("row.account_error", { account: problem.account })),
              h("p", { class: "row-failure" }, problem.message)))),
        mails.map((mail) => mailRow(mail, options)),
      ),
    ));
  }
  return sections;
}

// A row of big numbers. Zeros stay out, so the row says only what matters.
export function summaryStats(summary, dryRun) {
  const stats = [
    ["save", summary.saved, t(dryRun ? "summary.would_save" : "summary.saved")],
    ["review", summary.for_review, t("summary.review")],
    ["dispose", summary.disposed, t(dryRun ? "summary.would_dispose" : "summary.disposed")],
    ["portal", summary.portal_fetches, t("summary.portal")],
    ["unknown", summary.unknown_invoices, t("summary.unknown")],
    ["failed", summary.failed, t("summary.failed")],
  ].filter(([, value]) => value > 0);
  return h("dl", { class: "stats" }, stats.map(([tone, value, label]) =>
    h("div", { class: `stat stat-${tone}` }, h("dd", {}, value), h("dt", {}, label))));
}
