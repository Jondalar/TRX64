//! Spec 876 — the POT lines: what `$D419`/`$D41A` of the machine's own SID read.
//!
//! Each control port has two POT lines. CIA1 PA6 closes port 1's 4066 switch and PA7
//! closes port 2's; a pin programmed as input is pulled high and closes its switch too.
//! The SID measures once every 512 PHI2 cycles and latches the count, so the registers
//! move only at `clk ≡ 0 (mod 512)`.
//!
//! The host hands over finished bytes (`$FF` = open). There is no device model and no
//! position mapping here: a paddle's count, a 1351's value and a 2nd/3rd fire button's
//! `$00`/`$FF` are the host's result.
//!
//! The latch is lazy and exact: nothing runs per cycle. [`PotLines::settle`] brings it up
//! to the last boundary at or before `clk` using the selection and values that stood
//! since the previous settle, and everything that changes either — a CIA1 `$DC00`/`$DC02`
//! write, `set`/`clear`, the reset that replaces CIA1 — settles first, with the old state.

/// The POT lines of both control ports and the SID's latch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PotLines {
    /// Port 1, port 2: `[x, y]`, or `None` for nothing there.
    set: [Option<[u8; 2]>; 2],
    /// What `$D419` / `$D41A` read now.
    latch: [u8; 2],
    /// The last 512-cycle boundary the latch reflects (`clk >> 9`).
    sampled: u64,
    /// CPU reads of `$D419`/`$D41A` on chip 0 — a counter, not state.
    pub reads: u64,
    /// Every value those reads returned, as a 256-bit set — a counter, not state.
    pub seen: [u64; 4],
}

impl Default for PotLines {
    fn default() -> Self {
        PotLines { set: [None, None], latch: [0xff, 0xff], sampled: 0, reads: 0, seen: [0; 4] }
    }
}

/// Which ports CIA1's port-A output byte selects: bit 0 = port 1 (PA6), bit 1 = port 2
/// (PA7). The byte is `Cia::pa_output()`, where an input pin counts as high.
#[inline]
pub fn select(pa_output: u8) -> u8 {
    (pa_output >> 6) & 3
}

/// Two POT lines in parallel, in counts: the count grows with R, so both switches closed
/// give `R1·R2/(R1+R2)`. `0` is a line tied to VCC and wins; `$FF` is an open line and
/// leaves the other side.
#[inline]
pub fn parallel(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        0
    } else if a == 0xff {
        b
    } else if b == 0xff {
        a
    } else {
        let (a, b) = (u16::from(a), u16::from(b));
        (a * b / (a + b)) as u8
    }
}

fn port_index(port: u8) -> Result<usize, String> {
    match port {
        1 | 2 => Ok(port as usize - 1),
        p => Err(format!("pot: control port 1 or 2, not {p}")),
    }
}

impl PotLines {
    /// What the SID would latch now under selection `sel`.
    pub fn combine(&self, sel: u8) -> [u8; 2] {
        let a = self.set[0].unwrap_or([0xff, 0xff]);
        let b = self.set[1].unwrap_or([0xff, 0xff]);
        match sel & 3 {
            0 => [0xff, 0xff],
            1 => a,
            2 => b,
            _ => [parallel(a[0], b[0]), parallel(a[1], b[1])],
        }
    }

    /// Bring the latch up to the last boundary at or before `clk`. `sel` is the selection
    /// that stood since the previous settle; the caller settles BEFORE changing it.
    #[inline]
    pub fn settle(&mut self, clk: u64, sel: u8) {
        let block = clk >> 9;
        if block != self.sampled {
            self.latch = self.combine(sel);
            self.sampled = block;
        }
    }

    /// A CPU read of `$D419` (`axis` 0) or `$D41A` (`axis` 1) on chip 0.
    #[inline]
    pub fn read(&mut self, clk: u64, sel: u8, axis: usize) -> u8 {
        self.settle(clk, sel);
        let v = self.latch[axis & 1];
        self.reads += 1;
        self.seen[(v >> 6) as usize] |= 1u64 << (v & 63);
        v
    }

    /// What a read would answer, without settling.
    pub fn peek(&self, clk: u64, sel: u8, axis: usize) -> u8 {
        if clk >> 9 != self.sampled {
            self.combine(sel)[axis & 1]
        } else {
            self.latch[axis & 1]
        }
    }

    /// The lines of `port` now carry `x` / `y`, from `clk` on. Settles first.
    pub fn set(&mut self, clk: u64, sel: u8, port: u8, x: u8, y: u8) -> Result<(), String> {
        let i = port_index(port)?;
        self.settle(clk, sel);
        self.set[i] = Some([x, y]);
        Ok(())
    }

    /// Nothing on `port` from `clk` on. Settles first.
    pub fn clear(&mut self, clk: u64, sel: u8, port: u8) -> Result<(), String> {
        let i = port_index(port)?;
        self.settle(clk, sel);
        self.set[i] = None;
        Ok(())
    }

    /// What is set on `port` (None = cleared, or not a port).
    pub fn get(&self, port: u8) -> Option<(u8, u8)> {
        let i = port_index(port).ok()?;
        self.set[i].map(|[x, y]| (x, y))
    }

    /// The latch as it stands after the last settle.
    pub fn latch(&self) -> [u8; 2] {
        self.latch
    }

