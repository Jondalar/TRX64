# wl-trx64 Play API — consumer contract

The stable surface a web app (e.g. the Wasteland editor) codes against to **mount a
cartridge image, boot it, show the live picture, and take keyboard/joystick input** —
without touching the emulator's internals. This is the daemon's existing WS contract,
frozen and documented for consumers; **the daemon does not change for this** (Spec 799).

- **Provider:** the `wl-trx64` container running `trx64-daemon` (streaming, default mode).
- **Consumer:** any front-end. **The recommended path is to embed the shipped
  `<trx64-player>` web component (`web/trx64-player.js`)** — it already implements this
  whole contract (frame decode, key map, audio, reconnect). An embedder writes one tag;
  it never re-implements §4/§5. See §9. This doc specifies the wire protocol underneath it
  for anyone who must build a client from scratch (non-JS, custom UI).
- **Scope of this doc:** exactly the methods + frame formats the play loop needs. The
  daemon has a much larger monitor/trace/checkpoint surface; a play consumer ignores it.

---

## 1. Transport

**ONE WebSocket.** Two message kinds on it:

- **Control = JSON-RPC 2.0 over TEXT frames.** Request
  `{"jsonrpc":"2.0","id":<n>,"method":"<m>","params":{…}}` → response
  `{"jsonrpc":"2.0","id":<n>,"result":…}` or `{… ,"error":{"code","message"}}`.
  Server-initiated **notifications** (no `id`) also arrive as text, e.g.
  `{"method":"debug/running", …}`.
- **A/V = BINARY frames** (§4). Every binary WS message is one video or audio frame.

There is **one live machine per container** (Spec 787). Every client connected to the
same container drives/sees the SAME machine — the emulator is a single shared C64, not
per-connection. `session_id` may be passed but is not required (single session).

**No auth on the daemon WS** — see §7.

---

## 2. The play flow (happy path)

```
                     consumer                                wl-trx64 daemon
  Play clicked   ──► open WS  ws://<trx64>:4340  ───────────►  (accepts)
                 ──► {"method":"session/create"}          ───►  build/attach machine
                 ──► {"method":"media/mount",
                        "params":{"path":"/play/<name>.crt"}} ─►  power-cycle + boot the cart
                     ◄── BIN_VIC + BIN_AUDIO frames (~50/s) ◄──  (streams while a client is on)
  user types     ──► session/key_down / key_up  ──────────►   keyboard matrix
  user plays     ──► session/joystick_set  ──────────────►    joystick port 2
  Play closed    ──► session/close  (or just close the WS)  ─► machine stays; idle → 0% CPU
```

- The image file is written into the shared `/play` volume by the consumer's build
  step BEFORE this; `path` is the **container-side** path, identical in both containers
  by mount convention (`/play/<name>.crt`).
- After `media/mount` on a cart the machine **cold-boots and free-runs**; frames start
  flowing on their own. No explicit "run" call is needed for the view.
- Mounting a NEW cart later = call `media/mount` again (it power-cycles). To go back to a
  clean C64 without a cart, unmount/eject (see the daemon monitor; out of the play scope).

---

## 3. Control methods (the play subset)

All are JSON-RPC `method`s. `params` shown; omit `session_id` (single machine).

| method | params | returns / effect |
|---|---|---|
| `ping` | — | `{}` — liveness (use for the container healthcheck) |
| `session/create` | — | attaches/creates the one machine; returns session id + state |
| `session/state` | — | `{ c64Cycles, runState:"running"\|"paused", cpu:{pc,a,x,y,sp,flags}, vic:{…}, controlOwner, streamPump, … }` |
| `media/mount` | `{ "path": "/play/x.crt" }` | mount `.crt`/`.d64`/`.g64`; a cart power-cycles + boots. Returns `{ detail:{ mapperType, name, … }, … }` |
| `session/key_down` | `{ "key": "<NAME>" }` | press one C64 key (held). `key` = PETSCII name (§5) |
| `session/key_up` | `{ "key": "<NAME>" }` | release it |
| `session/joystick_set` | `{ "port":2, "up":bool, "down":bool, "left":bool, "right":bool, "fire":bool }` | set port-2 joystick lines (omit = false) |
| `session/joystick_clear` | `{ "port":2 }` | release all lines |
| `session/screenshot` | — | `{ dataUrl:"data:image/png;base64,…" }` — one PNG (384×272). For a thumbnail; the live view uses BIN_VIC, not this |
| `session/close` | — | soft-close (machine + media stay; a later create re-attaches) |
| `debug/pause` / `debug/run` | — | freeze / resume (usually not needed for play; the cart free-runs) |

Server **notifications** the consumer may observe (all text, no `id`): `debug/running`,
`debug/paused` (run-state changed → toggle a "running/paused" badge), `debug/stopped`.

---

## 4. Binary frame formats (byte-exact)

Every binary WS message: **`[type:u8][seq:u32 LE]`** (5-byte envelope) + a type-specific
payload. `type` `0x01` = VIC video, `0x02` = audio. `seq` = monotonic counter per stream.

### 4.1 BIN_VIC (0x01) — one video frame

Payload after the 5-byte envelope:

```
offset  size            field
  0     u16 LE          w      (= 384)
  2     u16 LE          h      (= 272)
  4     u8              fmt    (= 1, palette-indexed — the only format)
  5     u8              rsvd   (= 0)
  6     u32 LE          cycle  (C64 CPU cycle count, truncated)
 10     48 bytes        palette: 16 × (R,G,B), COLODORE order, index 0..15
 58     w*h bytes       indices: one byte per pixel; the LOW NIBBLE (& 0x0F) is the
                        palette index. Row-major, top-left first.
```

