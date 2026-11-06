import { api, errorText } from "../lib/api.js";
import { h, icon, replace } from "../lib/dom.js";
import { t } from "../lib/i18n.js";
import { confirmDialog, emptyState, field, notice, pageHeader, showIssues, textInput, toast } from "../lib/ui.js";

const defaultFilename = (name) => `{date:%Y-%m-%d} - ${name || "Company"} - {invoice_no}.pdf`;

function slug(name, taken) {
  const base = name
    .normalize("NFKD")
    .replace(/[̀-ͯ]/g, "")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "") || "company";
  let id = base;
  for (let n = 2; taken.includes(id); n += 1) id = `${base}-${n}`;
  return id;
}

function previewName(template) {
  return template
    .replace(/\{date(?::([^}]*))?\}/g, (_, format) =>
      (format ?? "%Y-%m-%d").replace(/%Y/g, "2026").replace(/%m/g, "11").replace(/%d/g, "03").replace(/%y/g, "26"))
    .replace(/\{invoice_no\}/g, "R-1001");
}

function blankDraft(domain = "") {
  return {
    isNew: true,
    id: "",
    name: domain ? domain.split(".")[0].replace(/^./, (c) => c.toUpperCase()) : "",
    mail: { from_domain: domain, subject: "", attachment: "" },
    output: { dir: "~/Invoices", filename: "" },
    disposalChoice: "default",
    moveTo: "",
    hasPortal: false,
    schedule: "",
  };
}

function draftFrom(company) {
  const disposal = company.disposal;
  return {
    isNew: false,
    id: company.id,
    name: company.name,
    mail: {
      from_domain: company.mail?.from_domain ?? "",
      subject: company.mail?.subject ?? "",
      attachment: company.mail?.attachment ?? "",
    },
    output: { ...company.output },
    disposalChoice: disposal === "delete" ? "delete" : disposal?.move_to ? "move" : "default",
    moveTo: disposal?.move_to ?? "",
    hasPortal: Boolean(company.portal),
    schedule: company.portal?.schedule ?? "",
    original: company,
  };
}

function companyFrom(draft, id) {
  const company = {
    id,
    name: draft.name.trim(),
    output: { dir: draft.output.dir.trim(), filename: draft.output.filename },
  };
  if (draft.mail.from_domain.trim()) {
    company.mail = { from_domain: draft.mail.from_domain.trim() };
    if (draft.mail.subject) company.mail.subject = draft.mail.subject;
    if (draft.mail.attachment) company.mail.attachment = draft.mail.attachment;
  }
  if (draft.hasPortal) {
    // Keeps recipe and credentials that were written by hand.
    company.portal = {
      credentials: `keyring:${id}`,
      recipe: `recipes/${id}.yaml`,
      ...(draft.original?.portal ?? {}),
    };
    if (draft.schedule.trim()) company.portal.schedule = draft.schedule.trim();
    else delete company.portal.schedule;
  }
  if (draft.disposalChoice === "delete") company.disposal = "delete";
  if (draft.disposalChoice === "move") company.disposal = { move_to: draft.moveTo.trim() };
  return company;
}

