# Local Jev decisions with the Laya provider

Envelope's mail engine asks Jev one typed decision question set per new message.
`--jev-backend openrouter` is the default and sends that request to TypeSafe Jev
over the OpenRouter Decisions API. `--jev-backend laya` is an optional local
provider that answers the identical request on-device, with no API key and no
content egress.

Both backends produce exactly the same validated decision fields and feed the
same policy. Neither one owns mailbox side effects.

## Prerequisites

| Requirement | Value |
| --- | --- |
| Hardware | Apple Silicon (MLX) |
| OS | macOS 14 or newer |
| Python | 3.11 or newer |
| Package | `laya-mlx` ([mizorewww/laya-mlx](https://github.com/mizorewww/laya-mlx)) |
| Model | `aac6fef/laya-mlx` |
| Revision | `047678560251f28113ee8f5df4be82102c7bf336` |
| Endpoint | `http://127.0.0.1:8791/decide` |
| Health | `http://127.0.0.1:8791/health` |

The model and revision are pinned in `crates/email/src/jev.rs` and in
`scripts/laya_jev_provider.py`. The provider resolves that immutable Hub
snapshot to an absolute cache path and verifies SHA-256 for every runtime model,
configuration, and tokenizer file before loading. A same-named relative
directory or modified cache entry is rejected rather than being reported as the
pinned revision. Envelope also refuses a response from a provider serving a
different model or revision.

## Setup

Install the runtime into whichever interpreter will run the provider:

```bash
python3 -m pip install laya-mlx
```

Pre-fetch and verify the pinned checkpoint. This is the only step that is
allowed to download weights:

```bash
python3 scripts/laya_jev_provider.py setup
```

## Start the provider

```bash
python3 scripts/laya_jev_provider.py serve
```

The process loads the checkpoint once and keeps it resident. It binds exactly
IPv4 `127.0.0.1` on port `8791`; there is no host or port option and no way to
bind an external interface. `serve` forces the Hugging Face hub offline, so a
decision can never trigger a download. Its single-threaded request loop also
keeps inference serialized without creating an unbounded set of waiting threads.

Leave it running in its own terminal, or under whatever process supervisor you
already use. Envelope never starts, stops, or spawns it.

## Check health

```bash
envelope engine laya-health --json
```

```json
{
  "backend": "laya",
  "endpoint": "http://127.0.0.1:8791/health",
  "expected_model": "aac6fef/laya-mlx",
  "expected_revision": "047678560251f28113ee8f5df4be82102c7bf336",
  "durable_model_identity": "aac6fef/laya-mlx:047678560251f28113ee8f5df4be82102c7bf336",
  "content_egress": false,
  "fallback_to_openrouter": false,
  "ready": true,
  "status": "ok",
  "dtype": "float16",
  "error_code": null
}
```

The probe carries no message, sender, or history content. It exits non-zero when
the provider is unreachable (`laya_provider_unavailable`) or is serving weights
other than the pinned revision (`laya_model_mismatch`).

The provider has its own equivalent probe, useful when Envelope is not on the
same machine's PATH:

```bash
python3 scripts/laya_jev_provider.py health
```

## Run the engine locally

```bash
envelope engine once --account you@example.com --jev-backend laya --json
envelope engine run  --account you@example.com --jev-backend laya --interval-seconds 300 --json
```

Durable decisions record `backend: "laya"` and the model identity
`aac6fef/laya-mlx:047678560251f28113ee8f5df4be82102c7bf336`. That identity is
truthful: a local answer is never labeled `typesafe/jev-1.13`.

## What stays local

Envelope sends the same typed `state` and `questions` payload it would send to
OpenRouter — normalized sender address and domain, subject, at most 8 KiB of
derived plain text, received timestamp, read/unread/junk flags, attachment
presence, and bounded indexed sender/interaction/reply statistics — to the
loopback endpoint only.

* No API key, bearer token, or other credential is sent.
* Redirects are disabled, and environment and system proxies are bypassed, so
  `HTTP_PROXY`, `ALL_PROXY`, and macOS proxy configuration cannot intercept the
  loopback request. The bundled Python health command uses a direct
  `HTTPConnection` to the numeric loopback address and likewise cannot follow a
  redirect or use a proxy.
* The endpoint is compiled in. There is no environment override, no discovery,
  and no caller-supplied endpoint.
* Envelope never invokes Python through a shell and never passes message content
  on a command line.

## Provider hardening

The bundled provider accepts only `POST /decide` and `GET /health`. A `/decide`
request must carry an `application/json` content type and a `Content-Length` of
at most 128 KiB, must name the pinned repository and revision, and may contain
only the `model`, `revision`, `state`, and `questions` fields. Responses are
capped at 64 KiB. The single-threaded server serializes inference, so concurrent
engine passes queue instead of racing the model or creating unbounded waiting
threads. Errors return a coarse code and never echo request content. Logs record
only the method, normalized known route, and status; raw request targets and
queries are never logged.

Envelope's client times out a decision after 30 seconds. If native MLX inference
itself hangs, the provider process may remain occupied and should be restarted
by its process supervisor; later decisions continue to fail closed to review.

## No fallback

A Laya failure is a closed `laya_jev_failed` review decision. The message stays
in Inbox, unchanged. Envelope does not retry the decision against OpenRouter, and
no automatic cross-provider fallback exists anywhere in the engine.

A wrong model or revision, malformed output, out-of-range probabilities, a
distribution that does not sum to one, a choice that is not the highest
probability option, an unreachable or unready provider, an oversized response,
and a timeout all produce the same closed outcome.

To reconsider one such message later, name the same stored identity explicitly:

```bash
envelope engine recover 42 --account you@example.com \
  --jev-backend laya --retry-jev --confirm-new-jev-call
```

That path accepts only a stored `laya_jev_failed` decision under the `laya`
backend and its pinned model, reclassifies that exact current-UIDVALIDITY
message, and never rewinds the new-mail watermark.

## Accuracy is not confidence

Laya reports an entropy-based confidence alongside each distribution. Envelope's
existing thresholds are unchanged and conservative: a normal route needs 0.80
probability and confidence, junk and unsubscribe candidates need 0.92, and an
interrupting notification needs 0.90 on urgency, urgency confidence, and the
notify probability. A local answer that does not clear its threshold abstains to
`review` rather than acting. Expect more abstentions locally than with
OpenRouter, which is the intended fail-closed direction.

## Live verification

An ignored integration test exercises the real production endpoint end to end:

```bash
python3 scripts/laya_jev_provider.py serve &
cargo test -p envelope-email-transport --test laya_live -- --ignored --nocapture
```

It touches no mailbox and sends no mail.
