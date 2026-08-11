//! `eventToRow` — the frame → `trace_event` row projection.
//!
//! This is the parity-critical module: a reader that decodes every byte
//! correctly can still fail the gate here, on `seq` accounting, on channel
//! routing, or on `data_json` **text**.
//!
//! Three rules that look like bugs and are NOT to be "fixed":
//!
//! 1. **`seq` increments for every non-MARK record**, including the reserved
//!    opcodes and the loader-lens lanes that emit no row. `seq` therefore has
//!    GAPS and `event_count < max(seq)+1` whenever such records are present.
//!    MARK consumes no `seq`.
//! 2. **Channel routing is by OPCODE, never by address.** Every C64 bus access —
//!    including `$D000-$DFFF` — is written as `0x11 RAM_WRITE` by both writers;
//!    `0x12 IO_WRITE` has no live producer. Routing `$Dxxx` to `io` would change
//!    query results.
//! 3. **VIC `kind_code` is decoded and then discarded** — the row always says
//!    `"kind":"raster"`.
//!
//! `data_json` is built by a hand-rolled writer, not `serde_json`: key ORDER is
//! part of the byte-compared contract, and every number must serialize like
//! `JSON.stringify` of an integral JS `number` — `12345`, never `12345.0`
//! (Spec 802 R2 J-1; `12345.0` would also break the `CAST(... AS UBIGINT)` in
//! the compat views).

use crate::decode::*;

/// One `trace_event` row, in appender column order.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceEventRow {
    pub seq: u64,
    /// The raw f64 cycle; the appender writes `clock()` (truncated).
    pub cycle: f64,
    pub channel: &'static str,
    pub trigger_kind: &'static str,
    pub capture_kind: &'static str,
    pub data_json: String,
}

impl TraceEventRow {
    /// The cycle as the UBIGINT the appender stores.
    #[inline]
    pub fn clock(&self) -> u64 {
        let t = self.cycle.trunc();
        if t <= 0.0 {
            0
        } else {
            t as u64
        }
    }
}

/// Integral f64 → JSON integer text (no `.0`).
#[inline]
fn int_txt(v: f64) -> i64 {
    v.trunc() as i64
}

