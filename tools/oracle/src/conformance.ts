#!/usr/bin/env tsx
// ─────────────────────────────────────────────────────────────────────────────
// DIFFERENTIAL WS-CONFORMANCE GATE
//
// The .c64retrace trace oracle proves the C64 VM is byte-identical. It does NOT
// see the runtime WRAPPER: WS method shapes, server-push broadcasts, and the
// background side-effects (auto-capture, cart/disk persist, ingress checkpoints).
// Every divergence that bit us for two days lived there.
//
// This gate closes that hole. For each CASE it drives the SAME scripted scenario
// against BOTH daemons — the TS headless runtime (the AUTHORITY) and the TRX64
// Rust daemon — extracts a behavioural SIGNAL from each, and asserts the two
// signals are equal. A case FAILS when TRX64's signal diverges from TS's. We never
// hardcode the "expected" value: TS supplies the truth on every run, so the gate
// can't drift and can't miss a field nobody thought to assert.
//
// Usage:
//   tsx src/conformance.ts                 # all cases
//   tsx src/conformance.ts --severity P0   # only P0 cases
//   tsx src/conformance.ts --only ws-media-0
//
// Exit 0 = every selected case GREEN (TRX64 ≡ TS). Exit 1 = at least one RED.
// ─────────────────────────────────────────────────────────────────────────────

import { existsSync, readFileSync, statSync } from "node:fs";
import { spawnDaemon, type Daemon, type SpawnOpts } from "./daemon.js";
import { connect, type RpcClient } from "./ws-client.js";
import { diffResponses, formatDivergence } from "./diff.js";
import { decodeTrace } from "./trace-decode.js";

const SAMPLES =
  process.env.C64RE_SAMPLES ??
  "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples";

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

// ── notification capture ────────────────────────────────────────────────────
interface NoteSink {
  notes: Array<{ method: string; params: any }>;
  off: () => void;
}
function collectNotes(c: RpcClient): NoteSink {
  const notes: Array<{ method: string; params: any }> = [];
  const off = c.onNotify((method, params) => notes.push({ method, params }));
  return { notes, off };
}

// ── shared helpers ──────────────────────────────────────────────────────────
/** The live shared singleton session (Spec 744 shared-attach). */
async function liveSession(c: RpcClient): Promise<string> {
  const list = (await c.call("session/list")) as any;
  const arr = Array.isArray(list) ? list : list?.sessions ?? list?.result ?? [];
  if (arr[0]?.sessionId) return arr[0].sessionId;
  const created = (await c.call("session/create", {})) as any;
  return created?.sessionId ?? created?.session_id;
}

async function state(c: RpcClient, sid: string): Promise<any> {
  return (await c.call("session/state", { session_id: sid })) as any;
}

/** Poll until the machine is running AND has advanced past `minCyc` (booted), or
 *  give up after `timeoutMs`. Returns the final state. Under --stream the daemon's
 *  stream loop is the driver; this just waits for it to reach a live idle. */
async function waitRunningBooted(c: RpcClient, sid: string, minCyc: number, timeoutMs: number): Promise<any> {
  const deadline = Date.now() + timeoutMs;
  let st = await state(c, sid);
  while (Date.now() < deadline) {
    st = await state(c, sid);
    const cyc = st.c64Cycles ?? st.cycles ?? st.cpu?.cycles ?? 0;
    if (st.runState === "running" && cyc >= minCyc) return st;
    await sleep(200);
  }
  return st;
}

// ─────────────────────────────────────────────────────────────────────────────
// CASES
// ─────────────────────────────────────────────────────────────────────────────
interface ConfCase {
  id: string;
  severity: "P0" | "P1" | "P2";
  title: string;
  /** How to spawn both daemons for this case. */
  spawn?: SpawnOpts;
  /** Extract a JSON-able behavioural signal from one (already-connected) daemon. */
  signal: (c: RpcClient, d: Daemon) => Promise<unknown>;
  /** When set, the case is SKIPPED by the default suite and reported as BLOCKED
   *  (neither GREEN nor RED — it does not gate). Used when the divergence is real and
   *  the TRX64 fix is verified out-of-band, but the TS AUTHORITY cannot report the
   *  comparison signal under THIS oracle harness (e.g. a TS query method that awaits a
   *  worker thread which is non-functional under tsx-from-src). The `signal` is kept
   *  intact so the case re-arms automatically once the harness limitation is lifted —
   *  run it explicitly with `--only <id> --include-blocked`. */
  blocked?: string;
}

const SCRAMBLE_D64 = (() => {
  try { return readFileSync(`${SAMPLES}/scramble_infinity.d64`); } catch { return Buffer.alloc(0); }
})();

// A minimal, valid EasyFlash .crt (hw type 0x20, EXROM=1/GAME=0 = ultimax boot,
// 1 × 16K CHIP at bank 0 / load $8000). There is NO .crt in the samples corpus, so a
// CRT case generates one here. EasyFlash is a WRITABLE mapper (AM29F040B flash), so
// both daemons treat it as a real live attach + writable-flash target. Mirrors the
// TRX64 `build_crt_for_test(32, 1, 0, ...)` daemon-test layout: 64-byte header
// ("C64 CARTRIDGE   " + headerLen 0x40 + ver 0x0100 + hw + exrom + game + 6 rsvd +
// 32-byte name), then "CHIP" packets (packetLen 0x10+data, type 0, bank, load, size).
function makeEasyFlashCrt(name = "EF"): Buffer {
  const hdr = Buffer.alloc(0x40);
  hdr.write("C64 CARTRIDGE   ", 0, "ascii");
  hdr.writeUInt32BE(0x40, 0x10);   // header length
  hdr.writeUInt16BE(0x0100, 0x14); // version
  hdr.writeUInt16BE(32, 0x16);     // hardware type = 0x20 = EasyFlash
  hdr.writeUInt8(1, 0x18);         // EXROM (1 = inactive at power; EasyFlash boots ultimax)
  hdr.writeUInt8(0, 0x19);         // GAME  (0)
  hdr.write(name, 0x20, "ascii");  // 32-byte cartridge name
  // Bank 0: 16K of erased flash (0xFF) + a ROMH reset vector ($8000) so a boot is sane.
  const bank0 = Buffer.alloc(0x4000, 0xff);
  bank0[0x3ffc] = 0x00; bank0[0x3ffd] = 0x80;
  const chip = Buffer.alloc(0x10);
  chip.write("CHIP", 0, "ascii");
  chip.writeUInt32BE(0x10 + bank0.length, 4); // packet length = header + data
  chip.writeUInt16BE(0, 8);                    // chip type 0 = ROM/flash
  chip.writeUInt16BE(0, 10);                   // bank 0
  chip.writeUInt16BE(0x8000, 12);              // load address
  chip.writeUInt16BE(bank0.length, 14);        // ROM image size
  return Buffer.concat([hdr, chip, bank0]);
}
const EASYFLASH_CRT = makeEasyFlashCrt();

const SCRAMBLE_D64_B = (() => {
  // A SECOND seed disk for the recents-ordering case (mount A then B → B newest).
  // Reuse the scramble image bytes under a different name; the recents store keys on
  // the (distinct) path, so identical bytes are fine — only the basename/order matter.
  return SCRAMBLE_D64;
})();

// ── c64re-own VSF module reader ───────────────────────────────────────────────
// Both daemons write the c64re-own compact VSF framing (session-vsf.ts / vsf.rs):
//   file header = "VICE Snapshot File\x1A" (19) + major + minor + null-term machine
//   per module = null-terminated name + major + minor + 4-byte LE data length + data
// Returns the DATA length (excluding the module header) of the named module, or -1
// when absent / on a parse error. Used to read the DRIVECPU module's byte length back
// off the saved file (no per-module length is exposed over the WS reply).
function vsfModuleDataLen(buf: Buffer, want: string): number {
  const MAGIC = "VICE Snapshot File\x1a"; // 19 bytes
  if (buf.length < MAGIC.length + 2 || buf.toString("latin1", 0, MAGIC.length) !== MAGIC) return -1;
  // Skip magic (19) + major (1) + minor (1); machine name is null-terminated.
  let cur = MAGIC.length + 2;
  const nameEnd = buf.indexOf(0x00, cur);
  if (nameEnd < 0) return -1;
  cur = nameEnd + 1; // past the machine-name null
  while (cur < buf.length) {
    const nul = buf.indexOf(0x00, cur);
    if (nul < 0) break;
    const name = buf.toString("latin1", cur, nul);
    cur = nul + 1;
    if (cur + 6 > buf.length) break; // major + minor + 4-byte length
    cur += 2; // major, minor
    const len = buf.readUInt32LE(cur);
    cur += 4;
    if (cur + len > buf.length) break;
    if (name === want) return len;
    cur += len;
  }
  return -1;
}

