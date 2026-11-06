import { api, errorText } from "../lib/api.js";
import { h, icon, replace } from "../lib/dom.js";
import { t, tn } from "../lib/i18n.js";
import { badge, confirmDialog, field, notice, pageHeader, showIssues, textInput, toast, withBusy } from "../lib/ui.js";

function accountId(user, taken) {
  const base = (user.split("@")[0] || "mailbox").toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "") || "mailbox";
  let id = base;
  for (let n = 2; taken.includes(id); n += 1) id = `${base}-${n}`;
  return id;
}

// The form for one mailbox, used here and on first start. The password field
// is cleared as soon as its value is in the keychain.
export function mailboxForm({ account, passwordStored, pathPrefix = "" }) {
  const server = textInput({ value: account.imap, class: "mono", placeholder: "imap.example.com:993", autofocus: !account.imap });
  const user = textInput({ value: account.user, placeholder: "invoices@example.com" });
  const password = h("input", { type: "password", autocomplete: "new-password" });
  const watch = textInput({ value: account.watch || "INBOX", class: "mono" });

  const element = h("div", { class: "form" },
    h("div", { class: "form-row" },
      field({ label: t("mailbox.field.server"), hint: t("mailbox.field.server_hint"), path: `${pathPrefix}imap`, input: server }),
      field({ label: t("mailbox.field.folder"), path: `${pathPrefix}watch`, input: watch }),
    ),
    h("div", { class: "form-row" },
      field({ label: t("mailbox.field.user"), path: `${pathPrefix}user`, input: user }),
      field({
        label: t("mailbox.field.password"),
        hint: t(passwordStored ? "mailbox.field.password_stored" : "mailbox.field.password_hint"),
        path: `${pathPrefix}password`,
        input: password,
      }),
    ),
  );

  return {
    element,
    values: () => ({ imap: server.value.trim(), user: user.value.trim(), watch: watch.value.trim() }),
    password: () => password.value,
    clearPassword() {
      password.value = "";
    },
    hasPassword: () => password.value.length > 0,
    markPassword(message) {
      showIssues(element, [{ path: `${pathPrefix}password`, message }]);
    },
  };
}

export function cleanupForm(disposal, onChange) {
  const folder = textInput({
    value: disposal.move_to ?? "",
    class: "mono",
    placeholder: "Invoices/Done",
    oninput: (event) => {
      disposal.move_to = event.target.value;
      onChange?.();
    },
  });
  const folderField = field({
    label: t("cleanup.folder"),
    hint: t("cleanup.folder_hint"),
    path: "mail.disposal.move_to",
    input: folder,
  });
  const warning = notice("danger", t("cleanup.permanent_warning"));

  const refresh = () => {
    folderField.hidden = disposal.mode === "all_delete";
    warning.hidden = disposal.delete_to_trash;
  };

  const options = ["pdf_move_link_delete", "all_move", "all_delete"].map((mode) =>
    h("label", { class: "option" },
      h("input", {
        type: "radio",
        name: "cleanup-mode",
        checked: disposal.mode === mode,
        onchange: () => {
          disposal.mode = mode;
          refresh();
          onChange?.();
        },
      }),
      h("span", { class: "option-text" },
        h("span", { class: "option-title" }, t(`cleanup.${mode}`)),
        h("span", { class: "option-hint" }, t(`cleanup.${mode}_hint`)),
      )));

  const element = h("div", { class: "form" },
    h("div", { class: "options" }, options),
    folderField,
    h("label", { class: "check" },
      h("input", {
        type: "checkbox",
        checked: disposal.delete_to_trash,
        onchange: (event) => {
          disposal.delete_to_trash = event.target.checked;
          refresh();
          onChange?.();
        },
      }),
      h("span", {},
        h("span", { class: "check-title" }, t("cleanup.trash")),
        h("span", { class: "field-hint" }, t("cleanup.trash_hint")))),
    warning,
    h("p", { class: "safety-note" }, icon("check"), t("cleanup.safety")),
  );
  refresh();
  return element;
}