/// Translate one decoded frame into its row, or `None` when the opcode maps to
/// no row (MARK — handled by the caller — plus the reserved opcodes 0x21/0x32/
/// 0x33 and the read-set lanes 0x34/0x35/0x36, which the caller still counts
/// against `seq`).
///
/// The read-set lanes deliberately produce NO DuckDB row: they are not a
/// per-access timeline to query, they are a whole-run summary whose consumer
/// (C64RE's loader lens / `validate_extraction`) reads the `.c64retrace` stream
/// directly. Spec 785 C1 keeps CART_READ (0x36) on that same footing as the disk
/// lane rather than inventing a second consumption path for it.
pub fn event_to_row(ev: &DecodedEvent, seq: u64) -> Option<TraceEventRow> {
    match ev.op {
        OP_CPU_STEP | OP_DRIVE_CPU_STEP => {
            let drive = ev.op == OP_DRIVE_CPU_STEP;
            // key order: pc, opcode, b1, b2, a, x, y, sp, p [, side, clk]
            let mut j = JsonBuf::new();
            j.num("pc", ev.pc.unwrap_or(0) as i64);
            j.num("opcode", ev.opcode.unwrap_or(0) as i64);
            j.num("b1", ev.b1.unwrap_or(0) as i64);
            j.num("b2", ev.b2.unwrap_or(0) as i64);
            j.num("a", ev.a.unwrap_or(0) as i64);
            j.num("x", ev.x.unwrap_or(0) as i64);
            j.num("y", ev.y.unwrap_or(0) as i64);
            j.num("sp", ev.sp.unwrap_or(0) as i64);
            j.num("p", ev.p.unwrap_or(0) as i64);
            if drive {
                j.str_lit("side", "drive");
                j.num("clk", int_txt(ev.cycle));
            }
            Some(TraceEventRow {
                seq,
                cycle: ev.cycle,
                channel: if drive { "drive_pc" } else { "cpu" },
                trigger_kind: "pc-range",
                capture_kind: "cpu-row",
                data_json: j.finish(),
            })
        }
        OP_RAM_WRITE | OP_IO_WRITE | OP_DRIVE_RAM_WRITE => {
            let drive = ev.op == OP_DRIVE_RAM_WRITE;
            // key order: addr, value, op, pc, side [, oldValue], cycle_c64|cycle_drive
            let mut j = JsonBuf::new();
            j.num("addr", ev.addr.unwrap_or(0) as i64);
            j.num("value", ev.value.unwrap_or(0) as i64);
            j.str_lit(
                "op",
                if ev.access == Some(ACCESS_WRITE) { "write" } else { "read" },
            );
            j.num("pc", ev.pc.unwrap_or(0) as i64);
            j.str_lit("side", if drive { "drive" } else { "c64" });
            // Emitted ONLY when present. Never emit `null` — `bus_events.old_value`
            // relies on json_extract returning NULL for a MISSING key.
            if let Some(old) = ev.old_value {
                j.num("oldValue", old as i64);
            }
            j.num(if drive { "cycle_drive" } else { "cycle_c64" }, int_txt(ev.cycle));
            Some(TraceEventRow {
                seq,
                cycle: ev.cycle,
                // by OPCODE, not by address (see module docs, rule 2)
                channel: if ev.op == OP_IO_WRITE { "io" } else { "bus_access" },
                trigger_kind: "mem-access",
                capture_kind: "mem-row",
                data_json: j.finish(),
            })
        }
        OP_IEC_LINE_CHANGE => {
            let l = ev.lines.unwrap_or(0);
            let mut j = JsonBuf::new();
            // Every key is a JSON boolean, never 0/1.
            j.bool("atn", l & IEC_BIT_ATN != 0);
            j.bool("clk", l & IEC_BIT_CLK != 0);
            j.bool("data", l & IEC_BIT_DATA != 0);
            j.bool("c64_atn", l & IEC_BIT_C64_ATN != 0);
            j.bool("c64_clk", l & IEC_BIT_C64_CLK != 0);
            j.bool("c64_data", l & IEC_BIT_C64_DATA != 0);
            j.bool("drv_clk", l & IEC_BIT_DRV_CLK != 0);
            j.bool("drv_data", l & IEC_BIT_DRV_DATA != 0);
            j.bool("drv_atn_ack", l & IEC_BIT_DRV_ATN_ACK != 0);
            Some(TraceEventRow {
                seq,
                cycle: ev.cycle,
                channel: "iec",
                trigger_kind: "iec-transition",
                capture_kind: "iec-row",
                data_json: j.finish(),
            })
        }
        OP_VIC_REG_WRITE => {
            let mut j = JsonBuf::new();
            // `kind_code` is decoded and DISCARDED — always the literal "raster".
            j.str_lit("kind", "raster");
            j.num("raster_y", ev.raster_y.unwrap_or(0) as i64);
            j.num("value", ev.value.unwrap_or(0) as i64);
            Some(TraceEventRow {
                seq,
                cycle: ev.cycle,
                channel: "vic",
                trigger_kind: "raster-window",
                capture_kind: "vic-row",
                data_json: j.finish(),
            })
        }
        OP_SID_REG_WRITE => {
            let mut j = JsonBuf::new();
            j.num("reg", ev.reg.unwrap_or(0) as i64);
            j.num("value", ev.value.unwrap_or(0) as i64);
            Some(TraceEventRow {
                seq,
                cycle: ev.cycle,
                channel: "sid",
                trigger_kind: "mem-access",
                capture_kind: "raw",
                data_json: j.finish(),
            })
        }
        // MARK (0x01) → trace_mark, handled by the caller (consumes no seq).
        // 0x21 / 0x32 / 0x33 reserved, 0x34 / 0x35 / 0x36 read-set lanes: no row,
        // but the caller still consumes a seq number for them.
        _ => None,
    }
}

