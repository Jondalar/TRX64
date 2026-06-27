// Minimal JSON-RPC 2.0 over WebSocket client. The daemon uses standard text-frame
// JSON-RPC 2.0 ({jsonrpc, id, method, params} -> {jsonrpc, id, result|error}).
// Binary frames ([type:u8][seq:u32 LE][payload]) are the screenshot/stream channel:
// BIN_VIC (0x01) palette-indexed video frames + BIN_AUDIO (0x02) PCM. Most flows are
// pure JSON-RPC, but the checkpoint-scrub cases need to SEE the binary VIC frame the
// restore pushes — `onBinary` exposes the decoded BIN_VIC envelope for that.

import { WebSocket } from "ws";

/** A decoded BIN_VIC (0x01) video frame pushed by the stream/restore path:
 *  envelope [type:u8][seq:u32 LE] + header [w:u16][h:u16][fmt:u8][rsvd][cycle:u32] +
 *  48-byte palette + w*h colour indices. (1:1 with streaming.rs build_vic_frame /
 *  ws-server.ts pushFrame.) */
export interface BinVicFrame {
  type: number;       // 0x01
  seq: number;
  width: number;
  height: number;
  fmt: number;        // 1 = palette-indexed
  cycle: number;      // c64 cpu cycles >>> 0
  indices: Uint8Array; // w*h palette indices
}

export interface RpcClient {
  call(method: string, params?: Record<string, unknown>): Promise<unknown>;
  /** Register a server-PUSH notification listener (id-less JSON-RPC frame), e.g.
   *  "debug/breakpoint_hit". Returns an unsubscribe fn. Used by the integration
   *  harness to prove a breakpoint actually fires its notification. */
  onNotify(handler: (method: string, params: unknown) => void): () => void;
  /** Register a BINARY-frame listener. Receives a decoded BIN_VIC frame (type 0x01)
   *  for each binary WS message the daemon pushes (the video stream / one-shot
   *  restore present). Non-VIC binary types are delivered with `indices` empty.
   *  Returns an unsubscribe fn. Used by ws-checkpoint-scrub-1/-2 to read the
   *  rolled-back picture the restore presents. */
  onBinary(handler: (frame: BinVicFrame) => void): () => void;
  close(): void;
}

interface Pending {
  resolve: (v: unknown) => void;
  reject: (e: Error) => void;
}

export async function connect(endpoint: string, timeoutMs = 60_000): Promise<RpcClient> {
  const ws = new WebSocket(endpoint);
  const pending = new Map<number, Pending>();
  const notifyHandlers = new Set<(method: string, params: unknown) => void>();
  const binaryHandlers = new Set<(frame: BinVicFrame) => void>();
  let nextId = 1;

  const decodeBinVic = (buf: Buffer): BinVicFrame | null => {
    // envelope [type:u8][seq:u32 LE] + header [w:u16][h:u16][fmt:u8][rsvd][cycle:u32].
    if (buf.length < 15) return null;
    const type = buf[0]!;
    const seq = buf.readUInt32LE(1);
    if (type !== 0x01) return { type, seq, width: 0, height: 0, fmt: 0, cycle: 0, indices: new Uint8Array(0) };
    const width = buf.readUInt16LE(5);
    const height = buf.readUInt16LE(7);
    const fmt = buf[9]!;
    const cycle = buf.readUInt32LE(11);
    // header(10 after the 5-byte envelope) + 48-byte palette, then w*h indices.
    const idxStart = 5 + 10 + 48;
    const indices = idxStart <= buf.length ? new Uint8Array(buf.subarray(idxStart)) : new Uint8Array(0);
    return { type, seq, width, height, fmt, cycle, indices };
  };

  ws.on("message", (data, isBinary) => {
    if (isBinary) {
      // The screenshot/stream binary channel (BIN_VIC / BIN_AUDIO). Decode BIN_VIC
      // and fan out to any binary listeners (checkpoint-scrub cases).
      if (binaryHandlers.size > 0) {
        const buf = Buffer.isBuffer(data) ? data : Buffer.from(data as ArrayBuffer);
        const f = decodeBinVic(buf);
        if (f) for (const h of binaryHandlers) h(f);
      }
      return;
    }
    let msg: { id?: number; method?: string; params?: unknown; result?: unknown; error?: { message?: string } };
    try {
      msg = JSON.parse(data.toString());
    } catch {
      return;
    }
    if (typeof msg.id !== "number") {
      // server-PUSH notification (no id) — fan out to listeners.
      if (typeof msg.method === "string") {
        for (const h of notifyHandlers) h(msg.method, msg.params);
      }
      return;
    }
    const p = pending.get(msg.id);
    if (!p) return;
    pending.delete(msg.id);
    if (msg.error) p.reject(new Error(msg.error.message ?? "rpc error"));
    else p.resolve(msg.result);
  });

  await new Promise<void>((resolve, reject) => {
    ws.once("open", () => resolve());
    ws.once("error", reject);
  });

  return {
    call(method, params) {
      const id = nextId++;
      const payload = JSON.stringify({ jsonrpc: "2.0", id, method, params: params ?? {} });
      return new Promise<unknown>((resolve, reject) => {
        const timer = setTimeout(() => {
          pending.delete(id);
          reject(new Error(`rpc timeout: ${method}`));
        }, timeoutMs);
        pending.set(id, {
          resolve: (v) => {
            clearTimeout(timer);
            resolve(v);
          },
          reject: (e) => {
            clearTimeout(timer);
            reject(e);
          },
        });
        ws.send(payload);
      });
    },
    onNotify(handler) {
      notifyHandlers.add(handler);
      return () => notifyHandlers.delete(handler);
    },
    onBinary(handler) {
      binaryHandlers.add(handler);
      return () => binaryHandlers.delete(handler);
    },
    close() {
      ws.close();
    },
  };
}
