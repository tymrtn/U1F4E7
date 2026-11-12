"""Layout checks against the real built frontend.

The shell talks to Rust through window.__TAURI_INTERNALS__.invoke. Stubbing
that lets the actual dist bundle run in Chromium, so these exercise the real
render() rather than a hand-written replica of it.

Layout is asserted by measurement. A screenshot proves nothing on its own —
the bug this suite exists to catch was an iframe that collapsed to a few
hundred pixels while every element was still present in the DOM.
"""

from __future__ import annotations

import contextlib
import functools
import http.server
import threading

from harness import DIST, Suite, free_port

STUB_DASHBOARD = b"<!doctype html><title>stub</title><body style='margin:0;height:3000px'>dashboard</body>"

IPC_STUB = """
window.__TAURI_INTERNALS__ = {
  transformCallback: (cb) => { const id = Math.floor(Math.random() * 1e9); window[`_cb_${id}`] = cb; return id; },
  invoke: (cmd) => {
    if (cmd === 'desktop_status' || cmd === 'open_dashboard' || cmd === 'restart_server') {
      return Promise.resolve(window.__E2E_STATUS__);
    }
    if (cmd === 'copy_diagnostics') {
      return Promise.resolve({ status: window.__E2E_STATUS__, diagnostics: 'stub', copied: true, error: null });
    }
    return Promise.resolve(null);
  },
};
"""


def status(readiness="dashboard_ready", healthy=True, url=None, error=None, guidance=None, existing=None):
    return {
        "guidance": guidance,
        "existingServerUrl": existing,
        "readiness": readiness,
        "running": healthy,
        "healthy": healthy,
        "dashboardUrl": url,
        "binaryPath": "/Applications/Envelope.app/Contents/MacOS/envelope",
        "binarySource": "bundled sidecar",
        "binaryAttempts": [{"source": "bundled sidecar", "path": "/Applications/Envelope.app/Contents/MacOS/envelope"}],
        "port": 51234,
        "portSource": "free localhost port",
        "lastHealthCheckAt": "2026-08-19T22:00:00Z",
        "healthChecks": [
            {"path": "/", "url": "http://127.0.0.1:51234/", "ok": healthy, "statusCode": 200 if healthy else None, "error": None},
        ],
        "stdoutTail": "",
        "stderrTail": "" if healthy else "boom",
        "error": error,
        "recommendedCommand": "'/Applications/Envelope.app/Contents/MacOS/envelope' serve --no-background-sweeps --no-auth --port 51234",
    }


class Handler(http.server.SimpleHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith("/stub-dashboard"):
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(STUB_DASHBOARD)))
            self.end_headers()
            self.wfile.write(STUB_DASHBOARD)
            return
        super().do_GET()

    def log_message(self, *args):
        pass


@contextlib.contextmanager
def serve_dist():
    port = free_port()
    handler = functools.partial(Handler, directory=str(DIST))
    httpd = http.server.ThreadingHTTPServer(("127.0.0.1", port), handler)
    t = threading.Thread(target=httpd.serve_forever, daemon=True)
    t.start()
    try:
        yield port
    finally:
        httpd.shutdown()


MEASURE = """() => {
  const h = (s) => { const e = document.querySelector(s); return e ? Math.round(e.getBoundingClientRect().height) : null; };
  return {
    viewport: window.innerHeight,
    viewportWidth: window.innerWidth,
    scrollWidth: document.documentElement.scrollWidth,
    scrollHeight: document.documentElement.scrollHeight,
    iframe: h('#dashboardFrame'),
    statusbar: h('.statusbar'),
    topbar: h('.topbar'),
    actions: h('.actions'),
    diagnostics: h('.diagnostics'),
    failure: h('.failure-screen'),
    loadBanner: h('#dashboardLoadState'),
  };
}"""


