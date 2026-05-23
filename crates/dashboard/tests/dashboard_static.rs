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
            && DASHBOARD_JS.contains("Today / Needs Attention")
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
    assert!(
        DASHBOARD_CSS.contains(".mail-shell")
            && DASHBOARD_CSS.contains("grid-template-columns: 280px")
            && DASHBOARD_CSS.contains(".reader-pane"),
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
    assert!(
        DASHBOARD_JS.contains("not_available: bulk action execution is not wired yet"),
        "prototype-only bulk actions should be honest instead of pretending to mutate mail"
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
    assert!(
        DASHBOARD_JS.contains("Reconnect")
            && DASHBOARD_JS.contains("not_available: reconnect flow is not wired yet"),
        "reconnect should be visible but honest until recovery flow is wired"
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
    assert!(
        !DASHBOARD_JS.contains("/accounts/${acct.id}/verify"),
        "rendering account health must not fire live auth probes from first-paint sidebar rendering"
    );
}
