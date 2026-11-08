# Docsweep

## Project goal

Docsweep is a free, open-source desktop app in Rust for macOS, Windows and Linux that collects incoming invoices automatically.
It saves PDF attachments from mails and names them according to a configurable schema.
It downloads invoices from customer portals using login and TOTP, driven by recipes rather than a permanently running AI.
It cleans up the processed mails according to rules, but only after the invoice is provably stored and indexed.
The naming conventions and the configuration model are binding; `names.rs`, `paths.rs` and `config.rs` define them.

`KONZEPT.md` and `BOTSCHUTZ.md` are the maintainer's private notes in the working directory. They must never be committed, and changes to them are left to the maintainer.

## Workspace

```
crates/
  docsweep-core/     Config, index, mail pipeline, secrets, recipes, recipe engine, schedules, automation and AI
    src/portal/      Recipe model, login secrets, `BrowserDriver`, `ConfirmationHandler` and `PortalFetcher` traits, engine, `hosts`, `draft`, `recording`
    src/pickup.rs    Assisted mode: `PickupSession`, `DownloadsFolder`, `import_file`
    src/ai/          `sanitize`, providers, prompts, checks of model output, recipe generation
  docsweep-browser/  Installed Chrome/Edge through CDP (chromiumoxide), `fetch_company`, `BrowserFetcher`, `read_page`, `Recorder`
    tests/           Real-browser tests against a fictional portal; `tests/fixtures/portal` holds its pages
  docsweep-cli/      Binary `docsweep`
  docsweep-gui/      Desktop app "Docsweep" (Tauri 2), Cargo binary `docsweep-gui`
    src/             Commands and tray; business logic stays in core
    ui/              Interface: plain ES modules, no framework, no npm, no build step
scripts/             Developer scripts: IMAP and browser test runners
```

`docsweep-core` never depends on the browser crate. Portal fetches dock on through the traits in `portal::driver`, so the engine is tested with a fake driver in ordinary `cargo test`. The CLI and GUI call `docsweep_browser::fetch_company`, and hand `docsweep_browser::BrowserFetcher` to real scans and automatic runs.

## Commands

Toolchain: `rust-toolchain.toml` pins the Rust version; use rustup.

```sh
cargo build --workspace
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
scripts/imap-tests.sh                 # IMAP tests against GreenMail (needs Java 21+)
scripts/browser-tests.sh              # headless Chrome against a fictional portal (needs Chrome or Edge)
scripts/browser-tests.sh --visible    # also the test that opens a browser window
cargo test -p docsweep-core -- --ignored keychain handshake  # real keychain, needs internet
cargo run -p docsweep-cli -- mail-scan --dry-run
cargo run -p docsweep-cli -- fetch <company> --dry-run
cargo run -p docsweep-cli -- import <company> <file.pdf> --dry-run
cargo run -p docsweep-cli -- test-recipe <file> [--company <id>]
cargo run -p docsweep-cli -- recipe generate <company> --url <url> [--describe "..."]
cargo run -p docsweep-cli -- recipe record <company> --url <url>   # opens a window
cargo run -p docsweep-gui
DOCSWEEP_CODESIGN_IDENTITY=<hash> cargo run -p docsweep-gui  # macOS: sign first, so the keychain's "Always Allow" survives rebuilds
cargo install tauri-cli --locked      # once, for bundling
(cd crates/docsweep-gui && cargo tauri build)
```

`fmt`, `clippy -D warnings` and `test` must be green before every commit. Linux builds need `libwebkit2gtk-4.1-dev`, `libayatana-appindicator3-dev`, `librsvg2-dev` and `libssl-dev`.

## Conventions