const CASES: ConfCase[] = [
  // ── P0: ws-session-debug-0 — free-run breakpoint under --stream ────────────
  // Set a breakpoint on the BASIC idle loop ($E5CD, hit every iteration) while the
  // --stream loop is the live driver. TS gates breakpoints in its per-frame tick,
  // so the machine HALTS + fires debug/breakpoint_hit|stopped + runState→paused.
  // TRX64's stream loop checks nothing → never halts. (Audit P0 ws-session-debug-0.)
  {
    id: "ws-session-debug-0",
    severity: "P0",
    title: "free-run breakpoint under --stream halts the machine",
    spawn: { stream: true },
    async signal(c) {
      const sid = await liveSession(c);
      // --stream does NOT auto-run on either runtime; debug/run starts the driver.
      await c.call("debug/run", { session_id: sid });
      // Poll until the machine has booted to the BASIC idle loop (cyc ≥ 2.5M): only
      // then are IRQs enabled and $EA31 (KERNAL IRQ handler) firing every frame, so
      // the breakpoint is guaranteed reachable by the continuous driver. (TS oracle
      // daemon is tsx-from-src ≈ 4fps, so this is ~25 s of wall time — bounded 45 s.)
      let st = await state(c, sid);
      const deadline = Date.now() + 45_000;
      while (Date.now() < deadline) {
        st = await state(c, sid);
        if ((st.c64Cycles ?? st.cycles ?? st.cpu?.cycles ?? 0) >= 2_500_000) break;
        await sleep(500);
      }
      // A one-shot debug/run may have left TRX64 paused at the budget; re-arm the
      // continuous driver so the bp test exercises the free-run path, not a one-shot.
      if (st.runState !== "running") await c.call("debug/run", { session_id: sid });
      const sink = collectNotes(c);
      await c.call("debug/break_add", { session_id: sid, pc: 0xea31 });
      await sleep(4000); // continuous driver must hit $EA31 + halt
      st = await state(c, sid);
      sink.off();
      return {
        halted: st.runState === "paused",
        firedHaltBroadcast: sink.notes.some(
          (n) => n.method === "debug/breakpoint_hit" || n.method === "debug/stopped" || n.method === "debug/paused",
        ),
      };
    },
  },

  // ── P0: ws-media-0 — disk mount routes through the ingress boundary ─────────
  // media/mount a disk. TS routes through the ingress service: captures a
  // before/after checkpoint (so the media event is replayable) and tops media/
  // recent with the mounted disk. TRX64 attaches the disk directly → null
  // checkpoint ids + recents untouched (and, downstream, silent outgoing-disk
  // write loss on the next swap). (Audit P0 ws-media-0.)
  {
    id: "ws-media-0",
    severity: "P0",
    title: "disk mount routes through the ingress boundary (checkpoint + recents)",
    spawn: { seedFiles: [{ rel: "fixtureA.d64", bytes: SCRAMBLE_D64 }] },
    async signal(c, d) {
      const sid = await liveSession(c);
      const diskPath = `${d.projectDir}/fixtureA.d64`;
      const mountResp = (await c.call("media/mount", { session_id: sid, path: diskPath, slot: 8 })) as any;
      const recent = (await c.call("media/recent", {})) as any;
      const recentArr: any[] = Array.isArray(recent) ? recent : recent?.recent ?? recent?.result ?? [];
      const norm = (p: string) => (p ? p.split("/").pop() : p); // basename — path roots differ by design
      // Ingress captures a before/after checkpoint and embeds the id in the mount
      // event (TS: event.checkpointAfterId = "cp_0_0"); a direct attach has none.
      const cpId = mountResp?.event?.checkpointAfterId ?? mountResp?.event?.checkpointBeforeId ?? null;
      return {
        // Boolean: was the mount routed through the checkpointing ingress at all?
        mountCapturedCheckpoint: cpId != null,
        // The just-mounted disk must appear in recents (ingress addRecent).
        recentIncludesMounted: recentArr.some((r) => norm(r?.path) === "fixtureA.d64"),
      };
    },
  },

  // ── P1: ws-media-1 — CRT mount routes through the ingress boundary ──────────
  // (Audit theme T3.) media/mount a .crt. TS's adaptMount routes a CRT through the
  // SAME ingress as a disk (ingestMedia kind:crt, ws-server.ts:1776-1789): it
  // captures a before/after checkpoint so the cart attach is replayable. P0-B
  // (1f533ee) routed the TRX64 CRT mount/swap branches through the ingress too; this
  // case is the differential REGRESSION GUARD. Signal: mount the generated EasyFlash
  // .crt and report whether the mount event carries a non-null checkpoint id (the
  // tell that it went through the checkpointing ingress, not a bare attach).
  // BOTH runtimes: mountCapturedCheckpoint=true. (Audit ws-media-1.)
  {
    id: "ws-media-1",
    severity: "P1",
    title: "CRT mount routes through the ingress boundary (checkpoint captured)",
    spawn: { seedFiles: [{ rel: "fixture.crt", bytes: EASYFLASH_CRT }] },
    async signal(c, d) {
      const sid = await liveSession(c);
      const crtPath = `${d.projectDir}/fixture.crt`;
      const mountResp = (await c.call("media/mount", { session_id: sid, path: crtPath, slot: 0 })) as any;
      // A fresh session's first medium is the experiment ROOT — only an after-
      // checkpoint is captured (before is omitted, no prior medium). So the tell is
      // checkpointAfterId, exactly as in ws-media-0.
      const cpId = mountResp?.event?.checkpointAfterId ?? mountResp?.event?.checkpointBeforeId ?? null;
      return {
        // Boolean: was the CRT mount routed through the checkpointing ingress?
        mountCapturedCheckpoint: cpId != null,
      };
    },
  },

  // ── P1: ws-media-8 — media/recent overlays the persisted recents store ──────
  // (Audit theme T3.) TS's media/recent overlays a GLOBAL persisted recents store
  // (recent-files.ts getRecent: newest-first, max 10, each carrying a `mountedAt`
  // timestamp; addRecent stamps it on every ingest) AHEAD of the dir scans
  // (ws-server.ts:1809-1887). TRX64's scan_recent_media was project+samples dir scan
  // ONLY (alphabetical, no store, no mountedAt). Fix: maintain a recents store updated
  // on every mount (newest-first, cap 10, mountedAt), overlaid ahead of the dir scan,
  // 1:1 with recent-files.ts. Signal: mount disk A then disk B (two seed disks), then
  // read media/recent — assert the FIRST entry is the most-recently-mounted (B) and
  // entries carry a mountedAt field. TS: {topIsNewest:true, hasMountedAt:true}; TRX64
  // (before fix): {false,false}. C64RE_RECENT_FILE points at a per-daemon temp store so
  // neither runtime touches the user's real recents (and the two daemons can't share).
  {
    id: "ws-media-8",
    severity: "P1",
    title: "media/recent overlays the persisted recents store (newest-first + mountedAt)",
    spawn: {
      seedFiles: [
        { rel: "diskA.d64", bytes: SCRAMBLE_D64 },
        { rel: "diskB.d64", bytes: SCRAMBLE_D64_B },
      ],
      // Isolate the global recents store per daemon (env applied to BOTH kinds) so
      // the user's real ~/.config/c64re/recent-media.json is never read or written,
      // and each daemon starts from an empty store.
      env: { C64RE_RECENT_FILE: `/tmp/trx64-oracle-recent-${process.pid}-${Date.now()}.json` },
    },
    async signal(c, d) {
      const sid = await liveSession(c);
      // Mount A first, then B — B is the most-recently-mounted, so it must top recents.
      await c.call("media/mount", { session_id: sid, path: `${d.projectDir}/diskA.d64`, slot: 8 });
      await c.call("media/mount", { session_id: sid, path: `${d.projectDir}/diskB.d64`, slot: 8 });
      const recent = (await c.call("media/recent", {})) as any;
      const arr: any[] = Array.isArray(recent) ? recent : recent?.recent ?? recent?.result ?? [];
      const norm = (p: string) => (p ? p.split("/").pop() : p);
      return {
        // The most-recently-mounted disk (B) must be the FIRST recents entry.
        topIsNewest: arr.length > 0 && norm(arr[0]?.path) === "diskB.d64",
        // Every store-sourced entry carries a mountedAt timestamp (recent-files.ts).
        hasMountedAt: arr.length > 0 && typeof arr[0]?.mountedAt === "string" && arr[0].mountedAt.length > 0,
      };
    },
  },

  // ── P1: ws-media-14 — cart eject resumes with the LIVE pacing, not pal/1 ─────
  // (Audit theme T3.) A cartridge eject is a power-cycle that ends RUNNING. TS routes
  // it through the ingress (checkpoint before/after) and resumes via ctrl.run() with
  // the LIVE pacing — run() broadcasts debug/running carrying `this.pacing`, which is
  // whatever set_pacing last selected (e.g. "warp"); ws-server.ts:1799-1807 →
  // ingress.ts eject + run(). TRX64's cart eject (main.rs media/unmount cart branch)
  // resumed running but broadcast a HARDCODED {mode:"pal",ratio:1}. Fix: broadcast the
  // live st.pacing_mode/ratio. Signal: mount a cart, set_pacing warp, collectNotes,
  // media/unmount the cart, and read {ejectCheckpoint: event.checkpointAfterId != null,
  // resumePacing: the debug/running broadcast's pacing.mode}. TS: {true,"warp"}; TRX64
  // (before fix): {true,"pal"} (the checkpoint half is the P0-B regression guard).
  {
    id: "ws-media-14",
    severity: "P1",
    title: "cart eject resumes with the live pacing (not hardcoded pal/1) + checkpoints",
    spawn: { seedFiles: [{ rel: "fixture.crt", bytes: EASYFLASH_CRT }] },
    async signal(c, d) {
      const sid = await liveSession(c);
      const crtPath = `${d.projectDir}/fixture.crt`;
      await c.call("media/mount", { session_id: sid, path: crtPath, slot: 0 });
      // Select WARP pacing — the eject must resume at THIS pace, not reset to pal.
      await c.call("session/set_pacing", { session_id: sid, mode: "warp" });
      const sink = collectNotes(c);
      const ejectResp = (await c.call("media/unmount", { session_id: sid, slot: 0 })) as any;
      await sleep(300); // let the debug/running broadcast land
      sink.off();
      // The cart-eject power-cycle resumes via a debug/running broadcast carrying the
      // live pacing (the LAST debug/running pushed during the eject window).
      const running = sink.notes.filter((n) => n.method === "debug/running");
      const lastRunning = running[running.length - 1]?.params as any;
      const cpId = ejectResp?.event?.checkpointAfterId ?? ejectResp?.event?.checkpointBeforeId ?? null;
      return {
        // Regression guard (P0-B): the eject is checkpointed (replayable).
        ejectCheckpoint: cpId != null,
        // The behavioural signal: the resume keeps the live pacing (warp), not pal.
        resumePacing: lastRunning?.pacing?.mode ?? null,
      };
    },
  },

  // ── P1: ws-media-3 + background-workers-async-10 — cart auto-persist fires ───
  // while PAUSED (wall-clock cadence, not run-state-gated). (Audit theme T3.) TS's
  // cart auto-persist runs on an INDEPENDENT 1 s setInterval (runtime-controller.ts:
  // 219-226 → maybeAutoPersistCart) that fires regardless of run-state, with a
  // WALL-CLOCK debounce (Date.now() - settleAt ≥ CART_AUTOPERSIST_DEBOUNCE_MS). So a
  // flash delta then pause/JAM/bp before the debounce STILL reaches the host .crt.
  // TRX64 drove the persist ONLY from the stream loop's `if running` block on a FRAME
  // counter (frame_seq advances only while running), so a dirty-then-pause never
  // persisted. Fix: drive cart (+disk) auto-persist from a wall-clock cadence that
  // fires regardless of run-state.
  //
  // BLOCKED by the oracle harness (NOT a TRX64 defect): to make the signal differential
  // the case would have to DIRTY the cart flash through the WS surface (an AM29F040B
  // AA/55/A0/<addr,data> program sequence routed through the cart mapper's write path),
  // which no JSON WS method exposes — mem/poke does not reach the mapper write the way
  // the running CPU does, and running real flash-programming code under the ~4 fps tsx
  // oracle to a settled+paused state is far heavier than the 240 s gate budget. The fix
  // is verified DIRECTLY on TRX64 (`ws_media_3_cart_autopersist_fires_while_paused`,
  // main.rs tests): mount a writable EasyFlash, drive a real byte-program (dirty), PAUSE
  // the machine (running=false), tick the wall-clock persist cadence past the debounce,
  // and assert the host .crt FILE bytes changed — proving the persist no longer depends
  // on the run-state. Re-arm if a WS method to drive a cart-mapper write (or a synthetic
  // "dirty flash" hook) lands in the oracle.
  {
    id: "ws-media-3",
    severity: "P1",
    title: "cart flash auto-persist fires while PAUSED (wall-clock cadence, not if-running)",
    blocked:
      "Dirtying cart flash needs an AM29F040B program sequence through the mapper write " +
      "path, which no JSON WS method exposes (mem/poke doesn't reach it) and which is far " +
      "heavier than the gate budget under the ~4fps tsx oracle. Fix verified DIRECTLY on " +
      "TRX64: ws_media_3_cart_autopersist_fires_while_paused (dirty flash → PAUSE → tick " +
      "wall-clock persist cadence past the debounce → host .crt FILE bytes changed).",
    spawn: { stream: true, seedFiles: [{ rel: "fixture.crt", bytes: EASYFLASH_CRT }] },
    async signal(c, d) {
      // Kept intact so the case re-arms once a cart-write WS method exists. This proxy
      // can only observe the host file size (not a faithful dirty→paused→persist), so
      // it is NOT a faithful signal — see `blocked`.
      const sid = await liveSession(c);
      const crtPath = `${d.projectDir}/fixture.crt`;
      await c.call("media/mount", { session_id: sid, path: crtPath, slot: 0 });
      await c.call("debug/pause", { session_id: sid }).catch(() => undefined);
      await sleep(500);
      return { hostCrtPresent: existsSync(crtPath) };
    },
  },

  // ── P1: ws-session-debug-5 — break_* entries carry `addr`, not `pc` ─────────
  // TS uniformly keys a breakpoint entry by `addr` (runtime-controller.ts
  // listBreakpoints → {num, addr}; ws-server.ts break_add/del/list echo it). TRX64
  // emitted `{num, pc}`, so a UI/LLM reading `entry.addr` saw undefined. The signal
  // reads the breakpoints array returned by break_add AND break_list and reports
  // which key the entry actually carries. TS: hasAddr=true,hasPc=false. TRX64
  // (before fix): hasAddr=false,hasPc=true. (Audit P1 ws-session-debug-5.)
  {
    id: "ws-session-debug-5",
    severity: "P1",
    title: "break_add/del/list entries key on `addr` (not `pc`)",
    async signal(c) {
      const sid = await liveSession(c);
      const added = (await c.call("debug/break_add", { session_id: sid, pc: 0xea31 })) as any;
      const listed = (await c.call("debug/break_list", { session_id: sid })) as any;
      const fromAdd: any[] = added?.breakpoints ?? [];
      const fromList: any[] = listed?.breakpoints ?? [];
      const entry = fromList[0] ?? fromAdd[0] ?? {};
      return {
        // The load-bearing field: the entry must expose its address as `addr`.
        addEntryHasAddr: fromAdd.length > 0 && fromAdd.every((e) => "addr" in e),
        listEntryHasAddr: fromList.length > 0 && fromList.every((e) => "addr" in e),
        // …and must NOT leak the legacy `pc` key (TS never emits it here).
        entryHasPc: "pc" in entry,
        // The address value itself must survive under `addr`.
        addrValue: entry.addr ?? null,
      };
    },
  },

  // ── P1: ws-media-2 — disk eject reports the REAL run-state ──────────────────
  // A disk eject is a live device op (the C64 keeps running). TS's ingress reports
  // paused = (runState === "paused"), so a running machine ejecting a disk returns
  // paused:false. TRX64 hardcoded paused:!is_cart = true for every disk eject. The
  // signal mounts a disk, runs to booted, ejects, and reads the unmount `paused`.
  // TS: false. TRX64 (before fix): true. (Audit P1 ws-media-2.)
  {
    id: "ws-media-2",
    severity: "P1",
    title: "disk eject reports real run-state (paused:false while running)",
    spawn: { stream: true, seedFiles: [{ rel: "fixtureA.d64", bytes: SCRAMBLE_D64 }] },
    async signal(c, d) {
      const sid = await liveSession(c);
      const diskPath = `${d.projectDir}/fixtureA.d64`;
      await c.call("media/mount", { session_id: sid, path: diskPath, slot: 8 });
      // --stream does NOT auto-run; debug/run starts the live driver.
      await c.call("debug/run", { session_id: sid });
      // Wait until the machine is genuinely running + booted past the IRQ-on point
      // so the eject happens while running (the divergence is run-state-dependent).
      const st = await waitRunningBooted(c, sid, 2_500_000, 60_000);
      if (st.runState !== "running") await c.call("debug/run", { session_id: sid });
      const ejectResp = (await c.call("media/unmount", { session_id: sid, slot: 8 })) as any;
      return {
        // The behavioural signal: a disk eject on a RUNNING machine is never paused.
        ejectPaused: ejectResp?.paused === true,
      };
    },
  },

  // ── P1: ws-session-debug-12 — cold reset clears the checkpoint ring ─────────
  // A cold power-cycle is a new machine: the ring's anchors belong to the OLD
  // timeline, so TS drops the ring on resetCold (ws-server.ts → checkpointRing
  // .clear()). TRX64's cold reset path left the ring populated. The signal captures
  // two checkpoints, asserts the ring is non-empty, cold-resets, then lists the ring
  // count. TS: 0. TRX64 (before fix): 2. (Audit P1 ws-session-debug-12.)
  {
    id: "ws-session-debug-12",
    severity: "P1",
    title: "session/reset {mode:cold} clears the checkpoint ring",
    async signal(c) {
      const sid = await liveSession(c);
      await c.call("checkpoint/capture", { session_id: sid });
      await c.call("checkpoint/capture", { session_id: sid });
      const before = (await c.call("checkpoint/list", { session_id: sid })) as any;
      const beforeCount = (before?.checkpoints ?? []).length;
      await c.call("session/reset", { session_id: sid, mode: "cold" });
      const after = (await c.call("checkpoint/list", { session_id: sid })) as any;
      const afterCount = (after?.checkpoints ?? []).length;
      return {
        // Pre-condition (both runtimes accumulate ≥1 anchor) — guards a false green.
        hadCheckpoints: beforeCount > 0,
        // The behavioural signal: the cold reset must leave the ring empty.
        ringEmptyAfterColdReset: afterCount === 0,
      };
    },
  },

  // ── P1: ws-trace-monitor-misc-18 — runtime/mark requires an active trace ─────
  // TS throws if no trace is active (you cannot stamp a marker into a stream that
  // isn't recording); only with an active trace does it record + return the real
  // mark count. TRX64 returned ok with a fabricated count regardless. The signal
  // calls runtime/mark with NO active trace and reports whether it succeeded. TS:
  // ok=false (error). TRX64 (before fix): ok=true. (Audit P1 ws-trace-monitor-misc-18.)
  {
    id: "ws-trace-monitor-misc-18",
    severity: "P1",
    title: "runtime/mark errors when no trace is active",
    async signal(c) {
      const sid = await liveSession(c);
      let ok = false;
      let marks: unknown = null;
      try {
        const r = (await c.call("runtime/mark", { session_id: sid, label: "probe" })) as any;
        ok = true;
        marks = r?.marks ?? null;
      } catch {
        ok = false;
      }
      return {
        // The behavioural signal: marking an inactive trace must NOT succeed.
        markSucceededWithoutTrace: ok,
      };
    },
  },

  // ── P1: formats-state-6 — `sid` trace domain enables ONLY the sid channel ────
  // TS maps `sid` → the `sid` channel alone (no live producer → empty stream), so
  // ADDING `sid` to a trace's domains contributes ZERO extra events. TRX64's `sid`
  // domain wrongly also flipped on cpu+mem, so adding `sid` to a cpu trace inflated
  // the event count with all RAM/IO writes. A bare ["sid"] trace is an invalid
  // definition (no triggers/captures) that TS rejects up front, so the discriminator
  // is differential: run a ["c64-cpu"] trace (cpu only) and a ["c64-cpu","sid"] trace
  // over the same cycle budget, and report whether the `sid` domain inflated the
  // count. TS: false (sid adds nothing). TRX64 (before fix): true (sid → +mem).
  // (Audit P1 formats-state-6.)
  {
    id: "formats-state-6",
    severity: "P1",
    title: "trace `sid` domain enables only sid (adds no cpu/mem events)",
    async signal(c) {
      const sid = await liveSession(c);
      const runTrace = async (domains: string[]): Promise<number> => {
        await c.call("trace/start_domains", { session_id: sid, domains });
        await c.call("session/run", { session_id: sid, cycles: 300_000 });
        const status = (await c.call("trace/run/status", { session_id: sid })) as any;
        const n = Number(status?.eventCount ?? 0);
        await c.call("trace/run/stop", { session_id: sid }).catch(() => undefined);
        return n;
      };
      const cpuOnly = await runTrace(["c64-cpu"]);
      const cpuPlusSid = await runTrace(["c64-cpu", "sid"]);
      return {
        // The behavioural signal: adding the `sid` domain must NOT inflate the
        // event count (no mem/cpu co-enable). >1.5× = the sid→cpu+mem leak fired.
        sidDomainInflatesEvents: cpuOnly > 0 && cpuPlusSid > cpuOnly * 1.5,
      };
    },
  },

  // ── P1: streaming-av-5 — session/frame_available JSON notification per frame ─
  // TS pushes a lightweight `session/frame_available` JSON notification on every
  // presented frame (alongside the binary VIC frame), for metadata-only consumers.
  // TRX64's stream loop pushed only the binary VIC frame; the NotifyHub was never
  // called. The signal spawns --stream, runs, collects notifications ~3s, and counts
  // the frame_available pushes. TS: > 0. TRX64 (before fix): 0. (Audit P1 streaming-av-5.)
  {
    id: "streaming-av-5",
    severity: "P1",
    title: "session/frame_available JSON notification emitted per presented frame",
    spawn: { stream: true },
    async signal(c) {
      const sid = await liveSession(c);
      const sink = collectNotes(c);
      await c.call("debug/run", { session_id: sid });
      await sleep(3000); // the running stream loop presents frames continuously
      sink.off();
      const frameNotes = sink.notes.filter((n) => n.method === "session/frame_available");
      const first = frameNotes[0]?.params as any;
      return {
        // The behavioural signal: at least one frame_available notification arrived.
        gotFrameAvailable: frameNotes.length > 0,
        // …and it carries the TS payload shape ({session_id, frame, c64Cycles}).
        hasPayloadShape:
          first != null &&
          "session_id" in first &&
          "frame" in first &&
          "c64Cycles" in first,
      };
    },
  },

  // ── P1: broadcasts-1 — JAM auto-break under --stream (regression guard) ──────
  // A KIL/JAM illegal opcode (0x02) jams the CPU: clk keeps cycling but PC is
  // frozen (VICE-faithful), so a free-running advance never aborts on it. TS's
  // per-frame tick detects the jammed state and HALTS (runState→paused) +
  // server-PUSHes debug/stopped with reason "jam" (Spec 764, runtime-controller.ts
  // :791-807). P0-A (926a399) lifted that detection into TRX64's stream loop
  // (stream_debug_gated_advance, main.rs:1143-1171); this case is the regression
  // guard. Load a 1-byte PRG `[$02]` at $1000 and run it (PC=$1000) under the
  // continuous --stream driver, then read the run-state + the pushed stop reason.
  // BOTH runtimes: jammed=true, reason "jam", broadcastReasonJam=true.
  {
    id: "broadcasts-1",
    severity: "P1",
    title: "JAM (KIL) auto-break halts + pushes debug/stopped reason=jam under --stream",
    spawn: { stream: true },
    async signal(c) {
      const sid = await liveSession(c);
      const sink = collectNotes(c);
      // Load `[$02]` (KIL) at $1000 and run from there. run_prg pause→setPC=$1000
      // →continue, and under --stream the continuous loop is the live driver.
      // bytes_b64 = base64([0x00,0x10, 0x02]) = the 2-byte load addr $1000 + the KIL.
      const prgB64 = Buffer.from([0x00, 0x10, 0x02]).toString("base64");
      await c.call("runtime/run_prg", { session_id: sid, bytes_b64: prgB64, run: 0x1000 });
      // The driver executes the KIL within a frame; give it a few frames to halt.
      await sleep(4000);
      const st = await state(c, sid);
      sink.off();
      const jamStop = sink.notes.find(
        (n) => n.method === "debug/stopped" && (n.params?.stop?.reason === "jam"),
      );
      return {
        // A jammed CPU makes no progress — the machine must be paused.
        jammed: st.runState === "paused",
        // …and the stop reason carried over the debug/stopped push must be "jam".
        broadcastReasonJam: jamStop != null,
        // The stop reason value itself (load-bearing — must read "jam" on both).
        reason: (jamStop?.params as any)?.stop?.reason ?? null,
      };
    },
  },

  // ── P1: background-workers-async-5 — trace firehose fed during free-run ───────
  // TS's tick() drains the active trace once per completed frame, so its binary
  // writer keeps appending to the `.c64retrace` authority while the machine free-runs
  // (runtime-controller.ts:869-874). TRX64's stream loop advanced with a NullSink +
  // a no-op trace path, so a trace started DURING a --stream free-run recorded
  // nothing. The signal starts the continuous driver, starts a cpu+memory trace to an
  // explicit `.c64retrace` path under the project dir, free-runs ~1M+ cycles, stops
  // (finalizes the log), decodes the file and counts events. TS: many. TRX64 (before
  // wiring run_cycle_budget into the free-run advance): ~0. The signal is the
  // CROSSED-THRESHOLD boolean (TS 50/PAL-frame vs TRX64 25/PAL-frame cadences differ,
  // and the TS oracle is ~4 fps, so absolute counts diverge by design — both runtimes
  // must simply produce a substantial, non-empty trace). (Audit P1 background-workers-async-5.)
  {
    id: "background-workers-async-5",
    severity: "P1",
    title: "live trace is fed every frame while free-running under --stream",
    spawn: { stream: true },
    async signal(c, d) {
      const sid = await liveSession(c);
      const duckdbPath = `${d.projectDir}/livetrace.duckdb`;
      const retracePath = `${d.projectDir}/livetrace.c64retrace`;
      // Start the continuous --stream driver, then wait until it has booted a bit so
      // the trace window captures live CPU/memory activity (not a cold idle).
      await c.call("debug/run", { session_id: sid });
      await waitRunningBooted(c, sid, 1_500_000, 60_000);
      // Start the trace the SAME way the WS does (trace/start_domains), to an explicit
      // .c64retrace path so the case can read it back on either runtime.
      await c.call("trace/start_domains", {
        session_id: sid,
        domains: ["c64-cpu", "memory"],
        output: duckdbPath,
      });
      // Free-run a window so the per-frame drain feeds the firehose. The TS oracle is
      // ~4 fps (≈80k cyc/frame), so ~25 wall-seconds is ~1M+ cycles of free-run.
      const cycStart = (await state(c, sid)).c64Cycles ?? 0;
      const deadline = Date.now() + 40_000;
      while (Date.now() < deadline) {
        await sleep(2000);
        const cyc = (await state(c, sid)).c64Cycles ?? 0;
        if (cyc - cycStart >= 1_000_000) break;
      }
      // Stop the trace → finalizes the .c64retrace (TS awaits the writer; TRX64 writes
      // the buffer synchronously). wait_index:false so we don't block on the DuckDB index.
      await c.call("trace/run/stop", { session_id: sid, wait_index: false }).catch(() => undefined);
      // The writer finalize may land a beat after the RPC resolves (TS worker); give it
      // a short grace + retry the read.
      let events = 0;
      for (let i = 0; i < 5; i++) {
        if (existsSync(retracePath)) {
          try {
            const buf = readFileSync(retracePath);
            events = decodeTrace(buf).records.length;
            if (events > 0) break;
          } catch { /* partial flush — retry */ }
        }
        await sleep(500);
      }
      return {
        // The behavioural signal: a trace started DURING free-run captured a
        // substantial event stream (not the empty log of a NullSink advance).
        traceCapturedDuringFreeRun: events > 100,
      };
    },
  },

  // ── P1: background-workers-async-0 — recorder auto-fed during free-run ────────
  // TS's tick() feeds the active recorder one omitMedia anchor at the per-second
  // auto-capture cadence (runtime-controller.ts:846-852), so a FREE-RUNNING machine
  // grows recorder anchors over time WITHOUT any explicit capture. TRX64's stream
  // loop fed only the checkpoint ring; the recorder advanced only on an explicit
  // recorder/capture (main.rs recorder/capture handler), so a --stream free-run
  // left it frozen. The recorder is default-OFF on TS (opt-in C64RE_RECORDER=1, set
  // on BOTH daemons via spawn.env); TRX64 needs an explicit recorder/start (TS has
  // none — its recorder is created by run() — so the call is best-effort). The signal
  // takes a baseline anchor count, free-runs, polls for growth, and reports whether
  // the count INCREASED over the window. TS: true (auto-fed). TRX64 (before fix):
  // false (flat). (Audit P1 background-workers-async-0 + ws-checkpoint-scrub-7.)
  {
    id: "background-workers-async-0",
    severity: "P1",
    title: "recorder auto-fed each cadence while free-running under --stream",
    // BLOCKED by the TS oracle harness, NOT by a TRX64 defect. The TS recorder's
    // every query method (recorder/list + recorder/status) does `await c.recorder
    // .list()/.stats()`, which round-trips to a node:worker_threads worker resolved
    // at WORKER_PATH = `${dirname(import.meta.url)}/recorder-worker.js`. Under the
    // tsx-from-src oracle daemon, import.meta.url is the SRC `.ts` dir — where only
    // recorder-worker.ts exists (the built .js lives under dist/) — so the worker
    // fails to load, its error is swallowed, and the query promise NEVER resolves
    // (rpc timeout). So the TS authority cannot report the recorder anchor count here.
    // The fix IS verified directly on TRX64 (in-process recorder, no worker): under
    // --stream the anchor count grows 1→3→6→8→11→13→16 over 12 s of free-run
    // (was flat at 1 before stream_maybe_feed_recorder, main.rs). Re-arm once the TS
    // recorder worker resolves under tsx (or the oracle runs the built TS daemon).
    blocked:
      "TS recorder/list|status awaits a worker thread that is non-functional under " +
      "tsx-from-src (recorder-worker.js resolves to the src .ts dir). Fix verified " +
      "directly on TRX64 (anchors grow 1→16 over 12s free-run under --stream).",
    spawn: { stream: true, env: { C64RE_RECORDER: "1" } },
    async signal(c) {
      const sid = await liveSession(c);
      // TRX64 needs an explicit recorder/start; TS auto-creates the recorder in run()
      // and has no such method → ignore the error there.
      await c.call("recorder/start", { session_id: sid }).catch(() => undefined);
      // Start the continuous --stream driver (on TS this ALSO creates the recorder).
      await c.call("debug/run", { session_id: sid });
      const listCount = async (): Promise<number> => {
        const r = (await c.call("recorder/list", { session_id: sid })) as any;
        return Array.isArray(r?.anchors) ? r.anchors.length : 0;
      };
      // Baseline AFTER the driver is live (recorder/start may have captured 1 anchor;
      // we measure GROWTH from here, so a runtime starting at 1 vs 0 doesn't matter).
      const baseline = await listCount();
      // Poll for growth. The recorder cadence is in EMULATED frames (TS 50 / TRX64 25)
      // and the TS oracle daemon emulates ~4 fps, so one TS cadence ≈ 12.5s wall —
      // poll up to ~60s for at least one fresh anchor (well under the 240s RPC cap).
      const deadline = Date.now() + 60_000;
      let latest = baseline;
      while (Date.now() < deadline) {
        await sleep(2000);
        latest = await listCount();
        if (latest > baseline) break;
      }
      return {
        // The behavioural signal: free-running grows the recorder anchor count.
        recorderGrewWhileFreeRunning: latest > baseline,
      };
    },
  },

  // ── P1: ws-checkpoint-scrub-0 — restore then="keep" inherits the run-state ────
  // checkpoint-restore is the shared, broadcast-rich path (audit theme T4). A
  // then="keep" (or omitted `then`) restore must INHERIT the prior run-state: a
  // RUNNING machine stays running (TS: runtime-controller.ts:541-552/588 — keep →
  // pause=false → runState UNCHANGED). TRX64 (before fix, main.rs:5409-5415) forced
  // running=false on any non-"run" intent, so a keep-restore of a running machine
  // wrongly PAUSED it. The signal: under --stream, run to a booted/running machine,
  // capture an anchor, restore {then:"keep"}, and read runState after. TS: "running";
  // TRX64 (before fix): "paused". (Audit ws-checkpoint-scrub-0.)
  {
    id: "ws-checkpoint-scrub-0",
    severity: "P1",
    title: 'restore then="keep" inherits the prior run-state (running stays running)',
    spawn: { stream: true },
    async signal(c) {
      const sid = await liveSession(c);
      // Start the continuous --stream driver and let it boot a bit so the machine is
      // genuinely RUNNING (not a one-shot budget that left it paused at the end).
      await c.call("debug/run", { session_id: sid });
      let st = await waitRunningBooted(c, sid, 1_500_000, 60_000);
      // Re-arm the continuous driver if a one-shot debug/run left it paused at budget.
      if (st.runState !== "running") await c.call("debug/run", { session_id: sid });
      // Capture an anchor of the live (running) state.
      const cap = (await c.call("checkpoint/capture", { session_id: sid })) as any;
      const cpId = cap?.ref?.id ?? cap?.id;
      // Restore with then omitted (≡ "keep"). A keep-restore of a RUNNING machine must
      // leave it running.
      await c.call("checkpoint/restore", { session_id: sid, id: cpId, then: "keep" });
      // Give the stream loop a beat to keep advancing if it is (still) running, then read.
      await sleep(1000);
      st = await state(c, sid);
      return {
        // The behavioural signal: a keep-restore of a running machine stays "running".
        runStateAfterKeepRestore: st.runState,
      };
    },
  },

  // ── P1: ws-checkpoint-scrub-4 — restore then="pause" pushes debug/stopped ─────
  // A then="pause" restore must server-PUSH debug/stopped (reason "pause") so a
  // passive UI freezes the run-state on the scrub (TS: runtime-controller.ts:614-617
  // stopInfo reason:"pause" + broadcast debug/stopped). TRX64 (before fix,
  // main.rs:5404-5436) pushed only audio/flush + debug/checkpoint_restored on a
  // restore — never debug/stopped. The signal: capture an anchor, collectNotes,
  // restore {then:"pause"}, count debug/stopped (reason "pause") pushes. TS: ≥1;
  // TRX64 (before fix): 0. (Audit ws-checkpoint-scrub-4.)
  {
    id: "ws-checkpoint-scrub-4",
    severity: "P1",
    title: 'restore then="pause" broadcasts debug/stopped (reason "pause")',
    spawn: { stream: true },
    async signal(c) {
      const sid = await liveSession(c);
      // No need to free-run; a fresh paused machine can capture + restore an anchor.
      const cap = (await c.call("checkpoint/capture", { session_id: sid })) as any;
      const cpId = cap?.ref?.id ?? cap?.id;
      const sink = collectNotes(c);
      await c.call("checkpoint/restore", { session_id: sid, id: cpId, then: "pause" });
      await sleep(500);
      sink.off();
      const stopped = sink.notes.filter(
        (n) => n.method === "debug/stopped" && n.params?.stop?.reason === "pause",
      );
      return {
        // The behavioural signal: a then=pause restore pushes debug/stopped(reason=pause).
        pushedDebugStoppedPause: stopped.length > 0,
      };
    },
  },

  // ── P1: ws-checkpoint-scrub-1 — restore pushes a fresh frame ──────────────────
  // BLOCKED by the oracle harness (NOT a TRX64 defect). The TS controller ALWAYS
  // presentFrame()s on restore so a paused canvas refreshes to the rolled-back
  // picture "with no client-grab dependency" (runtime-controller.ts:606-613). But
  // TS's presentFrame pushes the BINARY VIC frame ONLY (ws-server.ts:474-503
  // pushFrame); it does NOT emit the JSON `session/frame_available` on restore (that
  // is emitted only inside the RUNNING loop's maybePresentFrame, runtime-
  // controller.ts:907). So there is no faithful JSON proxy: the text ws-client
  // cannot read the binary frame, and a `session/frame_available`-count signal would
  // diverge the WRONG way (TS pushes none on restore). The TRX64 fix mirrors TS
  // exactly (a fresh BINARY frame on restore, no extra JSON) and is verified DIRECTLY
  // on TRX64: `checkpoint_restore_requests_one_shot_frame_present` (main.rs tests)
  // asserts a restore sets `force_present_frame`, which the otherwise-silent paused
  // stream loop consumes once to push exactly one BIN_VIC — the TRX64 equivalent of
  // TS's unconditional presentFrame() on restore. Re-arm if a JSON frame-content
  // method (or a binary-frame-reading ws-client) lands in the oracle.
  {
    id: "ws-checkpoint-scrub-1",
    severity: "P1",
    title: "restore pushes a fresh frame (paused canvas refreshes to the rolled-back picture)",
    blocked:
      "TS pushes a BINARY VIC frame on restore (ws-server.ts pushFrame) that the text " +
      "ws-client cannot read, and emits NO JSON session/frame_available on restore — so " +
      "there is no faithful JSON proxy. Fix verified DIRECTLY on TRX64: a restore sets " +
      "force_present_frame → the paused stream loop pushes one BIN_VIC (test " +
      "checkpoint_restore_requests_one_shot_frame_present).",
    spawn: { stream: true },
    async signal(c) {
      // Kept intact so the case re-arms once a frame-content JSON signal exists. This
      // proxy is the session/frame_available-on-restore count (which is NOT faithful —
      // see `blocked`), retained only to document the shape.
      const sid = await liveSession(c);
      const cap = (await c.call("checkpoint/capture", { session_id: sid })) as any;
      const cpId = cap?.ref?.id ?? cap?.id;
      const sink = collectNotes(c);
      await c.call("checkpoint/restore", { session_id: sid, id: cpId, then: "pause" });
      await sleep(500);
      sink.off();
      return {
        frameAvailableOnRestore: sink.notes.some((n) => n.method === "session/frame_available"),
      };
    },
  },

  // ── P1: ws-checkpoint-scrub-2 — restore honours the `render` flag ─────────────
  // BLOCKED by the oracle harness (NOT a TRX64 defect). A framebuffer-OMITTED
  // auto-anchor restored with render:true must re-sim ~1 frame to regenerate the
  // framebuffer so the paused canvas shows a picture (TS: runtime-controller.ts:
  // 544/599-601). TRX64 (before fix) never read `render` and c64re_snapshot.rs:775
  // skips the omitted fb, so the canvas stayed black/stale. The divergence is in the
  // framebuffer PIXEL content, which NO JSON method exposes (vic/inspect/* report VIC
  // REGISTERS, not pixels) and the text ws-client cannot read the binary frame — so
  // there is no faithful JSON proxy on either runtime. The TRX64 fix (honour
  // render:true → re-sim one PAL frame after the state restore) is verified DIRECTLY
  // on TRX64: `checkpoint_restore_render_regenerates_omitted_framebuffer` (main.rs
  // tests) boots ROMs, paints a real screen, omits the anchor framebuffer, and
  // asserts that a render:true restore REGENERATES the live `displayed` buffer (the
  // stamped sentinel is overwritten) while a no-render restore leaves it stale.
  {
    id: "ws-checkpoint-scrub-2",
    severity: "P1",
    title: "restore honours render:true (re-sims a frame so a fb-omitted anchor gets a picture)",
    blocked:
      "The divergence is in framebuffer PIXEL content; no JSON method exposes it " +
      "(vic/inspect/* report VIC REGISTERS, not pixels) and the text ws-client cannot " +
      "read the binary frame — no faithful JSON proxy. Fix verified DIRECTLY on TRX64: " +
      "a render:true restore regenerates the live `displayed` buffer (test " +
      "checkpoint_restore_render_regenerates_omitted_framebuffer).",
    spawn: { stream: true },
    async signal(c) {
      // Kept intact so the case re-arms once a frame-content JSON signal exists. The
      // best available JSON read is vic/inspect (REGISTERS only) — not a faithful
      // framebuffer-content signal (see `blocked`), retained to document the intent.
      const sid = await liveSession(c);
      const cap = (await c.call("checkpoint/capture", { session_id: sid })) as any;
      const cpId = cap?.ref?.id ?? cap?.id;
      await c.call("checkpoint/restore", { session_id: sid, id: cpId, then: "pause", render: true });
      const open = (await c.call("vic/inspect/open", { session_id: sid }).catch(() => null)) as any;
      return {
        // NOT faithful: this only reflects VIC registers, not framebuffer pixels.
        inspectReturnedFrame: open?.frame != null,
      };
    },
  },

  // ── P1: ws-session-debug-1 — debug/run is async (replies running, never blocks) ─
  // TS's debug/run replies `running` IMMEDIATELY (controller.run() flips runState +
  // pushes debug/running + schedules the loop, returns ctrl.state() WITHOUT blocking;
  // ws-server.ts:985-991 + runtime-controller.ts:262-284). A later breakpoint halt is
  // PUSHED via debug/stopped from the loop — the reply itself is never the halt.
  // TRX64 (before fix) ran an INLINE synchronous run_until_break inside debug/run when
  // a breakpoint/observer was armed (run_debug_control, main.rs:903-1042 / DEBUG_RUN_
  // BUDGET=10M cyc), so debug/run BLOCKED until the bp hit (or the 10M budget) and
  // could even REPLY "paused". The fix drops the inline sync run from debug/run — it
  // sets running + broadcasts debug/running + returns 'running' immediately, and the
  // (P0-A) bp/observer/JAM-aware stream loop drives the halt + pushes debug/stopped.
  // Signal: arm a bp at an address the running code hits ($EA31 = KERNAL IRQ handler,
  // reached once IRQs are on a few hundred K cycles in), call debug/run and measure
  // {replyRunState: the reply's runState, repliedFast: did debug/run return < ~1s}.
  // TS: {running, true}; TRX64 (before fix): {paused-or-running, false (blocks)}.
  {
    id: "ws-session-debug-1",
    severity: "P1",
    title: "debug/run replies 'running' immediately and never blocks (async-scheduled)",
    spawn: { stream: true },
    async signal(c) {
      const sid = await liveSession(c);
      // Arm the bp BEFORE debug/run. Pre-fix TRX64 would then run inline until $EA31
      // is reached (the machine boots within the 10M budget), blocking the reply.
      await c.call("debug/break_add", { session_id: sid, pc: 0xea31 });
      const t0 = Date.now();
      const reply = (await c.call("debug/run", { session_id: sid })) as any;
      const elapsed = Date.now() - t0;
      return {
        // The reply's own run-state: TS reports 'running' (the loop drives the halt).
        replyRunState: reply?.runState ?? null,
        // debug/run must NOT block on the inline run — a fast (<1s) reply.
        repliedFast: elapsed < 1000,
      };
    },
  },

  // ── P1: ws-session-debug-2 — session/run is rejected while the loop owns the machine
  // session/run is a MANUAL/HEADLESS primitive only; the autonomous loop owns the
  // clock under debug/run. TS throws when runState==='running' so the two clocks can't
  // double-advance the CPU (ws-server.ts:842-848). TRX64 (before fix, main.rs:2941-
  // 2950) ran the budget unconditionally. The signal: --stream, debug/run (→running),
  // then session/run {cycles:N}, report whether session/run errored. TS: true; TRX64
  // (before fix): false. (Audit ws-session-debug-2.)
  {
    id: "ws-session-debug-2",
    severity: "P1",
    title: "session/run is rejected while the autonomous loop is running",
    spawn: { stream: true },
    async signal(c) {
      const sid = await liveSession(c);
      // debug/run is now async (replies running immediately); the loop owns the clock.
      await c.call("debug/run", { session_id: sid });
      let threw = false;
      try {
        await c.call("session/run", { session_id: sid, cycles: 10_000 });
      } catch {
        threw = true;
      }
      return {
        // The behavioural signal: a manual session/run on a running machine must error.
        threw,
      };
    },
  },

  // ── P1: ws-session-debug-3 — a paused session/run honours exec breakpoints ──────
  // A MANUAL (paused) session/run honours exec breakpoints: it step-pasts a bp it is
  // sitting on, runs the cycle budget WITH the bp set, and on a hit returns early with
  // a breakpoint{pc,num,registers} object (ws-server.ts:852-901). TRX64 (before fix)
  // ran the raw cycle budget and returned only {c64Cycles} — no halt, no breakpoint{}.
  // The signal: a PAUSED machine, set a bp at an address reached within the budget,
  // session/run {cycles: big}, report {hasBreakpoint, stoppedEarly}. We pick $EA31 and
  // boot the machine to just before IRQs are live with a debug/step-free warm-up via a
  // bounded session/run, so the bp is reachable inside the budget. TS: true/true;
  // TRX64 (before fix): false/false. (Audit ws-session-debug-3.)
  {
    id: "ws-session-debug-3",
    severity: "P1",
    title: "a manual (paused) session/run honours exec breakpoints + returns breakpoint{}",
    async signal(c) {
      const sid = await liveSession(c);
      // Machine is PAUSED (no --stream, no debug/run). Warm it past the cold reset so
      // IRQs are enabled and $EA31 (KERNAL IRQ handler) is reachable within the budget.
      // This bounded session/run runs while paused (no loop owns the clock yet).
      await c.call("session/run", { session_id: sid, cycles: 3_000_000 });
      await c.call("debug/break_add", { session_id: sid, pc: 0xea31 });
      const budget = 2_000_000;
      const r = (await c.call("session/run", { session_id: sid, cycles: budget })) as any;
      const advanced = Number(r?.c64Cycles ?? 0);
      return {
        // The reply must carry a breakpoint{} object on a hit.
        hasBreakpoint: r?.breakpoint != null && typeof r.breakpoint.pc === "number",
        // …and the run must have stopped EARLY (a bp hit, not the full budget).
        // (advanced is the absolute cycle count; a hit leaves the machine well below
        // start+budget. We re-read against the start cycle implicitly: a hit fires in
        // far fewer than `budget` cycles, so we assert the bp object presence drove it.)
        stoppedEarly: r?.breakpoint != null && advanced > 0,
      };
    },
  },

  // ── P1: ws-session-debug-4 — debug/step returns the full controller.state() shape ─
  // TS's debug/step returns c.state() = {runState,pacing,pc,cycles,frame,breakpoints,
  // stop,controlOwner} (ws-server.ts:994-1000 → runtime-controller.ts:344-363). TRX64
  // (before fix, main.rs:3545-3554) returned a FLAT register dict {runState,pc,a,x,y,
  // sp,flags,cycles} — missing pacing/frame/breakpoints/stop/controlOwner. The signal
  // reads the top-level keys of the debug/step reply as presence booleans. TS: all
  // present; TRX64 (before fix): pacing/frame/breakpoints/stop/controlOwner missing.
  // (Audit ws-session-debug-4.)
  {
    id: "ws-session-debug-4",
    severity: "P1",
    title: "debug/step returns the full controller.state() shape (not a flat register dict)",
    async signal(c) {
      const sid = await liveSession(c);
      const r = (await c.call("debug/step", { session_id: sid })) as any;
      const has = (k: string) => r != null && Object.prototype.hasOwnProperty.call(r, k);
      return {
        // The full controller.state() key set (ws-server.ts:994-1000).
        hasRunState: has("runState"),
        hasPacing: has("pacing"),
        hasPc: has("pc"),
        hasCycles: has("cycles"),
        hasFrame: has("frame"),
        hasBreakpoints: has("breakpoints"),
        hasStop: has("stop"),
        hasControlOwner: has("controlOwner"),
      };
    },
  },

  // ── P1: ws-session-debug-6 — session/create honours trace_out/trace_domains ──────
  // TS's session/create threads trace_out + trace_domains (+ device_id/pal/start_track/
  // write_protected): when trace_out is set it opens a session trace atomically via
  // startSessionTrace, so a trace is ACTIVE right after create (ws-server.ts:608-633).
  // TRX64 (before fix, main.rs:2896-2916) read NONE of these params and hardcoded
  // trace:null, so no trace ever opened on create. The signal: session/create with a
  // trace_out path + trace_domains, then trace/run/status, report whether a trace is
  // active. TS: true; TRX64 (before fix): false. (Audit ws-session-debug-6.)
  {
    id: "ws-session-debug-6",
    severity: "P1",
    title: "session/create honours trace_out/trace_domains (opens a session trace)",
    async signal(c, d) {
      const tracePath = `${d.projectDir}/create-trace.duckdb`;
      const created = (await c.call("session/create", {
        trace_out: tracePath,
        trace_domains: ["c64-cpu"],
      })) as any;
      const sid = created?.sessionId ?? created?.session_id ?? (await liveSession(c));
      const status = (await c.call("trace/run/status", { session_id: sid })) as any;
      return {
        // The behavioural signal: a trace is active immediately after the create.
        traceOpened: status?.active === true,
      };
    },
  },

  // ── P1: formats-state-1 — VSF embeds the 1541 drive snapshot (DRIVECPU module) ──
  // (Audit theme T6.) TS's saveSessionVsf embeds drive1541.snapshot() into the
  // DRIVECPU module (session-vsf.ts:116-118 — the full drive core blob: DRIVE8 +
  // DRIVECPU0 + 1541VIA1D0 + VIA2D0). TRX64's save_vsf wrote ser_drivecpu() = 0 bytes
  // (vsf.rs:219/314), so a saved VSF carried an EMPTY drive module — the 1541 state was
  // lost. Fix: serialize the live drive (drive_snapshot::capture_drive1541) into the
  // DRIVECPU module. Signal: mount a disk + run so the drive CPU is live, vsf/save to an
  // abs path, read the file back and parse the DRIVECPU module's data length. TS:
  // driveModuleNonEmpty=true; TRX64 (before fix): false.
  {
    id: "formats-state-1",
    severity: "P1",
    title: "VSF save embeds the 1541 drive snapshot (non-empty DRIVECPU module)",
    spawn: { stream: true, seedFiles: [{ rel: "fixtureA.d64", bytes: SCRAMBLE_D64 }] },
    async signal(c, d) {
      const sid = await liveSession(c);
      const diskPath = `${d.projectDir}/fixtureA.d64`;
      await c.call("media/mount", { session_id: sid, path: diskPath, slot: 8 });
      // Run so the drive CPU has booted past its reset (the DOS ROM init runs even
      // without a host LOAD — the drive CPU + VIA state is live). --stream does not
      // auto-run; debug/run starts the live driver, then we wait for boot.
      await c.call("debug/run", { session_id: sid });
      await waitRunningBooted(c, sid, 1_500_000, 60_000);
      const vsfPath = `${d.projectDir}/state.vsf`;
      await c.call("vsf/save", { session_id: sid, output_path: vsfPath });
      let driveLen = -1;
      if (existsSync(vsfPath)) {
        try { driveLen = vsfModuleDataLen(readFileSync(vsfPath), "DRIVECPU"); } catch { driveLen = -1; }
      }
      return {
        // The behavioural signal: the saved VSF's DRIVECPU module carries the drive
        // blob (non-empty), not an empty stub.
        driveModuleNonEmpty: driveLen > 0,
      };
    },
  },

  // ── P1: formats-state-2 — .c64re dump captures the cartridge flash + .crt bytes ──
  // (Audit theme T6.) TS's checkpoint capture threads the attached cartridge's original
  // .crt bytes (cartBytes = cartMedia.bytes) + its mutable flash image (cartFlash =
  // getWritableImage()) into the RuntimeCheckpoint (headless-machine-kernel.ts:988-989).
  // A writable EasyFlash's getWritableImage() returns the flash array even when clean
  // (cartridge.ts:913), so a dump of an attached EasyFlash carries BOTH non-null.
  // TRX64's capture_runtime_checkpoint hardcoded cartBytes/cartFlash = null
  // (c64re_snapshot.rs:901-902) — the cartridge flash was lost across dump/undump. Fix:
  // capture the cart bytes + writable image into the .c64re checkpoint. Signal: mount a
  // writable EasyFlash, snapshot/dump to an abs .c64re path, read the file (magic +
  // gzip(JSON)) and report whether checkpoint.cartBytes/cartFlash are non-null. TS:
  // {cartBytesCaptured:true, cartFlashCaptured:true}; TRX64 (before fix): {false,false}.
  {
    id: "formats-state-2",
    severity: "P1",
    title: ".c64re dump captures the cartridge flash + .crt bytes (not null)",
    spawn: { seedFiles: [{ rel: "fixture.crt", bytes: EASYFLASH_CRT }] },
    async signal(c, d) {
      const { gunzipSync } = await import("node:zlib");
      const sid = await liveSession(c);
      const crtPath = `${d.projectDir}/fixture.crt`;
      await c.call("media/mount", { session_id: sid, path: crtPath, slot: 0 });
      const snapPath = `${d.projectDir}/cart.c64re`;
      await c.call("snapshot/dump", { session_id: sid, path: snapPath });
      let cartBytesCaptured = false;
      let cartFlashCaptured = false;
      if (existsSync(snapPath)) {
        try {
          const raw = readFileSync(snapPath);
          // .c64re container = magic(8) + version(1) + sha256(32) + gzip(JSON.stringify(doc)).
          const doc = JSON.parse(gunzipSync(raw.subarray(41)).toString("utf8")) as any;
          const cp = doc?.checkpoint ?? {};
          cartBytesCaptured = cp.cartBytes != null;
          cartFlashCaptured = cp.cartFlash != null;
        } catch { /* parse error → stays false */ }
      }
      return {
        // The behavioural signal: the dump carries the cart's original .crt bytes…
        cartBytesCaptured,
        // …and its writable flash image (so undump restores the flash).
        cartFlashCaptured,
      };
    },
  },

  // ── P1: ws-trace-monitor-misc-23 — trace/run/stop returns the real run descriptor ──
  // (Audit theme T6.) TS's traceRun.stop() returns the real RuntimeTraceRun: the run's
  // definitionId (= def.id), cycleStart/cycleEnd, overheadMs, and marks[] (trace-run.ts
  // stop()). TRX64's finalize_trace hardcoded definitionId="live-capture" and dropped
  // cycle window / overhead / marks / media (main.rs:2280-2285). Fix: finalize_trace
  // returns the real definition id + cycleStart/cycleEnd + overheadMs + marks[] + media.
  // Signal: register a NAMED definition, start a trace with it, add a mark, run a known
  // cycle window, stop, and read the stop descriptor's {definitionId, hasCycleWindow,
  // markCount}. TS: {<the named id>, true, 1}; TRX64 (before fix): {"live-capture",
  // false, 0}.
  {
    id: "ws-trace-monitor-misc-23",
    severity: "P1",
    title: "trace/run/stop returns the real definitionId + cycle window + marks",
    async signal(c, d) {
      const sid = await liveSession(c);
      // Register a NAMED trace definition (a captureAll-shaped cpu trace). The id is
      // explicit so we can assert the stop descriptor echoes it (not "live-capture").
      const defId = "misc23-named-trace";
      const definition = {
        id: defId,
        version: 1,
        name: "misc23 named trace",
        domains: ["c64-cpu"],
        triggers: [{ kind: "pc-range", domain: "c64-cpu", from: 0, to: 0xffff }],
        captures: [{ kind: "cpu-row", domain: "c64-cpu" }],
        retention: "evidence",
        checkpointPolicy: "none",
      };
      const put = (await c.call("trace/definition/put", { session_id: sid, definition })) as any;
      const realId = put?.id ?? defId;
      const out = `${d.projectDir}/misc23.duckdb`;
      await c.call("trace/run/start", { session_id: sid, definition_id: realId, output: out });
      // Stamp one mark, then advance a known cycle window so cycleStart != cycleEnd.
      await c.call("trace/run/mark", { session_id: sid, label: "m0" });
      await c.call("session/run", { session_id: sid, cycles: 200_000 });
      const stop = (await c.call("trace/run/stop", { session_id: sid, wait_index: false })) as any;
      const run = stop?.run ?? {};
      const hasCycleWindow =
        typeof run.cycleStart === "number" &&
        typeof run.cycleEnd === "number" &&
        run.cycleEnd >= run.cycleStart;
      return {
        // The stop descriptor must echo the real definition id, not "live-capture".
        reportedDefId: run.definitionId ?? null,
        // …carry a populated cycle window…
        hasCycleWindow,
        // …and the mark we stamped (marks[] length).
        markCount: Array.isArray(run.marks) ? run.marks.length : 0,
      };
    },
  },

  // ── P1: ws-trace-monitor-misc-2 — monitor `trace on/off/status/mark` is wired ──
  // The monitor REPL (monitor/exec) advertises `trace on|off|status|mark` in its
  // `help` text, but TRX64's run_monitor had NO `trace` arm → the verb fell through
  // to `unknown command: trace` (the help LIES). TS monitor-shell.ts:413-441 drives
  // ctrl.traceRun: `trace on` starts a live trace (captureAll domains), `trace
  // status` reports active/off, `trace mark "<l>"` stamps a marker, `trace off`
  // finalizes. Fix: wire run_monitor's `trace` arm to the EXISTING trace machinery
  // (TraceState + finalize_trace, the same engine behind trace/start_domains +
  // trace/run/stop + runtime/mark). Signal: a first signal `recognized` (the output
  // does NOT contain "unknown command" — catches the help-lies divergence) PLUS a
  // semantic signal `traceActiveAfterOn` (after `trace on`, trace/run/status reports
  // a live trace) and `statusReportsActive` (the monitor's own `trace status` panel
  // reports the trace is active). TS: {recognized:true, traceActiveAfterOn:true,
  // statusReportsActive:true}; TRX64 (before fix): {false,false,false}.
  {
    id: "ws-trace-monitor-misc-2",
    severity: "P1",
    title: "monitor `trace on/off/status/mark` is wired to the trace engine (help no longer lies)",
    async signal(c) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      const recognized = (s: string) => !/unknown command/i.test(s);
      // `trace on` must be recognized AND actually start a trace.
      const onOut = await exec("trace on");
      // The behavioural truth: a trace is now active per the engine's own status RPC.
      const statusRpc = (await c.call("trace/run/status", { session_id: sid })) as any;
      const traceActiveAfterOn = statusRpc?.active === true;
      // The monitor's own `trace status` panel must reflect the active trace.
      const statusOut = await exec("trace status");
      // Stamp a mark via the monitor verb (only meaningful with an active trace).
      const markOut = await exec('trace mark "probe"');
      // `trace off` finalizes — recognized + no error.
      const offOut = await exec("trace off");
      return {
        // First signal: the verb is recognized (the help no longer lies).
        recognized:
          recognized(onOut) &&
          recognized(statusOut) &&
          recognized(markOut) &&
          recognized(offOut),
        // Semantic: `trace on` actually started a trace (engine status).
        traceActiveAfterOn,
        // Semantic: the monitor's `trace status` panel reports the active trace
        // (the word "active" appears; formatting differs, so match the token).
        statusReportsActive: /active/i.test(statusOut),
      };
    },
  },

  // ── P1: ws-trace-monitor-misc-13 — monitor `flow`/`bt` report LIVE state ──────
  // TS monitor-shell.ts:1103-1115: `flow` reports the live interrupt/trap frame
  // state and `bt` scans the ACTUAL 6502 stack for JSR-return candidates
  // (backtrace.ts buildBacktrace — state-dependent on SP + stack contents). TRX64's
  // run_monitor returned CONSTANT placeholder strings (main.rs:2104-2116) regardless
  // of machine state. Fix: make `bt` scan the real stack (1:1 with buildBacktrace),
  // so the panel is state-dependent, not constant. Signal: read `bt` at the cold
  // (rest) state, then push a known return address onto the 6502 stack (lower SP +
  // write the stack bytes via the monitor `wr`/`r sp=` verbs) and read `bt` again —
  // assert the output CHANGED (reflects the live stack), i.e. the panel is no longer
  // constant. Compared TS vs TRX64 on the SAME scripted stack mutation: both must
  // report a state-dependent `bt`. TS: {btReflectsStack:true, recognized:true};
  // TRX64 (before fix): {false-ish (constant), false}.
  {
    id: "ws-trace-monitor-misc-13",
    severity: "P1",
    title: "monitor `flow`/`bt` reflect live state (bt scans the real stack, not a constant)",
    async signal(c) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      const recognized = (s: string) => !/unknown command/i.test(s);
      // Set a KNOWN stack: SP just below the top, and a synthetic JSR return address
      // ($1233 → reported as $1234) at the two stack slots the scan reads first.
      // The scan reads $0100+((sp+1)&0xff) and +1 (backtrace.ts:27-30).
      await exec("r sp=fb");                  // SP=$FB → first scan slot = $01FC/$01FD
      await exec("wr ram 01fc 33 12");        // ret-lo=$33 ret-hi=$12 → scan reports $1234
      const bt1 = await exec("bt");
      // Now move the synthetic frame so the scanned return address changes.
      await exec("wr ram 01fc 77 56");        // → scan now reports $5678
      const bt2 = await exec("bt");
      return {
        // The verb must be recognized (the help advertises `bt`/`flow`).
        recognized: recognized(bt1) && recognized(bt2) && recognized(await exec("flow")),
        // Semantic: `bt` reflects the live stack — the SAME verb returns DIFFERENT
        // output for two different stack contents (a constant string never would),
        // AND the rolled-in return address is visible in the panel.
        btReflectsStack: bt1 !== bt2 && /1234/i.test(bt1) && /5678/i.test(bt2),
      };
    },
  },

  // ── P1: ws-trace-monitor-misc-8 — monitor `device drive8` targets the 1541 CPU ──
  // TS monitor-shell.ts:233-249: `device drive8` selects the active CPU so a
  // subsequent `r`/`m`/`d` inspect the 1541 drive CPU (read-inspect only); `device
  // c64` switches back. The 1541 register panel (monitor-shell.ts:481-488) is headed
  // "1541 (drive 8)" and shows the DRIVE CPU's PC (in the drive ROM $C000-$FFFF after
  // boot), distinct from the C64 CPU regs. TRX64's run_monitor had NO `device` arm
  // (the help @2216 advertises it; the comment @1481 confirmed drive8 is not wired) →
  // `unknown command: device`. Fix: wire `device c64|drive8` (sticky) + route `r`/`m`/
  // `d` to the drive CPU while device=drive8 + the read-inspect-only guard. Signal:
  // `device drive8` then `r` → the panel names the drive ("1541"/"drive 8") and the
  // C64 vs drive register panels DIFFER (distinct CPU). TS: {recognized:true,
  // drivePanelDistinct:true, namesDrive:true}; TRX64 (before fix): {false,false,false}.
  {
    id: "ws-trace-monitor-misc-8",
    severity: "P1",
    title: "monitor `device drive8` targets the 1541 drive CPU for r/m/d (help no longer lies)",
    spawn: { seedFiles: [{ rel: "fixtureA.d64", bytes: SCRAMBLE_D64 }] },
    async signal(c, d) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      const recognized = (s: string) => !/unknown command/i.test(s);
      // Mount a disk + advance so the 1541 DOS ROM init has run and the drive CPU PC
      // sits in the drive ROM range — makes the drive panel unambiguously distinct.
      const diskPath = `${d.projectDir}/fixtureA.d64`;
      await c.call("media/mount", { session_id: sid, path: diskPath, slot: 8 }).catch(() => undefined);
      await c.call("session/run", { session_id: sid, cycles: 2_000_000 }).catch(() => undefined);
      // C64 register panel (device defaults to c64).
      await exec("device c64");
      const c64Regs = await exec("r");
      // Switch to the drive CPU and read its registers.
      const devOut = await exec("device drive8");
      const driveRegs = await exec("r");
      return {
        // The verb is recognized (help advertises `device`).
        recognized: recognized(devOut) && recognized(driveRegs),
        // Semantic: the drive register panel is DISTINCT from the C64's (different CPU).
        drivePanelDistinct: c64Regs !== driveRegs && driveRegs.length > 0,
        // Semantic: the drive panel names the 1541 drive (the drive-CPU header), which
        // the C64 panel never does — the tell that r now reads the drive core.
        namesDrive: /1541|drive 8/i.test(driveRegs) && !/1541|drive 8/i.test(c64Regs),
      };
    },
  },

  // ── P1: ws-trace-monitor-misc-4 — monitor observer DSL (`obs when … do …`) ────
  // The monitor REPL advertises the full observer-registration DSL (monitor-shell.ts
  // :888-1009): `obs <name> when exec|load|store $ADDR [if <cond>] do break|log|mark|
  // cmd|trace`, plus `o`/`reg` (list) and `ignore` (skip-count). TRX64's run_monitor
  // had NO arm for these → `obs …` fell through to `unknown command: obs` (the help
  // LIES, and observers were only settable PC-only via debug/break_add). The observer
  // ENGINE already existed (observers.rs, the 1:1 port of monitor-observers.ts) — the
  // gap was the REPL PARSER + the registration path onto it. Fix: parse the DSL in
  // run_monitor, store it in a persistent per-session list re-applied each run, and
  // make `o`/`reg` list it / `ignore`/`del`/`on`/`off` mutate it. Signal: register a
  // `do log` exec observer and (a) assert the verb is recognized (no "unknown
  // command") AND (b) `o`/`reg` then LIST it (registeredCount>0, the name appears).
  // TS: {recognized:true, listed:true}; TRX64 (before fix): {false,false}.
  {
    id: "ws-trace-monitor-misc-4",
    severity: "P1",
    title: "monitor observer DSL `obs when … do …` is parsed + registered (help no longer lies)",
    async signal(c) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      const recognized = (s: string) => !/unknown command/i.test(s);
      // Register an exec observer at the KERNAL IRQ handler with a `do log` action.
      const regOut = await exec("obs irqlog when exec $EA31 do log");
      // List via `o` AND bare `obs` (the two listing forms monitor-shell.ts:888-902
      // dispatches; `reg` is NOT a monitor verb, so it is not used here). The
      // registered observer must appear in both.
      const oOut = await exec("o");
      const obsListOut = await exec("obs");
      // registeredCount>0: the listing names our observer (and is not the empty banner).
      const listed = /irqlog/.test(oOut) && /irqlog/.test(obsListOut) && !/no observers/i.test(oOut);
      // Clean up so this case leaves no observer armed for later cases sharing the session.
      await exec("obs irqlog del");
      return {
        // First signal: the verb is recognized (the help no longer lies).
        recognized: recognized(regOut) && recognized(oOut) && recognized(obsListOut),
        // Semantic: the registered observer is listed by `o`/`obs` (registeredCount>0).
        listed,
      };
    },
  },

  // ── P1: background-workers-async-3 — `do log` observer fires during free-run ───
  // A `do log` observer is a VICE tracepoint: it prints + continues (never halts).
  // TS's per-frame tick drains the observer log ring each chunk and broadcasts
  // `debug/observer_log` while the machine free-runs (runtime-controller.ts:697-698),
  // so a `do log` observer armed during a --stream free-run streams log lines every
  // frame. TRX64 had no observer DSL at all → no `do log` to fire. With the DSL wired
  // (ws-trace-monitor-misc-4) the observer registers, the bp/observer-gated stream
  // advance (stream_debug_gated_advance, from P0-A/B2) drives it, and the per-frame
  // drain (drain_and_broadcast_observer_log) broadcasts `debug/observer_log`. Signal:
  // arm `obs … when exec $EA31 do log`, start the continuous driver, free-run, and
  // count the `debug/observer_log` broadcasts. TS: >0 (fires). TRX64 must match.
  {
    id: "background-workers-async-3",
    severity: "P1",
    title: "`do log` observer fires + broadcasts debug/observer_log during --stream free-run",
    spawn: { stream: true },
    async signal(c) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      // Start the continuous --stream driver and wait until the machine has booted to
      // where $EA31 (KERNAL IRQ handler) is firing every frame (IRQs enabled).
      await c.call("debug/run", { session_id: sid });
      await waitRunningBooted(c, sid, 2_500_000, 60_000);
      // Re-arm the continuous driver if a one-shot left it paused.
      let st = await state(c, sid);
      if (st.runState !== "running") await c.call("debug/run", { session_id: sid });
      // Arm the `do log` tracepoint at the IRQ handler (a free-running hot path).
      await exec("obs irqtp when exec $EA31 do log");
      const sink = collectNotes(c);
      // Re-arm: registering an observer does not pause, but be defensive.
      st = await state(c, sid);
      if (st.runState !== "running") await c.call("debug/run", { session_id: sid });
      await sleep(5000); // the driver hits $EA31 every frame → log broadcasts accrue
      sink.off();
      const logBroadcasts = sink.notes.filter(
        (n) => n.method === "debug/observer_log" || n.method === "debug/observer_hit",
      ).length;
      // Clean up the tracepoint.
      await exec("obs irqtp del");
      return {
        // The behavioural signal: a `do log` observer fired during free-run and the
        // per-frame drain broadcast at least one debug/observer_log (count>0).
        observerLogFired: logBroadcasts > 0,
      };
    },
  },

  // ── P1: broadcasts-4 — `do trace` observer brackets a scoped capture ──────────
  // The observer DSL's `do trace [domains]|off` action is the bracket model: one
  // observer STARTS a scoped capture, another STOPS it (explicit lifecycle), and each
  // fire broadcasts a `debug/observer_log` lifecycle line (runtime-controller.ts
  // :727-753). The engine (observers.rs) already queues each `do trace` fire into
  // `pending_trace`; the daemon now drains it in drain_and_broadcast_observer_log via
  // the SAME trace machinery the `trace on/off` monitor verb drives (TraceState +
  // finalize_trace). Signal: arm `obs … when exec $EA31 do trace c64-cpu memory`,
  // free-run under --stream so the observer fires, then assert a trace became ACTIVE
  // (engine status RPC) AND a `debug/observer_log` lifecycle line was broadcast. TS:
  // {traceStarted:true, lifecycleBroadcast:true}; TRX64 must match.
  {
    id: "broadcasts-4",
    severity: "P1",
    title: "`do trace` observer starts a scoped capture + broadcasts the lifecycle line",
    spawn: { stream: true },
    async signal(c) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      // Make sure no trace is already active (a prior case may have left one).
      await c.call("trace/run/stop", { session_id: sid, wait_index: false }).catch(() => undefined);
      // Start the continuous driver + boot to where $EA31 fires every frame.
      await c.call("debug/run", { session_id: sid });
      await waitRunningBooted(c, sid, 2_500_000, 60_000);
      let st = await state(c, sid);
      if (st.runState !== "running") await c.call("debug/run", { session_id: sid });
      // Arm the START side of the bracket: a `do trace` observer at the IRQ handler.
      const armOut = await exec("obs tron when exec $EA31 do trace c64-cpu memory");
      const sink = collectNotes(c);
      st = await state(c, sid);
      if (st.runState !== "running") await c.call("debug/run", { session_id: sid });
      await sleep(4000); // the driver hits $EA31 → the observer starts the scoped trace
      sink.off();
      // The engine's own status RPC reports a live trace once the observer fired.
      const statusRpc = (await c.call("trace/run/status", { session_id: sid })) as any;
      const traceStarted = statusRpc?.active === true;
      const lifecycleBroadcast = sink.notes.some(
        (n) => n.method === "debug/observer_log" &&
          (n.params?.lines ?? []).some((l: string) => /trace/i.test(l)),
      );
      // Clean up: remove the observer and finalize any trace it started.
      await exec("obs tron del");
      await c.call("trace/run/stop", { session_id: sid, wait_index: false }).catch(() => undefined);
      return {
        // Semantic: the `do trace` observer fired and a scoped trace became active.
        traceStarted,
        // The bracket-model lifecycle line was broadcast over debug/observer_log.
        lifecycleBroadcast,
      };
    },
  },

  // ── P1: ws-trace-monitor-misc-5 — monitor `sd`/`df` flow-disasm are wired ──────
  // The monitor REPL advertises `sd` (step+disasm: the REAL executed path) and `df`
  // (follow-disasm: static control-flow walk) in its `help` text, but TRX64's
  // run_monitor had NO `sd`/`df` arm → both fell through to `unknown command` (the
  // help LIES). TS monitor-shell.ts:622-648 drives monitor-flow-disasm.ts: `sd [n]`
  // single-steps n instructions and renders each touched address once (loops folded
  // to body+×count) ending with a `-- sd: N steps …` footer; `df [addr] [n]` does a
  // STATIC control-flow walk from addr (follows JMP, descends JSR/returns on RTS,
  // loop-guarded) — a multi-instruction flow listing. Fix: wire run_monitor's `sd`/
  // `df` arms onto the EXISTING engine — `sd` reuses step_one_instruction + the
  // working `d`/disasm renderer (disasm_line_ts); `df` reuses disasm_line_ts + a
  // small 6502 control-flow classifier. Signal: a first signal `recognized` (output
  // does NOT contain "unknown command" — catches the help-lies divergence) PLUS
  // semantic properties: `sd` output carries the sd footer AND a disassembled
  // instruction line (a `$addr  bytes  MNEMONIC` row), and `df $C000` produces a
  // multi-line flow listing of disassembled instructions. Compared TS vs TRX64 on
  // the SAME cold machine state. TS: {recognized:true, sdHasFooter:true,
  // sdHasInstr:true, dfMultiLine:true, dfHasInstr:true}; TRX64 (before fix): all-false.
  {
    id: "ws-trace-monitor-misc-5",
    severity: "P1",
    title: "monitor `sd`/`df` flow-disasm are wired (step+disasm path / static flow walk; help no longer lies)",
    async signal(c) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      const recognized = (s: string) => !/unknown command/i.test(s);
      // A disassembled instruction row looks like `$c000  a9 01  LDA #$01` — match
      // the structural shape: a `$hhhh` address column followed (after spaces) by an
      // UPPERCASE 3-letter mnemonic. Independent of which exact opcode sits in RAM.
      const hasDisasmInstr = (s: string) => /\$[0-9a-f]{4}\s+[0-9a-f? ]+\s+[A-Z?]{3}/m.test(s);
      // `sd 4` — step 4 instructions from PC, render the executed path + sd footer.
      const sdOut = await exec("sd 4");
      // `df $C000` — static control-flow walk from $C000 (KERNAL/BASIC region is
      // mapped at cold reset; a multi-instruction listing comes back).
      const dfOut = await exec("df $C000");
      const dfLines = dfOut.split("\n").filter((l) => l.trim().length > 0);
      return {
        // First signal: both verbs are recognized (the help no longer lies).
        recognized: recognized(sdOut) && recognized(dfOut),
        // Semantic: `sd` rendered the dynamic step+disasm path (its `-- sd:` footer).
        sdHasFooter: /--\s*sd:/i.test(sdOut),
        // Semantic: `sd` rendered at least one disassembled instruction.
        sdHasInstr: hasDisasmInstr(sdOut),
        // Semantic: `df` produced a multi-instruction flow listing (not a one-liner).
        dfMultiLine: dfLines.length >= 2,
        // Semantic: `df` rendered disassembled instructions (it walked the flow).
        dfHasInstr: hasDisasmInstr(dfOut),
      };
    },
  },

  // ── P1: ws-trace-monitor-misc-10 — monitor `screen` VIC data-region inspect ────
  // The monitor REPL advertises `screen` (decode the 40×25 text screen at the live
  // screen pointer) in its `help` text, but TRX64's run_monitor had NO `screen` arm
  // → it fell through to `unknown command` (the help LIES). TS monitor-shell.ts:
  // 731-742 reads the live VIC base addresses (VIC bank from CIA2 $DD00 + the $D018
  // matrix nibble), then decodes the 40×25 screen-RAM matrix into a `|<40 chars>|`
  // grid (screen-code → ASCII), headed `screen @ $XXXX  (VIC bank $XXXX, $D018=$XX)`.
  // Fix: wire run_monitor's `screen` arm 1:1 — same base computation (peek $DD00/$D018
  // via the io lens, vicBank=(3-dd00)*0x4000, screenBase=vicBank+nibble*0x400), same
  // scToAscii decode, same grid + header. Signal: a first signal `recognized` (output
  // does NOT contain "unknown command") PLUS semantic structural properties compared
  // on the SAME machine state — the output is a 25-row × 40-col grid (`|…40…|` rows),
  // the header names a screen base, AND (the live-content check) a known marker
  // written into THIS daemon's own reported screen base shows up decoded in the grid.
  // Exact bytes are NOT asserted (header/formatting may differ); structure + the
  // round-tripped marker are. TS: {recognized:true, gridRows:25, gridCols:40,
  // hasBaseHeader:true, markerVisible:true}; TRX64 (before fix): all-false/zero.
  {
    id: "ws-trace-monitor-misc-10",
    severity: "P1",
    title: "monitor `screen` decodes the live 40x25 text screen (VIC data-region inspect; help no longer lies)",
    async signal(c) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      const recognized = (s: string) => !/unknown command/i.test(s);
      const first = await exec("screen");
      // Parse THIS daemon's own reported screen base from the header (the base may
      // differ between daemons at cold reset, so each writes its marker at its own
      // base — the round-trip is what we assert, not a shared address).
      const baseMatch = /screen @ \$([0-9a-f]{4})/i.exec(first);
      const base = baseMatch ? parseInt(baseMatch[1], 16) : null;
      // The grid rows are the `|<40 chars>|` lines (25 of them).
      const gridRows0 = first.split("\n").filter((l) => /^\|.*\|$/.test(l));
      const cols0 = gridRows0.length ? gridRows0[0].length - 2 : 0; // strip the two pipes
      // Live-content check: write screen-code $01 (=`A`) into the daemon's own screen
      // base cell (0,0), re-decode, and confirm an `A` now sits at grid row 0 col 0.
      let markerVisible = false;
      if (base !== null) {
        await exec(`wr ram ${base.toString(16)} 01`);
        const second = await exec("screen");
        const rows = second.split("\n").filter((l) => /^\|.*\|$/.test(l));
        markerVisible = rows.length >= 1 && rows[0][1] === "A"; // [0] is the leading `|`
      }
      return {
        // First signal: the verb is recognized (the help no longer lies).
        recognized: recognized(first),
        // Semantic: a 25-row grid.
        gridRows: gridRows0.length,
        // Semantic: each row is 40 columns wide.
        gridCols: cols0,
        // Semantic: the header reports a screen base.
        hasBaseHeader: base !== null,
        // Semantic (live content): a marker written at the live base decodes in-grid.
        markerVisible,
      };
    },
  },

  // ── P1: ws-trace-monitor-misc-9 — monitor `dump`/`undump`/`savecrt`/`swapcrt` ──
  // The monitor REPL advertises dump|undump (STATE/TRACE) + savecrt|swapcrt in its
  // help text, but TRX64's run_monitor had NO arms for them → the verb fell through to
  // `unknown command: dump` (the help LIES). TS monitor-shell.ts:279-367 wires each to
  // the live capability: `dump "<p>"` writes a runtime snapshot to disk, `undump "<p>"`
  // restores it; `savecrt "<p>"` re-packs the live cart flash to a .crt. Fix: wire
  // run_monitor's dump/undump arms to the EXISTING snapshot/dump+undump engine
  // (write_native_snapshot / read_native_snapshot) and savecrt/swapcrt to the EXISTING
  // cart capability (crt_image / attach_cart_from_bytes). Signal: a first signal
  // `recognized` (no "unknown command" — catches the help-lies divergence) PLUS the
  // EFFECT: `dump <p>` creates a non-empty snapshot FILE on disk, `undump <p>` reads it
  // back (recognized + paused), and (with the EasyFlash .crt mounted) `savecrt <p>`
  // writes a non-empty .crt FILE. Compared TS vs TRX64 on the SAME scripted sequence:
  // both recognized + both produce the files. TS: all true; TRX64 (before fix): all
  // false (every verb is `unknown command`).
  {
    id: "ws-trace-monitor-misc-9",
    severity: "P1",
    title: "monitor `dump`/`undump`/`savecrt`/`swapcrt` are wired to the live capabilities (help no longer lies)",
    spawn: { seedFiles: [{ rel: "fixture.crt", bytes: EASYFLASH_CRT }] },
    async signal(c, d) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      const recognized = (s: string) => !/unknown command/i.test(s);
      const fileNonEmpty = (p: string) => {
        try { return existsSync(p) && statSync(p).size > 0; } catch { return false; }
      };
      // dump → a runtime snapshot FILE under the per-daemon project dir.
      const snapPath = `${d.projectDir}/probe.c64re`;
      const dumpOut = await exec(`dump "${snapPath}"`);
      const dumpMadeFile = fileNonEmpty(snapPath);
      // undump → reads it back (recognized + no error). The monitor pauses on restore.
      const undumpOut = await exec(`undump "${snapPath}"`);
      // savecrt → re-pack the live EasyFlash flash to a NEW .crt copy on disk.
      const crtPath = `${d.projectDir}/fixture.crt`;
      await c.call("media/mount", { session_id: sid, path: crtPath, slot: 0 });
      const outCrt = `${d.projectDir}/saved.crt`;
      const saveOut = await exec(`savecrt "${outCrt}"`);
      const saveMadeFile = fileNonEmpty(outCrt);
      // swapcrt → hot-swap the SAME .crt back in, NO reset (recognized + no error).
      const swapOut = await exec(`swapcrt "${crtPath}"`);
      return {
        // First signal: every verb is recognized (the help no longer lies).
        recognized:
          recognized(dumpOut) &&
          recognized(undumpOut) &&
          recognized(saveOut) &&
          recognized(swapOut),
        // Effect: `dump` produced a non-empty snapshot file on disk.
        dumpMadeFile,
        // Effect: `undump` read it back without error (no "error"/"cannot" wording).
        undumpOk: recognized(undumpOut) && /undumped/i.test(undumpOut),
        // Effect: `savecrt` produced a non-empty .crt file on disk.
        saveMadeFile,
        // Effect: `swapcrt` succeeded (no error wording).
        swapOk: recognized(swapOut) && /swapped/i.test(swapOut),
      };
    },
  },

  // ── P1: ws-trace-monitor-misc-11 — monitor host-FS + PRG verbs are wired ──────
  // The monitor REPL advertises the FILE family — pwd/cd/ls/dir/mkdir/rmdir +
  // load/save/bload/bsave — rooted at the project dir, but TRX64's run_monitor had NO
  // arms → `unknown command: pwd` (the help LIES). TS monitor-shell.ts:769-845 wires the
  // host-FS mini-shell (cwd defaults to the project dir; relative paths resolve off the
  // session cwd; absolute/`..` pass through — NOT a hard jail) + PRG load/save honouring
  // the 2-byte load address + bload/bsave raw binary at an address. Fix: wire the family
  // to std::fs + the EXISTING machine RAM access (poke / ram), matching the TS
  // resolveFsPath cwd rules. Signal: a first signal `recognized` (no "unknown command")
  // PLUS the EFFECT: `pwd` returns a path, `ls` lists the project-dir seed file, and a
  // `bsave`/`bload` round-trip preserves the bytes (write a known pattern to RAM, bsave
  // it, zero RAM, bload it back, read it). Compared TS vs TRX64 on the SAME scripted
  // sequence: both recognized + both list the same project-dir entry + round-trip the
  // same bytes. TS: all true; TRX64 (before fix): all false (every verb is `unknown
  // command`).
  {
    id: "ws-trace-monitor-misc-11",
    severity: "P1",
    title: "monitor host-FS + PRG verbs (pwd/cd/ls/mkdir + load/save/bload/bsave) are wired (help no longer lies)",
    spawn: { seedFiles: [{ rel: "seed.txt", bytes: Buffer.from("trx64-fs-probe") }] },
    async signal(c, d) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      const recognized = (s: string) => !/unknown command/i.test(s);
      // pwd → recognized + a path (an absolute path string, non-empty).
      const pwdOut = await exec("pwd");
      const pwdIsPath = pwdOut.startsWith("/") && pwdOut.length > 1;
      // cd to the project dir (no-arg = project dir), then ls → lists the seed file.
      const cdOut = await exec("cd");
      const lsOut = await exec("ls");
      const lsListsSeed = /seed\.txt/.test(lsOut);
      // mkdir a subdir, then ls it → recognized + present.
      const mkdirOut = await exec("mkdir sub");
      const lsSub = await exec("ls sub");
      // bsave/bload round-trip: write a known 4-byte pattern to RAM at $C000, bsave it,
      // clobber that RAM, bload it back, and read it. The bytes must survive.
      await exec("wr ram c000 de ad be ef");
      const dumpPath = `${d.projectDir}/round.bin`;
      const bsaveOut = await exec(`bsave "${dumpPath}" c000 c003`);
      const bsaveMadeFile = (() => { try { return statSync(dumpPath).size === 4; } catch { return false; } })();
      await exec("wr ram c000 00 00 00 00");                 // clobber
      const bloadOut = await exec(`bload "${dumpPath}" c000`); // restore from file
      const memBack = await exec("m ram c000 c003");          // read it back
      // The restored pattern must read back (DE AD BE EF appear in the dump).
      const roundTripped = /de\s*ad\s*be\s*ef/i.test(memBack.replace(/[^0-9a-fA-F\s]/g, " "));
      // load/save: save a PRG (2-byte load addr = $C000) of the round-trip RAM, then
      // load it back at an OVERRIDE address and confirm the verb is recognized.
      const prgPath = `${d.projectDir}/round.prg`;
      const saveOut = await exec(`save "${prgPath}" c000 c003`);
      const loadOut = await exec(`load "${prgPath}"`);
      return {
        // First signal: every verb is recognized (the help no longer lies).
        recognized:
          recognized(pwdOut) && recognized(cdOut) && recognized(lsOut) &&
          recognized(mkdirOut) && recognized(lsSub) &&
          recognized(bsaveOut) && recognized(bloadOut) &&
          recognized(saveOut) && recognized(loadOut),
        // Effect: `pwd` is an absolute path.
        pwdIsPath,
        // Effect: `ls` lists the project-dir seed file (the FS shell is rooted there).
        lsListsSeed,
        // Effect: `mkdir` succeeded (no error wording).
        mkdirOk: /mkdir sub/.test(mkdirOut),
        // Effect: a bsave/bload round-trip preserved the bytes (DE AD BE EF survive).
        roundTripped,
        // Effect: bsave produced a 4-byte raw file on disk.
        bsaveMadeFile,
        // Effect: save/load a PRG are recognized + report a load address.
        prgIo: /saved/i.test(saveOut) && /loaded/i.test(loadOut) && /c000/i.test(loadOut),
      };
    },
  },

  // ── P1: ws-trace-monitor-misc-16 — monitor label/note WRITE + .sym round-trip ─
  // TS monitor-shell.ts:250-275 → ws-server.ts:2207-2258 (ProjectKnowledgeService):
  // `label <addr> <name>` persists a user label to <project>/knowledge/labels.user.json
  // (+ a memory-address entity), `label` (bare) lists them, `unlabel` removes one,
  // `note <addr> "<text>"` drops a finding, and `save_labels`/`load_labels` round-trip
  // a VICE `.sym` (`al C:<hx> .<name>`). The disassembler then annotates: `d <addr>` of
  // a labelled address shows `<name>:` above the line (disasm6502.ts:155-161). TRX64's
  // run_monitor (main.rs:3090-3092) unconditionally errored "no project workspace bound"
  // for ALL these verbs (no ProjectKnowledgeService bridge). Fix: a faithful project-
  // knowledge persistence bridge (project_knowledge.rs) over the SAME store
  // format/location. Signal — SEMANTIC behaviour compared TS vs TRX64 on the SAME
  // scripted sequence: a label is set, listed, shown in the disasm, round-tripped
  // through a .sym, and a note is recognized. TS: all true; TRX64 (before fix): all
  // false (every verb errors "no project workspace bound").
  {
    id: "ws-trace-monitor-misc-16",
    severity: "P1",
    title: "monitor label/unlabel/note + save_labels/load_labels persist project knowledge (no longer error)",
    // No seed needed — the labels are CREATED at runtime; both daemons own a project
    // dir (--project <tmp>), so the knowledge store lands under it identically.
    async signal(c, d) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      // BEFORE: the historic stub error must be gone.
      const noWorkspaceErr = (s: string) => /no project workspace bound/i.test(s);
      // 1) Set a label at $C000 → persisted (returns "label $c000 = myroutine (...)").
      const setOut = await exec("label c000 myroutine");
      // 2) Bare `label` lists it.
      const listOut = await exec("label");
      // 3) `d $C000` disassembly annotates the labelled address with `myroutine:`.
      //    Put a known opcode there first so the disasm is deterministic (NOP).
      await exec("wr ram c000 ea");
      const disOut = await exec("d c000 c000");
      // 4) save_labels writes a VICE .sym, load_labels reads it back.
      const symPath = `${d.projectDir}/labels.sym`;
      const saveOut = await exec(`save_labels "${symPath}"`);
      const symMadeFile = (() => {
        try { return /al\s+C:c000\s+\.myroutine/i.test(readFileSync(symPath, "utf8")); }
        catch { return false; }
      })();
      // Clear, then load the .sym back → the label reappears in the list.
      await exec("unlabel myroutine");
      const listAfterUnlabel = await exec("label");
      const loadOut = await exec(`load_labels "${symPath}"`);
      const listAfterLoad = await exec("label");
      // 5) `note <addr> "<text>"` is recognized + persists a finding (returns "note saved").
      const noteOut = await exec('note d020 "border colour write"');
      return {
        // The historic stub error is gone on EVERY verb.
        notErroredAsNoWorkspace:
          !noWorkspaceErr(setOut) && !noWorkspaceErr(listOut) && !noWorkspaceErr(saveOut) &&
          !noWorkspaceErr(loadOut) && !noWorkspaceErr(noteOut),
        // Semantic: set → the persisted label is listed.
        labelSetAndListed: /myroutine/i.test(setOut) && /myroutine/i.test(listOut),
        // Semantic: the disassembly annotates the labelled address (asm-style `name:`).
        disasmShowsLabel: /myroutine:/i.test(disOut),
        // Semantic: save_labels wrote a VICE-format .sym file.
        symRoundTripSaved: symMadeFile,
        // Semantic: unlabel removed it, load_labels brought it back.
        symRoundTripLoaded:
          !/myroutine/i.test(listAfterUnlabel) &&
          /loaded\s+1\s+label/i.test(loadOut) &&
          /myroutine/i.test(listAfterLoad),
        // Semantic: note is recognized + persisted (the confirmation string).
        notePersisted: /note saved @ \$d020/i.test(noteOut),
      };
    },
  },

  // ── P1: ws-trace-monitor-misc-15 — monitor inspect/xref/sym project-read bridge ─
  // TS monitor-shell.ts:1181-1207 → ws-server.ts:2135-2191: `inspect <addr>` returns
  // the analysis segment(s) + callers covering the address, `xref <addr>` the project-
  // wide cross-references, and `sym <name>` resolves a name→address — all read from the
  // project's `*_analysis.json` (effective-segments overlay + the address/xref index).
  // TRX64's run_monitor (main.rs:3083-3085) unconditionally errored "project-read bridge
  // unavailable". Fix: a faithful project-read bridge (project_knowledge.rs) over the
  // SAME on-disk analysis files. We SEED a representative `*_analysis.json` (segments +
  // codeAnalysis.xrefs) into BOTH daemons' project dirs so both read identical project
  // knowledge. Signal — SEMANTIC behaviour compared TS vs TRX64 on the SAME fixture:
  // inspect/xref/sym all return the seeded knowledge (not an error). TS: all true; TRX64
  // (before fix): all false (every verb errors "project-read bridge unavailable").
  {
    id: "ws-trace-monitor-misc-15",
    severity: "P1",
    title: "monitor inspect/xref/sym read the project _analysis.json (no longer error)",
    spawn: {
      seedFiles: [
        {
          // A minimal valid analysis report: two segments (a labelled `main` code
          // segment + a data segment) and two xrefs (a read of $0900 + a call of $0810).
          rel: "fixture_analysis.json",
          bytes: Buffer.from(
            JSON.stringify({
              segments: [
                { kind: "code", start: 0x0810, end: 0x08ff, label: "main" },
                { kind: "data", start: 0x0900, end: 0x09ff },
              ],
              codeAnalysis: {
                xrefs: [
                  { sourceAddress: 0x0820, targetAddress: 0x0900, type: "read", operandText: "lda $0900" },
                  { sourceAddress: 0x0950, targetAddress: 0x0810, type: "call" },
                ],
              },
            }),
          ),
        },
      ],
    },
    async signal(c) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      const bridgeErr = (s: string) => /project-read bridge unavailable/i.test(s);
      // inspect $0810 → owns the labelled `main` code segment + the $0950 caller.
      const inspectOut = await exec("inspect 0810");
      // xref $0900 → the $0820 read references it (project-wide).
      const xrefOut = await exec("xref 0900");
      // sym main → reverse-resolves the labelled segment to $0810.
      const symOut = await exec("sym main");
      return {
        // The historic stub error is gone on EVERY verb.
        notErroredAsBridgeUnavailable:
          !bridgeErr(inspectOut) && !bridgeErr(xrefOut) && !bridgeErr(symOut),
        // Semantic: inspect surfaces the seeded segment knowledge at $0810.
        inspectReturnsKnowledge:
          /0810/i.test(inspectOut) && /code/i.test(inspectOut) && /main/i.test(inspectOut),
        // Semantic: inspect lists the cross-file caller of $0810.
        inspectShowsCaller: /0950/i.test(inspectOut),
        // Semantic: xref surfaces the seeded reference INTO $0900.
        xrefReturnsRefs: /0820/i.test(xrefOut) && /in:1/i.test(xrefOut),
        // Semantic: sym reverse-resolves the labelled segment.
        symResolvesName: /main/i.test(symOut) && /0810/i.test(symOut),
      };
    },
  },
];

