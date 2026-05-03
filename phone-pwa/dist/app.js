// claws phone PWA — Phase 1 single-file UI.
// Phase 2 swaps this for a Vite/Preact build with xterm.js.

const TOKEN_KEY = "claws.deviceToken";
const $app = document.getElementById("app");

const state = {
  token: localStorage.getItem(TOKEN_KEY),
  view: "list", // "list" | "detail" | "pair"
  selectedId: null,
  sessions: [],
  ptyLines: [], // for selected session
  ptyNextSeq: 0,
  ws: null,
  wsAlive: false,
  pairError: null,
};

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
      sortSessions();
      render();
      break;
    case "session.update": {
      const s = msg.session;
      const idx = state.sessions.findIndex((x) => x.id === s.id);
      if (idx >= 0) state.sessions[idx] = s;
      else state.sessions.push(s);
      sortSessions();
      render();
      break;
    }
    case "session.removed":
      state.sessions = state.sessions.filter((s) => s.id !== msg.session_id);
      if (state.selectedId === msg.session_id) {
        state.selectedId = null;
        state.view = "list";
      }
      render();
      break;
    case "session.output":
      if (msg.session_id === state.selectedId) {
        appendPtyChunk(msg.data_b64);
        state.ptyNextSeq = msg.next_seq;
        render();
      }
      break;
    case "session.permission_request":
      // Phase 3 will surface a structured Allow/Deny modal here.
      // For now, the awaiting_permission status badge in the list view + a
      // browser notification (when permission is granted) is enough.
      maybeNotify(msg);
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

// ---- PTY rendering ----------------------------------------------------------

function appendPtyChunk(b64) {
  const bin = atob(b64);
  // We don't run a full vt100 emulator client-side in Phase 1 — we strip
  // common ANSI escape sequences and append. Phase 2 swaps in xterm.js.
  const text = stripAnsi(bin);
  const last = state.ptyLines[state.ptyLines.length - 1] ?? "";
  const merged = last + text;
  const lines = merged.split(/\r?\n/);
  state.ptyLines = state.ptyLines.slice(0, -1).concat(lines);
  // Keep last 500 lines.
  if (state.ptyLines.length > 500) state.ptyLines = state.ptyLines.slice(-500);
}

function stripAnsi(s) {
  // Minimal: CSI ... letter, OSC ... BEL, single-char escapes.
  return s
    .replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, "")
    .replace(/\x1b\][^\x07]*\x07/g, "")
    .replace(/\x1b[=>]/g, "")
    .replace(/\r/g, "");
}

// ---- Notifications ----------------------------------------------------------

function maybeNotify(msg) {
  if (!("Notification" in window)) return;
  if (Notification.permission !== "granted") return;
  const s = state.sessions.find((x) => x.id === msg.session_id);
  const body = msg.tool_name ? `wants to run ${msg.tool_name}` : "needs your input";
  new Notification(`${s?.name || "claws"} — needs you`, { body, tag: msg.session_id });
}

async function ensureNotificationPermission() {
  if (!("Notification" in window)) return;
  if (Notification.permission === "default") {
    try { await Notification.requestPermission(); } catch {}
  }
}

// ---- Views ------------------------------------------------------------------

function render() {
  if (!state.token) { renderPair(); return; }
  if (state.view === "detail" && state.selectedId) renderDetail();
  else renderList();
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
  const empty = state.sessions.length ? "" : `<div class="empty">No sessions yet. Spawn one from claws on your dev box.</div>`;
  const root = el(`
    <div class="app" id="app">
      <header>
        <div class="title">claws</div>
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
}

function renderDetail() {
  const s = state.sessions.find((x) => x.id === state.selectedId);
  if (!s) { state.view = "list"; render(); return; }
  const lines = state.ptyLines.map((l) => `<div class="line">${escapeHtml(l)}</div>`).join("");
  const root = el(`
    <div class="app" id="app">
      <div class="detail-bar">
        <button class="back" aria-label="Back">←</button>
        <div class="name">${escapeHtml(displayName(s))} · <span style="color:var(--muted);font-weight:400">${escapeHtml(stateLabel(s))}</span></div>
        <button class="menu" aria-label="Menu">⋯</button>
      </div>
      <div class="view">
        <div class="pty" id="pty">${lines}</div>
        <div class="chips">
          <button class="chip" data-keys="\\r">⏎ Enter</button>
          <button class="chip" data-keys="1">1</button>
          <button class="chip" data-keys="2">2</button>
          <button class="chip" data-keys="\\u001b">Esc</button>
          <button class="chip" data-keys="\\u001b[A">↑</button>
          <button class="chip" data-keys="\\u001b[B">↓</button>
          <button class="chip" data-keys="\\u0003">^C</button>
        </div>
        <form class="input-bar" id="form">
          <input id="msg" placeholder="Type and send…" autocapitalize="sentences" />
          <button type="submit">Send</button>
        </form>
      </div>
    </div>
  `);
  document.getElementById("app").replaceWith(root);
  const pty = root.querySelector("#pty");
  pty.scrollTop = pty.scrollHeight;
  root.querySelector(".back").addEventListener("click", () => { state.view = "list"; state.selectedId = null; render(); });
  root.querySelectorAll(".chip").forEach((c) => {
    c.addEventListener("click", () => sendKeys(decodeKeys(c.dataset.keys)));
  });
  root.querySelector("#form").addEventListener("submit", (e) => {
    e.preventDefault();
    const input = root.querySelector("#msg");
    const v = input.value;
    input.value = "";
    sendKeys(v + "\r");
  });
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
  state.ptyLines = [];
  state.ptyNextSeq = 0;
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

// ---- Boot -------------------------------------------------------------------

(async () => {
  const paired = await tryAutoPair();
  if (state.token) {
    connectWS();
    ensureNotificationPermission();
  }
  render();
})();
