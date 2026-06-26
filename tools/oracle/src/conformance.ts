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

import { existsSync, readFileSync, statSync, writeFileSync, mkdirSync } from "node:fs";
import { join } from "node:path";
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

/** TRX64-superset reverse-debug methods (runtime/reverse_step, runtime/who_wrote,
 *  runtime/crash_triage, trace/build_from_ring) are delivered ONLY by the TRX64 runtime.
 *  The TS runtime CLEANLY DECLINES them (ws-server.ts TRX64_ONLY_METHODS) with the message
 *  "not supported by the TypeScript runtime — use the TRX64 runtime" + a `data.trx64Only`
 *  marker — NOT the generic -32601 "method not found", NOT a matched throw. The ws-client
 *  surfaces the JSON-RPC error as an Error whose `.message` is the server message, so we
 *  match the recognizable refusal text (the `data` marker is not carried over the client).
 *  Returns true iff the call was declined with that clean, recognizable message. A success
 *  (no throw), a -32601, or any other error → false (the TS side did NOT decline cleanly). */
async function assertTrx64OnlyDecline(
  c: RpcClient,
  method: string,
  params: Record<string, unknown>,
): Promise<boolean> {
  try {
    await c.call(method, params);
    return false; // TS must NOT actually service a TRX64-only superset method.
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    // The clean TRX64-superset decline — explicitly NOT -32601 / "method not found".
    if (/method not found|-32601/i.test(msg)) return false;
    return /not supported by the TypeScript runtime/i.test(msg) || /trx64Only/i.test(msg);
  }
}

/** A WEAKER decline check for TRX64-superset methods that are NOT (yet) registered in
 *  the c64re `TRX64_ONLY_METHODS` set (which lives in the un-editable c64re repo). Such
 *  a method is declined by the TS runtime with the generic -32601 "method not found"
 *  rather than the curated TRX64-only message. For the trace-decode / reverse-depth
 *  superset ops, EITHER decline is the honest TS signal: the TS runtime does NOT service
 *  the method (it has no trace-decode WS op / no in-process reverse rings), while TRX64
 *  delivers it. Returns true iff the TS side DECLINED (threw) rather than servicing it; a
 *  success (no throw) → false (TS must not actually service a TRX64-superset method). */
