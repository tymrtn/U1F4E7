// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2

const DASHBOARD_JS: &str = include_str!("../static/dashboard.js");
const DASHBOARD_CSS: &str = include_str!("../static/dashboard.css");
const DASHBOARD_HTML: &str = include_str!("../static/index.html");

#[test]
fn dashboard_rewrites_cid_images_and_blocks_remote_images_by_default() {
    assert!(
        DASHBOARD_JS.contains("function sanitizeEmailHtml"),
        "dashboard HTML renderer should sanitize and rewrite email HTML before srcdoc"
    );
    assert!(
        DASHBOARD_JS.contains("cid:") && DASHBOARD_JS.contains("content_id"),
        "dashboard renderer should map cid: image refs to attachment metadata"
    );
    assert!(
        DASHBOARD_JS.contains("data-remote-src") && DASHBOARD_JS.contains("Remote images blocked"),
        "remote images should be blocked by default with explicit reader UI"
    );
    assert!(
        DASHBOARD_JS.contains("Load remote images"),
        "reader needs an explicit load control for remote images"
    );
}

#[test]
fn dashboard_email_sanitizer_blocks_non_img_remote_load_surfaces() {
    for marker in [
        "script, style",
        "svg, image",
        "name === 'background'",
        "name === 'srcset'",
        "name === 'poster'",
        "xlink:href",
        "function isProtocolRelativeUrl",
        "function isBlockedEmailUrl",
        "function hasCssUrlLoad",
        "isBlockedEmailUrl(trimmed)",
        "isInlineAttachment(attachment)",
    ] {
        assert!(
            DASHBOARD_JS.contains(marker),
            "email sanitizer should fail closed for remote-load surface marker: {marker}"
        );
    }
}

#[test]
fn dashboard_reader_sizing_is_not_fixed_to_legacy_400px_iframe() {
    assert!(
        !DASHBOARD_JS.contains("minHeight = '400px'")
            && !DASHBOARD_JS.contains("min-height: 400px"),
        "reader iframe should not be pinned to the old 400px minimum"
    );
    assert!(
        DASHBOARD_JS.contains("className = 'email-frame'")
            || DASHBOARD_JS.contains("className = \"email-frame\""),
        "HTML email iframe should use the shared responsive reader sizing class"
    );
    assert!(
        DASHBOARD_CSS.contains(".email-frame") && DASHBOARD_CSS.contains("min-height: min("),
        "reader CSS should size against available viewport with scrolling"
    );
}

#[test]
fn dashboard_attachment_list_labels_inline_images_separately() {
    assert!(
        DASHBOARD_JS.contains("isInlineAttachment"),
        "dashboard should classify inline image attachments"
    );
    assert!(
        DASHBOARD_JS.contains("Inline images") && DASHBOARD_JS.contains("Downloadable attachments"),
        "attachment list should distinguish inline images from true downloads"
    );
}

#[test]
fn dashboard_message_deep_links_are_route_first_without_legacy_slug_gate() {
    assert_eq!(
        DASHBOARD_JS.matches("function parseDashboardRoute").count(),
        1,
        "dashboard should have one route parser; duplicate declarations can mask direct-message behavior"
    );
    assert_eq!(
        DASHBOARD_JS
            .matches("async function applyDashboardRoute")
            .count(),
        1,
        "dashboard should have one route applier; duplicate declarations make route-first review fragile"
    );
    assert!(
        DASHBOARD_JS.contains("kind: 'message'")
            && DASHBOARD_JS.contains("await openMessage(route.uid, route.accountId"),
        "message deep links should parse `/accounts/<id>/messages/<uid>` and open the target message directly"
    );
    assert!(
        DASHBOARD_JS.contains("loadAccounts({ autoSelect: false })")
            && DASHBOARD_JS.contains("const routed = await applyDashboardRoute(route)"),
        "deep-link boot should load only account metadata before applying the target route"
    );
    assert!(
        !DASHBOARD_JS.contains("resolveAccountSlug(state.route.accountSlug)"),
        "account-id message routes must not pass through the old accountSlug gate before route application"
    );
}

