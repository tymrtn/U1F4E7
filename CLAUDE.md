# Envelope Development Notes

## Commands
- Format: `cargo fmt --check`
- Tests: `cargo test --workspace`
- Full clippy currently has pre-existing store lint debt; use baseline-aware review before treating `cargo clippy --workspace --all-targets -- -D warnings` as a feature regression.

## Evidence export invariants
- Evidence collection must be read-only against mailboxes: use `EXAMINE` and `BODY.PEEK[]`; never mark messages read or mutate mailbox state.
- Raw RFC822 `.eml` files are canonical evidence; preserve full headers and attachments inside the `.eml`.
- Evidence bundles must include verifiable manifest/index/checksum material and reject traversal or symlink tricks during verification.
- Thread expansion is header-based only for MVP (`Message-ID`, `In-Reply-To`, `References`) and must remain bounded; do not add subject-only fallback without explicit warning semantics and tests.
- Do not include secrets in manifests, logs, docs, or examples. Provenance paths/account metadata are intentionally included but should be treated as sensitive.
