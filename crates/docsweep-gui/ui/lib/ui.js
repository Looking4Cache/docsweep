// Shared building blocks: fields, dialogs, messages.

import { h, icon, replace } from "./dom.js";
import { t } from "./i18n.js";

export function pageHeader(title, lead, ...actions) {
  return h("header", { class: "page-header" },
    h("div", {},
      h("h1", {}, title),
      lead && h("p", { class: "lead" }, lead),
    ),
    actions.length > 0 && h("div", { class: "page-actions" }, actions),
  );
}

export function emptyState(iconName, title, lead, ...actions) {
  return h("div", { class: "empty" },
    h("div", { class: "empty-icon" }, icon(iconName)),
    h("h2", {}, title),
    h("p", {}, lead),
    actions.length > 0 && h("div", { class: "empty-actions" }, actions),
  );
}

// A labelled form field. `path` connects it to validation messages.
export function field({ label, hint, path, input, optional }) {
  const error = h("p", { class: "field-error", hidden: true });
  const wrapper = h("label", { class: "field", "data-path": path },
    h("span", { class: "field-label" }, label,
      optional && h("span", { class: "field-optional" }, t("common.optional"))),
    input,
    hint && h("span", { class: "field-hint" }, hint),
    error,
  );
  return wrapper;
}

export function textInput(props) {
  return h("input", { type: "text", spellcheck: false, autocapitalize: "off", autocomplete: "off", ...props });
}

// Shows validation issues next to their fields. Returns what could not be
// placed, for a general message.
export function showIssues(container, issues, mapPath = (path) => path) {
  for (const element of container.querySelectorAll(".field-error")) {
    element.hidden = true;
    element.closest(".field")?.classList.remove("has-error");
  }
  const unplaced = [];
  for (const issue of issues) {
    const path = mapPath(issue.path);
    const target = container.querySelector(`[data-path="${CSS.escape(path)}"]`);
    if (target) {
      const error = target.querySelector(".field-error");
      error.textContent = issue.message;
      error.hidden = false;
      target.classList.add("has-error");
    } else {
      unplaced.push(`${issue.path}: ${issue.message}`);
    }
  }
  return unplaced;
}

export function notice(tone, text) {
  return h("div", { class: `notice notice-${tone}`, role: tone === "danger" ? "alert" : "status" },
    icon(tone === "danger" || tone === "warning" ? "alert" : "check"),
    h("span", {}, text));
}

export function badge(tone, text, iconName) {
  return h("span", { class: `badge badge-${tone}` }, iconName && icon(iconName), text);
}

// A modal built on the native <dialog>, which brings focus handling and Escape.
export function showDialog(build) {
  return new Promise((resolve) => {
    const dialog = h("dialog", { class: "dialog" });
    const close = (value) => {
      dialog.close();
      dialog.remove();
      resolve(value);
    };
    dialog.addEventListener("cancel", (event) => {
      event.preventDefault();
      close(undefined);
    });
    replace(dialog, build(close));
    document.body.append(dialog);
    dialog.showModal();
  });
}

export function confirmDialog({ title, text, items = [], confirmLabel, danger = false }) {
  return showDialog((close) =>
    h("div", { class: "dialog-body" },
      h("h2", {}, title),
      h("p", { class: "dialog-text" }, text),
      items.length > 0 && h("ul", { class: "dialog-list" }, items.map((item) =>
        h("li", { class: item.danger ? "is-danger" : "" }, icon(item.icon), h("span", {}, item.text)))),
      h("div", { class: "dialog-actions" },
        h("button", { class: "button", onclick: () => close(false) }, t("common.cancel")),
        h("button", {
          class: danger ? "button button-danger" : "button button-primary",
          onclick: () => close(true),
        }, confirmLabel),
      ),
    )).then(Boolean);
}

let toastTimer;
export function toast(text) {
  document.querySelector(".toast")?.remove();
  const element = h("div", { class: "toast", role: "status" }, icon("check"), text);
  document.body.append(element);
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => element.remove(), 2600);
}

// A button that shows it is busy and cannot be pressed twice.
export async function withBusy(button, busyLabel, work) {
  const label = button.textContent;
  button.disabled = true;
  button.textContent = busyLabel ?? label;
  try {
    return await work();
  } finally {
    button.disabled = false;
    button.textContent = label;
  }
}
