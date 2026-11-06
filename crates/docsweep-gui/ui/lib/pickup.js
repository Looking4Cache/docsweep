// Assisted pick-ups: the person downloads in their own browser, and these dialogs
// ask about each new PDF. File names and PDF text are untrusted, so they only ever
// go in as text.

import { api, errorText, onPickupStatus } from "./api.js";
import { fileName, h, replace } from "./dom.js";
import { formatDate, t } from "./i18n.js";
import { notice, showDialog, toast } from "./ui.js";

const REVIEW_NOTES = {
  review_no_number: "pickup.review_no_number",
  review_no_date: "pickup.review_no_date",
  review_number_conflict: "pickup.review_conflict",
};

function duplicateText({ existing, reason }) {
  if (existing) return t("pickup.duplicate", { file: fileName(existing) });
  return reason ? t("pickup.duplicate_reason", { reason }) : t("pickup.duplicate_known");
}

function candidateDialog(app, session, candidate) {
  const { file_name: name, plan } = candidate;
  const duplicate = plan.decision.kind === "duplicate";
  const review = REVIEW_NOTES[plan.decision.kind];
  return showDialog((close) => {
    const message = h("div", {});
    const body = h("div", { class: "dialog-body" });

    // Whatever else a dialog ends in, a file nobody confirmed stays where it is.
    const finishWith = (tone, text) => {
      replace(body,
        h("h2", {}, t("pickup.result_title")),
        notice(tone, text),
        h("div", { class: "dialog-actions" },
          h("button", { class: "button button-primary", onclick: () => close("answered") }, t("common.close"))));
    };

    const yes = h("button", {
      class: "button button-primary",
      onclick: async () => {
        yes.disabled = true;
        replace(message);
        try {
          const { outcome, session: next } = await api.confirmPickup(name);
          app.setPickupSession(next);
          app.refreshBadges();
          if (outcome.kind === "duplicate") {
            finishWith("warning", duplicateText(outcome));
          } else if (outcome.original_kept) {
            finishWith("warning", t("pickup.original_kept", { file: fileName(outcome.path), reason: outcome.original_kept }));
          } else {
            toast(t(outcome.needs_review ? "pickup.saved_review" : "pickup.saved", { file: fileName(outcome.path) }));
            close("answered");
          }
        } catch (error) {
          replace(message, notice("danger", errorText(error)));
          yes.disabled = false;
        }
      },
    }, t("pickup.confirm"));

    replace(body,
      h("h2", {}, t("pickup.confirm_title", { name: session.company_name })),
      h("p", { class: "dialog-text" }, t("pickup.confirm_lead")),
      h("dl", { class: "about" },
        h("dt", {}, t("pickup.file")), h("dd", { class: "doc-name" }, name),
        h("dt", {}, t("review.number")), h("dd", {}, plan.invoice_no ?? t("pickup.not_found")),
        h("dt", {}, t("review.date")), h("dd", {}, plan.date ? formatDate(plan.date) : t("pickup.not_found")),
        !duplicate && h("dt", {}, t("pickup.saved_as")),
        !duplicate && h("dd", { class: "doc-name" }, fileName(plan.target_path)),
      ),
      duplicate && notice("warning", duplicateText(plan.decision)),
      review && notice("warning", t(review)),
      message,
      h("div", { class: "dialog-actions" },
        h("button", { class: "button", onclick: () => close("no") }, t(duplicate ? "pickup.leave" : "pickup.not_this")),
        !duplicate && yes,
      ),
    );
    return body;
  });
}

let asking = false;

async function askAboutNext(app) {
  if (asking) return;
  asking = true;
  try {
    for (;;) {
      const session = app.pickupSession;
      const candidate = session?.candidates?.[0];
      if (!candidate) break;
      const answer = await candidateDialog(app, session, candidate);
      if (answer !== "answered") {
        // Saying no, or pressing Escape: the file stays in the folder.
        app.setPickupSession(await api.ignorePickup(candidate.file_name));
      }
    }
  } catch (error) {
    app.pickupError = errorText(error);
    app.repaintScan?.();
  } finally {
    asking = false;
  }
}

export function watchPickups(app) {
  const follow = (session) => {
    if (app.pickupSession?.watching && !session?.watching) toast(t("pickup.time_up"));
    app.setPickupSession(session);
    askAboutNext(app);
  };
  onPickupStatus(follow);
  api.pickupStatus().then(follow);
}

// A click on an item: opens the portal in the default browser and starts looking.
export async function startPickup(app, companyId) {
  app.pickupError = null;
  try {
    app.setPickupSession(await api.startPickup(companyId));
  } catch (error) {
    app.pickupError = errorText(error);
    app.setPickupSession(null);
  }
}

export async function endPickup(app) {
  await api.endPickup();
  app.setPickupSession(null);
}