#[test]
fn dashboard_phase1_shell_uses_three_pane_mail_layout() {
    assert!(
        DASHBOARD_HTML.contains("class=\"mail-shell\"")
            && DASHBOARD_HTML.contains("id=\"mail-sidebar\"")
            && DASHBOARD_HTML.contains("id=\"message-pane\"")
            && DASHBOARD_HTML.contains("id=\"reader\" class=\"reader-pane\""),
        "dashboard should render a left mailbox sidebar, middle list, and right reader pane"
    );
    assert!(
        DASHBOARD_JS.contains("Unified Inbox")
            && DASHBOARD_JS.contains("Needs Attention")
            && DASHBOARD_JS.contains("Snoozed")
            && DASHBOARD_JS.contains("Sent")
            && DASHBOARD_JS.contains("Drafts")
            && DASHBOARD_JS.contains("All Mail"),
        "left sidebar should expose the Phase 1 mailbox set"
    );
    assert!(
        DASHBOARD_HTML.contains("id=\"accounts-list\" class=\"account-tree\"")
            && DASHBOARD_JS.contains("renderAccountMailboxButtons"),
        "accounts should render as mailbox groups with nested folders when available"
    );
    // Mail shell still uses a fixed three-pane grid; sidebar width may vary
    // (260px desktop / 240px ≤1280) so the assertion is structural, not
    // pixel-exact.
    assert!(
        DASHBOARD_CSS.contains(".mail-shell")
            && DASHBOARD_CSS.contains(".reader-pane")
            && DASHBOARD_CSS.contains(
                ".mail-shell {\n  min-height: 0;\n  display: grid;\n  grid-template-columns:"
            ),
        "CSS should size the operator mail shell as fixed dashboard panes"
    );
}

#[test]
fn dashboard_cockpit_is_attention_strip_with_expandable_panel() {
    assert!(
        DASHBOARD_HTML.contains("class=\"attention-strip\"")
            && DASHBOARD_HTML.contains("id=\"btn-toggle-cockpit\"")
            && DASHBOARD_HTML.contains("id=\"cockpit-panel\"")
            && DASHBOARD_HTML.contains("hidden"),
        "Agent Cockpit should start as an attention strip with an expandable detail panel"
    );
    assert!(
        DASHBOARD_JS.contains("function setCockpitExpanded")
            && DASHBOARD_JS.contains("aria-expanded")
            && DASHBOARD_JS.contains("setCockpitExpanded(false)"),
        "cockpit expansion should be keyboard/screen-reader visible and collapsed on first paint"
    );
}

#[test]
fn dashboard_first_paint_does_not_auto_probe_account_mailboxes() {
    assert!(
        DASHBOARD_JS.contains("await selectUnifiedInbox();"),
        "normal boot should land on the cached unified inbox surface"
    );
    assert!(
        DASHBOARD_JS.contains("api(refresh ? 'POST' : 'GET'")
            && DASHBOARD_JS.contains("/messages/unified/refresh?limit=50"),
        "unified inbox refresh should be explicit; default load should use cached local index"
    );
    assert!(
        !DASHBOARD_JS.contains("selectAccount(state.accounts[0])"),
        "single-account first paint must not auto-select the account and trigger live IMAP folder/message probes"
    );
    assert!(
        DASHBOARD_JS.contains("First paint does not probe IMAP credentials"),
        "empty/local states should be honest about avoiding live credential probes"
    );
}

#[test]
fn dashboard_message_list_exposes_shared_message_primitives() {
    assert!(
        DASHBOARD_JS.contains("function messagePrimitive")
            && DASHBOARD_JS.contains("state:")
            && DASHBOARD_JS.contains("actions:")
            && DASHBOARD_JS.contains("audit_event")
            && DASHBOARD_JS.contains("render_hint")
            && DASHBOARD_JS.contains("rollback_token")
            && DASHBOARD_JS.contains("equivalent_cli"),
        "message rows should be backed by the smaller shared mail primitive contract, not CLI flag sprawl"
    );
    assert!(
        DASHBOARD_JS.contains("data-primitive") && DASHBOARD_JS.contains("message"),
        "message rows should advertise their primitive kind for human/agent UI handoff"
    );
}

#[test]
fn dashboard_message_list_has_mail_client_density_controls() {
    assert!(
        DASHBOARD_HTML.contains("id=\"message-bulk-toolbar\"")
            && DASHBOARD_HTML.contains("id=\"select-all-messages\""),
        "message list should have a persistent bulk-selection toolbar"
    );
    for marker in [
        "msg-select",
        "msg-star",
        "msg-sender",
        "msg-subject-line",
        "msg-snippet",
        "msg-labels",
        "msg-attachment",
        "msg-date",
    ] {
        assert!(
            DASHBOARD_JS.contains(marker),
            "message rows should expose Gmail-grade density marker: {marker}"
        );
    }
}

