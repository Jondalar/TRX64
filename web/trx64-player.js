// <trx64-player> — a self-contained, framework-free web component that embeds a live
// wl-trx64 C64 in a page: connects to the daemon WS, mounts a cart, blits the video, and
// forwards keyboard/joystick input. The frame decoder + key map live HERE (with TRX64, who
// owns the frame format) so consumers never re-implement them — an embedder just does:
//
//   <script type="module" src="trx64-player.js"></script>
//   <trx64-player ws="wss://editor/ws/play" image="/play/wl.crt" audio joystick></trx64-player>
//
// Attributes:
//   ws        (required)  daemon WS URL (through your auth proxy in production)
//   image     (optional)  container-side path to mount on connect, e.g. /play/wl.crt
//   audio     (boolean)   enable WebAudio playback (starts on first user gesture)
//   joystick  (boolean)   start in joystick mode (arrows/WASD + Space=fire) instead of keys
//   autostart (boolean)   connect immediately (default: connect on first click/attach)
//   mount-policy          if-changed (default) | always | never. The machine is SHARED and
//                         mounting a cart POWER-CYCLES it, so the default mounts only when
//                         nothing is mounted or a DIFFERENT image is: opening / re-opening a
//                         view attaches to the running machine instead of rebooting it.
//   readonly  (boolean)   observer mode — render video/audio, send NO keyboard/joystick.
//
// Methods: call(method, params) · mount(path) · readState()
// Events:  trx64-connected {state} · trx64-state {state} · trx64-runstate {runState}
//
// Contract: docs/wl-trx64-play-api.md. No build step, no dependencies.

const BIN_VIC = 0x01, BIN_AUDIO = 0x02;
const VW = 384, VH = 272, SR = 44100;

// Browser KeyboardEvent.code → C64 PETSCII key NAME (§5 of the API doc). Symbolic-ish map:
// good for menus + typing; gameplay uses joystick mode. Entries that need a shifted C64 key
// carry a `shift:true` (the player holds L_SHIFT around them).
const KEYMAP = {
  Enter: "RETURN", Space: "SPACE", Backspace: "DEL", Escape: "RUN_STOP", Home: "HOME",
  ShiftLeft: "L_SHIFT", ShiftRight: "R_SHIFT", ControlLeft: "CTRL", ControlRight: "CTRL",
  AltLeft: "C_EQ", MetaLeft: "C_EQ", // the Commodore key
  ArrowDown: "CRSR_DN", ArrowRight: "CRSR_RT",
  ArrowUp: { name: "CRSR_DN", shift: true }, ArrowLeft: { name: "CRSR_RT", shift: true },
  F1: "F1", F3: "F3", F5: "F5", F7: "F7",
  F2: { name: "F1", shift: true }, F4: { name: "F3", shift: true },
  F6: { name: "F5", shift: true }, F8: { name: "F7", shift: true },
  Minus: "-", Equal: "=", Semicolon: ";", Quote: ":", Comma: ",", Period: ".",
  Slash: "/", Backslash: "POUND", Backquote: "LARROW",
  BracketLeft: "@", BracketRight: "*",
};
function codeToKey(e) {
  if (/^Key[A-Z]$/.test(e.code)) return e.code.slice(3);           // KeyA → "A"
  if (/^Digit[0-9]$/.test(e.code)) return e.code.slice(5);         // Digit4 → "4"
  if (/^Numpad[0-9]$/.test(e.code)) return e.code.slice(6);
  return KEYMAP[e.code] ?? null;
}

class Trx64Player extends HTMLElement {
  constructor() {
    // A custom-element constructor must NOT touch attributes/DOM (Chrome throws
    // "createElement result must not have attributes"). Only set plain JS fields here;
    // read attributes + build the shadow DOM in connectedCallback.
    super();
    // NB `_reqId`, NOT `id`: `id` is a REFLECTED DOM property, so `this.id = 0` would set the
    // element's id ATTRIBUTE — and an element that already carries attributes makes Chrome
    // reject `document.createElement("trx64-player")` ("the result must not have attributes").
    this.ws = null; this._reqId = 0; this.pend = new Map();
    this.mode = "keyboard";
    this.held = new Set();                 // C64 key names currently down (for cleanup)
    this.joy = { up: false, down: false, left: false, right: false, fire: false };
    this.audioCtx = null; this.audioTime = 0;
    this.reconnectMs = 500;
    this._img = null;
  }

