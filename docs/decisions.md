# Decisions provider

Envelope can ask a decision model typed questions about a message. Two features
use it, and both are off by default:

- **The mail engine** (`envelope engine once|run|recover`) asks where new mail
  belongs: route, urgency, whether to notify you, whether it needs a reply, and
  whether it is bulk mail. It runs only when you start it.
- **rShield's Jev analyzer** (`threat.analyzers.jev`) asks whether a message is
  phishing, whom it impersonates, and what it asks you to do.

The model answers typed questions (`choice` and `noul`) and never writes text.
Envelope validates every answer against the options it asked for and treats
anything malformed as no answer. Code decides what happens next, never the
model.

## What leaves your machine

With a hosted provider (the default when either feature is on), each decision
sends this to the provider you configured:

- the message subject and up to 8 KiB of its plain text (derived from the HTML
  part when there is no text part);
- the sender's address and domain;
- counts of your past mail with that sender (received, sent, replied threads)
  and the message's read/unread/junk flags and whether it has attachments.

It never sends credentials, recipient lists, Message-IDs, attachment bytes,
local paths or unsubscribe URLs. The API key is read from an environment
variable at call time and is never written to disk or logged.

rShield records every hosted call as a `lookup_performed` event on the message,
with the provider, the model and the number of bytes sent. The event holds no
message content. The mail engine keeps its own decision rows
(`envelope engine decisions`).

With `decisions.provider laya`, nothing leaves the machine (see below).

## Configuration

| Key | Default | Meaning |
|---|---|---|
| `decisions.provider` | `openrouter` | `openrouter`, `laya` or `custom` |
| `decisions.base_url` | `https://openrouter.ai/api/alpha/decisions` | Endpoint; must be https. Required for `custom` |
| `decisions.model` | `typesafe/jev-1.13` | Model id. Required for `custom` |
| `decisions.key_env` | `OPENROUTER_API_KEY` | Name of the environment variable holding the key |
| `decisions.propose_actions` | `false` | Offer actions from confident engine routes (below) |
| `decisions.route_actions.<route>` | see below | The action a route proposes |
| `threat.analyzers.jev` | `false` | Turn on rShield's Jev analyzer |
| `threat.jev.required` | `false` | Treat a failed Jev call as an `unavailable` verdict |

```bash
export OPENROUTER_API_KEY=sk-or-...          # whitespace in the value is stripped
envelope config set threat.analyzers.jev true
envelope threat scan --account you@example.com --limit 5
```

A `custom` provider must speak the same Decisions API as OpenRouter's
(`POST {model, state, questions}`, answers under `answers`):

```bash
envelope config set decisions.provider custom
envelope config set decisions.base_url https://decide.example.net/v1/decisions
envelope config set decisions.model acme/judge-2
envelope config set decisions.key_env ACME_DECISIONS_KEY
```

Hosted endpoints go through Envelope's guarded HTTP client: the host must
resolve to public addresses, redirects are not followed, and a request times
out after 20 seconds. An invalid value is refused when you set it.

There is no fallback. If the configured provider fails, the engine files the
message for human review and rShield records the analyzer as skipped with the
reason. Neither ever retries on a different provider.

## Running locally with Laya

`decisions.provider laya` sends the same typed state and questions to a
pinned Laya-MLX checkpoint served on `http://127.0.0.1:8791`. It takes no API
key, bypasses proxies, and refuses any address other than IPv4 loopback. Laya
uses a fixed endpoint and model, so `decisions.base_url`, `decisions.model` and
`decisions.key_env` are refused while it is selected.

```bash
python3 scripts/laya_jev_provider.py setup   # pre-fetch and verify the checkpoint
python3 scripts/laya_jev_provider.py serve   # keep running
envelope engine laya-health
envelope config set decisions.provider laya
```

`--jev-backend laya|openrouter|custom` on `envelope engine` overrides the
configured provider for one run. Setup and checksum details are in
[jev-laya-provider.md](jev-laya-provider.md).

## rShield weights

| Answer | Signal | Weight |
|---|---|---|
| `phishing_risk` ≥ 0.90 | `jev_phishing_risk` | 40 |
| `phishing_risk` ≥ 0.70 | `jev_phishing_risk` | 15 |
| risk ≥ 0.70 and `impersonation` other than `none` at ≥ 0.80 | `jev_impersonation` | 10 |

Jev's signals together are capped at 69, one below the `dangerous` threshold,
so the model alone can never make a message dangerous. It can push a message
that local analyzers already found suspicious over the line.

## Proposed actions

With `decisions.propose_actions true`, a route that clears the engine's
confidence thresholds becomes an offer: an `action_offered` event (listed by
`envelope events list`, attributed to source `jev`). Nothing runs until you
apply it with `envelope actions confirm <event-id>`, or drop it with
`envelope actions dismiss <event-id>`. An offer can only add a tag, set a flag,
or move to a folder other than Trash or Junk, and each message gets at most
one.

| Route | Default offer |
|---|---|
| `follow_up` | `add_tag:follow-up` |
| `important` | `add_tag:important` |
| `digest_news` | `add_tag:digest` |
| `unsubscribe_candidate` | `add_tag:unsubscribe-candidate` |
| `junk`, `routine`, `review` | none |

```bash
envelope config set decisions.propose_actions true
envelope config set decisions.route_actions.important move:Receipts
envelope config set decisions.route_actions.digest_news none
```
