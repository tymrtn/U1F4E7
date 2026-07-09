# Envelope systemd user units

Two units ship in this directory:

| Unit | Purpose |
|------|---------|
| `envelope-watch@.service` | IMAP IDLE watcher (one instance per account) |
| `envelope-dashboard.service` | Local dashboard on `127.0.0.1:3141` |

Both use **systemd credentials** for the master passphrase — the kernel enforces 0600 on the credential file and strips the trailing newline before presenting it to the service.

## 1. Passphrase file

Create the passphrase file with strict permissions before enabling any unit:

```bash
install -m 600 /dev/null ~/.config/envelope-email/passphrase
printf '%s' 'your-passphrase' > ~/.config/envelope-email/passphrase
```

Never use `>` with a trailing newline from `echo`; `printf '%s'` omits it. systemd strips one trailing newline from credential files, so a bare newline added by echo would vanish, but the convention here keeps the file clean.

## 2. Enable the watcher

The watcher unit is a template (`@.service`). The instance name is the account email. Because email addresses contain `@`, systemd requires it to be escaped with `systemd-escape`:

```bash
# Find the escaped form of your address
systemd-escape --template='envelope-watch@.service' you@example.com
# Output: envelope-watch@you@example.com.service
#         (systemd treats the first @ as the template delimiter;
#          subsequent @ in the instance name are passed through unescaped
#          because @ is not special in instance specifiers — so for most
#          single-@ addresses the plain form works fine)

systemctl --user enable --now envelope-watch@you@example.com.service
```

Note on escaping: systemd unit names use `@` to separate the template name from the instance specifier. In practice `you@example.com` as an instance works because systemd only treats the *first* `@` as a delimiter. If your address contains characters that need escaping (spaces, slashes), use `systemd-escape` to produce the correct instance name:

```bash
INSTANCE=$(systemd-escape 'you+tag@example.com')
systemctl --user enable --now "envelope-watch@${INSTANCE}.service"
```

## 3. Enable the dashboard

```bash
systemctl --user enable --now envelope-dashboard.service
```

The dashboard binds to `127.0.0.1:3141` by default. To verify it is running:

```bash
systemctl --user status envelope-dashboard.service
curl -s http://localhost:3141/health
```

## 4. Linger (boot-start on servers without an active login session)

By default, user services start only after a user logs in and stop when the last session ends. On servers or headless machines you want the services to start at boot and persist without a logged-in session:

```bash
loginctl enable-linger $USER
```

Verify linger is set:

```bash
loginctl show-user $USER | grep Linger
# Linger=yes
```

## 5. Logs

```bash
journalctl --user -u envelope-watch@you@example.com.service -f
journalctl --user -u envelope-dashboard.service -f
```

## 6. Multiple accounts

Enable one watcher instance per configured account:

```bash
for ACCOUNT in you@gmail.com work@company.com; do
  systemctl --user enable --now "envelope-watch@${ACCOUNT}.service"
done
```