  connectedCallback() {
    const root = this.attachShadow({ mode: "open" });
    root.innerHTML = `
      <style>
        :host { display:inline-block; background:#000; font:12px monospace; color:#8f8; }
        .wrap { position:relative; line-height:0; }
        canvas { width:100%; height:auto; image-rendering:pixelated; display:block; background:#000; }
        .bar { display:flex; gap:8px; align-items:center; padding:4px 6px; background:#111; line-height:1.4; }
        .bar button { font:inherit; color:#8f8; background:#222; border:1px solid #444; cursor:pointer; padding:2px 8px; }
        .bar .st { margin-left:auto; opacity:.8; }
        .dot { width:8px; height:8px; border-radius:50%; background:#666; display:inline-block; }
        .dot.on { background:#4caf50; } .dot.err { background:#e44; }
        .hint { opacity:.6; }
      </style>
      <div class="wrap" tabindex="0">
        <canvas width="${VW}" height="${VH}"></canvas>
      </div>
      <div class="bar">
        <span class="dot"></span>
        <button class="mode"></button>
        <span class="hint"></span>
        <span class="st">idle</span>
      </div>`;
    this.$wrap = root.querySelector(".wrap");
    this.$canvas = root.querySelector("canvas");
    this.$ctx = this.$canvas.getContext("2d");
    this._img = this.$ctx.createImageData(VW, VH);
    this.$dot = root.querySelector(".dot");
    this.$st = root.querySelector(".st");
    this.$hint = root.querySelector(".hint");
    this.$mode = root.querySelector(".mode");
    this.$mode.onclick = () => this.setMode(this.mode === "keyboard" ? "joystick" : "keyboard");
    this.setMode(this.mode);

    // Input is captured while the widget has focus (click it to grab the keyboard).
    this.$wrap.addEventListener("keydown", (e) => this.onKey(e, true));
    this.$wrap.addEventListener("keyup", (e) => this.onKey(e, false));
    this.$wrap.addEventListener("mousedown", () => { this.$wrap.focus(); if (this.hasAttribute("audio")) this.ensureAudio(); });
    this.$wrap.addEventListener("blur", () => this.releaseAll());

    if (this.hasAttribute("autostart")) this.connect();
    else { this.$st.textContent = "click to start"; this.$wrap.addEventListener("mousedown", () => this.connect(), { once: true }); }
  }

  disconnectedCallback() { this.releaseAll(); try { this.ws?.close(); } catch {} this.ws = null; }

  // ── WS + JSON-RPC ─────────────────────────────────────────────────────────
  call(method, params = {}) {
    return new Promise((resolve) => {
      if (!this.ws || this.ws.readyState !== 1) return resolve(null);
      const i = ++this._reqId; this.pend.set(i, resolve);
      this.ws.send(JSON.stringify({ jsonrpc: "2.0", id: i, method, params }));
    });
  }

  connect() {
    const url = this.getAttribute("ws");
    if (!url) { this.setStatus("no ws= attribute", "err"); return; }
    if (this.ws) return;
    this.setStatus("connecting…");
    const ws = new WebSocket(url);
    ws.binaryType = "arraybuffer";
    this.ws = ws;
    ws.onopen = async () => {
      this.setStatus("connected", "on"); this.reconnectMs = 500;
      await this.call("session/create");
      const state = await this.readState();
      this.emit("trx64-connected", { state });
      // The machine is SHARED: mounting a cart power-cycles it, so mounting on every
      // connect reboots a game someone else (or you, in another tab) is playing. Only
      // mount when the policy says so.
      const image = this.getAttribute("image");
      if (image && this.shouldMount(image, state)) await this.mount(image);
      else if (image) this.setStatus("attached", "on");
    };
    ws.onmessage = (e) => this.onMessage(e);
    ws.onerror = () => this.setStatus("ws error", "err");
    ws.onclose = () => {
      this.setStatus("disconnected", "err"); this.ws = null; this.releaseAll();
      if (this.isConnected) setTimeout(() => this.connect(), this.reconnectMs = Math.min(this.reconnectMs * 2, 8000));
    };
  }

  // ── embedder surface: state, mount policy, events ─────────────────────────
  /** Fire a DOM event so an embedder's control bar can show TRUTH instead of guessing.
   *  Events: `trx64-connected` {state}, `trx64-state` {state}, `trx64-runstate` {runState}. */
  emit(type, detail) {
    this.dispatchEvent(new CustomEvent(type, { detail, bubbles: true, composed: true }));
  }

  /** Read `session/state` and publish it as `trx64-state`. Returns the state (or null). */
  async readState() {
    const r = await this.call("session/state");
    const state = r?.result ?? null;
    if (state) this.emit("trx64-state", { state });
    return state;
  }

  /** Decide whether connecting should mount `image`, per the `mount-policy` attribute:
   *  `if-changed` (default) — mount only when nothing is mounted or a DIFFERENT image is;
   *  `always` — legacy behaviour (every connect power-cycles); `never` — attach only. */
  shouldMount(image, state) {
    const policy = (this.getAttribute("mount-policy") || "if-changed").toLowerCase();
    if (policy === "never") return false;
    if (policy === "always") return true;
    const cart = state?.media?.cart;
    if (!cart || !cart.path) return true;        // nothing in the machine → mount
    return cart.path !== image;                  // different image → mount; same → attach
  }

  /** Explicitly mount an image ("the build finished, take this cart"). Power-cycles on a
   *  cart, so it is a deliberate action — never a side effect of showing the machine. */
  async mount(path) {
    const r = await this.call("media/mount", { path });
    const failed = !!r?.error;
    this.setStatus(failed ? `mount failed: ${r.error.message}` : "playing", failed ? "err" : "on");
    await this.readState();
    return r;
  }

