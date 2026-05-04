// claws phone PWA — vanilla single-file UI with Web Push + permission UI.
// (The reset escape hatch lives inline in index.html so it works even
// when this file is held hostage by a stale service worker cache.)

const TOKEN_KEY = "claws.deviceToken";
const PUSH_FLAG_KEY = "claws.pushSubscribed";
const THEME_KEY = "claws.theme";

// Same theme set as the TUI (src/theme.rs). Order matters — the picker
// renders in this order and matches the TUI's selection cycle.
const THEMES = [
  { name: "default",     label: "default" },
  { name: "catppuccin",  label: "catppuccin mocha" },
  { name: "tokyo-night", label: "tokyo night" },
  { name: "nord",        label: "nord" },
  { name: "mono",        label: "monochrome" },
];

function applyTheme(name) {
  const known = THEMES.find((t) => t.name === name) ? name : "catppuccin";
  document.documentElement.dataset.theme = known;
  localStorage.setItem(THEME_KEY, known);
  // Update the iOS Safari status-bar tint color in real time.
  const meta = document.querySelector('meta[name="theme-color"]');
  if (meta) {
    const bg = getComputedStyle(document.documentElement).getPropertyValue("--bg").trim();
    if (bg) meta.setAttribute("content", bg);
  }
}

function loadThemeAtBoot() {
  applyTheme(localStorage.getItem(THEME_KEY) || "catppuccin");
}
loadThemeAtBoot();
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
  // What's currently mounted in `#app`. Snapshot-driven render() calls
  // dispatch to in-place update*() functions when this matches the desired
  // view, so the DOM (and focused input / xterm) survives. Only a real
  // view change pays the replaceWith cost.
  mountedView: null, // "pair" | "list" | "detail"
  mountedDetailId: null,
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
  try { fitAddon.fit(); } catch {}
  // Tell the daemon our new viewport so it can resize OUR private parser
  // (per-client virtual screen — no effect on the actual PTY or any other
  // viewer). Daemon replies with a fresh state_formatted at the new size.
  if (term && state.selectedId) {
    sendWS({
      kind: "resize",
      session_id: state.selectedId,
      rows: term.rows,
      cols: term.cols,
    });
  }
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
    // Take xterm's helper textarea out of the focus path — our Ace-style
    // iOS input proxy is the only element that should receive keyboard.
    disableXtermHelperTextarea();
    // Keyboard-aware viewport. When the iOS keyboard appears, visualViewport
    // shrinks below window.innerHeight; we mirror that as bottom padding on
    // the body so the .pty-mount (flex:1) shrinks to the visible area and
    // xterm's scroll-to-cursor keeps your typing in view. Pure layout-side
    // adjustment — no xterm.fit, no resize message, so iOS focus on the
    // input proxy isn't disturbed.
    if (window.visualViewport) {
      // Only listen to `resize` — keyboard show/hide is the resize event
      // we care about. `scroll` fires continuously while iOS Safari's URL
      // bar collapses/expands on touch-scroll, and updating the inset on
      // each one made the layout jitter visibly. Threshold tiny deltas
      // (URL-bar shimmies) so we only react to real keyboard transitions.
      let lastInset = 0;
      const adjustForKeyboard = () => {
        const vv = window.visualViewport;
        const inset = Math.max(0, window.innerHeight - vv.height);
        if (Math.abs(inset - lastInset) < 50) return;
        lastInset = inset;
        document.documentElement.style.setProperty("--keyboard-inset", `${inset}px`);
      };
      window.visualViewport.addEventListener("resize", adjustForKeyboard);
      adjustForKeyboard();
    }
  }
  // Defer fit + subscribe until layout settles. xterm's first measure
  // can return zeros on some devices when called inside a single rAF
  // after open(); we double-rAF + short delay to be sure dimensions
  // are real. Subscribe carries our actual rows/cols so the daemon
  // builds OUR private virtual screen at the right geometry.
  requestAnimationFrame(() => requestAnimationFrame(() => {
    setTimeout(() => {
      if (fitAddon) { try { fitAddon.fit(); } catch {} }
      if (state.selectedId && state.ws && state.ws.readyState === 1) {
        sendWS({
          kind: "subscribe",
          session_id: state.selectedId,
          rows: term.rows || 24,
          cols: term.cols || 80,
        });
      }
      t.scrollToBottom();
    }, 50);
  }));
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