export function renderCompanies(app, options = {}) {
  const root = h("div", { class: "page page-wide" });
  let selectedId = null;
  let draft = null;

  const companies = () => app.config.companies;

  function select(id) {
    selectedId = id;
    draft = draftFrom(companies().find((company) => company.id === id));
    paint();
  }

  function startNew(domain) {
    selectedId = null;
    draft = blankDraft(domain);
    draft.output.filename = defaultFilename(draft.name);
    paint();
  }

  async function persist(nextCompanies) {
    const config = { ...app.config, companies: nextCompanies };
    await api.saveConfig(config);
    app.setConfig(config);
  }

  function listView() {
    return h("aside", { class: "master" },
      h("button", { class: "button button-block", onclick: () => startNew("") }, icon("plus"), t("companies.add")),
      h("ul", { class: "master-list" },
        draft?.isNew && h("li", {}, h("button", { class: "master-item is-active" },
          h("span", { class: "master-name" }, draft.name || t("companies.new")))),
        companies().map((company) => h("li", {},
          h("button", {
            class: `master-item${company.id === selectedId ? " is-active" : ""}`,
            onclick: () => select(company.id),
          },
            h("span", { class: "master-name" }, company.name),
            h("span", { class: "master-meta" }, company.mail?.from_domain ?? t("companies.section.portal")),
          ))),
      ),
    );
  }

  function editorView() {
    const message = h("div", {});
    const filename = textInput({ value: draft.output.filename, class: "mono" });
    const preview = h("p", { class: "field-hint" });
    const refreshPreview = () => {
      preview.textContent = t("companies.preview", { name: previewName(filename.value) });
    };
    refreshPreview();
    filename.addEventListener("input", () => {
      draft.output.filename = filename.value;
      draft.filenameEdited = true;
      refreshPreview();
    });

    const insertToken = (token) => {
      const start = filename.selectionStart ?? filename.value.length;
      const end = filename.selectionEnd ?? start;
      filename.setRangeText(token, start, end, "end");
      filename.dispatchEvent(new Event("input"));
      filename.focus();
    };

    const nameInput = textInput({
      value: draft.name,
      autofocus: draft.isNew,
      oninput: (event) => {
        draft.name = event.target.value;
        if (draft.isNew && !draft.filenameEdited) {
          filename.value = defaultFilename(draft.name.trim());
          draft.output.filename = filename.value;
          refreshPreview();
        }
      },
    });

    const dirInput = textInput({
      value: draft.output.dir,
      class: "mono",
      oninput: (event) => { draft.output.dir = event.target.value; },
    });

    const moveField = field({
      label: t("companies.field.move_to"),
      path: "disposal.move_to",
      input: textInput({
        value: draft.moveTo,
        class: "mono",
        oninput: (event) => { draft.moveTo = event.target.value; },
      }),
    });
    moveField.hidden = draft.disposalChoice !== "move";

    const permanent = !app.config.mail.disposal.delete_to_trash;
    const disposalHint = h("p", { class: "field-hint" });
    const refreshDisposal = () => {
      moveField.hidden = draft.disposalChoice !== "move";
      disposalHint.hidden = draft.disposalChoice === "move";
      disposalHint.classList.toggle("is-danger", draft.disposalChoice === "delete" && permanent);
      disposalHint.textContent = draft.disposalChoice === "default"
        ? t("companies.disposal.default_hint")
        : t(permanent ? "companies.disposal.delete_permanent" : "companies.disposal.delete_trash");
    };
    refreshDisposal();

    const segmented = h("div", { class: "segmented", role: "radiogroup" }, ["default", "move", "delete"].map((choice) =>
      h("button", {
        class: `segment${draft.disposalChoice === choice ? " is-active" : ""}`,
        role: "radio",
        "aria-checked": String(draft.disposalChoice === choice),
        onclick: (event) => {
          draft.disposalChoice = choice;
          for (const button of segmented.children) {
            button.classList.remove("is-active");
            button.setAttribute("aria-checked", "false");
          }
          event.currentTarget.classList.add("is-active");
          event.currentTarget.setAttribute("aria-checked", "true");
          refreshDisposal();
        },
      }, t(`companies.disposal.${choice}`))));

    const scheduleRow = h("div", { hidden: !draft.hasPortal },
      h("p", { class: "field-hint" }, t("companies.portal.note")));
    const portalToggle = h("label", { class: "check" },
      h("input", {
        type: "checkbox",
        checked: draft.hasPortal,
        onchange: (event) => {
          draft.hasPortal = event.target.checked;
          scheduleRow.hidden = !draft.hasPortal;
        },
      }),
      h("span", {}, t("companies.portal.toggle")));

    const form = h("div", { class: "form" });

    async function save(button) {
      const id = draft.isNew ? slug(draft.name, companies().map((c) => c.id)) : draft.id;
      const company = companyFrom(draft, id);
      const next = draft.isNew
        ? [...companies(), company]
        : companies().map((existing) => (existing.id === id ? company : existing));
      const position = next.findIndex((existing) => existing.id === id);
      button.disabled = true;
      try {
        await persist(next);
        toast(t("common.saved"));
        select(id);
      } catch (error) {
        const prefix = `companies[${position}].`;
        const unplaced = showIssues(form, error?.issues ?? [], (path) =>
          path === `companies[${position}]` ? "mail.from_domain" : path.replace(prefix, ""));
        const rest = error?.issues?.length ? unplaced.join(" ") : errorText(error);
        replace(message, rest && notice("danger", rest));
        button.disabled = false;
      }
    }

    async function remove() {
      const confirmed = await confirmDialog({
        title: t("companies.delete_title", { name: draft.name }),
        text: t("companies.delete_text"),
        confirmLabel: t("common.delete"),
        danger: true,
      });
      if (!confirmed) return;
      try {
        await persist(companies().filter((company) => company.id !== draft.id));
        selectedId = null;
        draft = null;
        paint();
      } catch (error) {
        replace(message, notice("danger", errorText(error)));
      }
    }

    const saveButton = h("button", { class: "button button-primary", onclick: () => save(saveButton) }, t("common.save"));

    const mailInput = (key, extra) => textInput({
      value: draft.mail[key],
      class: "mono",
      oninput: (event) => { draft.mail[key] = event.target.value; },
      ...extra,
    });

    replace(form,
      field({ label: t("companies.field.name"), path: "name", input: nameInput }),

      h("section", { class: "form-section" },
        h("h3", {}, t("companies.section.mails")),
        h("p", { class: "form-lead" }, t("companies.section.mails_lead")),
        field({ label: t("companies.field.domain"), hint: t("companies.field.domain_hint"), path: "mail.from_domain", input: mailInput("from_domain") }),
        h("div", { class: "form-row" },
          field({ label: t("companies.field.subject"), hint: t("companies.field.subject_hint"), path: "mail.subject", optional: true, input: mailInput("subject") }),
          field({ label: t("companies.field.attachment"), hint: t("companies.field.attachment_hint"), path: "mail.attachment", optional: true, input: mailInput("attachment") }),
        ),
      ),

      h("section", { class: "form-section" },
        h("h3", {}, t("companies.section.filing")),
        field({
          label: t("companies.field.dir"),
          path: "output.dir",
          input: h("div", { class: "input-with-button" }, dirInput,
            h("button", {
              class: "button",
              onclick: async (event) => {
                event.preventDefault();
                const picked = await api.pickFolder();
                if (picked) {
                  dirInput.value = picked;
                  draft.output.dir = picked;
                }
              },
            }, icon("folder"), t("common.browse"))),
        }),
        h("div", { class: "field", "data-path": "output.filename" },
          h("span", { class: "field-label" }, t("companies.field.filename")),
          filename,
          h("div", { class: "tokens" },
            h("button", { class: "token", onclick: () => insertToken("{date:%Y-%m-%d}") }, t("companies.token.date")),
            h("button", { class: "token", onclick: () => insertToken("{invoice_no}") }, t("companies.token.number")),
          ),
          preview,
          h("p", { class: "field-error", hidden: true }),
        ),
      ),

      h("section", { class: "form-section" },
        h("h3", {}, t("companies.section.disposal")),
        segmented,
        disposalHint,
        moveField,
      ),

      h("section", { class: "form-section" },
        h("h3", {}, t("companies.section.portal")),
        portalToggle,
        scheduleRow,
      ),

      message,
      h("div", { class: "form-actions" },
        !draft.isNew && h("button", { class: "button button-quiet-danger", onclick: remove }, t("common.delete")),
        h("span", { class: "spacer" }),
        draft.isNew && h("button", {
          class: "button",
          onclick: () => {
            draft = null;
            paint();
          },
        }, t("common.cancel")),
        saveButton,
      ),
    );
    return h("section", { class: "detail" }, form);
  }

  function paint() {
    if (companies().length === 0 && !draft) {
      replace(root, pageHeader(t("companies.title"), t("companies.lead")),
        emptyState("companies", t("companies.empty_title"), t("companies.empty_lead"),
          h("button", { class: "button button-primary", onclick: () => startNew("") }, icon("plus"), t("companies.add"))));
      return;
    }
    replace(root, pageHeader(t("companies.title"), t("companies.lead")),
      h("div", { class: "master-detail" },
        listView(),
        draft ? editorView() : h("section", { class: "detail detail-empty" }, h("p", {}, t("companies.select"))),
      ));
  }

  if (options.draftDomain !== undefined) startNew(options.draftDomain);
  else if (options.create) startNew("");
  else if (companies().length > 0) select(companies()[0].id);
  else paint();
  return root;
}
