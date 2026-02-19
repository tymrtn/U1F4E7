# /architect

Architecture review skill for Envelope Email.

## When to Use

Run `/architect` when proposing changes that affect:
- New endpoints or API surface changes
- Database schema modifications
- Transport layer (SMTP/IMAP) integration
- Credential storage or security boundaries
- Cross-cutting concerns (middleware, error handling)

## What It Does

1. Reads the proposed change or story
2. Reviews against ARCHITECTURE.md and existing patterns
3. Identifies risks, trade-offs, and alternatives
4. Produces a recommendation with rationale

## Output Format

```
## Architecture Review

**Change**: <summary>
**Risk**: low | medium | high
**Recommendation**: approve | revise | reject

### Analysis
<detailed review>

### Trade-offs
<what you gain, what you lose>

### Alternatives Considered
<other approaches and why they were rejected>
```
