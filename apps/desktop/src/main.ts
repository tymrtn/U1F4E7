import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type Readiness =
  | "server_not_started"
  | "server_stopped"
  | "server_start_failed"
  | "dashboard_route_failure"
  | "static_asset_failure"
  | "api_route_failure"
  | "dashboard_ready";

type EffectiveReadiness = Readiness | "dashboard_loaded";

type BinaryAttempt = {
  source: string;
  path: string;
};

type HealthCheck = {
  path: string;
  url: string;
  ok: boolean;
  statusCode: number | null;
  error: string | null;
};

type StartupGuidance = {
  kind: string;
  title: string;
  explanation: string;
  remedy: string;
};

type DesktopStatus = {
  readiness: Readiness;
  running: boolean;
  healthy: boolean;
  dashboardUrl: string | null;
  binaryPath: string | null;
  binarySource: string | null;
  binaryAttempts: BinaryAttempt[];
  port: number | null;
  portSource: string | null;
  lastHealthCheckAt: string | null;
  healthChecks: HealthCheck[];
  stdoutTail: string;
  stderrTail: string;
  error: string | null;
  recommendedCommand: string | null;
  guidance: StartupGuidance | null;
  existingServerUrl: string | null;
};

type CopyDiagnosticsResult = {
  status: DesktopStatus;
  diagnostics: string;
  copied: boolean;
  error: string | null;
};

const appElement = document.querySelector<HTMLDivElement>("#app");

if (!appElement) {
  throw new Error("missing app root");
}

const app: HTMLDivElement = appElement;

let currentStatus: DesktopStatus | null = null;
let loading = true;
let dashboardVisible = false;
let dashboardLoadedAt: string | null = null;
let notice: string | null = null;
let copyFallback: string | null = null;
// Diagnostics are for when something is wrong. While the dashboard is healthy
// they stay closed and the mail gets the whole window; a failure forces them
// open regardless.
let diagnosticsOpen = false;
/// URL currently mounted in the iframe, so a restart onto a new port is noticed.
let mountedDashboardUrl: string | null = null;

void bootstrap();

async function bootstrap(): Promise<void> {
  render();

  await bindNativeEvents();
  await refreshStatus({ showLoading: true, autoOpenDashboard: true });

  window.setInterval(() => {
    void refreshStatus({
      showLoading: false,
      // Always re-open on a healthy poll. Passing dashboardVisible here meant a
      // single slow probe cleared the flag and every later poll then declined to
      // re-mount, stranding the user on the failure screen after the server had
      // already recovered.
      autoOpenDashboard: true,
    });
  }, 5000);
}

async function bindNativeEvents(): Promise<void> {
  await listen<DesktopStatus>("desktop-status", (event) => {
    setStatus(event.payload, { autoOpenDashboard: event.payload.healthy });
  });

  await listen<DesktopStatus>("desktop-open-dashboard", (event) => {
    setStatus(event.payload, { autoOpenDashboard: true });
  });

  await listen<CopyDiagnosticsResult>(
    "desktop-diagnostics-copied",
    (event) => {
      void handleCopyResult(event.payload);
    },
  );
}

async function refreshStatus(options: {
  showLoading: boolean;
  autoOpenDashboard: boolean;
}): Promise<void> {
  if (options.showLoading && !currentStatus) {
    loading = true;
    render();
  }

  try {
    const status = await invoke<DesktopStatus>("desktop_status");
    setStatus(status, { autoOpenDashboard: options.autoOpenDashboard });
  } catch (error) {
    renderFatalError(errorMessage(error));
  }
}

async function openDashboard(): Promise<void> {
  try {
    const status = await invoke<DesktopStatus>("open_dashboard");
    setStatus(status, { autoOpenDashboard: status.healthy });

    if (!status.healthy) {
      notice = "Dashboard is not ready; diagnostics are shown below.";
      render();
    }
  } catch (error) {
    renderFatalError(errorMessage(error));
  }
}

