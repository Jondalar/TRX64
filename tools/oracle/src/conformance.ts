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
];

// ─────────────────────────────────────────────────────────────────────────────
// RUNNER
// ─────────────────────────────────────────────────────────────────────────────
async function runCase(cse: ConfCase): Promise<{ ok: boolean; tsSig: unknown; trxSig: unknown; detail: string }> {
  let ts: Daemon | undefined;
  let trx: Daemon | undefined;
  try {
    [ts, trx] = await Promise.all([spawnDaemon("ts", cse.spawn), spawnDaemon("trx64", cse.spawn)]);
    const tsC = await connect(ts.endpoint, 60_000);
    const trxC = await connect(trx.endpoint, 60_000);
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