Decode to RGBA: `rgb = palette[(indices[p] & 0x0F)]`, alpha = 0xFF. Blit onto a
384×272 canvas (`putImageData`). ~50 frames/s at PAL realtime; latest-frame-wins (drop
if you can't keep up).

### 4.2 BIN_AUDIO (0x02) — one audio chunk

Payload after the envelope: **interleaved s16le STEREO PCM at 44100 Hz** (reSID is mono,
duplicated to L+R). Feed it to a WebAudio ring for playback. Audio is **optional/v2** for
a first consumer — the video loop stands alone.

---

## 5. Keyboard: `key` names (PETSCII matrix)

`session/key_down` / `key_up` take a `key` string = the C64 matrix key NAME (not a
browser `KeyboardEvent.key`; the consumer maps browser events → these):

- Letters `A`–`Z`, digits `0`–`9`.
- `RETURN` `SPACE` `DEL` `RUN_STOP` `HOME` `CTRL` `L_SHIFT` `R_SHIFT` `C_EQ` (Commodore key)
- Function keys `F1` `F3` `F5` `F7`; cursors `CRSR_DN` `CRSR_RT` (shift them for up/left).
- Symbols `+` `-` `*` `/` `=` `;` `:` `,` `.` `@` `POUND` `LARROW` (`←`) etc.

Send `key_down` on press, `key_up` on release. For a typed sequence with no physical
release (e.g. paste), press+release each with a small gap. (The daemon also has
`session/type` `{text}` for bulk PETSCII text — handy for LOAD/RUN.)

Joystick: WASD/arrows → `session/joystick_set {port:2, …}`; the C64 games read port 2.

---

## 6. Session lifecycle + resource shape

- **Container is permanent; sessions are the lifecycle.** Create on "Play", close on
  idle. An idle daemon (no connected client / paused machine) is **~0 % CPU**.
- **Recommended idle-kill:** the consumer closes the session (or drops the WS) after
  ~10 min with no connected viewer. The 100 %-CPU failure mode is a *free-running session
  with no one watching* — not the daemon itself.
- **Measured (QNAP native amd64, Spec 799 §7):** one streaming session = **~40 % of one
  core, ~150 MiB**, 50 fps. Put `--cpus`/memory caps on the container so a busy session
  can never starve the web app.
- **One machine.** A second concurrent player waits (or, later, a Spec 787 scratch
  process inside the container). Scale-out = more containers.

---

## 7. Auth + networking (consumer's job)

The daemon WS has **no authentication** — it trusts its network. The consumer MUST NOT
expose the daemon port publicly. Two supported shapes:

1. **Compose-internal only** — the daemon port is reachable only on the shared bridge
   network; the browser never talks to it directly.
2. **Reverse-proxy with auth** — the web app exposes e.g. `/ws/play` and proxies it to
   `ws://<trx64>:4340`, applying its own auth (Basic-Auth / session cookie) on the
   upgrade. This is the recommended shape for a browser Play tab.

Container reachability: the daemon binds `0.0.0.0` in the image (`TRX64_BIND=0.0.0.0`);
reach it by service name / static IP on the shared bridge, or a published port on the LAN.

---

## 8. Errors + healthcheck

- JSON-RPC errors: `{ "error": { "code": <int>, "message": <str> } }`. Common: `-32602`
  (bad/missing params, e.g. `key required`, `path required`), `-32001` (state conflict,
  e.g. `session is running under the autonomous loop` — pause before a manual
  `session/run`), `-32601` (unknown method).
- **Healthcheck:** open the WS and send `ping` → expect `{}`. (Or a bare TCP connect to
  the port for a liveness-only probe.)

---

## 9. Consuming it — embed the shipped `<trx64-player>` web component

TRX64 ships the browser client alongside the daemon: **`web/trx64-player.js`**, a
self-contained, framework-free custom element. It owns the frame decode (§4), the PETSCII
key map (§5), audio, focus/input handling, and auto-reconnect — so a consumer never
re-implements the protocol. This is deliberate: the frame format lives with TRX64, who
defines it, and the editor stays untouched. An embedder writes **one tag**:

```html
<script type="module" src="/trx64-player.js"></script>

<!-- ws through your auth proxy (§7); image = container-side path in /play -->
<trx64-player ws="wss://editor.example/ws/play" image="/play/wl.crt" audio></trx64-player>
```

Attributes: `ws` (required, daemon WS URL), `image` (cart/disk path to mount on connect),
`audio` (enable WebAudio, starts on first click), `joystick` (start in joystick mode),
`autostart` (connect immediately vs. on first click). Swap carts by setting `image` and
re-connecting; the component exposes the same `session/*` calls internally.

The editor's Play tab is then just: serve `trx64-player.js`, proxy `/ws/play` to the
daemon with auth (§7), drop the tag into the Play panel. No decode code, no key tables.

`web/demo.html` is a zero-dependency harness that boots a cart with only this component —
the exact shape an editor embeds.

**Building a client from scratch (non-JS / custom UI):** §4 (byte-exact frames) + §5
(key names) are the full wire spec; `web/trx64-player.js` is the reference implementation
to read. The Play loop is: connect WS → `session/create` → `media/mount` → blit BIN_VIC
frames onto a 384×272 canvas → forward keys as §5 names.

---

Cross-links: Spec 799 (packaging + sidecar arch — `../../C64ReverseEngineeringMCP/specs/799-trx64-docker.md`), `docker/Dockerfile` (the image), `web/trx64-player.js` (the shipped client) + `web/demo.html` (embed harness).
