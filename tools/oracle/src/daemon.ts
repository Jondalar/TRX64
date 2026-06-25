// Hermetic daemon lifecycle. Each oracle run spawns a FRESH daemon on an ephemeral
// port with a throwaway project dir, so c64Cycles + absolute trace cycles start at a
// clean cold reset and goldens are reproducible. Torn down (process group + tmp) after.

import { spawn, type ChildProcess } from "node:child_process";
import { mkdtempSync, rmSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import net from "node:net";
import { connect } from "./ws-client.js";

export type DaemonKind = "ts" | "trx64";

const C64RE_ROOT =
  process.env.C64RE_ROOT ?? "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP";
const TRX64_BIN =
  process.env.TRX64_DAEMON_BIN ??
  "/Users/alex/Development/C64/Tools/TRX64/target/debug/trx64-daemon";

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

async function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.once("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const addr = srv.address() as net.AddressInfo;
      srv.close(() => resolve(addr.port));
    });
  });
}

export interface Daemon {
  kind: DaemonKind;
  endpoint: string;
  /** The resolved project dir — so a conformance case can read media files back
   *  off disk (e.g. to assert an outgoing-disk persist actually hit the file). */
  projectDir: string;
  stop(): void;
}

export interface SpawnOpts {
  /** Launch with `--stream` so the per-frame stream loop is the live driver
   *  (required to exercise free-run behaviour: bp/JAM/observer/auto-capture). */
  stream?: boolean;
  /** Files to write into the project dir BEFORE the daemon boots, e.g. a seed
   *  disk/cart so media/recent + media/mount have real fixtures. `rel` is a path
   *  relative to the project dir; nested dirs are created. */
  seedFiles?: Array<{ rel: string; bytes: Buffer | Uint8Array }>;
}

export async function spawnDaemon(kind: DaemonKind, opts: SpawnOpts = {}): Promise<Daemon> {
  const port = await freePort();
  const projectDir = mkdtempSync(join(tmpdir(), `trx64-oracle-${kind}-`));
  const endpoint = `ws://127.0.0.1:${port}`;

  for (const f of opts.seedFiles ?? []) {
    const abs = join(projectDir, f.rel);
    mkdirSync(dirname(abs), { recursive: true });
    writeFileSync(abs, f.bytes);
  }
  const streamArg = opts.stream ? ["--stream"] : [];

  let child: ChildProcess;
  if (kind === "ts") {
    child = spawn(
      "node_modules/.bin/tsx",
      ["src/runtime/headless/daemon/run.ts", "--project", projectDir, "--port", String(port), ...streamArg],
      { cwd: C64RE_ROOT, stdio: "ignore", detached: true, env: { ...process.env, C64RE_RUNTIME_AUTOSTART: "0" } },
    );
  } else {
    child = spawn(TRX64_BIN, ["--project", projectDir, "--port", String(port), ...streamArg], {
      stdio: "ignore",
      detached: true,
    });
  }

  const stop = () => {
    try {
      if (child.pid) process.kill(-child.pid, "SIGTERM");
    } catch {
      try { child.kill("SIGTERM"); } catch { /* already gone */ }
    }
    try { rmSync(projectDir, { recursive: true, force: true }); } catch { /* ignore */ }
  };

  try {
    await waitReady(endpoint, 30_000);
  } catch (e) {
    stop();
    throw e;
  }
  return { kind, endpoint, projectDir, stop };
}

async function waitReady(endpoint: string, timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let lastErr: unknown;
  while (Date.now() < deadline) {
    try {
      const c = await connect(endpoint, 2_000);
      await c.call("ping", {}).catch(() => undefined); // best-effort liveness
      c.close();
      return;
    } catch (e) {
      lastErr = e;
      await sleep(250);
    }
  }
  throw new Error(`daemon not ready at ${endpoint} within ${timeoutMs}ms: ${String(lastErr)}`);
}
