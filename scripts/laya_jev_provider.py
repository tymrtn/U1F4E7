#!/usr/bin/env python3
# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)
"""Envelope's local Jev provider, backed by a pinned Laya-MLX checkpoint.

Envelope's `--jev-backend laya` posts the same typed `state + questions`
decision request it would post to OpenRouter, and this process answers it with
Laya's native typed `choice`/`noul` heads. Nothing here generates text or JSON
with a language model.

Security properties this process is responsible for:

  * It binds exactly IPv4 127.0.0.1 on one fixed port. There is no host option.
  * It accepts only `POST /decide` and `GET /health`. Everything else is
    refused before a body is read.
  * `/decide` requires a JSON content type and a bounded `Content-Length`.
    Request and response bytes are both capped.
  * It serves exactly one pinned checkpoint revision, declares that identity in
    every response, and refuses a request that names different weights.
  * `serve` runs with the Hugging Face hub forced offline, so a decision can
    never trigger a download. Pre-fetching is the explicit `setup` step.
  * The single-threaded server serializes inference, so concurrent Envelope
    passes queue instead of racing the model or creating waiting threads.
  * It holds no credential, returns no secret, and logs only method, normalized
    route, and status: never message, sender, history, or raw query content.

Usage:

    python3 scripts/laya_jev_provider.py setup [--dtype float16]
    python3 scripts/laya_jev_provider.py serve [--dtype float16]
    python3 scripts/laya_jev_provider.py health

Requires Apple Silicon, macOS 14+, Python 3.11+, and `pip install laya-mlx`.
"""

from __future__ import annotations

import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import socket
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

# Pinned identity. These three values are mirrored by
# crates/email/src/jev.rs (LAYA_MODEL_REPO, LAYA_MODEL_REVISION,
# LAYA_PROVIDER_PORT) and must change together.
MODEL_REPO = "aac6fef/laya-mlx"
MODEL_REVISION = "047678560251f28113ee8f5df4be82102c7bf336"
DEFAULT_PORT = 8791
RUNTIME_FILE_SHA256 = {
    "encoder/config.json": "bf3ab80598fdccf414855a2ce80f22859e4492d06ca8a62ddd1cfb63972f8979",
    "mlx_config.json": "c022368f128dc9b7f6478a9163c96e0406f1da6a7c6bae660ea8515b016ed7ef",
    "model.safetensors": "b9c07bf14be2fa5c78a9193a3e6d840ac80e89e62fc40f425834c3d8a6eaa3de",
    "rl_agent_config.json": "d96dc2cb39d6375e030ff48c9957088f3c52668f45c30c56504f4e801ed3ee62",
    "tokenizer/tokenizer.json": "6c8aaa9a542084f2457eab775d4eeb51f92a70c0fd9de28d5edb0ddec3c08d30",
    "tokenizer/tokenizer_config.json": "50044de60daaa73df97d262e15a40d4faf0160e7d742df64b377877a1320dd12",
}
RUNTIME_ALLOW_PATTERNS = list(RUNTIME_FILE_SHA256)

HOST = "127.0.0.1"
MAX_REQUEST_BYTES = 128 * 1024
MAX_RESPONSE_BYTES = 64 * 1024
MAX_QUESTIONS = 16
SOCKET_TIMEOUT_SECONDS = 30
ALLOWED_DTYPES = ("float16", "float32")
REQUEST_FIELDS = {"model", "revision", "state", "questions"}

_AGENT = None
_AGENT_DTYPE = "float16"