// ---- iOS keyboard input proxy (Ace pattern) --------------------------------
//
// Why this is shaped the way it is: iOS Safari dismisses the soft keyboard
// when the focused element's geometry changes, when it gets reparented, or
// when its `value` is set to "". The xterm helper textarea + every variant
// we tried all violated at least one of those rules. The Ace editor's
// textinput.js solves it (proven on Cloud9, Replit mobile, etc.) by:
//
//   1. Using a real <textarea> at fixed 1×1 with opacity 0 — so geometry is
//      invariant under any reflow.
//   2. Never reparenting it — it lives at body root and stays put.
//   3. Never clearing it — instead the value cycles between PLACEHOLDER
//      and PLACEHOLDER+keystrokes, with the cursor anchored mid-string.
//      iOS treats that as "still being edited" and keeps the keyboard up.
//
// The PLACEHOLDER sentinel:
//   "\n ab" + cursor + "cde fg\n"
// gives autocorrect "anchor" characters so it doesn't suggest replacements
// for the entire content, and lets us detect arrow keys via selectionchange
// (the only event iOS fires for arrow keys with a hardware keyboard).

const IOS_PLACEHOLDER_PRE = "\n ab";
const IOS_PLACEHOLDER_POST = "cde fg\n";
const IOS_PLACEHOLDER = IOS_PLACEHOLDER_PRE + IOS_PLACEHOLDER_POST;
const IOS_CURSOR_HOME = IOS_PLACEHOLDER_PRE.length;
const IOS_CURSOR_TAIL = IOS_CURSOR_HOME;

const iosInput = document.getElementById("ios-input");
let iosComposing = false;

function iosResetSelection() {
  if (!iosInput) return;
  iosInput.value = IOS_PLACEHOLDER;
  try { iosInput.setSelectionRange(IOS_CURSOR_HOME, IOS_CURSOR_TAIL); } catch (e) {}
}

// MUST be called synchronously inside touchend / click / mousedown.
// Async — including setTimeout(fn, 0) — breaks iOS's user-gesture rule.
function focusIosInput() {
  if (!iosInput) return;
  iosResetSelection();
  iosInput.focus({ preventScroll: true });
}

if (iosInput) {
  iosResetSelection();

  iosInput.addEventListener("input", () => {
    if (iosComposing) return;
    const value = iosInput.value;
    if (value === IOS_PLACEHOLDER) return;

    // Common-prefix length against PLACEHOLDER_PRE.
    let prefixLen = 0;
    const minPre = Math.min(IOS_CURSOR_HOME, value.length);
    while (prefixLen < minPre && value[prefixLen] === IOS_PLACEHOLDER[prefixLen]) {
      prefixLen++;
    }
    // Common-suffix length against PLACEHOLDER_POST.
    let suffixLen = 0;
    const placeholderTail = IOS_PLACEHOLDER.length - IOS_CURSOR_TAIL;
    while (
      suffixLen < placeholderTail &&
      suffixLen < value.length - prefixLen &&
      value[value.length - 1 - suffixLen] === IOS_PLACEHOLDER[IOS_PLACEHOLDER.length - 1 - suffixLen]
    ) {
      suffixLen++;
    }

    const inserted = value.slice(prefixLen, value.length - suffixLen);
    const deletedLeft = IOS_CURSOR_HOME - prefixLen;
    const deletedRight = placeholderTail - suffixLen;

    // Translate into terminal bytes. Backspace = DEL (0x7f) per Claude's
    // line-edit conventions. Forward-delete = CSI 3 ~.
    for (let i = 0; i < deletedLeft; i++) sendKeys("\x7f");
    if (inserted) {
      // textarea returns soft-Enter as \n; the terminal wants \r.
      sendKeys(inserted.replace(/\n/g, "\r"));
    }
    for (let i = 0; i < deletedRight; i++) sendKeys("\x1b[3~");

    // Re-anchor the placeholder so the next keystroke diff still works.
    iosResetSelection();
  });

  // Special keys: Enter/Tab/Backspace at the boundary, plus hardware-only
  // keys that don't fire `input` (function keys, etc.).
  iosInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      sendKeys("\r");
      e.preventDefault();
      iosResetSelection();
    } else if (e.key === "Tab") {
      sendKeys("\t");
      e.preventDefault();
    } else if (e.key === "Backspace" && iosInput.selectionStart === IOS_CURSOR_HOME) {
      // Backspace at the boundary — input event won't see anything.
      sendKeys("\x7f");
      e.preventDefault();
    }
  });

  // IME composition support — even English autocorrect uses this. While
  // composing, we suppress per-keystroke forwarding; the final string
  // arrives in the input event after compositionend.
  iosInput.addEventListener("compositionstart", () => { iosComposing = true; });
  iosInput.addEventListener("compositionend", () => {
    iosComposing = false;
    iosInput.dispatchEvent(new Event("input"));
  });

  // iOS arrow keys (hardware keyboards) only fire `selectionchange` on
  // document, not on the textarea. Map cursor positions to arrow escapes.
  document.addEventListener("selectionchange", () => {
    if (document.activeElement !== iosInput) return;
    if (iosComposing) return;
    const s = iosInput.selectionStart;
    const e = iosInput.selectionEnd;
    if (s !== e) return;
    if (s === IOS_CURSOR_HOME) return;
    let seq = null;
    if (s === 0) seq = "\x1b[A";                                  // up
    else if (s === IOS_CURSOR_HOME - 1) seq = "\x1b[D";           // left
    else if (s === IOS_CURSOR_TAIL) seq = "\x1b[C";               // right
    else if (s === IOS_PLACEHOLDER.length - 1) seq = "\x1b[B";    // down
    if (seq) sendKeys(seq);
    iosResetSelection();
  });
}

