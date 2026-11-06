import { api, errorText } from "../lib/api.js";
import { h, icon, replace } from "../lib/dom.js";
import { formatDateTime, t, tn } from "../lib/i18n.js";
import { reportList, summaryStats } from "../lib/report.js";
import { describeSchedule } from "../lib/schedule.js";
import { confirmDialog, emptyState, field, folderInput, notice, pageHeader, sendDialog, showDialog, showIssues, textInput, toast, withBusy } from "../lib/ui.js";

const DRAFTS_KEY = "docsweep.company-drafts";

const defaultFilename = (name) => `{date:%Y-%m-%d} - ${name || "Company"} - {invoice_no}.pdf`;

// Unsaved work survives leaving the page, switching companies and restarts.
// Drafts hold what the form shows, except for secrets: those stay in their
// fields and go straight to the keychain.
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
    recipe: "",
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
    recipe: company.portal?.recipe ?? "",
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
    if (draft.recipe?.trim()) company.portal.recipe = draft.recipe.trim();
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

    const secretsOf = () => (draft.isNew ? { credentials: false, totp: false } : app.portalSecretsOf(draft.id));
    const secretInput = (autocomplete) => h("input", { type: "password", autocomplete });
    const portalUser = textInput({ autocomplete: "off" });
    const portalPassword = secretInput("new-password");
    const portalTotp = secretInput("off");

    // Says in words what the expression means and when it next comes up.
    const scheduleHint = h("span", { class: "field-hint" });
    const scheduleInput = textInput({
      value: draft.schedule ?? "",
      class: "mono",
      placeholder: "0 9 10 * *",
      oninput: (event) => {
        draft.schedule = event.target.value;
        refreshSchedule();
      },
    });
    const scheduleField = field({
      label: t("schedule.label"),
      path: "portal.schedule",
      optional: true,
      hint: scheduleHint,
      input: scheduleInput,
    });
    async function refreshSchedule() {
      const expression = scheduleInput.value.trim();
      const { next_run: nextRun, error } = expression
        ? await api.describeSchedule(expression)
        : { next_run: null, error: null };
      if (expression !== scheduleInput.value.trim()) return;
      // The same place where a problem appears after saving.
      const problem = scheduleField.querySelector(".field-error");
      problem.textContent = error ?? "";
      problem.hidden = !error;
      scheduleField.classList.toggle("has-error", Boolean(error));
      scheduleHint.hidden = Boolean(error);
      if (error) return;
      scheduleHint.textContent = !expression ? t("schedule.none") : [
        describeSchedule(expression),
        t("schedule.next_run", { time: formatDateTime(nextRun) }),
        !app.config.automation?.enabled && t("schedule.automation_off"),
      ].filter(Boolean).join(" ");
    }
    refreshSchedule();

    const recipeInput = textInput({
      value: draft.recipe ?? "",
      class: "mono",
      placeholder: "recipes/example.yaml",
      oninput: (event) => { draft.recipe = event.target.value; },
    });

    const portalFields = h("div", { class: "form", hidden: !draft.hasPortal },
      h("p", { class: "field-hint" }, t("companies.portal.note")),
      field({
        label: t("companies.portal.recipe"),
        hint: t("companies.portal.recipe_hint"),
        path: "portal.recipe",
        input: recipeInput,
      }),
      h("div", { class: "form-row" },
        field({
          label: t("companies.portal.user"),
          hint: t(secretsOf().credentials ? "companies.portal.login_stored" : "companies.portal.login_hint"),
          path: "portal.credentials",
          input: portalUser,
        }),
        field({ label: t("companies.portal.password"), path: "portal.password", input: portalPassword }),
      ),
      field({
        label: t("companies.portal.totp"),
        hint: t(secretsOf().totp ? "companies.portal.totp_stored" : "companies.portal.totp_hint"),
        path: "portal.totp",
        optional: true,
        input: portalTotp,
      }),
      scheduleField,
      fetchPanel(),
      recipeTools(),
    );
    const portalToggle = h("label", { class: "check" },
      h("input", {
        type: "checkbox",
        checked: draft.hasPortal,
        onchange: (event) => {
          draft.hasPortal = event.target.checked;
          portalFields.hidden = !draft.hasPortal;
        },
      }),
      h("span", {}, t("companies.portal.toggle")));

    const form = h("div", { class: "form" });
    // Every field updates the draft first; this runs as the event bubbles up.
    form.addEventListener("input", remember);
    form.addEventListener("change", remember);

    // Values go into the keychain once. The fields are then cleared, and nothing
    // ever comes back from there.
    async function storeTypedSecrets(id, company) {
      if (portalUser.value.trim() && portalPassword.value) {
        await api.setPortalCredentials(company.portal.credentials.replace("keyring:", ""), portalUser.value.trim(), portalPassword.value);
        portalUser.value = "";
        portalPassword.value = "";
        app.setPortalSecretStored(id, "credentials");
      }
      if (company.portal?.totp && portalTotp.value) {
        await api.setTotpKey(company.portal.totp.replace("keyring:", ""), portalTotp.value);
        portalTotp.value = "";
        app.setPortalSecretStored(id, "totp");
      }
    }

    // Always a preview first; a real run only after confirming what it found.
    function fetchPanel() {
      const saved = !draft.isNew && draft.original?.portal;
      if (!saved) return h("p", { class: "field-hint" }, t("fetch.needs_saving"));
      const id = draft.id;
      const name = draft.name;
      const result = h("div", {});
      const showBrowser = h("input", { type: "checkbox" });

      const run = async (dryRun, button, busyKey) => withBusy(button, t(busyKey), async () => {
        try {
          const outcome = await api.runFetch(id, dryRun, showBrowser.checked);
          showResult(outcome, dryRun);
          if (!dryRun) app.refreshBadges();
        } catch (error) {
          replace(result, notice("danger", errorText(error)));
        }
      });

      async function confirmRun(summary, button) {
        const items = [];
        if (summary.saved > 0) items.push({ icon: "file", text: tn("scan.confirm_saves", summary.saved) });
        if (summary.for_review > 0) items.push({ icon: "flag", text: tn("scan.confirm_reviews", summary.for_review) });
        const confirmed = await confirmDialog({
          title: t("fetch.confirm_title", { name }),
          text: t("fetch.confirm_text"),
          items,
          confirmLabel: t("fetch.confirm_run"),
        });
        if (confirmed) run(false, button, "fetch.running");
      }

      function showResult({ reports, summary, warning }, dryRun) {
        const hasNew = summary.saved + summary.for_review > 0;
        const download = h("button", {
          class: "button button-primary",
          disabled: !hasNew,
          onclick: () => confirmRun(summary, download),
        }, t("fetch.run"));
        replace(result,
          warning && notice("warning", warning),
          summaryStats(summary, dryRun),
          reportList(reports, { done: !dryRun }),
          dryRun && h("div", { class: "action-bar" },
            h("span", { class: "action-note" }, icon("question"), t(hasNew ? "fetch.preview_note" : "fetch.nothing_new")),
            h("div", { class: "action-buttons" }, download)),
        );
      }

      const fetchButton = h("button", {
        class: "button",
        onclick: () => run(true, fetchButton, "fetch.previewing"),
      }, t("fetch.now"));

      return h("div", { class: "form" },
        h("div", { class: "form-actions" },
          h("label", { class: "check" }, showBrowser, h("span", {}, t("fetch.show_browser"))),
          h("span", { class: "spacer" }),
          fetchButton,
        ),
        result,
      );
    }

    // Makes a draft file, never the recipe in use. A person decides on each thing
    // that matters, and only the app's side holds what is sent or written. Neither
    // address nor notes go into the stored form draft.
    function recipeTools() {
      if (draft.isNew || !draft.original?.portal) return null;
      const id = draft.id;
      const name = draft.name;
      const result = h("div", {});
      const address = textInput({ class: "mono", placeholder: "https://portal.example.com/login", inputmode: "url" });
      const description = textInput({});
      const addressField = field({ label: t("recipe.address"), hint: t("recipe.address_hint"), path: "recipe.address", input: address });
      const aiReady = Boolean(app.config.llm);

      const wanted = () => {
        const value = address.value.trim();
        showIssues(addressField.parentElement, value ? [] : [{ path: "recipe.address", message: t("recipe.address_missing") }]);
        return value;
      };

      async function finishDraft(view) {
        if (view.host_changes.length > 0) {
          const keep = await confirmDialog({
            title: t("recipe.host_title"),
            text: t("recipe.host_text"),
            items: view.host_changes.map((change) => ({ icon: "alert", danger: true, text: t("recipe.host_change", change) })),
            confirmLabel: t("recipe.host_keep"),
            danger: true,
          });
          if (!keep) {
            await api.discardRecipeWork();
            return;
          }
        }
        if (view.exists) {
          const replaceIt = await confirmDialog({
            title: t("recipe.replace_title"),
            text: t("recipe.replace_text", { file: view.file_name }),
            confirmLabel: t("recipe.replace"),
            danger: true,
          });
          if (!replaceIt) {
            await api.discardRecipeWork();
            return;
          }
        }
        await api.saveRecipeDraft(view.host_changes.length > 0, view.exists);
        const useButton = h("button", {
          class: "button",
          onclick: () => {
            recipeInput.value = view.file_name;
            recipeInput.dispatchEvent(new Event("input", { bubbles: true }));
            replace(result, notice("info", t("recipe.used")));
          },
        }, t("recipe.use"));
        replace(result,
          notice("info", t("recipe.saved", { file: view.file_name })),
          h("pre", { class: "prompt-text", tabindex: "0" }, view.text),
          h("div", { class: "form-actions" }, h("span", { class: "spacer" }), useButton));
      }

      const generateButton = h("button", {
        class: "button",
        disabled: !aiReady,
        onclick: () => {
          const target = wanted();
          if (!target) return;
          replace(result);
          let prompt;
          withBusy(generateButton, t("recipe.looking"), async () => {
            prompt = await api.prepareRecipe(id, target, description.value);
          }).then(async () => {
            const send = await sendDialog({
              title: t("recipe.send_title", { provider: prompt.provider }),
              lead: t("recipe.send_lead"),
              text: prompt.text,
              confirmLabel: t("ai.send_to", { provider: prompt.provider }),
            });
            if (!send) {
              await api.discardRecipeWork();
              return;
            }
            const view = await withBusy(generateButton, t("recipe.waiting", { provider: prompt.provider }), () => api.sendRecipePrompt());
            await finishDraft(view);
          }).catch((error) => replace(result, notice("danger", errorText(error))));
        },
      }, t("recipe.generate"));

      const recordButton = h("button", {
        class: "button",
        onclick: async () => {
          const target = wanted();
          if (!target) return;
          replace(result);
          let closeDialog = () => {};
          const dialog = showDialog((close) => {
            closeDialog = close;
            return h("div", { class: "dialog-body" },
              h("h2", {}, t("recipe.recording_title", { name })),
              h("p", { class: "dialog-text" }, t("recipe.recording_lead")),
              h("div", { class: "dialog-actions" },
                h("button", { class: "button button-primary", onclick: () => api.finishRecording() }, t("recipe.recording_finish"))),
            );
          });
          try {
            const view = await api.recordRecipe(id, target);
            closeDialog();
            await dialog;
            await finishDraft(view);
          } catch (error) {
            closeDialog();
            replace(result, notice("danger", errorText(error)));
          }
        },
      }, t("recipe.record"));

      return h("div", { class: "form recipe-tools" },
        h("h4", {}, t("recipe.title")),
        h("p", { class: "field-hint" }, t("recipe.lead")),
        addressField,
        aiReady && field({ label: t("recipe.describe"), hint: t("recipe.describe_hint"), optional: true, input: description }),
        !aiReady && h("p", { class: "field-hint" }, t("recipe.generate_off")),
        h("div", { class: "form-actions" }, h("span", { class: "spacer" }), recordButton, generateButton),
        result,
      );
    }

    async function save(button) {
      const key = draft.key;
      const id = draft.isNew ? slug(draft.name, companies().map((c) => c.id)) : draft.id;
      const company = companyFrom(draft, id);
      if (portalUser.value.trim() || portalPassword.value) {
        if (!draft.hasPortal || !portalUser.value.trim() || !portalPassword.value) {
          showIssues(form, [{ path: "portal.credentials", message: t("companies.portal.both_required") }]);
          return;
        }
      }
      if (draft.hasPortal && portalTotp.value) company.portal.totp ??= `keyring:${id}-totp`;
      const next = draft.isNew
        ? [...companies(), company]
        : companies().map((existing) => (existing.id === id ? company : existing));
      const position = next.findIndex((existing) => existing.id === id);
      button.disabled = true;
      try {
        // Saved first, so a configuration problem leaves no stray secret behind.
        await persist(next);
        await storeTypedSecrets(id, company);
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
        portalFields,
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
