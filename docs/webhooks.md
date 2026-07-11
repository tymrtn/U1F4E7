# Envelope webhooks

Envelope can deliver events (new mail, draft created, send queued, etc.) to any
HTTP endpoint via durable webhook push with HMAC-SHA256 signing and
exponential-backoff retries.

---

## Adding a route

```bash
envelope events routes add --url https://your-service.internal/webhook
# Output includes the signing secret ONCE:
#   route_id: abc123
#   secret: whsec_<64-char-hex>   ← copy and store now; never shown again
```

Scope to specific event types:

```bash
envelope events routes add \
  --url https://your-service.internal/webhook \
  --event-types message.received,draft.created
```

Scope to a single account:

```bash
envelope events routes add \
  --url https://your-service.internal/webhook \
  --account you@example.com
```

List routes (secret shown as a short prefix only):

```bash
envelope events routes list
```

Remove a route:

```bash
envelope events routes remove <route-id>
```

---

## HMAC-SHA256 verification

Every delivery includes an `X-Envelope-Signature` header containing a hex-encoded
HMAC-SHA256 signature computed over the raw request body with your route secret.

Verify in Python:

```python
import hashlib, hmac

def verify_envelope_signature(secret: str, body: bytes, header: str) -> bool:
    """Verify X-Envelope-Signature over the raw request body."""
    expected = hmac.new(
        secret.encode(),
        body,
        hashlib.sha256,
    ).hexdigest()
    return hmac.compare_digest(expected, header)

# In a Flask handler:
# sig = request.headers.get("X-Envelope-Signature", "")
# ok = verify_envelope_signature(ROUTE_SECRET, request.get_data(), sig)
# if not ok:
#     return "invalid signature", 401
```

Verify in Node.js:

```js
const crypto = require("crypto");

function verifyEnvelopeSignature(secret, body, header) {
  const expected = crypto
    .createHmac("sha256", secret)
    .update(body)
    .digest("hex");
  return crypto.timingSafeEqual(Buffer.from(expected), Buffer.from(header));
}
```

Always use a constant-time comparison to prevent timing attacks.

---

## Delivery semantics and deduplication

Envelope delivers **at-least-once**. The same event can be delivered more than once
(e.g., after a retry). Receivers **must** deduplicate on the
`X-Envelope-Delivery` header, which contains a stable delivery ID for each event +
route pair.

```
X-Envelope-Delivery: del_abc123def456   ← dedupe on this
X-Envelope-Signature: <hex>
```

Store received delivery IDs and discard duplicates before processing.

---

## Backoff and dead-letter behavior

- Delivery is retried with exponential backoff on non-2xx responses or connection
  errors.
- After the retry budget is exhausted, the delivery is moved to dead-letter status.
- Dead-lettered deliveries do not retry automatically; trigger a manual retry:

```bash
# List deliveries (pending / dead / delivered / all)
envelope events deliveries list --status dead

# Retry a dead-lettered delivery
envelope events deliveries retry <delivery-id>
```

---

## Legacy routes and unsigned deliveries

Routes created before signing support was added do not have a secret. These routes
send events without an `X-Envelope-Signature` header. To add signing, remove and
recreate the route:

```bash
envelope events routes remove <old-route-id>
envelope events routes add --url <same-url>   # new secret issued
```

---

## Security note

Route URLs are operator-trusted. Envelope does not validate that the destination
URL is safe before POSTing. If your Envelope node can reach internal network
addresses, a misconfigured route URL could POST event payloads to internal services.
Keep webhook endpoint URLs on external or explicitly trusted hosts, or restrict
network egress from the machine running Envelope.