    /// The boundary (`clk >> 9`) the latch reflects.
    pub fn sampled(&self) -> u64 {
        self.sampled
    }

    /// PHI2 cycles from `clk` to the next sample.
    pub fn cycles_to_next_sample(clk: u64) -> u64 {
        512 - (clk & 511)
    }

    /// The values the CPU reads returned, ascending.
    pub fn seen_values(&self) -> Vec<u8> {
        (0..=255u8).filter(|&v| self.seen[(v >> 6) as usize] & (1u64 << (v & 63)) != 0).collect()
    }

    /// Nothing set and the latch open: the checkpoint writes no node then.
    pub fn is_default(&self) -> bool {
        self.set == [None, None] && self.latch == [0xff, 0xff]
    }

    /// The state a checkpoint without a `pot` node stands for, at `clk`.
    pub fn reset_to_default(&mut self, clk: u64) {
        self.set = [None, None];
        self.latch = [0xff, 0xff];
        self.sampled = clk >> 9;
    }

    /// Spec 876 D6 — the checkpoint node, or `None` for the default.
    pub fn checkpoint(&self) -> Option<serde_json::Value> {
        if self.is_default() {
            return None;
        }
        let side = |s: Option<[u8; 2]>| match s {
            Some([x, y]) => serde_json::json!([x, y]),
            None => serde_json::Value::Null,
        };
        Some(serde_json::json!({
            "set": [side(self.set[0]), side(self.set[1])],
            "latch": [self.latch[0], self.latch[1]],
            "sampled": self.sampled,
        }))
    }

    /// Restore from a `pot` node (the counters are left as they are).
    pub fn restore(&mut self, node: &serde_json::Value) -> Result<(), String> {
        let byte = |v: &serde_json::Value| -> Result<u8, String> {
            v.as_u64().filter(|&n| n <= 0xff).map(|n| n as u8).ok_or_else(|| format!("restore pot: not a byte: {v}"))
        };
        let pair = |v: &serde_json::Value| -> Result<[u8; 2], String> {
            match v.as_array().map(|a| a.as_slice()) {
                Some([x, y]) => Ok([byte(x)?, byte(y)?]),
                _ => Err(format!("restore pot: not an [x, y] pair: {v}")),
            }
        };
        let sides = node.get("set").and_then(|v| v.as_array()).filter(|a| a.len() == 2).ok_or("restore pot: `set` is not two ports")?;
        let mut set = [None, None];
        for (i, s) in sides.iter().enumerate() {
            set[i] = if s.is_null() { None } else { Some(pair(s)?) };
        }
        let latch = pair(node.get("latch").ok_or("restore pot: no `latch`")?)?;
        let sampled = node.get("sampled").and_then(|v| v.as_u64()).ok_or("restore pot: no `sampled`")?;
        self.set = set;
        self.latch = latch;
        self.sampled = sampled;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallel_is_exact_integer_arithmetic() {
        assert_eq!(parallel(100, 100), 50);
        assert_eq!(parallel(200, 56), 43);
        assert_eq!(parallel(0, 77), 0);
        assert_eq!(parallel(77, 0), 0);
        assert_eq!(parallel(0xff, 77), 77);
        assert_eq!(parallel(77, 0xff), 77);
        assert_eq!(parallel(0xff, 0xff), 0xff);
        // Every pair against the real quotient, computed in rationals.
        for a in 1..=254u32 {
            for b in 1..=254u32 {
                assert_eq!(u32::from(parallel(a as u8, b as u8)), a * b / (a + b));
            }
        }
    }

    /// VICE computes the quotient in `double` with a resistor scale (`joyport.c:449-480`)
    /// and truncates. Recorded, not judged: the pairs where that lands one below the exact
    /// quotient.
    #[test]
    fn vice_double_path_against_exact() {
        let scale = f64::from(470000.0f32 / 255.0f32);
        let mut lower = Vec::new();
        for a in 1..=254u32 {
            for b in 1..=254u32 {
                let (r1, r2) = (scale * f64::from(a), scale * f64::from(b));
                let vice = ((r1 * r2) / (r1 + r2) / scale) as u8;
                let exact = parallel(a as u8, b as u8);
                assert!(vice == exact || vice + 1 == exact, "({a},{b}): VICE {vice}, exact {exact}");
                if vice != exact {
                    lower.push((a, b));
                }
            }
        }
        eprintln!("VICE's double path is one below the exact quotient on {} of 64516 pairs: {:?}", lower.len(), &lower[..lower.len().min(8)]);
    }

    #[test]
    fn a_node_round_trips() {
        let mut p = PotLines::default();
        p.set(1000, 1, 2, 0x12, 0x34).unwrap();
        p.settle(2048, 2);
        let node = p.checkpoint().expect("not default");
        let mut q = PotLines::default();
        q.restore(&node).unwrap();
        assert_eq!((q.set, q.latch, q.sampled), (p.set, p.latch, p.sampled));
        assert!(PotLines::default().checkpoint().is_none());
    }

    #[test]
    fn a_port_outside_one_and_two_is_refused() {
        let mut p = PotLines::default();
        assert_eq!(p.set(0, 0, 3, 1, 1).unwrap_err(), "pot: control port 1 or 2, not 3");
        assert_eq!(p.clear(0, 0, 0).unwrap_err(), "pot: control port 1 or 2, not 0");
    }
}
