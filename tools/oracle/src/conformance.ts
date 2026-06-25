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

import { existsSync, readFileSync } from "node:fs";
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
