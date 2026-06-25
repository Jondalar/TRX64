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

import { readFileSync } from "node:fs";
import { spawnDaemon, type Daemon, type SpawnOpts } from "./daemon.js";
import { connect, type RpcClient } from "./ws-client.js";
import { diffResponses, formatDivergence } from "./diff.js";

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

  const selected = CASES.filter(
    (c) => (!sev || c.severity === sev) && (!only || c.id === only),
  );
  if (selected.length === 0) {
    console.error("no cases match the filter");
    process.exit(2);
  }

  console.log(`\nDifferential WS-conformance gate — ${selected.length} case(s)\n`);
  let red = 0;
  for (const cse of selected) {
    process.stdout.write(`[${cse.severity}] ${cse.id} — ${cse.title}\n`);
    try {
      const r = await runCase(cse);
      if (r.ok) {
        console.log(`   GREEN  TRX64 ≡ TS  ${JSON.stringify(r.tsSig)}\n`);
      } else {
        red++;
        console.log(`   RED    ${r.detail}`);
        console.log(`          TS   = ${JSON.stringify(r.tsSig)}`);
        console.log(`          TRX64= ${JSON.stringify(r.trxSig)}\n`);
      }
    } catch (e) {
      red++;
      console.log(`   ERROR  ${e instanceof Error ? e.message : String(e)}\n`);
    }
  }
  console.log(`${selected.length - red}/${selected.length} GREEN, ${red} RED`);
  process.exit(red === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(e);
  process.exit(2);
});