async function restartServer(): Promise<void> {
  loading = true;
  dashboardVisible = false;
  dashboardLoadedAt = null;
  copyFallback = null;
  notice = "Restarting the local Envelope server.";
  render();

  try {
    const status = await invoke<DesktopStatus>("restart_server");
    setStatus(status, { autoOpenDashboard: status.healthy });
  } catch (error) {
    renderFatalError(errorMessage(error));
  }
}

async function openInBrowser(): Promise<void> {
  try {
    const status = await invoke<DesktopStatus>("open_dashboard_in_browser");
    setStatus(status, { autoOpenDashboard: dashboardVisible });
    notice = status.dashboardUrl
      ? "Dashboard URL opened in the system browser."
      : "No dashboard URL is available yet.";
    render();
  } catch (error) {
    notice = errorMessage(error);
    render();
  }
}

async function copyDiagnostics(): Promise<void> {
  try {
    const result = await invoke<CopyDiagnosticsResult>("copy_diagnostics");
    await handleCopyResult(result);
  } catch (error) {
    renderFatalError(errorMessage(error));
  }
}

async function openExistingDashboard(): Promise<void> {
  try {
    setStatus(await invoke<DesktopStatus>("open_existing_dashboard"), {
      autoOpenDashboard: false,
    });
  } catch (error) {
    notice = errorMessage(error);
    render();
  }
}

async function quitApp(): Promise<void> {
  await invoke("quit_app");
}

/// Clear any transient banner. Called when a user action starts and when a
/// healthy status lands, so "Restarting the local Envelope server." cannot sit
/// above a working mailbox for the rest of the session.
function clearNotice(): void {
  notice = null;
}

function setStatus(
  status: DesktopStatus,
  options: { autoOpenDashboard: boolean },
): void {
  const wasDashboardVisible = dashboardVisible;
  const wasHealthy = currentStatus?.healthy ?? false;

  currentStatus = status;
  loading = false;

  if (!status.healthy) {
    dashboardVisible = false;
    dashboardLoadedAt = null;
  } else if (options.autoOpenDashboard && status.dashboardUrl) {
    dashboardVisible = true;
  }

  // Every notice is about the server not being ready or an action in flight.
  // Once the mail is up they are all stale.
  if (status.healthy && dashboardVisible) {
    clearNotice();
  }

  // Patching skips the DOM rebuild while nothing structural changed. A restart
  // hands back a fresh ephemeral port, and patchLiveStatus never touches the
  // iframe, so the URL has to break out of it or the window keeps showing a
  // dead port for the rest of the session.
  const sameDashboard = status.dashboardUrl === mountedDashboardUrl;

  if (wasDashboardVisible && dashboardVisible && wasHealthy && status.healthy && sameDashboard) {
    patchLiveStatus(status);
    return;
  }

  render();
}

async function handleCopyResult(result: CopyDiagnosticsResult): Promise<void> {
  currentStatus = result.status;
  loading = false;
  copyFallback = null;

  if (result.copied) {
    notice = "Diagnostics copied.";
    render();
    return;
  }

  try {
    await navigator.clipboard.writeText(result.diagnostics);
    notice = "Diagnostics copied.";
    render();
  } catch {
    copyFallback = result.diagnostics;
    notice = result.error
      ? `Clipboard unavailable: ${result.error}`
      : "Clipboard unavailable; diagnostics are selectable below.";
    render();
    document.querySelector<HTMLTextAreaElement>("#copyFallback")?.select();
  }
}

