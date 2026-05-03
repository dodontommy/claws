// claws phone PWA — vanilla single-file UI with Web Push + permission UI.
// (The reset escape hatch lives inline in index.html so it works even
// when this file is held hostage by a stale service worker cache.)

const TOKEN_KEY = "claws.deviceToken";
const PUSH_FLAG_KEY = "claws.pushSubscribed";
const $app = document.getElementById("app");

const state = {
  token: localStorage.getItem(TOKEN_KEY),
  view: "list", // "list" | "detail" | "pair"
  selectedId: null,
  sessions: [],
  ptyNextSeq: 0,
  ws: null,
  wsAlive: false,
  pairError: null,
  // Per-session permission-request payloads. Cleared when status leaves
  // awaiting_permission. Keyed by session_id.
  permissionRequests: {},
};

// Persistent xterm.js Terminal. Lives outside the rerender cycle because
// xterm manages its own DOM and breaks if its node is replaced. We attach
// it into the detail view's #pty-mount on entry and detach it on exit.
let term = null;
let fitAddon = null;
let termHost = null;
let termSession = null; // session id whose bytes the terminal currently shows

// ---- Pairing ----------------------------------------------------------------

// Pairing code is delivered in the URL fragment so it never hits any server log:
//   https://host/#code=<one-shot>
function readPairCodeFromHash() {
  const h = window.location.hash || "";
  const m = h.match(/code=([A-Za-z0-9_-]+)/);
  return m ? m[1] : null;
}

async function redeemPairCode(code) {
  const r = await fetch("/api/pair", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ code }),
  });
  if (!r.ok) {
    const text = await r.text();
    throw new Error(text || `pair failed: ${r.status}`);
  }
  const { device_token } = await r.json();
  if (!device_token) throw new Error("no device_token in response");
  localStorage.setItem(TOKEN_KEY, device_token);
  state.token = device_token;
  history.replaceState({}, "", "/");
}

async function tryAutoPair() {
  const code = readPairCodeFromHash();
  if (!code) return false;
  try {
    await redeemPairCode(code);
    return true;
  } catch (e) {
    state.pairError = String(e.message || e);
    return false;
  }
}

// ---- WebSocket --------------------------------------------------------------

