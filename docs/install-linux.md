# Installing Envelope on Linux / VPS

## Option A — prebuilt tarball (fastest)

Pre-built tarballs land with each release. On a fresh VPS:

```bash
# Download the latest release tarball (replace VERSION and ARCH as needed)
VERSION=0.13.0
ARCH=x86_64-unknown-linux-gnu   # or aarch64-unknown-linux-gnu on ARM
curl -LO "https://github.com/tymrtn/U1F4E7/releases/download/v${VERSION}/envelope-${VERSION}-${ARCH}.tar.gz"
tar -xzf "envelope-${VERSION}-${ARCH}.tar.gz"
install -m 755 envelope ~/.local/bin/envelope
```

> Note: an `install.sh` script that automates tarball download and PATH setup ships
> with releases. Check the release notes for `dist/install.sh` once available.

## Option B — `cargo install` from source (requires Rust)

```bash
# 1. Install Rust (skip if already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# 2. Install directly from the repository
cargo install --git https://github.com/tymrtn/U1F4E7 --bin envelope
# Binary lands in ~/.cargo/bin/envelope — already on PATH after rustup setup
```

## Option C — full source build

```bash
# 1. Install Rust (skip if already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# 2. Clone and build
git clone https://github.com/tymrtn/U1F4E7
cd U1F4E7
cargo build --release
# binary: target/release/envelope

# 3. Install on PATH
install -m 755 target/release/envelope ~/.local/bin/envelope
```

Add `~/.local/bin` to PATH if not present (`echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc`).

---

## Passphrase setup (headless / non-interactive)

On a VPS with no interactive terminal, the credential store needs a passphrase file
rather than an interactive prompt.

```bash
# 1. Create the passphrase file with strict permissions
install -m 600 /dev/null ~/.config/envelope-email/passphrase
printf '%s' 'your-long-random-passphrase' > ~/.config/envelope-email/passphrase

# 2. Point Envelope at it
export ENVELOPE_MASTER_PASSPHRASE_FILE="$HOME/.config/envelope-email/passphrase"

# 3. Add an account (non-interactive — opt into stdin explicitly)
printf '%s\n' "$APP_PASSWORD" | envelope accounts add --email you@example.com --password-stdin

# 4. Verify
envelope quickstart
```

To re-encrypt under a new passphrase:

```bash
envelope accounts rekey
```

Add `ENVELOPE_MASTER_PASSPHRASE_FILE` to your shell profile or systemd unit environment
so it is set for all Envelope invocations.

---

## systemd --user units

Envelope ships systemd user units for the watcher and dashboard. See
[dist/systemd/README.md](../dist/systemd/README.md) for full setup instructions.

Quick path:

```bash
# Enable linger so user services start at boot without a login session
loginctl enable-linger $USER

# Enable the IMAP watcher (one instance per account)
systemctl --user enable --now envelope-watch@you@example.com.service

# Enable the dashboard
systemctl --user enable --now envelope-dashboard.service

# Verify
systemctl --user status envelope-watch@you@example.com.service
curl -s http://localhost:3141/health
```

Logs:

```bash
journalctl --user -u envelope-watch@you@example.com.service -f
```
