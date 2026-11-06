// The only place that talks to the Rust side.

const invoke = (command, args) => window.__TAURI__.core.invoke(command, args);

export const api = {
  getState: () => invoke("get_state"),
  saveConfig: (config) => invoke("save_config", { config }),
  setSecret: (id, value) => invoke("set_secret", { id, value }),
  testMailbox: (account) => invoke("test_mailbox", { account }),
  runScan: (dryRun) => invoke("run_scan", { dryRun }),
  listInvoices: () => invoke("list_invoices"),
  listRuns: () => invoke("list_runs"),
  completeReview: (id, invoiceNo, date) => invoke("complete_review", { id, invoiceNo, date }),
  revealInvoice: (id) => invoke("reveal_invoice", { id }),
  pickFolder: () => invoke("pick_folder"),
};

export function onTrayPreview(handler) {
  window.__TAURI__.event.listen("tray-preview", handler);
}

export function errorText(error) {
  return error?.message ?? String(error);
}
