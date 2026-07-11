# Envelope credential backends

Envelope encrypts IMAP/SMTP passwords at rest. Three backends are available; the
file backend is the default and works everywhere.

---

## File backend (default)

Credentials are stored in an AES-256-GCM encrypted file:

```
~/.config/envelope-email/credentials.json   (mode 0600)
```

### Master key precedence

1. `ENVELOPE_MASTER_KEY` environment variable (raw hex key — set by quickstart in
   automation contexts that manage their own key material)
2. `ENVELOPE_MASTER_PASSPHRASE_FILE` — path to a file containing the passphrase
   (recommended for headless/VPS; file must be mode 0600)
3. Interactive passphrase prompt on stdin (default for interactive terminals)

### Passphrase file setup (VPS / non-interactive)

```bash
install -m 600 /dev/null ~/.config/envelope-email/passphrase
printf '%s' 'your-passphrase' > ~/.config/envelope-email/passphrase
export ENVELOPE_MASTER_PASSPHRASE_FILE="$HOME/.config/envelope-email/passphrase"
```

### Rekey (change passphrase)

```bash
envelope accounts rekey
```

Prompts for the current passphrase, then a new one, and re-encrypts in place.

### Insecure machine key (legacy / automation fallback)

```bash
envelope accounts add --email you@example.com \
  --password <app-password> \
  --insecure-machine-key
```

Derives the master key from `hostname + username` (SHA-256). Portable only as long
as both stay the same. Prefer a passphrase; use `--insecure-machine-key` only when
no passphrase can be supplied and the machine identity is stable.

---

## macOS Keychain backend

On macOS, pass `--credential-store keychain` to store and retrieve the master
passphrase through the system Keychain instead of the passphrase file.

```bash
envelope --credential-store keychain accounts add --email you@example.com --password <app-password>
envelope --credential-store keychain quickstart
```

The Keychain entry is created under the service name `envelope-email`. Touch ID /
password unlock applies. Not available on Linux.

---

## Secret Service backend (Linux desktop)

On Linux desktop systems with GNOME Keyring or KWallet running, pass
`--credential-store keychain`. The `keychain` cargo feature must be enabled at build
time (it is in release builds).

```bash
envelope --credential-store keychain accounts add --email you@example.com --password <app-password>
```

Secret Service access requires a running D-Bus session bus. On a headless VPS with no
session bus, this will fail. Use the file backend instead.

---

## What quickstart does (and does not do)

`envelope quickstart` reads an existing credential store passphrase but never
creates one. It uses read-only credential access so it does not mutate your
credential file during the health check. If no passphrase exists yet, quickstart
will prompt for it at the IMAP auth phase; for non-interactive contexts, set
`ENVELOPE_MASTER_PASSPHRASE_FILE` before running quickstart.
