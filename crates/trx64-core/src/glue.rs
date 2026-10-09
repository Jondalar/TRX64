//! glue.rs — the C64's glue logic between CIA2 port A and the VIC-II's bank:
//! VICE `c64/c64gluelogic.c`.
//!
//! Two boards. Type 0, the **discrete** glue (the 6569/6567 boards): a `$DD00` write
//! moves the VIC's bank at once. Type 1, the **custom IC** (252535-01, the C64C's
//! 8565/8562 boards): some bank changes take a cycle to settle, and for that cycle the
//! VIC fetches from a bank that is neither the old one nor the new one.
//!
//! The row of `models.toml` names the type (`glue = "discrete" | "custom-ic"`); there is no
//! separate setting. VICE's `GlueLogic` resource is the same choice, written by
//! `c64model_set` (`c64scmodel.c`).
//!
//! **What the bank is.** `c64/c64cia2.c` `store_ciapa` hands the glue the requested bank
//! (`~byte & 3` of the composed output) and `mem_set_vbank` / `vicii_set_vbank` makes it
//! the VIC's. Here the VIC's bank is DERIVED from CIA2's composed port-A output
//! ([`crate::Machine::vic_bank_base`]); the glue overrides that only while its alarm is
//! pending — [`GlueLogic::performed`] is the bank `perform_vbank_switch` last set, which
//! then differs from the requested one. When the alarm has run, the bank is the requested
//! one again and the derivation is right.

/// `GLUE_LOGIC_DISCRETE` / `GLUE_LOGIC_CUSTOM_IC` (c64gluelogic.h).
pub const GLUE_DISCRETE: u8 = 0;
pub const GLUE_CUSTOM_IC: u8 = 1;

/// The glue type a `models.toml` row names. `None` for a value the table loader refuses.
pub fn kind_of(glue: &str) -> Option<u8> {
    match glue {
        "discrete" => Some(GLUE_DISCRETE),
        "custom-ic" => Some(GLUE_CUSTOM_IC),
        _ => None,
    }
}

/// `glue_logic_type`, `old_vbank`, `glue_alarm_active` and the alarm's clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlueLogic {
    /// `glue_logic_type`.
    pub kind: u8,
    /// `old_vbank` — the bank the last `c64_glue_set_vbank` / `c64_glue_undump` was given
    /// (the REQUESTED bank, not necessarily the one the VIC has).
    pub old_vbank: u8,
    /// `glue_alarm` — the clock it runs at (`maincpu_clk + 1` when set); `None` =
    /// `glue_alarm_active == 0`.
    pub alarm: Option<u64>,
    /// The bank `perform_vbank_switch` last set, kept only while the alarm is pending (the
    /// VIC's bank then is NOT the requested one). `None` = the VIC has `old_vbank`.
    performed: Option<u8>,
}

impl GlueLogic {
    pub fn new(kind: u8) -> Self {
        GlueLogic { kind, old_vbank: 0, alarm: None, performed: None }
    }

    /// The VIC's bank number while the alarm is pending and the bank differs from the
    /// requested one; `None` = derive it from CIA2's port A.
    #[inline]
    pub fn override_bank(&self) -> Option<u8> {
        self.performed
    }

    /// `c64_glue_reset` — alarm off, `old_vbank = 0`, `perform_vbank_switch(0)`.
    pub fn reset(&mut self) {
        self.alarm = None;
        self.performed = None;
        self.old_vbank = 0;
    }

    /// `c64_glue_undump` — `perform_vbank_switch(vbank); old_vbank = vbank`. A pending
    /// alarm is left alone, as in VICE (the glue snapshot module that follows decides it).
    pub fn undump(&mut self, vbank: u8) {
        self.old_vbank = vbank;
        self.performed = if self.alarm.is_some() { Some(vbank) } else { None };
    }

    /// A checkpoint's pending alarm: due at `clk`, the VIC holding `vbank` until then.
    pub fn set_pending(&mut self, clk: u64, vbank: u8) {
        self.alarm = Some(clk);
        self.performed = Some(vbank);
    }

    /// `c64_glue_set_vbank(vbank, ddr_flag)` called at clock `clk` (`maincpu_clk`).
    /// `ddr_flag` is c64cia2.c's `pa_ddr_change`: this store was a `DDRA` write that
    /// changed the register.
    pub fn set_vbank(&mut self, vbank: u8, ddr_flag: bool, clk: u64) {
        // What the VIC has until this call changes it.
        let held = self.performed.unwrap_or(self.old_vbank);
        let mut new_vbank = vbank;
        let mut update_now = true;

        if self.kind == GLUE_CUSTOM_IC {
            if ((self.old_vbank ^ vbank) == 3) && ((vbank & vbank.wrapping_sub(1)) == 0) && (vbank != 0) {
                new_vbank = 3;
                self.alarm = Some(clk + 1); // glue_alarm_set
            } else if ddr_flag && (vbank < self.old_vbank) && ((self.old_vbank ^ vbank) != 3) {
                // "this is not quite accurate; the results flicker in some cases"
                update_now = false;
                self.alarm = Some(clk + 1);
            }
        }

        let vic_has = if update_now { new_vbank } else { held };
        self.performed = if self.alarm.is_some() { Some(vic_has) } else { None };
        self.old_vbank = vbank;
    }

    /// `glue_alarm_handler` — `perform_vbank_switch(old_vbank); glue_alarm_unset()`, when
    /// the alarm is due at `clk`.
    #[inline]
    pub fn run_alarm(&mut self, clk: u64) {
        if let Some(t) = self.alarm {
            if clk >= t {
                self.alarm = None;
                self.performed = None;
            }
        }
    }

    /// `c64_glue_snapshot_read_module`'s tail: `glue_alarm_active = snap_alarm_active`,
    /// and `glue_alarm_set()` (at `clk + 1`) when it was active on a custom-IC glue.
    pub fn restore_vsf(&mut self, old_vbank: u8, active: bool, clk: u64) {
        self.old_vbank = old_vbank;
        self.alarm = if active && self.kind == GLUE_CUSTOM_IC { Some(clk + 1) } else { None };
        // VICE's undump put the requested bank on the VIC and the module does not carry
        // the intermediate one: the alarm settles to `old_vbank` on its own.
        self.performed = None;
    }
}