/// Minimal ordered JSON object writer.
///
/// Deliberately NOT `serde_json::Map` (alphabetical BTreeMap ordering) and NOT
/// a `Vec<(String, Value)>` with `preserve_order` (which would perturb every
/// other JSON response in the workspace). Values here are only integers,
/// booleans and a fixed set of ASCII string literals, so no escaping is needed —
/// `escape` exists as a guard should that ever change.
struct JsonBuf {
    s: String,
    first: bool,
}

impl JsonBuf {
    fn new() -> Self {
        let mut s = String::with_capacity(96);
        s.push('{');
        JsonBuf { s, first: true }
    }
    #[inline]
    fn sep(&mut self) {
        if self.first {
            self.first = false;
        } else {
            self.s.push(',');
        }
    }
    #[inline]
    fn key(&mut self, k: &str) {
        self.sep();
        self.s.push('"');
        self.s.push_str(k);
        self.s.push_str("\":");
    }
    #[inline]
    fn num(&mut self, k: &str, v: i64) {
        self.key(k);
        let mut b = itoa(v);
        self.s.push_str(b.as_str());
        b.clear();
    }
    #[inline]
    fn bool(&mut self, k: &str, v: bool) {
        self.key(k);
        self.s.push_str(if v { "true" } else { "false" });
    }
    #[inline]
    fn str_lit(&mut self, k: &str, v: &str) {
        self.key(k);
        self.s.push('"');
        escape_into(&mut self.s, v);
        self.s.push('"');
    }
    fn finish(mut self) -> String {
        self.s.push('}');
        self.s
    }
}

#[inline]
fn itoa(v: i64) -> String {
    v.to_string()
}

