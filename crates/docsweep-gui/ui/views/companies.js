import { api, errorText } from "../lib/api.js";
import { h, icon, replace } from "../lib/dom.js";
import { t } from "../lib/i18n.js";
import { confirmDialog, emptyState, field, folderInput, notice, pageHeader, showIssues, textInput, toast } from "../lib/ui.js";

const DRAFTS_KEY = "docsweep.company-drafts";

const defaultFilename = (name) => `{date:%Y-%m-%d} - ${name || "Company"} - {invoice_no}.pdf`;

// Unsaved work survives leaving the page, switching companies and restarts.
// Drafts only hold what the form shows; secrets never pass through this form.
function loadDrafts() {
  try {
    return JSON.parse(localStorage.getItem(DRAFTS_KEY)) ?? {};
  } catch {
    return {};
  }
}

const drafts = loadDrafts();
let selectedKey = null;

function saveDrafts() {
  try {
    localStorage.setItem(DRAFTS_KEY, JSON.stringify(drafts));
  } catch {
    // Without storage, drafts still last until the app quits.
  }
}

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
  const name = domain ? domain.split(".")[0].replace(/^./, (c) => c.toUpperCase()) : "";
  return {
    key: `new:${Date.now()}`,
    isNew: true,
    id: "",
    name,
    mail: { from_domain: domain, subject: "", attachment: "" },
    output: { dir: "", filename: defaultFilename(name) },
    disposalChoice: "default",
    moveTo: "",
    hasPortal: false,
    schedule: "",
  };
}

function draftFrom(company) {
  const disposal = company.disposal;
  return {
    key: company.id,
    isNew: false,
    id: company.id,
    name: company.name,
    mail: {
      from_domain: company.mail?.from_domain ?? "",
      subject: company.mail?.subject ?? "",
      attachment: company.mail?.attachment ?? "",
    },
    output: { dir: company.output.dir ?? "", filename: company.output.filename },
    disposalChoice: disposal === "delete" ? "delete" : disposal?.move_to ? "move" : "default",
    moveTo: disposal?.move_to ?? "",
    hasPortal: Boolean(company.portal),
    schedule: company.portal?.schedule ?? "",
    original: company,
  };
}

