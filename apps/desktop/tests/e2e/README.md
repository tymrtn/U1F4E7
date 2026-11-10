# Desktop end-to-end suite

Runs real permutations against a real built app. Nothing is mocked except
Tauri's IPC, which cannot exist outside the app.

```bash
npm run stage-sidecar        # from apps/desktop
npm run tauri build
python3 tests/e2e/run.py     # or --only server|lifecycle|ui
```

Exits non-zero on any failure. Every case uses a throwaway `HOME`, so it is
safe to run on a machine with real accounts configured.

## Groups

**server** — the bind/auth matrix. Both loopback families, both broad-bind
families, a configured token with and without `--no-auth`, and the SPA
fallback that makes a status-only health check unreliable.

**lifecycle** — what the app spawns and what it leaves behind: the bundled
sidecar rather than something off `PATH`, the read-only flags, survival when
the binary cannot be resolved, and three launch/quit cycles with no orphaned
server. Launches real windows, so it is local-only and not run in CI.

**ui** — layout, measured in Chromium against the real `dist` bundle with
`window.__TAURI_INTERNALS__` stubbed.

## Why layout is measured rather than screenshotted

The bug this suite was written after left every element present in the DOM
and correctly styled — the dashboard iframe simply collapsed to a few hundred
pixels because a percentage height had no definite parent to resolve against.
A screenshot review would have caught it only if someone looked carefully at
the right screenshot. Asserting that the visible bands sum to the viewport
catches it every time.

The same applies to `Details`: opening it used to restore the whole header and
control row, shoving the mail down the window. That is invisible to a
smoke test and obvious to an assertion about the iframe's height.