#[test]
fn dashboard_bulk_toolbar_prefers_mail_primitives_over_cli_matrix() {
    for marker in [
        "bulk-archive",
        "bulk-move",
        "bulk-label",
        "bulk-spam",
        "bulk-delete",
        "copy-equivalent-cli",
        "Selected message primitives",
    ] {
        assert!(
            DASHBOARD_HTML.contains(marker) || DASHBOARD_JS.contains(marker),
            "bulk triage toolbar should expose reviewed primitive actions: {marker}"
        );
    }
    // Honest about pending capability: the bulk-action handler must report
    // that mutation is not yet shipping — but it does so with operator-
    // friendly copy, not raw `not_available` telemetry strings.
    assert!(
        DASHBOARD_JS.contains("arrives in the next dashboard release")
            || DASHBOARD_JS.contains("arriving in v0.10.0"),
        "prototype-only bulk actions should announce when they'll ship, not pretend to mutate mail"
    );
    assert!(
        !DASHBOARD_JS.contains("toast('not_available")
            && !DASHBOARD_JS.contains("toast(\"not_available"),
        "user-facing toasts must not surface raw `not_available` telemetry strings"
    );
}

#[test]
fn dashboard_account_sidebar_exposes_account_health_primitives() {
    assert!(
        DASHBOARD_JS.contains("function accountHealthPrimitive")
            && DASHBOARD_JS.contains("primitive: 'account_health'")
            && DASHBOARD_JS.contains("state:")
            && DASHBOARD_JS.contains("actions:")
            && DASHBOARD_JS.contains("audit_event")
            && DASHBOARD_JS.contains("render_hint")
            && DASHBOARD_JS.contains("rollback_token")
            && DASHBOARD_JS.contains("equivalent_cli"),
        "account rows should be backed by the shared account_health primitive contract"
    );
    for status in [
        "healthy",
        "syncing",
        "stale",
        "auth_failed",
        "rate_limited",
        "unavailable",
        "reconnecting",
    ] {
        assert!(
            DASHBOARD_JS.contains(status),
            "account health primitive should name stable status: {status}"
        );
    }
}

#[test]
fn dashboard_account_sidebar_renders_health_badges_and_recovery_actions() {
    for marker in [
        "account-health-badge",
        "account-health-status",
        "account-sync-meta",
        "account-provider-capabilities",
        "account-reconnect",
        "data-primitive",
        "account_health",
    ] {
        assert!(
            DASHBOARD_JS.contains(marker) || DASHBOARD_CSS.contains(marker),
            "account hierarchy should expose account health UI marker: {marker}"
        );
    }
    // Reconnect must exist as an account-health affordance, and the agent
    // contract for the primitive must remain honest that the recovery flow
    // isn't wired yet (per CLAUDE.md aggregate-cockpit invariants).
    assert!(
        DASHBOARD_JS.contains("Reconnect")
            && DASHBOARD_JS.contains("not_available: reconnect flow is not wired yet"),
        "reconnect primitive contract should remain honest until recovery flow is wired"
    );
    // Contextual affordance regression guard: the Reconnect button must
    // only render when an account is actually unhealthy — never on a
    // healthy account.
    assert!(
        DASHBOARD_JS.contains("HEALTH_NEEDS_ACTION")
            && DASHBOARD_JS.contains("HEALTH_NEEDS_ACTION.has(status)"),
        "Reconnect button must be gated on an unhealthy status, not rendered unconditionally"
    );
}

#[test]
fn dashboard_account_health_uses_local_signals_without_first_paint_auth_probe() {
    assert!(
        DASHBOARD_JS.contains("deriveAccountHealth")
            && DASHBOARD_JS.contains("state.unifiedMeta")
            && DASHBOARD_JS.contains("state.cockpit"),
        "account health should derive from cached unified/cockpit signals before live recovery exists"
    );
    // First-paint regression guard: the `deriveAccountHealth` render path
    // must not call `/verify`. The verify URL IS now wired into the
    // reconnect click handler — which is a deliberate user action, not
    // first-paint render. We check that by locating `deriveAccountHealth`
    // and ensuring the verify URL doesn't appear inside its function body.
    let derive_start = DASHBOARD_JS
        .find("function deriveAccountHealth(")
        .expect("deriveAccountHealth function present");
    let derive_body_end = DASHBOARD_JS[derive_start..]
        .find("\n}\n")
        .map(|offset| derive_start + offset)
        .unwrap_or(DASHBOARD_JS.len());
    let derive_body = &DASHBOARD_JS[derive_start..derive_body_end];
    assert!(
        !derive_body.contains("/verify"),
        "deriveAccountHealth must not call the /verify endpoint from the render path"
    );
}

