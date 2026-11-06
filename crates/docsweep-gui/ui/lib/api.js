// The only place that talks to the Rust side.

const invoke = (command, args) => window.__TAURI__.core.invoke(command, args);

export const api = {
  getState: () => invoke("get_state"),
  saveConfig: (config) => invoke("save_config", { config }),
  setSecret: (id, value) => invoke("set_secret", { id, value }),
  setPortalCredentials: (id, user, password) => invoke("set_portal_credentials", { id, user, password }),
  setTotpKey: (id, key) => invoke("set_totp_key", { id, key }),
  testMailbox: (account) => invoke("test_mailbox", { account }),
  runScan: (dryRun) => invoke("run_scan", { dryRun }),
  runFetch: (companyId, dryRun, visible) => invoke("run_fetch", { companyId, dryRun, visible }),
  answerConfirmation: (done) => invoke("answer_confirmation", { done }),
  prepareRecipe: (companyId, address, description) => invoke("prepare_recipe", { companyId, address, description }),
  sendRecipePrompt: () => invoke("send_recipe_prompt"),
  saveRecipeDraft: (hostsConfirmed, overwrite) => invoke("save_recipe_draft", { hostsConfirmed, overwrite }),
  discardRecipeWork: () => invoke("discard_recipe_work"),
  recordRecipe: (companyId, address) => invoke("record_recipe", { companyId, address }),
  finishRecording: () => invoke("finish_recording"),
  listInvoices: () => invoke("list_invoices"),
  listRuns: () => invoke("list_runs"),
  completeReview: (id, invoiceNo, date) => invoke("complete_review", { id, invoiceNo, date }),
  revealInvoice: (id) => invoke("reveal_invoice", { id }),
  pickFolder: () => invoke("pick_folder"),
  automationStatus: () => invoke("automation_status"),
  describeSchedule: (expression) => invoke("describe_schedule", { expression }),
  autostartEnabled: () => invoke("autostart_enabled"),
  setAutostart: (enabled) => invoke("set_autostart", { enabled }),
};

export function onTrayPreview(handler) {
  window.__TAURI__.event.listen("tray-preview", handler);
}

export function onPortalConfirmation(handler) {
  window.__TAURI__.event.listen("portal-confirmation", (event) => handler(event.payload));
}

export function onAiSendConfirmation(handler) {
  window.__TAURI__.event.listen("ai-send-confirmation", (event) => handler(event.payload));
}

export function onPortalConfirmationEnded(handler) {
  window.__TAURI__.event.listen("portal-confirmation-ended", handler);
}

export function onAutomationStatus(handler) {
  window.__TAURI__.event.listen("automation-status", (event) => handler(event.payload));
}

export function errorText(error) {
  return error?.message ?? String(error);
}
