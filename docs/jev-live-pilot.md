# JEV mail-engine live pilot

This checklist is the authority-gated proof required before publishing the Show HN post. Run it only on an owned mailbox with an approved OpenRouter key and an approved notification target.

## Preconditions

- Candidate commit has passed workspace tests, frontend tests/check/build, contract drift, privacy scans, release build, and independent review.
- Previous installed Envelope binary is backed up with its version and checksum.
- Candidate reports the intended prerelease version from the actual installed path.
- Test account, recipient, and notification route are owned and reversible.
- Automatic junk movement remains off for the shadow phase.

## Install and process proof

1. Install the candidate through the real local path.
2. Verify `command -v envelope` and `envelope --version`.
3. Install, but do not silently enable, the platform engine service.
4. Add `OPENROUTER_API_KEY` to the owner-only engine environment file.
5. Enable the service explicitly.
6. Record the process/service identity and start time.
7. Verify `envelope engine status` matches the service state.

## Controlled journey

1. Run the first pass against INBOX.
   - Expected: `baselined`.
   - Expected JEV requests: zero.
   - Expected historical decisions: zero.
2. Deliver controlled messages representing:
   - needs reply;
   - important but not urgent;
   - urgent notification;
   - routine;
   - news digest;
   - obvious junk;
   - unsubscribe candidate;
   - ambiguous review.
3. Wait through one five-minute interval.
4. Verify each message has exactly one durable decision and no later UID advanced over unfinished work.
5. Verify the dashboard’s sender, subject, route, urgency, confidence, and link against the actual messages.
6. Correct one route in the dashboard.
   - Expected: original model route remains visible.
   - Expected: correction revision increments.
   - Expected: no rule is created.
7. Compile a digest.
   - Expected IMAP access: `EXAMINE` plus header-only `BODY.PEEK`.
   - Expected: source messages remain unread and unmoved.
8. Consume the digest and reload.
   - Expected: successful items disappear from the pending digest.
   - Expected: failed items remain pending.
9. Configure and test one `mail_engine_urgent` route.
10. Deliver one controlled urgent message and verify:
    - deterministic local event;
    - one delivery row;
    - signed downstream receipt;
    - no subject, sender, body, recipient, or secret in the event payload.
11. Restart the engine and dashboard.
    - Expected: decisions, correction, digest state, and delivery state persist.
    - Expected: no duplicate JEV request, urgent event, or delivery.

## Failure cases

- Remove the OpenRouter key before a controlled new message.
  - Expected: `openrouter_api_key_missing` review state.
  - Expected network request: zero.
  - Restore the key and run the explicit safe retry once.
- Simulate an interrupted pre-request claim older than ten minutes.
  - Expected: `decision_interrupted` human-review state.
  - Expected new JEV request: zero.
- Submit a correction with a stale revision.
  - Expected: revision conflict and no state change.
- Leave urgent delivery unconfigured.
  - Expected: dashboard says `not configured`; no claim of external alerting.

## Acceptance

The preview is ready to publish only when:

- every controlled message appears in the correct human bucket or explicit review;
- no destructive or outbound action occurs in observe-only mode;
- no test message is skipped or charged twice;
- correction, digest consumption, restart recovery, and one alert delivery are proven through installed runtime state;
- the dashboard and CLI agree;
- the public post uses only claims demonstrated by this run.

## Rollback

- Stop and unload the engine service.
- Restore the backed-up binary.
- Verify the prior version.
- Keep the candidate database backup for diagnosis; do not delete mailbox evidence.
