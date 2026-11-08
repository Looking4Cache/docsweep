import { api, errorText } from "../lib/api.js";
import { h, replace } from "../lib/dom.js";
import { t } from "../lib/i18n.js";
import { field, folderInput, notice, pageHeader, showIssues, textInput, toast, withBusy } from "../lib/ui.js";
import { cleanupForm } from "./mailboxes.js";

export function renderSettings(app) {
  const root = h("div", { class: "page" });

  // One card with its own save button, so changes stay easy to oversee.
  function card({ title, lead, form, extra, save, afterSave }) {
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
          await afterSave?.(config);
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

  // The configuration without a default, or with a new value for it.
  function withDefault(key, value) {
    const { defaults: old, ...rest } = app.config;
    const defaults = { ...old };
    if (value) defaults[key] = value;
    else delete defaults[key];
    return Object.keys(defaults).length > 0 ? { ...rest, defaults } : rest;
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
      save: () => withDefault("output_dir", folder.trim()),
    });
  }

  function downloadsCard() {
    let folder = app.config.defaults?.downloads_dir ?? "";
    const form = h("div", { class: "form" },
      field({
        label: t("settings.downloads.label"),
        hint: t("settings.downloads.hint"),
        path: "defaults.downloads_dir",
        optional: true,
        input: folderInput({
          value: folder,
          placeholder: app.systemDownloadsDir ?? "",
          onChange: (value) => { folder = value; },
        }),
      }),
    );
    return card({
      title: t("settings.downloads.title"),
      lead: t("settings.downloads.lead"),
      form,
      save: () => withDefault("downloads_dir", folder.trim()),
    });
  }

  // How long a link mail of a recipe company waits for its invoice.
  function linkMailCard() {
    const fallback = app.defaultGiveUpDays;
    let days = String(app.config.defaults?.link_mail_give_up_days ?? fallback);
    const form = h("div", { class: "form" },
      field({
        label: t("settings.link_mails.label"),
        hint: t("settings.link_mails.hint", { days: fallback }),
        path: "defaults.link_mail_give_up_days",
        input: textInput({
          value: days,
          class: "input-short",
          inputmode: "numeric",
          oninput: (event) => { days = event.target.value.trim(); },
        }),
      }),
    );
    return card({
      title: t("settings.link_mails.title"),
      lead: t("settings.link_mails.lead"),
      form,
      save: () => {
        const value = Number(days);
        if (!days || !Number.isInteger(value) || value < 1) {
          throw { message: "", issues: [{ path: "defaults.link_mail_give_up_days", message: t("settings.link_mails.invalid") }] };
        }
        return withDefault("link_mail_give_up_days", value === fallback ? null : value);
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

  // The key goes into the keychain once and cannot be read back. The configuration
  // only ever holds the reference to it.
  function aiCard() {
    const existing = app.config.llm;
    let enabled = Boolean(existing);
    let provider = existing?.provider ?? "anthropic";
    let model = existing?.model ?? "";
    const keyInput = h("input", { type: "password", autocomplete: "off" });

    const modelHint = h("span", {});
    const refreshHint = () => {
      modelHint.textContent = provider === "anthropic"
        ? t("ai.model_default", { model: app.anthropicDefaultModel })
        : t("ai.model_required");
    };
    refreshHint();

    const providers = h("div", { class: "segmented", role: "radiogroup" }, ["anthropic", "openai", "openrouter"].map((choice) =>
      h("button", {
        class: `segment${provider === choice ? " is-active" : ""}`,
        role: "radio",
        "aria-checked": String(provider === choice),
        onclick: (event) => {
          provider = choice;
          for (const button of providers.children) {
            button.classList.remove("is-active");
            button.setAttribute("aria-checked", "false");
          }
          event.currentTarget.classList.add("is-active");
          event.currentTarget.setAttribute("aria-checked", "true");
          refreshHint();
        },
      }, t(`ai.provider.${choice}`))));

    const details = h("div", { class: "form", hidden: !enabled },
      h("div", { class: "field" }, h("span", { class: "field-label" }, t("ai.provider")), providers),
      field({
        label: t("ai.model"),
        hint: modelHint,
        path: "llm.model",
        input: textInput({ value: model, class: "mono", oninput: (event) => { model = event.target.value.trim(); } }),
      }),
      field({
        label: t("ai.key"),
        hint: t(app.llmKeyStored ? "ai.key_stored" : "ai.key_hint"),
        path: "llm.api_key",
        input: keyInput,
      }),
    );
    const form = h("div", { class: "form" },
      checkbox({
        checked: enabled,
        title: t("ai.enabled"),
        hint: t("ai.enabled_hint"),
        onChange: (value) => {
          enabled = value;
          details.hidden = !enabled;
        },
      }),
      details,
    );
    return card({
      title: t("ai.title"),
      lead: t("ai.lead"),
      form,
      save: () => {
        const { llm: _unused, ...rest } = app.config;
        if (!enabled) return rest;
        if (!app.llmKeyStored && !keyInput.value) {
          throw { message: "", issues: [{ path: "llm.api_key", message: t("ai.key_missing") }] };
        }
        const llm = { provider, api_key: existing?.api_key ?? "keyring:llm-key" };
        if (model) llm.model = model;
        return { ...rest, llm };
      },
      afterSave: async (config) => {
        if (!config.llm || !keyInput.value) return;
        await api.setSecret(config.llm.api_key.replace("keyring:", ""), keyInput.value);
        keyInput.value = "";
        app.llmKeyStored = true;
      },
    });
  }

  replace(root,
    pageHeader(t("settings.title"), t("settings.lead")),
    folderCard(),
    downloadsCard(),
    linkMailCard(),
    cleanupCard(),
    automationCard(),
    aiCard(),
    h("article", { class: "card" },
      h("h2", { class: "card-heading" }, t("settings.about")),
      h("dl", { class: "about" },
        h("dt", {}, t("settings.about.version")), h("dd", {}, app.info.version),
        h("dt", {}, t("settings.about.config")), h("dd", {}, app.configPath),
      )),
  );
  return root;
}