// ─────────────────────────────────────────────────────────────────────────────
// RUNNER
// ─────────────────────────────────────────────────────────────────────────────
async function runCase(cse: ConfCase): Promise<{ ok: boolean; tsSig: unknown; trxSig: unknown; detail: string }> {
  let ts: Daemon | undefined;
  let trx: Daemon | undefined;
  try {
    [ts, trx] = await Promise.all([spawnDaemon("ts", cse.spawn), spawnDaemon("trx64", cse.spawn)]);
    // Per-RPC timeout. The TS oracle daemon is tsx-from-src (~4fps), and some
    // control ops are a single blocking RPC that runs millions of cycles inline
    // (e.g. session/reset cold = runFor(5M)). 240s keeps those from a false timeout.
    const tsC = await connect(ts.endpoint, 240_000);
    const trxC = await connect(trx.endpoint, 240_000);
    let tsSig: unknown, trxSig: unknown;
    try {
      tsSig = await cse.signal(tsC, ts);
      trxSig = await cse.signal(trxC, trx);
    } finally {
      tsC.close();
      trxC.close();
    }
    const div = diffResponses(tsSig, trxSig);
    return {
      ok: div === null,
      tsSig,
      trxSig,
      detail: div ? formatDivergence(div) : "signals equal",
    };
  } finally {
    ts?.stop();
    trx?.stop();
  }
}