// Disable xterm's built-in helper textarea so it doesn't compete for focus.
function disableXtermHelperTextarea() {
  if (!term) return;
  const ta = term.textarea || (term.element && term.element.querySelector("textarea"));
  if (ta) ta.tabIndex = -1;
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
  if (!state.token) {
    if (state.mountedView !== "pair") {
      detachTerminal();
      renderPair();
      state.mountedView = "pair";
      state.mountedDetailId = null;
    } else {
      updatePair();
    }
    return;
  }
  const desired = state.view === "detail" && state.selectedId ? "detail" : "list";
  if (desired === "list") {
    if (state.mountedView !== "list") {
      detachTerminal();
      renderList();
      state.mountedView = "list";
      state.mountedDetailId = null;
    } else {
      updateList();
    }
  } else {
    // Detail view — full re-render only on first entry or when the
    // selected session changes. Snapshot ticks for the SAME session
    // route to updateDetail() so xterm stays attached (no re-subscribe,
    // no flicker).
    if (state.mountedView !== "detail" || state.mountedDetailId !== state.selectedId) {
      detachTerminal();
      renderDetail();
      state.mountedView = "detail";
      state.mountedDetailId = state.selectedId;
    } else {
      updateDetail();
    }
  }
}

function updatePair() {
  // Pairing screen has only one piece of state-driven content (the error
  // text); the input already lives in the DOM and we never want to clobber
  // it mid-typing.
  const root = document.getElementById("app");
  if (!root) { state.mountedView = null; return render(); }
  const pair = root.querySelector(".pair");
  if (!pair) return;
  let err = pair.querySelector(".err");
  if (state.pairError) {
    if (!err) {
      err = document.createElement("div");
      err.className = "err";
      pair.appendChild(err);
    }
    err.textContent = state.pairError;
  } else if (err) {
    err.remove();
  }
}