/// JSON string escaping, matching `JSON.stringify` for the characters that can
/// occur here. Never exercised by the current key set — kept so a future field
/// cannot silently emit invalid JSON.
fn escape_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu_ev(op: u8, cycle: f64) -> DecodedEvent {
        DecodedEvent {
            op,
            cycle,
            pc: Some(0xc000),
            opcode: Some(0xa9),
            a: Some(1),
            x: Some(2),
            y: Some(3),
            sp: Some(0xfd),
            p: Some(0x24),
            b1: Some(0x41),
            b2: Some(0),
            ..Default::default()
        }
    }

    #[test]
    fn cpu_row_key_order_and_shape() {
        let r = event_to_row(&cpu_ev(OP_CPU_STEP, 1000.0), 7).unwrap();
        assert_eq!(r.channel, "cpu");
        assert_eq!(r.trigger_kind, "pc-range");
        assert_eq!(r.capture_kind, "cpu-row");
        assert_eq!(
            r.data_json,
            r#"{"pc":49152,"opcode":169,"b1":65,"b2":0,"a":1,"x":2,"y":3,"sp":253,"p":36}"#
        );
        assert_eq!(r.seq, 7);
    }

    #[test]
    fn drive_cpu_row_adds_side_and_clk_as_integer() {
        let r = event_to_row(&cpu_ev(OP_DRIVE_CPU_STEP, 12345.0), 0).unwrap();
        assert_eq!(r.channel, "drive_pc");
        assert!(r.data_json.ends_with(r#","side":"drive","clk":12345}"#), "{}", r.data_json);
        // J-1: never `12345.0`
        assert!(!r.data_json.contains("12345.0"));
    }

    #[test]
    fn mem_row_write_with_old_value() {
        let ev = DecodedEvent {
            op: OP_RAM_WRITE,
            cycle: 555.0,
            addr: Some(0x0400),
            value: Some(0x41),
            pc: Some(0xc003),
            access: Some(ACCESS_WRITE),
            old_value: Some(0x20),
            ..Default::default()
        };
        let r = event_to_row(&ev, 1).unwrap();
        assert_eq!(r.channel, "bus_access");
        assert_eq!(r.capture_kind, "mem-row");
        assert_eq!(
            r.data_json,
            r#"{"addr":1024,"value":65,"op":"write","pc":49155,"side":"c64","oldValue":32,"cycle_c64":555}"#
        );
    }

    #[test]
    fn mem_row_read_omits_old_value_key_entirely() {
        let ev = DecodedEvent {
            op: OP_RAM_WRITE,
            cycle: 556.0,
            addr: Some(0xd012),
            value: Some(0x33),
            pc: Some(0xc006),
            access: Some(ACCESS_READ),
            old_value: None,
            ..Default::default()
        };
        let r = event_to_row(&ev, 2).unwrap();
        assert!(!r.data_json.contains("oldValue"), "missing key, not null: {}", r.data_json);
        assert!(r.data_json.contains(r#""op":"read""#));
        // Routed by OPCODE: a $D012 access is still `bus_access`, never `io`.
        assert_eq!(r.channel, "bus_access");
    }

    #[test]
    fn io_write_opcode_routes_to_io_channel() {
        let ev = DecodedEvent {
            op: OP_IO_WRITE,
            cycle: 1.0,
            addr: Some(0xdc00),
            value: Some(0x7f),
            pc: Some(0xc000),
            access: Some(ACCESS_WRITE),
            ..Default::default()
        };
        assert_eq!(event_to_row(&ev, 0).unwrap().channel, "io");
    }

    #[test]
    fn drive_mem_row_uses_cycle_drive() {
        let ev = DecodedEvent {
            op: OP_DRIVE_RAM_WRITE,
            cycle: 99.0,
            addr: Some(0x1c00),
            value: Some(0x10),
            pc: Some(0xf556),
            access: Some(ACCESS_WRITE),
            ..Default::default()
        };
        let r = event_to_row(&ev, 3).unwrap();
        assert_eq!(r.channel, "bus_access");
        assert!(r.data_json.contains(r#""side":"drive""#));
        assert!(r.data_json.ends_with(r#""cycle_drive":99}"#), "{}", r.data_json);
    }

    #[test]
    fn iec_row_is_all_booleans_in_fixed_order() {
        let ev = DecodedEvent {
            op: OP_IEC_LINE_CHANGE,
            cycle: 4.0,
            lines: Some(IEC_BIT_ATN | IEC_BIT_DRV_CLK),
            ..Default::default()
        };
        let r = event_to_row(&ev, 0).unwrap();
        assert_eq!(
            r.data_json,
            r#"{"atn":true,"clk":false,"data":false,"c64_atn":false,"c64_clk":false,"c64_data":false,"drv_clk":true,"drv_data":false,"drv_atn_ack":false}"#
        );
        assert_eq!(r.channel, "iec");
        assert_eq!(r.trigger_kind, "iec-transition");
    }

    #[test]
    fn vic_row_discards_kind_code() {
        let ev = DecodedEvent {
            op: OP_VIC_REG_WRITE,
            cycle: 5.0,
            raster_y: Some(51),
            kind_code: Some(4), // badline — DISCARDED on purpose
            value: Some(0x1b),
            ..Default::default()
        };
        let r = event_to_row(&ev, 0).unwrap();
        assert_eq!(r.data_json, r#"{"kind":"raster","raster_y":51,"value":27}"#);
    }

    #[test]
    fn sid_row_shape() {
        let ev = DecodedEvent {
            op: OP_SID_REG_WRITE,
            cycle: 6.0,
            reg: Some(0xd404),
            value: Some(0x21),
            ..Default::default()
        };
        let r = event_to_row(&ev, 0).unwrap();
        assert_eq!(r.channel, "sid");
        assert_eq!(r.capture_kind, "raw");
        assert_eq!(r.data_json, r#"{"reg":54276,"value":33}"#);
    }

    #[test]
    fn dropped_opcodes_produce_no_row() {
        for op in [OP_MARK, OP_CIA_EVENT, OP_VIA_REG_WRITE, OP_GCR_EVENT, OP_DRIVE_HEAD, OP_BLOCK_READ] {
            let ev = DecodedEvent { op, cycle: 1.0, ..Default::default() };
            assert!(event_to_row(&ev, 0).is_none(), "op {op:#x} must map to no row");
        }
    }
}
