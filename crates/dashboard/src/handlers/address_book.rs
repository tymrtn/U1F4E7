// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Recipient autocomplete for the compose surfaces.
//!
//! Read-only and local: the handler reads the `contacts` address history that
//! the startup backfill, the unified-inbox refresh, and `envelope thread`
//! scans reconcile (see `envelope_email_store::address_book`), and never
//! touches IMAP. Typing in a To/Cc/Bcc field therefore costs one indexed
//! SQLite read against `contacts` — never a walk of the thread cache — and no
//! network.
//!
//! The response carries address-book metadata only — an address and a display
//! name. Subjects, snippets, bodies, and the ranking signal itself stay
//! server-side.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use envelope_email_store::models::Account;
use envelope_email_store::{ADDRESS_HISTORY_CHUNK_ROWS, AddressSuggestion, Database, StoreError};
use serde::Deserialize;
use serde_json::json;

use crate::state::AppState;

/// Suggestions returned when the caller does not ask for a specific count.
pub const DEFAULT_SUGGESTION_LIMIT: u32 = 8;

/// Hard ceiling on `limit`. A recipient dropdown that runs past this stops
/// being scannable, and the cap keeps the endpoint cheap.
pub const MAX_SUGGESTION_LIMIT: u32 = 10;

/// Longest query accepted. Anything beyond this is not a prefix a human typed.
pub const MAX_QUERY_CHARS: usize = 128;

#[derive(Debug, Deserialize)]
pub struct SuggestQuery {
    #[serde(default)]
    pub q: Option<String>,
    /// Taken as a string so a non-numeric value returns this module's stable
    /// JSON error instead of axum's plain-text query-rejection body.
    #[serde(default)]
    pub limit: Option<String>,
}

/// A rejected suggestion request, rendered with the dashboard's stable
/// `{code, error}` error shape.
#[derive(Debug)]
struct SuggestError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
}

impl SuggestError {
    fn bad_request(code: &'static str, message: &'static str) -> Self {
        SuggestError {
            status: StatusCode::BAD_REQUEST,
            code,
            message,
        }
    }
}

impl IntoResponse for SuggestError {
    fn into_response(self) -> axum::response::Response {
        (
            self.status,
            Json(json!({ "code": self.code, "error": self.message })),
        )
            .into_response()
    }
}

/// GET /api/accounts/{id}/address-suggestions?q=…&limit=…
///
/// Ranked recipient suggestions drawn from the account's local address
/// history. Read-only: no IMAP, no mailbox mutation, no draft side effects.
pub async fn suggest(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
    Query(params): Query<SuggestQuery>,
) -> impl IntoResponse {
    let query = match validate_query(params.q.as_deref()) {
        Ok(query) => query,
        Err(e) => return e.into_response(),
    };
    let limit = match validate_limit(params.limit.as_deref()) {
        Ok(limit) => limit,
        Err(e) => return e.into_response(),
    };

    let db = state.db.lock().await;
    let account = match resolve_account(&db, &account_id) {
        Ok(Some(account)) => account,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "code": "account_not_found", "error": "account not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "code": "db_error", "error": e.to_string() })),
            )
                .into_response();
        }
    };

    match db.suggest_addresses(&account.id, &query, limit as usize) {
        Ok(suggestions) => Json(build_response(&account.id, &query, limit, suggestions)),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "code": "db_error", "error": e.to_string() })),
            )
                .into_response();
        }
    }
    .into_response()
}

fn build_response(
    account_id: &str,
    query: &str,
    limit: u32,
    suggestions: Vec<AddressSuggestion>,
) -> serde_json::Value {
    json!({
        "account_id": account_id,
        "query": query,
        "limit": limit,
        "suggestions": suggestions,
    })
}

fn validate_query(raw: Option<&str>) -> Result<String, SuggestError> {
    let query = raw.unwrap_or_default().trim();
    if query.is_empty() {
        // The store happily ranks a blank query, but this endpoint is a
        // while-typing surface: an empty `q` is a client bug, not a request
        // for the whole address book.
        return Err(SuggestError::bad_request(
            "address_query_required",
            "q is required and must not be blank",
        ));
    }
    if query.chars().count() > MAX_QUERY_CHARS {
        return Err(SuggestError::bad_request(
            "address_query_too_long",
            "q is longer than 128 characters",
        ));
    }
    Ok(query.to_string())
}

