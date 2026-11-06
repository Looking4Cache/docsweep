# Docsweep

Docsweep is a free desktop app for macOS, Windows and Linux that collects incoming invoices for you. It saves PDF attachments from your mailbox under a consistent file name, fetches invoices from customer portals with recipes, keeps an index to catch duplicates, and tidies up the mails afterwards.

> **Status: early development.** The mail pipeline, portal downloads, link mails that start a download, schedules, the `docsweep` command line tool and a first version of the desktop app work. There are no signed builds so far.

## How it works

1. You describe each company in `docsweep.yaml`: which mails belong to it and how invoices are named.
2. `docsweep mail-scan --dry-run` shows what would happen, without changing anything.
3. `docsweep mail-scan` saves the invoices and cleans up the mails.

A mail is only moved or deleted after its invoice is stored and indexed. By default, "delete" means moving it to the trash. A mail that only links to a portal starts that company's download, and is cleaned up once a matching invoice is stored. Until then it stays in your inbox.

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

## Development

See [CLAUDE.md](CLAUDE.md) for the workspace layout, commands and rules, and [KONZEPT.md](KONZEPT.md) for the full concept (in German).

## Kurzfassung auf Deutsch

Docsweep sammelt Eingangsrechnungen automatisch ein: PDF-Anhänge aus Mails werden einheitlich benannt gespeichert und im Index erfasst, danach werden die Mails nach Regeln aufgeräumt. Aus Kundenportalen lädt Docsweep Rechnungen per Rezept, von Hand, ausgelöst durch Weblink-Mails oder nach Zeitplan. Automatische Läufe sind aus, bis man sie einschaltet. Das Projekt steht noch am Anfang. Unterstützt wird nur IMAP mit Passwort, kein OAuth2.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.