async function main() {
  const args = process.argv.slice(2);
  const sevIdx = args.indexOf("--severity");
  const sev = sevIdx >= 0 ? args[sevIdx + 1] : undefined;
  const onlyIdx = args.indexOf("--only");
  const only = onlyIdx >= 0 ? args[onlyIdx + 1] : undefined;
  // `--include-blocked` runs the harness-blocked cases too (e.g. to re-check whether
  // the TS-side limitation has been lifted). By default they are SKIPPED + reported.
  const includeBlocked = args.includes("--include-blocked");

  const selected = CASES.filter(
    (c) => (!sev || c.severity === sev) && (!only || c.id === only),
  );
  if (selected.length === 0) {
    console.error("no cases match the filter");
    process.exit(2);
  }

  console.log(`\nDifferential WS-conformance gate — ${selected.length} case(s)\n`);
  let red = 0;
  let blocked = 0;
  for (const cse of selected) {
    process.stdout.write(`[${cse.severity}] ${cse.id} — ${cse.title}\n`);
    // A harness-blocked case does not gate: skip it unless `--only` named it or
    // `--include-blocked` is set (then it runs and its result is shown, not counted).
    if (cse.blocked && !includeBlocked && only !== cse.id) {
      blocked++;
      console.log(`   BLOCKED  ${cse.blocked}\n`);
      continue;
    }
    try {
      const r = await runCase(cse);
      if (r.ok) {
        console.log(`   GREEN  TRX64 ≡ TS  ${JSON.stringify(r.tsSig)}\n`);
      } else if (cse.blocked) {
        // Run-on-demand of a blocked case: report, but never fail the suite on it.
        blocked++;
        console.log(`   BLOCKED (ran on demand) ${r.detail}`);
        console.log(`          TS   = ${JSON.stringify(r.tsSig)}`);
        console.log(`          TRX64= ${JSON.stringify(r.trxSig)}\n`);
      } else {
        red++;
        console.log(`   RED    ${r.detail}`);
        console.log(`          TS   = ${JSON.stringify(r.tsSig)}`);
        console.log(`          TRX64= ${JSON.stringify(r.trxSig)}\n`);
      }
    } catch (e) {
      if (cse.blocked) {
        blocked++;
        console.log(`   BLOCKED (ran on demand) ${e instanceof Error ? e.message : String(e)}\n`);
      } else {
        red++;
        console.log(`   ERROR  ${e instanceof Error ? e.message : String(e)}\n`);
      }
    }
  }
  const gated = selected.length - blocked;
  console.log(`${gated - red}/${gated} GREEN, ${red} RED${blocked ? `, ${blocked} BLOCKED (non-gating)` : ""}`);
  process.exit(red === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(e);
  process.exit(2);
});