  onMessage(e) {
    if (typeof e.data === "string") {
      let m; try { m = JSON.parse(e.data); } catch { return; }
      if (m.id != null && this.pend.has(m.id)) { this.pend.get(m.id)(m); this.pend.delete(m.id); return; }
      if (m.method === "debug/paused") { this.setStatus("paused"); this.emit("trx64-runstate", { runState: "paused" }); }
      else if (m.method === "debug/running") { this.setStatus("playing", "on"); this.emit("trx64-runstate", { runState: "running" }); }
      return;
    }
    const b = new Uint8Array(e.data);
    if (b[0] === BIN_VIC) this.drawVic(e.data, b);
    else if (b[0] === BIN_AUDIO && this.audioCtx) this.playAudio(e.data);
  }

  // ── video: BIN_VIC → canvas ───────────────────────────────────────────────
  drawVic(buf, b) {
    const dv = new DataView(buf);
    const w = dv.getUint16(5, true), h = dv.getUint16(7, true);
    if (!w || !h) return;
    if (this.$canvas.width !== w || this.$canvas.height !== h) {
      this.$canvas.width = w; this.$canvas.height = h; this._img = this.$ctx.createImageData(w, h);
    }
    const palOff = 15, idxOff = palOff + 48, n = w * h;   // envelope(5)+header(10)=15
    if (b.length < idxOff + n) return;
    const d = this._img.data;
    for (let p = 0; p < n; p++) {
      const c = palOff + (b[idxOff + p] & 0x0f) * 3, o = p * 4;
      d[o] = b[c]; d[o + 1] = b[c + 1]; d[o + 2] = b[c + 2]; d[o + 3] = 255;
    }
    this.$ctx.putImageData(this._img, 0, 0);
  }

  // ── audio: BIN_AUDIO (s16le stereo 44100) → WebAudio ──────────────────────
  ensureAudio() {
    if (this.audioCtx) { if (this.audioCtx.state === "suspended") this.audioCtx.resume(); return; }
    try { this.audioCtx = new (window.AudioContext || window.webkitAudioContext)({ sampleRate: SR }); this.audioTime = this.audioCtx.currentTime; } catch {}
  }
  playAudio(buf) {
    const pcm = new Int16Array(buf, 5);                   // skip the 5-byte envelope
    const frames = pcm.length >> 1;                       // interleaved stereo
    if (!frames) return;
    const ab = this.audioCtx.createBuffer(2, frames, SR);
    const L = ab.getChannelData(0), R = ab.getChannelData(1);
    for (let i = 0; i < frames; i++) { L[i] = pcm[i * 2] / 32768; R[i] = pcm[i * 2 + 1] / 32768; }
    const src = this.audioCtx.createBufferSource(); src.buffer = ab; src.connect(this.audioCtx.destination);
    const now = this.audioCtx.currentTime;
    if (this.audioTime < now) this.audioTime = now + 0.05; // small lead; resync on underrun
    src.start(this.audioTime); this.audioTime += ab.duration;
  }

  // ── input ─────────────────────────────────────────────────────────────────
  setMode(mode) {
    this.mode = mode; this.releaseAll();
    this.$mode.textContent = mode === "joystick" ? "🕹 joystick" : "⌨ keyboard";
    this.$hint.textContent = mode === "joystick" ? "arrows/WASD move · Space fire" : "click to type · keys go to the C64";
  }

  onKey(e, down) {
    if (e.repeat && down) return;
    // Observer mode: watch a shared machine without being able to steer it.
    if (this.hasAttribute("readonly")) return;
    e.preventDefault();
    if (this.mode === "joystick") return this.onJoyKey(e, down);
    const key = codeToKey(e);
    if (!key) return;
    if (typeof key === "object") {                        // shifted C64 key
      if (down) { this.press("L_SHIFT"); this.press(key.name); }
      else { this.release(key.name); this.release("L_SHIFT"); }
    } else { down ? this.press(key) : this.release(key); }
  }
  onJoyKey(e, down) {
    const map = { ArrowUp: "up", KeyW: "up", ArrowDown: "down", KeyS: "down",
      ArrowLeft: "left", KeyA: "left", ArrowRight: "right", KeyD: "right",
      Space: "fire", KeyZ: "fire", ControlLeft: "fire" };
    const dir = map[e.code]; if (!dir) return;
    if (this.joy[dir] === down) return;
    this.joy[dir] = down;
    this.call("session/joystick_set", { port: 2, ...this.joy });
  }
  press(name) { if (this.held.has(name)) return; this.held.add(name); this.call("session/key_down", { key: name }); }
  release(name) { if (!this.held.has(name)) return; this.held.delete(name); this.call("session/key_up", { key: name }); }
  releaseAll() {
    for (const k of this.held) this.call("session/key_up", { key: k });
    this.held.clear();
    if (Object.values(this.joy).some(Boolean)) { this.joy = { up: false, down: false, left: false, right: false, fire: false }; this.call("session/joystick_clear", { port: 2 }); }
  }

  setStatus(text, dot) { if (this.$st) this.$st.textContent = text; if (this.$dot) this.$dot.className = "dot" + (dot ? " " + dot : ""); }
}

customElements.define("trx64-player", Trx64Player);
export { Trx64Player };
