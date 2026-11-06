import { api, errorText } from "../lib/api.js";
import { h, replace } from "../lib/dom.js";
import { t } from "../lib/i18n.js";
import { field, folderInput, notice, pageHeader, showIssues, textInput, toast, withBusy } from "../lib/ui.js";
import { cleanupForm } from "./mailboxes.js";

export function renderSettings(app) {
  const root = h("div", { class: "page" });

  // One card with its own save button, so changes stay easy to oversee.
  function card({ title, lead, form, extra, save }) {
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
      extra && h("div", { class: "card-extra" }, extra),
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

  function checkbox({ checked, title, hint, onChange }) {
    return h("label", { class: "check" },
      h("input", { type: "checkbox", checked, onchange: (event) => onChange(event.target.checked, event.target) }),
      h("span", {},
        h("span", { class: "check-title" }, title),
        h("span", { class: "field-hint" }, hint)));
  }

  // The system knows about autostart, not the configuration, so this applies at
  // once and has no part in saving.
  function autostartField() {
    const message = h("div", {});
    const host = h("div", { class: "form" });
    api.autostartEnabled().then((enabled) => {
      replace(host,
        checkbox({
          checked: enabled,
          title: t("automation.autostart"),
          hint: t("automation.autostart_hint"),
          onChange: async (wanted, input) => {
            replace(message);
            try {
              await api.setAutostart(wanted);
              toast(t("common.saved"));
            } catch (error) {
              input.checked = !wanted;
              replace(message, notice("danger", errorText(error)));
            }
          },
        }),
        message);
    }).catch((error) => replace(host, notice("danger", errorText(error))));
    return host;
  }

  function automationCard() {
    let enabled = Boolean(app.config.automation?.enabled);
    let minutes = String(app.config.mail.poll_minutes ?? "");
    const form = h("div", { class: "form" },
      notice("warning", t("automation.warning")),
      checkbox({
        checked: enabled,
        title: t("automation.enabled"),
        hint: t("automation.enabled_hint"),
        onChange: (value) => { enabled = value; },
      }),
      field({
        label: t("automation.poll"),
        hint: t("automation.poll_hint"),
        path: "mail.poll_minutes",
        optional: true,
        input: textInput({
          value: minutes,
          class: "input-short",
          inputmode: "numeric",
          oninput: (event) => { minutes = event.target.value.trim(); },
        }),
      }),
    );
    return card({
      title: t("automation.title"),
      lead: t("automation.lead"),
      form,
      extra: autostartField(),
      save: () => {
        const { automation: _unused, ...rest } = app.config;
        const { poll_minutes: _alsoUnused, ...mail } = app.config.mail;
        if (minutes) {
          const value = Number(minutes);
          if (!Number.isInteger(value) || value < 0) {
            throw { message: "", issues: [{ path: "mail.poll_minutes", message: t("automation.poll_invalid") }] };
          }
          mail.poll_minutes = value;
        }
        return enabled ? { ...rest, automation: { enabled }, mail } : { ...rest, mail };
      },
    });
  }

  replace(root,
    pageHeader(t("settings.title"), t("settings.lead")),
    folderCard(),
    cleanupCard(),
    automationCard(),
    h("article", { class: "card" },
      h("h2", { class: "card-heading" }, t("settings.about")),
      h("dl", { class: "about" },
        h("dt", {}, t("settings.about.version")), h("dd", {}, app.info.version),
        h("dt", {}, t("settings.about.config")), h("dd", {}, app.configPath),
      )),
  );
  return root;
}
