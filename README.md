# Docsweep

Docsweep is a free desktop app for macOS, Windows and Linux that collects incoming invoices for you. It saves PDF attachments from your mailbox under a consistent file name, keeps an index to catch duplicates, and tidies up the mails afterwards.

> **Status: early development.** The mail pipeline and the `docsweep` command line tool are being built first. The desktop app is only a stub so far, and portal downloads are not implemented yet.

## How it works

1. You describe each company in `docsweep.yaml`: which mails belong to it and how invoices are named.
2. `docsweep mail-scan --dry-run` shows what would happen, without changing anything.
3. `docsweep mail-scan` saves the invoices and cleans up the mails.

A mail is only moved or deleted after its invoice is stored and indexed. By default, "delete" means moving it to the trash. Mails that only link to a portal stay in your inbox until portal downloads are available.

Only IMAP with a password or app password is supported. Mailboxes that require OAuth2, such as Microsoft 365, will not work.

## Getting started

Build from source with [rustup](https://rustup.rs). The pinned Rust version installs automatically.

```sh
cargo build --release -p docsweep-cli
```

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
      dir: ~/Invoices/Inbox
      filename: "{date:%Y-%m-%d} - Hetzner - {invoice_no}.pdf"
    disposal: { move_to: Invoices/Hetzner }
```

Then try it:

```sh
docsweep mail-scan --dry-run
```

## Development

See [CLAUDE.md](CLAUDE.md) for the workspace layout, commands and rules, and [KONZEPT.md](KONZEPT.md) for the full concept (in German).

## Kurzfassung auf Deutsch

Docsweep sammelt Eingangsrechnungen automatisch ein: PDF-Anhänge aus Mails werden einheitlich benannt gespeichert und im Index erfasst, danach werden die Mails nach Regeln aufgeräumt. Das Projekt steht noch am Anfang. Unterstützt wird nur IMAP mit Passwort, kein OAuth2.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.