function wsUrl() {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${location.host}/api/ws?token=${encodeURIComponent(state.token)}`;
}

function connectWS() {
  if (state.ws) state.ws.close();
  const ws = new WebSocket(wsUrl());
  state.ws = ws;
  ws.onopen = () => {
    state.wsAlive = true;
    render();
    // Subscribe to whatever the user is currently looking at.
    if (state.selectedId) sendWS({ kind: "subscribe", session_id: state.selectedId, since: state.ptyNextSeq });
  };
  ws.onclose = () => {
    state.wsAlive = false;
    render();
    setTimeout(connectWS, 1000);
  };
  ws.onerror = () => { try { ws.close(); } catch {} };
  ws.onmessage = (ev) => {
    let msg;
    try { msg = JSON.parse(ev.data); } catch { return; }
    handleServerEvent(msg);
  };
}

function sendWS(obj) {
  if (state.ws && state.ws.readyState === 1) {
    state.ws.send(JSON.stringify(obj));
  }
}

function handleServerEvent(msg) {
  switch (msg.kind) {
    case "snapshot":
      state.sessions = msg.sessions || [];
      // Drop permission-request entries for sessions that have moved on.
      for (const sid of Object.keys(state.permissionRequests)) {
        const s = state.sessions.find((x) => x.id === sid);
        if (!s || s.status !== "awaiting_permission") {
          delete state.permissionRequests[sid];
        }
      }
      sortSessions();
      render();
      break;
    case "session.update": {
      const s = msg.session;
      const idx = state.sessions.findIndex((x) => x.id === s.id);
      if (idx >= 0) state.sessions[idx] = s;
      else state.sessions.push(s);
      if (s.status !== "awaiting_permission") delete state.permissionRequests[s.id];
      sortSessions();
      render();
      break;
    }
    case "session.removed":
      state.sessions = state.sessions.filter((s) => s.id !== msg.session_id);
      delete state.permissionRequests[msg.session_id];
      if (state.selectedId === msg.session_id) {
        state.selectedId = null;
        state.view = "list";
      }
      render();
      break;
    case "session.output":
      if (msg.session_id === state.selectedId) {
        writePtyChunkToTerm(msg.data_b64);
        state.ptyNextSeq = msg.next_seq;
        // No render() — xterm has updated itself, the chrome is unchanged.
      }
      break;
    case "session.permission_request":
      state.permissionRequests[msg.session_id] = {
        name: msg.name,
        tool_name: msg.tool_name || null,
        at: Date.now(),
      };
      maybeNotify({
        session_id: msg.session_id,
        title: `${msg.name || "session"} — needs you`,
        body: msg.tool_name ? `wants to run ${msg.tool_name}` : "needs your input",
      });
      render();
      break;
    case "session.exited":
      // Snapshot will reflect the new status; we just clear any open prompt.
      delete state.permissionRequests[msg.session_id];
      render();
      break;
  }
}

function sortSessions() {
  const order = { awaiting_permission: 0, resume_failed: 1, streaming: 2, idle: 3, spawning: 4, exited: 5 };
  const isActive = (s) => s.status === "awaiting_permission" || s.status === "streaming" || s.status === "spawning";
  state.sessions.sort((a, b) => {
    const pa = order[a.status] ?? 99;
    const pb = order[b.status] ?? 99;
    if (pa !== pb) return pa - pb;
    if (isActive(a)) return a.id_seq - b.id_seq;
    return b.last_activity_ms - a.last_activity_ms;
  });
}

// ---- xterm integration ------------------------------------------------------

let termOpened = false;

function ensureTerminal() {
  if (term) return term;
  if (typeof Terminal === "undefined") {
    console.warn("xterm.js not loaded; terminal rendering disabled");
    return null;
  }
  termHost = document.getElementById("term-host");
  term = new Terminal({
    convertEol: false,
    cursorBlink: true,
    cursorStyle: "block",
    disableStdin: false,
    scrollback: 5000,
    fontFamily: 'ui-monospace, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace',
    fontSize: 12,
    lineHeight: 1.2,
    theme: {
      background: "#000000",
      foreground: "#cdd6f4",
      cursor: "#89b4fa",
      cursorAccent: "#000000",
      selectionBackground: "#45475a",
      black: "#181825",
      red: "#f38ba8",
      green: "#a6e3a1",
      yellow: "#f9e2af",
      blue: "#89b4fa",
      magenta: "#cba6f7",
      cyan: "#94e2d5",
      white: "#cdd6f4",
      brightBlack: "#45475a",
      brightRed: "#f38ba8",
      brightGreen: "#a6e3a1",
      brightYellow: "#f9e2af",
      brightBlue: "#89b4fa",
      brightMagenta: "#cba6f7",
      brightCyan: "#94e2d5",
      brightWhite: "#a6adc8",
    },
  });
  if (typeof FitAddon !== "undefined") {
    fitAddon = new FitAddon.FitAddon();
    term.loadAddon(fitAddon);
  }
  // term.open() is deferred to attachTerminalTo() — calling it while
  // termHost is `hidden` (display: none) leaves xterm with zero-sized
  // internal canvases and it never renders even after we unhide.
  // User input flows directly: xterm captures keystrokes, we forward
  // each chunk to the daemon's session.send_input.
  term.onData((data) => {
    if (state.selectedId) sendKeys(data);
  });
  return term;
}

function fitTerm() {
  if (!fitAddon || !termHost || termHost.hidden) return;
  // Deliberately do NOT send a resize to the daemon. The PTY is shared with
  // any dev-box TUI viewers; resizing it from the phone causes Claude to
  // redraw at the phone's geometry, which makes the dev TUI visibly thrash.
  // We let xterm display whatever size the daemon's PTY currently is —
  // padding/wrap on the phone is a fine trade-off for a stable shared view.
  try { fitAddon.fit(); } catch {}
}

// Persistent streaming UTF-8 decoder. Critical because PTY chunks can split
// a multi-byte sequence across calls — non-streaming decode would emit a
// U+FFFD replacement at the boundary and we'd lose the character.
let utf8Decoder = null;

function writePtyChunkToTerm(b64) {
  const t = ensureTerminal();
  if (!t) return;
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  // Decode UTF-8 explicitly here and write as a string. xterm.js's
  // Uint8Array path was producing double-encoded output (UTF-8 bytes
  // rendered as if Latin-1 then re-encoded), turning ✻ into âœ» on
  // screen. Decoding here and passing a string sidesteps that path.
  if (!utf8Decoder) utf8Decoder = new TextDecoder("utf-8");
  const text = utf8Decoder.decode(bytes, { stream: true });
  t.write(text);
}

function resetUtf8Decoder() {
  // Drop any pending partial sequence when switching sessions.
  utf8Decoder = null;
}

function attachTerminalTo(mount) {
  const t = ensureTerminal();
  if (!t) return;
  termHost.hidden = false;
  if (termHost.parentElement !== mount) mount.appendChild(termHost);
  // Open AFTER reparenting + unhiding so xterm measures real dimensions.
  // Once opened, subsequent reparenting works because the internal DOM
  // moves with termHost as a unit.
  if (!termOpened) {
    t.open(termHost);
    termOpened = true;
    // No window/visualViewport resize listener at all. iOS Safari fires
    // `resize` (not just visualViewport.resize) when the keyboard shows,
    // so any auto-fit ends up triggering an xterm relayout that steals
    // focus from the active input and dismisses the keyboard. Initial
    // fit happens on first attach; that's enough for our purposes.
    // Rotation will need a manual page reload to re-fit, which is a
    // worthwhile trade for typing actually working.
  }
  // Defer fit until layout settles.
  requestAnimationFrame(() => {
    fitTerm();
    t.scrollToBottom();
  });
}

function detachTerminal() {
  if (!termHost) return;
  termHost.hidden = true;
  // Park back on body so it's not destroyed by an #app rerender.
  if (termHost.parentElement && termHost.parentElement.id !== "term-host-park") {
    document.body.appendChild(termHost);
  }
}

function resetTerminalForSession(id) {
  const t = ensureTerminal();
  if (!t) return;
  if (termSession !== id) {
    t.reset();
    termSession = id;
    resetUtf8Decoder();
  }
}

// ---- Notifications ----------------------------------------------------------

function maybeNotify(msg) {
  // Foreground notifications only — Web Push (handled by the service worker)
  // covers the background case. Tag by session so a burst on one session
  // collapses into a single notification.
  if (!("Notification" in window)) return;
  if (Notification.permission !== "granted") return;
  const title = msg.title || "claws";
  const body = msg.body || "";
  try {
    new Notification(title, { body, tag: msg.session_id ? `claws:${msg.session_id}` : "claws" });
  } catch {}
}

async function ensureNotificationPermission() {
  if (!("Notification" in window)) return;
  if (Notification.permission === "default") {
    try { await Notification.requestPermission(); } catch {}
  }
}

/// Subscribe this device for Web Push if it isn't already. Idempotent on
/// repeated runs because the browser dedupes by application server key.
async function ensurePushSubscription() {
  if (!("serviceWorker" in navigator) || !("PushManager" in window)) return;
  if (Notification.permission !== "granted") return;
  const reg = await navigator.serviceWorker.ready;
  let sub = await reg.pushManager.getSubscription();
  if (!sub) {
    let key;
    try {
      const r = await fetch("/api/push/vapid_key", { headers: authHeaders() });
      if (!r.ok) return;
      const { application_server_key } = await r.json();
      key = urlB64ToUint8Array(application_server_key);
    } catch (e) {
      console.warn("vapid fetch failed", e);
      return;
    }
    try {
      sub = await reg.pushManager.subscribe({
        userVisibleOnly: true,
        applicationServerKey: key,
      });
    } catch (e) {
      console.warn("push subscribe failed", e);
      return;
    }
  }
  // Send the subscription to the daemon (it dedupes by endpoint).
  const json = sub.toJSON();
  try {
    await fetch("/api/push/subscribe", {
      method: "POST",
      headers: { "Content-Type": "application/json", ...authHeaders() },
      body: JSON.stringify({
        endpoint: json.endpoint,
        p256dh: json.keys && json.keys.p256dh,
        auth: json.keys && json.keys.auth,
      }),
    });
    localStorage.setItem(PUSH_FLAG_KEY, "1");
  } catch (e) {
    console.warn("subscribe POST failed", e);
  }
}

function authHeaders() {
  return state.token ? { Authorization: `Bearer ${state.token}` } : {};
}

function urlB64ToUint8Array(b64) {
  const padding = "=".repeat((4 - (b64.length % 4)) % 4);
  const base64 = (b64 + padding).replace(/-/g, "+").replace(/_/g, "/");
  const raw = atob(base64);
  const out = new Uint8Array(raw.length);
  for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i);
  return out;
}

// ---- Views ------------------------------------------------------------------

function render() {
  if (!state.token) { detachTerminal(); renderPair(); return; }
  if (state.view === "detail" && state.selectedId) {
    renderDetail();
  } else {
    detachTerminal();
    renderList();
  }
}

function el(html) {
  const t = document.createElement("template");
  t.innerHTML = html.trim();
  return t.content.firstElementChild;
}

function escapeHtml(s) {
  return String(s ?? "").replace(/[&<>"]/g, (c) => ({"&":"&amp;","<":"&lt;",">":"&gt;","\"":"&quot;"}[c]));
}

function renderPair() {
  const code = readPairCodeFromHash() || "";
  $app.innerHTML = "";
  const root = el(`
    <div class="app">
      <header><div class="title">claws</div></header>
      <div class="view">
        <div class="pair">
          <h1>Pair this device</h1>
          <p>Open the QR or pairing link from <code>claws phone pair</code> on your dev box.</p>
          <input id="code" placeholder="Pair code" value="${escapeHtml(code)}" autocapitalize="none" autocorrect="off" spellcheck="false" />
          <button id="go">Pair</button>
          ${state.pairError ? `<div class="err">${escapeHtml(state.pairError)}</div>` : ""}
        </div>
      </div>
    </div>
  `);
  $app.replaceWith(root);
  root.id = "app";
  root.querySelector("#go").addEventListener("click", async () => {
    const code = root.querySelector("#code").value.trim();
    if (!code) return;
    try {
      await redeemPairCode(code);
      state.pairError = null;
      connectWS();
      ensureNotificationPermission();
      render();
    } catch (e) {
      state.pairError = String(e.message || e);
      render();
    }
  });
}

function renderList() {
  const conn = state.wsAlive ? "live" : "gone";
  const connText = state.wsAlive ? "live" : "offline";
  const rows = state.sessions.map((s) => `
    <div class="row" data-id="${escapeHtml(s.id)}">
      <div class="dot ${escapeHtml(s.status)}"></div>
      <div class="meta">
        <div class="name">${escapeHtml(displayName(s))}${s.dangerous ? '<span class="danger">!</span>' : ""}</div>
        <div class="sub">${escapeHtml(s.cwd)}</div>
      </div>
      <div class="state">${escapeHtml(stateLabel(s))}</div>
    </div>
  `).join("");
  const empty = state.sessions.length ? "" : `<div class="empty">No sessions yet. Tap + to spawn one.</div>`;
  const root = el(`
    <div class="app" id="app">
      <header>
        <div class="title">claws</div>
        <button class="hdr-btn" id="new-btn" aria-label="New session">+</button>
        <div class="conn ${conn}">${connText}</div>
      </header>
      <div class="view">
        <div class="list">${rows}${empty}</div>
      </div>
    </div>
  `);
  document.getElementById("app").replaceWith(root);
  root.querySelectorAll(".row").forEach((r) => {
    r.addEventListener("click", () => openSession(r.dataset.id));
  });
  root.querySelector("#new-btn").addEventListener("click", () => openSpawnSheet());
  if (state.spawnOpen) renderSpawnSheet(root);
}

function openSpawnSheet() {
  // Default cwd: the most recent session's cwd, or empty.
  const recent = state.sessions[0]?.cwd || "";
  state.spawnOpen = true;
  state.spawnForm = {
    cwd: recent,
    model: "default",
    flags: "",
    submitting: false,
    error: null,
  };
  render();
}

function closeSpawnSheet() {
  state.spawnOpen = false;
  state.spawnForm = null;
  render();
}

function renderSpawnSheet(root) {
  const f = state.spawnForm;
  if (!f) return;
  const sheet = el(`
    <div class="sheet-backdrop">
      <div class="sheet">
        <div class="sheet-title">new session</div>
        <label class="sheet-label">directory</label>
        <input class="sheet-input" id="sf-cwd" autocapitalize="none" autocorrect="off" spellcheck="false" value="${escapeHtml(f.cwd)}" />
        <label class="sheet-label">model</label>
        <div class="model-row" id="sf-model">
          ${["default","opus","sonnet","haiku"].map((m) => `<button type="button" class="model-pill ${m === f.model ? "selected" : ""}" data-m="${m}">${m}</button>`).join("")}
        </div>
        <label class="sheet-label">flags <span class="sheet-hint">(optional)</span></label>
        <input class="sheet-input" id="sf-flags" autocapitalize="none" autocorrect="off" spellcheck="false" placeholder="--effort xhigh · --add-dir <path>" value="${escapeHtml(f.flags)}" />
        ${f.error ? `<div class="sheet-err">${escapeHtml(f.error)}</div>` : ""}
        <div class="sheet-actions">
          <button class="sheet-cancel" id="sf-cancel">Cancel</button>
          <button class="sheet-submit" id="sf-submit" ${f.submitting ? "disabled" : ""}>${f.submitting ? "Spawning…" : "Spawn"}</button>
        </div>
      </div>
    </div>
  `);
  root.appendChild(sheet);
  sheet.addEventListener("click", (e) => { if (e.target === sheet) closeSpawnSheet(); });
  sheet.querySelector("#sf-cancel").addEventListener("click", closeSpawnSheet);
  sheet.querySelectorAll(".model-pill").forEach((p) => {
    p.addEventListener("click", () => { f.model = p.dataset.m; render(); });
  });
  sheet.querySelector("#sf-cwd").addEventListener("input", (e) => { f.cwd = e.target.value; });
  sheet.querySelector("#sf-flags").addEventListener("input", (e) => { f.flags = e.target.value; });
  sheet.querySelector("#sf-submit").addEventListener("click", () => submitSpawn());
}

async function submitSpawn() {
  const f = state.spawnForm;
  if (!f || f.submitting) return;
  if (!f.cwd.trim()) { f.error = "directory required"; render(); return; }
  f.submitting = true;
  f.error = null;
  render();
  // Naive flag split — same shell-words behavior would need a JS parser.
  // A space-split is sufficient for the simple cases the user is likely
  // to type on a phone; complex quoted args belong on the desktop.
  const extra_args = f.flags.split(/\s+/).filter(Boolean);
  const body = {
    cwd: f.cwd,
    model: f.model === "default" ? null : f.model,
    extra_args,
    create_cwd: true,
  };
  try {
    const r = await fetch("/api/sessions", {
      method: "POST",
      headers: { "Content-Type": "application/json", ...authHeaders() },
      body: JSON.stringify(body),
    });
    if (!r.ok) {
      const t = await r.text();
      throw new Error(t || `spawn failed: ${r.status}`);
    }
    const info = await r.json();
    closeSpawnSheet();
    // Jump directly into the session we just created.
    setTimeout(() => openSession(info.id), 100);
  } catch (e) {
    f.error = String(e.message || e);
    f.submitting = false;
    render();
  }
}

function renderDetail() {
  const s = state.sessions.find((x) => x.id === state.selectedId);
  if (!s) { state.view = "list"; render(); return; }
  const pr = state.permissionRequests[s.id];
  const isAwaiting = s.status === "awaiting_permission" || pr;
  const promptBlock = isAwaiting ? `
    <div class="prompt-banner">
      <div class="prompt-title">⚠ needs you${pr && pr.tool_name ? ` · <span class="tool">${escapeHtml(pr.tool_name)}</span>` : ""}</div>
      <div class="prompt-actions">
        <button class="prompt-btn allow" data-keys="1\\r">1 · Yes</button>
        <button class="prompt-btn allow-all" data-keys="2\\r">2 · Yes, always</button>
        <button class="prompt-btn deny" data-keys="3\\r">3 · No</button>
      </div>
    </div>` : "";
  const root = el(`
    <div class="app" id="app">
      <div class="detail-bar">
        <button class="back" aria-label="Back">←</button>
        <div class="name">${escapeHtml(displayName(s))} · <span style="color:var(--muted);font-weight:400">${escapeHtml(stateLabel(s))}</span></div>
        <button class="menu" aria-label="Menu">⋯</button>
      </div>
      <div class="view">
        ${promptBlock}
        <div class="pty-mount" id="pty-mount"></div>
        <div class="chips">
          <button class="chip" data-keys="\\u001b">Esc</button>
          <button class="chip" data-keys="\\u001b[A">↑</button>
          <button class="chip" data-keys="\\u001b[B">↓</button>
          <button class="chip" data-keys="\\u0003">^C</button>
          <button class="chip" data-keys="\\u0004">^D</button>
        </div>
        <!-- Plain input bar. Trying to redirect into xterm on iOS Safari
             tripped every iOS keyboard quirk known to humanity; a real
             textarea that the user actually edits behaves correctly. -->
        <form class="input-bar" id="form">
          <textarea id="msg" rows="1" placeholder="Type and send…" autocapitalize="sentences"></textarea>
          <button type="submit">Send</button>
        </form>
      </div>
    </div>
  `);
  document.getElementById("app").replaceWith(root);
  const mount = root.querySelector("#pty-mount");
  attachTerminalTo(mount);
  root.querySelector(".back").addEventListener("click", () => {
    detachTerminal();
    state.view = "list"; state.selectedId = null; render();
  });
  root.querySelectorAll(".chip, .prompt-btn").forEach((c) => {
    c.addEventListener("click", () => {
      // Keyboard chip focuses the password proxy synchronously in the
      // click handler so iOS pops the keyboard.
      if (c.dataset.action === "focus") {
        const ta = document.getElementById("ptype");
        if (ta) ta.focus({ preventScroll: true });
        return;
      }
      sendKeys(decodeKeys(c.dataset.keys));
    });
  });
  // Plain input bar wiring. Tap Send (or hit Enter on a hardware keyboard)
  // to submit. The textarea forwards its full value plus \r as one
  // bracketed-paste-wrapped block on the daemon side, so multi-line
  // paste survives intact instead of submitting on the first \n.
  const msg = root.querySelector("#msg");
  const grow = () => {
    msg.style.height = "auto";
    msg.style.height = Math.min(msg.scrollHeight, 5 * 22) + "px";
  };
  msg.addEventListener("input", grow);
  // On mobile keyboards Enter inserts a newline; we keep that behaviour
  // (Enter ≠ Send on phone) and the user taps the Send button to commit.
  // On desktop hardware Cmd/Ctrl-Enter sends.
  msg.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      submitMsg();
    }
  });
  const form = root.querySelector("#form");
  form.addEventListener("submit", (e) => {
    e.preventDefault();
    submitMsg();
  });
  function submitMsg() {
    const v = msg.value;
    if (!v) return;
    msg.value = "";
    msg.style.height = "auto";
    sendKeys(v + "\r");
  }
  // Tapping the terminal area focuses the proxy synchronously inside the
  // touch handler so iOS pops the keyboard.
  const ptyMount = document.getElementById("pty-mount");
  const focusInput = () => ptype.focus({ preventScroll: true });
  ptyMount.addEventListener("touchstart", focusInput, { passive: true });
  ptyMount.addEventListener("mousedown", focusInput);
}

function decodeKeys(s) {
  return s.replace(/\\r/g, "\r").replace(/\\n/g, "\n").replace(/\\u([0-9a-fA-F]{4})/g, (_, h) => String.fromCharCode(parseInt(h, 16)));
}

function sendKeys(text) {
  if (!state.selectedId) return;
  const b64 = btoa(text);
  sendWS({ kind: "send_input", session_id: state.selectedId, data_b64: b64 });
}

function openSession(id) {
  state.selectedId = id;
  state.view = "detail";
  state.ptyNextSeq = 0;
  resetTerminalForSession(id);
  sendWS({ kind: "subscribe", session_id: id, since: 0 });
  render();
}

function displayName(s) {
  return s.display_override || s.ai_title || s.name || "session";
}
function stateLabel(s) {
  switch (s.status) {
    case "awaiting_permission": return "needs you";
    case "streaming": return "working";
    case "spawning": return "starting";
    case "exited": return s.exit_code === 0 ? "exited" : `exited ${s.exit_code ?? ""}`;
    case "resume_failed": return "resume failed";
    default: return s.status;
  }
}

// ---- Deep-link from notification --------------------------------------------

if ("serviceWorker" in navigator) {
  navigator.serviceWorker.addEventListener("message", (e) => {
    const d = e.data || {};
    if (d.kind === "deeplink" && d.session_id) {
      openSession(d.session_id);
    }
  });
}

function readDeepLinkFromQuery() {
  const u = new URL(location.href);
  return u.searchParams.get("session");
}

// ---- Boot -------------------------------------------------------------------

(async () => {
  await tryAutoPair();
  if (state.token) {
    connectWS();
    await ensureNotificationPermission();
    ensurePushSubscription();
    const deep = readDeepLinkFromQuery();
    if (deep) {
      // Defer until WS delivers a snapshot; openSession will subscribe when
      // we have ws.readyState === 1. Best-effort:
      const tryOpen = () => {
        if (state.ws && state.ws.readyState === 1) {
          openSession(deep);
          history.replaceState({}, "", "/");
        } else {
          setTimeout(tryOpen, 200);
        }
      };
      tryOpen();
    }
  }
  render();
})();