function render(): void {
  if (loading && !currentStatus) {
    app.innerHTML = `
      <section class="center">
        <h1>Envelope</h1>
        <p>Starting the local dashboard server.</p>
      </section>
    `;
    return;
  }

  if (!currentStatus) {
    renderFatalError("Desktop status is unavailable.");
    return;
  }

  const status = currentStatus;
  const readiness = effectiveReadiness(status);
  const canOpenDashboard = Boolean(status.healthy && status.dashboardUrl);
  const canOpenBrowser = Boolean(status.dashboardUrl);

  const mailReady = Boolean(dashboardVisible && status.healthy && status.dashboardUrl);
  // The header and control row belong to the broken case: they are how you
  // recover when there is no mail to read.
  const showChrome = !mailReady;
  const showDiagnostics = !mailReady || diagnosticsOpen;

  ensureSkeleton();

  setSlot(
    "#chromeRoot",
    (showChrome ? chromeMarkup(status, readiness, canOpenDashboard, canOpenBrowser) : "") +
      (notice ? `<p class="notice">${escapeHtml(notice)}</p>` : ""),
  );

  const workspace = document.querySelector<HTMLDivElement>("#workspaceRoot");
  if (workspace) {
    workspace.className = workspaceClass(mailReady);
  }

  const host = document.querySelector<HTMLElement>("#dashboardHost");
  if (host) {
    host.hidden = !mailReady;
    host.className = `dashboard-pane${diagnosticsOpen ? "" : " bare"}`;
    if (mailReady) {
      mountDashboard(status.dashboardUrl ?? "");
    }
  }

  const failure = document.querySelector<HTMLElement>("#failureSlot");
  if (failure) {
    failure.hidden = mailReady;
    failure.innerHTML = mailReady ? "" : renderFailureScreen(status);
  }

  const diagnostics = document.querySelector<HTMLElement>("#diagnosticsSlot");
  if (diagnostics) {
    diagnostics.hidden = !showDiagnostics;
    diagnostics.innerHTML = showDiagnostics ? renderDiagnostics(status) : "";
  }

  setSlot("#statusRoot", mailReady ? renderStatusBar(status, readiness) : "");

  bindButton("openDashboard", openDashboard);
  bindButton("openBrowser", openInBrowser);
  bindButton("copyDiagnostics", copyDiagnostics);
  bindButton("restartServer", restartServer);
  bindButton("refreshStatus", () =>
    refreshStatus({ showLoading: false, autoOpenDashboard: dashboardVisible }),
  );
  bindButton("quitApp", quitApp);
  bindButton("toggleDiagnostics", toggleDiagnostics);
  bindButton("retryServer", restartServer);
  bindButton("openExisting", openExistingDashboard);
  document.querySelector<HTMLTextAreaElement>("#copyFallback")?.select();
}

/// Build the persistent frame once. Everything render() rewrites lives inside a
/// slot; the dashboard host is never one of them, because rebuilding it
/// recreates the iframe and reloads the SPA, discarding scroll position, the
/// open message, and any half-written reply.
/// Header and control row. Shown only when the mail is not usable.
function chromeMarkup(
  status: DesktopStatus,
  readiness: EffectiveReadiness,
  canOpenDashboard: boolean,
  canOpenBrowser: boolean,
): string {
  return `
    <header class="topbar">
      <div>
        <h1>Envelope</h1>
        <p id="statusMessage">${escapeHtml(readinessMessage(readiness))}</p>
      </div>
      <div class="topbar-status">
        <span id="statusPill" class="pill ${statusClass(readiness)}">${escapeHtml(
          readinessLabel(readiness),
        )}</span>
        <span id="lastHealthCheck">Last check: ${escapeHtml(
          status.lastHealthCheckAt ?? "not run",
        )}</span>
      </div>
    </header>

    <nav class="actions" aria-label="Desktop controls">
      <button class="primary" id="openDashboard" ${canOpenDashboard ? "" : "disabled"}>Open Dashboard</button>
      <button id="openBrowser" ${canOpenBrowser ? "" : "disabled"}>Open in Browser</button>
      <button id="copyDiagnostics">Copy Diagnostics</button>
      <button id="restartServer">Restart Server</button>
      <button id="refreshStatus">Refresh</button>
      <button id="quitApp">Quit</button>
    </nav>
  `;
}