function updateList() {
  const root = document.getElementById("app");
  if (!root) { state.mountedView = null; return render(); }

  // Counts.
  const counts = { streaming: 0, awaiting_permission: 0, idle: 0, exited: 0 };
  for (const s of state.sessions) {
    if (counts[s.status] !== undefined) counts[s.status]++;
  }
  const countsEl = root.querySelector(".counts");
  if (countsEl) {
    countsEl.innerHTML = `
      ${counts.streaming ? `<span class="c working"><span class="glyph">●</span> ${counts.streaming}</span>` : ""}
      ${counts.awaiting_permission ? `<span class="c awaiting"><span class="glyph">★</span> ${counts.awaiting_permission}</span>` : ""}
      ${counts.idle ? `<span class="c idle"><span class="glyph">◐</span> ${counts.idle}</span>` : ""}
    `;
  }

  // Connection chip.
  const conn = root.querySelector(".conn");
  if (conn) {
    conn.className = `conn ${state.wsAlive ? "live" : "gone"}`;
    conn.textContent = state.wsAlive ? "live" : "offline";
  }

  // Session rows. No focusable elements live inside rows, so it's safe to
  // rebuild innerHTML on each tick.
  const list = root.querySelector(".list");
  if (list) {
    const rows = state.sessions.map((s) => `
      <div class="row ${escapeHtml(s.status)}" data-id="${escapeHtml(s.id)}">
        <div class="marker"><span class="glyph">${statusGlyph(s.status)}</span></div>
        <div class="meta">
          <div class="name">${escapeHtml(displayName(s))}${s.dangerous ? '<span class="danger">!</span>' : ""}</div>
          <div class="sub">${escapeHtml(s.cwd)}</div>
        </div>
        <div class="state">${escapeHtml(stateLabel(s))}</div>
      </div>
    `).join("");
    const empty = state.sessions.length ? "" : `<div class="empty"><pre>╭───────────╮
│  no       │
│  sessions │
╰───────────╯</pre>tap <strong>+</strong> to spawn one</div>`;
    list.innerHTML = rows + empty;
    list.querySelectorAll(".row").forEach((r) => {
      r.addEventListener("click", () => openSession(r.dataset.id));
    });
  }

  // Sheet open/close. We deliberately do NOT touch a sheet that's already
  // mounted — its <input> may be focused and rebuilding it would dismiss
  // the iOS keyboard.
  const existingSheet = root.querySelector(".sheet-backdrop");
  if (state.spawnOpen && !existingSheet) {
    renderSpawnSheet(root);
  } else if (state.themeOpen && !existingSheet) {
    renderThemeSheet(root);
  } else if (!state.spawnOpen && !state.themeOpen && existingSheet) {
    existingSheet.remove();
  }
}

