"""Shared plumbing for the desktop end-to-end suite.

Every case runs against a throwaway HOME. Nothing here may touch the real
mail database or credential store: these tests are meant to be runnable by
anyone, on a machine with real accounts configured, without consequence.
"""

from __future__ import annotations

import contextlib
import json
import os
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path

DESKTOP_DIR = Path(__file__).resolve().parents[2]
REPO_ROOT = DESKTOP_DIR.parents[1]
SIDECAR = DESKTOP_DIR / "src-tauri" / "binaries"
BUNDLE = DESKTOP_DIR / "src-tauri" / "target" / "release" / "bundle" / "macos" / "Envelope.app"
APP_BIN = BUNDLE / "Contents" / "MacOS" / "envelope-desktop"
BUNDLED_SERVER = BUNDLE / "Contents" / "MacOS" / "envelope"
DIST = DESKTOP_DIR / "dist"


def host_triple() -> str:
    out = subprocess.run(["rustc", "-vV"], capture_output=True, text=True, check=True).stdout
    for line in out.splitlines():
        if line.startswith("host: "):
            return line[len("host: "):].strip()
    raise RuntimeError("could not determine host triple from rustc -vV")


def staged_server() -> Path:
    """The newest available server binary.

    Preferring the bundled copy unconditionally silently tests stale code
    whenever the sidecar has been restaged but the app not rebuilt — which is
    exactly the mistake this suite exists to catch. Pick by mtime instead.
    """
    candidates = [p for p in (BUNDLED_SERVER, SIDECAR / f"envelope-{host_triple()}") if p.is_file()]

    if not candidates:
        raise RuntimeError(
            f"no server binary found. Run `npm run stage-sidecar` in {DESKTOP_DIR} first."
        )

    return max(candidates, key=lambda p: p.stat().st_mtime)


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def port_is_listening(port: int) -> bool:
    with socket.socket() as s:
        s.settimeout(0.4)
        return s.connect_ex(("127.0.0.1", port)) == 0


@dataclass
class Response:
    status: int
    content_type: str
    body: bytes
    allow_origin: str | None = None

    def json(self):
        return json.loads(self.body)


def http_get(port: int, path: str, timeout: float = 20.0, headers: dict | None = None) -> Response:
    url = f"http://127.0.0.1:{port}{path}"
    req = urllib.request.Request(url, headers={"Accept": "*/*", **(headers or {})})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return Response(
                r.status,
                r.headers.get("Content-Type", ""),
                r.read(),
                r.headers.get("Access-Control-Allow-Origin"),
            )
    except urllib.error.HTTPError as e:
        return Response(
            e.code,
            e.headers.get("Content-Type", ""),
            e.read(),
            e.headers.get("Access-Control-Allow-Origin"),
        )


@contextlib.contextmanager
def throwaway_home():
    """An isolated HOME so the server builds a fresh database and config."""
    d = tempfile.mkdtemp(prefix="envelope-e2e-")
    try:
        (Path(d) / "Library" / "Application Support").mkdir(parents=True, exist_ok=True)
        yield Path(d)
    finally:
        shutil.rmtree(d, ignore_errors=True)


@contextlib.contextmanager
def server(args: list[str], home: Path, env_extra: dict | None = None, wait: bool = True):
    """Run the sidecar server, always reaping it on the way out."""
    env = dict(os.environ)
    env["HOME"] = str(home)
    env.pop("ENVELOPE_MASTER_PASSPHRASE_FILE", None)
    env.pop("ENVELOPE_MASTER_KEY", None)
    env.pop("ENVELOPE_DASHBOARD_TOKEN", None)
    env.pop("ENVELOPE_DASHBOARD_TAILSCALE_ALLOW", None)
    if env_extra:
        env.update(env_extra)

    proc = subprocess.Popen(
        [str(staged_server()), *args],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    try:
        if wait:
            port = int(args[args.index("--port") + 1])
            for _ in range(200):
                if proc.poll() is not None:
                    break
                if port_is_listening(port):
                    break
                time.sleep(0.1)
        yield proc
    finally:
        if proc.poll() is None:
            proc.terminate()
            with contextlib.suppress(subprocess.TimeoutExpired):
                proc.wait(timeout=5)
        if proc.poll() is None:
            proc.kill()


@dataclass
class Result:
    name: str
    group: str
    passed: bool
    detail: str = ""
    evidence: list[str] = field(default_factory=list)


class Suite:
    def __init__(self) -> None:
        self.results: list[Result] = []

    def record(self, name, group, passed, detail="", evidence=None) -> Result:
        r = Result(name, group, passed, detail, evidence or [])
        self.results.append(r)
        mark = "PASS" if passed else "FAIL"
        print(f"  [{mark}] {name}" + (f" — {detail}" if detail else ""), flush=True)
        for line in r.evidence:
            print(f"         {line}", flush=True)
        return r

    @property
    def failed(self) -> list[Result]:
        return [r for r in self.results if not r.passed]