export function renderMailboxes(app) {
  const root = h("div", { class: "page" });
  let adding = false;

  async function persist(mail) {
    const config = { ...app.config, mail };
    await api.saveConfig(config);
    app.setConfig(config);
  }

  function accountCard(account, position) {
    const stored = app.isPasswordStored(account.id);
    const prefix = `mail.accounts[${position}].`;
    const form = mailboxForm({ account, passwordStored: stored, pathPrefix: prefix });
    const message = h("div", {});

    async function storeTypedPassword() {
      if (!form.hasPassword()) return;
      await api.setSecret(account.password.replace("keyring:", ""), form.password());
      form.clearPassword();
      app.setPasswordStored(account.id, true);
    }

    const testButton = h("button", {
      class: "button",
      onclick: () => withBusy(testButton, t("mailbox.testing"), async () => {
        try {
          await storeTypedPassword();
          const count = await api.testMailbox({ ...account, ...form.values() });
          replace(message, notice("success", tn("mailbox.test_ok", count)));
        } catch (error) {
          replace(message, notice("danger", errorText(error)));
        }
      }),
    }, t("mailbox.test"));

    const saveButton = h("button", {
      class: "button button-primary",
      onclick: () => withBusy(saveButton, null, async () => {
        const accounts = app.config.mail.accounts.map((existing, i) =>
          i === position ? { ...existing, ...form.values() } : existing);
        try {
          await storeTypedPassword();
          await persist({ ...app.config.mail, accounts });
          toast(t("common.saved"));
          paint();
        } catch (error) {
          const unplaced = showIssues(form.element, error?.issues ?? []);
          const rest = error?.issues?.length ? unplaced.join(" ") : errorText(error);
          replace(message, rest && notice("danger", rest));
        }
      }),
    }, t("common.save"));

    const removeButton = app.config.mail.accounts.length > 1 && h("button", {
      class: "button button-quiet-danger",
      onclick: async () => {
        const confirmed = await confirmDialog({
          title: t("mailbox.remove_title", { name: account.user }),
          text: t("mailbox.remove_text"),
          confirmLabel: t("mailbox.remove"),
          danger: true,
        });
        if (!confirmed) return;
        try {
          await persist({ ...app.config.mail, accounts: app.config.mail.accounts.filter((_, i) => i !== position) });
          paint();
        } catch (error) {
          replace(message, notice("danger", errorText(error)));
        }
      },
    }, t("mailbox.remove"));

    return h("article", { class: "card" },
      h("header", { class: "card-header" },
        h("div", { class: "card-title" }, icon("mailboxes"), h("span", {}, account.user)),
        stored
          ? badge("success", t("mailbox.password_stored"), "check")
          : badge("warning", t("mailbox.password_missing"), "alert"),
      ),
      form.element,
      message,
      h("div", { class: "form-actions" }, removeButton, h("span", { class: "spacer" }), testButton, saveButton),
    );
  }

  function newAccountCard() {
    const blank = { imap: "", user: "", watch: "INBOX" };
    const position = app.config.mail.accounts.length;
    const form = mailboxForm({ account: blank, passwordStored: false, pathPrefix: `mail.accounts[${position}].` });
    const message = h("div", {});

    const saveButton = h("button", {
      class: "button button-primary",
      onclick: () => withBusy(saveButton, null, async () => {
        const values = form.values();
        const id = accountId(values.user, app.config.mail.accounts.map((a) => a.id));
        const secretId = `mail-${id}`;
        if (!form.hasPassword()) {
          form.markPassword(t("mailbox.password_required"));
          return;
        }
        const account = { id, ...values, password: `keyring:${secretId}` };
        try {
          // Saved first, so a configuration problem leaves no stray secret behind.
          await persist({ ...app.config.mail, accounts: [...app.config.mail.accounts, account] });
          await api.setSecret(secretId, form.password());
          form.clearPassword();
          app.setPasswordStored(id, true);
          adding = false;
          toast(t("common.saved"));
          paint();
        } catch (error) {
          const unplaced = showIssues(form.element, error?.issues ?? []);
          const rest = error?.issues?.length ? unplaced.join(" ") : errorText(error);
          replace(message, rest && notice("danger", rest));
        }
      }),
    }, t("common.save"));

    return h("article", { class: "card" },
      h("header", { class: "card-header" },
        h("div", { class: "card-title" }, icon("plus"), h("span", {}, t("mailbox.new")))),
      form.element,
      message,
      h("div", { class: "form-actions" },
        h("span", { class: "spacer" }),
        h("button", { class: "button", onclick: () => { adding = false; paint(); } }, t("common.cancel")),
        saveButton),
    );
  }

  function paint() {
    replace(root,
      pageHeader(t("mailbox.title"), t("mailbox.lead"),
        !adding && h("button", { class: "button", onclick: () => { adding = true; paint(); } }, icon("plus"), t("mailbox.add"))),
      adding && newAccountCard(),
      app.config.mail.accounts.map(accountCard),
    );
  }

  paint();
  return root;
}