fn validate_limit(raw: Option<&str>) -> Result<u32, SuggestError> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(DEFAULT_SUGGESTION_LIMIT);
    };
    let limit: u32 = raw.parse().map_err(|_| {
        SuggestError::bad_request("address_limit_invalid", "limit must be a whole number")
    })?;
    if limit == 0 || limit > MAX_SUGGESTION_LIMIT {
        return Err(SuggestError::bad_request(
            "address_limit_out_of_range",
            "limit must be between 1 and 10",
        ));
    }
    Ok(limit)
}

/// Bring one account's address history up to date, a bounded chunk at a time.
///
/// The dashboard shares a single database mutex across every request, so this
/// takes and releases the lock once per chunk and yields between them rather
/// than holding it for the whole catch-up. On a first run over an existing
/// install that is six figures of thread-cache rows; afterwards it is the
/// handful of rows that arrived since, because the store reads above a durable
/// watermark.
///
/// Failures only warn. Address history is an autocomplete convenience, and
/// hiding a mailbox because a suggestion cache could not be written would be a
/// far worse outcome than a stale dropdown.
pub(crate) async fn catch_up_account(state: &AppState, account_id: &str) {
    loop {
        let pass = {
            let db = state.db.lock().await;
            db.reconcile_address_history_chunk(account_id, ADDRESS_HISTORY_CHUNK_ROWS)
        };
        match pass {
            Ok(pass) if pass.pending => tokio::task::yield_now().await,
            Ok(_) => return,
            Err(e) => {
                tracing::warn!(
                    account_id = %account_id,
                    "address history reconcile failed; recipient suggestions may be stale: {e}"
                );
                return;
            }
        }
    }
}

