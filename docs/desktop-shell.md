# Desktop shell

A Tauri v2 wrapper around the local dashboard. It supervises an `envelope`
server rather than reimplementing any mail UI: it picks a free loopback port,
spawns the bundled server, health-checks it, and renders the dashboard in a
webview. Diagnostics, not decoration — when the server fails, the window shows
why instead of a blank page.

Source: `apps/desktop`. It declares its own `[workspace]`, so root
`cargo test --all` does not compile it; CI runs it as a separate job.

## Build

```bash
cd apps/desktop
npm ci
npm run stage-sidecar   # builds the workspace binary into src-tauri/binaries/
npm run tauri build
```

`stage-sidecar` is not optional. Tauri resolves `externalBin` by appending the
host target triple, so the file must be `binaries/envelope-<triple>`; at bundle
time it lands at `Contents/MacOS/envelope`. A missing sidecar fails the build
rather than producing a bundle with no server.

For day-to-day work, `npm run tauri:dev` uses the repo's `target/debug/envelope`
without staging anything.

## How the server is located

`resolve_binary` tries, in order:

1. `ENVELOPE_DESKTOP_BIN` — explicit override, for testing a specific build.
2. **Sidecar** — next to the running executable. The only source that works in a
   shipped bundle.
3. `target/debug/envelope` under the repo — development convenience, derived
   from `CARGO_MANIFEST_DIR`.
4. `PATH`.

Steps 3 and 4 cannot carry a release. `CARGO_MANIFEST_DIR` is a compile-time
constant that bakes in the build machine's directory, and an app launched from
Finder or the Dock gets `PATH=/usr/bin:/bin:/usr/sbin:/sbin` — which excludes
`~/.local/bin`, `~/.cargo/bin`, and Homebrew. That is why the sidecar exists.

## Credentials

A GUI launch inherits no shell environment, so the spawned server has no key
source and no terminal to prompt on — without help every account fails with
"no passphrase available and no interactive terminal".

The shell therefore points the server at a passphrase file unless the
environment already names a key source (`ENVELOPE_MASTER_PASSPHRASE_FILE` or a
raw `ENVELOPE_MASTER_KEY` both win). Two filenames are probed, because two exist
in the wild: `envelope-master-passphrase`, which installed launcher scripts
export, and `passphrase`, which [credential-backends.md](credential-backends.md)
prescribes. If neither is present nothing is set and the server reports its own
credential error rather than being pointed at a path that does not exist.

## Why the shell passes `--no-auth`

The shell runs a *private* server on an ephemeral loopback port. A configured
`dashboard.auth_token` exists for the `tailscale serve` case and does not apply
to that instance — and the dashboard UI has no way to present a bearer token, so
an inherited token would return 401 for every API call while the window still
rendered.

`--no-auth` is deliberately narrow, because "loopback" is not the same as
"private": the documented remote setup puts `tailscale serve` in front of
loopback 3141, which is why `auth.rs` enforces on *configuration* rather than
bind address. So `serve` refuses `--no-auth` on the default port whenever auth is
configured, and warns on any other port. The shell is unaffected — it picks an
ephemeral port per launch.

While auth is not enforced the server also refuses cross-origin requests
outright. Reflecting every `localhost:*` origin is only safe when a credential is
required; without one it would let any local page read the mailbox.

## Running beside an installed Envelope

The app's server and an installed dashboard share
the same mail home. Only one of them runs the background jobs: mailbox sync,
add-ons, shelf life, digests, scheduled sends and the startup recovery of
interrupted sends. `serve` takes an OS lock on `background.lock` in the home
before starting them. A server that finds it held serves its window only and
logs "Another Envelope server on this mail home runs the background jobs", then
checks every 5 seconds and takes the jobs over when the other server stops.
Mail sent from the app is queued in the shared database and leaves through
whichever server holds the lock. A server waiting for the lock still sends
`new_mail` on its own event stream: every 5 seconds it compares each account's
indexed INBOX with what it saw last and announces what the other server's sync
wrote.

## Releasing

Push a `desktop-v*` tag. `.github/workflows/release-desktop.yml` stages the
sidecar, bundles, signs, notarizes, and then verifies the result — it fails the
run if the artifact comes out adhoc-signed or unstapled, rather than publishing
something Gatekeeper will reject.

These repository secrets must exist first (Settings → Secrets and variables →
Actions). The workflow fails with a named error if any is missing:

| Secret | What it is |
| --- | --- |
| `APPLE_CERTIFICATE` | Developer ID Application cert, `.p12`, base64-encoded |
| `APPLE_CERTIFICATE_PASSWORD` | Password for that `.p12` |
| `APPLE_SIGNING_IDENTITY` | e.g. `Developer ID Application: Name (TEAMID)` |
| `APPLE_ID` | Apple ID used for notarization |
| `APPLE_PASSWORD` | App-specific password, not the account password |
| `APPLE_TEAM_ID` | 10-character team identifier |
