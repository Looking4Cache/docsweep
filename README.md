# Docsweep

Docsweep is a free desktop app for macOS, Windows and Linux that collects incoming invoices for you. It saves PDF attachments from your mailbox under a consistent file name, fetches invoices from customer portals with recipes, keeps an index to catch duplicates, and tidies up the mails afterwards.

> **Status: early development.** The mail pipeline, portal downloads, link mails that start a download, schedules, the recipe recorder, optional AI help for recipes, the `docsweep` command line tool and a first version of the desktop app work. There are no signed builds so far.

## How it works

1. You describe each company in `docsweep.yaml`: which mails belong to it and how invoices are named.
2. `docsweep mail-scan --dry-run` shows what would happen, without changing anything.
3. `docsweep mail-scan` saves the invoices and cleans up the mails.

A mail is only moved or deleted after its invoice is stored and indexed. By default, "delete" means moving it to the trash. A mail that only links to a portal starts that company's download, and is cleaned up once a matching invoice is stored. Until then it stays in your inbox. For portals that turn away automated browsers, you download the invoice yourself and Docsweep files it; see [Assisted mode](#assisted-mode).

Only IMAP with a password or app password is supported. Mailboxes that require OAuth2, such as Microsoft 365, will not work.

## Getting started

Build from source with [rustup](https://rustup.rs). The pinned Rust version installs automatically.

```sh
cargo run -p docsweep-gui                # the desktop app, which guides you through the setup
cargo build --release -p docsweep-cli    # the command line tool
```

The rest of this section describes the command line tool.

Store your mail password in the system keychain. Docsweep asks for it without echoing it:

```sh
docsweep secret set mail-invoices
```

Create the configuration file, or point `DOCSWEEP_CONFIG` or `--config` at it:

| System | Location |
| --- | --- |
| macOS | `~/Library/Application Support/Docsweep/docsweep.yaml` |
| Windows | `%APPDATA%\Docsweep\docsweep.yaml` |
| Linux | `~/.config/docsweep/docsweep.yaml` |

```yaml
defaults:
  output_dir: ~/Invoices/Inbox   # for companies without their own folder

mail:
  accounts:
    - id: invoices
      imap: imap.example.com:993
      user: invoices@example.com
      password: keyring:mail-invoices
      watch: INBOX
  disposal:
    mode: pdf_move_link_delete   # all_delete | all_move | pdf_move_link_delete
    move_to: Invoices/Done
    delete_to_trash: true

companies:
  - id: hetzner
    name: Hetzner
    mail: { from_domain: hetzner.com, attachment: "(?i).*\\.pdf" }
    output:
      filename: "{date:%Y-%m-%d} - Hetzner - {invoice_no}.pdf"
    disposal: { move_to: Invoices/Hetzner }
```

Then try it:

```sh
docsweep mail-scan --dry-run
```

## Portal downloads

A company with a `portal` gets its invoices from a customer portal: Docsweep logs in, reads the list of invoices, compares it with the index and downloads only what is new. This needs Chrome or Edge to be installed, and runs without any AI. Each company has its own browser profile, so sessions persist.

```yaml
companies:
  - id: example
    name: Example
    portal:
      credentials: keyring:example        # user name and password
      totp: keyring:example-totp          # optional: setup key of the authenticator app
      recipe: example.yaml                # relative to the recipes folder of the data directory
      params: { domain: portal.example.com }  # optional: values for the recipe's parameters
    output:
      filename: "{date:%Y-%m-%d} - Example - {invoice_no}.pdf"
```

```sh
docsweep secret set-portal example        # asks for the user name, and for the password without echoing it
docsweep secret set-totp example-totp     # asks for the setup key without echoing it
docsweep test-recipe recipe.yaml          # checks a recipe file
docsweep test-recipe recipe.yaml --company example   # also tries it as a dry run
docsweep fetch example --dry-run          # lists what is new; downloads nothing
docsweep fetch example                    # add --visible to watch the browser
```

The credentials entry in the keychain holds `{"user":"...","password":"..."}`. A dry run does log in, so the browser profile changes, but no invoice is downloaded and neither the folder nor the index is touched.

A recipe describes one portal. Selectors are CSS; in a field, `selector` reads the text and `selector@attribute` an attribute.

```yaml
params: { domain: portal.example.com }  # optional: `{domain}` in `goto` addresses
step_timeout_secs: 30                   # optional: how long a step waits for the page
confirmation:                           # optional
  detect: "#captcha, .confirm-by-mail"  # shows that a person is needed
  timeout_secs: 300                     # how long to wait for them
steps:
  - goto: https://{domain}/login
  - dismiss_cookie_banner: true         # or the selector of the button
  - login:
      user: "#username"
      next: "#continue"                 # optional: button if the password is on a second page
      password: "#password"
      submit: "button[type=submit]"
      totp: "#otp"                      # optional
      totp_submit: "#verify"            # optional: button for the code, otherwise `submit`
      logged_in: "a.logout"             # recommended: shows that a saved session is still active
  - goto: https://{domain}/invoices
  - extract_list:
      rows: "table.invoices tbody tr"
      empty: ".no-invoices"             # optional: shows that the list is really empty
      next_page: "a.next"               # optional: the list has several pages
      max_pages: 50                     # optional
      fields:
        date: "td:nth-child(1)"
        invoice_no: "td:nth-child(2)"
        amount: "td:nth-child(3)"
        url: "a.download@href"
  - download_new:
      key: invoice_no                   # the field that identifies a document across runs
      html_to_pdf: true                 # optional: print documents that are HTML pages
```

If each row leads to a page with its documents, as with orders, the list gets an `open_detail` section instead of a `url` field. An order without any document is reported as missing, and the run goes on:

```yaml
  - extract_list:
      rows: li.order
      fields: { order: .number, details: a.details@href }
      open_detail:
        url: details                    # the field with the link
        rows: li.document
        missing: .no-documents          # optional: shows that there is no document
        fields: { invoice_no: .number, url: a@href }
  - download_new: { key: invoice_no }
```

Docsweep never tries to get around a captcha or bot detection. If the `detect` selector appears, or a step waits too long, the browser becomes visible and you are asked in the terminal or in a dialog of the app. If nobody answers in time, the run ends as failed for that company; what was stored until then stays. A login is never submitted twice in one run, and credentials only go to `https` addresses.

Documents are fetched from inside the page, so the browser's rules apply: a document on another host only works if that host allows it. Number and date in the file take priority over those in the list, and a file without any number is stored for review, just as with mails.

## Link mails

A mail that matches a company with a portal but has no PDF is a link mail. In a real scan, by `docsweep mail-scan` or in the app, it starts that company's recipe, at most once per company per scan. A preview or dry run never starts a browser; it only says that a download would start.

The mail is only cleaned up by the normal rules (`disposal`, the trash by default) if both of these hold:

- the download finished without failures, and
- the index holds a stored portal invoice of that company that matches the mail: its entry is newer than the mail's, or its invoice date is on or after the mail's date.

A successful download that found nothing new is not enough, because the invoice may not be in the portal yet. The mail then stays in place, unflagged, and the portal is checked again at most once per day. A new link mail does not wait for that. A failed download leaves the mail in place and flags it.

## Assisted mode

Some portals use a bot check such as Cloudflare Turnstile, which rejects browsers under automation even with a person clicking. Docsweep does not hide its automation and solves no captchas. For these portals it automates nothing: you log in and download in your usual browser, and Docsweep files the PDF.

```yaml
companies:
  - id: example
    name: Example
    mail: { from_domain: example.com, subject: "(?i)rechnung" }
    portal:
      mode: assisted                     # recipe (the default) | assisted
      url: https://portal.example.com/login
    output:
      filename: "{date:%Y-%m-%d} - Example - {invoice_no}.pdf"
```

`portal.url` is required and must be `http` or `https`. An assisted company has no `credentials` or `totp`, because Docsweep keeps no login for it, and no `schedule`, because nothing runs on its own. Switching a company to this mode is a choice in its editor, or in this file; Docsweep does not do it by itself.

1. A scan notes a link mail of this company and starts nothing, whether you started the scan or it runs automatically.
2. The Scan page lists it under "Invoices to pick up", with a count in the navigation. Automatic runs do not open a browser or the window for it.
3. "Pick up invoice" opens `portal.url` in your default browser. It does not attach to that browser, read its cookies or look at its pages.
4. From that click until you end it, for at most 15 minutes, Docsweep looks once a second for new `.pdf` files directly in the downloads folder. It waits until a file stops growing, skips partial downloads (`.crdownload`, `.part`, `.download`), subfolders and symlinks, opens nothing else, and checks that the content really is a PDF.
5. For each new PDF it brings its window up and asks whether this is an invoice of the company, showing the file name, the number and the date it read. Nothing is filed without your yes. A missing number or date means the file is saved for review, to complete under Invoices.
6. The file is saved byte for byte under the company's file name, with the same duplicate check as a mail attachment. Only once it is in the index is the original deleted. A duplicate is reported, and its original stays where it is.
7. The next scan finds the stored invoice and cleans up the link mail, under the rule for link mails above.

The folder is `defaults.downloads_dir`, or under Settings; without it, your system's downloads folder.

```yaml
defaults:
  downloads_dir: ~/Downloads
```

The same import works from the command line, for testing and scripting. Naming the file counts as the confirmation, so the original is deleted too once the invoice is indexed:

```sh
docsweep import example ~/Downloads/invoice.pdf --dry-run
docsweep import example ~/Downloads/invoice.pdf
```

## Automatic runs

All automation is off until you turn it on. In the app, that is the section "Automatic runs" under Settings; in the file, it looks like this:

```yaml
automation:
  enabled: true          # the master switch; schedules and polling do nothing without it

mail:
  poll_minutes: 30       # optional: check the mailboxes every 30 minutes, at least 5
  # accounts and disposal as above

companies:
  - id: example
    portal:
      schedule: "0 9 10 * *"   # optional: on the 10th at 9:00 local time
      # credentials and recipe as above
```

**Automatic runs save invoices and clean up mails without a preview.** They only happen while the app is open; closing its window keeps it in the tray, and it goes on working there. "Start Docsweep at login" under Settings starts it in the tray. The command line tool has no long-running mode.

`portal.schedule` is a cron expression with five fields: minute, hour, day, month, weekday. An invalid one is refused when the configuration is loaded. Dates missed while the app was closed are caught up once at the next start, not once per date. A new schedule starts counting when it is first seen. A scheduled run that fails waits for its next date. Only one run happens at a time, whether started by hand or automatically; one that has to wait starts right afterwards.

If an automatic run needs a person, the window comes up with the usual dialog and its time limit. If nobody answers, the run fails for that company and the others go on. Automatic runs are marked in the history, except for checks that found nothing to do, and problems show as a badge on "History" and a notice on the Scan page.

## Recording and AI help

A portal run needs no AI. If you would rather not write a recipe by hand, there are two ways to get a draft, written to `<company>.draft.yaml` in the recipes folder. An existing file is only replaced if you agree. Check a draft with `docsweep test-recipe <file> --company <id>`, then point `portal.recipe` at it. In the app, both are in the company editor under "Make a recipe".

```sh
docsweep recipe record example --url https://portal.example.com/login
docsweep recipe generate example --url https://portal.example.com/login --describe "Invoices are under Billing"
```

**Recording** opens a browser window with the company's profile. Log in as usual, go to your invoices and click one document link in the list. Docsweep notes selectors and what kind of field you used, and never what you type: a password field and a code field become the `login` step, with the app filling in the values at each run. No page content and no cookies are kept. The draft says in its comments what is still to do, such as adding `logged_in`.

**Generating** and **repairing** use a language model with your own API key. Without an `llm` section, nothing is ever sent. In the app, it is set up under Settings, in the card "AI help", where the key goes straight into the system keychain.

```yaml
llm:
  provider: anthropic        # anthropic, openai or openrouter
  api_key: keyring:llm-key   # store it with: docsweep secret set llm-key
  model: claude-sonnet-5-5   # required for openai and openrouter; anthropic defaults to claude-fable-5-1, its most capable and expensive model
```

For Anthropic, Docsweep also asks the API to rerun a request on another model if a safety classifier wrongly declines it (`fallbacks: "default"`).

To let a recipe be repaired, add `ai_fallback: repair_step` to its file (`off` is the default; `full_agent` is not available). If a `login` or `extract_list` step no longer matches, the provider gets one chance to suggest new selectors, at most once per run. The recipe is only saved once the whole run worked with the new step, the previous file stays as `<recipe>.bak`, and the report and history say so. A dry run sends nothing and only says that a repair would be possible. A login that was already submitted is not repaired in that run, because the credentials would go out twice.

What protects you:

- The model never sees passwords, TOTP codes, cookies, session tokens or keys. Everything sent goes through one sanitiser: of a page, only tag names, a few attributes that selectors need, and text remain. Field values, hidden fields, scripts, styles, comments, query strings and images are gone, and mail addresses and long digit runs such as IBANs are replaced. **Names and postal addresses in page text can remain.** So you see exactly this text and have to agree: each time you generate, and before the first repair per company and provider. After that, repairs go without asking, including in automatic runs, and each is reported.
- Page content and answers are untrusted, because a page can try to talk a model into something. An answer is only parsed into the recipe format and validated; it cannot run anything. A repair can only change selectors of its own step, and the password only goes into a real password field. A generated recipe with a `goto` or `login` on a new host is only kept if you confirm it after seeing the old and new host; a repair cannot change a host at all, and sends nothing while the browser is on a host the recipe does not know.
- A captcha goes to you as usual, never to a model. Downloads still go through the index comparison, the duplicate check and normal storage.

## Development

See [CLAUDE.md](CLAUDE.md) for the workspace layout, commands and rules, and [KONZEPT.md](KONZEPT.md) for the full concept (in German).

## Kurzfassung auf Deutsch

Docsweep sammelt Eingangsrechnungen automatisch ein: PDF-Anhänge aus Mails werden einheitlich benannt gespeichert und im Index erfasst, danach werden die Mails nach Regeln aufgeräumt. Aus Kundenportalen lädt Docsweep Rechnungen per Rezept, von Hand, ausgelöst durch Weblink-Mails oder nach Zeitplan. Portale, die automatisierte Browser abweisen, bleiben im assistierten Modus Handarbeit: Man lädt die Rechnung im eigenen Browser herunter, und Docsweep legt das PDF nach einer Rückfrage ab. Automatische Läufe sind aus, bis man sie einschaltet. Rezepte lassen sich im Browser aufzeichnen, und mit eigenem API-Schlüssel kann ein Sprachmodell Entwürfe schreiben und einzelne Schritte reparieren; es sieht nie Passwörter, Codes oder Cookies. Das Projekt steht noch am Anfang. Unterstützt wird nur IMAP mit Passwort, kein OAuth2.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.