function ensureSkeleton(): void {
  if (document.querySelector("#shellRoot")) {
    return;
  }

  app.innerHTML = `
    <div class="desktop-shell" id="shellRoot">
      <div id="chromeRoot"></div>
      <div id="workspaceRoot" class="workspace">
        <section id="dashboardHost" class="dashboard-pane" aria-label="Envelope dashboard" hidden></section>
        <div id="failureSlot"></div>
        <div id="diagnosticsSlot"></div>
      </div>
      <div id="statusRoot"></div>
    </div>
  `;
}

function setSlot(selector: string, html: string): void {
  const el = document.querySelector<HTMLElement>(selector);
  if (el) {
    el.innerHTML = html;
  }
}

/// Mount the dashboard, or point it somewhere new. A restart picks a fresh
/// ephemeral port, so the URL has to be compared — otherwise the iframe keeps
/// showing a dead port forever.
function mountDashboard(url: string): void {
  const host = document.querySelector<HTMLElement>("#dashboardHost");
  if (!host || !url) {
    return;
  }

  const existing = host.querySelector<HTMLIFrameElement>("#dashboardFrame");

  if (existing) {
    if (mountedDashboardUrl !== url) {
      mountedDashboardUrl = url;
      dashboardLoadedAt = null;
      existing.src = url;
    }
    return;
  }

  const loadState = document.createElement("div");
  loadState.id = "dashboardLoadState";
  loadState.className = "load-state";
  loadState.textContent = "Loading dashboard webview.";

  const frame = document.createElement("iframe");
  frame.id = "dashboardFrame";
  frame.title = "Envelope dashboard";
  frame.addEventListener("load", () => {
    dashboardLoadedAt = new Date().toISOString();
    document.querySelector("#dashboardLoadState")?.remove();
    setSlot("#statusRoot", currentStatus ? renderStatusBar(currentStatus, "dashboard_loaded") : "");
    bindButton("toggleDiagnostics", toggleDiagnostics);
  });

  host.replaceChildren(loadState, frame);
  mountedDashboardUrl = url;
  frame.src = url;
}

function toggleDiagnostics(): void {
  diagnosticsOpen = !diagnosticsOpen;
  render();
}

function workspaceClass(mailReady: boolean): string {
  if (mailReady && !diagnosticsOpen) {
    return "workspace full-bleed";
  }

  return mailReady ? "workspace with-dashboard" : "workspace diagnostics-only";
}

/// One line at the foot of the window: enough to see the server is alive and
/// to reach the details, without spending the window on them.
function renderStatusBar(status: DesktopStatus, readiness: EffectiveReadiness): string {
  const port = status.port === null ? "" : ` · port ${status.port}`;

  return `
    <footer class="statusbar">
      <span class="dot ${statusClass(readiness)}"></span>
      <span>${escapeHtml(readinessLabel(readiness))}${escapeHtml(port)}</span>
      <button id="toggleDiagnostics" class="linkish">
        ${diagnosticsOpen ? "Hide details" : "Details"}
      </button>
    </footer>
  `;
}


function renderFailureScreen(status: DesktopStatus): string {
  const readiness = effectiveReadiness(status);
  const guidance = status.guidance;
  const raw = status.error ?? readinessMessage(readiness);

  return `
    <section class="failure-screen">
      <h2>${escapeHtml(guidance ? guidance.title : readinessLabel(readiness))}</h2>
      <p>${escapeHtml(guidance ? guidance.explanation : raw)}</p>
      ${guidance ? `<p class="remedy">${escapeHtml(guidance.remedy)}</p>` : ""}

      <div class="failure-actions">
        <button class="primary" id="retryServer">Start the server</button>
        ${
          status.existingServerUrl
            ? `<button id="openExisting">Open the Envelope already running</button>`
            : ""
        }
      </div>

      ${
        guidance
          ? `<details class="raw-detail">
               <summary>What the server reported</summary>
               <p>${escapeHtml(raw)}</p>
               ${
                 status.recommendedCommand
                   ? `<p class="command"><code>${escapeHtml(status.recommendedCommand)}</code></p>`
                   : ""
               }
             </details>`
          : status.recommendedCommand
            ? `<p class="command"><code>${escapeHtml(status.recommendedCommand)}</code></p>`
            : ""
      }
    </section>
  `;
}

