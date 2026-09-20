# Show HN draft: Envelope’s new-mail decision engine

## Title

Show HN: Envelope – a local email runtime that uses JEV to triage new mail without rules

## Post

Envelope is a local Rust email runtime for agents and human operators. This preview adds a new-mail decision engine backed by `typesafe/jev-1.13` through OpenRouter’s Decisions API.

On first use, Envelope records the current INBOX UID and starts from there. It does not upload or classify historical mail. Every later message is evaluated once with bounded context that Envelope already knows locally: sender and domain history, read/unread/junk counts, previous inbound and outbound interactions, reply history, thread facts, message flags, subject, and up to 8 KiB of derived text.

The typed result can place a message in one of seven routes:

- needs reply;
- important;
- routine;
- news digest;
- junk;
- unsubscribe candidate;
- human review.

Urgency and user-notification intent are separate typed decisions. Envelope, rather than JEV, owns the policy thresholds and side effects.

The dashboard shows the resulting queues in attention order. A person can open the exact message or correct its route without creating a rule. Corrections are revision-guarded local records; they do not overwrite the original model result or claim to retrain the model.

The default is observe-only. Envelope does not send mail, delete messages, unsubscribe, or move junk unless the operator separately enables the relevant bounded action. Automatic junk movement remains an explicit `--apply` option and requires UIDPLUS. The packaged five-minute service intentionally omits `--apply`.

The news digest reads only `From`, `Subject`, and `Date` with IMAP `EXAMINE` and `BODY.PEEK`. A person can consume compiled items locally without marking source messages read or moving them. Urgent events are content-free and durable; external delivery requires a separately configured signed event route and `--deliver`.

Data sent to OpenRouter for each new message:

- normalized sender address and domain;
- subject and at most 8 KiB of derived plain text;
- received timestamp, read/unread/junk flags, and attachment presence;
- bounded sender, interaction, reply, and thread statistics.

Envelope excludes credentials, local paths, Message-IDs, recipient lists, attachment bytes, unsubscribe URLs, and raw provider errors. Production transport is pinned to the OpenRouter Decisions endpoint.

The engine runs as a foreground CLI or an opt-in launchd/systemd service every five minutes. Paid-call claims, mailbox watermarks, digest consumption, corrections, notification events, delivery retries, and review states survive restarts in local SQLite.

This is a prerelease. The useful test is whether it reduces inbox decisions without hiding mistakes. The repository includes the contract, safety boundaries, synthetic end-to-end fixture, dashboard tests, and the exact failure states shown to operators.

Repository: [replace with the public commit or prerelease URL before posting]

## Demo sequence

```bash
export OPENROUTER_API_KEY=...

envelope engine once --account you@example.com
envelope engine run --account you@example.com --interval-seconds 300

envelope engine status
envelope engine decisions --limit 50
envelope engine digest --limit 25
envelope serve
```

Show these in the recording:

1. First pass establishes a baseline and sends no historical mail.
2. A controlled new message arrives.
3. The next pass records one decision.
4. The dashboard places it in the expected bucket and links to the message.
5. Correct the route once; reload and show the durable correction plus original model route.
6. Compile and consume one digest item; reload and show it stays consumed while the source message remains unread and unmoved.
7. Show urgent delivery as `not configured`, then configure and test one route before claiming alerts work.
8. Restart the engine and dashboard; show the same state.

## Claims not to make

- Do not say Envelope learns from corrections. Corrections are local overrides only.
- Do not say it handles historical mail. The engine starts from a new-mail baseline.
- Do not call the digest an AI summary. It is a durable header-level rollup.
- Do not promise native push while the dashboard is closed. Remote alerts require a configured event route.
- Do not say unsubscribe is automatic. JEV only creates an unsubscribe candidate.
- Do not say junk movement is the default. It requires explicit `--apply`.
- Do not call fixture tests live-provider proof.
