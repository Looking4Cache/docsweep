import { api, errorText } from "../lib/api.js";
import { fileName, h, icon, replace } from "../lib/dom.js";
import { formatAmount, formatDate, t } from "../lib/i18n.js";
import { badge, emptyState, field, notice, pageHeader, showDialog, textInput, toast } from "../lib/ui.js";

const STATUS_TONES = { stored: "success", needs_review: "warning", portal_fetch_required: "neutral" };
const FILTERS = [
  ["all", () => true],
  ["review", (row) => row.status === "needs_review"],
  ["portal", (row) => row.status === "portal_fetch_required"],
];

function renderTemplate(template, date, number) {
  const [year, month, day] = date.split("-");
  const formats = { Y: year, m: month, d: day, y: (year ?? "").slice(2) };
  return template
    .replace(/\{date(?::([^}]*))?\}/g, (_, format) =>
      (format ?? "%Y-%m-%d").replace(/%([Ymdy])/g, (match, key) => formats[key] ?? match))
    .replace(/\{invoice_no\}/g, number.replace(/[\/\\:*?"<>|]/g, "-"));
}

function reviewDialog(row, company, onDone) {
  return showDialog((close) => {
    const number = textInput({ value: row.invoice_no ?? "", autofocus: true });
    const date = h("input", { type: "date", value: row.invoice_date ?? "" });
    const preview = h("code", { class: "file-preview" });
    const message = h("div", {});
    const update = () => {
      preview.textContent = company
        ? renderTemplate(company.output.filename, date.value, number.value.trim() || "…")
        : "";
    };
    number.addEventListener("input", update);
    date.addEventListener("input", update);
    update();

    const submit = h("button", {
      class: "button button-primary",
      onclick: async () => {
        submit.disabled = true;
        try {
          await api.completeReview(row.id, number.value, date.value);
          close(true);
          onDone();
        } catch (error) {
          replace(message, notice("danger", errorText(error)));
          submit.disabled = false;
        }
      },
    }, t("review.submit"));

    return h("div", { class: "dialog-body" },
      h("h2", {}, t("review.title")),
      h("p", { class: "dialog-text" }, t("review.lead")),
      h("button", {
        class: "file-chip",
        onclick: () => api.revealInvoice(row.id).catch((error) => replace(message, notice("danger", errorText(error)))),
      }, icon("file"), h("span", {}, fileName(row.file_path)), h("span", { class: "file-chip-action" }, t("review.open"))),
      h("div", { class: "form-row" },
        field({ label: t("review.number"), input: number }),
        field({ label: t("review.date"), input: date }),
      ),
      company && h("div", { class: "field" },
        h("span", { class: "field-label" }, t("review.file_name")), preview),
      message,
      h("div", { class: "dialog-actions" },
        h("button", { class: "button", onclick: () => close(false) }, t("common.cancel")),
        submit,
      ),
    );
  });
}

export function renderInvoices(app) {
  const root = h("div", { class: "page" });
  let rows = [];
  let activeFilter = "all";
  let search = "";
  const tableHost = h("div", {});

  const companyName = (id) => app.config.companies.find((company) => company.id === id)?.name ?? id;

  function matches(row) {
    const test = FILTERS.find(([name]) => name === activeFilter)[1];
    if (!test(row)) return false;
    if (!search) return true;
    const haystack = [row.invoice_no, companyName(row.company_id), fileName(row.file_path)].join(" ").toLowerCase();
    return haystack.includes(search);
  }

  function rowView(row) {
    const inReview = row.status === "needs_review";
    return h("tr", { class: inReview ? "is-review" : "" },
      h("td", { class: "cell-date" }, formatDate(row.invoice_date)),
      h("td", {}, companyName(row.company_id)),
      h("td", { class: "cell-number" }, row.invoice_no ?? h("span", { class: "muted" }, t("invoices.no_number"))),
      h("td", { class: "cell-amount" }, formatAmount(row.amount, row.currency)),
      h("td", {}, badge(STATUS_TONES[row.status], t(`status.${row.status}`))),
      h("td", { class: "cell-actions" },
        inReview && h("button", {
          class: "button button-small button-primary",
          onclick: () => reviewDialog(row, app.config.companies.find((c) => c.id === row.company_id), () => {
            toast(t("common.saved"));
            load();
            app.refreshBadges();
          }),
        }, t("invoices.review")),
        row.file_path && h("button", {
          class: "button button-small button-quiet",
          title: fileName(row.file_path),
          onclick: () => api.revealInvoice(row.id).catch((error) => replace(tableHost, notice("danger", errorText(error)))),
        }, t("invoices.reveal")),
      ),
    );
  }

  function paintTable() {
    const visible = rows.filter(matches);
    if (visible.length === 0) {
      replace(tableHost, h("p", { class: "no-match" }, t("invoices.no_match")));
      return;
    }
    replace(tableHost, h("div", { class: "table-wrap" },
      h("table", { class: "table" },
        h("thead", {}, h("tr", {},
          h("th", {}, t("invoices.col.date")),
          h("th", {}, t("invoices.col.company")),
          h("th", {}, t("invoices.col.number")),
          h("th", { class: "cell-amount" }, t("invoices.col.amount")),
          h("th", {}, t("invoices.col.status")),
          h("th", {}),
        )),
        h("tbody", {}, visible.map(rowView)),
      )));
  }

  function paint() {
    if (rows.length === 0) {
      replace(root, pageHeader(t("invoices.title"), t("invoices.lead")),
        emptyState("invoices", t("invoices.empty_title"), t("invoices.empty_lead"),
          h("button", { class: "button button-primary", onclick: () => app.navigate("scan") }, t("scan.preview"))));
      return;
    }
    const reviewCount = rows.filter((row) => row.status === "needs_review").length;
    const chips = h("div", { class: "chips", role: "tablist" }, FILTERS.map(([name]) =>
      h("button", {
        class: `chip${activeFilter === name ? " is-active" : ""}`,
        onclick: () => {
          activeFilter = name;
          paint();
        },
      }, t(`invoices.filter.${name}`), name === "review" && reviewCount > 0 && h("span", { class: "chip-count" }, reviewCount))));
    const searchBox = textInput({
      class: "search",
      type: "search",
      placeholder: t("invoices.search"),
      value: search,
      oninput: (event) => {
        search = event.target.value.toLowerCase();
        paintTable();
      },
    });
    replace(root, pageHeader(t("invoices.title"), t("invoices.lead")),
      h("div", { class: "toolbar" }, chips, searchBox), tableHost);
    paintTable();
  }

  async function load() {
    try {
      rows = await api.listInvoices();
      paint();
    } catch (error) {
      replace(root, pageHeader(t("invoices.title")), notice("danger", errorText(error)));
    }
  }

  replace(root, pageHeader(t("invoices.title"), t("invoices.lead")));
  load();
  return root;
}