function renderDiagnostics(status: DesktopStatus): string {
  return `
    <aside class="diagnostics" aria-label="Envelope diagnostics">
      <section class="panel">
        <h2>Server</h2>
        <div class="grid">
          <div class="label">Status</div>
          <div class="${status.healthy ? "ok" : "fail"}">${escapeHtml(
            readinessLabel(effectiveReadiness(status)),
          )}</div>
          <div class="label">Running</div>
          <div class="${status.running ? "ok" : "fail"}">${status.running ? "yes" : "no"}</div>
          <div class="label">Healthy</div>
          <div class="${status.healthy ? "ok" : "fail"}">${status.healthy ? "yes" : "no"}</div>
          <div class="label">Dashboard URL</div>
          <div><code>${escapeHtml(status.dashboardUrl ?? "(not assigned)")}</code></div>
          <div class="label">Binary</div>
          <div><code>${escapeHtml(status.binaryPath ?? "(not resolved)")}</code></div>
          <div class="label">Binary source</div>
          <div>${escapeHtml(status.binarySource ?? "(none)")}</div>
          <div class="label">Port</div>
          <div>${status.port ?? "(not assigned)"} ${status.portSource ? `(${escapeHtml(status.portSource)})` : ""}</div>
          <div class="label">Last health check</div>
          <div id="serverLastHealthCheck">${escapeHtml(status.lastHealthCheckAt ?? "not run")}</div>
          <div class="label">Command</div>
          <div><code>${escapeHtml(status.recommendedCommand ?? "(not available)")}</code></div>
        </div>
      </section>

      ${
        status.binaryAttempts.length > 0
          ? `<section class="panel">
              <h2>Binary Resolution</h2>
              <div class="attempts">${renderBinaryAttempts(status.binaryAttempts)}</div>
            </section>`
          : ""
      }

      <section class="panel">
        <h2>Health Checks</h2>
        <table>
          <thead>
            <tr>
              <th>Check</th>
              <th>Status</th>
              <th>URL</th>
              <th>Error</th>
            </tr>
          </thead>
          <tbody>
            ${renderHealthRows(status.healthChecks)}
          </tbody>
        </table>
      </section>

      <section class="panel">
        <h2>stdout tail</h2>
        <pre>${escapeHtml(status.stdoutTail || "(empty)")}</pre>
      </section>

      <section class="panel">
        <h2>stderr tail</h2>
        <pre>${escapeHtml(status.stderrTail || "(empty)")}</pre>
      </section>

      ${
        copyFallback
          ? `<section class="panel">
              <h2>Copy Fallback</h2>
              <textarea id="copyFallback" readonly>${escapeHtml(copyFallback)}</textarea>
            </section>`
          : ""
      }
    </aside>
  `;
}

function renderBinaryAttempts(attempts: BinaryAttempt[]): string {
  return attempts
    .map(
      (attempt) => `
        <div class="attempt">
          <span>${escapeHtml(attempt.source)}</span>
          <code>${escapeHtml(attempt.path || "(empty)")}</code>
        </div>
      `,
    )
    .join("");
}

function renderHealthRows(checks: HealthCheck[]): string {
  if (checks.length === 0) {
    return `
      <tr>
        <td colspan="4">No health checks have run.</td>
      </tr>
    `;
  }

  return checks
    .map((check) => {
      const status = check.statusCode
        ? `${check.statusCode}`
        : check.ok
          ? "ok"
          : "failed";

      return `
        <tr>
          <td><code>${escapeHtml(check.path)}</code></td>
          <td class="${check.ok ? "ok" : "fail"}">${escapeHtml(status)}</td>
          <td><code>${escapeHtml(check.url)}</code></td>
          <td>${escapeHtml(check.error ?? "")}</td>
        </tr>
      `;
    })
    .join("");
}


