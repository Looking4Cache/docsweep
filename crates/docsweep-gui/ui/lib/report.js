// Turns scan reports into the grouped mail list, for previews, results and the
// history. Steps of a failed mail are never worded as done.

import { fileName, h, icon } from "./dom.js";
import { t } from "./i18n.js";
import { badge } from "./ui.js";

const GROUP_ORDER = ["failed", "review", "save", "portal", "unknown", "other"];

function groupOf(mail) {
  if (mail.outcome.kind === "failed") return "failed";
  if (mail.outcome.kind === "link_resolved") return "save";
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
  const effects = { saves: 0, reviews: 0, moves: 0, trash: 0, permanent: 0, fetches: 0 };
  const fetching = new Set();
  for (const account of reports) {
    for (const mail of account.mails) {
      const action = mail.plan.action;
      if (action.kind === "portal_fetch_required" && action.fetch_due) fetching.add(action.company_id);
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
  effects.fetches = fetching.size;
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

function disposalBadge(disposal, done) {
  if (disposal.kind === "move_to") {
    return badge("neutral", t(done ? "row.dest_moved" : "row.dest_move", { folder: disposal.value }), "arrow");
  }
  if (disposal.kind === "trash") return badge("neutral", t(done ? "row.dest_trashed" : "row.dest_trash"), "trash");
  return badge("danger", t(done ? "row.dest_permanent_done" : "row.dest_permanent"), "trash");
}

function destination(mail, done) {
  const action = mail.plan.action;
  if (mail.outcome.kind === "link_resolved") return disposalBadge(mail.outcome.disposal, true);
  if (action.kind !== "invoices") return badge("neutral", t("row.dest_stays"));
  if (action.decision.kind === "keep_and_flag") return badge("warning", t("row.dest_keep"), "flag");
  return disposalBadge(action.decision.value, done);
}

// What is happening to the invoice behind a link mail.
function linkMailNote(mail) {
  switch (mail.outcome.kind) {
    case "planned":
      return t(mail.plan.action.fetch_due ? "row.portal_due" : "row.portal_checked_today");
    case "link_resolved":
      return t("row.portal_stored");
    case "awaiting_invoice":
      return t("row.portal_waiting");
    case "failed":
      return t("row.portal_not_stored");
    default:
      return t("row.portal");
  }
}

function mailRow(mail, { onCreateCompany, done }) {
  const { plan, outcome } = mail;
  const action = plan.action;
  const failed = outcome.kind === "failed";
  const details = [];

  if (action.kind === "invoices") {
    details.push(action.documents.map((document) => documentLine(document, failed)));
  } else if (action.kind === "portal_fetch_required") {
    details.push(h("div", { class: "doc is-muted" }, icon("link"), h("span", {}, linkMailNote(mail))));
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
      !failed && destination(mail, done),
      action.kind === "unknown_invoice" && onCreateCompany && h("button", {
        class: "button button-small",
        onclick: () => onCreateCompany(plan.from),
      }, t("row.make_company")),
    ),
  );
}

const FETCH_GROUP_OF = {
  would_download: "save",
  stored: "save",
  stored_for_review: "review",
  missing: "missing",
  already_known: "known",
  unresolved: "known",
  failed: "failed",
};

const FETCH_FILE_OUTCOMES = ["would_download", "stored", "stored_for_review"];

function fetchEntryNote(entry) {
  const outcome = entry.outcome;
  switch (outcome.kind) {
    case "would_download":
      return outcome.file_name ?? t("row.fetch_would_download");
    case "stored":
      return fileName(outcome.path);
    case "stored_for_review":
      return `${fileName(outcome.path)}: ${t(outcome.reason.kind === "number_not_found" ? "row.review_number" : "row.review_conflict")}`;
    case "missing":
      return t("row.fetch_missing");
    case "already_known":
      return t("row.already");
    case "unresolved":
      return `${t("row.unresolved")}: ${outcome.reason}`;
    default:
      return outcome.message;
  }
}

function fetchEntryRow(entry, companyName) {
  const failed = entry.outcome.kind === "failed";
  const facts = [entry.date, entry.amount].filter(Boolean).join(" · ");
  return h("li", { class: "mail-row" },
    h("div", { class: "mail-main" },
      h("div", { class: "mail-head" },
        h("span", { class: "mail-subject" }, entry.key),
        h("span", { class: "mail-company" }, companyName),
      ),
      facts && h("div", { class: "mail-from" }, facts),
      failed
        ? h("p", { class: "row-failure" }, fetchEntryNote(entry))
        : h("div", { class: "doc is-muted" }, icon("file"),
          // Only file names are set in the monospace font, not sentences.
          h("span", { class: FETCH_FILE_OUTCOMES.includes(entry.outcome.kind) ? "doc-name" : "" }, fetchEntryNote(entry))),
    ),
  );
}

// The reports of portal fetches: one row per list entry, by urgency.
function fetchReportList(reports) {
  const order = ["failed", "review", "save", "missing", "known"];
  const groups = Object.fromEntries(order.map((name) => [name, []]));
  for (const report of reports) {
    if (report.error) {
      groups.failed.push(h("li", { class: "mail-row" },
        h("div", { class: "mail-main" },
          h("span", { class: "mail-subject" }, report.company_name),
          h("p", { class: "row-failure" }, report.error))));
    }
    for (const entry of report.entries) {
      groups[FETCH_GROUP_OF[entry.outcome.kind]].push(fetchEntryRow(entry, report.company_name));
    }
  }
  return order.filter((name) => groups[name].length > 0).map((name) =>
    h("section", { class: `group group-${name}` },
      h("h2", { class: "group-title" }, t(`group.${name}`), h("span", { class: "group-count" }, groups[name].length)),
      h("ul", { class: "mail-list" }, groups[name])));
}

// Renders groups in order of urgency. Empty groups do not appear.
export function reportList(reports, { onCreateCompany, done = true } = {}) {
  // The history keeps mailboxes and portal downloads in one list.
  const fetches = reports.filter((report) => report.entries);
  const accounts = reports.filter((report) => report.mails);
  if (accounts.length === 0) return fetchReportList(fetches);
  const mailSections = mailReportList(accounts, { onCreateCompany, done });
  if (fetches.length === 0) return mailSections;
  return [
    mailSections,
    h("h2", { class: "report-heading" }, t("report.portal_downloads")),
    fetchReportList(fetches),
  ];
}

function mailReportList(reports, { onCreateCompany, done }) {
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
    ["unknown", summary.missing, t("summary.missing")],
    ["failed", summary.failed, t("summary.failed")],
  ].filter(([, value]) => value > 0);
  return h("dl", { class: "stats" }, stats.map(([tone, value, label]) =>
    h("div", { class: `stat stat-${tone}` }, h("dd", {}, value), h("dt", {}, label))));
}