function updateDetail() {
  const root = document.getElementById("app");
  if (!root) { state.mountedView = null; return render(); }
  const s = state.sessions.find((x) => x.id === state.selectedId);
  if (!s) {
    state.view = "list";
    state.mountedView = null;
    state.mountedDetailId = null;
    render();
    return;
  }

  // Detail bar: name + status tag.
  const nameEl = root.querySelector(".detail-bar .name");
  if (nameEl) {
    nameEl.innerHTML = `
      ${escapeHtml(displayName(s))}
      <span class="sep">·</span>
      <span class="state-tag ${escapeHtml(s.status)}">${escapeHtml(stateLabel(s))}</span>
    `;
  }

  // Permission prompt banner — toggle in place. Don't recreate it if it's
  // already there with the same shape, to avoid layout jitter.
  const pr = state.permissionRequests[s.id];
  const isAwaiting = s.status === "awaiting_permission" || pr;
  const view = root.querySelector(".view");
  let banner = view ? view.querySelector(".prompt-banner") : null;
  if (isAwaiting && view) {
    const bannerHtml = `
      <div class="prompt-title">⚠ needs you${pr && pr.tool_name ? ` · <span class="tool">${escapeHtml(pr.tool_name)}</span>` : ""}</div>
      <div class="prompt-actions">
        <button class="prompt-btn allow" data-keys="1\\r">1 · Yes</button>
        <button class="prompt-btn allow-all" data-keys="2\\r">2 · Yes, always</button>
        <button class="prompt-btn deny" data-keys="3\\r">3 · No</button>
      </div>
    `;
    if (!banner) {
      banner = document.createElement("div");
      banner.className = "prompt-banner";
      view.insertBefore(banner, view.firstChild);
    }
    if (banner.dataset.shape !== bannerHtml) {
      banner.innerHTML = bannerHtml;
      banner.dataset.shape = bannerHtml;
      banner.querySelectorAll(".prompt-btn").forEach((b) => {
        b.addEventListener("click", () => sendKeys(decodeKeys(b.dataset.keys)));
      });
    }
  } else if (!isAwaiting && banner) {
    banner.remove();
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
      <header><div class="brand">claws</div></header>
      <div class="view">
        <div class="pair">
          <div class="pair-logo">▎ claws</div>
          <h1>pair this device</h1>
          <p>Run <code>claws phone pair</code> on your dev box and enter the code below — or scan the QR it prints to skip the typing.</p>
          <input id="code" placeholder="pair code" value="${escapeHtml(code)}" autocapitalize="none" autocorrect="off" spellcheck="false" />
          <button id="go">pair</button>
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

  // Aggregate status counts — same shape as the TUI's top bar (e.g. "3● 1◐ 1★").
  const counts = { streaming: 0, awaiting_permission: 0, idle: 0, exited: 0 };
  for (const s of state.sessions) {
    if (counts[s.status] !== undefined) counts[s.status]++;
  }

  const rows = state.sessions.map((s) => `
    <div class="row ${escapeHtml(s.status)}" data-id="${escapeHtml(s.id)}">
      <div class="marker"><span class="glyph">${statusGlyph(s.status)}</span></div>
      <div class="meta">
        <div class="name">${escapeHtml(displayName(s))}${s.dangerous ? '<span class="danger">!</span>' : ""}</div>
        <div class="sub">${escapeHtml(s.cwd)}</div>
      </div>
      <div class="state">${escapeHtml(stateLabel(s))}</div>
    </div>
  `).join("");
  const empty = state.sessions.length ? "" : `<div class="empty"><pre>╭───────────╮
│  no       │
│  sessions │
╰───────────╯</pre>tap <strong>+</strong> to spawn one</div>`;
  const root = el(`
    <div class="app" id="app">
      <header>
        <div class="brand">claws</div>
        <div class="counts">
          ${counts.streaming ? `<span class="c working"><span class="glyph">●</span> ${counts.streaming}</span>` : ""}
          ${counts.awaiting_permission ? `<span class="c awaiting"><span class="glyph">★</span> ${counts.awaiting_permission}</span>` : ""}
          ${counts.idle ? `<span class="c idle"><span class="glyph">◐</span> ${counts.idle}</span>` : ""}
        </div>
        <button class="hdr-btn" id="theme-btn" aria-label="Theme">t</button>
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
  root.querySelector("#theme-btn").addEventListener("click", () => openThemeSheet());
  if (state.spawnOpen) renderSpawnSheet(root);
  if (state.themeOpen) renderThemeSheet(root);
}

function openThemeSheet() {
  state.themeOpen = true;
  render();
}

function closeThemeSheet() {
  state.themeOpen = false;
  render();
}

function renderThemeSheet(root) {
  const current = localStorage.getItem(THEME_KEY) || "catppuccin";
  const sheet = el(`
    <div class="sheet-backdrop">
      <div class="sheet">
        <div class="sheet-title">theme</div>
        <div class="theme-list">
          ${THEMES.map((t) => `
            <button type="button" class="theme-row ${t.name === current ? "active" : ""}" data-theme="${t.name}">
              <span class="marker">${t.name === current ? "›" : " "}</span>
              <span class="label">${escapeHtml(t.label)}</span>
              <span class="swatches" data-theme-preview="${t.name}">
                <span class="sw sw-good"></span>
                <span class="sw sw-warn"></span>
                <span class="sw sw-bad"></span>
                <span class="sw sw-accent"></span>
                <span class="sw sw-cost"></span>
              </span>
            </button>
          `).join("")}
        </div>
        <div class="sheet-actions">
          <button class="sheet-cancel" id="theme-close">close</button>
        </div>
      </div>
    </div>
  `);
  root.appendChild(sheet);
  sheet.addEventListener("click", (e) => { if (e.target === sheet) closeThemeSheet(); });
  sheet.querySelector("#theme-close").addEventListener("click", closeThemeSheet);
  sheet.querySelectorAll(".theme-row").forEach((b) => {
    b.addEventListener("click", () => {
      applyTheme(b.dataset.theme);
      closeThemeSheet();
    });
  });
}

function statusGlyph(status) {
  switch (status) {
    case "streaming": return "●";
    case "awaiting_permission": return "★";
    case "idle": return "◐";
    case "spawning": return "◦";
    case "exited": return "✓";
    case "resume_failed": return "✗";
    default: return "·";
  }
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
        <label class="sheet-label">flags <span class="sheet-hint">// optional</span></label>
        <input class="sheet-input" id="sf-flags" autocapitalize="none" autocorrect="off" spellcheck="false" placeholder="--effort xhigh, --add-dir &lt;path&gt;" value="${escapeHtml(f.flags)}" />
        ${f.error ? `<div class="sheet-err">${escapeHtml(f.error)}</div>` : ""}
        <div class="sheet-actions">
          <button class="sheet-cancel" id="sf-cancel">cancel</button>
          <button class="sheet-submit" id="sf-submit" ${f.submitting ? "disabled" : ""}>${f.submitting ? "spawning…" : "spawn"}</button>
        </div>
      </div>
    </div>
  `);
  root.appendChild(sheet);
  sheet.addEventListener("click", (e) => { if (e.target === sheet) closeSpawnSheet(); });
  sheet.querySelector("#sf-cancel").addEventListener("click", closeSpawnSheet);
  sheet.querySelectorAll(".model-pill").forEach((p) => {
    // Mutate the .selected class in place rather than calling render() —
    // a render() while the sheet's input has iOS focus would route through
    // updateList(), which by design leaves the sheet alone, and we'd
    // miss the visual update.
    p.addEventListener("click", () => {
      f.model = p.dataset.m;
      sheet.querySelectorAll(".model-pill").forEach((q) => {
        q.classList.toggle("selected", q.dataset.m === f.model);
      });
    });
  });
  sheet.querySelector("#sf-cwd").addEventListener("input", (e) => { f.cwd = e.target.value; });
  sheet.querySelector("#sf-flags").addEventListener("input", (e) => { f.flags = e.target.value; });
  sheet.querySelector("#sf-submit").addEventListener("click", () => submitSpawn(sheet));
}

async function submitSpawn(sheet) {
  const f = state.spawnForm;
  if (!f || f.submitting) return;
  // Mutate the existing sheet DOM directly instead of going through
  // render(); the sheet may hold an iOS-focused input and a re-render
  // would dismiss the keyboard.
  const errEl = () => sheet ? sheet.querySelector(".sheet-err") : null;
  const setError = (msg) => {
    if (!sheet) return;
    let e = errEl();
    if (!e) {
      e = document.createElement("div");
      e.className = "sheet-err";
      const actions = sheet.querySelector(".sheet-actions");
      if (actions) sheet.querySelector(".sheet").insertBefore(e, actions);
      else sheet.querySelector(".sheet").appendChild(e);
    }
    e.textContent = msg;
  };
  const clearError = () => { const e = errEl(); if (e) e.remove(); };
  const setSubmitting = (val) => {
    if (!sheet) return;
    const btn = sheet.querySelector("#sf-submit");
    if (btn) {
      btn.disabled = val;
      btn.textContent = val ? "spawning…" : "spawn";
    }
  };

  if (!f.cwd.trim()) { f.error = "directory required"; setError(f.error); return; }
  f.submitting = true;
  f.error = null;
  clearError();
  setSubmitting(true);
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
    setError(f.error);
    setSubmitting(false);
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
        <div class="name">
          ${escapeHtml(displayName(s))}
          <span class="sep">·</span>
          <span class="state-tag ${escapeHtml(s.status)}">${escapeHtml(stateLabel(s))}</span>
        </div>
        <button class="menu" aria-label="Menu">⋯</button>
      </div>
      <div class="view">
        ${promptBlock}
        <div class="pty-mount" id="pty-mount"></div>
        <div class="chips">
          <button class="chip" data-action="focus">⌨︎ Type</button>
          <button class="chip" data-keys="\\u001b">Esc</button>
          <button class="chip" data-keys="\\u001b[A">↑</button>
          <button class="chip" data-keys="\\u001b[B">↓</button>
          <button class="chip" data-keys="\\u0003">^C</button>
          <button class="chip" data-keys="\\u0004">^D</button>
        </div>
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
      // Keyboard chip focuses the iOS input proxy synchronously in the
      // click handler so iOS pops the keyboard.
      if (c.dataset.action === "focus") {
        focusIosInput();
        return;
      }
      sendKeys(decodeKeys(c.dataset.keys));
    });
  });
  // Tapping the terminal area also summons the keyboard, mirroring native
  // iOS terminal apps. Fires inside the touch handler.
  const ptyMount = document.getElementById("pty-mount");
  ptyMount.addEventListener("touchend", focusIosInput, { passive: false });
  ptyMount.addEventListener("mousedown", focusIosInput);
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
  // The actual subscribe is sent in attachTerminalTo() once xterm has
  // settled and we know our true rows/cols — those go to the daemon so
  // it can build our private virtual screen at the right geometry.
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