/// Accept either the account id or its email address, matching every other
/// account-scoped dashboard route.
fn resolve_account(db: &Database, account_id: &str) -> Result<Option<Account>, StoreError> {
    if let Some(account) = db.get_account(account_id)? {
        return Ok(Some(account));
    }
    db.find_account_by_email(account_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use envelope_email_store::CredentialBackend;
    use envelope_email_store::models::IndexedMessageInput;
    use tower::ServiceExt;

    /// Append one message to the deep thread cache the way an IMAP scan does.
    #[allow(clippy::too_many_arguments)]
    fn seed_thread_message(
        db: &Database,
        account: &str,
        uid: u32,
        message_id: &str,
        from_address: &str,
        to_addresses: &str,
        date: &str,
        is_outbound: bool,
    ) {
        seed_thread_message_with_copies(
            db,
            account,
            uid,
            message_id,
            from_address,
            to_addresses,
            None,
            None,
            date,
            is_outbound,
        );
    }

    /// The same, with the Cc/Bcc a scan retains when the headers are present.
    #[allow(clippy::too_many_arguments)]
    fn seed_thread_message_with_copies(
        db: &Database,
        account: &str,
        uid: u32,
        message_id: &str,
        from_address: &str,
        to_addresses: &str,
        cc_addresses: Option<&str>,
        bcc_addresses: Option<&str>,
        date: &str,
        is_outbound: bool,
    ) {
        let thread = db
            .create_thread(&format!("subject-{account}-{uid}"), date, date, account)
            .unwrap();
        db.upsert_thread_message(
            &thread.thread_id,
            uid,
            Some(message_id),
            None,
            None,
            if is_outbound { "Sent" } else { "INBOX" },
            from_address,
            to_addresses,
            cc_addresses,
            bcc_addresses,
            date,
            "Subject",
            is_outbound,
            Some("snippet that must never reach a suggestion"),
        )
        .unwrap();
    }

    fn seeded_state() -> AppState {
        let db = Database::open_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('acc1', 'Work', 'me@example.test', 'example.test',
                         'smtp.example.test', 587, 'imap.example.test', 993, 'x'),
                        ('acc2', 'Other', 'other@example.test', 'example.test',
                         'smtp.example.test', 587, 'imap.example.test', 993, 'x')",
                [],
            )
            .unwrap();

        db.upsert_indexed_message_summaries(
            "acc1",
            "INBOX",
            1,
            &[
                IndexedMessageInput {
                    uid: 1,
                    message_id: Some("<a@example.test>".into()),
                    from_addr: "Ada Lovelace <ada@example.test>".into(),
                    to_addr: "me@example.test".into(),
                    subject: "Victim impact statement filing".into(),
                    date: Some("Tue, 12 May 2026 12:00:00 +0000".into()),
                    flags: Vec::new(),
                    size: 10,
                    snippet: Some("the clerk has received your statement".into()),
                    thread_id: None,
                },
                IndexedMessageInput {
                    uid: 2,
                    message_id: Some("<b@example.test>".into()),
                    from_addr: "Grace Hopper <grace@example.test>".into(),
                    to_addr: "me@example.test".into(),
                    subject: "Compiler notes".into(),
                    date: Some("Wed, 13 May 2026 12:00:00 +0000".into()),
                    flags: Vec::new(),
                    size: 10,
                    snippet: Some("about the compiler".into()),
                    thread_id: None,
                },
            ],
        )
        .unwrap();

        // The deep thread cache — the source that actually holds an established
        // install's correspondents — carries a name the INBOX snapshot never
        // saw, and an outbound recipient who never wrote back.
        seed_thread_message(
            &db,
            "acc1",
            1,
            "<t1@example.test>",
            "adele@court.test",
            "me@example.test",
            "2026-05-14T12:00:00Z",
            false,
        );
        seed_thread_message_with_copies(
            &db,
            "acc1",
            2,
            "<t2@example.test>",
            "me@example.test",
            "clerk@court.test",
            Some("courtroom-deputy@court.test"),
            Some("cocounsel@court.test"),
            "2026-05-15T12:00:00Z",
            true,
        );
        db.reconcile_address_history("acc1").unwrap();

        seed_thread_message(
            &db,
            "acc2",
            1,
            "<t3@other.test>",
            "adrian@other.test",
            "other@example.test",
            "2026-05-14T12:00:00Z",
            false,
        );

        db.upsert_indexed_message_summaries(
            "acc2",
            "INBOX",
            1,
            &[IndexedMessageInput {
                uid: 1,
                message_id: Some("<c@example.test>".into()),
                from_addr: "Adam Other <adam@other.test>".into(),
                to_addr: "other@example.test".into(),
                subject: "Unrelated".into(),
                date: Some("Tue, 12 May 2026 12:00:00 +0000".into()),
                flags: Vec::new(),
                size: 10,
                snippet: None,
                thread_id: None,
            }],
        )
        .unwrap();
        db.reconcile_address_history("acc2").unwrap();

        AppState::new(db, CredentialBackend::File)
    }

    async fn get(uri: &str) -> (StatusCode, serde_json::Value) {
        get_with(seeded_state(), uri).await
    }

    async fn get_with(state: AppState, uri: &str) -> (StatusCode, serde_json::Value) {
        let app = crate::dashboard_router(state);
        let res = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    fn emails(body: &serde_json::Value) -> Vec<String> {
        body["suggestions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["email"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn returns_ranked_suggestions_for_the_account() {
        let (status, body) = get("/api/accounts/acc1/address-suggestions?q=ada").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["account_id"], "acc1");
        assert_eq!(body["query"], "ada");
        assert_eq!(body["limit"], 8);
        assert_eq!(body["suggestions"].as_array().unwrap().len(), 1);
        assert_eq!(body["suggestions"][0]["email"], "ada@example.test");
        assert_eq!(body["suggestions"][0]["name"], "Ada Lovelace");
    }

    /// The INBOX snapshot holds a few dozen recent messages; the thread cache
    /// holds the correspondence. Both must reach the dropdown, including
    /// someone this account only ever wrote to.
    #[tokio::test]
    async fn suggestions_reach_the_deep_thread_cache() {
        let (status, body) = get("/api/accounts/acc1/address-suggestions?q=court").await;
        assert_eq!(status, StatusCode::OK);
        let mut found = emails(&body);
        found.sort();
        assert_eq!(
            found,
            vec![
                "adele@court.test",
                "clerk@court.test",
                "cocounsel@court.test",
                "courtroom-deputy@court.test",
            ]
        );
    }

    /// To, Cc, and Bcc all feed the one shared address history, and all three
    /// come back as the same address-only row. A Bcc recipient in particular
    /// must carry no marker saying they were blind-copied — the endpoint's
    /// contract is `{email, name}` and nothing else.
    #[tokio::test]
    async fn to_cc_and_bcc_all_reach_the_suggestion_endpoint() {
        let (status, body) = get("/api/accounts/acc1/address-suggestions?q=court.test").await;
        assert_eq!(status, StatusCode::OK);
        let found = emails(&body);
        for (header, address) in [
            ("To", "clerk@court.test"),
            ("Cc", "courtroom-deputy@court.test"),
            ("Bcc", "cocounsel@court.test"),
        ] {
            assert!(
                found.iter().any(|row| row == address),
                "the {header} recipient {address} never reached the address book: {found:?}"
            );
        }

        let (_, bcc) = get("/api/accounts/acc1/address-suggestions?q=cocounsel").await;
        let row = bcc["suggestions"][0].as_object().unwrap();
        assert_eq!(row.keys().collect::<Vec<_>>(), vec!["email", "name"]);
        assert_eq!(row["email"], "cocounsel@court.test");
        let serialized = serde_json::to_string(&bcc).unwrap();
        for leaked in ["bcc", "Bcc", "blind"] {
            assert!(
                !serialized.contains(leaked),
                "a Bcc recipient must look like any other suggestion: {serialized}"
            );
        }
    }

    #[tokio::test]
    async fn suggestions_carry_no_subjects_or_snippets() {
        let (status, body) = get("/api/accounts/acc1/address-suggestions?q=a").await;
        assert_eq!(status, StatusCode::OK);
        let serialized = serde_json::to_string(&body).unwrap();
        assert!(!serialized.contains("Victim impact statement"));
        assert!(!serialized.contains("the clerk has received"));
        assert!(!serialized.contains("snippet that must never reach"));
        assert!(!serialized.contains("message_count"));
        assert!(!serialized.contains("last_seen"));
        // The row shape is exactly {email, name} — nothing else rides along.
        let keys: Vec<&String> = body["suggestions"][0].as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["email", "name"]);
    }

    #[tokio::test]
    async fn account_scope_is_enforced() {
        let (_, body) = get("/api/accounts/acc1/address-suggestions?q=ad").await;
        let serialized = serde_json::to_string(&body["suggestions"]).unwrap();
        for leaked in ["adam@other.test", "adrian@other.test"] {
            assert!(
                !serialized.contains(leaked),
                "another account's history must not leak: {serialized}"
            );
        }

        let (status, body) = get("/api/accounts/acc2/address-suggestions?q=ad").await;
        assert_eq!(status, StatusCode::OK);
        let mut found = emails(&body);
        found.sort();
        assert_eq!(
            found,
            vec!["adam@other.test", "adrian@other.test"],
            "acc2 sees its own snapshot and thread history, and only its own"
        );
    }

    #[tokio::test]
    async fn account_may_be_addressed_by_email() {
        let (status, body) = get("/api/accounts/me%40example.test/address-suggestions?q=ada").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["account_id"], "acc1");
        assert_eq!(body["suggestions"][0]["email"], "ada@example.test");
    }

    #[tokio::test]
    async fn unknown_account_is_a_stable_404() {
        let (status, body) = get("/api/accounts/nope/address-suggestions?q=ada").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "account_not_found");
    }

    #[tokio::test]
    async fn blank_or_missing_query_is_rejected() {
        for uri in [
            "/api/accounts/acc1/address-suggestions",
            "/api/accounts/acc1/address-suggestions?q=",
            "/api/accounts/acc1/address-suggestions?q=%20%20",
        ] {
            let (status, body) = get(uri).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
            assert_eq!(body["code"], "address_query_required", "{uri}");
        }
    }

    #[tokio::test]
    async fn overlong_query_is_rejected() {
        let long = "a".repeat(MAX_QUERY_CHARS + 1);
        let (status, body) = get(&format!("/api/accounts/acc1/address-suggestions?q={long}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "address_query_too_long");
    }

    #[tokio::test]
    async fn limit_is_validated_against_the_ceiling() {
        let (status, body) = get("/api/accounts/acc1/address-suggestions?q=a&limit=0").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "address_limit_out_of_range");

        let (status, body) = get("/api/accounts/acc1/address-suggestions?q=a&limit=11").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "address_limit_out_of_range");

        let (status, body) = get("/api/accounts/acc1/address-suggestions?q=a&limit=abc").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "address_limit_invalid");
    }

    #[tokio::test]
    async fn limit_caps_the_returned_rows() {
        let (status, body) = get("/api/accounts/acc1/address-suggestions?q=e&limit=1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["limit"], 1);
        assert_eq!(body["suggestions"].as_array().unwrap().len(), 1);
    }

    /// A rescan that corrects a recipient must take the address it invented out
    /// of the dropdown, while a contact someone added by hand survives the same
    /// rebuild even with no history behind it at all.
    #[tokio::test]
    async fn a_corrected_header_stops_the_endpoint_offering_the_stale_address() {
        let state = seeded_state();
        {
            let db = state.db.lock().await;
            db.upsert_contact(&envelope_email_store::models::Contact {
                id: "manual-1".into(),
                account_id: "acc1".into(),
                email: "registry@court.test".into(),
                name: None,
                tags: "[]".into(),
                notes: None,
                message_count: 0,
                first_seen: Some("2026-01-01T00:00:00".into()),
                last_seen: Some("2026-01-01T00:00:00".into()),
                created_at: "2026-01-01T00:00:00".into(),
                updated_at: "2026-01-01T00:00:00".into(),
            })
            .unwrap();

            // The same message, rescanned with the recipient corrected.
            seed_thread_message_with_copies(
                &db,
                "acc1",
                2,
                "<t2@example.test>",
                "me@example.test",
                "clerk@court.test",
                Some("courtroom-deputy@court.test"),
                None,
                "2026-05-15T12:00:00Z",
                true,
            );
        }
        catch_up_account(&state, "acc1").await;

        let (status, body) =
            get_with(state, "/api/accounts/acc1/address-suggestions?q=court").await;
        assert_eq!(status, StatusCode::OK);
        let mut found = emails(&body);
        found.sort();
        assert_eq!(
            found,
            vec![
                "adele@court.test",
                "clerk@court.test",
                "courtroom-deputy@court.test",
                "registry@court.test",
            ],
            "the corrected-away Bcc is gone; the hand-added contact is not"
        );
    }

    /// The send edge, end to end at the surface that matters: send a draft and
    /// the next compose can suggest everyone it went to — no restart, no
    /// thread scan, no unified refresh, and no reconcile triggered by the GET.
    /// The endpoint stays the read-only query it claims to be: it is the send
    /// that wrote the history, and asking twice writes nothing.
    #[tokio::test]
    async fn a_just_sent_draft_reaches_the_endpoint_without_a_refresh() {
        let state = seeded_state();
        {
            let db = state.db.lock().await;
            let draft = db
                .create_draft(
                    "acc1",
                    "Nia Filer <nia@registry.test>",
                    Some("Filing"),
                    Some("body"),
                    None,
                    None,
                    Some("deputy@registry.test"),
                    Some("archive@registry.test"),
                    Some("human:dashboard"),
                )
                .unwrap();
            let lease = db
                .claim_draft_for_immediate_send(&draft.id, draft.revision)
                .unwrap()
                .expect("claim");
            db.mark_draft_sent(&draft.id, &lease, Some("<sent@example.test>"))
                .unwrap();
        }

        let (status, body) = get_with(
            state.clone(),
            "/api/accounts/acc1/address-suggestions?q=registry.test",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let found = emails(&body);
        for (header, address) in [
            ("To", "nia@registry.test"),
            ("Cc", "deputy@registry.test"),
            ("Bcc", "archive@registry.test"),
        ] {
            assert!(
                found.iter().any(|row| row == address),
                "the {header} recipient of a just-sent draft was not suggestible: {found:?}"
            );
        }
        // The display name written on the To line comes back with the address.
        let (_, named) = get_with(
            state.clone(),
            "/api/accounts/acc1/address-suggestions?q=nia@",
        )
        .await;
        assert_eq!(named["suggestions"][0]["email"], "nia@registry.test");
        assert_eq!(named["suggestions"][0]["name"], "Nia Filer");

        // The GET is a read. Two more of them change no stored signal.
        let before = signal(&state, "nia@registry.test").await;
        for _ in 0..2 {
            get_with(
                state.clone(),
                "/api/accounts/acc1/address-suggestions?q=nia",
            )
            .await;
        }
        assert_eq!(
            signal(&state, "nia@registry.test").await,
            before,
            "the suggestion endpoint must not write"
        );
    }

    /// The stored ranking signal for one of acc1's contacts.
    async fn signal(state: &AppState, email: &str) -> i64 {
        let db = state.db.lock().await;
        db.conn()
            .query_row(
                "SELECT MAX(message_count, history_count, history_sent_count) FROM contacts
                 WHERE account_id = 'acc1' AND lower(email) = ?1",
                [email],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[tokio::test]
    async fn a_query_that_matches_nothing_returns_an_empty_list() {
        let (status, body) = get("/api/accounts/acc1/address-suggestions?q=zzzznobody").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["suggestions"].as_array().unwrap().is_empty());
    }
}