def run(suite: Suite) -> None:
    group = "ui"
    print("\n== frontend layout ==", flush=True)

    if not (DIST / "index.html").is_file():
        suite.record("frontend bundle exists", group, False, f"missing {DIST}/index.html; run `npm run build`")
        return

    try:
        from playwright.sync_api import sync_playwright
    except ImportError:
        suite.record("playwright available", group, False, "pip3 install playwright && python3 -m playwright install chromium")
        return

    with serve_dist() as port, sync_playwright() as pw:
        browser = pw.chromium.launch()

        def open_shell(st, width=1280, height=860):
            page = browser.new_page(viewport={"width": width, "height": height})
            page.add_init_script(IPC_STUB)
            page.add_init_script(f"window.__E2E_STATUS__ = {__import__('json').dumps(st)};")
            page.goto(f"http://127.0.0.1:{port}/index.html")
            page.wait_for_timeout(900)
            return page

        healthy_url = f"http://127.0.0.1:{port}/stub-dashboard.html"

        # Healthy: the mail owns the window, and the two visible bands must sum
        # to it exactly. This is the assertion that fails when a percentage
        # height stops resolving.
        page = open_shell(status(url=healthy_url))
        m = page.evaluate(MEASURE)
        band_sum = (m["iframe"] or 0) + (m["statusbar"] or 0)
        suite.record(
            "healthy: dashboard plus status bar fill the window exactly",
            group,
            m["iframe"] is not None and abs(band_sum - m["viewport"]) <= 2,
            evidence=[f"iframe={m['iframe']} statusbar={m['statusbar']} sum={band_sum} viewport={m['viewport']}"],
        )
        suite.record(
            "healthy: dashboard is taller than half the window",
            group,
            (m["iframe"] or 0) > m["viewport"] * 0.5,
            detail="guards against a collapsed iframe with the DOM still intact",
            evidence=[f"iframe={m['iframe']} of {m['viewport']}"],
        )
        suite.record(
            "healthy: debug chrome is absent",
            group,
            m["topbar"] is None and m["actions"] is None and m["diagnostics"] is None,
            evidence=[f"topbar={m['topbar']} actions={m['actions']} diagnostics={m['diagnostics']}"],
        )
        suite.record(
            "healthy: the loading banner is gone once loaded",
            group,
            m["loadBanner"] is None,
            evidence=[f"loadBanner={m['loadBanner']}"],
        )
        suite.record(
            "healthy: the page does not scroll",
            group,
            m["scrollHeight"] <= m["viewport"] + 2 and m["scrollWidth"] <= m["viewportWidth"] + 2,
            evidence=[f"scroll={m['scrollWidth']}x{m['scrollHeight']} viewport={m['viewportWidth']}x{m['viewport']}"],
        )

        # Details brings the panel back without pushing the page out of shape.
        page.click("#toggleDiagnostics")
        page.wait_for_timeout(500)
        m2 = page.evaluate(MEASURE)
        suite.record(
            "Details reveals diagnostics",
            group,
            m2["diagnostics"] is not None and m2["iframe"] is not None,
            evidence=[f"diagnostics={m2['diagnostics']} iframe={m2['iframe']}"],
        )
        suite.record(
            "Details does not make the page scroll",
            group,
            m2["scrollWidth"] <= m2["viewportWidth"] + 2
            and m2["scrollHeight"] <= m2["viewport"] + 2,
            detail="the panel scrolls itself; the window stays put",
            evidence=[
                f"scroll={m2['scrollWidth']}x{m2['scrollHeight']} viewport={m2['viewportWidth']}x{m2['viewport']}"
            ],
        )
        suite.record(
            "Details keeps the dashboard the same height",
            group,
            m2["iframe"] is not None and abs(m2["iframe"] - (m["iframe"] or 0)) <= 60,
            detail="opening diagnostics must not resize the mail out from under the reader",
            evidence=[f"iframe {m['iframe']} -> {m2['iframe']}"],
        )
        # The iframe holds the user's session: scroll position, the open message,
        # a half-written reply. Recreating the element reloads the SPA and throws
        # all of it away, so toggling chrome must not touch the node.
        page2 = open_shell(status(url=healthy_url))
        page2.evaluate("document.querySelector('#dashboardFrame').dataset.e2eMark = 'original'")
        page2.click("#toggleDiagnostics")
        page2.wait_for_timeout(400)
        survived_open = page2.evaluate(
            "document.querySelector('#dashboardFrame')?.dataset.e2eMark || 'RECREATED'"
        )
        page2.click("#toggleDiagnostics")
        page2.wait_for_timeout(400)
        survived_close = page2.evaluate(
            "document.querySelector('#dashboardFrame')?.dataset.e2eMark || 'RECREATED'"
        )
        suite.record(
            "toggling Details keeps the same iframe, not a reload",
            group,
            survived_open == "original" and survived_close == "original",
            detail="recreating it discards scroll position and any half-written reply",
            evidence=[f"after open={survived_open} after close={survived_close}"],
        )

        # A transient health blip must not cost the user their draft either.
        page2.evaluate(
            "window.__E2E_STATUS__ = Object.assign({}, window.__E2E_STATUS__, {healthy: false, readiness: 'api_route_failure'})"
        )
        page2.wait_for_timeout(6500)
        page2.evaluate(
            "window.__E2E_STATUS__ = Object.assign({}, window.__E2E_STATUS__, {healthy: true, readiness: 'dashboard_ready'})"
        )
        page2.wait_for_timeout(6500)
        recovered = page2.evaluate(
            "document.querySelector('#dashboardFrame')?.dataset.e2eMark || 'RECREATED'"
        )
        # Surviving in a hidden host is not recovery: the mail has to come back.
        visible = page2.evaluate(
            "!document.querySelector('#dashboardHost')?.hidden"
        )
        suite.record(
            "a health blip and recovery keeps the same iframe, and re-shows it",
            group,
            recovered == "original" and bool(visible),
            detail="one slow probe should neither reload the mailbox nor strand the failure screen",
            evidence=[f"after blip+recovery mark={recovered} dashboardVisible={visible}"],
        )
        # A restart picks a fresh ephemeral port. The menu path never cleared
        # dashboardVisible, so the iframe kept pointing at the dead old port.
        page2.evaluate(
            "window.__E2E_STATUS__ = Object.assign({}, window.__E2E_STATUS__, {dashboardUrl: window.__E2E_STATUS__.dashboardUrl + '?restarted=1'})"
        )
        page2.wait_for_timeout(6500)
        moved = page2.evaluate(
            "document.querySelector('#dashboardFrame')?.getAttribute('src') || ''"
        )
        suite.record(
            "a restart onto a new port re-points the dashboard",
            group,
            "restarted=1" in moved,
            detail="otherwise the window shows a dead port until relaunch",
            evidence=[f"iframe src now {moved[-40:]!r}"],
        )
        page2.close()

        page.close()

        # The window can be dragged to its declared minimum; nothing may spill.
        page = open_shell(status(url=healthy_url), width=960, height=640)
        m3 = page.evaluate(MEASURE)
        suite.record(
            "at the app's minimum window size nothing overflows",
            group,
            m3["scrollWidth"] <= m3["viewportWidth"] + 2 and (m3["iframe"] or 0) > 0,
            evidence=[f"960x640 -> scrollWidth={m3['scrollWidth']} iframe={m3['iframe']}"],
        )
        page.close()

        # Failure is the case the diagnostics exist for; they must appear
        # unasked, and the mail must not pretend to be there.
        page = open_shell(status(readiness="server_start_failed", healthy=False, error="server exited immediately"))
        m4 = page.evaluate(MEASURE)
        suite.record(
            "failure: diagnostics appear without being asked for",
            group,
            m4["diagnostics"] is not None and m4["failure"] is not None,
            evidence=[f"diagnostics={m4['diagnostics']} failureScreen={m4['failure']}"],
        )
        body = page.inner_text("body")
        suite.record(
            "failure: the reason is on screen",
            group,
            "server exited immediately" in body,
            evidence=["error text present" if "server exited immediately" in body else "error text MISSING"],
        )
        suite.record(
            "failure: controls are available",
            group,
            page.locator("#restartServer").count() == 1 and page.locator("#copyDiagnostics").count() == 1,
            evidence=["restart + copy-diagnostics present"],
        )
        page.close()

        # A recognised failure must lead with what happened and what to do, and
        # must offer the working server when one is running. The raw message
        # stays reachable but is not the headline.
        page = open_shell(
            status(
                readiness="server_start_failed",
                healthy=False,
                error="Error: migration error: MigrationDefinition(DatabaseTooFarAhead)",
                guidance={
                    "kind": "schema_too_new",
                    "title": "Your mailbox is newer than this app",
                    "explanation": "The database was upgraded by a newer version of Envelope.",
                    "remedy": "Update Envelope Desktop.",
                },
                existing="http://127.0.0.1:3141/",
            )
        )
        body = page.inner_text("body")
        suite.record(
            "a recognised failure explains itself instead of dumping an exit status",
            group,
            "Your mailbox is newer than this app" in body
            and "Update Envelope Desktop." in body,
            evidence=["title and remedy shown"],
        )
        suite.record(
            "a recognised failure offers a way out, not a dead end",
            group,
            page.locator("#retryServer").count() == 1
            and page.locator("#openExisting").count() == 1,
            detail="start the server, or open the Envelope already running",
            evidence=[
                f"retry={page.locator('#retryServer').count()} openExisting={page.locator('#openExisting').count()}"
            ],
        )
        suite.record(
            "the raw server message is still reachable",
            group,
            page.locator(".raw-detail").count() == 1
            and "DatabaseTooFarAhead" in page.inner_html(".raw-detail"),
            evidence=["raw output kept under a disclosure"],
        )
        page.close()

        # With nothing else running there is no false promise of an escape hatch.
        page = open_shell(
            status(
                readiness="server_start_failed",
                healthy=False,
                error="boom",
                guidance={
                    "kind": "port_in_use",
                    "title": "Something already has that port",
                    "explanation": "Another process took it.",
                    "remedy": "Starting again picks a new port.",
                },
            )
        )
        suite.record(
            "no escape-hatch button when nothing else is serving",
            group,
            page.locator("#openExisting").count() == 0
            and page.locator("#retryServer").count() == 1,
            evidence=["retry offered, no false 'open existing'"],
        )
        page.close()

        browser.close()
