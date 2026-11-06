// First start: one mailbox and the cleanup default make a valid configuration.

import { api, errorText } from "../lib/api.js";
import { h, mark, replace } from "../lib/dom.js";
import { t } from "../lib/i18n.js";
import { notice, showIssues, withBusy } from "../lib/ui.js";
import { cleanupForm, mailboxForm } from "./mailboxes.js";

function idFor(user) {
  return (user.split("@")[0] || "mailbox").toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "") || "mailbox";
}

export function renderSetup(app) {
  const form = mailboxForm({ account: { imap: "", user: "", watch: "INBOX" }, passwordStored: false, pathPrefix: "mail.accounts[0]." });
  const disposal = { mode: "pdf_move_link_delete", move_to: "Invoices/Done", delete_to_trash: true };
  const cleanup = cleanupForm(disposal);
  const message = h("div", {});
  const container = h("div", {}, form.element, h("hr", { class: "divider" }),
    h("h2", { class: "setup-subtitle" }, t("cleanup.title")), cleanup);

  const submit = h("button", {
    class: "button button-primary button-large",
    onclick: () => withBusy(submit, t("setup.working"), async () => {
      const values = form.values();
      const id = idFor(values.user);
      const secretId = `mail-${id}`;
      showIssues(container, []);
      replace(message);
      if (!form.hasPassword()) {
        form.markPassword(t("mailbox.password_required"));
        return;
      }
      const next = { ...disposal };
      if (next.mode === "all_delete" || !next.move_to) delete next.move_to;
      const config = {
        mail: { accounts: [{ id, ...values, password: `keyring:${secretId}` }], disposal: next },
        companies: [],
      };
      try {
        // A failed attempt keeps the password in the field for the next try.
        await api.setSecret(secretId, form.password());
        await api.testMailbox(config.mail.accounts[0]);
        await api.saveConfig(config);
        form.clearPassword();
        app.setPasswordStored(id, true);
        app.setConfig(config);
        app.navigate("companies", { create: true });
      } catch (error) {
        const unplaced = showIssues(container, error?.issues ?? []);
        const rest = error?.issues?.length ? unplaced.join(" ") : errorText(error);
        replace(message, rest && notice("danger", rest));
      }
    }),
  }, t("setup.submit"));

  return h("div", { class: "setup" },
    h("div", { class: "setup-card" },
      h("div", { class: "setup-mark" }, mark()),
      h("h1", {}, t("setup.title")),
      h("p", { class: "lead" }, t("setup.lead")),
      container,
      message,
      h("div", { class: "setup-actions" }, submit),
      h("p", { class: "setup-foot" }, t("setup.only_imap")),
    ),
  );
}

export function renderBrokenConfig(error, path) {
  return h("div", { class: "setup" },
    h("div", { class: "setup-card" },
      h("div", { class: "setup-mark" }, mark()),
      h("h1", {}, t("setup.broken_title")),
      h("p", { class: "lead" }, t("setup.broken_lead")),
      h("code", { class: "file-preview" }, path),
      error.issues.length > 0
        ? h("ul", { class: "issue-list" }, error.issues.map((issue) =>
          h("li", {}, h("code", {}, issue.path), h("span", {}, issue.message))))
        : notice("danger", error.message),
    ));
}
