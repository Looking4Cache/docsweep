// Runs in the page while a person shows Docsweep a portal. It reports selectors
// and what kind of field something is, and never reads what anybody typed: not
// the content of a field, and no key events. The text of list cells is only used
// here, to tell a date from an amount, and stays in the page.
(() => {
  if (window.top !== window || window.__docsweepRecorderInstalled) return;
  window.__docsweepRecorderInstalled = true;

  const send = (event) => {
    try {
      window.docsweepRecord(JSON.stringify(event));
    } catch {
      // The binding is gone, so recording has ended.
    }
  };

  // Generated names change with each build, so they make bad selectors.
  const isStable = (name) => Boolean(name)
    && !/\d{3,}/.test(name)
    && !/[0-9a-f]{8,}/i.test(name)
    && !/^(css|sc|jss|ng|ember)[-_]/.test(name);

  const isUnique = (selector) => {
    try {
      return document.querySelectorAll(selector).length === 1;
    } catch {
      return false;
    }
  };

  const stableClasses = (element) => [...element.classList].filter(isStable).slice(0, 2);

  const describe = (element) => {
    const tag = element.localName;
    const classes = stableClasses(element).map((name) => `.${CSS.escape(name)}`).join("");
    return `${tag}${classes}`;
  };

  function selectorFor(element) {
    if (isStable(element.id) && isUnique(`#${CSS.escape(element.id)}`)) return `#${CSS.escape(element.id)}`;
    for (const attribute of ["data-testid", "data-test", "data-qa", "name", "autocomplete", "aria-label"]) {
      const value = element.getAttribute(attribute);
      if (value && isStable(value)) {
        const selector = `${element.localName}[${attribute}=${JSON.stringify(value)}]`;
        if (isUnique(selector)) return selector;
      }
    }
    const kind = element.getAttribute("type");
    if (kind && kind.toLowerCase() !== "text") {
      const selector = `${element.localName}[type=${JSON.stringify(kind)}]`;
      if (isUnique(selector)) return selector;
    }
    if (isUnique(describe(element))) return describe(element);

    // The path from the nearest element that can be named.
    const path = [];
    for (let current = element; current && current !== document.documentElement; current = current.parentElement) {
      if (current !== element && isStable(current.id) && isUnique(`#${CSS.escape(current.id)}`)) {
        path.unshift(`#${CSS.escape(current.id)}`);
        break;
      }
      const siblings = current.parentElement ? [...current.parentElement.children] : [current];
      path.unshift(`${describe(current)}:nth-child(${siblings.indexOf(current) + 1})`);
    }
    return path.join(" > ");
  }

  const CODE_NAME = /otp|totp|mfa|2fa|two.?factor|one.?time|verification|auth.?code|security.?code|code/i;
  const USER_NAME = /user|login|mail|account|kunden|benutzer|customer|email|identifier/i;

  function roleOf(field) {
    const type = (field.getAttribute("type") ?? "text").toLowerCase();
    if (type === "password") return "password";
    const autocomplete = (field.getAttribute("autocomplete") ?? "").toLowerCase();
    if (autocomplete.includes("one-time-code")) return "totp";
    const names = `${field.name} ${field.id} ${autocomplete} ${field.getAttribute("aria-label") ?? ""}`;
    if (CODE_NAME.test(names) && !USER_NAME.test(names)) return "totp";
    if (type === "email" || autocomplete.includes("username") || autocomplete.includes("email")) return "user";
    if (USER_NAME.test(names)) return "user";
    // The only text field in a form with a password is its user name.
    const form = field.form;
    if (form?.querySelector("input[type=password]")) return "user";
    return "other";
  }

  const isField = (element) => element instanceof HTMLInputElement
    && !["hidden", "submit", "button", "reset", "image", "checkbox", "radio", "file"]
      .includes((element.getAttribute("type") ?? "text").toLowerCase());

  const reported = new WeakSet();
  const onTyped = (event) => {
    const field = event.target;
    if (!isField(field) || reported.has(field)) return;
    reported.add(field);
    send({ kind: "field", selector: selectorFor(field), role: roleOf(field) });
  };
  addEventListener("input", onTyped, true);
  addEventListener("change", onTyped, true);

  const DATE = /\b\d{1,2}[./-]\d{1,2}[./-]\d{2,4}\b|\b\d{4}-\d{2}-\d{2}\b/;
  const AMOUNT = /\d[\d.,]*\s?(€|EUR|\$|USD|CHF|£)|(€|EUR|\$|USD|CHF|£)\s?\d/;
  const NUMBER = /^[A-Za-z]{0,6}[-_/ ]?\d[\w./-]*$/;

  function cellKind(text) {
    const trimmed = text.trim();
    if (DATE.test(trimmed)) return "date";
    if (AMOUNT.test(trimmed)) return "amount";
    if (NUMBER.test(trimmed)) return "number";
    return "other";
  }

  // The row of a list: a table row, a list item, or one of several alike.
  function rowOf(element) {
    const row = element.closest("tr, li, [role=row]");
    if (row && row.closest("thead") === null) return row;
    for (let current = element.parentElement; current?.parentElement; current = current.parentElement) {
      const alike = [...current.parentElement.children].filter((sibling) => describe(sibling) === describe(current));
      if (alike.length >= 2) return current;
    }
    return null;
  }

  function rowHint(link) {
    const row = rowOf(link);
    if (!row?.parentElement) return null;
    let rows;
    if (row.localName === "tr" && row.closest("table")) {
      const table = row.closest("table");
      const body = row.parentElement.localName === "tbody" ? " tbody" : "";
      rows = `${isStable(table.id) ? `#${CSS.escape(table.id)}` : describe(table)}${body} tr`;
    } else {
      rows = `${selectorFor(row.parentElement)} > ${describe(row)}`;
    }
    const linkClasses = stableClasses(link).map((name) => `.${CSS.escape(name)}`).join("");
    const cells = [...row.children]
      .map((cell, position) => ({ cell, position }))
      .filter(({ cell }) => !cell.contains(link))
      .map(({ cell, position }) => ({
        selector: `${cell.localName}:nth-child(${position + 1})`,
        kind: cellKind(cell.textContent ?? ""),
      }));
    return { rows, link: `${link.localName}${linkClasses}`, cells };
  }

  addEventListener("click", (event) => {
    const element = event.target instanceof Element ? event.target : null;
    const target = element?.closest("a, button, input[type=submit], input[type=button], [role=button]");
    if (!target) return;
    const looksLikeDocument = target.localName === "a" && target.getAttribute("href");
    send({
      kind: "click",
      selector: selectorFor(target),
      row: looksLikeDocument ? rowHint(target) : null,
    });
  }, true);

  addEventListener("submit", (event) => {
    send({ kind: "submit", selector: event.submitter ? selectorFor(event.submitter) : null });
  }, true);
})();
