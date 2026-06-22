// Decode a .c64retrace binary log into comparable TraceRecords.
// Layout mirrors C64RE src/runtime/headless/trace/binary-format.ts byte-for-byte.
//
// Header:  magic "C64RETR1" (8) | version u16 LE | flags u16 LE | metaLen u32 LE | meta[metaLen]
// Events:  [opcode u8][cycle f64 LE][op-specific payload]

import type { TraceRecord } from "./diff.js";

export enum TraceOp {
  Mark = 0x01,
  CpuStep = 0x10,
  RamWrite = 0x11,
  IoWrite = 0x12,
  VicRegWrite = 0x20,
  SidRegWrite = 0x22,
  IecLineChange = 0x23,
  DriveCpuStep = 0x30,
  DriveRamWrite = 0x31,
}

const MAGIC = "C64RETR1";

export interface DecodedTrace {
  meta: Record<string, unknown>;
  version: number;
  records: TraceRecord[];
}

export function decodeTrace(buf: Buffer): DecodedTrace {
  if (buf.length < 16 || buf.subarray(0, 8).toString("latin1") !== MAGIC) {
    throw new Error("decodeTrace: bad magic (not a .c64retrace v1/v2 log)");
  }
  const version = buf.readUInt16LE(8);
  const metaLen = buf.readUInt32LE(12);
  const metaJson = buf.subarray(16, 16 + metaLen).toString("utf8");
  const meta = metaJson ? (JSON.parse(metaJson) as Record<string, unknown>) : {};
  // v1 mem-access records are 1 byte shorter (no trailing old_value).
  const memHasOld = version >= 2;

  const records: TraceRecord[] = [];
  let off = 16 + metaLen;

  while (off < buf.length) {
    const op = buf.readUInt8(off);
    off += 1;
    const cycle = buf.readDoubleLE(off);
    off += 8;

    switch (op) {
      case TraceOp.CpuStep:
      case TraceOp.DriveCpuStep: {
        const fields = {
          pc: buf.readUInt16LE(off),
          opcode: buf.readUInt8(off + 2),
          a: buf.readUInt8(off + 3),
          x: buf.readUInt8(off + 4),
          y: buf.readUInt8(off + 5),
          sp: buf.readUInt8(off + 6),
          p: buf.readUInt8(off + 7),
          b1: buf.readUInt8(off + 8),
          b2: buf.readUInt8(off + 9),
        };
        off += 10;
        records.push({ family: op === TraceOp.CpuStep ? "cpu" : "drive-cpu", cycle, fields });
        break;
      }
      case TraceOp.RamWrite:
      case TraceOp.IoWrite:
      case TraceOp.DriveRamWrite: {
        const fields: Record<string, number> = {
          addr: buf.readUInt16LE(off),
          value: buf.readUInt8(off + 2),
          pc: buf.readUInt16LE(off + 3),
          access: buf.readUInt8(off + 5),
        };
        off += 6;
        if (memHasOld) {
          fields.old = buf.readUInt8(off);
          off += 1;
        }
        const family = op === TraceOp.RamWrite ? "ram" : op === TraceOp.IoWrite ? "io" : "drive-ram";
        records.push({ family, cycle, fields });
        break;
      }
      case TraceOp.VicRegWrite: {
        const fields = {
          rasterY: buf.readUInt16LE(off),
          kind: buf.readUInt8(off + 2),
          value: buf.readUInt8(off + 3),
        };
        off += 4;
        records.push({ family: "vic", cycle, fields });
        break;
      }
      case TraceOp.SidRegWrite: {
        const fields = { reg: buf.readUInt16LE(off), value: buf.readUInt8(off + 2) };
        off += 3;
        records.push({ family: "sid", cycle, fields });
        break;
      }
      case TraceOp.IecLineChange: {
        const fields = { lines: buf.readUInt16LE(off) };
        off += 2;
        records.push({ family: "iec", cycle, fields });
        break;
      }
      case TraceOp.Mark: {
        const labelLen = buf.readUInt16LE(off);
        off += 2;
        const label = buf.subarray(off, off + labelLen).toString("utf8");
        off += labelLen;
        records.push({ family: "mark", cycle, fields: { label } });
        break;
      }
      default:
        throw new Error(`decodeTrace: unknown op 0x${op.toString(16)} at offset ${off - 9}`);
    }
  }

  return { meta, version, records };
}