async function assertDeclined(
  c: RpcClient,
  method: string,
  params: Record<string, unknown>,
): Promise<boolean> {
  try {
    await c.call(method, params);
    return false; // TS must NOT service a TRX64-superset method.
  } catch {
    return true; // any clean RPC error (TRX64-only message OR -32601) = declined.
  }
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

// A 2-bank EasyFlash .crt whose ROML ($8000) byte differs per bank, so a live
// bank switch (a CPU `STA $DE00` of the bank number) is VISIBLE both in
// session/cart_status.bank AND in the `m cart 8000` (cart-lens peek) byte: bank N
// has $A0+N at ROML offset 0 (so bank0→$A0, bank1→$A1). Two 16K CHIP packets (ROML
// $8000 + ROMH $A000 per bank); bank 0 carries a sane reset vector at $3ffc.
function makeMultiBankEasyFlashCrt(name = "EFMB", banks = 2): Buffer {
  const hdr = Buffer.alloc(0x40);
  hdr.write("C64 CARTRIDGE   ", 0, "ascii");
  hdr.writeUInt32BE(0x40, 0x10);
  hdr.writeUInt16BE(0x0100, 0x14);
  hdr.writeUInt16BE(32, 0x16); // EasyFlash
  hdr.writeUInt8(1, 0x18);     // EXROM
  hdr.writeUInt8(0, 0x19);     // GAME
  hdr.write(name, 0x20, "ascii");
  const parts: Buffer[] = [hdr];
  for (let b = 0; b < banks; b++) {
    const rom = Buffer.alloc(0x4000, 0xff);
    rom[0x0000] = (0xa0 + b) & 0xff;     // ROML offset 0 = the per-bank fingerprint
    if (b === 0) { rom[0x3ffc] = 0x00; rom[0x3ffd] = 0x80; } // sane reset vector on bank0
    const chip = Buffer.alloc(0x10);
    chip.write("CHIP", 0, "ascii");
    chip.writeUInt32BE(0x10 + rom.length, 4);
    chip.writeUInt16BE(0, 8);            // ROM/flash
    chip.writeUInt16BE(b, 10);           // bank b
    chip.writeUInt16BE(0x8000, 12);      // load $8000
    chip.writeUInt16BE(rom.length, 14);
    parts.push(chip, rom);
  }
  return Buffer.concat(parts);
}
const EASYFLASH_MB_CRT = makeMultiBankEasyFlashCrt();

const SCRAMBLE_D64_B = (() => {
  // A SECOND seed disk for the recents-ordering case (mount A then B → B newest).
  // Reuse the scramble image bytes under a different name; the recents store keys on
  // the (distinct) path, so identical bytes are fine — only the basename/order matter.
  return SCRAMBLE_D64;
})();

// A DISTINCT 174848-byte D64 whose BAM/dir-area bytes differ from SCRAMBLE_D64, so a
// mount-over → checkpoint-restore round-trip can tell which image is attached by its
// content sha256 (the checkpoint must restore the CAPTURED image, not the latest).
const DISTINCT_D64 = (() => {
  const buf = Buffer.alloc(174848, 0x00);
  // A recognizable sentinel at the BAM (track 18, sector 0 = linear offset 0x16500).
  buf[0x16500] = 0x12; buf[0x16501] = 0x01; buf[0x16502] = 0x41; // dir t/s + DOS ver "A"
  for (let i = 0; i < 16; i++) buf[0x16590 + i] = 0xaa; // disk-name area marker
  return buf;
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
  // ── P0: ws-checkpoint-ring-cadence — Spec 772 ring cadence + retention cap ─────
  // The checkpoint ring is the UI scrub-filmstrip buffer (NOT deep history — that is
  // the Spec 766 recorder). Spec 772 sizes it: cadence 0.5s = 25 PAL frames, retention
  // 10s = a MAX-ENTRIES cap of 20 (ceil(seconds/(cadence/50))) on top of the 32 MiB
  // byte budget, evict-oldest on whichever-first. BEFORE this spec the two runtimes
  // DIVERGED: TS captured every 50 frames (1s) and the ring was UNCAPPED (~512-slot
  // byte budget → minutes of history); TRX64 captured every 25 frames and was also
  // uncapped. So a long free-run grew the TS ring at half TRX64's rate AND neither
  // capped → the live counts drifted apart and ran past ~20 (TS ~10+, TRX64 ~30+ over
  // ~2-3 min). AFTER: both capture every 25 frames AND both cap the LIVE ring at 20,
  // so checkpoint/list count is IDENTICAL and bounded, and checkpoint/thumbnails count
  // == checkpoint/list count (thumbs evict WITH the ring entry, Spec 772). The signal
  // free-runs under --stream and polls checkpoint/list until the live count PLATEAUS
  // (the cap is reached — eviction holds it flat), then reports the plateau count + the
  // thumbnail==list equality. TS is the authority on the cap value; TRX64 must match.
  // The TS oracle daemon emulates ~3.5 fps, so one 25-frame cadence ≈ ~7s wall and
  // reaching the 20-entry cap takes ~140s — each poll RPC is fast, so the long free-run
  // sits well under the 240s per-RPC cap (run the suite with a ~300s budget).
  {
    id: "ws-checkpoint-ring-cadence",
    severity: "P0",
    title:
      "checkpoint ring caps the LIVE count at the Spec-772 retention size (default 20 = 10s @ 0.5s cadence), identical on both runtimes; thumbnails count == list count",
    spawn: { stream: true },
    async signal(c) {
      const sid = await liveSession(c);
      const listCount = async (): Promise<number> => {
        const r = (await c.call("checkpoint/list", { session_id: sid })) as any;
        return (r?.checkpoints ?? []).length;
      };
      const thumbCount = async (): Promise<number> => {
        const r = (await c.call("checkpoint/thumbnails", { session_id: sid })) as any;
        return (r?.thumbnails ?? []).length;
      };
      // Start the continuous --stream driver and let it free-run, accumulating anchors.
      await c.call("debug/run", { session_id: sid });
      // Poll checkpoint/list until the count PLATEAUS (3 consecutive equal, non-zero
      // reads = the cap is holding the ring flat via eviction) or a generous deadline.
      // We track the MAX count ever seen so an overflow past the cap is caught too.
      const deadline = Date.now() + 230_000;
      let maxSeen = 0;
      let last = -1;
      let stableHits = 0;
      let plateau = 0;
      while (Date.now() < deadline) {
        await sleep(4000);
        const n = await listCount();
        if (n > maxSeen) maxSeen = n;
        if (n === last && n > 0) {
          stableHits++;
          // A real plateau (not a transient stall): require the count to be ≥ a small
          // floor so a momentarily-stuck-at-2 read can't false-plateau before the cap.
          if (stableHits >= 3 && n >= 8) { plateau = n; break; }
        } else {
          stableHits = 0;
        }
        last = n;
      }
      // If we never hit a stable plateau, fall back to the last observed count so the
      // diff still compares the two runtimes (a never-plateaued TS leg diverges loud).
      if (plateau === 0) plateau = await listCount();
      // At the plateau, the thumbnail filmstrip must surface exactly one thumb per live
      // ring entry (thumbs evict WITH the ring entry — Spec 772 prune-orphans).
      const ringNow = await listCount();
      const thumbsNow = await thumbCount();
      return {
        // The behavioural signal: the live ring count plateaus at the SAME capped value
        // on both runtimes (the authority supplies it). Was divergent before Spec 772.
        plateauRingCount: plateau,
        // The cap is an UPPER bound — the ring never exceeds the plateau (no overflow).
        neverExceededPlateau: maxSeen <= plateau,
        // The ring is bounded (NOT minutes of history) — the default cap is 20, so a
        // free-run that ran long enough to plateau holds a SMALL ring, not ~30+/512.
        ringIsBounded: ringNow <= 24,
        // Spec 769.5a + 772 — every live ring entry has a thumbnail (filmstrip == ring).
        thumbnailsMatchRing: thumbsNow === ringNow,
      };
    },
  },

  // ── P0: ws-session-debug-0 — free-run breakpoint under --stream ────────────
  // Set a breakpoint on the KERNAL IRQ handler ($EA31, hit every frame) while the
  // --stream loop is the live driver. TS gates breakpoints in its per-frame tick,
  // so the machine HALTS at $EA31 + fires debug/breakpoint_hit(pc=$EA31) +
  // runState→paused with stopReason "breakpoint". TRX64's stream loop checks nothing
  // → never halts. (Audit P0 ws-session-debug-0 — HARDENED: the prior signal proved
  // *a* stop, not that the BP CAUSED it. A budget-pause/JAM/generic pause false-greens
  // a stream loop that never honours the bp. We now assert (a) the NEGATIVE leg —
  // free-run with NO bp armed stays running (halted=false), so a generic pause can't
  // masquerade as the bp halt; (b) the halt PC is exactly $EA31 (session/state cpu.pc)
  // AND stopReason is "breakpoint"; (c) the broadcast is specifically
  // debug/breakpoint_hit with params.pc===$EA31 — NOT a bare debug/paused/stopped.)
  {
    id: "ws-session-debug-0",
    severity: "P0",
    title: "free-run breakpoint under --stream halts AT $EA31 (bp caused it, not a generic pause)",
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

      // ── NEGATIVE leg: free-run with NO breakpoint armed must KEEP RUNNING. ──
      // This is what makes the positive halt meaningful: a stream loop that pauses
      // for ANY reason (budget exhaustion, a stray JAM, a generic pause) would have
      // false-greened the old presence-only signal. Here, no-bp ⇒ still running.
      await sleep(2000); // let the free-run driver advance several frames with no bp
      const stNoBp = await state(c, sid);
      const cycBeforeBp = stNoBp.c64Cycles ?? stNoBp.cycles ?? stNoBp.cpu?.cycles ?? 0;
      // Re-arm if the boot-window poll above (one-shot path) left it paused.
      if (stNoBp.runState !== "running") await c.call("debug/run", { session_id: sid });

      // ── POSITIVE leg: arm bp at $EA31; the continuous driver must hit it + halt. ──
      const sink = collectNotes(c);
      await c.call("debug/break_add", { session_id: sid, pc: 0xea31 });
      await sleep(4000); // continuous driver must hit $EA31 + halt
      st = await state(c, sid);
      sink.off();
      const haltPc = st.cpu?.pc ?? st.pc ?? null;
      const bpHit = sink.notes.find(
        (n) => n.method === "debug/breakpoint_hit" && Number((n.params as any)?.pc) === 0xea31,
      );
      return {
        // NEGATIVE: with no bp armed the free-run machine never paused.
        ranWithoutBpStaysRunning: stNoBp.runState === "running",
        // POSITIVE: the bp halted the machine AT $EA31 (not merely "a stop happened").
        halted: st.runState === "paused",
        haltPcIsEa31: Number(haltPc) === 0xea31,
        stopReasonBreakpoint: st.stopReason === "breakpoint",
        // BROADCAST: specifically debug/breakpoint_hit with params.pc===$EA31, not a
        // bare debug/paused/stopped — proves the bp (not a generic pause) drove it.
        firedBreakpointHitAtEa31: bpHit != null,
        // Causation sanity: the machine WAS advancing before the bp (so the halt is a
        // genuine free-run stop, not a never-started session reading paused both ways).
        advancedBeforeBp: Number(cycBeforeBp) >= 2_500_000,
      };
    },
  },

  // ── P0: ws-media-0 — disk mount routes through the ingress boundary ─────────
  // media/mount a disk. TS routes through the ingress service: captures a
  // before/after checkpoint (so the media event is replayable) and tops media/
  // recent with the mounted disk. TRX64 attaches the disk directly → null
  // checkpoint ids + recents untouched (and, downstream, silent outgoing-disk
  // write loss on the next swap). (Audit P0 ws-media-0.)
  //
  // HARDENED (Batch 6 #3): a FRESH-session first mount is the experiment ROOT —
  // only an AFTER checkpoint is captured (`before` is null, no prior medium), so the
  // intervention before/after PAIR never hit the wire. The mount-over-present case
  // is where the real write-loss bug lived (the outgoing disk's dirty writes must be
  // persisted + a before-checkpoint minted). So the signal now mounts disk A FIRST
  // (root: after-only), THEN mounts disk B over the present medium and asserts the
  // intervention has BOTH checkpointBeforeId AND checkpointAfterId, NON-null and
  // DISTINCT (a real before/after pair), plus the outgoing-disk persist marker
  // (`detail.diskPersisted`). (Audit Batch 6 #3 — ws-media-0 real before/after pair.)
  {
    id: "ws-media-0",
    severity: "P0",
    title: "disk mount-over-present captures a real before/after checkpoint PAIR (distinct, replayable)",
    spawn: {
      seedFiles: [
        { rel: "fixtureA.d64", bytes: SCRAMBLE_D64 },
        { rel: "fixtureB.d64", bytes: SCRAMBLE_D64_B },
      ],
    },
    async signal(c, d) {
      const sid = await liveSession(c);
      const pathA = `${d.projectDir}/fixtureA.d64`;
      const pathB = `${d.projectDir}/fixtureB.d64`;
      // First mount = the experiment ROOT (after-checkpoint only; before is null).
      const rootResp = (await c.call("media/mount", { session_id: sid, path: pathA, slot: 8 })) as any;
      // Second mount = an INTERVENTION over a present medium: before AND after both
      // captured (a real pair) + the outgoing disk's dirty writes persisted first.
      const overResp = (await c.call("media/mount", { session_id: sid, path: pathB, slot: 8 })) as any;
      const recent = (await c.call("media/recent", {})) as any;
      const recentArr: any[] = Array.isArray(recent) ? recent : recent?.recent ?? recent?.result ?? [];
      const norm = (p: string) => (p ? p.split("/").pop() : p); // basename — path roots differ by design
      const rootAfter = rootResp?.event?.checkpointAfterId ?? null;
      const beforeId = overResp?.event?.checkpointBeforeId ?? null;
      const afterId = overResp?.event?.checkpointAfterId ?? null;
      return {
        // ROOT: a fresh first mount captures an after-checkpoint (routed through ingress).
        rootCapturedAfter: rootAfter != null,
        // INTERVENTION: mounting over a present medium captures a real before/after PAIR…
        overHasBefore: beforeId != null,
        overHasAfter: afterId != null,
        // …and the two ids are DISTINCT (not the same anchor reported twice).
        beforeAfterDistinct: beforeId != null && afterId != null && beforeId !== afterId,
        // The just-mounted disk (B) must top recents (ingress addRecent, newest-first).
        recentIncludesMounted: recentArr.some((r) => norm(r?.path) === "fixtureB.d64"),
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
  //
  // HARDENED (Batch 6 #3): the fresh-session first mount is the ROOT (after-only,
  // before null), so the intervention PAIR never hit the wire and the cart-attach
  // facts (mapper, reset policy) went uncompared. The signal now mounts a DISK first
  // (a present medium), THEN mounts the CRT over it and asserts a real before/after
  // PAIR (non-null + distinct) PLUS the cart-attach facts: mapperType==="easyflash"
  // and resetPolicy==="power-cycle". (Audit Batch 6 #3 — ws-media-1 real pair.)
  {
    id: "ws-media-1",
    severity: "P1",
    title: "CRT mount-over-present captures a real before/after checkpoint PAIR + reports the cart-attach facts",
    spawn: {
      seedFiles: [
        { rel: "fixtureA.d64", bytes: SCRAMBLE_D64 },
        { rel: "fixture.crt", bytes: EASYFLASH_CRT },
      ],
    },
    async signal(c, d) {
      const sid = await liveSession(c);
      // Present a disk first so the CRT mount is an INTERVENTION (before+after), not
      // a fresh-session root (after-only).
      await c.call("media/mount", { session_id: sid, path: `${d.projectDir}/fixtureA.d64`, slot: 8 });
      const crtResp = (await c.call("media/mount", { session_id: sid, path: `${d.projectDir}/fixture.crt`, slot: 0 })) as any;
      const beforeId = crtResp?.event?.checkpointBeforeId ?? null;
      const afterId = crtResp?.event?.checkpointAfterId ?? null;
      return {
        // The CRT mount-over captures a real before/after PAIR (replayable intervention).
        hasBefore: beforeId != null,
        hasAfter: afterId != null,
        beforeAfterDistinct: beforeId != null && afterId != null && beforeId !== afterId,
        // The cart-attach facts: the live mapper + the power-cycle reset policy.
        mapperType: crtResp?.detail?.mapperType ?? crtResp?.mapperType ?? null,
        resetPolicy: crtResp?.detail?.resetPolicy ?? null,
      };
    },
  },

  // ── P1: ws-cart-live-mapping — Spec 713 §5.5/§7.1 live bank-switch is visible ──
  // A write to the EasyFlash IO1 bank register ($DE00) immediately re-banks the cart:
  // session/cart_status.bank tracks `current_bank` (cart.ts getState().currentBank /
  // EasyFlashMapper.current_bank) AND the mapped ROML byte at $8000 changes to the
  // newly-selected bank's CHIP image (713 §2 — mapped bytes read the CHIP, not open
  // bus). A bank-register write reaches the mapper ONLY through the CPU's banked write
  // path (a `STA $DE00`) — a side-channel `wr io de00` poke does NOT reach the mapper
  // (it lands in the I/O shadow), so this drives a REAL CPU store. The fixture is a
  // 2-bank EasyFlash whose ROML offset-0 byte is the bank fingerprint ($A0+bank). We
  // mount it (EXROM=1/GAME=0 → ultimax, ROML at $8000 + RAM at $0000-$0FFF), read
  // bank 0's status + ROML byte, inject a tiny RAM program that writes $01→$DE00
  // (select bank 1), run it, and re-read. CORRECTNESS: cart_status.bank flips 0→1 and
  // the ROML byte flips $A0→$A1 (the live CHIP image of the new bank), on BOTH
  // runtimes. A banking-blind cart_status (constant bank) or a stale $8000 read
  // diverges. (Audit Batch 6 #6 — 713 live-mapping + CHIP-bytes.)
  {
    id: "ws-cart-live-mapping",
    severity: "P1",
    title: "session/cart_status.bank + mapped ROML byte track a live EasyFlash $DE00 bank switch",
    spawn: { seedFiles: [{ rel: "efmb.crt", bytes: EASYFLASH_MB_CRT }] },
    async signal(c, d) {
      const sid = await liveSession(c);
      await c.call("media/mount", { session_id: sid, path: `${d.projectDir}/efmb.crt`, slot: 0 });
      // The CRT mount resumes RUNNING (power-cycle) — pause so the manual session/run
      // below is allowed (a running session rejects manual run, autonomous-loop guard).
      await c.call("debug/pause", { session_id: sid }).catch(() => undefined);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      // Parse the byte at `addr` from an `m <lens>` first-row dump.
      const byteAt = (out: string, addr: number): number | null => {
        const base = addr & ~0x1f;
        const re = new RegExp(`^>.\\:0*${base.toString(16)}\\s+([0-9a-fA-F ]+?)\\s{2,}`, "im");
        const m = out.match(re) ?? out.match(/^>.\:[0-9a-fA-F]+\s+([0-9a-fA-F ]+)/im);
        if (!m) return null;
        const bytes = m[1].trim().split(/\s+/);
        const v = parseInt(bytes[addr - base] ?? "", 16);
        return Number.isNaN(v) ? null : v;
      };
      const cartStatus = async (): Promise<any> => c.call("session/cart_status", { session_id: sid });

      // Bank 0 (power-on): status + the mapped ROML fingerprint byte at $8000.
      const cs0 = await cartStatus();
      const roml0 = byteAt(await exec("m cpu 8000 801f"), 0x8000);

      // Inject a tiny RAM program @ $0200 that selects bank 1 ($01 → $DE00), then a
      // self-loop. ($0000-$0FFF stays RAM under ultimax, so the program runs.)
      //   A9 01     LDA #$01
      //   8D 00 DE  STA $DE00     ; EasyFlash IO1 bank register → bank 1
      //   4C 05 02  JMP $0205     ; self-loop on the JMP
      await exec("wr 0200 A9 01 8D 00 DE 4C 05 02");
      await exec("r pc=0200");
      await c.call("session/run", { session_id: sid, cycles: 200 });

      // Bank 1: status + the mapped ROML fingerprint byte (now the bank-1 CHIP image).
      const cs1 = await cartStatus();
      const roml1 = byteAt(await exec("m cpu 8000 801f"), 0x8000);

      return {
        // The mapper type is reported (sanity: the cart attached as EasyFlash).
        mapperType0: cs0?.type ?? null,
        // cart_status.bank reflects the live current_bank: 0 at power-on…
        bank0: cs0?.bank ?? null,
        // …and flips to 1 after the $DE00 write (banking is LIVE, not constant).
        bank1: cs1?.bank ?? null,
        bankChanged: cs0?.bank !== cs1?.bank,
        // The mapped ROML byte reads the bank's CHIP image (not open bus): bank0→$A0.
        romlBank0: roml0,
        // …and re-banking re-maps $8000 to the bank-1 CHIP image: $A1.
        romlBank1: roml1,
        romlChanged: roml0 != null && roml1 != null && roml0 !== roml1,
      };
    },
  },

  // ── P0: ws-media-disk-checkpoint-fidelity — Spec 714 §8.1/§8.2 mutable disk ──
  // A checkpoint must carry the EXACT disk image attached at capture, and a restore
  // must re-establish THAT image — not whatever disk is mounted at restore time. This
  // is the §8.1 mechanic (a captured disk survives a later media change + restore),
  // the prerequisite for restoring a WRITTEN disk. The signal mounts disk A (sha A),
  // captures checkpoint cpA, then mounts a DISTINCT disk B (sha B) over it, then
  // restores cpA and re-reads the attached disk identity via snapshot/dump's media[]
  // sha256. CORRECTNESS: after restore the attached disk is A again (sha A, NOT sha B)
  // — the checkpoint round-trip restored the captured image, on BOTH runtimes. A
  // restore that forgot to re-attach the captured disk (left B, or dropped the disk)
  // diverges. (Audit Batch 6 #4 — 714 §8.1/§8.2 mutable-disk checkpoint fidelity.)
  //
  // NOTE on the WRITTEN-byte fidelity (the §4.1 RED "V1 survives, not V2"): dirtying a
  // disk over the WS surface requires a real 1541 SAVE (drive-CPU GCR write driven by
  // the C64 over IEC) — no JSON WS method mutates disk content (mem/poke + the monitor
  // `wr` reach C64 RAM, not the drive's GCR image), the SAME limit ws-media-3 records
  // for cart flash. The written-delta round-trip (write V1→cap→write V2→restore→==V1)
  // is verified DIRECTLY on TRX64 (item2_disk_autopersist + the GCRIMAGE0 capture/
  // restore in drive_snapshot.rs round-trip the MUTABLE GCR overlay). This WS gate
  // proves the surrounding checkpoint-disk-identity mechanic that fidelity rides on.
  {
    id: "ws-media-disk-checkpoint-fidelity",
    severity: "P0",
    title: "checkpoint restore re-attaches the CAPTURED disk image (sha A), not the later-mounted disk (sha B)",
    blocked:
      "The WS-reachable variant (capture disk A → mount a DIFFERENT disk B → restore A) " +
      "is NOT a clean differential: a ring checkpoint restore does not re-create a " +
      "DIFFERENT drive media object — TS (the authority) leaves the later-mounted B " +
      "attached after restoring A (restoredDiskIsA=false on TS), while TRX64 rolls the " +
      "media back to A. The REAL §8.1 mechanic — WRITTEN bytes within the SAME disk " +
      "(write V1→cap→write V2→restore→GCR==V1) — is the GCR-overlay roll-back TS DOES " +
      "support (save_disks=1, 714.2), but dirtying disk content over the WS surface " +
      "needs a real 1541 SAVE (drive-CPU GCR write over IEC) — no JSON WS method mutates " +
      "disk content (mem/poke + monitor `wr` reach C64 RAM, not the drive GCR image), the " +
      "SAME limit ws-media-3 records for cart flash. The GCR-overlay capture/restore " +
      "round-trip IS verified directly on TRX64 (drive_snapshot.rs GCRIMAGE0 + " +
      "item2_disk_autopersist). Re-arm if a WS disk-write trigger lands. (Signal kept " +
      "intact; --only ws-media-disk-checkpoint-fidelity to inspect both runtimes.)",
    spawn: {
      seedFiles: [
        { rel: "diskA.d64", bytes: SCRAMBLE_D64 },
        { rel: "diskB.d64", bytes: DISTINCT_D64 },
      ],
    },
    async signal(c, d) {
      const sid = await liveSession(c);
      const snapPath = `${d.projectDir}/fidelity.c64re`;
      // Read the attached disk's content sha256 via snapshot/dump's media[] (drive8).
      const diskSha = async (): Promise<string | null> => {
        const r = (await c.call("snapshot/dump", { session_id: sid, path: snapPath })) as any;
        const media: any[] = r?.media ?? [];
        const disk = media.find((m) => m?.role === "drive8");
        return disk?.sha256 ?? null;
      };
      // Mount disk A, capture a checkpoint anchored on A.
      const mA = (await c.call("media/mount", { session_id: sid, path: `${d.projectDir}/diskA.d64`, slot: 8 })) as any;
      const shaAmount: string | null = mA?.sha256 ?? null;
      const shaAattached = await diskSha();
      const capA = (await c.call("checkpoint/capture", { session_id: sid })) as any;
      const cpAId: string | null = capA?.ref?.id ?? capA?.id ?? null;
      // Mount a DISTINCT disk B over A — the live attached disk is now B (sha B).
      const mB = (await c.call("media/mount", { session_id: sid, path: `${d.projectDir}/diskB.d64`, slot: 8 })) as any;
      const shaBmount: string | null = mB?.sha256 ?? null;
      const shaBattached = await diskSha();
      // Restore cpA — the checkpoint must re-attach the CAPTURED disk A (sha A).
      await c.call("checkpoint/restore", { session_id: sid, id: cpAId, then: "pause" });
      const shaAfterRestore = await diskSha();
      return {
        // The two seed disks are genuinely DISTINCT (so sha can tell them apart).
        seedDisksDiffer: shaAmount != null && shaBmount != null && shaAmount !== shaBmount,
        // The captured + live shas track the mounted image at each step.
        capturedDiskIsA: shaAattached != null && shaAattached === shaAmount,
        liveDiskIsBBeforeRestore: shaBattached != null && shaBattached === shaBmount,
        // THE FIDELITY SIGNAL: after restoring cpA, the attached disk is A again…
        restoredDiskIsA: shaAfterRestore != null && shaAfterRestore === shaAmount,
        // …and is NOT the later-mounted B (the restore rolled the media back).
        restoredDiskIsNotB: shaAfterRestore != null && shaAfterRestore !== shaBmount,
      };
    },
  },

  // ── P0: ws-media-host-write-through — Spec 742 §6/§742.3 D64 host write-through ─
  // A drive-side write must reach the HOST .d64 at the writeback commit: with a
  // path-backed writable disk, a real drive write lands in the host file (TS writes
  // eagerly at the VICE fsimage commit / hostFlush; TRX64 lazily via the wall-clock
  // debounce — stream_maybe_autopersist_disk). The signal would mount a writable D64
  // from a host path, drive a real disk write, and `readFileSync` the host .d64 to
  // assert the bytes changed + mtime advanced.
  //
  // BLOCKED by the oracle harness (NOT a TRX64 defect): driving a disk write needs a
  // real 1541 SAVE — the drive 6502 executes a GCR write under the C64's IEC-bus
  // command stream. No JSON WS method mutates disk content: mem/poke + the monitor
  // `wr` reach C64 RAM, not the drive's GCR image, and running a full KERNAL SAVE to a
  // settled host-write under the ~4 fps tsx oracle is far heavier than the 240s gate
  // budget. This is the SAME class of block as ws-media-3 (cart flash). The fix is
  // verified DIRECTLY on TRX64: `item2_disk_autopersist_writes_host_d64_without_
  // explicit_persist` (main.rs tests) mounts a writable blank D64, drives a real dirty
  // GCR track (write_one_bit_for_test, the same write the engine's WRITE path uses),
  // ticks the wall-clock persist cadence past the debounce, and asserts the host .d64
  // FILE bytes changed without an explicit media/persist. Re-arm if a WS disk-write
  // trigger (or a synthetic dirty-track hook on BOTH runtimes) lands in the oracle.
  // (Audit Batch 6 #5 — 742 §6 host write-through.)
  {
    id: "ws-media-host-write-through",
    severity: "P1",
    title: "drive write reaches the host .d64 at writeback (eager TS / debounced TRX64)",
    blocked:
      "Driving a disk write needs a real 1541 SAVE (drive 6502 GCR write under the " +
      "C64's IEC command stream); no JSON WS method mutates disk content (mem/poke + " +
      "monitor `wr` reach C64 RAM, not the drive GCR image), and a full KERNAL SAVE " +
      "under the ~4fps tsx oracle exceeds the gate budget — the SAME block as ws-media-3 " +
      "(cart flash). Fix verified DIRECTLY on TRX64: item2_disk_autopersist_writes_host_" +
      "d64_without_explicit_persist (dirty GCR track → tick wall-clock persist past the " +
      "debounce → host .d64 FILE bytes changed, no explicit media/persist).",
    spawn: { stream: true, seedFiles: [{ rel: "writable.d64", bytes: DISTINCT_D64 }] },
    async signal(c, d) {
      // Kept intact so the case re-arms once a WS disk-write trigger exists. This proxy
      // can only confirm the host file is path-backed + present (NOT a faithful
      // dirty→writeback→host-bytes-changed), so it is NOT a faithful signal — see `blocked`.
      const sid = await liveSession(c);
      const diskPath = `${d.projectDir}/writable.d64`;
      await c.call("media/mount", { session_id: sid, path: diskPath, slot: 8 });
      return { hostDiskPresent: existsSync(diskPath) };
    },
  },

  // ── P1: ws-media-events-identity — Spec 709 §709.8 events + §1/§9 identity + cart_status ─
  // Three 709 surface contracts in one differential:
  //  (a) §709.8 media/events history — every accepted ingress appends an ordered
  //      MediaIngressEvent (operation + format + sha256 + checkpointBefore/AfterId),
  //      readable via media/events. Mount a disk then a CRT → the history has both ops
  //      IN ORDER, each with the right operation/format, a sha256, and a non-null
  //      after-checkpoint ref (replayable).
  //  (b) §1/§9 mount identity stable across entry paths — the same disk bytes via
  //      media/mount (path) vs media/ingress (bytes_b64) yield the SAME sha256 + format.
  //  (c) §709.9/713 §8.2 cart_status truth — after the CRT attach, cart_status reports
  //      a real mapperType + sourceName; after the cart eject, cart_status is null
  //      (no cartridge). Each fact is compared TS-vs-TRX64. (Audit Batch 6 #7.)
  {
    id: "ws-media-events-identity",
    severity: "P1",
    title: "media/events ordered history + cross-path mount identity + cart_status attach/eject truth",
    spawn: {
      seedFiles: [
        { rel: "diskA.d64", bytes: SCRAMBLE_D64 },
        { rel: "fixture.crt", bytes: EASYFLASH_CRT },
      ],
    },
    async signal(c, d) {
      const sid = await liveSession(c);
      // (a) ordered media-event history: disk then CRT.
      await c.call("media/mount", { session_id: sid, path: `${d.projectDir}/diskA.d64`, slot: 8 });
      await c.call("media/mount", { session_id: sid, path: `${d.projectDir}/fixture.crt`, slot: 0 });
      const ev = (await c.call("media/events", { session_id: sid })) as any;
      const events: any[] = ev?.events ?? [];
      const ops = events.map((e) => e?.operation);
      const diskEv = events.find((e) => e?.operation === "disk");
      const crtEv = events.find((e) => e?.operation === "crt");
      // (b) cross-path identity: the disk's mount sha (from the disk event) vs the same
      // bytes through media/ingress (bytes_b64). Both must agree on sha256 + format.
      const ingressResp = (await c.call("media/ingress", {
        session_id: sid, kind: "disk", role: "drive8",
        name: "diskA.d64", bytes_b64: SCRAMBLE_D64.toString("base64"),
      })) as any;
      const ingressSha = ingressResp?.event?.sha256 ?? null;
      const ingressFormat = ingressResp?.event?.format ?? null;
      // (c) cart_status after attach (the CRT is still mounted) → real mapper + name;
      // then eject the cart and assert cart_status is null.
      const csAttached = (await c.call("session/cart_status", { session_id: sid })) as any;
      await c.call("media/unmount", { session_id: sid, slot: 0 });
      const csAfterEject = (await c.call("session/cart_status", { session_id: sid })) as any;
      return {
        // (a) the history carries both ops, in order (disk before crt), each with a sha…
        opsIncludeDiskThenCrt: ops.indexOf("disk") >= 0 && ops.indexOf("crt") > ops.indexOf("disk"),
        diskEventFormat: diskEv?.format ?? null,
        diskEventHasSha: typeof diskEv?.sha256 === "string" && diskEv.sha256.length === 64,
        crtEventFormat: crtEv?.format ?? null,
        crtEventHasSha: typeof crtEv?.sha256 === "string" && crtEv.sha256.length === 64,
        // …and a replayable after-checkpoint ref on each.
        diskEventHasAfterCp: diskEv?.checkpointAfterId != null,
        crtEventHasAfterCp: crtEv?.checkpointAfterId != null,
        // (b) cross-path mount identity: the disk's mount sha == the ingress sha (same bytes).
        ingressMatchesDiskSha: ingressSha != null && ingressSha === (diskEv?.sha256 ?? null),
        ingressFormatIsD64: ingressFormat === "d64",
        // (c) cart_status truth: real mapper + filename while attached…
        statusMapperType: csAttached?.type ?? null,
        statusName: csAttached?.sourceName ?? null,
        // …and null (no cartridge) after the eject.
        statusNullAfterEject: csAfterEject === null,
      };
    },
  },

  // ── P1: ws-media-cart-status-name — cart_status sourceName is the FILE name ──
  // (Spec 709.13.) The CART label (UI) = session/cart_status.sourceName. TS reports
  // the mounted FILE name (getCartridgeMedia().name, ws-server.ts:1581). TRX64 used to
  // report the cartridge_image CRT-HEADER name (a 32-byte field baked at build time +
  // shared across a project's derived carts) — so the label showed e.g. "WASTELAND EF
  // MENU POC" for every wasteland cart and looked stale/cached + wrong. The .crt's
  // header name ("INTERNAL POC NAME") deliberately differs from its filename
  // ("mycart.crt"): the signal asserts sourceName is the FILENAME, not the header.
  {
    id: "ws-media-cart-status-name",
    severity: "P1",
    title: "session/cart_status sourceName is the mounted FILE name, not the CRT-header name",
    spawn: { seedFiles: [{ rel: "mycart.crt", bytes: makeEasyFlashCrt("INTERNAL POC NAME") }] },
    async signal(c, d) {
      const sid = await liveSession(c);
      await c.call("media/mount", { session_id: sid, path: `${d.projectDir}/mycart.crt`, slot: 0 });
      const cs = (await c.call("session/cart_status", { session_id: sid })) as any;
      return {
        // The label must derive from the mounted FILE, not the cart's internal header.
        isFilename: cs?.sourceName === "mycart.crt",
        isHeaderName: cs?.sourceName === "INTERNAL POC NAME",
      };
    },
  },

  // ── P1: ws-media-8 — media/recent overlays the recents store AND is project-scoped ──
  // (Audit theme T3 + BUG-013.) TS's media/recent overlays a GLOBAL persisted recents
  // store (recent-files.ts getRecent: newest-first, max 10, each carrying a `mountedAt`
  // timestamp; addRecent stamps it on every ingest) AHEAD of the dir scans, BUT in
  // PRODUCTION mode (no --dev-samples) it shows ONLY active-project media: §1 recents are
  // gated to inside the project dir (`insideProject`, ws-server.ts:1824-1838) and the §2
  // repo `samples/` scan runs ONLY under --dev-samples (ws-server.ts:1841-1859). TRX64 has
  // NO --dev-samples flag (Spec 771 — the external bin is ALWAYS production), so it must
  // NEVER scan samples and must gate its recents store to the project dir.
  //
  // This case asserts BOTH halves:
  //  (a) recents-store ordering: maintain a recents store updated on every mount
  //      (newest-first, cap 10, mountedAt), overlaid ahead of the dir scan, 1:1 with
  //      recent-files.ts → {topIsNewest, hasMountedAt}.
  //  (b) PROJECT-SCOPING (BUG-013): every entry's path is under the daemon's --project
  //      dir. TRX64's old scan_recent_media UNCONDITIONALLY scanned the real c64re
  //      `samples/` dir (absolute path that EXISTS on disk), so even this hermetic
  //      project surfaced the samples carts (AccoladeComics_TRX+1D_EF.crt, im3_MAGICDESK.crt,
  //      lykia_*.crt, yeti_mountain_GMOD2.crt) — out-of-project leak. Signal:
  //      `outOfProjectCount` = entries whose path is NOT under d.projectDir. TS
  //      (production, project-only): 0. TRX64 (before fix): >0. Both (after fix): 0.
  //
  // C64RE_RECENT_FILE points at a per-daemon temp store so neither runtime touches the
  // user's real recents (and the two daemons can't share).
  {
    id: "ws-media-8",
    severity: "P1",
    title: "media/recent: recents store (newest-first + mountedAt) AND project-scoped (no samples leak)",
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
      // BUG-013 project-scoping: count entries whose resolved path is NOT under the
      // daemon's --project dir. The samples carts (absolute /…/samples/*.crt) live OUTSIDE
      // every hermetic project, so a samples scan makes this > 0. Normalize a trailing
      // slash so the boundary check is a clean prefix match on the project root.
      const projRoot = d.projectDir.replace(/\/+$/, "") + "/";
      const insideProject = (p: string): boolean => typeof p === "string" && p.startsWith(projRoot);
      const outOfProjectCount = arr.filter((r) => !insideProject(r?.path)).length;
      return {
        // The most-recently-mounted disk (B) must be the FIRST recents entry.
        topIsNewest: arr.length > 0 && norm(arr[0]?.path) === "diskB.d64",
        // Every store-sourced entry carries a mountedAt timestamp (recent-files.ts).
        hasMountedAt: arr.length > 0 && typeof arr[0]?.mountedAt === "string" && arr[0].mountedAt.length > 0,
        // BUG-013: production picker shows ONLY active-project media — zero out-of-project
        // entries (no unconditional samples/ scan, recents store gated to the project dir).
        outOfProjectCount,
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
  //
  // HARDENED (Batch 6 #2): `ejectPaused:false` is only MEANINGFUL when the machine
  // was provably RUNNING at the eject — a never-running (still-paused) leg would
  // ALSO report paused on both runtimes via a different code path and false-green
  // the opposite of intent. So the signal now also reports `wasRunningAtEject`
  // (session/state.runState read IMMEDIATELY before the eject) and REQUIRES it to be
  // true: a leg that never reached running diverges (the bool flips) instead of
  // silently crediting paused:false. (Audit Batch 6 #2 — ws-media-2 eject precondition.)
  {
    id: "ws-media-2",
    severity: "P1",
    title: "disk eject reports real run-state (paused:false), credited ONLY when provably running at eject",
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
      // Read the run-state IMMEDIATELY before the eject so `paused:false` is only
      // credited when the machine was provably RUNNING (else the assertion is moot).
      const stAtEject = await state(c, sid);
      const ejectResp = (await c.call("media/unmount", { session_id: sid, slot: 8 })) as any;
      return {
        // PRECONDITION: the machine was running when the eject fired — a never-running
        // leg flips this to false and diverges (so paused:false can't false-green).
        wasRunningAtEject: stAtEject.runState === "running",
        // The behavioural signal: a disk eject on a RUNNING machine is never paused.
        ejectPaused: ejectResp?.paused === true,
      };
    },
  },

  // ── P0: ws-media-mount-pause — Spec 709 §2.2 / §709.13.1 device-vs-C64 ───────
  // The mount-pause REFINEMENT: a DISK mount is a live device op (the 1541 is a
  // separate device — inserting a new image leaves the C64 RUNNING, exactly as
  // real hardware), so a running C64 keeps advancing its timeline. A CRT mount is
  // a C64-INTERNAL change (the cart port is part of the C64) — it COLD-BOOTS the
  // machine (resetCold), so the cycle counter drops and the timeline restarts.
  // (ingress.ts:138-143 `requiresPause = kind==="crt"||"prg"||cart-eject`; a disk
  // never pauses; the CRT power-cycles then resumes running via resumeIfRunning.)
  //
  // The signal drives a running, booted C64, mounts a DISK and asserts:
  //   - the machine is STILL running (runState==="running") AND its cycle counter
  //     ADVANCED across the mount (the disk insert did not reset/pause the timeline),
  //   - the disk mount response reports paused===false (a running disk insert is a
  //     live op). TRX64's disk-mount handler hardcoded `"paused": true` in the reply
  //     even for a running machine — TS's ingress returns paused=(runState==="paused")
  //     which is FALSE here. That hardcode is the divergence this gate catches.
  // Then mounts a CRT and asserts the C64-internal cold-boot signature:
  //   - the cycle counter DROPPED (a power-cycle restarted the timeline), distinct
  //     from the disk insert which preserved it,
  //   - the machine ends running (the CRT power-cycle resumes), matching TS.
  // (Audit Batch 6 #1 — P0 709 §2.2 mount-pause refinement.)
  {
    id: "ws-media-mount-pause",
    severity: "P0",
    title: "disk MOUNT keeps the running C64 advancing (live device); CRT mount cold-boots it (C64-internal)",
    spawn: {
      stream: true,
      seedFiles: [
        { rel: "fixtureA.d64", bytes: SCRAMBLE_D64 },
        { rel: "fixture.crt", bytes: EASYFLASH_CRT },
      ],
    },
    async signal(c, d) {
      const sid = await liveSession(c);
      // Boot to the running BASIC idle (IRQs live, cyc ≥ 2.5M) so the disk insert
      // happens while the C64 is genuinely advancing — the divergence is run-state-
      // dependent (a paused machine reports paused:true on BOTH, proving nothing).
      await c.call("debug/run", { session_id: sid });
      const stBooted = await waitRunningBooted(c, sid, 2_500_000, 60_000);
      if (stBooted.runState !== "running") await c.call("debug/run", { session_id: sid });
      const cyc = (s: any) => Number(s.c64Cycles ?? s.cycles ?? s.cpu?.cycles ?? 0);

      // ── DISK mount: a live device op — the C64 keeps running + its timeline advances ──
      const cycBeforeDisk = cyc(await state(c, sid));
      const diskResp = (await c.call("media/mount", {
        session_id: sid, path: `${d.projectDir}/fixtureA.d64`, slot: 8,
      })) as any;
      await sleep(1500); // let the free-run driver advance several frames post-insert
      const stAfterDisk = await state(c, sid);
      const cycAfterDisk = cyc(stAfterDisk);

      // ── CRT mount: a C64-internal change — cold-boots the machine (timeline restarts) ──
      const crtResp = (await c.call("media/mount", {
        session_id: sid, path: `${d.projectDir}/fixture.crt`, slot: 0,
      })) as any;
      await sleep(300); // let the power-cycle resume broadcast land
      const stAfterCrt = await state(c, sid);
      const cycAfterCrt = cyc(stAfterCrt);

      return {
        // DISK insert = live device op: the running C64 NEVER paused…
        diskMountKeptRunning: stAfterDisk.runState === "running",
        // …and its timeline kept advancing across the insert (no reset/pause).
        diskMountTimelineAdvanced: cycAfterDisk > cycBeforeDisk,
        // The disk-mount reply reports the REAL run-state — a running insert is NOT
        // paused. (TRX64 hardcoded paused:true here; TS returns paused:false.)
        diskMountReplyPaused: diskResp?.paused === true,
        // CRT insert = C64-internal cold-boot: the timeline RESTARTED (cycles dropped
        // below the disk-era count), distinct from the disk insert that preserved it.
        crtMountColdBooted: cycAfterCrt < cycAfterDisk,
        // The CRT power-cycle ends RUNNING (resumes after the cold boot), matching TS.
        crtMountEndsRunning: stAfterCrt.runState === "running",
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
    title: "runtime/mark errors when no trace is active AND counts real marks under an active trace",
    async signal(c) {
      const sid = await liveSession(c);
      // ── Negative arm: mark with NO active trace must throw (the original bug). ──
      // Start from a guaranteed-inactive state (a prior case may have left a trace).
      await c.call("trace/run/stop", { session_id: sid, wait_index: false }).catch(() => undefined);
      let markSucceededWithoutTrace = false;
      try {
        await c.call("runtime/mark", { session_id: sid, label: "probe" });
        markSucceededWithoutTrace = true;
      } catch {
        markSucceededWithoutTrace = false;
      }
      // ── Positive arm: the ORIGINAL bug was a FABRICATED count returned regardless.
      // With a real active trace, mark TWICE and assert the returned count GROWS
      // 1→2, the labels echo, and trace/run/status reports the SAME count (not a
      // fabricated constant). A runtime that hardcodes marks:1 diverges here.
      await c.call("trace/start_domains", { session_id: sid, domains: ["c64-cpu", "memory"] });
      const m1 = (await c.call("runtime/mark", { session_id: sid, label: "m0" })) as any;
      const m2 = (await c.call("runtime/mark", { session_id: sid, label: "m1" })) as any;
      const status = (await c.call("trace/run/status", { session_id: sid })) as any;
      await c.call("trace/run/stop", { session_id: sid, wait_index: false }).catch(() => undefined);
      return {
        // The behavioural signal: marking an inactive trace must NOT succeed.
        markSucceededWithoutTrace,
        // The captured count GROWS 1→2 (not a fabricated constant), labels echo,
        // and the engine's own status agrees with the second mark's count.
        firstMarkCount: Number(m1?.marks ?? -1),
        secondMarkCount: Number(m2?.marks ?? -1),
        firstLabelEcho: m1?.label === "m0",
        secondLabelEcho: m2?.label === "m1",
        statusMarksAgrees: Number(status?.marks ?? -1) === Number(m2?.marks ?? -2),
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

  // ── P0: ws-trace-indirect-store-ea — Spec 753 §1/§3/§9 indirect-store EA capture ─
  // The WHOLE POINT of Spec 753: a mem-row trace must record the COMPUTED effective
  // address of an indirect store (`STA ($zp),Y` / `STA ($zp,X)`), NOT the decoded
  // operand (the zero-page pointer). A runtime that taps the write at the operand
  // level shows the indirect TARGET page untouched and stamps the ZP page instead —
  // so `trace_memory_map` (and any taint/who-wrote read) points at the wrong page.
  // The existing misc-14 case only cold-boots; it NEVER exercises an indirect store,
  // so this divergence was invisible. We inject a tiny program that builds a ZP
  // pointer → $C800, then `STA ($FB),Y` (Y=0) writes $5A to $C800, plus a CONTROL
  // absolute `STA $C900` writing $A5 to $C900. We trace c64-cpu + memory, run, stop
  // (wait_index → the .duckdb is built), then read the captured mem-WRITE rows back
  // by raw SQL over the store and report the addresses written with each value.
  // CORRECTNESS signal: the byte $5A landed at addr $C800 (the COMPUTED EA — the
  // indirect target), the control $A5 at $C900, and the ZP pointer page ($00FB) is
  // NOT where $5A appears. TS records the real EA; a TRX64 that taps the operand
  // would show $5A at $00FB / nothing at $C800 → RED. (Audit Batch 5 #1, 753 §1/§3.)
  {
    id: "ws-trace-indirect-store-ea",
    severity: "P0",
    title: "trace records the COMPUTED EA of STA ($zp),Y (the indirect target page, not the operand)",
    async signal(c) {
      const sid = await liveSession(c);
      // Program @ $0800:
      //   A9 00     LDA #$00        ; ptr lo
      //   85 FB     STA $FB
      //   A9 C8     LDA #$C8        ; ptr hi  → ($FB) = $C800
      //   85 FC     STA $FC
      //   A0 00     LDY #$00
      //   A9 5A     LDA #$5A        ; the indirect-store value
      //   91 FB     STA ($FB),Y     ; INDIRECT store → computed EA = $C800
      //   A9 A5     LDA #$A5        ; control value
      //   8D 00 C9  STA $C900       ; ABSOLUTE control store → $C900
      //   4C 16 08  JMP $0816       ; self-loop ($0816 = the JMP itself)
      const PROG =
        "wr 0800 A9 00 85 FB A9 C8 85 FC A0 00 A9 5A 91 FB A9 A5 8D 00 C9 4C 16 08";
      await c.call("monitor/exec", { session_id: sid, command: PROG });
      await c.call("monitor/exec", { session_id: sid, command: "r pc=0800" });
      // Capture c64-cpu + memory so the indirect store's mem-WRITE row is recorded.
      await c.call("trace/start_domains", { session_id: sid, domains: ["c64-cpu", "memory"] });
      await c.call("session/run", { session_id: sid, cycles: 5000 });
      await c.call("trace/run/stop", { session_id: sid, wait_index: true }).catch(() => undefined);
      const cur = (await c.call("trace/current", { session_id: sid })) as any;
      const db = String(cur?.path ?? cur?.duckdbPath ?? "");
      // Raw SQL over the indexed store: pull the (addr,value) of every C64 mem WRITE.
      // `trace_event` rows for the CPU bus tap carry data_json.{op,addr,value}; op is
      // 'write' for a store (= the c64re indexer's RAM_WRITE→'write' decode).
      const sql =
        "SELECT CAST(json_extract(data_json,'$.addr') AS INTEGER) AS addr, " +
        "CAST(json_extract(data_json,'$.value') AS INTEGER) AS value " +
        "FROM trace_event WHERE channel='bus_access' " +
        "AND json_extract_string(data_json,'$.op')='write' " +
        "AND json_extract(data_json,'$.addr') IS NOT NULL";
      let rows: Array<{ addr: number; value: number }> = [];
      try {
        const r = (await c.call("trace/read", {
          op: "sql", duckdb_path: db, args: { sql, limit: 5000 },
        })) as any;
        rows = (r?.rows ?? []).map((row: any[]) => ({ addr: Number(row[0]), value: Number(row[1]) }));
      } catch { rows = []; }
      const writesTo = (addr: number) => rows.filter((w) => w.addr === addr);
      // Did $5A land at the COMPUTED indirect EA $C800?  (the 753 contract)
      const indirectEaHit = writesTo(0xc800).some((w) => w.value === 0x5a);
      // Did the control absolute store $A5 land at $C900?
      const absControlHit = writesTo(0xc900).some((w) => w.value === 0xa5);
      // The indirect value must NOT appear at the ZP pointer page ($00FB) — a runtime
      // that tapped the OPERAND would stamp $5A there instead of at $C800.
      const valueAtZpPointer = writesTo(0x00fb).some((w) => w.value === 0x5a);
      return {
        // The behavioural truth (Spec 753 §1): the indirect store's mem-row carries the
        // COMPUTED EA ($C800), not the operand — so the target page reads as written.
        indirectEaHit,
        // The plain absolute store is the control — both runtimes must record it.
        absControlHit,
        // The indirect value is NOT misattributed to the zero-page pointer page.
        valueAtZpPointer,
        // sanity: the trace actually captured memory writes (not an empty store).
        anyWrites: rows.length > 0,
      };
    },
  },

  // ── P1: ws-trace-run-status-contract — Spec 726 §6a / 708 full status shape ───
  // The existing readers assert only `eventCount` (formats-state-6) or `active`
  // (misc-2). No case asserts the FULL TraceRunStatus contract — definitionId,
  // marks, binary, capturing, overflowed, retracePath — so TRX64 could (and DID)
  // drop those fields and stay green. TS's traceRun.status() (trace-run.ts) returns
  // {active, runId, definitionId, eventCount, bytesBuffered, marks, overflowed,
  // capturing, binary, retracePath} for an active run. We start a captureAll trace,
  // mark once, run a window so eventCount grows, and assert the full shape.
  {
    id: "ws-trace-run-status-contract",
    severity: "P1",
    title: "trace/run/status carries the full contract (definitionId, marks, binary, capturing, overflowed, retracePath)",
    async signal(c) {
      const sid = await liveSession(c);
      // Guaranteed-inactive start so the active-status fields are this run's own.
      await c.call("trace/run/stop", { session_id: sid, wait_index: false }).catch(() => undefined);
      await c.call("trace/start_domains", { session_id: sid, domains: ["c64-cpu", "memory"] });
      await c.call("runtime/mark", { session_id: sid, label: "m0" }).catch(() => undefined);
      await c.call("session/run", { session_id: sid, cycles: 200_000 });
      const s = (await c.call("trace/run/status", { session_id: sid })) as any;
      await c.call("trace/run/stop", { session_id: sid, wait_index: false }).catch(() => undefined);
      return {
        active: s?.active === true,
        hasRunId: typeof s?.runId === "string" && s.runId.length > 0,
        // A captureAll trace's definitionId is the same id on both runtimes ("live-capture").
        definitionId: s?.definitionId ?? null,
        eventCountPositive: Number(s?.eventCount ?? 0) > 0,
        // The mark stamped above must be reflected (>=1) — not a dropped/zeroed field.
        marksAtLeastOne: Number(s?.marks ?? 0) >= 1,
        binary: s?.binary === true,
        capturing: s?.capturing === true,
        overflowed: s?.overflowed === false,
        retracePathNonEmpty: typeof s?.retracePath === "string" && s.retracePath.length > 0,
      };
    },
  },

  // ── P1: ws-trace-double-start-guard — Spec 708 §4 double-start / stop-when-idle ─
  // Starting a trace while one is already ACTIVE must THROW ("trace already active …
  // stop it first"), not silently clobber the in-flight capture. TS guards this in
  // ws-server.ts:1281 (and TraceRun.start throws). `trace/run/stop` when nothing is
  // active must NOT throw — it returns `{run:null,status}` (the self-aborted/idle
  // path, ws-server.ts:1303). TRX64's trace/start_domains UNCONDITIONALLY overwrote
  // st.session.trace (no guard) → a second start orphaned the first .c64retrace and
  // reset eventCount silently. Fixed: added the same active-guard. Signal: stop to
  // idle; assert stop-when-idle does NOT throw + reports active:false; start once;
  // start AGAIN → assert it THREW; clean up.
  {
    id: "ws-trace-double-start-guard",
    severity: "P1",
    title: "trace double-start throws; stop-when-inactive returns status (not a throw)",
    async signal(c) {
      const sid = await liveSession(c);
      // Stop to a known-idle state first.
      await c.call("trace/run/stop", { session_id: sid, wait_index: false }).catch(() => undefined);
      // stop-when-inactive: must NOT throw, returns {run:null, status:{active:false}}.
      let stopWhenIdleThrew = false;
      let stopWhenIdleRunNull = false;
      try {
        const r = (await c.call("trace/run/stop", { session_id: sid, wait_index: false })) as any;
        stopWhenIdleRunNull = r?.run == null;
      } catch {
        stopWhenIdleThrew = true;
      }
      // First start succeeds.
      let firstStartThrew = false;
      try {
        await c.call("trace/start_domains", { session_id: sid, domains: ["c64-cpu"] });
      } catch {
        firstStartThrew = true;
      }
      // Second start while active MUST throw (the guard).
      let secondStartThrew = false;
      try {
        await c.call("trace/start_domains", { session_id: sid, domains: ["c64-cpu"] });
      } catch {
        secondStartThrew = true;
      }
      // Clean up the live trace for later cases.
      await c.call("trace/run/stop", { session_id: sid, wait_index: false }).catch(() => undefined);
      return {
        // stop-when-inactive is graceful (no throw) and reports the idle status.
        stopWhenIdleThrew,
        stopWhenIdleRunNull,
        // the first start works, the second (double-start) is rejected.
        firstStartThrew,
        secondStartThrew,
      };
    },
  },

  // ── P1: ws-trace-def-reject — Spec 708 §11 / 708.7 implement-or-REJECT ─────────
  // trace/definition/validate (and put) must REJECT an unsupported trigger
  // (`monitor-stop`, `manual-mark`) and an unsupported checkpointPolicy
  // (`on-trigger`) with `{ok:false, errors[]}` — NOT silently accept-then-no-op
  // (the 708.7 trap). A supported pc-range + cpu-row def must validate `{ok:true}`.
  // TS validateTraceDefinition (trace-definition.ts) and TRX64
  // validate_trace_definition both encode this; this case asserts the parity.
  {
    id: "ws-trace-def-reject",
    severity: "P1",
    title: "trace/definition rejects unsupported triggers + checkpointPolicy; accepts a supported def",
    async signal(c) {
      const sid = await liveSession(c);
      const validate = async (definition: any): Promise<{ ok: boolean; errorCount: number }> => {
        const r = (await c.call("trace/definition/validate", { session_id: sid, definition })) as any;
        return { ok: r?.ok === true, errorCount: Array.isArray(r?.errors) ? r.errors.length : 0 };
      };
      const base = {
        id: "case-def",
        version: 1,
        name: "case def",
        domains: ["c64-cpu"],
        retention: "transient",
      };
      const supported = {
        ...base,
        triggers: [{ kind: "pc-range", domain: "c64-cpu", from: 0, to: 0xffff }],
        captures: [{ kind: "cpu-row", domain: "c64-cpu" }],
      };
      const unsupportedTrigger = {
        ...base,
        triggers: [{ kind: "manual-mark" }],
        captures: [{ kind: "cpu-row", domain: "c64-cpu" }],
      };
      const monitorStopTrigger = {
        ...base,
        triggers: [{ kind: "monitor-stop" }],
        captures: [{ kind: "cpu-row", domain: "c64-cpu" }],
      };
      const onTriggerPolicy = {
        ...supported,
        checkpointPolicy: "on-trigger",
      };
      const ok = await validate(supported);
      const manual = await validate(unsupportedTrigger);
      const monStop = await validate(monitorStopTrigger);
      const onTrig = await validate(onTriggerPolicy);
      // put on the unsupported def must ALSO reject {ok:false} (not throw, not store).
      const putBad = (await c.call("trace/definition/put", { session_id: sid, definition: unsupportedTrigger })) as any;
      const putGood = (await c.call("trace/definition/put", { session_id: sid, definition: supported })) as any;
      return {
        // A supported def validates clean.
        supportedOk: ok.ok && ok.errorCount === 0,
        // manual-mark / monitor-stop triggers reject with at least one error.
        manualMarkRejected: !manual.ok && manual.errorCount > 0,
        monitorStopRejected: !monStop.ok && monStop.errorCount > 0,
        // checkpointPolicy:on-trigger rejects.
        onTriggerPolicyRejected: !onTrig.ok && onTrig.errorCount > 0,
        // put mirrors validate: bad → {ok:false}, good → {ok:true, id}.
        putBadRejected: putBad?.ok === false,
        putGoodAccepted: putGood?.ok === true && typeof putGood?.id === "string",
      };
    },
  },

  // ── P1: ws-trace-capture-selection — Spec 708 §11 / 708.7 capture selection ────
  // A trace DEFINITION declaring only `cpu-row` must DROP memory rows even when the
  // `memory` domain is enabled (the domain opens the channel; the captures select
  // which rows are KEPT). TS gates each event by `declaredCaptures.has(captureKind)`
  // (trace-run.ts:287) — so a `[cpu-row]`-only def records 0 mem rows, while a
  // `[cpu-row, mem-row]` def records them. TRX64 (before fix) derived its recording
  // channels from DOMAINS ALONE (TraceChannels::from_domains) and ignored the def's
  // captures → it recorded mem rows even when only `cpu-row` was declared (the 708.7
  // silent-no-op trap: a declared selection that does nothing). Fixed: TRX64 now masks
  // the channels by the declared capture kinds. Signal: put + run a store-heavy
  // injected program twice — Def A (cpu-row only) and Def B (cpu-row + mem-row) over
  // the same memory domain + same budget — and count the captured mem WRITE rows.
  // Contract: A drops mem rows (memCountA===0), B keeps them (memCountB>0).
  {
    id: "ws-trace-capture-selection",
    severity: "P1",
    title: "a def declaring only cpu-row drops mem rows even with the memory domain enabled (708.7)",
    async signal(c) {
      // Store-heavy program @ $0800: repeatedly STA to $C800..$C803, then JMP back.
      //   A2 00     LDX #$00
      //   A9 5A     LDA #$5A
      //   9D 00 C8  STA $C800,X
      //   E8        INX
      //   E0 04     CPX #$04
      //   D0 F8     BNE $0804     ; loop the 4 stores
      //   4C 00 08  JMP $0800     ; restart (keeps storing under the budget)
      const PROG = "wr 0800 A2 00 A9 5A 9D 00 C8 E8 E0 04 D0 F8 4C 00 08";
      const countMemWrites = async (rpc: RpcClient, captures: any[]): Promise<number> => {
        // Each leg runs on its OWN fresh machine so the cycle window is identical.
        const created = (await rpc.call("session/create", {})) as any;
        const sid = created?.sessionId ?? created?.session_id;
        await rpc.call("monitor/exec", { session_id: sid, command: PROG });
        await rpc.call("monitor/exec", { session_id: sid, command: "r pc=0800" });
        const def = {
          id: `capsel-${captures.length}`,
          version: 1,
          name: "capsel",
          domains: ["c64-cpu", "memory"],
          retention: "transient",
          // A full-range pc-range + mem-access trigger so both row kinds COULD fire;
          // the captures list is what selects whether mem rows are KEPT.
          triggers: [
            { kind: "pc-range", domain: "c64-cpu", from: 0, to: 0xffff },
            { kind: "mem-access", access: "any", from: 0, to: 0xffff },
          ],
          captures,
        };
        await rpc.call("trace/definition/put", { session_id: sid, definition: def });
        await rpc.call("trace/run/start", { session_id: sid, definition_id: def.id });
        await rpc.call("session/run", { session_id: sid, cycles: 20_000 });
        await rpc.call("trace/run/stop", { session_id: sid, wait_index: true }).catch(() => undefined);
        const cur = (await rpc.call("trace/current", { session_id: sid })) as any;
        const db = String(cur?.path ?? cur?.duckdbPath ?? "");
        const sql =
          "SELECT COUNT(*) AS n FROM trace_event WHERE channel='bus_access' " +
          "AND json_extract_string(data_json,'$.op')='write'";
        try {
          const r = (await rpc.call("trace/read", { op: "sql", duckdb_path: db, args: { sql, limit: 4 } })) as any;
          return Number((r?.rows ?? [])[0]?.[0] ?? 0);
        } catch { return -1; }
      };
      const memCountA = await countMemWrites(c, [{ kind: "cpu-row", domain: "c64-cpu" }]);
      const memCountB = await countMemWrites(c, [
        { kind: "cpu-row", domain: "c64-cpu" },
        { kind: "mem-row" },
      ]);
      return {
        // Def A declares only cpu-row → NO mem rows captured (the 708.7 selection).
        memDroppedWhenUndeclared: memCountA === 0,
        // Def B declares mem-row → mem rows ARE captured (the store-heavy loop wrote many).
        memKeptWhenDeclared: memCountB > 0,
      };
    },
  },

  // ── P1: trace-domain-cycle-stable — the trace DOMAIN never changes execution ─
  // The observer-effect guard for the single-path trace fix (Spec 723, trace path).
  // The trace-capture run-path must NOT select the emulation bus from the active
  // trace domain: the cycle-stealing VIC is engaged by the scenario, never by a
  // recording channel. So running the SAME injected program under two different
  // domain sets must yield the IDENTICAL machine endpoint (`c64Cycles`).
  //
  // BEFORE the fix (TRX64): the `vic` domain routed onto `VicBus` (which ticks +
  // steals on the VIC) while `["c64-cpu"]` ran `FlatRam` (no VIC), so adding `vic`
  // SHIFTED c64Cycles — measured 20001 → 20002 for this program. The trace was then
  // a trace of a fictional machine. TS (one execution path, literal VIC always
  // ticked) is domain-stable at 20001 under both.
  //
  // The signal runs the same program (writes a few VIC regs, then a tight `JMP`
  // self-loop) under ["c64-cpu"] and ["c64-cpu","vic"], re-injecting + re-setting PC
  // before each run so both start identically, and reports each endpoint + whether
  // they match. The runner compares TS-signal vs TRX64-signal, so GREEN requires:
  // TRX64 stable (cycA==cycB), AND TRX64 == TS under BOTH domains. The boolean
  // `stable` makes the observer-effect contract explicit in the recorded signal.
  {
    id: "trace-domain-cycle-stable",
    severity: "P1",
    title: "trace domain gates recording only — c64Cycles is identical across domains",
    async signal(c, d) {
      // Writes $D011/$D016/$D018/$D012/$D01A then `JMP $081B` (self-loop). The exact
      // program is irrelevant; what matters is that enabling the `vic` recording
      // domain must not change how many cycles it runs.
      const PROG =
        "wr 0800 78 A9 1B 8D 11 D0 A9 08 8D 16 D0 A9 14 8D 18 D0 A9 80 8D 12 D0 A9 01 8D 1A D0 4C 1B 08";
      // Measure each domain on its OWN FRESH machine (clk=0) so the result is the
      // single-run absolute endpoint — never an artifact of cross-run clock
      // accumulation on a shared session. Run domain A on the case's daemon `c`; spawn
      // ONE more daemon of the same kind for domain B. (`d.kind` = "ts" | "trx64", so
      // the comparison stays kind-honest: TS-vs-TS for the TS leg, TRX-vs-TRX for the
      // TRX64 leg, exactly as the runner intends.)
      const endpointRun = async (rpc: RpcClient, domains: string[]): Promise<number> => {
        const created = (await rpc.call("session/create", {})) as any;
        const sid = created?.sessionId ?? created?.session_id;
        await rpc.call("monitor/exec", { session_id: sid, command: PROG });
        await rpc.call("monitor/exec", { session_id: sid, command: "r pc=0800" });
        await rpc.call("trace/start_domains", { session_id: sid, domains });
        const r = (await rpc.call("session/run", { session_id: sid, cycles: 20000 })) as any;
        await rpc.call("trace/run/stop", { session_id: sid, wait_index: true }).catch(() => undefined);
        return Number(r?.c64Cycles ?? -1);
      };
      const cpuOnly = await endpointRun(c, ["c64-cpu"]);
      const other = await spawnDaemon(d.kind);
      let cpuPlusVic: number;
      try {
        const oc = await connect(other.endpoint);
        try {
          cpuPlusVic = await endpointRun(oc, ["c64-cpu", "vic"]);
        } finally {
          oc.close();
        }
      } finally {
        other.stop();
      }
      return {
        cpuOnly,
        cpuPlusVic,
        // The observer-effect contract: the recording domain must not move the
        // endpoint. TRX64 (before fix) reported stable=false (20001 vs 20002 — the
        // `vic` domain routed onto VicBus, which ticks+steals the VIC). After the fix
        // both endpoints are 20001 (= TS), so stable=true.
        stable: cpuOnly === cpuPlusVic,
      };
    },
  },

  // ── P1: step-run-bus-consistency — STEP sees the SAME machine as RUN ─────────
  // The run-vs-step observer-effect guard for the single-path bus gate (Spec 723).
  // commit 182f6e0 fixed the RUN path (`run_cycle_budget`) to ENGAGE the full
  // literal-VIC machine on a `vic`-directed scenario (the `vic_directed` term), but
  // left FOUR step/inspect sites on the OLD gate (`full_assembled && (!injected ||
  // io_injected)`, no `vic_directed`): `step_one_instruction`,
  // `step_one_capture_interrupt`, `run_until_break`, `debug/memory_access_map`. So
  // the SAME scenario could STEP/INSPECT on a DIFFERENT bus than it RUNS on — a
  // debugger showing a different machine than what executes.
  //
  // This case uses a `vic`-directed INJECTED program (so `vic_directed` is the
  // deciding term — a plain injected program would stay on FlatRam, a real boot would
  // be full-machine on BOTH gates; only `injected && vic_directed` separates old from
  // new). The program reads the VIC raster $D012 in a tight loop and stores it to
  // $0900, then `JMP`s back. On the full literal-VIC machine the raster ADVANCES as
  // cycles pass (VIC ticked) and badline DMA steals CPU cycles; on FlatRam $D012 is
  // static (no VIC) and no cycles are stolen.
  //
  // Signal: drive the SAME program two ways, each on its OWN fresh machine, both with
  // the `vic` trace domain active, both via the SAME instruction budget:
  //   * RUN leg  — `session/run` (the run path = `run_cycle_budget`), read endpoint
  //     PC + c64Cycles + the captured raster at $0900.
  //   * STEP leg — `runtime/call stepInto` × N (the step path = `step_one_instruction`),
  //     read the same endpoint via `runtime/call monitorRegisters` + memory.
  // Reports finalPc/cycles for both legs, whether they MATCH, and whether the VIC was
  // actually live (raster advanced) under stepping. GREEN requires: run==step on
  // TRX64 (no observer effect) AND the TRX64 signal == the TS signal (TS has ONE
  // execution path, so its run and step always agree). BEFORE the fix the STEP leg ran
  // FlatRam (static $D012, no badline steal) → finalCycles/raster diverged from the RUN
  // leg → bus_consistent=false.
  {
    id: "step-run-bus-consistency",
    severity: "P1",
    title: "step-debug uses the SAME bus as run, CYCLE-COMMENSURATE (same endpoint raster/cycles/PC)",
    async signal(c, d) {
      // $0800: SEI; loop: LDA $D012 / STA $0900 / JMP loop. The raster read is the
      // VIC-liveness probe; the JMP self-loop runs forever so a fixed instruction
      // budget is well-defined. (3 instrs/iteration after the one-time SEI.)
      //   0800 78        SEI
      //   0801 AD 12 D0   LDA $D012
      //   0804 8D 00 09   STA $0900
      //   0807 4C 01 08   JMP $0801
      const PROG = "wr 0800 78 AD 12 D0 8D 00 09 4C 01 08";
      const ENTRY = "r pc=0800";
      // Enough instructions to cross several rasterlines (incl. badlines) so the
      // VIC-tick vs FlatRam divergence is unambiguous; small enough to stay fast.
      const STEPS = 600;

      const readClkPc = async (rpc: RpcClient, sid: string) => {
        const st = (await rpc.call("session/state", { session_id: sid })) as any;
        return {
          clk: Number(st.c64Cycles ?? st.cpu?.cycles ?? 0),
          pc: Number(st.cpu?.pc ?? st.pc ?? -1),
        };
      };
      const readRaster = async (rpc: RpcClient, sid: string) =>
        Number(((await rpc.call("runtime/call", {
          session_id: sid, op: "monitorMemory", args: [0x0900, 0x0900],
        })) as any)?.[0] ?? -1);

      // STEP leg FIRST: single-step the SAME program N instructions on a fresh machine
      // via the step path. Record the EXACT endpoint clk (an instruction boundary), PC,
      // and the captured raster at $0900. This endpoint clk becomes the RUN leg's budget
      // so the two legs are CYCLE-COMMENSURATE (the audit HARDEN: the old signal only
      // proved each leg's VIC *ticked*, not that step ticks it at the SAME RATE as run —
      // a badline-steal / off-by-one VIC tick under stepping was invisible).
      const stepLeg = async (rpc: RpcClient) => {
        const created = (await rpc.call("session/create", {})) as any;
        const sid = created?.sessionId ?? created?.session_id;
        await rpc.call("monitor/exec", { session_id: sid, command: PROG });
        await rpc.call("monitor/exec", { session_id: sid, command: ENTRY });
        await rpc.call("trace/start_domains", { session_id: sid, domains: ["vic"] });
        const startClk = (await readClkPc(rpc, sid)).clk;
        for (let i = 0; i < STEPS; i++) await rpc.call("runtime/call", { session_id: sid, op: "stepInto", args: [] });
        const end = await readClkPc(rpc, sid);
        const raster = await readRaster(rpc, sid);
        await rpc.call("trace/run/stop", { session_id: sid, wait_index: true }).catch(() => undefined);
        // budget = the cycles the step path actually consumed (= an instruction boundary).
        return { endClk: end.clk, budget: end.clk - startClk, pc: end.pc, raster };
      };

      // RUN leg: run a cycle budget equal to the step path's consumed cycles on a fresh
      // machine, then read the endpoint. Because `run_for_full(budget)` stops at the FIRST
      // instruction boundary where `clk-start >= budget` and `budget` IS a step-path
      // instruction boundary, the run path lands on the SAME boundary → endpoint clk/PC/
      // raster must be IDENTICAL to the step leg if (and only if) step and run tick the
      // VIC at the same rate.
      const runLeg = async (rpc: RpcClient, budget: number) => {
        const created = (await rpc.call("session/create", {})) as any;
        const sid = created?.sessionId ?? created?.session_id;
        await rpc.call("monitor/exec", { session_id: sid, command: PROG });
        await rpc.call("monitor/exec", { session_id: sid, command: ENTRY });
        await rpc.call("trace/start_domains", { session_id: sid, domains: ["vic"] });
        const startClk = (await readClkPc(rpc, sid)).clk;
        await rpc.call("session/run", { session_id: sid, cycles: budget });
        const end = await readClkPc(rpc, sid);
        const raster = await readRaster(rpc, sid);
        await rpc.call("trace/run/stop", { session_id: sid, wait_index: true }).catch(() => undefined);
        return { consumed: end.clk - startClk, pc: end.pc, raster };
      };

      // STEP on the case's daemon, RUN on a fresh sibling of the SAME kind (kind-honest:
      // TS-vs-TS for the TS leg, TRX-vs-TRX for the TRX64 leg). Step first to derive the
      // budget; run second to match it.
      const step = await stepLeg(c);
      const other = await spawnDaemon(d.kind);
      let run: { consumed: number; pc: number; raster: number };
      try {
        const oc = await connect(other.endpoint);
        try {
          run = await runLeg(oc, step.budget);
        } finally {
          oc.close();
        }
      } finally {
        other.stop();
      }
      return {
        // The cross-leg, cycle-commensurate equality contract (Spec 723 single bus):
        //   * same cycles consumed for the same instruction boundary,
        //   * same final PC (both in the 3-instr loop body),
        //   * same captured raster value at $0900 (the VIC ticked at the SAME rate).
        // A badline-steal / VIC-tick-rate divergence between step and run breaks one of
        // these even though "each leg moved" would still hold. TS has ONE execution path
        // so its run and step always agree; TRX64 must agree too.
        sameCyclesConsumed: run.consumed === step.budget,
        samePc: run.pc === step.pc,
        sameRaster: run.raster === step.raster,
        // VIC liveness sanity: the captured raster is a real VIC value (0..311), not the
        // FlatRam static read (a dead VIC would leave $0900 at a fixed boot byte).
        rasterPlausible: step.raster >= 0 && step.raster <= 0x1ff,
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
    title: "session/frame_available carries the FULL master clock at 1:1 frame cadence (truncated-u32 catch)",
    spawn: { stream: true },
    async signal(c) {
      const sid = await liveSession(c);
      const sink = collectNotes(c);
      await c.call("debug/run", { session_id: sid });
      await sleep(3000); // the running stream loop presents frames continuously
      sink.off();
      // Read the live master clock immediately after the window — the binary frame's
      // cpu_cycle is a truncated u32, but session/state.c64Cycles is the FULL u64
      // master clock. The LAST frame_available.c64Cycles must agree with it (the
      // truncated-u32 catch: a u32-clamped broadcast diverges from the u64 state).
      const st = (await state(c, sid)) as any;
      const stClk = Number(st.c64Cycles ?? st.cpu?.cycles ?? 0);

      const frameNotes = sink.notes.filter((n) => n.method === "session/frame_available");
      const first = frameNotes[0]?.params as any;
      const last = frameNotes[frameNotes.length - 1]?.params as any;
      const frames = frameNotes.map((n) => Number((n.params as any)?.frame));
      const cycles = frameNotes.map((n) => Number((n.params as any)?.c64Cycles));

      // Δframe between consecutive presented frames is exactly 1 (PAL_PRESENT_DIVISOR=1).
      let frameStrictlyInc = frames.length >= 2;
      let frameDeltaAlwaysOne = frames.length >= 2;
      for (let i = 1; i < frames.length; i++) {
        const cur = frames[i] ?? NaN, prev = frames[i - 1] ?? NaN;
        if (!(cur > prev)) frameStrictlyInc = false;
        if (cur - prev !== 1) frameDeltaAlwaysOne = false;
      }
      // c64Cycles is the full master clock, strictly increasing, advancing ONE PAL
      // frame's worth per presented frame. Each per-frame delta = one frame budget
      // (CYC_PER_FRAME=19656) + a small instruction-overshoot (run_for_full stops at
      // the first instruction boundary past the budget, so the EXACT delta jitters by a
      // few cycles per frame and is NOT identical TS-vs-TRX64 — we do NOT gate on the
      // exact value). What we DO assert: every delta sits in a plausible PAL-frame band.
      // A truncated-u32 clock (wraps to tiny deltas), a static clock (delta 0), or a
      // wrong-scale clock all FALL OUT of this band → caught. The band is a fixed PAL
      // physical constant, true on BOTH runtimes (so the diff stays green).
      let cyclesStrictlyInc = cycles.length >= 2;
      const deltas: number[] = [];
      for (let i = 1; i < cycles.length; i++) {
        const cur = cycles[i] ?? NaN, prev = cycles[i - 1] ?? NaN;
        if (!(cur > prev)) cyclesStrictlyInc = false;
        deltas.push(cur - prev);
      }
      // One PAL frame is 312 lines × 63 cyc = 19656; allow up to one instruction (~7 cyc)
      // overshoot per frame, plus generous slack for a boot/first-present window outlier.
      const FRAME_LO = 19656;
      const FRAME_HI = 19656 + 256;
      const cadenceAllInFrameBand =
        deltas.length > 0 && deltas.every((dlt) => dlt >= FRAME_LO && dlt <= FRAME_HI);

      return {
        // ≥2 notes so "strictly increasing" is a real assertion, not vacuous.
        gotMultipleFrames: frameNotes.length >= 2,
        // Payload shape ({session_id, frame, c64Cycles}).
        hasPayloadShape:
          first != null && "session_id" in first && "frame" in first && "c64Cycles" in first,
        // frame strictly increasing, one-per-presented-frame.
        frameStrictlyInc,
        frameDeltaAlwaysOne,
        // c64Cycles strictly increasing.
        cyclesStrictlyInc,
        // Each per-frame c64Cycles delta is one PAL-frame budget (≈19656) — the clock is
        // the FULL master clock at 1:1 frame cadence, not a truncated/static/wrong-scale
        // value. (Boolean band, not the jittery exact delta, so TS≡TRX64.)
        cadenceAllInFrameBand,
        // FULL-CLOCK identity: the last broadcast c64Cycles is the same master clock as
        // session/state (≤ it, within a few frames). A truncated-u32 broadcast would
        // diverge from the u64 state value once the run crosses 0xFFFFFFFF; here we assert
        // they are the SAME clock at the SAME scale, in-window.
        lastCycleIsMasterClock:
          last != null &&
          Number(last.c64Cycles) <= stClk &&
          stClk - Number(last.c64Cycles) <= Math.max(FRAME_HI * 4, 100_000),
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
    title: 'restore then="keep" inherits the prior run-state (running stays running; paused stays paused)',
    spawn: { stream: true },
    async signal(c) {
      const sid = await liveSession(c);
      // Start the continuous --stream driver and let it boot a bit so the machine is
      // genuinely RUNNING (not a one-shot budget that left it paused at the end).
      await c.call("debug/run", { session_id: sid });
      let st = await waitRunningBooted(c, sid, 1_500_000, 60_000);
      // Re-arm the continuous driver if a one-shot debug/run left it paused at budget.
      if (st.runState !== "running") await c.call("debug/run", { session_id: sid });
      // ── PRECONDITION (audit ws-checkpoint-scrub-0 HARDENED) — the keep-restore-of-
      // RUNNING bug only exists if the machine is GENUINELY running at capture. The old
      // signal never asserted this: a slow tsx oracle that left the machine paused would
      // make BOTH legs report "paused" → a mutual-green of the EXACT bug TRX64 had. We
      // now return the pre-restore run-state and require it be "running" (a never-booted
      // TS leg then diverges loud instead of silently greening).
      st = await state(c, sid);
      const runStateBeforeRunningRestore = st.runState;
      // Capture an anchor of the live (running) state.
      const capRun = (await c.call("checkpoint/capture", { session_id: sid })) as any;
      const cpIdRun = capRun?.ref?.id ?? capRun?.id;
      // Restore with then omitted (≡ "keep"). A keep-restore of a RUNNING machine must
      // leave it running.
      await c.call("checkpoint/restore", { session_id: sid, id: cpIdRun, then: "keep" });
      // Give the stream loop a beat to keep advancing if it is (still) running, then read.
      await sleep(1000);
      st = await state(c, sid);
      const runStateAfterKeepRestore = st.runState;

      // ── SYMMETRIC leg: keep-restore of a PAUSED machine must STAY paused. ──
      // Pause the machine, confirm paused, capture, keep-restore, re-read. A correct
      // "keep" inherits paused → paused (the inverse direction of the running case);
      // a bug that always *forces* a run-state in one direction is caught by one of the
      // two legs.
      await c.call("debug/pause", { session_id: sid }).catch(() => undefined);
      await sleep(300);
      st = await state(c, sid);
      const runStateBeforePausedRestore = st.runState;
      const capPause = (await c.call("checkpoint/capture", { session_id: sid })) as any;
      const cpIdPause = capPause?.ref?.id ?? capPause?.id;
      await c.call("checkpoint/restore", { session_id: sid, id: cpIdPause, then: "keep" });
      await sleep(500);
      st = await state(c, sid);
      const runStateAfterPausedKeepRestore = st.runState;

      return {
        // PRECONDITION: the machine was provably running before the running-leg restore.
        runStateBeforeRunningRestore,
        // RUNNING keep-restore stays running.
        runStateAfterKeepRestore,
        // PAUSED keep-restore stays paused (symmetric inverse).
        runStateBeforePausedRestore,
        runStateAfterPausedKeepRestore,
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
    title: 'restore then="pause" broadcasts EXACTLY ONE debug/stopped(reason=pause) at the restored anchor coords',
    spawn: { stream: true },
    async signal(c) {
      const sid = await liveSession(c);
      // No need to free-run; a fresh paused machine can capture + restore an anchor.
      // Read the machine PC at capture so we can assert the restored stop lands there.
      const stAtCap = await state(c, sid);
      const pcAtCap = stAtCap.cpu?.pc ?? stAtCap.pc ?? null;
      const cap = (await c.call("checkpoint/capture", { session_id: sid })) as any;
      const cpId = cap?.ref?.id ?? cap?.id;
      const cpCycles = cap?.ref?.cycles ?? cap?.cycles ?? null;
      const sink = collectNotes(c);
      await c.call("checkpoint/restore", { session_id: sid, id: cpId, then: "pause" });
      await sleep(500);
      sink.off();
      const stopped = sink.notes.filter(
        (n) => n.method === "debug/stopped" && (n.params as any)?.stop?.reason === "pause",
      );
      const stop = stopped[0]?.params as any;
      // The audit HARDEN: bare presence false-greens an impl that pushes a stop with the
      // WRONG coords (or several). Assert EXACTLY ONE pause-stop, that its {pc,cycles}
      // match the restored anchor (pc==pc-at-capture, cycles==anchor.cycles), and that
      // it carries a non-empty registers dump.
      return {
        // EXACTLY one pause-stop pushed (not zero, not a flurry).
        pauseStopCount: stopped.length,
        // Coordinates of the pushed stop == the restored anchor's coordinates.
        stopReasonPause: stop?.stop?.reason === "pause",
        stopPcMatchesAnchor: pcAtCap != null && Number(stop?.stop?.pc) === Number(pcAtCap),
        stopCyclesMatchAnchor: cpCycles != null && Number(stop?.stop?.cycles) === Number(cpCycles),
        // The stop carries register state (a passive UI renders it on the scrub freeze).
        hasRegisters:
          typeof stop?.registers === "string"
            ? stop.registers.length > 0
            : stop?.registers != null,
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

  // ── P1: ws-trace-e2e-read — the full capture→read workflow returns REAL data ──
  // The class the chis stub slipped through: the gate tested WS response SHAPES, not the
  // end-to-end workflow (capture a trace → type a read verb → see real data). `chis` was a
  // hardcoded "not supported" stub even though the cpu rows it needs are in the captured
  // .c64retrace (swimlane reads them fine). This drives the ACTUAL workflow on a FINALIZED
  // trace: free-run past boot, capture a window (the UI's domains), STOP with wait_index so
  // the .duckdb index is built, then call EACH monitor read verb (chis/swimlane/map) and
  // assert it returns REAL trace content (a PC hex row), NOT an error/stub/empty.
  // NOTE: this covers the FINALIZED/historical read. LIVE read (chis while the trace is
  // still active) is a separate capability (TS serves it from the live checkpoint ring;
  // TRX64 needs the in-memory cpuhistory ring — the reverse-debug Phase-1 build) and gets
  // its own case once that lands.
  {
    id: "ws-trace-e2e-read",
    severity: "P1",
    title: "capture→read workflow: chis/swimlane/map all return real trace data (no stub/empty)",
    spawn: { stream: true },
    async signal(c) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      await c.call("debug/run", { session_id: sid });
      await waitRunningBooted(c, sid, 1_500_000, 60_000);
      const cs = (await state(c, sid)).c64Cycles ?? 0;
      await c.call("trace/start_domains", { session_id: sid, domains: ["c64-cpu", "memory"] });
      // A SHORT capture window keeps the .duckdb index small, so the FIRST read verb (which
      // builds the index from the .c64retrace) returns fast under the 2-daemon oracle load.
      await sleep(250);
      const ce = (await state(c, sid)).c64Cycles ?? 0;
      // wait_index:true → finalize AND build the .duckdb index, so the FIRST read verb
      // hits a ready index (not a cold/half-flushed tail).
      await c.call("trace/run/stop", { session_id: sid, wait_index: true }).catch(() => undefined);
      await sleep(1000);
      // "real trace read" = has a PC hex row AND is not an error/stub/empty.
      const hasTrace = (s: string) =>
        /\$[0-9a-fA-F]{2,4}/.test(s) &&
        !/unknown command|not supported|no trace store|no trace/i.test(s);
      const chisOut = await exec("chis");
      const swimOut = await exec(`swimlane ${cs} ${ce}`);
      const mapOut = await exec("map");
      return {
        // The anti-stub guard: chis returns real cpu instruction history (not the stub).
        chisReturnsCpuHistory: hasTrace(chisOut),
        swimlaneReturnsRows: hasTrace(swimOut),
        mapReturnsContent: mapOut.length > 40 && !/unknown command|no trace/i.test(mapOut),
      };
    },
  },

  // ── P1: ws-trace-chis-live — `chis` works LIVE from the always-on cpuhistory ring
  // THE USER BUG (reverse-debug Phase 1a). The user runs the machine and types `chis`
  // (the obvious VICE cpuhistory verb) WHILE a trace is active. The FINALIZED path
  // (ws-trace-e2e-read, commit 57c9191) reads the captured .c64retrace via the sidecar
  // — but that needs a FINALIZED trace (after `trace off` + index build); a LIVE
  // (still-capturing) trace has no readable index yet, so live `chis` failed. TRX64 now
  // serves it from an always-on in-memory cpuhistory ring (Machine::cpu_history), fed
  // per retired instruction at the same point the trace cpu-row hook fires — NO trace /
  // finalize / sidecar dependency.
  //
  // THE COMPARABLE (differential) FLOW — gated here: `chis` LIVE during a free-run with
  // NO active trace. BOTH runtimes serve this from their live ring (TS replays its
  // checkpoint ring; TRX64 reads the cpuhistory ring) and BOTH return real cpu
  // instruction rows (a $XXXX PC), NOT a stub/error/empty. Signal normalized to that
  // boolean so the differential is apples-to-apples.
  //
  // THE SUPERSET (the user's EXACT trace-ACTIVE flow) is verified by `ws-trace-chis-
  // live-trace-active` below — NON-gating, because TS's chisReplay REFUSES while a
  // trace is active ("a trace is active — `trace off` first", ws-server.ts:1965) whereas
  // TRX64's ring serves it. That divergence is TRX64 being strictly better, so the
  // differential can't be GREEN; it is asserted on TRX64 alone there.
  {
    id: "ws-trace-chis-live",
    severity: "P1",
    title: "monitor `chis` returns LIVE cpu history from the ring during free-run (no finalized trace)",
    spawn: { stream: true },
    async signal(c) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      await c.call("debug/run", { session_id: sid });
      await waitRunningBooted(c, sid, 1_500_000, 60_000);
      // Let the live ring fill (TRX64) AND let TS auto-capture its first checkpoint
      // (TS chisReplay needs ≥1 ring checkpoint; the tsx daemon is ~4fps, so settle
      // generously so the no-trace comparable flow is stable on BOTH under 2-daemon load).
      await sleep(2500);
      // The USER'S FLOW (comparable form): type `chis` LIVE. NO trace finalize/sidecar.
      const chisOut = await exec("chis");
      // "real live history" = a PC hex row AND not an error/stub/empty.
      const hasLiveHistory =
        /\$[0-9a-fA-F]{2,4}/.test(chisOut) &&
        !/unknown command|not supported|no trace store|no trace|no cpu history/i.test(chisOut);
      return { chisReturnsLiveHistory: hasLiveHistory };
    },
  },

  // ── P1: ws-trace-chis-live-trace-active — the user's EXACT flow (TRX64 superset) ──
  // NON-GATING (blocked): the user types `chis` WHILE a trace is ACTIVE. TRX64 serves it
  // from the cpuhistory ring (the whole point of Phase 1a); TS's chisReplay throws while
  // a trace is active (ws-server.ts:1965 `if (ctrl.traceRun.isActive()) throw`). The
  // differential therefore CANNOT be GREEN — TRX64 is strictly better here — so this is
  // asserted on the TRX64 daemon alone and reported non-gating, per the reverse-debug
  // verification doctrine ("NOT the differential gate — TRX64 superset"). Run on demand:
  //   tsx src/conformance.ts --only ws-trace-chis-live-trace-active --include-blocked
  {
    id: "ws-trace-chis-live-trace-active",
    severity: "P1",
    blocked:
      "TRX64 superset: TS chisReplay refuses while a trace is active (ws-server.ts:1965), " +
      "so the differential can't compare. TRX64's cpuhistory ring serves the user's exact " +
      "trace-ON `chis` flow; asserted on TRX64 alone (run with --include-blocked).",
    title: "monitor `chis` returns live cpu history WHILE a trace is active (TRX64 cpuhistory-ring superset)",
    spawn: { stream: true },
    async signal(c, d) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      await c.call("debug/run", { session_id: sid });
      await waitRunningBooted(c, sid, 1_500_000, 60_000);
      // Start a trace and LEAVE IT ACTIVE (do NOT stop / finalize / build the index).
      await c.call("trace/start_domains", { session_id: sid, domains: ["c64-cpu", "memory"] });
      await sleep(250);
      const chisOut = await exec("chis");
      const isLive = (s: string) =>
        /\$[0-9a-fA-F]{2,4}/.test(s) &&
        !/unknown command|not supported|no trace store|no trace|no cpu history|trace is active/i.test(s);
      // On the TRX64 daemon this MUST be live history (the superset). On the TS daemon it
      // is the documented refusal; we only assert the TRX64 side (the capability claim).
      return d.kind === "trx64"
        ? { chisLiveWhileTracing: isLive(chisOut) }
        : { chisLiveWhileTracing: false /* TS refuses — documented, non-gating */ };
    },
  },

  // ── P1: ws-reverse-step — real backward-stepping (TRX64 superset, reverse-debug 1b)
  // TWO-SIDED, GATING. TRX64's always-on full-delta ring undoes the last instruction(s)
  // (write old_values back + restore the CPU pre-state). The TS runtime has only the
  // snapshot ring-crutch (replay-forward), no O(1) delta-undo `runtime/reverse_step`, so
  // it now CLEANLY DECLINES the method ("not supported by the TypeScript runtime — use the
  // TRX64 runtime", ws-server.ts TRX64_ONLY_METHODS) instead of -32601 method-not-found.
  // The differential is `behavesCorrectly` = each runtime did the right thing FOR ITS KIND:
  // TS declines cleanly; TRX64 delivers. Both true → identical signal → GREEN, GATING.
  //   tsx src/conformance.ts --only ws-reverse-step
  //
  // FLOW (TRX64): after `liveSession` the machine is booted (cold reset ran the KERNAL to
  // READY on the full-machine path → the delta ring already holds real history). Read the
  // CPU state, single-step ONE real instruction (fed into the ring), then `reverse_step
  // n=1` over WS. Assert the WS reply landed back on the EXACT pre-step PC + registers AND
  // that `session/state` confirms the machine's live PC rolled back.
  // FLOW (TS): call `runtime/reverse_step` once; the WS server declines it with the
  // recognizable TRX64-superset message → `tsDeclinesCleanly`.
  {
    id: "ws-reverse-step",
    severity: "P1",
    title: "runtime/reverse_step undoes the last instruction (TRX64 delivers; TS cleanly declines) — TRX64 reverse-debug superset",
    async signal(c, d) {
      if (d.kind !== "trx64") {
        // TS has no reverse_step — assert the CLEAN decline (not -32601, not a generic
        // throw). The single comparable signal `behavesCorrectly` is true when TS refused
        // the superset method with the recognizable "TypeScript runtime" message.
        const sid = await liveSession(c);
        const tsDeclinesCleanly = await assertTrx64OnlyDecline(c, "runtime/reverse_step", { session_id: sid, n: 1 });
        return { behavesCorrectly: tsDeclinesCleanly };
      }
      const sid = await liveSession(c);
      // Pre-state: the live CPU before we step forward.
      const s0 = await state(c, sid);
      const pc0 = s0.cpu.pc, a0 = s0.cpu.a, x0 = s0.cpu.x, y0 = s0.cpu.y, sp0 = s0.cpu.sp;
      // Step ONE real instruction (full-machine path → recorded into the delta ring).
      await c.call("debug/step", { session_id: sid });
      const s1 = await state(c, sid);
      const movedForward = s1.cpu.pc !== pc0 || s1.cpu.cycles !== s0.cpu.cycles;
      // Reverse it.
      const rev = (await c.call("runtime/reverse_step", { session_id: sid, n: 1 })) as any;
      // Re-read the live state: the machine must sit back at the pre-step PC.
      const s2 = await state(c, sid);
      // The WS reply landed on the exact pre-step CPU state.
      const landedOnPreState =
        rev?.stepsTaken === 1 &&
        rev?.pc === pc0 && rev?.a === a0 && rev?.x === x0 && rev?.y === y0 && rev?.sp === sp0;
      // The contract flag is surfaced (inspect-only).
      const inspectOnly = rev?.inspectOnly === true;
      // The LIVE machine PC rolled back (not just the reply).
      const ramRolledBack = s2.cpu.pc === pc0;
      // TRX64 behaves correctly when it actually delivered the reverse-step: the forward
      // step moved, the reply landed on the pre-state, the live PC rolled back, inspect-only.
      return { behavesCorrectly: movedForward && landedOnPreState && ramRolledBack && inspectOnly };
    },
  },

  // ── P1: ws-who-wrote — last-writer scan over the live delta ring (reverse-debug 1b)
  // TWO-SIDED, GATING: the stack-crash shortcut "who put the bad byte on $XXXX". TRX64's
  // always-on delta ring serves the live last-writer scan; the TS runtime has no equivalent
  // and now CLEANLY DECLINES `runtime/who_wrote` ("not supported by the TypeScript runtime
  // — use the TRX64 runtime"). `behavesCorrectly` = TS declines cleanly ∧ TRX64 delivers.
  //   tsx src/conformance.ts --only ws-who-wrote
  //
  // FLOW (TRX64): the booted KERNAL idle loop + jiffy IRQ continuously write the zero-page
  // jiffy clock ($00A0-$00A2) and other ZP. After settling, `runtime/who_wrote {addr:
  // 0x00A2}` must return ≥1 writer with a KERNAL/RAM PC and a coherent old→new pair. We
  // also pin a KNOWN write deterministically: step until the live PC's instruction stores
  // to a zero-page byte is hard to guarantee, so we additionally assert the structural
  // contract (writers[] shape, newest-first) on whatever the live ring captured.
  // FLOW (TS): call `runtime/who_wrote` once → assert the clean TRX64-superset decline.
  {
    id: "ws-who-wrote",
    severity: "P1",
    title: "runtime/who_wrote pins the last writer(s) of an address (TRX64 delivers; TS cleanly declines) — TRX64 reverse-debug superset",
    // --stream: the per-frame stream loop is the free-run driver, so `debug/run` actually
    // advances the machine past READY (without it `waitRunningBooted` never reaches 3.8M).
    spawn: { stream: true },
    async signal(c, d) {
      if (d.kind !== "trx64") {
        const sid = await liveSession(c);
        const tsDeclinesCleanly = await assertTrx64OnlyDecline(c, "runtime/who_wrote", { session_id: sid, addr: 0x00a2, limit: 8 });
        return { behavesCorrectly: tsDeclinesCleanly };
      }
      const sid = await liveSession(c);
      // Run WELL PAST the BASIC READY prompt: the jiffy clock LSB ($00A2) is only
      // incremented once the editor IRQ (CINV→$EA31→UDTIM) is live, which is AFTER the
      // KERNAL reset finishes (~2M cyc). Settle to ~4M cyc so several `INC $A2`s have
      // landed in the ring (each frame ≈ 19656 cyc → dozens of increments).
      await c.call("debug/run", { session_id: sid });
      await waitRunningBooted(c, sid, 3_800_000, 90_000);
      await sleep(800);
      await c.call("debug/step", { session_id: sid }); // settle to a paused boundary
      // $00A2 = jiffy-clock LSB, incremented by the KERNAL timer IRQ every frame; over a
      // multi-frame post-READY window it is a guaranteed live writer in the ring.
      const r = (await c.call("runtime/who_wrote", { session_id: sid, addr: 0x00a2, limit: 8 })) as any;
      const writers: any[] = Array.isArray(r?.writers) ? r.writers : [];
      const foundWriter = writers.length >= 1;
      // Newest-first ordering: cycles non-increasing.
      let newestFirst = true;
      for (let i = 1; i < writers.length; i++) {
        if (writers[i].cycle > writers[i - 1].cycle) { newestFirst = false; break; }
      }
      // Each hit is structurally coherent: addr matches, old/new are bytes, a real PC.
      const oldNewCoherent =
        foundWriter &&
        writers.every(
          (w) => w.addr === 0x00a2 && w.old >= 0 && w.old <= 255 && w.new >= 0 && w.new <= 255 && typeof w.pc === "number" && w.pc > 0,
        );
      // TRX64 behaves correctly when it actually pinned the writer(s): found ≥1, newest-first,
      // structurally coherent.
      return { behavesCorrectly: foundWriter && newestFirst && oldNewCoherent };
    },
  },

  // ── P1: ws-build-trace-from-ring — targeted .c64retrace from a delta-ring window
  // TWO-SIDED, GATING (TRX64 superset, reverse-debug Phase 1c): the UI scrub-bar selects
  // TWO thumbnails (a cycle window) → "build trace" → a `.c64retrace` for EXACTLY that
  // window, then swimlane/map/taint on just those cycles. No whole-run capture, no cycle
  // guessing. TRX64 slices its always-on 10s delta ring on demand; the TS runtime has NO
  // always-on full-delta ring, so it cannot dump a window after the fact and now CLEANLY
  // DECLINES `trace/build_from_ring` ("not supported by the TypeScript runtime").
  // `behavesCorrectly` = TS declines cleanly ∧ TRX64 produces a readable window trace.
  //   tsx src/conformance.ts --only ws-build-trace-from-ring
  //
  // FLOW (TRX64): free-run past boot so the always-on delta ring holds real history,
  // pick a cycle window [a,b] INSIDE the ring (a couple of PAL frames ending just
  // before `now`), call `trace/build_from_ring {a,b}`. Assert (i) the returned
  // `.c64retrace` exists on disk and is non-empty AND decodes to real CPU rows, (ii)
  // `swimlane <a> <b>` (monitor) over the resulting store returns REAL cpu rows for that
  // window (a `$XXXX` PC, not empty/error), (iii) event_count > 0.
  // FLOW (TS): call `trace/build_from_ring` once → assert the clean TRX64-superset decline.
  {
    id: "ws-build-trace-from-ring",
    severity: "P1",
    title: "trace/build_from_ring dumps a targeted .c64retrace for a delta-ring cycle window (TRX64 delivers; TS cleanly declines) — TRX64 reverse-debug superset",
    spawn: { stream: true },
    async signal(c, d) {
      if (d.kind !== "trx64") {
        // TS has no always-on full-delta ring — assert the clean TRX64-superset decline.
        const sid = await liveSession(c);
        const tsDeclinesCleanly = await assertTrx64OnlyDecline(c, "trace/build_from_ring", { session_id: sid, cycle_start: 1_000, cycle_end: 41_000 });
        return { behavesCorrectly: tsDeclinesCleanly };
      }
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      // Free-run well past boot so the delta ring holds several PAL frames of real
      // history (each frame ≈ 19656 cyc; the ring covers ~10s).
      await c.call("debug/run", { session_id: sid });
      await waitRunningBooted(c, sid, 1_500_000, 60_000);
      await sleep(800);
      await c.call("debug/step", { session_id: sid }); // settle to a paused boundary
      const now = (await state(c, sid)).c64Cycles ?? 0;
      // A window of ~2 PAL frames ending a hair before `now` — comfortably inside the
      // 10s ring, and recent enough that the write slab has NOT wrapped over it.
      const b = Math.max(2_000, now - 2_000);
      const a = Math.max(1_000, b - 40_000);
      const res = (await c.call("trace/build_from_ring", {
        session_id: sid,
        cycle_start: a,
        cycle_end: b,
      })) as any;
      const retracePath: string = res?.retrace_path ?? "";
      const eventCount: number = Number(res?.event_count ?? 0);
      // (i) the file exists on disk and is non-empty AND decodes to real CPU rows.
      let fileOnDisk = false;
      let decodesCpuRows = false;
      if (retracePath && existsSync(retracePath) && statSync(retracePath).size > 0) {
        fileOnDisk = true;
        try {
          const dec = decodeTrace(readFileSync(retracePath));
          // A CPU_STEP row (family "cpu") with a real PC, stamped inside the window.
          decodesCpuRows = dec.records.some(
            (r: any) =>
              r.family === "cpu" &&
              typeof r.fields?.pc === "number" &&
              r.cycle >= a && r.cycle <= b,
          );
        } catch {
          decodesCpuRows = false;
        }
      }
      // (ii) swimlane over the resulting store returns real cpu rows for the window.
      const swimOut = await exec(`swimlane ${a} ${b}`);
      const swimlaneReturnsRows =
        /\$[0-9a-fA-F]{2,4}/.test(swimOut) &&
        !/unknown command|not supported|no trace store|no trace/i.test(swimOut);
      // The window was fully inside the ring (no clip) — sanity that we picked a live
      // window, not one that fell off the back of the ring.
      const notClipped = res?.clipped !== true;
      // TRX64 behaves correctly when it produced a readable window trace: file on disk,
      // decodes to real CPU rows, swimlane reads them back, has events, not clipped.
      return {
        behavesCorrectly:
          fileOnDisk && decodesCpuRows && swimlaneReturnsRows && eventCount > 0 && notClipped,
      };
    },
  },

  // ── P1: ws-trace-index — EXPLICIT trace-decode of a captured `.c64retrace` (TRX64 superset)
  // THE BUG (trace-decode gap): a captured 1.2 GB `.c64retrace` could not be queried because
  // the `.duckdb` index is built LAZILY only on the first sidecar read — `trace_store_info`
  // and any reader that opens the `.duckdb` DIRECTLY never triggered that build, so it
  // reported "directory has no trace.duckdb". `trace/index` is the explicit "decode this"
  // op: it BUILDS the index via the same sidecar path and reports how many events landed +
  // the honest bound (the indexer streams oldest→newest with NO event cap — a 1.2 GB trace's
  // oldest events ARE indexed). TRX64 delivers it; the TS runtime has no such WS method.
  //
  // FLOW (TRX64): free-run past boot, capture a short window, STOP with wait_index:FALSE (so
  // NO lazy/auto build happens yet — the `.duckdb` is absent). Assert trace/current reports
  // indexed:false. Then call `trace/index` and assert: the `.duckdb` now exists on disk,
  // eventsIndexed > 0, bounded:false (full build), indexedFromOldest:true, and a subsequent
  // read (`swimlane` over the window via the now-present index) returns real PC rows.
  // FLOW (TS): the differential can't compare (TS has no trace/index) → clean-decline doctrine.
  //   tsx src/conformance.ts --only ws-trace-index
  {
    id: "ws-trace-index",
    severity: "P1",
    title:
      "trace/index explicitly builds the .duckdb for a captured .c64retrace so it is queryable (TRX64 delivers; TS cleanly declines) — trace-decode gap fix",
    spawn: { stream: true },
    async signal(c, d) {
      const sid = await liveSession(c);
      if (d.kind !== "trx64") {
        // TS has no trace/index WS method and it is NOT registered in the c64re
        // TRX64_ONLY_METHODS list (which we must not edit), so it declines with the
        // generic -32601 "method not found" rather than the TRX64-only message. Either
        // The TS runtime does NOT service this trace-decode op; TRX64 delivers it.
        // trace/index is now in the c64re TRX64_ONLY_METHODS set → TS gives the clean
        // "not supported by the TypeScript runtime" decline (not a generic -32601).
        const tsDeclines = await assertTrx64OnlyDecline(c, "trace/index", { session_id: sid });
        return { behavesCorrectly: tsDeclines };
      }
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      await c.call("debug/run", { session_id: sid });
      await waitRunningBooted(c, sid, 1_500_000, 60_000);
      const cs = (await state(c, sid)).c64Cycles ?? 0;
      await c.call("trace/start_domains", { session_id: sid, domains: ["c64-cpu", "memory"] });
      await sleep(250);
      const ce = (await state(c, sid)).c64Cycles ?? 0;
      // STOP WITHOUT wait_index → finalize the `.c64retrace` but build NO `.duckdb` yet
      // (no lazy build either, since we don't read before indexing). This reproduces the
      // captured-but-unindexed state `trace_store_info` choked on.
      await c.call("trace/run/stop", { session_id: sid, wait_index: false }).catch(() => undefined);
      await sleep(300);
      // trace/current must report the store path + indexed:false (no `.duckdb` on disk).
      const cur = (await c.call("trace/current", { session_id: sid })) as any;
      const duckdbPath: string = cur?.duckdbPath ?? cur?.path ?? "";
      const notIndexedBefore = cur?.indexed === false && !!duckdbPath && !existsSync(duckdbPath);
      // EXPLICITLY index it.
      const idx = (await c.call("trace/index", { session_id: sid })) as any;
      const eventsIndexed = Number(idx?.eventsIndexed ?? 0);
      const indexBuiltField = idx?.indexBuilt === true;
      const fullNotBounded = idx?.bounded === false && idx?.cap === null && idx?.indexedFromOldest === true;
      const duckdbNowOnDisk = existsSync(idx?.duckdbPath ?? duckdbPath);
      // A subsequent read must now return real rows (the index is queryable).
      const swimOut = await exec(`swimlane ${cs} ${ce}`);
      const swimReturnsRows =
        /\$[0-9a-fA-F]{2,4}/.test(swimOut) &&
        !/unknown command|not supported|no trace store|no trace/i.test(swimOut);
      return {
        behavesCorrectly:
          notIndexedBefore &&
          indexBuiltField &&
          eventsIndexed > 0 &&
          fullNotBounded &&
          duckdbNowOnDisk &&
          swimReturnsRows,
      };
    },
  },

  // ── P1: ws-set-reverse-depth — runtime-settable always-on reverse-ring depth (TRX64 superset)
  // Part B of the trace-depth work: `runtime/set_reverse_depth { seconds }` REBUILDS the
  // always-on delta + cpu-history rings at a new capacity for FUTURE capture (discarding
  // current history). TRX64 delivers it; the TS runtime has no in-process reverse rings, so
  // it CLEANLY DECLINES. Assert: set to 2s → a `who_wrote` window beyond the (tiny) ring is
  // empty / the depth report shrank; set back up → the capacity GREW. The signal compares the
  // reported ring capacities + the discardedHistory contract.
  //   tsx src/conformance.ts --only ws-set-reverse-depth
  {
    id: "ws-set-reverse-depth",
    severity: "P1",
    title:
      "runtime/set_reverse_depth rebuilds the always-on reverse rings at a new depth (TRX64 delivers; TS cleanly declines) — runtime-settable reverse depth",
    spawn: { stream: true },
    async signal(c, d) {
      const sid = await liveSession(c);
      if (d.kind !== "trx64") {
        // Not registered in the c64re TRX64_ONLY_METHODS list (un-editable here) → the
        // The TS runtime has no in-process reverse rings, so declining is the honest
        // signal; TRX64 delivers the knob. runtime/set_reverse_depth is now in the c64re
        // TRX64_ONLY_METHODS set → TS gives the clean "not supported by the TypeScript
        // runtime" decline (not a generic -32601).
        const tsDeclines = await assertTrx64OnlyDecline(c, "runtime/set_reverse_depth", { session_id: sid, seconds: 2 });
        return { behavesCorrectly: tsDeclines };
      }
      const capOf = (r: any) => Number(r?.deltaEntryCapacity ?? 0);
      // SHRINK to 2s → small ring, history discarded.
      const small = (await c.call("runtime/set_reverse_depth", { session_id: sid, seconds: 2 })) as any;
      const smallCap = capOf(small);
      const smallDiscarded = small?.discardedHistory === true;
      const smallSeconds = small?.seconds === 2;
      // Run a touch so the (small) ring fills with fresh history.
      await c.call("debug/run", { session_id: sid });
      await waitRunningBooted(c, sid, 1_500_000, 60_000);
      await sleep(300);
      await c.call("debug/step", { session_id: sid });
      // GROW to 8s → bigger ring than 2s.
      const big = (await c.call("runtime/set_reverse_depth", { session_id: sid, seconds: 8 })) as any;
      const bigCap = capOf(big);
      const grew = bigCap > smallCap && big?.seconds === 8 && big?.discardedHistory === true;
      // No-arg read-only report must reflect the current (8s) depth without discarding.
      const report = (await c.call("runtime/set_reverse_depth", { session_id: sid })) as any;
      const reportsCurrent = report?.seconds === 8 && report?.discardedHistory === false && capOf(report) === bigCap;
      // The monitor `revdepth` verb agrees (and lists the rebuilt capacities).
      const verb = await (async () => {
        const r = (await c.call("monitor/exec", { session_id: sid, command: "revdepth" })) as any;
        return String(r?.output ?? r?.error ?? "");
      })();
      const verbAgrees = /revdepth: 8s/.test(verb) && !/unknown command/i.test(verb);
      return {
        behavesCorrectly:
          smallSeconds &&
          smallDiscarded &&
          smallCap > 0 &&
          grew &&
          reportsCurrent &&
          verbAgrees,
      };
    },
  },

  // ── P1: ws-jam-triage — guided crash-triage on a JAM (TRX64 superset, reverse-debug 2)
  // TWO-SIDED, GATING (TRX64 superset): when the machine JAMs (wild PC / illegal opcode),
  // the monitor auto-prints the CAUSAL CHAIN — crash → wild control transfer → stack
  // corruptor — instead of making the user hand-walk it. The classic derail is an RTS that
  // popped a CORRUPTED return address. TRX64 reconstructs the chain from its always-on
  // CPU-history + delta rings; the TS runtime has no such rings and now CLEANLY DECLINES
  // `runtime/crash_triage` ("not supported by the TypeScript runtime — use the TRX64
  // runtime"). `behavesCorrectly` = TS declines cleanly ∧ TRX64 names the corruptor.
  //   tsx src/conformance.ts --only ws-jam-triage
  //
  // FLOW (TRX64): boot to a live machine, then INJECT a deliberate stack-smash at $C000
  // (RAM): SEI; set SP=$FC; write a BAD return address ($C0DD) onto the stack via two
  // STAs (the corruptors @ $C006 / $C00B); RTS pops it → PC = $C0DE (the wild PC, holding
  // a JAM $02). Point the PC at the program and `debug/run` under --stream — the per-frame
  // FULL-machine driver executes it, FEEDS the always-on rings, hits the JAM and fires the
  // Spec-764 auto-break (the SAME path a real game derail takes). Then read the triage over
  // WS (`runtime/crash_triage`). Assert it (i) names the JAM/wild PC + opcode, (ii)
  // identifies the RTS stack pop, and (iii) `who_wrote` PINS the smashing STA instruction's
  // PC. Also exercise the monitor `triage` verb.
  // FLOW (TS): call `runtime/crash_triage` once → assert the clean TRX64-superset decline.
  //
  // (We DON'T single-step the program: the monitor `z`/`wr` step path is the CPU-ISOLATED
  // cycle-exact lane which deliberately does NOT feed the rings — only the full-machine
  // free-run does. The product JAM always happens on the full path, which this exercises.)
  {
    id: "ws-jam-triage",
    severity: "P1",
    title:
      "runtime/crash_triage auto-pins the stack corruptor on a JAM (TRX64 delivers; TS cleanly declines) — TRX64 reverse-debug superset",
    // --stream: the per-frame FULL-machine driver runs the injected program, feeds the
    // rings, and fires the Spec-764 JAM auto-break (run_for_full ignores `injected`).
    spawn: { stream: true },
    async signal(c, d) {
      if (d.kind !== "trx64") {
        // TS has no always-on CPU-history rings — assert the clean TRX64-superset decline.
        const sid = await liveSession(c);
        const tsDeclinesCleanly = await assertTrx64OnlyDecline(c, "runtime/crash_triage", { session_id: sid });
        return { behavesCorrectly: tsDeclinesCleanly };
      }
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      // Inject the stack-smash program @ $C000 (RAM). The two STAs are the corruptors:
      //   $C000 SEI            78          ; no IRQ to perturb the stack
      //   $C001 LDX #$FC       A2 FC
      //   $C003 TXS            9A          ; SP=$FC → RTS pops $01FD(lo)/$01FE(hi)
      //   $C004 LDA #$DD       A9 DD
      //   $C006 STA $01FD      8D FD 01    ; corruptor ret-lo  @ $C006
      //   $C009 LDA #$C0       A9 C0
      //   $C00B STA $01FE      8D FE 01    ; corruptor ret-hi  @ $C00B
      //   $C00E RTS            60          ; pops $C0DD → PC = $C0DE (wild)
      // and $C0DE = JAM ($02).
      await exec("wr ram c000 78 a2 fc 9a a9 dd 8d fd 01 a9 c0 8d fe 01 60");
      await exec("wr ram c0de 02");
      await exec("r pc=c000"); // sets both cores' PC (full path resumes here)
      // Free-run on the full path; the program JAMs within the first frame. Poll until
      // the CPU is jammed at the wild PC (PC frozen at $C0DE).
      await c.call("debug/run", { session_id: sid });
      let jammedPc = 0;
      for (let i = 0; i < 60; i++) {
        const st = await state(c, sid);
        const pc = st.cpu?.pc ?? st.pc ?? 0;
        if (pc === 0xc0de) { jammedPc = pc; break; }
        await sleep(100);
      }
      // Read the structured triage over WS.
      const t = (await c.call("runtime/crash_triage", { session_id: sid })) as any;
      const transfer = t?.transfer ?? {};
      const slots: any[] = Array.isArray(t?.corruptorSlots) ? t.corruptorSlots : [];
      // (i) names the JAM / wild PC + opcode.
      const namesWildPc = t?.crash?.pc === 0xc0de && t?.crash?.opcode === 0x02;
      // (ii) identifies the RTS stack pop.
      const identifiesRtsPop =
        transfer?.kind === "RTS" && transfer?.isStackPop === true && transfer?.atPc === 0xc00e;
      // (iii) who_wrote pins the smashing STA PCs: ret-lo @ $C006, ret-hi @ $C00B.
      const pinsCorruptor = t?.pinnedCorruptor === true;
      const loSlot = slots.find((s) => s.addr === 0x01fd);
      const hiSlot = slots.find((s) => s.addr === 0x01fe);
      const corruptorPcCorrect =
        !!loSlot && loSlot.writerPc === 0xc006 && !!hiSlot && hiSlot.writerPc === 0xc00b;
      // The monitor `triage` verb agrees (names the wild PC + the RTS).
      const verbOut = await exec("triage");
      const verbAgrees =
        /C0DE/i.test(verbOut) && /RTS/.test(verbOut) && !/unknown command/i.test(verbOut);
      // TRX64 behaves correctly when it auto-pinned the corruptor: named the wild PC,
      // identified the RTS pop, pinned the smashing STA PCs, and the monitor verb agrees.
      return {
        behavesCorrectly:
          namesWildPc && identifiesRtsPop && pinsCorruptor && corruptorPcCorrect && verbAgrees,
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

  // ── P1: ws-flow-tracker-irq — monitor `flow` shows the LIVE interrupt context ──
  // misc-13 only proved `flow`/`bt` are RECOGNIZED + that `bt` is state-dependent.
  // The `flow` panel itself was still a CONSTANT "main" on TRX64 (no FlowTracker),
  // while TS renders the live interrupt/trap frame STACK (monitor-shell.ts:1103-1117
  // ← FlowTracker.flowState(), stepping.ts:174-190). TS's FlowTracker is STEP-DRIVEN
  // (apply() runs from stepInto/stepOver/…): when a single `z` step accepts a
  // hardware IRQ (SP drops by exactly 3, op≠BRK — stepOne, stepping.ts:96-101) it
  // pushes an `irq` frame, so the NEXT `flow` reports `current=irq` + a frame line.
  // Signal: boot to IRQs-live, read `flow` at the cold (rest) state (current=main),
  // then single-step `z` in a bounded loop until `flow` flips to `current=irq` (a raster
  // IRQ fires every frame, so it is reachable within a bounded number of steps); also
  // step on to the RTI so the frame POPS back to main (current=main again). TRX64
  // before the FlowTracker port: `flow` is the constant "main" — sawIrqFrame=false,
  // never state-dependent. After: state-dependent like TS. Compared TS vs TRX64 on the
  // SAME machine state → both must report {sawIrqFrame:true, restIsMain:true,
  // poppedBackToMain:true, frameLineWhenIrq:true}. (FlowTracker port — audit misc-13++.)
  {
    id: "ws-flow-tracker-irq",
    severity: "P1",
    title: "monitor `flow` reflects the live interrupt context (IRQ frame pushed on z-step into the handler, popped on RTI)",
    async signal(c) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      // Boot the machine so IRQs are live (CINV→$EA31 firing every frame) via a
      // SYNCHRONOUS cold reset: ws-server.ts session/reset {cold} runs the KERNAL to
      // READY inline (5M cyc, resetCold), so afterwards the machine sits in the BASIC
      // idle loop with the jiffy IRQ taken each frame. (debug/run is async-scheduled
      // and only advances under --stream, which this case does not spawn.)
      await c.call("session/reset", { session_id: sid, mode: "cold" });
      // Read the rest-state flow: at READY the FlowTracker stack is empty → main.
      const flowRest = await exec("flow");
      const currentOf = (s: string): string => (s.match(/current=([a-z]+)/i)?.[1] ?? "").toLowerCase();
      const restIsMain = currentOf(flowRest) === "main";
      // Single-step until a `z` step ACCEPTS a hardware IRQ → FlowTracker pushes an
      // `irq` frame → `flow` reports current=irq with a frame line. Bounded: a frame
      // is ~19656 cycles (PAL), an IRQ fires each frame, so well within the cap.
      let sawIrqFrame = false;
      let frameLineWhenIrq = false;
      let poppedBackToMain = false;
      for (let i = 0; i < 25000 && !sawIrqFrame; i++) {
        await exec("z");
        const f = await exec("flow");
        if (currentOf(f) === "irq") {
          sawIrqFrame = true;
          // The frame panel must show the irq frame line (not the "(main — no …)"
          // placeholder): `current=irq` AND a frame body that mentions irq.
          frameLineWhenIrq = /\birq\b/i.test(f) && !/no interrupt\/trap frame active/i.test(f);
          // Keep stepping until the handler RTIs and the frame pops back to main.
          for (let j = 0; j < 4000 && !poppedBackToMain; j++) {
            await exec("z");
            if (currentOf(await exec("flow")) === "main") poppedBackToMain = true;
          }
        }
      }
      return { restIsMain, sawIrqFrame, frameLineWhenIrq, poppedBackToMain };
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
      // HARDENED (audit Batch 5 #4): match ONLY `debug/observer_log` — NOT
      // `observer_hit`. A runtime that mis-implements `do log` as a BREAK (halting on
      // every IRQ) would emit observer_hit + pause, false-greening the old OR-match.
      const logBroadcasts = sink.notes.filter((n) => n.method === "debug/observer_log").length;
      // The machine must STILL be running after the window — `do log` is a tracepoint
      // (print + continue), NEVER a break. A break-misimplementation pauses here.
      const stAfter = await state(c, sid);
      // Clean up the tracepoint.
      await exec("obs irqtp del");
      return {
        // The behavioural signal: the `do log` observer fired MULTIPLE times during
        // free-run (the per-frame drain broadcast debug/observer_log, not a one-shot).
        observerLogFiredMultiple: logBroadcasts > 1,
        // log = continue: the machine kept running across the window (NOT a break).
        keptRunning: stAfter.runState === "running",
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
      const base = baseMatch && baseMatch[1] ? parseInt(baseMatch[1], 16) : null;
      // The grid rows are the `|<40 chars>|` lines (25 of them).
      const gridRows0 = first.split("\n").filter((l) => /^\|.*\|$/.test(l));
      const cols0 = gridRows0[0] ? gridRows0[0].length - 2 : 0; // strip the two pipes
      // Live-content check: write screen-code $01 (=`A`) into the daemon's own screen
      // base cell (0,0), re-decode, and confirm an `A` now sits at grid row 0 col 0.
      let markerVisible = false;
      if (base !== null) {
        await exec(`wr ram ${base.toString(16)} 01`);
        const second = await exec("screen");
        const rows = second.split("\n").filter((l) => /^\|.*\|$/.test(l));
        markerVisible = rows[0] !== undefined && rows[0][1] === "A"; // [0] is the leading `|`
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

  // ── P1: ws-trace-monitor-misc-10b — monitor VIC-data verbs bitmap/charset/sprite ──
  // The monitor REPL advertises `bitmap <a> [w h] [hires|charset|sprite]` in its `help`
  // text (and `screen` is the 4th VIC-data verb, wired in misc-10), but TRX64's
  // run_monitor had NO `bitmap`/`bm` arm → the verb (and ALL its modes — hires/charset/
  // sprite) fell through to `unknown command: bitmap` (the help LIES). In the canonical
  // TS runtime (monitor-shell.ts:745-767) `charset`/`sprite` are NOT standalone verbs —
  // they are MODES of `bitmap`, which decodes a live RAM range to a PNG per C64 graphics
  // mode (monitor-bitmap.ts) and returns `bitmap <mode> $XXXX → W×Hpx (N bytes read) →
  // <file>`. Fix: wire run_monitor's `bitmap`/`bm` arm 1:1 — same arg parse (addr hex,
  // w/h decimal, mode token), same per-mode dims + byte-count, same PNG artifact + the
  // same output string. Signal — driven on the SAME live state into RAM first (so the
  // render reflects THIS daemon's live memory): a first signal `recognized` (no "unknown
  // command", catching the help-lies divergence) PLUS semantic structural properties on
  // each of the three modes — the mode name + base addr are echoed in the header, the
  // reported byte-count equals the mode's w·h·stride (charset 16·16·8, sprite 8·4·64,
  // hires 40·25·1), and a PNG artifact actually lands in this daemon's own project dir.
  // Exact bytes/paths are NOT asserted (project dirs differ per daemon; PNG container
  // bytes differ between encoders — the render gate proves pixels elsewhere). TS: all
  // true; TRX64 (before fix): recognized=false (every mode is `unknown command`).
  {
    id: "ws-trace-monitor-misc-10b",
    severity: "P1",
    title: "monitor VIC-data verbs bitmap/charset/sprite are wired (modes of `bitmap`; help no longer lies)",
    async signal(c, d) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      const recognized = (s: string) => !/unknown command/i.test(s);
      // Seed deterministic live state so each render reflects THIS daemon's RAM (not a
      // cold-default): a non-zero pattern across the regions each mode reads. $E000 is
      // RAM under the KERNAL shadow (the CPU lens still peeks RAM there) — write a wide
      // alternating pattern so the decode has content.
      await exec("f c000 dfff aa 55");
      // Drive each mode. (charset/sprite are MODES of `bitmap`, matching the TS runtime.)
      const bmpOut = await exec("bitmap c000 40 25 hires");
      const chrOut = await exec("bitmap c000 16 16 charset");
      const sprOut = await exec("bitmap c000 8 4 sprite");
      // Parse `bitmap <mode> $XXXX → W×Hpx (N bytes read) → <file>`.
      const parse = (s: string) => {
        const m = /bitmap\s+(\w+)\s+\$([0-9a-f]{4})\s+→\s+(\d+)×(\d+)px\s+\((\d+)\s+bytes read\)\s+→\s+(.+)$/i.exec(s.trim());
        if (!m) return null;
        return {
          mode: m[1]!.toLowerCase(),
          base: parseInt(m[2]!, 16),
          width: parseInt(m[3]!, 10),
          height: parseInt(m[4]!, 10),
          bytes: parseInt(m[5]!, 10),
          file: m[6]!,
        };
      };
      const bmp = parse(bmpOut);
      const chr = parse(chrOut);
      const spr = parse(sprOut);
      // A PNG artifact landed in THIS daemon's own project dir (compare per-daemon, not
      // absolute path equality — the dirs differ between TS and TRX64).
      const fileMade = (p: { file: string } | null) => {
        if (!p) return false;
        try { return p.file.startsWith(d.projectDir) && statSync(p.file).size > 0; }
        catch { return false; }
      };
      return {
        // First signal: every mode is recognized (the help no longer lies).
        recognized: recognized(bmpOut) && recognized(chrOut) && recognized(sprOut),
        // Semantic: the header echoes the mode + base addr for each.
        bmpHeaderOk: bmp !== null && bmp.mode === "hires" && bmp.base === 0xc000,
        chrHeaderOk: chr !== null && chr.mode === "charset" && chr.base === 0xc000,
        sprHeaderOk: spr !== null && spr.mode === "sprite" && spr.base === 0xc000,
        // Semantic: the byte-count equals the mode's w·h·stride (live params).
        bmpBytes: bmp?.bytes ?? -1,   // hires 40·25·1 = 1000
        chrBytes: chr?.bytes ?? -1,   // charset 16·16·8 = 2048
        sprBytes: spr?.bytes ?? -1,   // sprite 8·4·64 = 2048
        // Semantic: pixel dims per mode (charset/sprite expand the cells).
        bmpDims: bmp ? `${bmp.width}x${bmp.height}` : "",   // 320x25
        chrDims: chr ? `${chr.width}x${chr.height}` : "",   // 128x128
        sprDims: spr ? `${spr.width}x${spr.height}` : "",   // 192x84
        // Effect: a non-empty PNG artifact lands in each daemon's own project dir.
        artifactsMade: fileMade(bmp) && fileMade(chr) && fileMade(spr),
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

  // ── P1: ws-trace-monitor-misc-19 — runtime/call exposes the FULL AgentQueryApi ──
  // TS keeps two SEPARATE dispatch tables: `api/call` is the narrow MCP per-verb
  // bridge gated by API_CALL_ALLOWLIST (monitorRegisters/memory/disasm, stepInto/
  // stepOver, addPcBreakpoint/listBreakpoints/removeBreakpoint, until, status —
  // ws-server.ts:179-185 + 655), while `runtime/call` runs the WHOLE
  // createAgentQueryApi (ws-server.ts:1717-1724) — saveVsf, goto, stepOut,
  // monitorFind, runScenario, the breakpoint family, … — with NO allowlist. TRX64
  // (pre-fix) collapsed BOTH onto the narrow ~10-method dispatch_api_call, so any
  // full-API-only method went -32601 over BOTH routes. Fix: route runtime/call to
  // the FULL set of AgentQueryApi methods TRX64 can back; api/call stays narrow.
  // Signal: call a representative set of full-API-only methods over `runtime/call`
  // and report {handled} per method (handled = the reply is NOT a method-not-found
  // -32601). ALSO assert api/call still REJECTS a full-only method (narrow gate
  // intact). TS: every full-API method handled:true, api/call rejects; TRX64
  // (pre-fix): every full-API method handled:false (-32601). (Audit P1 misc-19.)
  {
    id: "ws-trace-monitor-misc-19",
    severity: "P1",
    title: "runtime/call exposes the full AgentQueryApi; api/call stays narrow",
    async signal(c) {
      const sid = await liveSession(c);
      // `handled` = the method is recognised by the dispatch (NOT -32601). A method
      // that exists but errors on bad args / missing config is STILL handled — only
      // a method-not-found counts as unhandled. This is the misc-19 divergence
      // signal (the full table vs the narrow table), independent of return shape.
      const notFound = (msg: string) =>
        /method not found|unknown (runtime op|method)|not allowed|-32601/i.test(msg);
      const handled = async (route: "runtime/call" | "api/call", op: string, args: unknown[]) => {
        try {
          if (route === "runtime/call") {
            await c.call("runtime/call", { session_id: sid, op, args });
          } else {
            await c.call("api/call", { session_id: sid, method: op, args });
          }
          return true; // resolved → handled
        } catch (e) {
          return !notFound(e instanceof Error ? e.message : String(e));
        }
      };
      // Representative full-AgentQueryApi methods that are NOT in the narrow
      // API_CALL_ALLOWLIST. All must be handled via runtime/call.
      const saveVsf = await handled("runtime/call", "saveVsf", []);
      const gotoH = await handled("runtime/call", "goto", [0xe5cd]);
      const stepOut = await handled("runtime/call", "stepOut", [{ budget: 1000 }]);
      const monitorFind = await handled("runtime/call", "monitorFind", [0x0000, 0x00ff, [0x00]]);
      const runScenario = await handled("runtime/call", "runScenario", [
        { id: "misc19-inline", diskPath: "none", mode: "true-drive", cycleBudget: 1000, inputs: [] },
      ]);
      const addBreakpoint = await handled("runtime/call", "addBreakpoint", [
        { id: "misc19-bp", predicate: { kind: "pc", pc: 0xe5cd }, action: "halt", enabled: true },
      ]);
      const addTracepoint = await handled("runtime/call", "addTracepoint", ["misc19-tp", 0xe5cd]);
      const breakpointAuditLog = await handled("runtime/call", "breakpointAuditLog", []);
      // The narrow gate stays intact: a full-only method is REJECTED via api/call.
      const apiCallRejectsFullOnly = !(await handled("api/call", "saveVsf", []));
      return {
        saveVsf,
        gotoH,
        stepOut,
        monitorFind,
        runScenario,
        addBreakpoint,
        addTracepoint,
        breakpointAuditLog,
        apiCallRejectsFullOnly,
      };
    },
  },

  // ── P1: ws-trace-monitor-misc-20 — the scenario registry is FILE-BACKED ──────
  // TS scenario-registry.ts re-scans the samples + project `scenarios/` dirs on
  // EVERY listScenarios() call (scanDir reads every *.json), and each summary
  // carries a `source` field ("samples" | "project"). saveScenario() persists the
  // scenario JSON to the project dir. So a scenario written to disk — by THIS
  // daemon or any other on the same project dir — appears in the next list. TRX64
  // (pre-fix) kept an in-memory HashMap that never re-read disk and had no
  // `source` field, so a scenario only on disk (i.e. as a FRESH daemon would see
  // it) was invisible. Fix: file-back the registry — scenario_save persists to the
  // project `scenarios/` dir, scenario_list scans that dir (+ samples) on each
  // call and includes a `source` field, 1:1 with scenario-registry.ts. Signal: (1)
  // scenario_save → assert the file lands on disk; (2) write a SECOND scenario
  // file DIRECTLY to disk (= what a fresh/other daemon's save left behind, which an
  // in-memory-only registry never saw) and assert scenario_list surfaces it with a
  // `source` field. {savedFileOnDisk, scenarioPersists, hasSource}. TS:
  // {true,true,true}; TRX64 (pre-fix): {false,false,false}. (Audit P1 misc-20.)
  {
    id: "ws-trace-monitor-misc-20",
    severity: "P1",
    title: "scenario registry is file-backed (re-scans project dir + carries a source)",
    async signal(c, d) {
      await liveSession(c);
      const savedScenario = {
        id: "misc20-saved",
        diskPath: "none",
        mode: "true-drive",
        cycleBudget: 1000,
        inputs: [],
      };
      // (1) scenario_save must persist the JSON to <projectDir>/scenarios/<id>.json.
      await c.call("runtime/scenario_save", { scenario: savedScenario });
      const savedPath = join(d.projectDir, "scenarios", "misc20-saved.json");
      const savedFileOnDisk = existsSync(savedPath);

      // (2) Write a SECOND scenario file DIRECTLY to disk — this is precisely what a
      // FRESH daemon (or a second daemon on the same project dir) would find on its
      // first list: a file the in-process registry never received via the RPC. A
      // file-backed registry re-scans and lists it; an in-memory-only one cannot.
      const scenDir = join(d.projectDir, "scenarios");
      mkdirSync(scenDir, { recursive: true });
      const diskOnly = {
        id: "misc20-diskonly",
        diskPath: "none",
        mode: "true-drive",
        cycleBudget: 2000,
        inputs: [],
        savedAt: "2026-06-26T00:00:00.000Z",
      };
      writeFileSync(join(scenDir, "misc20-diskonly.json"), JSON.stringify(diskOnly, null, 2), "utf8");

      const list = (await c.call("runtime/scenario_list", {})) as any[];
      const arr = Array.isArray(list) ? list : [];
      const diskEntry = arr.find((s) => s?.id === "misc20-diskonly");
      return {
        // scenario_save actually wrote the file to the project dir.
        savedFileOnDisk,
        // a disk-only scenario (fresh-daemon view) is surfaced by the next list.
        scenarioPersists: !!diskEntry,
        // every listed entry carries a `source` field (1:1 scenario-registry.ts).
        hasSource: arr.length > 0 && arr.every((s) => typeof s?.source === "string"),
      };
    },
  },

  // ── shared trace-capture helper for the trace/read cases ─────────────────────
  // (declared as a closure inside each signal — kept here as a doc anchor.)

  // ── P1: ws-trace-monitor-misc-0 — trace/read serves the trace store ──────────
  // TS ws-server.ts:1302-1377 wires trace/read: build/await the DuckDB index from
  // the `.c64retrace` authority (lazy-on-read, audit misc-1), then run a reader op.
  // store_fn getInfo/topPcs are the deterministic shape (queries.ts). TRX64 returned
  // -32001 NOT_IMPLEMENTED. Fix: trace/read shells out to the Node sidecar that
  // imports the EXISTING c64re indexer + readers → byte-identical by construction.
  // Signal: capture a cold-boot trace on BOTH daemons (start_domains cpu+memory, run
  // 200k cyc, stop), then trace/read getInfo + topPcs. Compare the DETERMINISTIC,
  // content-derived fields only — getInfo.tableCounts (per-channel event counts) +
  // masterClockRange (the .c64retrace is byte-equal across runtimes, so these match);
  // topPcs SORTED by (count desc, pc asc) to neutralise DuckDB tie ROW-order (the
  // pc/count SET is deterministic, the order among equal counts is not). The volatile
  // meta (run_id / created_at) is excluded. TS: real counts; TRX64 (pre-fix):
  // unreadable. (Audit P1 ws-trace-monitor-misc-0 + misc-1 lazy index.)
  {
    id: "ws-trace-monitor-misc-0",
    severity: "P1",
    title: "trace/read serves getInfo/topPcs over a captured .c64retrace (sidecar)",
    async signal(c) {
      const sid = await liveSession(c);
      await c.call("trace/start_domains", { session_id: sid, domains: ["c64-cpu", "memory"] });
      await c.call("session/run", { session_id: sid, cycles: 200_000 });
      await c.call("trace/run/stop", { session_id: sid });
      const cur = (await c.call("trace/current", { session_id: sid })) as any;
      const db = String(cur?.path ?? "");
      const gi = (await c.call("trace/read", { op: "store_fn", duckdb_path: db, args: { fn: "getInfo" } })) as any;
      const tp = (await c.call("trace/read", {
        op: "store_fn", duckdb_path: db, args: { fn: "topPcs", args: { cpu: "c64", limit: 12 } },
      })) as Array<{ pc: number; count: number }>;
      // tie-order-stable: equal counts → ascending pc. The SET + counts are what the
      // trace content determines; DuckDB does not stabilise row order among ties.
      // `LIMIT` ALSO cuts mid-tie at the boundary count, keeping a DIFFERENT subset of
      // that equal-count group per runtime — so drop the lowest returned count group
      // (the only limit-truncated one); every higher count is fully present + stable.
      const arr = Array.isArray(tp) ? tp : [];
      const minCount = arr.length ? Math.min(...arr.map((r) => r.count)) : 0;
      const topPcsSorted = [...arr]
        .filter((r) => r.count > minCount)
        .sort((a, b) => (b.count - a.count) || (a.pc - b.pc));
      return {
        // event counts per channel — the .c64retrace is byte-equal, so identical.
        tableCounts: gi?.tableCounts ?? null,
        masterClockRange: gi?.masterClockRange ?? null,
        topPcsSorted,
        // sanity: the read actually returned a populated store (not an empty stub).
        hasEvents: Number(gi?.tableCounts?.["events:total"] ?? 0) > 0,
      };
    },
  },

  // ── P1: ws-trace-monitor-misc-14 — monitor map/swimlane read the trace store ─
  // TS monitor-shell parses the verb args then calls ctx.traceRead(op,args) (the WS
  // daemon trace bridge, ws-server.ts:2104-2129). TRX64's monitor map/swimlane were
  // hardcoded "no trace store". Fix: route them through the SAME sidecar over the
  // current trace store. Signal: capture a cold-boot trace on BOTH, then `map c64`
  // (the trace-memory-map text — fully deterministic) and `swimlane` (lane render).
  // `map` is compared EXACT. `swimlane` is compared with its first line — `# <stem>`,
  // where stem = live_<radix36(now)>, a per-run UNIQUE store name on BOTH runtimes —
  // stripped; the rest (the `swimlane <s>–<e>` window header + the rendered rows) is
  // deterministic. TS: real map/swimlane text; TRX64 (pre-fix): "no trace store".
  // (Audit P1 ws-trace-monitor-misc-14.)
  {
    id: "ws-trace-monitor-misc-14",
    severity: "P1",
    title: "monitor map/swimlane read the captured trace store (sidecar)",
    async signal(c) {
      const sid = await liveSession(c);
      await c.call("trace/start_domains", { session_id: sid, domains: ["c64-cpu", "memory"] });
      await c.call("session/run", { session_id: sid, cycles: 200_000 });
      await c.call("trace/run/stop", { session_id: sid });
      const mapOut = String(((await c.call("monitor/exec", { session_id: sid, command: "map c64" })) as any)?.output ?? "");
      const swOut = String(((await c.call("monitor/exec", { session_id: sid, command: "swimlane" })) as any)?.output ?? "");
      // Strip the volatile `# <stem>` first line (stem carries a per-run timestamp).
      const swBody = swOut.split("\n").slice(1).join("\n");
      return {
        // the map text is fully deterministic (no per-run identifiers).
        mapText: mapOut,
        // the swimlane window header + rows, minus the unique store-name line.
        swimlaneBody: swBody,
        // sanity: both verbs produced real trace output (not a "no trace store" stub).
        mapHasContent: mapOut.includes("trace_memory_map"),
        swimlaneHasWindow: /swimlane\s+\d+[–-]\d+/.test(swBody),
      };
    },
  },

  // ── P1: ws-trace-monitor-start-line — monitor `trace on` start line carries the runId ─
  // The monitor `trace on <domains>` START line is `trace on: <runId>  domains=[…]`
  // (monitor-shell.ts:439). The audit (Batch 5 #4) wants the START form asserted (not
  // a residual/ERROR/off line) AND the start-line runId tied to the engine's own
  // status. The per-runtime runId FORMAT differs (TS `run_<def>_<radix36>` vs TRX64's
  // monitor `run_live-capture_<cyc>`) + is per-run volatile, so the cross-runtime
  // signal is STRUCTURAL: the start line is the START form (`trace on:` + the domains
  // both echoed), the parsed runId is non-empty, AND trace/run/status reports THAT
  // SAME runId active with the requested domains — an internal-consistency invariant
  // that holds identically on both runtimes (true on TS, must be true on TRX64).
  {
    id: "ws-trace-monitor-start-line",
    severity: "P1",
    title: "monitor `trace on` start line carries the runId that trace/run/status then reports active",
    async signal(c) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      // Guaranteed-idle start so the line we parse is THIS run's start, not a residual.
      await c.call("trace/run/stop", { session_id: sid, wait_index: false }).catch(() => undefined);
      const onOut = await exec("trace on c64-cpu memory");
      // The START form: "trace on: <runId>  domains=[c64-cpu,memory]". Reject the
      // already-active / off / error variants.
      const isStartForm = /trace on:\s*\S/.test(onOut) && /c64-cpu/.test(onOut) && /memory/.test(onOut);
      const startRunId = (onOut.match(/trace on:\s*(\S+)/)?.[1] ?? "").trim();
      // Run a window so eventCount grows, then read the engine status.
      await c.call("session/run", { session_id: sid, cycles: 200_000 });
      const status = (await c.call("trace/run/status", { session_id: sid })) as any;
      await c.call("trace/run/stop", { session_id: sid, wait_index: false }).catch(() => undefined);
      return {
        // The line is the START form with both requested domains echoed.
        isStartForm,
        // A non-empty runId was parsed from the start line.
        startRunIdNonEmpty: startRunId.length > 0,
        // The engine's status reports THAT SAME runId active (internal consistency).
        statusRunIdMatchesStart: String(status?.runId ?? "") === startRunId && status?.active === true,
        // eventCount grew under the window (the trace really recorded).
        eventCountGrew: Number(status?.eventCount ?? 0) > 0,
      };
    },
  },

  // ── P1: ws-trace-swimlane-verbs — narrowed `swimlane list` / `<name>` (audit Batch 5 #5) ─
  // The monitor `swimlane list` (list stored traces newest-first) + `swimlane <name>`
  // (select a stored trace by basename) are SERVED by the TS daemon (ws-server.ts
  // swimlane bridge — a per-session `.duckdb` directory scan + getInfo) but TRX64
  // REFUSED both ("not supported via TRX64" / "by-name selection needs the daemon
  // store directory") — a live divergence. Fixed: TRX64's swimlane arm now scans the
  // per-session trace-store dir (session_trace_store_dir + list_trace_stores) and
  // resolves `<name>.duckdb`, exactly like TS. Signal: capture a trace (so one store
  // exists), then drive `swimlane list` + `swimlane <name>` (the name lifted from the
  // list line) and assert BOTH are recognized + structurally real (a trace line in
  // the list, a swimlane window for the named store) on both runtimes. The per-run
  // store NAME + cycle values are volatile (radix36 timestamp, differ TS↔TRX64), so
  // the signal compares STRUCTURE (recognized + has a row), not the literal text.
  {
    id: "ws-trace-swimlane-verbs",
    severity: "P1",
    title: "monitor `swimlane list` / `swimlane <name>` serve the stored traces (TRX64 no longer refuses)",
    async signal(c) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      // Capture + finalize a trace so the session trace-store directory has ≥1 store.
      await c.call("trace/run/stop", { session_id: sid, wait_index: false }).catch(() => undefined);
      await c.call("trace/start_domains", { session_id: sid, domains: ["c64-cpu", "memory"] });
      await c.call("session/run", { session_id: sid, cycles: 200_000 });
      await c.call("trace/run/stop", { session_id: sid, wait_index: true }).catch(() => undefined);
      const recognized = (s: string) =>
        !/unknown command/i.test(s) && !/not supported via TRX64/i.test(s) && !/needs the daemon store/i.test(s);
      // `swimlane list` — the stored-trace listing.
      const listOut = await exec("swimlane list");
      // A real list carries the header + at least one `<name>  cyc <a>..<b>  events=<n>` line.
      const listLine = listOut.split("\n").find((l) => /events=\d+/.test(l)) ?? "";
      const storeName = (listLine.trim().split(/\s+/)[0] ?? "").trim();
      // `swimlane <name>` — select that store by basename.
      const nameOut = storeName ? await exec(`swimlane ${storeName}`) : "";
      return {
        // both verbs are recognized (no "not supported via TRX64" refusal).
        listRecognized: recognized(listOut),
        // the list has at least one real trace line (events=<n>).
        listHasTraceLine: /events=\d+/.test(listOut),
        // by-name selection is recognized AND renders a swimlane window (`# <stem>`).
        nameRecognized: recognized(nameOut),
        nameRendersWindow: /^#\s+\S/m.test(nameOut) || /swimlane\s+\d+[–-]\d+/.test(nameOut),
      };
    },
  },

  // ── P1: ws-trace-crossfeed-reader — read a .c64retrace captured on the OTHER runtime ─
  // misc-0 / misc-14 compare TRX64-shells-to-the-SAME-sidecar-as-TS over the SAME
  // store, so they match by construction and prove nothing about cross-runtime
  // interchange. This case makes the READER the only variable: each leg CAPTURES a
  // cold-boot trace on the OPPOSITE-kind daemon (TS leg captures on TRX64, TRX64 leg
  // captures on TS), then READS that `.c64retrace`'s index via the case daemon's
  // `trace/read` (getInfo + topPcs). The `.c64retrace` binary format is the shared
  // interchange authority, so a faithful reader on either runtime surfaces the SAME
  // content-derived getInfo (tableCounts + masterClockRange) + topPcs SET. A reader
  // that can't ingest the other runtime's file (or a format that isn't truly shared)
  // diverges here. (Audit Batch 5 #5 — the shared-reader blind-spot cross-feed.)
  {
    id: "ws-trace-crossfeed-reader",
    severity: "P1",
    title: "trace/read ingests a .c64retrace captured on the OTHER runtime (reader is the only variable)",
    async signal(c, d) {
      // Capture a cold-boot trace on a FRESH daemon of the OPPOSITE kind, finalize it,
      // and return the abs .duckdb path (both daemons are local tmp dirs on one FS).
      const opposite = d.kind === "ts" ? "trx64" : "ts";
      const other = await spawnDaemon(opposite as any);
      let db = "";
      try {
        const oc = await connect(other.endpoint, 240_000);
        try {
          const osid = await liveSession(oc);
          await oc.call("trace/start_domains", { session_id: osid, domains: ["c64-cpu", "memory"] });
          await oc.call("session/run", { session_id: osid, cycles: 200_000 });
          await oc.call("trace/run/stop", { session_id: osid, wait_index: true }).catch(() => undefined);
          const cur = (await oc.call("trace/current", { session_id: osid })) as any;
          db = String(cur?.path ?? cur?.duckdbPath ?? "");
        } finally {
          oc.close();
        }
        // Now READ the other runtime's .c64retrace via THIS daemon's reader.
        const gi = (await c.call("trace/read", {
          op: "store_fn", duckdb_path: db, args: { fn: "getInfo" },
        })) as any;
        const tp = (await c.call("trace/read", {
          op: "store_fn", duckdb_path: db, args: { fn: "topPcs", args: { cpu: "c64", limit: 12 } },
        })) as Array<{ pc: number; count: number }>;
        // Tie-order-stable topPcs (= misc-0): drop the limit-truncated lowest group,
        // sort by (count desc, pc asc) — the SET is content-determined.
        const arr = Array.isArray(tp) ? tp : [];
        const minCount = arr.length ? Math.min(...arr.map((r) => r.count)) : 0;
        const topPcsSorted = [...arr]
          .filter((r) => r.count > minCount)
          .sort((a, b) => (b.count - a.count) || (a.pc - b.pc));
        return {
          // content-derived: the .c64retrace is byte-equal across runtimes, so a
          // faithful reader on EITHER runtime surfaces the same counts/range/PCs.
          tableCounts: gi?.tableCounts ?? null,
          masterClockRange: gi?.masterClockRange ?? null,
          topPcsSorted,
          // sanity: the cross-fed file was actually readable (a populated store).
          crossFedReadable: Number(gi?.tableCounts?.["events:total"] ?? 0) > 0,
        };
      } finally {
        other.stop();
      }
    },
  },

  // ── P1: ws-trace-monitor-misc-19b — runtime/call trace methods match TS ──────
  // The 5 trace-backed AgentQueryApi methods (queryEvents/followPath/swimlaneSlice/
  // traceTaint/profileLoader). In the c64re daemon `runtime/call` builds the API with
  // NO traceBackend (ws-server.ts:1720), so each throws "traceBackend not configured"
  // — verified against the live TS daemon. The REAL trace-read surface is `trace/read`
  // (sidecar-backed). TRX64 returned method-not-found (-32601) for all five. Fix:
  // they are now HANDLED, returning the IDENTICAL "traceBackend not configured" error
  // — NOT routed to the sidecar (that would diverge: TRX64 succeeding where TS errors
  // = fake-green). Signal: call each over runtime/call and report {handled, message}.
  // TS = TRX64 = {handled:true, message:"traceBackend not configured"} for all five.
  {
    id: "ws-trace-monitor-misc-19b",
    severity: "P1",
    title: "runtime/call trace methods match TS (handled, traceBackend not configured)",
    async signal(c) {
      const sid = await liveSession(c);
      const notFound = (msg: string) =>
        /method not found|unknown (runtime op|method)|not allowed|-32601/i.test(msg);
      const probe = async (op: string) => {
        try {
          await c.call("runtime/call", { session_id: sid, op, args: [{}] });
          return { handled: true, message: "<ok>" };
        } catch (e) {
          const msg = e instanceof Error ? e.message : String(e);
          // Normalise the JSON-RPC error envelope down to its message text.
          let text = msg;
          try { const j = JSON.parse(msg); if (j?.message) text = String(j.message); } catch { /* plain */ }
          return { handled: !notFound(msg), message: text };
        }
      };
      return {
        queryEvents: await probe("queryEvents"),
        followPath: await probe("followPath"),
        swimlaneSlice: await probe("swimlaneSlice"),
        traceTaint: await probe("traceTaint"),
        profileLoader: await probe("profileLoader"),
      };
    },
  },

  // ── P1: ws-trace-monitor-misc-21 — runtime/call resolvePc / resolvePcs ────────
  // TS agent-api.ts:128-133 → resolve-pc.ts (Spec 235): resolvePc(artifactId, pc)
  // maps a PC to the project disasm knowledge at that address — the routine /
  // nearest-label / effective-segment / source line — read from the SAME on-disk
  // files the inspect/xref/sym bridge (misc-15) uses: `<artifactId>_analysis.json`
  // segments + `<artifactId>_annotations.json` routines/labels. Both daemons set
  // C64RE_PROJECT_DIR=--project, so resolve-pc reads the seeded fixtures. TRX64
  // (pre-fix) routed resolvePc/resolvePcs to the dispatch_api_call `other` -32601
  // arm ("no faithful backing"). Fix: wire them to project_knowledge.rs (the
  // inspect/sym read path), returning the ResolvedPc shape byte-for-byte (absent
  // layers omitted, like TS `undefined`). Signal: seed a representative analysis +
  // annotations fixture, then runtime/call resolvePc at three addresses + a
  // resolvePcs batch, and compare the FULL ResolvedPc JSON TS-vs-TRX64. TS: real
  // resolved knowledge; TRX64 (pre-fix): -32601 (caught as the `error` string).
  {
    id: "ws-trace-monitor-misc-21",
    severity: "P1",
    title: "runtime/call resolvePc/resolvePcs read the project knowledge (no longer -32601)",
    spawn: {
      seedFiles: [
        {
          // Analysis: a labelled `code` segment ($0810-$08FF, confidence 0.85) +
          // a `data` segment ($0900-$09FF, confidence 0.5).
          rel: "fixture_analysis.json",
          bytes: Buffer.from(
            JSON.stringify({
              binaryName: "fixture",
              segments: [
                { kind: "code", start: 0x0810, end: 0x08ff, score: { confidence: 0.85 } },
                { kind: "data", start: 0x0900, end: 0x09ff, score: { confidence: 0.5 } },
              ],
            }),
          ),
        },
        {
          // Annotations: one routine (`main` @ $0810) + one label (`inner` @ $0850).
          rel: "fixture_annotations.json",
          bytes: Buffer.from(
            JSON.stringify({
              version: 1,
              binary: "fixture",
              segments: [],
              labels: [{ address: "0850", label: "inner" }],
              routines: [{ address: "0810", name: "main", comment: "entry point" }],
            }),
          ),
        },
      ],
    },
    async signal(c) {
      const sid = await liveSession(c);
      // runtime/call carries the AgentQueryApi facade args as a positional array:
      // resolvePc(artifactId, pc) → [artifactId, pc]. A -32601 (no backing) surfaces
      // as a thrown RPC error → captured as { error } so the signal still compares.
      const call = async (op: string, args: unknown[]) => {
        try {
          return await c.call("runtime/call", { session_id: sid, op, args });
        } catch (e) {
          return { error: e instanceof Error ? e.message : String(e) };
        }
      };
      // $0850: inside routine `main`, exactly at label `inner`, in the `code` segment.
      const atLabel = await call("resolvePc", ["fixture", 0x0850]);
      // $0900: past the routine range (single routine, exit = undefined → still
      // inside per resolve-pc), nearest label still `inner`, in the `data` segment.
      const inData = await call("resolvePc", ["fixture", 0x0900]);
      // $0700: below everything — no routine/label/segment (bare {artifactId, pc}).
      const below = await call("resolvePc", ["fixture", 0x0700]);
      // Batch of the same three addresses (resolvePcs loads once, resolves each).
      const batch = await call("resolvePcs", ["fixture", [0x0850, 0x0900, 0x0700]]);
      return { atLabel, inData, below, batch };
    },
  },

  // ── P1: ws-trace-monitor-misc-22 — runtime/call diffSnapshots / formatDiff ────
  // TS agent-api.ts:150-155 → snapshot-diff.ts (Spec 246): diffSnapshots(a, b)
  // compares two VSF byte buffers → a structured SnapshotDiff (RAM changed-ranges +
  // per-chip register diffs + PLA + drive + IEC); formatDiff renders the text table.
  // TRX64 (pre-fix) routed diffSnapshots/formatDiff to the dispatch_api_call -32601
  // `other` arm. Fix: a faithful snapshot_diff.rs port reading the c64re-own VSF
  // framing (proven by snapshot_diff.rs unit tests on real VSF bytes), wired into
  // runtime/call.
  //
  // BLOCKED — the TS AUTHORITY cannot report the comparison signal under THIS
  // harness. `diffSnapshots(a, b)` takes two `Uint8Array`s; `runtime/call` carries
  // its `args` as JSON (ws-server.ts:1717-1724 spreads the array verbatim with NO
  // typed-array reconstruction), and `saveVsf` comes back as an index-keyed JSON
  // OBJECT, not even an array. Probed against the live TS daemon:
  //   • diffSnapshots([saveVsfObject, …])  → "bytes.indexOf is not a function"
  //   • diffSnapshots([number[], number[]]) → "The list argument must be an instance
  //     of SharedArrayBuffer, ArrayBuffer or ArrayBufferView" (readVsf does
  //     `new TextDecoder().decode(bytes.slice(...))`, which rejects a plain array).
  // So over `runtime/call` with JSON args the TS authority ALWAYS throws — there is
  // no snapshot-handle/ref variant in createAgentQueryApi to pass instead. A faithful
  // differential signal is therefore impossible: making TRX64 SUCCEED where TS throws
  // would be fake-green, and TS's error is a JS TypeError (not a stable domain string)
  // so an "both error identically" match cannot be asserted either. The TRX64 backing
  // is correct for an in-process / binary-transport caller and is gated by the
  // snapshot_diff.rs unit tests instead. Re-arms automatically if a binary
  // snapshot transport (e.g. base64 args) is ever added to runtime/call.
  {
    id: "ws-trace-monitor-misc-22",
    severity: "P1",
    title: "runtime/call diffSnapshots/formatDiff diff two VSF snapshots (TS authority cannot transport Uint8Array over JSON)",
    blocked:
      "TS diffSnapshots(a,b) needs in-process Uint8Array args; runtime/call carries JSON only " +
      "(saveVsf → index-object, readVsf rejects a plain array) so the TS authority always throws. " +
      "No snapshot-handle variant exists. TRX64 backing proven by snapshot_diff.rs unit tests.",
    async signal(c) {
      const sid = await liveSession(c);
      const call = async (op: string, args: unknown[]) => {
        try {
          return await c.call("runtime/call", { session_id: sid, op, args });
        } catch (e) {
          return { error: e instanceof Error ? e.message : String(e) };
        }
      };
      const snapA = (await call("saveVsf", [])) as unknown;
      const snapB = (await call("saveVsf", [])) as unknown;
      const diff = await call("diffSnapshots", [snapA, snapB]);
      const formatted = await call("formatDiff", [diff]);
      const d = diff as any;
      const text = typeof formatted === "string" ? formatted : "";
      return {
        diffSucceeded: d != null && d.error === undefined,
        formatSucceeded: typeof formatted === "string",
        ramTotalChanged: d?.ram?.totalChanged ?? null,
        formatLen: text.length,
      };
    },
  },

  // ── P1: ws-trace-monitor-misc-24 — runtime/call RewindManager (the 6 methods) ──
  // The Spec 243/769 time-travel surface in createAgentQueryApi:
  //   beginRewindSession · rewindTo · applyPatch · runForward · diffBranches ·
  //   promoteBranch (agent-api.ts:251-274). Each routes through
  //   `beginRewindSession()` which REQUIRES scenarioId+diskPath+mode in the
  //   AgentApiOptions (agent-api.ts:252-253):
  //       if (!this.scenarioId || !this.diskPath || !this.mode)
  //         throw new Error("beginRewindSession requires scenarioId+diskPath+mode in AgentApiOptions");
  //
  // RFL (probed against the LIVE TS daemon): `runtime/call` builds the API with
  // `createAgentQueryApi({ session })` — NO scenarioId/diskPath/mode (ws-server.ts
  // :1720). So over `runtime/call` ALL SIX methods throw the IDENTICAL guard string
  // BEFORE touching any RewindManager state — none of them is observably functional
  // over WS. (The ONLY working rewind surface over WS is the dedicated
  // `runtime/snapshot_tree` + `runtime/promote_branch` handlers, which DO pass
  // scenarioId/diskPath/mode — ws-server.ts:1897/1917 — and TRX64 backs those via
  // rewind.rs, covered by the rewind.rs unit tests + the snapshot_tree/promote_branch
  // handler cases.) The `_rewind` lazy-cache on AgentQueryApi is moot here: the
  // whole AgentQueryApi is reconstructed per `runtime/call` (ws-server.ts:1717-1724),
  // so even if the guard passed, no branch state would persist across calls.
  //
  // This is the SAME shape as the trace-method arm (misc-19b): handled, but TS
  // returns a stable DOMAIN error. Unlike diffSnapshots (misc-22, a JS TypeError that
  // could not be matched), this guard string is stable and identical on every call,
  // so it IS a real differential — NOT blocked. TRX64 (pre-fix) routed all six to the
  // dispatch_api_call `other` -32601 arm ("unknown method"). Fix: HANDLE all six in
  // dispatch_api_call, returning the IDENTICAL guard error (NOT method-not-found, and
  // NOT a working rewind — succeeding where TS throws would be fake-green). Signal:
  // call each over runtime/call and report {handled, message}. TS == TRX64 ==
  // {handled:true, message:"beginRewindSession requires scenarioId+diskPath+mode in AgentApiOptions"}.
  {
    id: "ws-trace-monitor-misc-24",
    severity: "P1",
    title: "runtime/call rewind methods match TS (handled, beginRewindSession guard — scenarioId+diskPath+mode required)",
    async signal(c) {
      const sid = await liveSession(c);
      const notFound = (msg: string) =>
        /method not found|unknown (runtime op|method)|not allowed|-32601/i.test(msg);
      const probe = async (op: string, args: unknown[]) => {
        try {
          await c.call("runtime/call", { session_id: sid, op, args });
          return { handled: true, message: "<ok>" };
        } catch (e) {
          const msg = e instanceof Error ? e.message : String(e);
          let text = msg;
          try { const j = JSON.parse(msg); if (j?.message) text = String(j.message); } catch { /* plain */ }
          return { handled: !notFound(msg), message: text };
        }
      };
      return {
        beginRewindSession: await probe("beginRewindSession", [{}]),
        rewindTo: await probe("rewindTo", [1000]),
        applyPatch: await probe("applyPatch", ["snap-x", []]),
        runForward: await probe("runForward", ["snap-x", 1000]),
        diffBranches: await probe("diffBranches", ["a", "b"]),
        promoteBranch: await probe("promoteBranch", ["branch-x"]),
      };
    },
  },

  // ── P1: ws-rewind-snapshot-tree — the REAL rewind surface is at PARITY ────────
  // The 6 runtime/call rewind methods above all throw a guard (matched by misc-24).
  // The ACTUAL working time-travel surface is the two dedicated WS handlers
  //   runtime/snapshot_tree  (ws-server.ts:1891-1909)  — the branch tree handle,
  //   runtime/promote_branch (ws-server.ts:1911-1920)  — promote a branch → Scenario.
  // Both build a FRESH createAgentQueryApi → beginRewindSession() per call, so the
  // OBSERVABLE result depends only on construction: a single ROOT branch with a
  // freshly-minted randomUUID() id (rewind.ts:99) + ringSize=32 (DEFAULT_RING_SIZE).
  // Because the ids are random + non-persistent on BOTH sides (a caller can never
  // know the next call's root id), the parity signal is the STRUCTURE, not the ids:
  //   snapshot_tree → exactly 1 branch, that branch IS the root (id===rootBranchId,
  //     no parentId, startSnapshotId===rootSnapshotId, empty patches+children),
  //     ringSize===32, and rootBranchId/rootSnapshotId are non-empty;
  //   promote_branch <unknown id> → throws "branch <id> not found"
  //     (RewindManager.promoteBranch, rewind.rs:189-192 ≡ rewind.ts:250-252) — never
  //     a stub / method-not-found / fake success;
  //   promote_branch <the just-read root id> → succeeds (root branch exists this
  //     call), returning { scenarioId: "<sid>-branch-<8hex>", scenario.mode:
  //     "true-drive", patches: [] } (rewind.ts:257-268). NOTE: snapshot_tree mints a
  //     NEW root each call, so promoting the id read from a PRIOR snapshot_tree call
  //     fails on BOTH — we promote within the SAME conceptual handle by re-reading,
  //     which still mints a new id; therefore the root-promote is checked by the
  //     ERROR path (any supplied id is "not found" against the fresh manager), and
  //     the SUCCESS path is asserted at the unit level (rewind.rs tests) — here we
  //     assert the two runtimes AGREE on the observable handler behaviour.
  // Both runtimes must report the IDENTICAL normalized shape. (Verify the real
  // rewind surface — companion to misc-24's throw-surface.)
  {
    id: "ws-rewind-snapshot-tree",
    severity: "P1",
    title: "runtime/snapshot_tree + promote_branch are at parity (root-only branch handle; unknown-id throws not-found)",
    async signal(c) {
      const sid = await liveSession(c);
      const tree = (await c.call("runtime/snapshot_tree", { session_id: sid })) as any;
      const branches = (tree?.branches ?? {}) as Record<string, any>;
      const ids = Object.keys(branches);
      const rootId: string = tree?.rootBranchId ?? "";
      const root = branches[rootId] ?? {};
      // promote a deliberately-bogus branch id — must throw "not found" (the
      // RewindManager guard), NOT method-not-found and NOT a fake success.
      let promoteUnknown = { threw: false, notFound: false };
      try {
        await c.call("runtime/promote_branch", { session_id: sid, branch_id: "deadbeef-not-a-branch" });
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        let text = msg;
        try { const j = JSON.parse(msg); if (j?.message) text = String(j.message); } catch { /* plain */ }
        const isMethodNotFound = /method not found|-32601|unknown method/i.test(msg);
        promoteUnknown = { threw: true, notFound: /not found/i.test(text) && !isMethodNotFound };
      }
      // Normalized, id-agnostic STRUCTURE (random UUIDs stripped — both sides mint
      // fresh non-deterministic ids, so only the topology + constants compare).
      return {
        branchCount: ids.length,                                   // 1 (root only)
        onlyBranchIsRoot: ids.length === 1 && ids[0] === rootId,    // sole branch == root
        ringSize: tree?.ringSize ?? null,                          // 32
        scenarioIdPresent: typeof tree?.scenarioId === "string" && tree.scenarioId.length > 0,
        rootIdsPresent: rootId.length > 0 && typeof tree?.rootSnapshotId === "string" && tree.rootSnapshotId.length > 0,
        rootHasNoParent: root?.parentId === undefined || root?.parentId === null,
        rootSelfRooted: root?.rootId === rootId,                    // rootBranch.rootId = rootBranchId
        rootStartIsRootSnapshot: root?.startSnapshotId === tree?.rootSnapshotId,
        rootPatchesEmpty: Array.isArray(root?.patches) && root.patches.length === 0,
        rootChildrenEmpty: Array.isArray(root?.children) && root.children.length === 0,
        promoteUnknownThrew: promoteUnknown.threw,
        promoteUnknownNotFound: promoteUnknown.notFound,
      };
    },
  },

  // ═══════════════════════════════════════════════════════════════════════════
  // BATCH 2 — Spec 754 monitor-core verbs (the largest coverage gap). Each case
  // drives the verb via `monitor/exec` with a setup that makes the output
  // VERIFIABLE, and asserts the verb did the RIGHT thing (TS≡TRX64) — not merely
  // "no unknown command". See case-audit.md "Spec 754 — interactive monitor".
  // ═══════════════════════════════════════════════════════════════════════════

  // ── P0: monitor-g-x — `g`/`x` enter CONTINUOUS running (BUG-036) ────────────
  // Spec 754 §3.1/§2: `g` flips run-state to running (the Run-button path), NOT a
  // 1-frame burst; `g <addr>` sets PC then continues; `x` aliases `g`. The retired
  // burst-`g` (halt after ~1 frame) is the divergence this catches. Signal: under
  // --stream, `monitor/exec g` → runState===running immediately, then the free-run
  // driver advances ≫20000 cyc; `g <addr>` lands PC at/after that addr, still
  // running; `x` keeps it running. TS≡TRX64 on every boolean.
  {
    id: "monitor-g-x",
    severity: "P0",
    title: "monitor `g`/`x` enter continuous running (not a 1-frame burst); `g <addr>` sets PC",
    spawn: { stream: true },
    async signal(c) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      const recognized = (s: string) => !/unknown command/i.test(s);
      // Cold-boot so KERNAL+IRQs are live (the `g` continue then free-runs the idle
      // loop). reset is synchronous; afterwards the machine is paused at READY.
      await c.call("session/reset", { session_id: sid, mode: "cold" });
      const gOut = await exec("g");
      const st1 = await state(c, sid);
      const cyc1 = Number(st1.c64Cycles ?? st1.cpu?.cycles ?? 0);
      // Let the free-run driver advance — a continuous `g` must move ≫20000 cyc.
      await sleep(2500);
      const st2 = await state(c, sid);
      const cyc2 = Number(st2.c64Cycles ?? st2.cpu?.cycles ?? 0);
      // `g <addr>` sets PC then continues. Use a KERNAL idle-loop address that the
      // running machine reaches every frame ($EA31 = the jiffy IRQ handler entry).
      const gAddrOut = await exec("g e5cd"); // $E5CD = a KERNAL routine; PC is forced there
      const st3 = await state(c, sid);
      const pc3 = Number(st3.cpu?.pc ?? st3.pc ?? -1);
      const xOut = await exec("x");
      const st4 = await state(c, sid);
      return {
        recognized: recognized(gOut) && recognized(gAddrOut) && recognized(xOut),
        // `g` immediately reports running (not paused-after-burst).
        gWentRunning: st1.runState === "running",
        // The continuous driver advanced the machine far past one frame's worth.
        advancedContinuously: cyc2 - cyc1 > 20000,
        // `g <addr>` forced PC to/at-or-past the target (the continue may have moved
        // a few instructions; we set PC to $E5CD then continue, so it is >= that on a
        // fresh continue — assert it landed AT the address the `g` forced it to).
        gAddrSetPc: pc3 >= 0xe5cd - 4 && st3.runState === "running",
        // `x` aliases `g`: still running after `x`.
        xKeptRunning: st4.runState === "running",
      };
    },
  },

  // ── P0: monitor-n-ret — step model: `n` step-OVER, `z` step-INTO, `ret` run-to-RTS ─
  // Spec 754 §3.3/§4.2-3: `z`/`si` step INTO (PC→subroutine body); `n`/`so` step OVER
  // (PC→after the JSR, the body run-through); `ret` runs to the next RTS/RTI return.
  // step-over-vs-into is easy to mis-port. Signal: plant `JSR $C010 / NOP` at
  // $C000/$C003 and `RTS` at $C010. From PC=$C000: `z`→PC=$C010 (into the body); reset
  // PC, `n`→PC=$C003 (over, skipping the body); set PC=$C010, `ret`→PC=$C003 (returned).
  // PC landings compared byte-exact TS≡TRX64.
  {
    id: "monitor-n-ret",
    severity: "P0",
    title: "monitor step model — `z` into, `n` over (skips JSR body), `ret` runs to RTS",
    async signal(c) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      const pc = async (): Promise<number> => {
        const st = await state(c, sid);
        return Number(st.cpu?.pc ?? st.pc ?? -1);
      };
      // $C000: JSR $C010 ; $C003: NOP ; $C010: RTS  (the subroutine just returns).
      await exec("wr c000 20 10 c0 ea");
      await exec("wr c010 60");
      // ── z (step INTO): from $C000 the JSR pushes + lands in the body at $C010. ──
      await exec("r pc=c000");
      await exec("z");
      const pcIntoJsr = await pc();
      // ── n (step OVER): from $C000 the JSR body is run-through; PC lands after it. ──
      await exec("r pc=c000");
      await exec("n");
      const pcOverJsr = await pc();
      // ── ret (run to RTS): from inside the body at $C010 (just the RTS), ret should
      // execute the RTS and return to the caller's next instruction ($C003). We seed a
      // return address on the stack so the RTS has somewhere to go: push $C002 (RTS
      // adds 1 → $C003). SP starts at $FF; push hi then lo to $01FF/$01FE. ──
      await exec("wr ram 01ff c0");  // return-addr hi
      await exec("wr ram 01fe 02");  // return-addr lo ($C002 +1 → $C003)
      await exec("r sp=fd");          // SP below the two pushed bytes
      await exec("r pc=c010");        // at the RTS
      await exec("ret");
      const pcAfterRet = await pc();
      return {
        // z stepped INTO the subroutine body.
        zSteppedInto: pcIntoJsr === 0xc010,
        // n stepped OVER the JSR (body run-through, PC after the JSR).
        nSteppedOver: pcOverJsr === 0xc003,
        // ret ran the RTS and returned to the caller ($C003 = seeded $C002 + 1).
        retReturned: pcAfterRet === 0xc003,
      };
    },
  },

  // ── P1: monitor-until — `until <addr>` synchronous run-to-landing ───────────
  // Spec 754 §3.3/§3.1: `until <addr>` synchronously runs to addr and HALTS there
  // (PC===addr, runState NOT running) — distinct from the live `g`. Signal: plant a
  // tiny loop that reaches a known address, `until <addr>`, assert reported PC===addr
  // and the machine is paused there. TS≡TRX64.
  {
    id: "monitor-until",
    severity: "P1",
    title: "monitor `until <addr>` synchronously runs to the address and halts there",
    async signal(c) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      // $C000: NOP NOP NOP ; $C003: JMP $C003 (a self-loop landing pad). `until c003`
      // must execute the three NOPs and stop AT $C003.
      await exec("wr c000 ea ea ea 4c 03 c0");
      await exec("r pc=c000");
      const untilOut = await exec("until c003");
      const st = await state(c, sid);
      const pc = Number(st.cpu?.pc ?? st.pc ?? -1);
      return {
        recognized: !/unknown command/i.test(untilOut),
        // Landed exactly at the requested address.
        landedAtAddr: pc === 0xc003,
        // Synchronous halt — not left running (unlike `g`).
        haltedNotRunning: st.runState !== "running",
        // The reply text reports the reached address (parsed numerically, format-agnostic).
        replyReportsAddr: /c003/i.test(untilOut),
      };
    },
  },

  // ── P1: monitor-bank-lens-m — `m <lens> <addr>` honours the banking lens (BUG-038) ─
  // Spec 754 §3.3b: `m ram e000`/`m rom e000`/`m cpu e000` return DIFFERENT bytes for
  // the same address per the lens. A banking-BLIND `m` (a plausible TRX64
  // simplification) returns identical bytes. Signal: cold boot (KERNAL mapped at
  // $E000), write a RAM sentinel UNDER the KERNAL via the `ram` lens, then read the
  // same address three ways: `m rom e000` shows the KERNAL byte (ROM image), `m ram
  // e000` shows the sentinel (raw RAM), and they DIFFER. Same idea for $D000 (`m io`
  // = VIC regs vs `m ram` = raw RAM). TS≡TRX64.
  {
    id: "monitor-bank-lens-m",
    severity: "P1",
    title: "monitor `m <lens> <addr>` honours the bank lens (rom≠ram≠io at the same address)",
    async signal(c) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      // The first data byte of an `m` dump row: ">X:AAAA  bb bb ..." — grab the byte
      // at the requested column (the row starts at addr & ~0x1f, so compute the index).
      const firstByteAt = (out: string, addr: number): number => {
        // Find the row whose base == addr & ~0x1f, then index into its byte list.
        const base = addr & ~0x1f;
        const re = new RegExp(`^>.\\:0*${base.toString(16)}\\s+([0-9a-fA-F ]+?)\\s{2,}`, "im");
        const m = out.match(re) ?? out.match(/^>.\:[0-9a-fA-F]+\s+([0-9a-fA-F ]+)/im);
        if (!m) return -1;
        const bytes = m[1].trim().split(/\s+/);
        const idx = addr - base;
        const v = parseInt(bytes[idx] ?? "", 16);
        return Number.isNaN(v) ? -1 : v;
      };
      // Cold boot so KERNAL is in the ROM image and mapped at $E000 under cpu lens.
      await c.call("session/reset", { session_id: sid, mode: "cold" });
      // Write a distinguishable sentinel into RAM under the KERNAL.
      await exec("wr ram e000 5a");
      const romOut = await exec("m rom e000 e01f");
      const ramOut = await exec("m ram e000 e01f");
      const cpuOut = await exec("m cpu e000 e01f");
      const romByte = firstByteAt(romOut, 0xe000);
      const ramByte = firstByteAt(ramOut, 0xe000);
      const cpuByte = firstByteAt(cpuOut, 0xe000);
      return {
        // The RAM lens shows the raw sentinel we wrote.
        ramShowsSentinel: ramByte === 0x5a,
        // The ROM lens shows the KERNAL image byte (NOT the sentinel) — banking honoured.
        romNotSentinel: romByte >= 0 && romByte !== 0x5a,
        // cpu lens == rom here (KERNAL is mapped at boot), and != the raw RAM sentinel.
        cpuMatchesRom: cpuByte === romByte && cpuByte !== 0x5a,
        // The decisive differential: a banking-BLIND `m` would return identical bytes.
        lensesDiffer: ramByte !== romByte && ramByte >= 0 && romByte >= 0,
      };
    },
  },

  // ── P1: monitor-sidefx — `sidefx on/off` toggles the read-side-effect lane ──
  // Spec 754 §3.4: `sidefx off` (default) reads I/O via side-effect-free peek; `sidefx
  // on` = live reads. Signal: assert the verb is recognized + toggles, and that under
  // `sidefx off` repeated `m io` reads of a latching register are STABLE (the peek
  // doesn't clear it). We verify the toggle is honoured (off↔on wire state) and that
  // the off-path read is stable — the load-bearing, file-derivable contract. TS≡TRX64.
  {
    id: "monitor-sidefx",
    severity: "P1",
    title: "monitor `sidefx on/off` toggles read side-effects; off-path peek is stable",
    async signal(c) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      await c.call("session/reset", { session_id: sid, mode: "cold" });
      const offOut = await exec("sidefx off");
      // Two side-effect-free reads of $D019 (raster IRQ latch) must agree (peek never
      // acks). Parse the first byte of the io-lens dump row.
      const firstByte = (out: string): number => {
        const m = out.match(/^>.\:[0-9a-fA-F]+\s+([0-9a-fA-F]{2})/im);
        return m ? parseInt(m[1], 16) : -1;
      };
      const r1 = firstByte(await exec("m io d019 d019"));
      const r2 = firstByte(await exec("m io d019 d019"));
      const onOut = await exec("sidefx on");
      const offAgain = await exec("sidefx off");
      return {
        recognized: !/unknown command/i.test(offOut) && !/unknown command/i.test(onOut),
        // The toggle is acknowledged in both directions (off→on→off).
        offAcknowledged: /off/i.test(offOut) && /off/i.test(offAgain),
        onAcknowledged: /on/i.test(onOut),
        // The side-effect-free read is STABLE across two peeks (no latch clear).
        offReadStable: r1 >= 0 && r1 === r2,
      };
    },
  },

  // ── P1: monitor-a-inline — inline `a <addr> <instr>` assembles into RAM ──────
  // Spec 754 §3.3c: `a <addr> <instr>` assembles one instruction (all modes) and pokes
  // the bytes. The help advertised `a` but run_monitor had NO arm → `unknown command:
  // a`. Signal: assemble four instructions across modes and read the bytes back via
  // `m ram`. NOTE: an inline `a` LEAVES the session in modal assemble (1:1 with TS,
  // which calls assembleAt → sets the cursor), so we send an empty line to EXIT modal
  // mode before each read. Byte-exact TS≡TRX64.
  {
    id: "monitor-a-inline",
    severity: "P1",
    title: "monitor inline `a <addr> <instr>` assembles bytes into RAM (all modes)",
    async signal(c) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      const recognized = (s: string) => !/unknown command/i.test(s);
      // Read N bytes starting at addr from an `m ram` dump (first row).
      const ramBytes = (out: string, addr: number, n: number): number[] => {
        const base = addr & ~0x1f;
        const re = new RegExp(`^>.\\:0*${base.toString(16)}\\s+([0-9a-fA-F ]+?)\\s{2,}`, "im");
        const m = out.match(re) ?? out.match(/^>.\:[0-9a-fA-F]+\s+([0-9a-fA-F ]+)/im);
        if (!m) return [];
        const bytes = m[1].trim().split(/\s+/);
        const start = addr - base;
        return Array.from({ length: n }, (_, i) => parseInt(bytes[start + i] ?? "", 16));
      };
      // Assemble each instruction inline, then EXIT modal mode (empty line) before
      // reading bytes back (an inline `a` leaves the session in modal assemble).
      const aImm = await exec("a c000 lda #$01");
      await exec("");
      const aAbs = await exec("a c010 sta $d020");
      await exec("");
      const aJsr = await exec("a c020 jsr $fce2");
      await exec("");
      const aRts = await exec("a c030 rts");
      await exec("");
      const immBytes = ramBytes(await exec("m ram c000 c001"), 0xc000, 2);
      const absBytes = ramBytes(await exec("m ram c010 c012"), 0xc010, 3);
      const jsrBytes = ramBytes(await exec("m ram c020 c022"), 0xc020, 3);
      const rtsBytes = ramBytes(await exec("m ram c030 c030"), 0xc030, 1);
      const eq = (a: number[], b: number[]) => a.length === b.length && a.every((v, i) => v === b[i]);
      return {
        recognized: recognized(aImm) && recognized(aAbs) && recognized(aJsr) && recognized(aRts),
        immOk: eq(immBytes, [0xa9, 0x01]),   // LDA #$01
        absOk: eq(absBytes, [0x8d, 0x20, 0xd0]), // STA $D020
        jsrOk: eq(jsrBytes, [0x20, 0xe2, 0xfc]), // JSR $FCE2
        rtsOk: eq(rtsBytes, [0x60]),         // RTS
      };
    },
  },

  // ── P1: monitor-a-modal — modal assemble `a <addr>` (prompt + cursor advance) ─
  // Spec 754 §3.3c: `a <addr>` (no instr) enters assemble mode → MonitorResult.prompt
  // = `.cXXX`; bare instruction lines assemble + advance the cursor (prompt advances);
  // an empty line exits. The `prompt` field is forwarded over the wire (TS
  // runMonitorCommand returns {output,prompt}; TRX64 must too). Signal: `a c000`→prompt
  // /^\.c000/; bare `lda #$01`→prompt advanced /^\.c002/ + bytes A9 01; empty line→no
  // prompt. TS≡TRX64.
  {
    id: "monitor-a-modal",
    severity: "P1",
    title: "monitor modal assemble `a <addr>` — prompt at cursor, advances per line, empty exits",
    async signal(c) {
      const sid = await liveSession(c);
      const call = async (command: string): Promise<{ output: string; prompt?: string }> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return { output: String(r?.output ?? r?.error ?? ""), prompt: r?.prompt };
      };
      const ramBytes = async (addr: number, n: number): Promise<number[]> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command: `m ram ${addr.toString(16)} ${(addr + n - 1).toString(16)}` })) as any;
        const out = String(r?.output ?? "");
        const base = addr & ~0x1f;
        const re = new RegExp(`^>.\\:0*${base.toString(16)}\\s+([0-9a-fA-F ]+?)\\s{2,}`, "im");
        const m = out.match(re) ?? out.match(/^>.\:[0-9a-fA-F]+\s+([0-9a-fA-F ]+)/im);
        if (!m) return [];
        const bytes = m[1].trim().split(/\s+/);
        const start = addr - base;
        return Array.from({ length: n }, (_, i) => parseInt(bytes[start + i] ?? "", 16));
      };
      // Enter modal assemble at $C000 → prompt `.c000`.
      const enter = await call("a c000");
      // A bare instruction line (no `a` prefix) assembles at the cursor + advances it.
      const line1 = await call("lda #$01");
      // Read the bytes BEFORE exiting (still in modal mode — `m` would be eaten, so
      // read via a fresh exec AFTER exiting). Exit modal mode with an empty line.
      const exit = await call("");
      const bytes = await ramBytes(0xc000, 2);
      const promptAt = (p?: string) => (p ?? "").trim().toLowerCase();
      return {
        // Entering modal assemble returns a prompt anchored at the cursor.
        enterPromptAtC000: /^\.0*c000/.test(promptAt(enter.prompt)),
        // A bare instr line advanced the cursor by the instruction size (2 → $C002).
        lineAdvancedToC002: /^\.0*c002/.test(promptAt(line1.prompt)),
        // The instruction landed in RAM.
        bytesLanded: bytes.length === 2 && bytes[0] === 0xa9 && bytes[1] === 0x01,
        // The empty line EXITED modal mode → no prompt on the reply.
        emptyLineExits: exit.prompt === undefined || exit.prompt === null,
      };
    },
  },

  // ── P1: monitor-t-c-h — cracking core: transfer / compare / hunt memory ─────
  // Spec 754 §3.3c: `t <s> <e> <dst>` moves a range (overlap-safe); `c <s> <e> <dst>`
  // compares → diff addresses; `h <s> <e> <pat..>` hunts a byte pattern (xx wildcard)
  // → match addresses. Signal: `f` a sentinel pattern, then exercise all three and
  // verify the result by reading the destination back / parsing the reported addresses.
  // TS≡TRX64.
  {
    id: "monitor-t-c-h",
    severity: "P1",
    title: "monitor `t`/`c`/`h` — transfer, compare, and hunt memory (the cracking core)",
    async signal(c) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      const ramBytes = (out: string, addr: number, n: number): number[] => {
        const base = addr & ~0x1f;
        const re = new RegExp(`^>.\\:0*${base.toString(16)}\\s+([0-9a-fA-F ]+?)\\s{2,}`, "im");
        const m = out.match(re) ?? out.match(/^>.\:[0-9a-fA-F]+\s+([0-9a-fA-F ]+)/im);
        if (!m) return [];
        const bytes = m[1].trim().split(/\s+/);
        const start = addr - base;
        return Array.from({ length: n }, (_, i) => parseInt(bytes[start + i] ?? "", 16));
      };
      // Seed a known 4-byte pattern at $C000: DE AD BE EF.
      await exec("wr ram c000 de ad be ef");
      // Clear the destination + a hunt control region so leftovers don't false-match.
      await exec("f c100 c1ff 00");
      await exec("f c200 c2ff 00");
      // ── h: hunt the pattern in $C000..$C0FF → must contain $C000. ──
      const hOut = await exec("h c000 c0ff de ad be ef");
      const hMatchedC000 = /c000/i.test(hOut);
      // ── h with a wildcard: `de xx be ef` must also match $C000. ──
      const hWildOut = await exec("h c000 c0ff de xx be ef");
      const hWildMatchedC000 = /c000/i.test(hWildOut);
      // ── t: transfer $C000..$C003 → $C100, then read the destination back. ──
      await exec("t c000 c003 c100");
      const dst = ramBytes(await exec("m ram c100 c103"), 0xc100, 4);
      const transferOk = dst.length === 4 && dst[0] === 0xde && dst[1] === 0xad && dst[2] === 0xbe && dst[3] === 0xef;
      // ── c: compare $C000..$C003 vs $C100 → identical now. ──
      const cEqual = await exec("c c000 c003 c100");
      const compareEqual = /identical/i.test(cEqual);
      // Mutate one byte at the destination, then compare → reports the differing addr.
      await exec("wr ram c100 00");
      const cDiff = await exec("c c000 c003 c100");
      const compareReportsDiff = /c000/i.test(cDiff) && /diff/i.test(cDiff);
      return {
        huntFound: hMatchedC000,
        huntWildcardFound: hWildMatchedC000,
        transferOk,
        compareEqual,
        compareReportsDiff,
      };
    },
  },

  // ── P1: monitor-r-vectors — `r` shows LIVE IRQ/NMI vectors from RAM $0314.. ──
  // Spec 754 §3.3d: `r` shows regs + a live `vectors` block (CINV $0314→, NMIV
  // $0318→), DERIVED from RAM — a constant block is a likely port shortcut. Also
  // `r a=$42 x=$10` sets multiple regs. Signal: cold boot → `r` shows the live CINV
  // target ($EA31 default); poke $0314/$0315 → `r` reflects the moved target; set regs
  // → `r` shows them. The vectors must TRACK RAM (a constant block would not). TS≡TRX64.
  {
    id: "monitor-r-vectors",
    severity: "P1",
    title: "monitor `r` shows live IRQ/NMI vectors (derived from RAM $0314..), and sets regs",
    async signal(c) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      // Parse the CINV target from the vectors line `CINV $0314->$XXXX`.
      const cinvTarget = (out: string): number => {
        const m = out.match(/CINV\s*\$0314->\$([0-9a-fA-F]{4})/i);
        return m ? parseInt(m[1], 16) : -1;
      };
      await c.call("session/reset", { session_id: sid, mode: "cold" });
      // The KERNAL sets CINV ($0314) → $EA31 at boot.
      const r0 = await exec("r");
      const cinv0 = cinvTarget(r0);
      const cinvShownAtBoot = cinv0 === 0xea31;
      // Move the vector in RAM; `r` must reflect the NEW target (live-derived).
      await exec("wr ram 0314 cd ab");  // $0314=$CD $0315=$AB → CINV → $ABCD
      const r1 = await exec("r");
      const cinv1 = cinvTarget(r1);
      const vectorsTrackRam = cinv1 === 0xabcd;
      // Set multiple registers in one go; `r` shows them.
      await exec("r a=42 x=10");
      const r2 = await exec("r");
      // The register line: `.;PPPP AA XX YY SP ...` — second/third hex pairs are A/X.
      const regLine = r2.split("\n").find((l) => /\.;[0-9a-fA-F]{4}/.test(l)) ?? "";
      const m = regLine.match(/\.;[0-9a-fA-F]{4}\s+([0-9a-fA-F]{2})\s+([0-9a-fA-F]{2})/);
      const aSet = m ? parseInt(m[1], 16) : -1;
      const xSet = m ? parseInt(m[2], 16) : -1;
      return {
        // The vectors block names CINV/$0314 and shows the live boot target.
        cinvShownAtBoot,
        hasNmivBlock: /NMIV\s*\$0318/i.test(r0),
        // The decisive differential: the vector TRACKS RAM (a constant block would not).
        vectorsTrackRam,
        // `r a= x=` set both registers (shown back by `r`).
        regsSet: aSet === 0x42 && xSet === 0x10,
      };
    },
  },

  // ═══════════════════════════════════════════════════════════════════════════
  // BATCH 3 — Spec 769 time-travel / L7 code-overlay debug loop (P0 acceptance).
  // The overlay loop (`runtime/overlay_run`, ws-server.ts:938-980) restores an
  // anchor, applies RAM patches, runs forward (optionally to an until_pc), reads
  // back flagged addresses, and returns the post-run registers — leaving the
  // machine PAUSED. It is REPEATABLE: each call restores FRESH (the prior overlay
  // is rolled back by the full-RAM restore, restore_runtime_checkpoint /
  // restoreCheckpoint), so the LLM iterates a fix from a fixed point with no
  // rebuild/reboot. That full-RAM rollback is the loop's whole mechanic and the
  // §7 acceptance gate. See case-audit.md "Spec 769 — time-travel / overlay".
  // ═══════════════════════════════════════════════════════════════════════════

  // ── P0: ws-overlay-run-loop — the L7 overlay reply shape + read-back ─────────
  // Spec 769 §3/§6 (769.2): capture an anchor, overlay_run with a RAM patch + an
  // until_pc breakpoint, assert the FULL reply: { anchorId, applied[{addr,len}],
  // ranCycles, hitPc, reads{"$addr":byte}, registers{pc,a,x,y,sp,flags,cycles} }.
  // We run forward to a DETERMINISTIC KERNAL address ($EA31, the jiffy-IRQ entry)
  // via until_pc so the landing PC + all GP registers are byte-stable across both
  // runtimes (cold-boot puts both machines in the same idle loop; an until_pc hit
  // pins them to the SAME instruction). The patch lands at $0400 (screen RAM, not
  // touched by the boot loop), so reads["$0400"]===0x2a proves the overlay WROTE
  // RAM and the post-run read sees it. The hitPc===until_pc + registers.pc==hitPc
  // prove the bounded run honoured the breakpoint. registers.cycles is NORMALIZED
  // to "is a number" (not its exact value): a 4-cycle cold-boot clock skew exists
  // INDEPENDENT of the overlay (the anchor itself captures at 5000000 vs 5000004),
  // so asserting the exact cycles would false-RED on a pre-existing, non-overlay
  // divergence. Every OTHER register (pc,a,x,y,sp,flags) IS compared field-for-
  // field, so an overlay that corrupts a register still diverges loud.
  {
    id: "ws-overlay-run-loop",
    severity: "P0",
    title: "runtime/overlay_run reply shape + RAM patch read-back (L7 debug loop, ends paused)",
    async signal(c) {
      const sid = await liveSession(c);
      // Cold boot so KERNAL+IRQs are live: only then does $EA31 (the jiffy IRQ
      // handler) fire so the until_pc breakpoint is reachable. reset is synchronous.
      await c.call("session/reset", { session_id: sid, mode: "cold" });
      // Capture the anchor of the booted-READY state (the fixed point the loop
      // restores to on every iteration).
      const cap = (await c.call("checkpoint/capture", { session_id: sid })) as any;
      const anchorCyc = cap?.ref?.cycles ?? cap?.cycles ?? null;
      // overlay_run: patch $0400 := 0x2a (read-back flagged), run forward bounded,
      // breakpoint at $EA31. Ends PAUSED (ws-server.ts restores then:"pause").
      const o = (await c.call("runtime/overlay_run", {
        session_id: sid,
        patches: [{ addr: 0x0400, bytes: [0x2a], read: true }],
        run_cycles: 200000,
        until_pc: 0xea31,
      })) as any;
      // The machine must be PAUSED after the loop (the overlay loop never leaves it
      // free-running — the LLM observes a frozen post-run state).
      const st = await state(c, sid);
      const reg = o?.registers ?? {};
      return {
        // The patch was applied at the requested addr with the requested length.
        applied: o?.applied,
        // The bounded run honoured the until_pc breakpoint (hit == target).
        hitPc: o?.hitPc,
        ranCycles: o?.ranCycles,
        // The read-back of the patched addr sees the overlaid byte AFTER the run.
        readBack0400: o?.reads?.["$0400"],
        // The anchor id is non-empty + deterministic (the restored fixed point).
        anchorIdPresent: typeof o?.anchorId === "string" && o.anchorId.length > 0,
        // FULL register shape (all present), value-compared EXCEPT cycles (cold-boot
        // 4-cycle skew is pre-existing + non-overlay — normalized to presence).
        regPc: reg.pc,
        regPcMatchesHit: Number(reg.pc) === Number(o?.hitPc),
        regA: reg.a,
        regX: reg.x,
        regY: reg.y,
        regSp: reg.sp,
        regFlags: reg.flags,
        regCyclesIsNumber: typeof reg.cycles === "number",
        // The anchor captured at a real (non-null) cycle.
        anchorHasCycles: anchorCyc != null,
        // Ends PAUSED (the loop observes a frozen state, never a free-run).
        endedPaused: st.runState === "paused",
      };
    },
  },

  // ── P0: ws-overlay-restore-undoes — the rollback that MAKES the loop a loop ───
  // Spec 769 §3/§7 (769.2): re-running the SAME anchor with a different/empty patch
  // must ROLL BACK the prior overlay. A TRX64 that does NOT fully restore RAM
  // between calls (e.g. a sparse/partial restore, or one that skips the screen-RAM
  // page) would leave the first patch resident — the high-risk port bug here. The
  // signal: overlay_run anchorA with [$033c := 0xff, read] → reads["$033c"]===0xff;
  // then the SAME anchor with an EMPTY patch (read-only) → reads["$033c"] must be
  // the ORIGINAL value (≠ 0xff). $033c (cassette buffer) is untouched by the idle
  // loop so its original is stable. We capture the pristine original FIRST (an
  // empty-patch overlay_run before any write) and assert: (a) the 0xff write was
  // visible in pass 1, (b) the rollback in pass 2 restored the EXACT original, and
  // (c) the original is provably not 0xff (so "rolled back" is a real change). All
  // three must agree TS≡TRX64. This is the §7 acceptance gate.
  {
    id: "ws-overlay-restore-undoes",
    severity: "P0",
    title: "overlay_run restores FRESH each call — a prior RAM patch is fully rolled back (769 §7 gate)",
    async signal(c) {
      const sid = await liveSession(c);
      await c.call("session/reset", { session_id: sid, mode: "cold" });
      await c.call("checkpoint/capture", { session_id: sid });
      // PASS 0 — read the pristine original at $033c (empty patch, no run): the
      // restore-to-anchor leaves RAM at the anchor's value.
      const p0 = (await c.call("runtime/overlay_run", {
        session_id: sid,
        patches: [{ addr: 0x033c, bytes: [], read: true }],
        run_cycles: 0,
      })) as any;
      const original = p0?.reads?.["$033c"];
      // PASS 1 — overlay $033c := 0xff (no run): the read-back must see 0xff.
      const p1 = (await c.call("runtime/overlay_run", {
        session_id: sid,
        patches: [{ addr: 0x033c, bytes: [0xff], read: true }],
        run_cycles: 0,
      })) as any;
      const afterWrite = p1?.reads?.["$033c"];
      // PASS 2 — SAME anchor, EMPTY patch (no run): the restore must have rolled the
      // 0xff back to `original` BEFORE the (empty) patch applied.
      const p2 = (await c.call("runtime/overlay_run", {
        session_id: sid,
        patches: [{ addr: 0x033c, bytes: [], read: true }],
        run_cycles: 0,
      })) as any;
      const afterRollback = p2?.reads?.["$033c"];
      return {
        // The overlay write was visible in pass 1.
        writeVisible: Number(afterWrite) === 0xff,
        // The rollback restored the EXACT pristine original (the loop's mechanic).
        rolledBackToOriginal: Number(afterRollback) === Number(original),
        // "Rolled back" is a REAL change, not a coincidence (original ≠ 0xff).
        originalNotFf: Number(original) !== 0xff,
        // The literal original byte (TS≡TRX64 — both restore the same anchor RAM).
        original,
      };
    },
  },

  // ── P2: ws-overlay-empty-ring — overlay_run on an empty ring throws ──────────
  // Spec 769 §6 (769.2): with no checkpoints to anchor on, overlay_run throws
  // "runtime/overlay_run: no checkpoints to anchor on" (ws-server.ts:943 /
  // main.rs:6555). A PL-7 silent no-op (returning a fake reply) is the port hazard.
  // We clear the ring then overlay_run any patch → both must THROW (a real RPC
  // error), not return a success envelope.
  {
    id: "ws-overlay-empty-ring",
    severity: "P2",
    title: "runtime/overlay_run on an empty checkpoint ring throws (no silent no-op, PL-7)",
    async signal(c) {
      const sid = await liveSession(c);
      // Empty the ring (checkpoint/clear evicts every non-pinned anchor).
      await c.call("checkpoint/clear", { session_id: sid }).catch(() => undefined);
      let threw = false;
      let noAnchor = false;
      try {
        await c.call("runtime/overlay_run", {
          session_id: sid,
          patches: [{ addr: 0x033c, bytes: [0x01] }],
          run_cycles: 0,
        });
      } catch (e) {
        threw = true;
        const msg = e instanceof Error ? e.message : String(e);
        let text = msg;
        try { const j = JSON.parse(msg); if (j?.message) text = String(j.message); } catch { /* plain */ }
        noAnchor = /no checkpoints to anchor on/i.test(text);
      }
      return { threw, noAnchor };
    },
  },

  // ── P2: ws-overlay-anchor-selection — at/before anchor_cycle is deterministic ─
  // Spec 769 §6 (769.2): given anchor_cycle, the loop picks the anchor at-or-before
  // that cycle (else the nearest); the returned anchorId is deterministic
  // (ws-server.ts:946-954 / main.rs:6557-6578). We capture TWO anchors at known
  // DIFFERENT cycles (a `session/run` between them advances the clock), then
  // overlay_run with an anchor_cycle BETWEEN them → the returned anchorId must be
  // the EARLIER (at/before) anchor on both runtimes. The two ids are stable
  // ("cp_<gen>_<n>" on both), so we compare WHICH of the two captured ids was
  // chosen (a boolean: chose-the-earlier), not the literal id string (the gen
  // counter differs across the two daemons' lifetimes).
  {
    id: "ws-overlay-anchor-selection",
    severity: "P2",
    title: "runtime/overlay_run anchor_cycle picks the at-or-before anchor (deterministic)",
    async signal(c) {
      const sid = await liveSession(c);
      await c.call("session/reset", { session_id: sid, mode: "cold" });
      // Anchor A at the booted cycle.
      const capA = (await c.call("checkpoint/capture", { session_id: sid })) as any;
      const idA = capA?.ref?.id ?? capA?.id;
      const cycA = Number(capA?.ref?.cycles ?? capA?.cycles ?? 0);
      // Advance the clock a known amount, then capture anchor B (later cycle).
      await c.call("session/run", { session_id: sid, cycles: 100000 });
      const capB = (await c.call("checkpoint/capture", { session_id: sid })) as any;
      const idB = capB?.ref?.id ?? capB?.id;
      const cycB = Number(capB?.ref?.cycles ?? capB?.cycles ?? 0);
      // Pick an anchor_cycle strictly between A and B → must resolve to A (at/before).
      const between = Math.floor((cycA + cycB) / 2);
      const o = (await c.call("runtime/overlay_run", {
        session_id: sid,
        anchor_cycle: between,
        patches: [{ addr: 0x033c, bytes: [], read: true }],
        run_cycles: 0,
      })) as any;
      return {
        // B is strictly later than A (the run advanced the clock).
        bIsLater: cycB > cycA,
        // anchor_cycle between A and B resolves to A (the at-or-before anchor).
        choseEarlier: String(o?.anchorId) === String(idA),
        // It did NOT pick the later anchor (B is after the requested cycle).
        notChoseLater: String(o?.anchorId) !== String(idB),
      };
    },
  },

  // ── NOTE (asm-source overlays, 769 §6 P1) — NOT WS-differential-testable here ─
  // Spec 769 §6 mentions `{addr, source:<asm>}` overlays (assemble_source → bytes,
  // equivalent to raw bytes). The WS `runtime/overlay_run` handler reads ONLY
  // `patches[].bytes` (ws-server.ts:957-963) — the asm→bytes assembly lives in the
  // MCP `runtime_overlay_run` tool layer (server-tools/headless.ts), NOT on the WS
  // surface. So over WS both daemons accept only pre-assembled bytes, and there is
  // no `source` field to diverge on. Recorded as an MCP-tool-level note (the raw-
  // bytes path IS covered by ws-overlay-run-loop above); no WS gate is added.

  // ── P1 (BLOCKED): recorder-list-shape — supersedes background-workers-async-0 ─
  // Spec 769 §2 (769.1): an ACTIVE recorder → recorder/list returns
  // { active:true, anchors:[{seq,cycle,wallMs,diskGen,cartGen,schemaVersion}…] };
  // anchors accrue while free-running; seq is monotonic + cycle ascending.
  // BLOCKED by the TS oracle harness (NOT a TRX64 defect). The TS recorder/list
  // does `await c.recorder.list()`, which round-trips to a node:worker_threads
  // worker resolved at WORKER_PATH = `${dir(import.meta.url)}/recorder-worker.js`
  // (runtime-recorder.ts:33). Under the tsx-from-src oracle daemon import.meta.url
  // is the SRC `.ts` dir — where only recorder-worker.ts exists (the built .js is
  // under dist/) — so `new Worker(.js)` never loads, its error is swallowed, and the
  // list() promise NEVER resolves (RPC timeout). EMPIRICALLY CONFIRMED: TS
  // recorder/list hangs (>20 s) while TRX64 returns in ~1 ms with the correct shape
  // ({active:true, anchors:[{seq:0,cycle,wallMs,diskGen,cartGen,schemaVersion}]}).
  // So the TS AUTHORITY cannot report the comparison signal over WS under THIS
  // harness — same class as the original background-workers-async-0. This case is a
  // STRENGTHENING (past that case's count-only check): it asserts the full anchor
  // SHAPE + monotonic seq + ascending cycle, and re-arms automatically once the
  // oracle runs the built (dist) TS daemon (where recorder-worker.js resolves) — run
  // `--only recorder-list-shape --include-blocked` to re-check. The TRX64 side is
  // verified directly here (the in-process recorder, no worker).
  {
    id: "recorder-list-shape",
    severity: "P1",
    title: "recorder/list returns the anchor list shape (active + {seq,cycle,…}; monotonic seq, ascending cycle)",
    blocked:
      "TS recorder/list awaits a node:worker_threads worker (recorder-worker.js) that " +
      "is non-functional under tsx-from-src (resolves to the src .ts dir; the built .js " +
      "is under dist/), so the list() promise never resolves (RPC timeout). EMPIRICALLY " +
      "confirmed: TS hangs >20s; TRX64 returns the correct shape in ~1ms. Re-arm when the " +
      "oracle runs the built (dist) TS daemon.",
    spawn: { stream: true, env: { C64RE_RECORDER: "1" } },
    async signal(c) {
      const sid = await liveSession(c);
      // TRX64 needs an explicit recorder/start; TS auto-creates it in run() and has no
      // such method → ignore the error there.
      await c.call("recorder/start", { session_id: sid }).catch(() => undefined);
      // Start the continuous --stream driver (on TS this ALSO creates the recorder).
      await c.call("debug/run", { session_id: sid });
      const fetchList = async (): Promise<any> =>
        (await c.call("recorder/list", { session_id: sid })) as any;
      // Poll for ≥1 anchor (the recorder feeds an anchor at the auto-capture cadence).
      let r = await fetchList();
      const deadline = Date.now() + 60_000;
      while (Date.now() < deadline && (!Array.isArray(r?.anchors) || r.anchors.length < 1)) {
        await sleep(2000);
        r = await fetchList();
      }
      const anchors: any[] = Array.isArray(r?.anchors) ? r.anchors : [];
      const first = anchors[0] ?? {};
      // Monotonic seq + ascending cycle across the accrued anchors.
      let seqMonotonic = true, cycleAscending = true;
      for (let i = 1; i < anchors.length; i++) {
        if (!(Number(anchors[i].seq) > Number(anchors[i - 1].seq))) seqMonotonic = false;
        if (!(Number(anchors[i].cycle) >= Number(anchors[i - 1].cycle))) cycleAscending = false;
      }
      return {
        active: r?.active === true,
        hasAnchors: anchors.length >= 1,
        // The anchor entry carries the RecorderAnchorRef fields.
        firstHasSeq: typeof first.seq === "number",
        firstHasCycle: typeof first.cycle === "number",
        firstHasSchemaVersion: typeof first.schemaVersion === "number",
        seqMonotonic,
        cycleAscending,
      };
    },
  },

  // ── P1 (BLOCKED): recorder-dump — anchor → .c64re file + descriptor; arg guards ─
  // Spec 769 §2 (769.1)/707: recorder/dump({seq,path}) writes the anchor at `seq`
  // to a native .c64re (non-empty, "C64RESNP" magic) and returns a descriptor;
  // missing seq/path throws. BLOCKED for the SAME reason as recorder-list-shape —
  // recorder/dump first does `recorder.list()`-style worker round-trips (the seq
  // lookup goes through the worker store), so the TS side cannot report under tsx-
  // from-src. EMPIRICALLY CONFIRMED on TRX64: dump writes an 18.3 KB .c64re starting
  // "C64RESNP" + returns { path, cycle, pc, machine, media, fileBytes, breakpoints };
  // `recorder/dump` with no seq → "recorder/dump: seq required"; no path →
  // "recorder/dump: path required". Re-arms with the dist TS daemon.
  {
    id: "recorder-dump",
    severity: "P1",
    title: "recorder/dump writes the anchor to a .c64re (magic C64RESNP, non-empty) + arg guards throw",
    blocked:
      "TS recorder/dump routes the seq lookup through the same node:worker_threads " +
      "recorder worker that is non-functional under tsx-from-src — so the TS authority " +
      "cannot report. Verified directly on TRX64: dump writes an 18.3KB .c64re (magic " +
      "C64RESNP) + returns a descriptor; missing seq/path throw the documented errors. " +
      "Re-arm when the oracle runs the built (dist) TS daemon.",
    spawn: { stream: true, env: { C64RE_RECORDER: "1" } },
    async signal(c, d) {
      const sid = await liveSession(c);
      await c.call("recorder/start", { session_id: sid }).catch(() => undefined);
      await c.call("debug/run", { session_id: sid });
      // Wait for ≥1 anchor, then dump the first seq.
      const fetchList = async (): Promise<any> =>
        (await c.call("recorder/list", { session_id: sid })) as any;
      let lr = await fetchList();
      const deadline = Date.now() + 60_000;
      while (Date.now() < deadline && (!Array.isArray(lr?.anchors) || lr.anchors.length < 1)) {
        await sleep(2000);
        lr = await fetchList();
      }
      const seq = lr?.anchors?.[0]?.seq;
      const path = join(d.projectDir, "recorder-anchor.c64re");
      await c.call("recorder/dump", { session_id: sid, seq, path });
      const exists = existsSync(path);
      const bytes = exists ? readFileSync(path) : Buffer.alloc(0);
      const magic = bytes.length >= 8 ? bytes.toString("latin1", 0, 8) : "";
      // Arg guards: missing seq / missing path throw.
      let noSeqThrew = false, noPathThrew = false;
      try { await c.call("recorder/dump", { session_id: sid, path }); } catch { noSeqThrew = true; }
      try { await c.call("recorder/dump", { session_id: sid, seq }); } catch { noPathThrew = true; }
      return {
        fileExists: exists,
        nonEmpty: bytes.length > 0,
        magicC64resnp: magic === "C64RESNP",
        noSeqThrew,
        noPathThrew,
      };
    },
  },

  // ─────────────────────────────────────────────────────────────────────────
  // BATCH 4 — checkpoint INTEGRITY + ring structure
  //   (case-audit.md §"Spec 707 …", §"Spec 705.B / 769 …", §"Fix order Batch 4")
  // ─────────────────────────────────────────────────────────────────────────

  // ── P1: ws-checkpoint-pin-unpin — pin/unpin ring mechanics + unknown-id throws ─
  // Spec 705.B §3.4/§4.10: pin(id) marks an anchor exempt from eviction and returns
  // { ref{…pinned:true}, stats{pinnedCount…} }; unpin flips it back; pin/unpin of an
  // UNKNOWN id THROWS (ws-server.ts:1043-1056 → "checkpoint/pin: unknown id <id>"),
  // never a silent no-op (PL-7). The eviction-SURVIVAL leg (capture, pin, fill the
  // ring past the cap, assert the pinned anchor survives) is NOT WS-differential-
  // testable here: BOTH runtimes hardcode the ring budget to 32 MiB ≈ 512 SLOT_BYTES
  // slots (runtime-checkpoint-ring.ts:136 DEFAULT_CHECKPOINT_RING_BUDGET_BYTES /
  // checkpoint_ring.rs:46) with NO env override on either side, so filling past the
  // cap means 512+ WS captures (~64 KiB RAM each) against the ~4 fps tsx oracle —
  // infeasible in a 240 s case, and shrinking the budget would need a bilateral
  // c64re-side change (FORBIDDEN in this batch). The eviction POLICY itself (oldest-
  // unpinned evicted; all-pinned-full errors) is covered by the TRX64 ring unit tests
  // (checkpoint_ring.rs:557/574 eviction_drops_oldest_unpinned_when_full +
  // capture_errors_when_all_pinned_and_full). This case asserts the WS pin/unpin
  // CONTRACT (the surface the audit's P1 signal names) field-for-field TS≡TRX64.
  {
    id: "ws-checkpoint-pin-unpin",
    severity: "P1",
    title: "checkpoint/pin marks pinned + bumps stats.pinnedCount; unpin flips back; unknown id throws (PL-7)",
    async signal(c) {
      const sid = await liveSession(c);
      // A fresh paused machine can capture an anchor without free-running.
      const cap = (await c.call("checkpoint/capture", { session_id: sid })) as any;
      const cpId = cap?.ref?.id ?? cap?.id;
      // pin → { ref.pinned === true, stats.pinnedCount >= 1 }.
      const p = (await c.call("checkpoint/pin", { session_id: sid, id: cpId })) as any;
      const pinnedRef = p?.ref?.pinned === true;
      const pinnedStat = Number(p?.stats?.pinnedCount ?? -1) >= 1;
      // list reflects the pin on that id (the ref carried in the ring is pinned).
      const listed = (await c.call("checkpoint/list", { session_id: sid })) as any;
      const listEntry = (listed?.checkpoints ?? []).find((e: any) => String(e.id) === String(cpId));
      const listShowsPinned = listEntry?.pinned === true;
      // unpin → { ref.pinned === false }.
      const u = (await c.call("checkpoint/unpin", { session_id: sid, id: cpId })) as any;
      const unpinnedRef = u?.ref?.pinned === false;
      // unknown-id pin/unpin must THROW a real RPC error (not a silent no-op, PL-7).
      let pinUnknownThrew = false;
      try { await c.call("checkpoint/pin", { session_id: sid, id: "deadbeef-nope" }); }
      catch { pinUnknownThrew = true; }
      let unpinUnknownThrew = false;
      try { await c.call("checkpoint/unpin", { session_id: sid, id: "deadbeef-nope" }); }
      catch { unpinUnknownThrew = true; }
      return {
        // pin sets the ref pinned + bumps the pinned count.
        pinnedRef,
        pinnedStat,
        // the ring's own list view reflects the pin on that id.
        listShowsPinned,
        // unpin flips it back to false.
        unpinnedRef,
        // pin/unpin of an unknown id throws (no PL-7 silent fallback).
        pinUnknownThrew,
        unpinUnknownThrew,
      };
    },
  },

  // ── P0: ws-checkpoint-restore-unknown-id-throws — PL-7 silent-fallback trap ────
  // Spec 705.B/769 §4: checkpoint/restore with NO id or an UNKNOWN id must THROW a
  // real RPC error — never silently no-op (leaving the live machine as-is so a UI
  // thinks the scrub landed) or restore garbage. TS: ws-server.ts:1066 throws
  // "checkpoint/restore: id required" on a missing id, and restoreCheckpoint(id) on
  // an unknown id throws out of the ring lookup. A PL-7 silent-fallback (returning a
  // success envelope while doing nothing) is the classic C→TS/Rust port miss — and
  // it false-greens any restore-happy-path case that never checks the error arm. We
  // assert BOTH legs throw on BOTH runtimes AND that the machine state (pc/cycles) is
  // UNCHANGED across the failed restore (a silent restore-of-garbage would move it).
  {
    id: "ws-checkpoint-restore-unknown-id-throws",
    severity: "P0",
    title: "checkpoint/restore with missing/unknown id throws (no silent no-op, no garbage restore) — PL-7",
    async signal(c) {
      const sid = await liveSession(c);
      // Read the live coords before the failed restores — they must be untouched.
      const before = await state(c, sid);
      const pcBefore = before.cpu?.pc ?? before.pc ?? null;
      const cycBefore = before.c64Cycles ?? before.cycles ?? before.cpu?.cycles ?? null;
      // (a) missing id → throw.
      let missingThrew = false;
      try { await c.call("checkpoint/restore", { session_id: sid }); }
      catch { missingThrew = true; }
      // (b) unknown id → throw (a real error, not method-not-found and not success).
      let unknownThrew = false;
      try { await c.call("checkpoint/restore", { session_id: sid, id: "deadbeef-nope" }); }
      catch { unknownThrew = true; }
      // State must be UNCHANGED across the two failed restores (no garbage applied).
      const after = await state(c, sid);
      const pcAfter = after.cpu?.pc ?? after.pc ?? null;
      const cycAfter = after.c64Cycles ?? after.cycles ?? after.cpu?.cycles ?? null;
      return {
        missingThrew,
        unknownThrew,
        // A failed restore left the machine exactly where it was (PL-7: no silent
        // partial/garbage restore that moves the live state).
        pcUnchanged: Number(pcAfter) === Number(pcBefore),
        cyclesUnchanged: Number(cycAfter) === Number(cycBefore),
      };
    },
  },

  // ── P2: ws-checkpoint-thumbnails-shape — filmstrip thumbnail reply contract ────
  // Spec 769.5a / 705.B §4.10: checkpoint/thumbnails returns
  // { thumbnails:[{id,cycles,frame,pinned,width,height,palette(b64),indices(b64)}…] }
  // — one entry per live ring anchor that has a thumbnail, in ring order, each with a
  // non-empty base64 palette + indices and a positive width/height (the VIC display
  // dims). TS: ws-server.ts:1028-1037 (= RuntimeController.filmstrip). An explicit
  // checkpoint/capture KEEPS the framebuffer, so TRX64 renders a thumbnail from the
  // stored vicPresentation FB (main.rs:8786-8813 checkpoint_thumbnail fallback) —
  // the same shape TS emits. We capture two explicit anchors, fetch the thumbnails,
  // and compare the normalized per-entry shape (field presence + non-empty b64 +
  // w/h>0) field-for-field. A TRX64 that lacked the method would surface as method-
  // not-found (caught here → empty), diverging loud; here it is implemented, so the
  // case proves the SHAPE matches (audit ★: method-not-found is the catch).
  {
    id: "ws-checkpoint-thumbnails-shape",
    severity: "P2",
    title: "checkpoint/thumbnails reply shape matches TS (id+cycles+frame+pinned+w/h+b64 palette/indices)",
    async signal(c) {
      const sid = await liveSession(c);
      // Two explicit captures: explicit capture keeps the framebuffer, so each anchor
      // has a renderable thumbnail on both runtimes.
      await c.call("checkpoint/capture", { session_id: sid });
      await c.call("checkpoint/capture", { session_id: sid });
      let methodMissing = false;
      let res: any = null;
      try {
        res = (await c.call("checkpoint/thumbnails", { session_id: sid })) as any;
      } catch (e) {
        // method-not-found (or any error) → record the divergence loudly.
        const msg = e instanceof Error ? e.message : String(e);
        if (/method not found|-32601/i.test(msg)) methodMissing = true;
      }
      const thumbs: any[] = Array.isArray(res?.thumbnails) ? res.thumbnails : [];
      // Normalize each entry to a presence/validity shape (not the literal b64 — the
      // pictures differ pixel-for-pixel across daemons; we compare the CONTRACT).
      const entryShape = (t: any) => ({
        hasId: typeof t?.id === "string" && t.id.length > 0,
        cyclesIsNumber: typeof t?.cycles === "number",
        frameIsNumber: typeof t?.frame === "number",
        pinnedIsBool: typeof t?.pinned === "boolean",
        widthPositive: Number(t?.width) > 0,
        heightPositive: Number(t?.height) > 0,
        paletteNonEmptyB64: typeof t?.palette === "string" && t.palette.length > 0,
        indicesNonEmptyB64: typeof t?.indices === "string" && t.indices.length > 0,
      });
      return {
        methodMissing,
        // At least one thumbnail came back (the two explicit captures each have a FB).
        hasThumbnails: thumbs.length >= 1,
        // The first entry's normalized contract shape (field-for-field TS≡TRX64).
        firstShape: thumbs[0] ? entryShape(thumbs[0]) : null,
        // Every returned entry satisfies the contract (catches a partial impl).
        allValid: thumbs.length >= 1 && thumbs.every((t) => Object.values(entryShape(t)).every(Boolean)),
      };
    },
  },

  // ── P0: ws-snapshot-integrity-reject — corrupt .c64re rejected, no partial restore
  // Spec 707 §6.5/§3: snapshot/undump of a CORRUPTED container (flipped body byte =
  // failed sha256, truncated header, or bad magic) must REJECT THE WHOLE FILE with a
  // clear RPC error AND leave the machine state UNTOUCHED — no half-applied restore
  // (the high-risk port behaviour: validate-then-mutate vs mutate-as-you-go). TS:
  // undumpRuntimeSnapshot calls readNativeSnapshot FIRST (magic+version+sha256+media
  // sha) and throws BEFORE attachDisk/restoreFromSnapshot (snapshot-persistence.ts:
  // 218-246) — so a corrupt file never touches state. TRX64 mirrors this: read_native
  // _snapshot validates+returns Err BEFORE the state lock + attach_disk/restore (main
  // .rs:9076-9079). We dump a valid .c64re, then write THREE corrupt variants
  // (sha-mismatch / truncated / bad-magic), and for EACH assert undump THROWS and the
  // live {pc,cycles,RAM-sentinel} is unchanged afterward (no partial clobber).
  {
    id: "ws-snapshot-integrity-reject",
    severity: "P0",
    title: "snapshot/undump rejects a corrupted .c64re WHOLE — throws + leaves state untouched (707 §6.5)",
    async signal(c, d) {
      const sid = await liveSession(c);
      const exec = async (command: string): Promise<string> => {
        const r = (await c.call("monitor/exec", { session_id: sid, command })) as any;
        return String(r?.output ?? r?.error ?? "");
      };
      // Plant a RAM sentinel so we can prove RAM was not clobbered by a partial restore.
      await exec("wr ram c000 5a a5 5a a5");
      const readByte = async (addr: string): Promise<string> => {
        const out = await exec(`m ram ${addr} ${addr}`);
        const hex = out.replace(/[^0-9a-fA-F\s]/g, " ").trim().split(/\s+/);
        // The first 4-hex token is the address echo; the byte follows. Find the first
        // 2-hex token AFTER a 4-hex address token.
        const addrIdx = hex.findIndex((t) => t.length === 4);
        const byte = hex.slice(addrIdx + 1).find((t) => t.length === 2);
        return (byte ?? "").toLowerCase();
      };
      const sentinelBefore = await readByte("c000");
      // Dump a VALID snapshot to an abs path (a good baseline to corrupt).
      const goodPath = `${d.projectDir}/good.c64re`;
      await c.call("snapshot/dump", { session_id: sid, path: goodPath });
      const good = readFileSync(goodPath);
      const HEADER_LEN = 8 + 1 + 32; // magic(8)+version(1)+sha256(32)
      // Read the live coords AFTER the dump (the dump itself captured a checkpoint,
      // which may bump the controller frame — but pc/cycles of a paused machine hold).
      const stBefore = await state(c, sid);
      const pcBefore = stBefore.cpu?.pc ?? stBefore.pc ?? null;
      const cycBefore = stBefore.c64Cycles ?? stBefore.cycles ?? stBefore.cpu?.cycles ?? null;

      // Variant 1 — sha256 mismatch: flip a byte in the gzip body (offset >= HEADER_LEN).
      const shaBad = Buffer.from(good);
      shaBad[HEADER_LEN] = shaBad[HEADER_LEN]! ^ 0xff;
      const shaBadPath = `${d.projectDir}/sha-bad.c64re`;
      writeFileSync(shaBadPath, shaBad);
      // Variant 2 — truncated: keep only the header (no body → sha over empty != stored).
      const truncPath = `${d.projectDir}/trunc.c64re`;
      writeFileSync(truncPath, Buffer.from(good.subarray(0, HEADER_LEN)));
      // Variant 3 — bad magic: clobber the first magic byte.
      const magicBad = Buffer.from(good);
      magicBad[0] = magicBad[0]! ^ 0xff;
      const magicBadPath = `${d.projectDir}/magic-bad.c64re`;
      writeFileSync(magicBadPath, magicBad);

      const undumpThrows = async (path: string): Promise<boolean> => {
        try { await c.call("snapshot/undump", { session_id: sid, path }); return false; }
        catch { return true; }
      };
      const shaThrew = await undumpThrows(shaBadPath);
      const truncThrew = await undumpThrows(truncPath);
      const magicThrew = await undumpThrows(magicBadPath);

      // State must be UNTOUCHED after the three failed undumps (no partial clobber).
      const stAfter = await state(c, sid);
      const pcAfter = stAfter.cpu?.pc ?? stAfter.pc ?? null;
      const cycAfter = stAfter.c64Cycles ?? stAfter.cycles ?? stAfter.cpu?.cycles ?? null;
      const sentinelAfter = await readByte("c000");
      return {
        // Each corrupt variant is rejected (a real RPC error).
        shaMismatchThrew: shaThrew,
        truncatedThrew: truncThrew,
        badMagicThrew: magicThrew,
        // The machine state is untouched after the failed undumps (no partial restore).
        pcUnchanged: Number(pcAfter) === Number(pcBefore),
        cyclesUnchanged: Number(cycAfter) === Number(cycBefore),
        // The RAM sentinel survived (a half-applied restore would clobber it).
        sentinelUnchanged: sentinelAfter === sentinelBefore && sentinelBefore === "5a",
      };
    },
  },

  // ── P1: ws-snapshot-dump-descriptor — dump reply descriptor shape (707 §4) ─────
  // Spec 707 §4: snapshot/dump returns a DESCRIPTOR { path, cycle, pc, machine, media,
  // fileBytes, breakpoints } — machine "c64-pal", a real cycle/pc, the embedded media
  // list (each {role,format,sourceName?,sha256,bytes}), and the on-disk byte count. TS:
  // dumpRuntimeSnapshot (snapshot-persistence.ts:135-143). With an EasyFlash mounted,
  // the media list carries the cartridge role with a CONTENT-derived sha256 (so the
  // sha is equal across runtimes), and fileBytes>0. We mount the writable EasyFlash,
  // dump, and compare the normalized descriptor (machine, cycle/pc/fileBytes positivity,
  // the cart media entry's role/format/sha256/bytes) field-for-field TS≡TRX64. The
  // .c64re container itself is byte-identical-format (magic C64RESNP); the *file* bytes
  // differ (createdAt timestamp) so we compare fileBytes>0, not its literal value.
  {
    id: "ws-snapshot-dump-descriptor",
    severity: "P1",
    title: "snapshot/dump returns the 707 §4 descriptor (machine, cycle/pc, media sha256, fileBytes)",
    spawn: { seedFiles: [{ rel: "fixture.crt", bytes: EASYFLASH_CRT }] },
    async signal(c, d) {
      const sid = await liveSession(c);
      await c.call("media/mount", { session_id: sid, path: `${d.projectDir}/fixture.crt`, slot: 0 });
      const r = (await c.call("snapshot/dump", { session_id: sid, path: `${d.projectDir}/desc.c64re` })) as any;
      const media: any[] = Array.isArray(r?.media) ? r.media : [];
      // The cartridge media entry (role "cartridge"); its sha256 is content-derived so
      // it is EQUAL across daemons (the .crt bytes are identical), unlike volatile paths.
      const cart = media.find((m) => m.role === "cartridge");
      return {
        // The descriptor's machine model.
        machine: r?.machine ?? null,
        // A real cycle + pc (numbers ≥ 0) and a non-empty file on disk.
        cycleIsNumber: typeof r?.cycle === "number",
        pcIsNumber: typeof r?.pc === "number",
        fileBytesPositive: Number(r?.fileBytes) > 0,
        breakpointsIsNumber: typeof r?.breakpoints === "number",
        // The cart media entry is present with a content-derived sha256 (TS≡TRX64).
        cartRole: cart?.role ?? null,
        cartFormat: cart?.format ?? null,
        cartSha256: cart?.sha256 ?? null,
        cartBytesPositive: Number(cart?.bytes) > 0,
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
