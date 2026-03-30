// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::{HeaderValue, Method};
use axum::response::{Html, IntoResponse, Json};
use axum::routing::{delete, get};
use envelope_email_store::Database;
use tokio::sync::Mutex;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::info;

type AppState = Arc<Mutex<Database>>;

/// Start the localhost dashboard on the given port.
pub async fn serve(port: u16) -> anyhow::Result<()> {
    let db = Database::open_default().map_err(|e| anyhow::anyhow!("{e}"))?;
    let state: AppState = Arc::new(Mutex::new(db));

    let app = Router::new()
        .route("/", get(index_page))
        .route("/api/accounts", get(list_accounts))
        .route("/api/accounts/{id}", delete(delete_account))
        .route("/api/stats", get(stats))
        .layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::predicate(|origin: &HeaderValue, _| {
                    let s = origin.to_str().unwrap_or("");
                    s.starts_with("http://localhost:") || s.starts_with("http://127.0.0.1:")
                }))
                .allow_methods([Method::GET, Method::POST, Method::DELETE])
                .allow_headers([
                    axum::http::header::CONTENT_TYPE,
                    axum::http::header::AUTHORIZATION,
                    axum::http::header::ACCEPT,
                ]),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind port {port}: {e}"))?;

    info!("dashboard listening on http://localhost:{port}");
    println!("Dashboard running at http://localhost:{port}");

    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("server error: {e}"))
}

async fn list_accounts(State(db): State<AppState>) -> impl IntoResponse {
    let db = db.lock().await;
    match db.list_accounts() {
        Ok(accounts) => Json(serde_json::json!({ "accounts": accounts })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("Error: {e}")).into_response(),
    }
}

async fn delete_account(
    State(db): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let db = db.lock().await;
    match db.delete_account(&id) {
        Ok(true) => Json(serde_json::json!({ "deleted": id })).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "Account not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("Error: {e}")).into_response(),
    }
}

async fn stats(State(db): State<AppState>) -> impl IntoResponse {
    let db = db.lock().await;
    let account_count = db.list_accounts().map(|a| a.len()).unwrap_or(0);
    Json(serde_json::json!({
        "accounts": account_count,
    }))
}

async fn index_page() -> Html<&'static str> {
    Html(INDEX_HTML)
}

const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Envelope Email</title>
<style>
  :root { --bg: #0a0a0a; --fg: #e0e0e0; --accent: #6366f1; --card: #141414; --border: #2a2a2a; --muted: #888; }
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif; background: var(--bg); color: var(--fg); min-height: 100vh; }
  .container { max-width: 720px; margin: 0 auto; padding: 2rem 1.5rem; }
  h1 { font-size: 1.5rem; font-weight: 600; margin-bottom: 0.25rem; }
  .subtitle { color: var(--muted); margin-bottom: 2rem; font-size: 0.9rem; }
  .stats { display: flex; gap: 1rem; margin-bottom: 2rem; }
  .stat-card { background: var(--card); border: 1px solid var(--border); border-radius: 8px; padding: 1rem 1.25rem; flex: 1; }
  .stat-card .label { font-size: 0.75rem; text-transform: uppercase; color: var(--muted); letter-spacing: 0.05em; }
  .stat-card .value { font-size: 1.75rem; font-weight: 600; margin-top: 0.25rem; }
  h2 { font-size: 1.1rem; font-weight: 500; margin-bottom: 1rem; }
  .account-list { list-style: none; }
  .account-item { background: var(--card); border: 1px solid var(--border); border-radius: 8px; padding: 1rem 1.25rem; margin-bottom: 0.75rem; display: flex; justify-content: space-between; align-items: center; }
  .account-info .name { font-weight: 500; }
  .account-info .email { color: var(--muted); font-size: 0.85rem; margin-top: 0.15rem; }
  .account-info .server { color: var(--muted); font-size: 0.75rem; margin-top: 0.25rem; }
  .btn-delete { background: none; border: 1px solid #ef4444; color: #ef4444; padding: 0.35rem 0.75rem; border-radius: 6px; cursor: pointer; font-size: 0.8rem; transition: all 0.15s; }
  .btn-delete:hover { background: #ef4444; color: #fff; }
  .empty { color: var(--muted); text-align: center; padding: 3rem 0; }
  .footer { margin-top: 3rem; text-align: center; color: var(--muted); font-size: 0.75rem; }
</style>
</head>
<body>
<div class="container">
  <h1>Envelope Email</h1>
  <p class="subtitle">Account Management</p>

  <div class="stats">
    <div class="stat-card">
      <div class="label">Accounts</div>
      <div class="value" id="account-count">-</div>
    </div>
  </div>

  <h2>Configured Accounts</h2>
  <ul class="account-list" id="accounts"></ul>

  <div class="footer">
    <p>Manage accounts via CLI: <code>envelope-email accounts add --email you@example.com</code></p>
  </div>
</div>
<script>
function createAccountItem(a) {
  const li = document.createElement('li');
  li.className = 'account-item';
  li.dataset.id = a.id;

  const info = document.createElement('div');
  info.className = 'account-info';

  const name = document.createElement('div');
  name.className = 'name';
  name.textContent = a.name;
  info.appendChild(name);

  const email = document.createElement('div');
  email.className = 'email';
  email.textContent = a.username;
  info.appendChild(email);

  const server = document.createElement('div');
  server.className = 'server';
  server.textContent = 'IMAP: ' + a.imap_host + ':' + a.imap_port + ' \u00B7 SMTP: ' + a.smtp_host + ':' + a.smtp_port;
  info.appendChild(server);

  const btn = document.createElement('button');
  btn.className = 'btn-delete';
  btn.textContent = 'Remove';
  btn.addEventListener('click', function() { removeAccount(a.id, a.username); });

  li.appendChild(info);
  li.appendChild(btn);
  return li;
}

async function load() {
  try {
    const [accountsRes, statsRes] = await Promise.all([
      fetch('/api/accounts'),
      fetch('/api/stats')
    ]);
    const { accounts } = await accountsRes.json();
    const stats = await statsRes.json();

    document.getElementById('account-count').textContent = stats.accounts;

    const list = document.getElementById('accounts');
    list.replaceChildren();

    if (!accounts.length) {
      const empty = document.createElement('li');
      empty.className = 'empty';
      empty.textContent = 'No accounts configured yet.';
      list.appendChild(empty);
      return;
    }

    accounts.forEach(function(a) {
      list.appendChild(createAccountItem(a));
    });
  } catch (e) {
    const list = document.getElementById('accounts');
    list.replaceChildren();
    const err = document.createElement('li');
    err.className = 'empty';
    err.textContent = 'Failed to load accounts.';
    list.appendChild(err);
  }
}

async function removeAccount(id, email) {
  if (!confirm('Remove account ' + email + '?')) return;
  await fetch('/api/accounts/' + encodeURIComponent(id), { method: 'DELETE' });
  load();
}

load();
</script>
</body>
</html>"#;
