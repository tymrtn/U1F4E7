# Orphans Audit — 2026-04-09

## Summary

A full audit of every commit in `envelope-email-rs` history was performed on 2026-04-09 to find `.rs` files that were added without corresponding `mod` declarations. Orphaned `.rs` files are silently ignored by `cargo build` — the compiler never reads them, so entire features can vanish without any build error.

**Finding: exactly one commit produced orphans — `27f3919 feat(drafts): IMAP-first draft management`.** Every other commit in the repo's history wired its source files correctly.

## Orphaned files (all from commit 27f3919)

| Path | Lines | Feature |
|---|---|---|
| `crates/store/src/snoozed.rs` | 455 | Snooze feature — impl Database with create/get/list/find/due_before |
| `crates/cli/src/commands/snooze.rs` | 821 | Snooze CLI — flexible datetime parsing, 5 reason codes, IMAP folder management |
| `crates/email/src/threading.rs` | 796 | Threading — 11-language subject normalization, RFC 2822 header parsing, `build_threads` |
| `crates/store/src/threads.rs` | 1,176 | Thread + ThreadMessage CRUD, `find_thread_by_uid`, thread context |
| `crates/cli/src/commands/thread.rs` | 239 | Thread CLI — `envelope thread <uid>` conversation view |
| **Total** | **3,487** | |

The same commit additionally **deleted** the backing `Thread`, `ThreadMessage`, and `SnoozedMessage` model structs from `crates/store/src/models.rs` and the corresponding `CREATE TABLE threads`, `CREATE TABLE thread_messages`, and `CREATE TABLE snoozed` statements from `crates/store/src/db.rs`. Net result: two entire feature systems (threading + snooze, ~3,487 lines) were shipped as cargo cult on disk, never compiled, never runnable.

`cargo build` reported success because orphan files don't block compilation.

## Root cause

Two linked failures:

1. **No orphan detection in CI.** Cargo's permissive treatment of orphan `.rs` files let the regression ship. Added `ci/check-orphans.sh` (see below) to fail CI when any non-exempt `.rs` file in `crates/*/src/` is not declared via `mod` or `pub mod`.

2. **Multi-feature commit with misleading subject.** The commit subject `feat(drafts): IMAP-first draft management` suggested a narrow drafts refactor. Reviewing `git show 27f3919 --stat` would have shown 20+ files touched across snooze, threading, drafts, AND model deletions — but that was never reviewed. Drafts *did* work; the other two features silently did not.

## Resolution (v0.3.0)

- **Orphan detection guard added.** `ci/check-orphans.sh` runs on every commit and CI push. It initially flagged all 5 orphans from `27f3919`.
- **Snooze feature restored in v0.3.0.** `SnoozedMessage` model recreated, `snoozed` table DDL recreated, impl audited and wired, CLI subcommand wired, background unsnooze worker added. Dashboard Snoozed folder view added.
- **Threading feature restored in v0.3.0.** `Thread` + `ThreadMessage` models recreated, `threads` + `thread_messages` tables recreated, both impl files audited and wired, `thread` CLI subcommand wired. Dashboard thread view uses the restored stack.
- **CLAUDE.md hard rule.** Every new `.rs` file in `crates/*/src/` must be declared via `mod` within the same session it's created. CI enforces this.

## Lessons

- Cargo's definition of "builds" is not "works." Orphan files are a compile-time silent failure mode.
- Commit subjects must reflect the full scope of the change. "feat(drafts)" covering a drafts refactor + snooze feature + threading feature + model deletions is a process failure.
- Every feature that touches CLI or IMAP must have a smoke test against a real account before the commit lands. "`cargo test` passes" is not evidence that `envelope thread <uid>` actually does anything.
- Memory: see `feedback_verify_end_to_end.md` in the project memory directory.