- One commit per working step. Code, identifiers, comments, commit messages and docs are in English; the conversation with the maintainer is in German.
- All user-visible GUI text goes through the translation catalogues in `crates/docsweep-gui/ui/i18n/` (`en.json`, `de.json`). Every key needs both, with the same `{placeholders}`; a test checks that. The language is chosen on the Settings page (automatic follows the system) and kept in the web view's storage; the web view reports it through `set_language`, so the tray follows. German texts address the user with "du", and a link mail is a "Hinweis-Mail".
- Names, paths, keychain entries and environment variables come from `docsweep_core::names` and `docsweep_core::paths`. Never repeat those literals elsewhere.
- The Bundle ID has a single source: `identifier` in `crates/docsweep-gui/tauri.conf.json`. `build.rs` exposes it as a constant. It must be final before the first signed build.
- Only password-based IMAP is supported. OAuth2 mailboxes are deliberately out of scope.
- Dry-run and real runs share one planning function (`mail::plan::plan_mail`). Only the executor changes anything, and a dry run only ever gets a `MailReader`.
- A document without a recognised invoice number, or with a known number but a different file, is stored with status `needs_review`, and its mail stays in place with `\Flagged`.
- A real scan notes a link mail as `portal_fetch_required` and then starts its company's recipe through `PortalFetcher`, once per company per scan and after all accounts were read, because a fetch can wait minutes for a person. A preview has no `PortalFetcher`, so it cannot start a browser.
- A link mail is cleaned up (`disposal_for` with `MailKind::Link`) only if the fetch had no failures and `Index::invoice_for_link_mail` finds a stored portal invoice for it: newer than the mail's row, or dated on or after the mail, with its file still there. Each invoice settles at most one link mail: `Index::link_assignment` pairs all waiting link mails of a company at once, closest dates first, and `resolve_link_mail` records the invoice in `settled_by`, after which it settles no other mail. The pick-up list uses the same pairing. A successful fetch that finds nothing leaves the mail unflagged and noted; a failed fetch flags it. `resolved_at` marks a row as done (`Index::link_mail_closure`), and a closed row's mail is never noted again, fetched for or cleaned up. A mail that is still waiting is retried at most once per local day (`company_runs.last_link_fetch_on`); a new mail does not wait for that. A recipe company's link mail that has waited `plan::LINK_MAIL_GIVE_UP_DAYS` (15, deliberately not configurable) since its date, or since it was noted, is given up in `plan_mail` (`give_up_after_days`, so a dry run shows it): no more downloads, `given_up_at` resolves the row without an invoice, and the mail is flagged once and then left alone. A mail seen for the first time always gets its download first. Assisted companies never give up; they have the Done button.
- `portal.mode: assisted` is for portals that reject automated browsers. Docsweep then automates nothing: `PickupSession` (`pickup.rs`) opens `portal.url` from the configuration in the default browser, and a person logs in and downloads there. Never add stealth, flag removal, attaching to the person's browser, reading its cookies or solving captchas to this or any other mode, and do not inspect that browser in any way. Assisted companies have no `credentials`, `totp` or `schedule`, and validation refuses them. Switching modes is a person's choice. A failed run whose page still shows the recipe's `confirmation.detect` or `looks_like_captcha` carries `FetchReport::bot_check`; the GUI then offers to open the company editor with assisted mode and the recipe's first `goto` filled in, for the person to review and save, and the CLI report says so. This only looks at the page, never switches by itself and never deletes keychain entries.
- A person can mark a waiting pick-up as done (the Done button, `pickup::dismiss_pickup`, after a confirmation): it resolves the row with `dismissed_at` and touches neither the mailbox nor an invoice, so `disposed_at` and `settled_by` stay empty.
- For an assisted company a scan only notes the link mail: `fetch_due` is false, `fetch_for_link_mails` and `due_work` skip it, and `fetch_company` refuses it. `decide_link_mails` has no fetch to wait for, so the index alone decides. Automatic runs change the count in the navigation and nothing else; only a person's click starts a session.
- A session looks at the downloads folder only between that click and its end, or `SESSION_LIMIT_MINUTES`. Past that, `poll` does not touch the folder. It takes what appeared since the start (`known`, compared by size and time, not with the clock, since a new file can carry an old time), once it stayed the same for two looks. File contents are opened through `DownloadsFolder::read` only for regular, finished, top-level `.pdf` files; tests watch that with a spy folder.
- Nothing from a downloads folder is imported without `PickupSession::confirm`, which takes a name the session offered and refuses a file that changed since. `pickup::import_file` is the one import, for the app and `docsweep import`: `store_verified`, the index, and only then the original goes. On any failure or duplicate the original stays. A missing number or date means `needs_review`. Files are never converted.
- A dry run reads an older index without migrating it, so every read of a newer column or table must handle its absence (`AUTOMATION_VERSION`, `PORTAL_KEY_VERSION`, `SETTLED_BY_VERSION`, `DISMISSED_VERSION`, `GIVEN_UP_VERSION`).
- All automation is off unless `automation.enabled` is set; `portal.schedule` and `mail.poll_minutes` do nothing without it. `automation::run_tick` decides from a clock passed in and the index, and the GUI's loop (`src/automation.rs`) only supplies them. Only `RunGate` lets a run start, for scans, previews, manual fetches and ticks alike, and what cannot start stays due. A scheduled run counts as soon as it starts, so a failure waits for the next date instead of repeating a login.
- Autostart is only driven by the app's own commands; the web view gets no permission for it. A start at login passes `--hidden` and starts in the tray.
- `portal.schedule` is validated by `schedule::Schedule`: exactly five fields, and it must come true at some point. The GUI's own words for it are in `ui/lib/schedule.js`, so they go through the catalogues.
- Recipes hold no secrets and live in the recipes directory; relative `portal.recipe` paths start there, and a leading `recipes/` does not count twice. The format is in `README.md` and `portal/recipe.rs`.
- `portal.credentials` holds `{"user":"...","password":"..."}` in one keychain entry, `portal.totp` the Base32 setup key (`portal/credentials.rs`). `serde_json` errors are never passed on, because they can quote their input.
- Portal runs share the mail pipeline's rules: `storage::store_verified`, `FilenameTemplate`, `extract_facts`, `check_duplicate`, `needs_review`. The number and date in the file win, and those of the list only fill gaps. The column `portal_key` recognises a list entry on later runs.
- A login submits at most once per run, so wrong credentials cannot lock an account. Credentials only go to `https` pages or to this computer. Links from pages are followed only if they are `http` or `https`.
- Never use `BrowserConfig::hide`, `Page::enable_stealth_mode`, `no_sandbox` or anything like them, and keep `respect_https_errors`: chromiumoxide ignores certificate errors unless told otherwise. When a portal wants a person, `ConfirmationHandler` asks one.
- Driver errors are built from fixed texts and selectors. Filled values, cookies and document addresses never appear in them or in reports, which end up in the history. No `tracing` subscriber is installed, because chromiumoxide traces its protocol messages, which include what is typed.
- Real-browser tests are `#[ignore]`, run with `WindowMode::Hidden`, and contact nothing but their own local server.
- The trash folder is found through IMAP `SPECIAL-USE` only. Without it the mail stays flagged; it is never deleted permanently as a fallback.
- A `needs_review` document is completed in the GUI (`review::complete_review`). The next scan then recognises the file as stored and cleans up its mail.
- The interface never receives a secret: commands only accept them and report whether one exists. Text from mails and PDFs is untrusted, so `ui/lib/dom.js` only creates text nodes; never assign HTML strings.
- The web view has only `core:default`. File, mail and keychain access goes through the app's own commands, and those take index ids rather than paths.
- AI is optional, and a portal run never needs it. Only `ai::sanitize::sanitize` can build a `SanitizedPrompt`, and `LlmProviderClient` takes nothing else, so all data for a provider passes through that one function. It copies from an allow-list instead of removing things. Add to the allow-list only what a selector needs. Instructions in `ai/prompts.rs` hold no data.
- A provider's error body is never read, because it could echo the key: errors are fixed texts by status code. Providers do not follow redirects. Endpoints are constants, and tests pass their own to `Anthropic::new` or `OpenAiCompatible::new`; no configuration or environment variable may change them. `llm.model` is required for OpenAI and OpenRouter, because Docsweep does not guess their model names; `provider::ANTHROPIC_DEFAULT_MODEL` is the only hard-coded model.
- Model output and page content are untrusted. `ai::output` only parses into the typed recipe model. A repair may change selectors of a `login` or `extract_list` step only: not its kind, list fields, `empty`, `max_pages` or whether a TOTP field exists. A model cannot set `ai_fallback`.
- `portal::hosts::HostBinding` is the only check for hosts in recipes from elsewhere. Anything that leaves the known hosts needs a person who saw the old and the new host; automated paths refuse it instead. Shared recipes must use it too.
- Repair (`Run::step_with_repair`) needs `ai_fallback: repair_step`, a configured provider, and no earlier repair in the run. It never runs after a submit, for a dry run, on a captcha or a detected confirmation, or on a page outside the recipe's hosts, and the first send per company and provider needs `ConfirmationHandler::approve_ai_send` (the default answer is no; consent is in the index table `ai_consents`). An automatic run (`RunOrigin::Automatic`, set on `BrowserFetcher` and `FetchOptions` by the GUI's automation loop and carried into `FetchJob::with_origin`) asks `approve_ai_send` for every send; a stored consent does not count there, and its answers are never written to `ai_consents`. Every construction of `BrowserFetcher` or `FetchOptions` names its origin. The recipe is only saved after the whole run succeeded with the new step, through `portal::draft::save_repaired_step`, which keeps a `.bak` and leaves a file alone that changed during the run. A repaired login is only typed into pages on the recipe's hosts, and the password only into `[type=password]`.
- The recorder's listener (`docsweep-browser/src/recorder.js`) must never read a typed value, and a test checks its text for that. Its events come from a page, so `portal::recording::build_draft` checks selectors and addresses again. Recorder and AI only ever write draft files; a person decides whether to use them.
- `defaults.output_dir` is the folder for companies without their own `output.dir`; settings that apply to everything belong on the Settings page.
- Unsaved company forms are kept as drafts in the web view's `localStorage`. Never put secrets into anything stored there.
- Core error messages are still English, also in the German interface; they need keys to be translated.
- Pin GitHub Actions to commit SHAs.

## Security rules (not negotiable)

- No real credentials, mails or invoices in the repo. Tests use fixtures (saved HTML pages, sample mails with dummy PDFs) and a local IMAP test server (GreenMail).
- Passwords, TOTP keys and API keys only through the `keyring` crate. The configuration holds only references (`keyring:<id>`).
- Passwords, TOTP codes, TOTP keys, cookies, session tokens and API keys never go to a language model and never into logs, error messages or reports.
- No request to a real provider, portal or mailbox in tests: providers are `ai::test_support::FakeProviderServer`.
- Mails are moved or deleted only after the invoice is provably stored and in the index. "Delete" means moving to the trash by default.
- No captcha circumvention.
- Unencrypted IMAP must stay unreachable from the YAML configuration; it exists only for tests through the internal API.
