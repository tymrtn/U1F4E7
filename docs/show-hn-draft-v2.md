# Show HN Draft — Envelope Email (v2)

## Title (80 chars max)

```
Show HN: Envelope – A Rust CLI email client with auto-discovery and JSON output
```

(77 chars)

## Post Body (URL field)

```
https://github.com/tymrtn/envelope-email-rs
```

## Comment (post this as the first comment)

I built Envelope because I wanted an email client that treated the terminal as a first-class interface — not a TUI you navigate with arrow keys, but a CLI you pipe through `jq`.

**What it does:** Envelope turns any IMAP/SMTP mailbox into a programmable interface. Add an account with `envelope-email accounts add --email you@gmail.com` and it auto-discovers your IMAP/SMTP servers via DNS (SRV records → MX lookup → common patterns). No config file. No TOML. Just email and password.

Every command supports `--json`:

```
envelope-email inbox --json | jq '.[0].subject'
envelope-email search "FROM boss@co.com SINCE 01-Mar-2026" --json
```

**Why not mutt/himalaya/aerc?**

- **mutt/neomutt** — Battle-tested, but it's a TUI from the 90s. Configuration is a black art. Getting structured output for scripts means screen-scraping or writing macros. It was built for humans at keyboards, not programs reading JSON.

- **himalaya** — Closest in spirit. Great project. But Himalaya requires a TOML config file with explicit server settings, compiles features via cargo flags, and supports multiple backends (Maildir, Notmuch, SMTP, Sendmail). Envelope is deliberately simpler: IMAP/SMTP only, auto-discovery handles server config, and it works out of the box with `brew install`.

- **aerc** — Beautiful TUI, but it's interactive-first. Same fundamental mismatch: designed for a human in the loop, not a script or agent calling it programmatically.

**Technical details:**

- Rust, ~4 crates (cli, email, store, dashboard)
- Auto-discovery: SRV → MX → common pattern probing with 3s TCP timeout
- Credentials stored in OS keychain (macOS Keychain via `keyring` crate), not plaintext
- State in SQLite at `~/.config/envelope-email/`
- Draft workflow: create → review → send/discard (designed for AI agents that compose on behalf of humans)
- Localhost dashboard via Axum (`envelope-email serve`)
- Install: `brew install tymrtn/tap/envelope-email` or `cargo install envelope-email`

**What it's not:** Envelope is a clean email client. It doesn't do email scoring, content analysis, or governance — that's a separate layer. Envelope reads, writes, and organizes mail. That's it.

License is FSL-1.1-ALv2 (functional source, converts to ALv2 after 2 years).

I'd love feedback on the auto-discovery approach and whether the "CLI-first, not TUI-first" positioning resonates. Happy to answer questions.
