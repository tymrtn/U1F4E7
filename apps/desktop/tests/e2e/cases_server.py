"""Server permutations: the bind/auth matrix the desktop shell depends on."""

from __future__ import annotations

from harness import (
    Suite,
    free_port,
    http_get,
    port_is_listening,
    server,
    staged_server,
    throwaway_home,
)

PROBE_PATHS = [
    ("/", "text/html"),
    ("/_app/version.json", "json"),
    ("/api/messages/unified?limit=1", "json"),
]


def run(suite: Suite) -> None:
    group = "server"
    print("\n== server permutations ==", flush=True)

    # The shell's own invocation. Every health path must answer with the media
    # type the probe demands, or the app reports a false failure.
    with throwaway_home() as home:
        port = free_port()
        with server(["serve", "--no-background-sweeps", "--no-auth", "--port", str(port)], home) as p:
            if p.poll() is not None:
                suite.record("shell invocation starts", group, False, "server exited during startup")
                return
            ev, ok = [], True
            for path, want in PROBE_PATHS:
                r = http_get(port, path)
                ev.append(f"{path} -> {r.status} {r.content_type}")
                if r.status != 200 or want not in r.content_type:
                    ok = False
            suite.record("shell invocation: all probe paths correct", group, ok, evidence=ev)

            # The SPA answers unknown paths with the shell, so a status-only
            # health check passes on a deleted asset. This is why the probe
            # asserts a media type; if this case ever stops returning HTML the
            # content-type assertion can be revisited.
            r = http_get(port, "/_app/definitely-not-here.json")
            suite.record(
                "SPA fallback returns HTML for a missing asset",
                group,
                r.status == 200 and "text/html" in r.content_type,
                evidence=[f"/_app/definitely-not-here.json -> {r.status} {r.content_type}"],
            )

    # --no-auth must never be a way to publish an open dashboard.
    for bind, label in [("0.0.0.0", "0.0.0.0"), ("::", "::")]:
        with throwaway_home() as home:
            port = free_port()
            with server(
                ["serve", "--no-background-sweeps", "--no-auth", "--bind", bind, "--port", str(port)],
                home,
                wait=False,
            ) as p:
                try:
                    p.wait(timeout=20)
                except Exception:
                    pass
                out = (p.stdout.read() if p.stdout else "") or ""
                listening = port_is_listening(port)
                suite.record(
                    f"--no-auth --bind {label} is refused",
                    group,
                    p.poll() is not None and not listening and "refusing to bind" in out,
                    evidence=[f"exited={p.poll() is not None} listening={listening}", out.strip().splitlines()[0] if out.strip() else "(no output)"],
                )

    # An unauthenticated broad bind was already refused before --no-auth existed.
    with throwaway_home() as home:
        port = free_port()
        with server(["serve", "--no-background-sweeps", "--bind", "0.0.0.0", "--port", str(port)], home, wait=False) as p:
            try:
                p.wait(timeout=20)
            except Exception:
                pass
            out = (p.stdout.read() if p.stdout else "") or ""
            suite.record(
                "plain --bind 0.0.0.0 with no auth is refused",
                group,
                p.poll() is not None and not port_is_listening(port),
                evidence=[out.strip().splitlines()[0] if out.strip() else "(no output)"],
            )

    # A configured token must still gate the API when --no-auth is absent, and
    # must be ignored when it is present. That pairing is the whole reason the
    # flag exists.
    for flag, expect, label in [([], 401, "enforced without --no-auth"), (["--no-auth"], 200, "bypassed with --no-auth")]:
        with throwaway_home() as home:
            port = free_port()
            with server(
                ["serve", "--no-background-sweeps", *flag, "--port", str(port)],
                home,
                env_extra={"ENVELOPE_DASHBOARD_TOKEN": "e2e-token"},
            ) as p:
                if p.poll() is not None:
                    suite.record(f"configured token {label}", group, False, "server exited")
                    continue
                r = http_get(port, "/api/messages/unified?limit=1")
                suite.record(
                    f"configured token {label}",
                    group,
                    r.status == expect,
                    evidence=[f"/api/messages/unified -> {r.status} (want {expect})"],
                )

    # Loopback in both families is local, so both are allowed unauthenticated.
    for bind, label in [("127.0.0.1", "IPv4"), ("::1", "IPv6")]:
        with throwaway_home() as home:
            port = free_port()
            with server(
                ["serve", "--no-background-sweeps", "--no-auth", "--bind", bind, "--port", str(port)],
                home,
                wait=False,
            ) as p:
                import time

                for _ in range(100):
                    if p.poll() is not None:
                        break
                    time.sleep(0.1)
                    if port_is_listening(port) or bind == "::1":
                        break
                time.sleep(1.0)
                suite.record(
                    f"{label} loopback bind is allowed with --no-auth",
                    group,
                    p.poll() is None,
                    evidence=[f"still running={p.poll() is None}"],
                )

    # The documented remote setup is `tailscale serve` in front of loopback 3141.
    # Dropping auth there would publish every mailbox to the tailnet, so the
    # fronted default port must refuse the flag outright.
    with throwaway_home() as home:
        with server(
            ["serve", "--no-background-sweeps", "--no-auth", "--port", "3141"],
            home,
            env_extra={"ENVELOPE_DASHBOARD_TOKEN": "e2e-token"},
            wait=False,
        ) as p:
            try:
                p.wait(timeout=20)
            except Exception:
                pass
            out = (p.stdout.read() if p.stdout else "") or ""
            suite.record(
                "--no-auth is refused on the tailscale-fronted default port",
                group,
                p.poll() is not None and "refusing --no-auth" in out,
                evidence=[out.strip().splitlines()[0][:140] if out.strip() else "(no output)"],
            )

    # An explicit private port is the desktop shell's case and must still work.
    with throwaway_home() as home:
        port = free_port()
        with server(
            ["serve", "--no-background-sweeps", "--no-auth", "--port", str(port)],
            home,
            env_extra={"ENVELOPE_DASHBOARD_TOKEN": "e2e-token"},
        ) as p:
            r = http_get(port, "/api/messages/unified?limit=1") if p.poll() is None else None
            suite.record(
                "--no-auth is allowed on an explicit non-default port",
                group,
                r is not None and r.status == 200,
                evidence=[f"port {port} -> {r.status if r else 'server exited'}"],
            )

    # Reflecting every localhost origin is only safe while a credential is
    # required. Unauthenticated, it would let any local page read the mailbox.
    with throwaway_home() as home:
        port = free_port()
        with server(["serve", "--no-background-sweeps", "--no-auth", "--port", str(port)], home) as p:
            r = http_get(port, "/api/messages/unified?limit=1", headers={"Origin": "http://localhost:3000"})
            suite.record(
                "unauthenticated: no cross-origin read is granted",
                group,
                r.allow_origin is None,
                detail="without this any local page could fetch the mailbox",
                evidence=[f"Access-Control-Allow-Origin: {r.allow_origin!r}"],
            )

    with throwaway_home() as home:
        port = free_port()
        with server(
            ["serve", "--no-background-sweeps", "--port", str(port)],
            home,
            env_extra={"ENVELOPE_DASHBOARD_TOKEN": "e2e-token"},
        ) as p:
            r = http_get(port, "/api/messages/unified?limit=1", headers={"Origin": "http://localhost:3000"})
            ok = http_get(
                port,
                "/api/messages/unified?limit=1",
                headers={"Origin": "http://localhost:3000", "Authorization": "Bearer e2e-token"},
            )
            suite.record(
                "authenticated: localhost origins still reflected, credential still required",
                group,
                r.allow_origin == "http://localhost:3000" and r.status == 401 and ok.status == 200,
                evidence=[f"no token -> {r.status} ACAO={r.allow_origin!r}; with token -> {ok.status}"],
            )

    suite.record(
        "server binary is the staged sidecar",
        group,
        staged_server().is_file(),
        evidence=[str(staged_server())],
    )