function patchLiveStatus(status: DesktopStatus): void {
  const readiness = effectiveReadiness(status);
  const pill = document.querySelector("#statusPill");

  if (pill) {
    pill.className = `pill ${statusClass(readiness)}`;
    pill.textContent = readinessLabel(readiness);
  }

  setText("#statusMessage", readinessMessage(readiness));
  setText("#lastHealthCheck", `Last check: ${status.lastHealthCheckAt ?? "not run"}`);
  setText("#serverLastHealthCheck", status.lastHealthCheckAt ?? "not run");
}

function effectiveReadiness(status: DesktopStatus): EffectiveReadiness {
  if (dashboardVisible && status.healthy && dashboardLoadedAt) {
    return "dashboard_loaded";
  }

  return status.readiness;
}

function readinessLabel(readiness: EffectiveReadiness): string {
  switch (readiness) {
    case "server_not_started":
      return "Server not started";
    case "server_stopped":
      return "Server stopped";
    case "server_start_failed":
      return "Server start failed";
    case "dashboard_route_failure":
      return "Dashboard route failure";
    case "static_asset_failure":
      return "Static asset failure";
    case "api_route_failure":
      return "API route failure";
    case "dashboard_ready":
      return "Dashboard ready";
    case "dashboard_loaded":
      return "Dashboard loaded";
  }
}

function readinessMessage(readiness: EffectiveReadiness): string {
  switch (readiness) {
    case "server_not_started":
      return "The local Envelope server is not running or the binary could not be resolved.";
    case "server_stopped":
      return "You stopped this window's Envelope server. Start it again to see your mail here. Mail syncs and sends only while an Envelope server is running.";
    case "server_start_failed":
      return "The Envelope server process did not start cleanly.";
    case "dashboard_route_failure":
      return "The dashboard route failed its localhost health check.";
    case "static_asset_failure":
      return "The dashboard static bundle failed its localhost health check.";
    case "api_route_failure":
      return "The dashboard API probe failed its localhost health check.";
    case "dashboard_ready":
      return "Health checks passed; the dashboard is ready to load.";
    case "dashboard_loaded":
      return "The dashboard webview loaded after passing health checks.";
  }
}

function statusClass(readiness: EffectiveReadiness): string {
  if (readiness === "dashboard_loaded" || readiness === "dashboard_ready") {
    return "ok-bg";
  }

  if (readiness === "server_not_started" || readiness === "server_start_failed") {
    return "fail-bg";
  }

  return "warn";
}

function bindButton(id: string, handler: () => void | Promise<void>): void {
  document.getElementById(id)?.addEventListener("click", () => {
    void handler();
  });
}

function setText(selector: string, value: string): void {
  const element = document.querySelector(selector);

  if (element) {
    element.textContent = value;
  }
}

function renderFatalError(message: string): void {
  // Reset the mounted state: the skeleton and iframe are about to be discarded,
  // so a later healthy status must take the full-render path.
  dashboardVisible = false;
  dashboardLoadedAt = null;
  mountedDashboardUrl = null;

  loading = false;
  app.innerHTML = `
    <section class="center">
      <h1>Envelope diagnostics</h1>
      <p class="fail">${escapeHtml(message)}</p>
      <div class="actions">
        <button class="primary" id="fatalRetry">Retry</button>
      </div>
    </section>
  `;

  bindButton("fatalRetry", () =>
    refreshStatus({ showLoading: true, autoOpenDashboard: true }),
  );
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function escapeHtml(value: string): string {
  return value.replace(/[&<>"']/g, (char) => {
    switch (char) {
      case "&":
        return "&amp;";
      case "<":
        return "&lt;";
      case ">":
        return "&gt;";
      case '"':
        return "&quot;";
      case "'":
        return "&#039;";
      default:
        return char;
    }
  });
}