def _load_agent(dtype: str, allow_download: bool):
    """Import and load the pinned checkpoint.

    The hub is forced offline unless a download was explicitly requested, so an
    Envelope decision can never pull weights.
    """
    os.environ["HF_HUB_DISABLE_TELEMETRY"] = "1"
    os.environ["HF_HUB_OFFLINE"] = "0" if allow_download else "1"

    import laya_mlx as laya  # imported after the offline flag is set
    from huggingface_hub import snapshot_download

    snapshot = Path(
        snapshot_download(
            repo_id=MODEL_REPO,
            revision=MODEL_REVISION,
            allow_patterns=RUNTIME_ALLOW_PATTERNS,
            local_files_only=not allow_download,
        )
    )
    if snapshot.name != MODEL_REVISION:
        raise RuntimeError("resolved snapshot does not match the pinned revision")
    for relative, expected in RUNTIME_FILE_SHA256.items():
        path = snapshot / relative
        if not path.is_file():
            raise RuntimeError("pinned snapshot is incomplete")
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        if digest != expected:
            raise RuntimeError("pinned snapshot checksum mismatch")

    agent = laya.load(
        snapshot,
        dtype=dtype,
        batch_size=MAX_QUESTIONS,
    )
    return agent


class _Handler(BaseHTTPRequestHandler):
    # HTTP/1.1 with an explicit Content-Length on every response.
    protocol_version = "HTTP/1.1"
    server_version = "EnvelopeLayaProvider/1"
    sys_version = ""
    timeout = SOCKET_TIMEOUT_SECONDS

    def log_message(self, format, *args):  # noqa: A002 - stdlib signature
        """Suppress BaseHTTPRequestHandler's raw request-target logging."""

    def log_request(self, code="-", size="-"):
        """Log only a normalized route and status, never a raw query target."""
        route = self.path.split("?", 1)[0]
        if route not in ("/health", "/decide"):
            route = "/other"
        sys.stderr.write("laya-provider %s %s %s\n" % (self.command, route, code))

    def _respond(self, status: int, payload: dict) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        if len(body) > MAX_RESPONSE_BYTES:
            status, body = 500, b'{"error":"response_too_large"}'
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _refuse(self, status: int, code: str) -> None:
        """Refuse with a coarse code. Request content is never echoed back."""
        self._respond(status, {"error": code})

    def do_GET(self):  # noqa: N802 - stdlib signature
        if self.path != "/health":
            self._refuse(404, "not_found")
            return
        self._respond(
            200,
            {
                "status": "ok",
                "model": MODEL_REPO,
                "revision": MODEL_REVISION,
                "dtype": _AGENT_DTYPE,
                "ready": _AGENT is not None,
            },
        )

    def do_POST(self):  # noqa: N802 - stdlib signature
        if self.path != "/decide":
            self._refuse(404, "not_found")
            return
        content_type = (self.headers.get("Content-Type") or "").split(";")[0].strip()
        if content_type.lower() != "application/json":
            self._refuse(415, "unsupported_media_type")
            return
        raw_length = self.headers.get("Content-Length")
        if raw_length is None or not raw_length.strip().isdigit():
            self._refuse(411, "length_required")
            return
        length = int(raw_length)
        if length == 0:
            self._refuse(400, "empty_body")
            return
        if length > MAX_REQUEST_BYTES:
            self._refuse(413, "request_too_large")
            return

        body = self.rfile.read(length)
        if len(body) != length:
            self._refuse(400, "truncated_body")
            return
        try:
            request = json.loads(body.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
            self._refuse(400, "invalid_json")
            return

        if not isinstance(request, dict) or not set(request) <= REQUEST_FIELDS:
            self._refuse(400, "invalid_request")
            return
        if (
            request.get("model") != MODEL_REPO
            or request.get("revision") != MODEL_REVISION
        ):
            self._refuse(409, "model_mismatch")
            return
        state, questions = request.get("state"), request.get("questions")
        if not isinstance(state, dict) or not state:
            self._refuse(400, "invalid_state")
            return
        if (
            not isinstance(questions, dict)
            or not questions
            or len(questions) > MAX_QUESTIONS
            or not all(isinstance(key, str) for key in questions)
        ):
            self._refuse(400, "invalid_questions")
            return
        if _AGENT is None:
            self._refuse(503, "model_not_loaded")
            return

        try:
            # HTTPServer handles one request at a time, so the shared MLX agent
            # is never entered concurrently and waiting requests cannot create
            # an unbounded set of per-connection threads.
            result = _AGENT.predict(state, questions)
        except Exception:  # noqa: BLE001 - never leak state into an error
            self._refuse(500, "inference_failed")
            return

        self._respond(
            200,
            {
                "model": MODEL_REPO,
                "revision": MODEL_REVISION,
                "answers": result.get("answers", {}),
                "usage": result.get("usage", {}),
            },
        )


class _LoopbackServer(HTTPServer):
    # IPv4 only. A loopback-looking hostname can never resolve elsewhere here.
    address_family = socket.AF_INET
    # Permit immediate supervised restart after the prior socket closes. An
    # active listener still owns the fixed address and prevents a second server.
    allow_reuse_address = True


def _serve(dtype: str) -> int:
    global _AGENT, _AGENT_DTYPE

    _AGENT_DTYPE = dtype
    try:
        _AGENT = _load_agent(dtype, allow_download=False)
    except Exception as error:  # noqa: BLE001 - startup must fail loudly
        print(
            "Failed to load the pinned checkpoint %s@%s: %s\n"
            "Run `python3 %s setup` once to pre-fetch it explicitly."
            % (MODEL_REPO, MODEL_REVISION, error, sys.argv[0]),
            file=sys.stderr,
        )
        return 1

    server = _LoopbackServer((HOST, DEFAULT_PORT), _Handler)
    bound_host, bound_port = server.server_address[:2]
    # Belt and braces: refuse to serve if anything but exact IPv4 loopback bound.
    if bound_host != HOST:
        server.server_close()
        print("Refusing to serve on %s" % (bound_host,), file=sys.stderr)
        return 1
    print(
        "Envelope Laya provider serving %s@%s (%s) on http://%s:%d\n"
        "  POST /decide   GET /health   no credential, no content egress"
        % (MODEL_REPO, MODEL_REVISION, dtype, bound_host, bound_port),
        file=sys.stderr,
    )
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


def _setup(dtype: str) -> int:
    try:
        _load_agent(dtype, allow_download=True)
    except Exception as error:  # noqa: BLE001
        print("Setup failed: %s" % (error,), file=sys.stderr)
        return 1
    print(
        "Pinned checkpoint %s@%s is present and loads as %s."
        % (MODEL_REPO, MODEL_REVISION, dtype)
    )
    return 0


def _health() -> int:
    connection = http.client.HTTPConnection(HOST, DEFAULT_PORT, timeout=5)
    try:
        connection.request("GET", "/health")
        response = connection.getresponse()
        body = response.read(MAX_RESPONSE_BYTES + 1)
        if response.status != 200 or len(body) > MAX_RESPONSE_BYTES:
            raise RuntimeError("unexpected health response")
        payload = json.loads(body.decode("utf-8"))
    except (OSError, http.client.HTTPException, json.JSONDecodeError, RuntimeError) as error:
        print(
            "Provider unreachable at http://%s:%d/health: %s"
            % (HOST, DEFAULT_PORT, error),
            file=sys.stderr,
        )
        return 1
    finally:
        connection.close()
    print(json.dumps(payload, indent=2))
    ok = (
        payload.get("status") == "ok"
        and payload.get("model") == MODEL_REPO
        and payload.get("revision") == MODEL_REVISION
        and payload.get("ready") is True
    )
    return 0 if ok else 1


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    subcommands = parser.add_subparsers(dest="command", required=True)

    setup = subcommands.add_parser(
        "setup", help="explicitly pre-fetch and verify the pinned checkpoint"
    )
    setup.add_argument("--dtype", choices=ALLOWED_DTYPES, default="float16")

    serve = subcommands.add_parser(
        "serve", help="serve the loopback-only decision endpoint"
    )
    serve.add_argument("--dtype", choices=ALLOWED_DTYPES, default="float16")

    subcommands.add_parser("health", help="probe the fixed local provider")

    args = parser.parse_args(argv)
    if args.command == "setup":
        return _setup(args.dtype)
    if args.command == "serve":
        return _serve(args.dtype)
    return _health()


if __name__ == "__main__":
    sys.exit(main())