#[test]
fn dashboard_ships_gmail_keyboard_shortcut_layer() {
    // Single source of truth for shortcuts plus a discoverable cheat sheet.
    assert!(
        DASHBOARD_JS.contains("const KEYBOARD_SHORTCUTS")
            && DASHBOARD_JS.contains("function handleGlobalKeydown"),
        "dashboard should define a keyboard shortcut table and global handler"
    );
    assert!(
        DASHBOARD_JS.contains("addEventListener('keydown', handleGlobalKeydown)"),
        "global keydown handler must be wired during event setup"
    );
    // Typing in inputs/textareas must never trigger navigation shortcuts.
    assert!(
        DASHBOARD_JS.contains("function isTextEntryFocused"),
        "keyboard handler must suppress shortcuts while text entry is focused"
    );
    assert!(
        DASHBOARD_HTML.contains("id=\"shortcut-sheet\"")
            && DASHBOARD_HTML.contains("id=\"shortcut-sheet-list\""),
        "index.html should contain the keyboard cheat-sheet modal"
    );
}

#[test]
fn dashboard_supports_shift_click_range_select_and_focus_nav() {
    assert!(
        DASHBOARD_JS.contains("function selectMessageRange"),
        "dashboard should support shift-click range selection like Gmail"
    );
    assert!(
        DASHBOARD_JS.contains("function moveMessageFocus")
            && DASHBOARD_JS.contains("state.focusedMessageKey"),
        "dashboard should track and move a focused message row for j/k navigation"
    );
    assert!(
        DASHBOARD_CSS.contains(".msg-row.focused"),
        "focused message row needs a visible affordance"
    );
}

#[test]
fn dashboard_reader_frame_autosizes_without_enabling_scripts() {
    assert!(
        DASHBOARD_JS.contains("function sizeReaderFrameToContent"),
        "HTML email iframe should size to its content height"
    );
    // Security invariant: same-origin may be granted for measurement, but
    // scripts must never be enabled on the email iframe. Guard against the
    // actual sandbox token (quoted), not prose mentioning the flag.
    assert!(
        !DASHBOARD_JS.contains("allow-scripts'") && !DASHBOARD_JS.contains("allow-scripts "),
        "email iframe must never enable the allow-scripts sandbox token"
    );
    assert!(
        DASHBOARD_JS.contains("setAttribute('sandbox', 'allow-same-origin')"),
        "email iframe uses allow-same-origin only, for height measurement"
    );
}

#[test]
fn dashboard_bulk_status_auto_clears_terminal_results() {
    assert!(
        DASHBOARD_JS.contains("autoClear") && DASHBOARD_JS.contains("state.bulkStatusTimer"),
        "terminal bulk-status summaries should auto-clear instead of lingering"
    );
}

#[test]
fn dashboard_sending_from_imap_draft_deletes_original_draft() {
    // Issue #61: composing from an IMAP Drafts message must clean up the
    // original draft after a successful send.
    assert!(
        DASHBOARD_JS.contains("composeImapDraft"),
        "composer state should track the originating IMAP draft {{accountId, uid, folder}}"
    );
    assert!(
        DASHBOARD_JS.contains("'imap-draft'"),
        "composing from an IMAP draft should use a dedicated compose mode"
    );
    assert!(
        DASHBOARD_JS.contains("state.composeMode = 'imap-draft'"),
        "openComposerFromImap should mark the composition as imap-draft"
    );
    // The cleanup DELETE must run on the send path, scoped to the original
    // draft's uid/folder, and must not be conflated with the send itself.
    assert!(
        DASHBOARD_JS.contains("state.composeMode === 'imap-draft' ? state.composeImapDraft : null"),
        "send path should only delete when the composition came from an IMAP draft"
    );
    assert!(
        DASHBOARD_JS.contains("`/accounts/${origin.accountId}/messages/${origin.uid}?folder=")
            && DASHBOARD_JS.contains("'DELETE'"),
        "successful send from an IMAP draft should DELETE the original draft uid in its folder"
    );
    assert!(
        DASHBOARD_JS.contains("could not be removed"),
        "a draft-delete failure must be surfaced separately and not reported as a send failure"
    );
}
