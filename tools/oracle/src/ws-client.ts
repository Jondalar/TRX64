// Minimal JSON-RPC 2.0 over WebSocket client. The daemon uses standard text-frame
// JSON-RPC 2.0 ({jsonrpc, id, method, params} -> {jsonrpc, id, result|error}).
// Binary frames ([type:u8][seq:u32 LE][payload]) are screenshot/stream only and are
// ignored here — a run+register+trace flow is pure JSON-RPC.

import { WebSocket } from "ws";

export interface RpcClient {
  call(method: string, params?: Record<string, unknown>): Promise<unknown>;
  /** Register a server-PUSH notification listener (id-less JSON-RPC frame), e.g.
   *  "debug/breakpoint_hit". Returns an unsubscribe fn. Used by the integration
   *  harness to prove a breakpoint actually fires its notification. */
  onNotify(handler: (method: string, params: unknown) => void): () => void;
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
  let nextId = 1;

  ws.on("message", (data, isBinary) => {
    if (isBinary) return; // screenshot/stream frame — not used by the oracle
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
    close() {
      ws.close();
    },
  };
}