function companyFrom(draft, id) {
  const company = { id, name: draft.name.trim(), output: { filename: draft.output.filename } };
  if (draft.output.dir.trim()) company.output.dir = draft.output.dir.trim();
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

// A new company counts as a draft once something was typed into it; an existing
// one once it differs from what is saved.
function hasContent(draft) {
  if (draft.isNew) {
    return Boolean(draft.name.trim() || draft.mail.from_domain.trim() || draft.mail.subject
      || draft.mail.attachment || draft.output.dir.trim());
  }
  const saved = companyFrom(draftFrom(draft.original), draft.id);
  return JSON.stringify(companyFrom(draft, draft.id)) !== JSON.stringify(saved);
}

export function renderCompanies(app, options = {}) {
  const root = h("div", { class: "page page-wide" });
  const listHost = h("aside", { class: "master" });
  // The form being edited. A blank new company lives only here, not in `drafts`.
  let draft = null;

  const companies = () => app.config.companies;

  // Drafts of companies that no longer exist, or whose base changed on disk.
  for (const [key, stored] of Object.entries(drafts)) {
    if (stored.isNew) continue;
    const company = companies().find((candidate) => candidate.id === stored.id);
    if (company) stored.original = company;
    else delete drafts[key];
  }
  saveDrafts();

  function open(key) {
    selectedKey = key;
    draft = drafts[key] ?? null;
    if (!draft) {
      const company = companies().find((candidate) => candidate.id === key);
      draft = company ? draftFrom(company) : null;
    }
    paint();
  }

  function startNew(domain) {
    draft = blankDraft(domain);
    selectedKey = draft.key;
    remember();
    paint();
  }

  // Called after every change in the form.
  function remember() {
    if (hasContent(draft)) drafts[draft.key] = draft;
    else delete drafts[draft.key];
    saveDrafts();
    paintList();
    discardButton.hidden = !drafts[draft.key];
  }

  function forget(key) {
    delete drafts[key];
    saveDrafts();
  }

  async function persist(nextCompanies) {
    const config = { ...app.config, companies: nextCompanies };
    await api.saveConfig(config);
    app.setConfig(config);
  }

  function paintList() {
    const newDrafts = Object.values(drafts).filter((entry) => entry.isNew);
    const showBlank = draft?.isNew && !drafts[draft.key];
    const item = (key, name, meta, isDraft) => h("li", {},
      h("button", {
        class: `master-item${key === selectedKey ? " is-active" : ""}`,
        onclick: () => open(key),
      },
        h("span", { class: "master-name" }, name),
        meta && h("span", { class: `master-meta${isDraft ? " is-draft" : ""}` }, meta),
      ));
    replace(listHost,
      h("button", { class: "button button-block", onclick: () => startNew("") }, icon("plus"), t("companies.add")),
      h("ul", { class: "master-list" },
        showBlank && item(draft.key, t("companies.new")),
        newDrafts.map((entry) => item(entry.key, entry.name.trim() || t("companies.new"), t("companies.draft"), true)),
        companies().map((company) => {
          const changed = drafts[company.id];
          return item(
            company.id,
            changed ? changed.name.trim() || company.name : company.name,
            changed ? t("companies.unsaved") : company.mail?.from_domain ?? t("companies.section.portal"),
            Boolean(changed),
          );
        }),
      ));
  }

  // Lives outside the form, because `remember` toggles it while typing.
  const discardButton = h("button", {
    class: "button button-quiet",
    onclick: async () => {
      const confirmed = await confirmDialog({
        title: t(draft.isNew ? "companies.discard_title" : "companies.discard_changes_title"),
        text: t("companies.discard_text"),
        confirmLabel: t(draft.isNew ? "companies.discard_draft" : "companies.discard_changes"),
        danger: true,
      });
      if (!confirmed) return;
      forget(draft.key);
      if (draft.isNew) {
        draft = null;
        openFirst();
      } else {
        open(draft.key);
      }
    },
  });

  function openFirst() {
    const first = Object.values(drafts).find((entry) => entry.isNew)?.key ?? companies()[0]?.id;
    if (first) open(first);
    else {
      selectedKey = null;
      paint();
    }
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
      filename.dispatchEvent(new Event("input", { bubbles: true }));
      filename.focus();
    };

    const nameInput = textInput({
      value: draft.name,
      autofocus: draft.isNew && !draft.name,
      oninput: (event) => {
        draft.name = event.target.value;
        if (draft.isNew && !draft.filenameEdited) {
          filename.value = defaultFilename(draft.name.trim());
          draft.output.filename = filename.value;
          refreshPreview();
        }
      },
    });

    const defaultFolder = app.config.defaults?.output_dir;
    const dirField = field({
      label: t("companies.field.dir"),
      path: "output.dir",
      optional: Boolean(defaultFolder),
      hint: defaultFolder
        ? t("companies.field.dir_default", { folder: defaultFolder })
        : t("companies.field.dir_no_default"),
      input: folderInput({
        value: draft.output.dir,
        placeholder: defaultFolder,
        onChange: (value) => {
          draft.output.dir = value;
          remember();
        },
      }),
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
          remember();
        },
      }, t(`companies.disposal.${choice}`))));

    const portalNote = h("p", { class: "field-hint", hidden: !draft.hasPortal }, t("companies.portal.note"));
    const portalToggle = h("label", { class: "check" },
      h("input", {
        type: "checkbox",
        checked: draft.hasPortal,
        onchange: (event) => {
          draft.hasPortal = event.target.checked;
          portalNote.hidden = !draft.hasPortal;
        },
      }),
      h("span", {}, t("companies.portal.toggle")));

    const form = h("div", { class: "form" });
    // Every field updates the draft first; this runs as the event bubbles up.
    form.addEventListener("input", remember);
    form.addEventListener("change", remember);

    async function save(button) {
      const key = draft.key;
      const id = draft.isNew ? slug(draft.name, companies().map((c) => c.id)) : draft.id;
      const company = companyFrom(draft, id);
      const next = draft.isNew
        ? [...companies(), company]
        : companies().map((existing) => (existing.id === id ? company : existing));
      const position = next.findIndex((existing) => existing.id === id);
      button.disabled = true;
      try {
        await persist(next);
        forget(key);
        toast(t("common.saved"));
        open(id);
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
        const key = draft.key;
        await persist(companies().filter((company) => company.id !== draft.id));
        forget(key);
        draft = null;
        openFirst();
      } catch (error) {
        replace(message, notice("danger", errorText(error)));
      }
    }

    const saveButton = h("button", { class: "button button-primary", onclick: () => save(saveButton) }, t("common.save"));

    const mailInput = (key) => textInput({
      value: draft.mail[key],
      class: "mono",
      oninput: (event) => { draft.mail[key] = event.target.value; },
    });

    discardButton.textContent = t(draft.isNew ? "companies.discard_draft" : "companies.discard_changes");
    discardButton.hidden = !drafts[draft.key];

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
        dirField,
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
        portalNote,
      ),

      message,
      h("div", { class: "form-actions" },
        !draft.isNew && h("button", { class: "button button-quiet-danger", onclick: remove }, t("common.delete")),
        h("span", { class: "spacer" }),
        discardButton,
        saveButton,
      ),
    );
    return h("section", { class: "detail" }, form);
  }

  function paint() {
    const hasDrafts = Object.keys(drafts).length > 0;
    if (companies().length === 0 && !draft && !hasDrafts) {
      replace(root, pageHeader(t("companies.title"), t("companies.lead")),
        emptyState("companies", t("companies.empty_title"), t("companies.empty_lead"),
          h("button", { class: "button button-primary", onclick: () => startNew("") }, icon("plus"), t("companies.add"))));
      return;
    }
    paintList();
    replace(root, pageHeader(t("companies.title"), t("companies.lead")),
      h("div", { class: "master-detail" },
        listHost,
        draft ? editorView() : h("section", { class: "detail detail-empty" }, h("p", {}, t("companies.select"))),
      ));
  }

  if (options.draftDomain !== undefined) startNew(options.draftDomain);
  else if (options.create) startNew("");
  else if (selectedKey && (drafts[selectedKey] || companies().some((c) => c.id === selectedKey))) open(selectedKey);
  else openFirst();
  return root;
}
