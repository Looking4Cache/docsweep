import { api, errorText } from "../lib/api.js";
import { h, replace } from "../lib/dom.js";
import { t } from "../lib/i18n.js";
import { field, folderInput, notice, pageHeader, showIssues, toast, withBusy } from "../lib/ui.js";
import { cleanupForm } from "./mailboxes.js";

export function renderSettings(app) {
  const root = h("div", { class: "page" });

  // One card with its own save button, so changes stay easy to oversee.
  function card({ title, lead, form, save }) {
    const message = h("div", {});
    const saveButton = h("button", {
      class: "button button-primary",
      onclick: () => withBusy(saveButton, null, async () => {
        showIssues(form, []);
        replace(message);
        try {
          const config = save();
          await api.saveConfig(config);
          app.setConfig(config);
          toast(t("common.saved"));
        } catch (error) {
          const unplaced = showIssues(form, error?.issues ?? []);
          const rest = error?.issues?.length ? unplaced.join(" ") : errorText(error);
          replace(message, rest && notice("danger", rest));
        }
      }),
    }, t("common.save"));
    return h("article", { class: "card" },
      h("header", { class: "card-header card-header-stacked" },
        h("h2", { class: "card-heading" }, title),
        lead && h("p", { class: "card-lead" }, lead)),
      form,
      message,
      h("div", { class: "form-actions" }, h("span", { class: "spacer" }), saveButton),
    );
  }

  function folderCard() {
    let folder = app.config.defaults?.output_dir ?? "";
    const form = h("div", { class: "form" },
      field({
        label: t("settings.folder.label"),
        hint: t("settings.folder.hint"),
        path: "defaults.output_dir",
        input: folderInput({ value: folder, placeholder: "~/Invoices", onChange: (value) => { folder = value; } }),
      }),
      h("p", { class: "field-hint" }, t("settings.folder.moved_note")),
    );
    return card({
      title: t("settings.folder.title"),
      form,
      save: () => {
        const trimmed = folder.trim();
        const { defaults: _unused, ...rest } = app.config;
        return trimmed ? { ...rest, defaults: { output_dir: trimmed } } : rest;
      },
    });
  }

  function cleanupCard() {
    const disposal = { ...app.config.mail.disposal };
    return card({
      title: t("cleanup.title"),
      lead: t("cleanup.lead"),
      form: cleanupForm(disposal),
      save: () => {
        const next = { ...disposal };
        if (!next.move_to) delete next.move_to;
        return { ...app.config, mail: { ...app.config.mail, disposal: next } };
      },
    });
  }

  replace(root,
    pageHeader(t("settings.title"), t("settings.lead")),
    folderCard(),
    cleanupCard(),
    h("article", { class: "card" },
      h("h2", { class: "card-heading" }, t("settings.about")),
      h("dl", { class: "about" },
        h("dt", {}, t("settings.about.version")), h("dd", {}, app.info.version),
        h("dt", {}, t("settings.about.config")), h("dd", {}, app.configPath),
      )),
  );
  return root;
}
