# Docsweep

## Project goal

Docsweep is a free, open-source desktop app in Rust for macOS, Windows and Linux that collects incoming invoices automatically.
It saves PDF attachments from mails and names them according to a configurable schema.
It downloads invoices from customer portals using login and TOTP, driven by recipes rather than a permanently running AI.
It cleans up the processed mails according to rules, but only after the invoice is provably stored and indexed.
The full concept is in `KONZEPT.md`; its naming conventions and configuration model are binding.

## Workspace

```
crates/
  docsweep-core/     Config, index, mail pipeline, secrets; later recipe engine, scheduler
  docsweep-browser/  Later: CDP connection and recorder (empty for now, depends on core)
  docsweep-cli/      Binary `docsweep`
  docsweep-gui/      Desktop app "Docsweep" (Tauri 2), Cargo binary `docsweep-gui`
    src/             Commands and tray; business logic stays in core
    ui/              Interface: plain ES modules, no framework, no npm, no build step
scripts/             Developer scripts, including the IMAP test runner
```

`docsweep-core` never depends on the browser crate. Later portal fetches dock on through a trait in core.

## Commands

Toolchain: `rust-toolchain.toml` pins the Rust version; use rustup.

```sh
cargo build --workspace
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
scripts/imap-tests.sh                 # IMAP tests against GreenMail (needs Java 21+)
cargo test -p docsweep-core -- --ignored keychain handshake  # real keychain, needs internet
cargo run -p docsweep-cli -- mail-scan --dry-run
cargo run -p docsweep-gui
cargo install tauri-cli --locked      # once, for bundling
(cd crates/docsweep-gui && cargo tauri build)
```

`fmt`, `clippy -D warnings` and `test` must be green before every commit. Linux builds need `libwebkit2gtk-4.1-dev`, `libayatana-appindicator3-dev`, `librsvg2-dev` and `libssl-dev`.

## Conventions

- One commit per working step. Code, identifiers, comments, commit messages and docs are in English; the conversation with the maintainer is in German.
- All user-visible GUI text goes through the translation catalogues in `crates/docsweep-gui/ui/i18n/`. English is the initial language, German follows.
- Names, paths, keychain entries and environment variables come from `docsweep_core::names` and `docsweep_core::paths`. Never repeat those literals elsewhere.
- The Bundle ID has a single source: `identifier` in `crates/docsweep-gui/tauri.conf.json`. `build.rs` exposes it as a constant. It must be final before the first signed build.
- Only password-based IMAP is supported. OAuth2 mailboxes are deliberately out of scope.
- Dry-run and real runs share one planning function (`mail::plan::plan_mail`). Only the executor changes anything, and a dry run only ever gets a `MailReader`.
- A document without a recognised invoice number, or with a known number but a different file, is stored with status `needs_review`, and its mail stays in place with `\Flagged`.
- Link mails stay in place and are only noted as `portal_fetch_required` until portal downloads exist, regardless of the configured disposal.
- The trash folder is found through IMAP `SPECIAL-USE` only. Without it the mail stays flagged; it is never deleted permanently as a fallback.
- A `needs_review` document is completed in the GUI (`review::complete_review`). The next scan then recognises the file as stored and cleans up its mail.
- The interface never receives a secret: commands only accept them and report whether one exists. Text from mails and PDFs is untrusted, so `ui/lib/dom.js` only creates text nodes; never assign HTML strings.
- The web view has only `core:default`. File, mail and keychain access goes through the app's own commands, and those take index ids rather than paths.
- `defaults.output_dir` is the folder for companies without their own `output.dir`; settings that apply to everything belong on the Settings page.
- Unsaved company forms are kept as drafts in the web view's `localStorage`. Never put secrets into anything stored there.
- Core error messages are English for now; they need keys before the German translation.
- Pin GitHub Actions to commit SHAs.

## Security rules (not negotiable)

- No real credentials, mails or invoices in the repo. Tests use fixtures (saved HTML pages, sample mails with dummy PDFs) and a local IMAP test server (GreenMail).
- Passwords, TOTP keys and API keys only through the `keyring` crate. The configuration holds only references (`keyring:<id>`).
- Passwords, TOTP codes and cookies never go to a language model and never into logs or error messages.
- Mails are moved or deleted only after the invoice is provably stored and in the index. "Delete" means moving to the trash by default.
- No captcha circumvention.
- Unencrypted IMAP must stay unreachable from the YAML configuration; it exists only for tests through the internal API.
