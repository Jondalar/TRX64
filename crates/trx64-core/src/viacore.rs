//! viacore.rs — STRICT 1:1 port of the c64re TypeScript VIA core + VIA2 hooks.
//!
//! PORT OF (verbatim, function-for-function):
//!   - src/runtime/headless/vice1541/viacore.ts (the shared VIA 6522 engine —
//!     itself a port of vice/src/core/viacore.c). Every `viacore_*` function is
//!     ported with the SAME name (snake_case), SAME field names, SAME control
//!     flow / branch order. The `ts:` tag on each fn cites the TS line.
//!   - src/runtime/headless/vice1541/via2d.ts (the 1541 VIA2 disk-controller
//!     hooks — store/read PRA/PRB, rotation/stepper/motor/speed-zone/SYNC).
//!   - src/runtime/headless/alarm/alarm-context.ts (the VICE alarm.c port that
//!     drives the T1/T2 timers).
//!
//! This is NOT a redesign. The structure mirrors the TS exactly. Where the TS
//! stores function-pointer hooks on `via_context_t` (store_pra, read_prb,
//! set_ca2, set_int, …), Rust cannot own self-borrowing closures cleanly, so the
//! hooks are dispatched through the [`ViaBackend`] trait — the backend carries
//! the rotation / IEC / interrupt state the TS reaches via `ctx.prv.drive` /
//! `ctx.context`. The viacore functions themselves are 1:1.
//!
//! The alarm context is owned per-VIA: VICE allocates the 5 alarms (T1zero,
//! T2zero, T2uflow, T2SR, SR) into the drive's shared alarm_context in
//! viacore_init; functionally each VIA's alarm set is independent, so each
//! `ViaContext` owns its own [`AlarmContext`]. Alarm callbacks are dispatched by
//! [`AlarmId`] back into the right `viacore_*` function.

#![allow(clippy::needless_range_loop)]
#![allow(clippy::collapsible_else_if)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::manual_range_contains)]

// =============================================================================
// Register file indices — drivetypes.ts:209-226 (via.h:35-55)
// =============================================================================
pub const VIA_PRB: usize = 0;
pub const VIA_PRA: usize = 1;
pub const VIA_DDRB: usize = 2;
pub const VIA_DDRA: usize = 3;
pub const VIA_T1CL: usize = 4;
pub const VIA_T1CH: usize = 5;
pub const VIA_T1LL: usize = 6;
pub const VIA_T1LH: usize = 7;
pub const VIA_T2CL: usize = 8;
pub const VIA_T2LL: usize = 8;
pub const VIA_T2CH: usize = 9;
pub const VIA_T2LH: usize = 9;
pub const VIA_SR: usize = 10;
pub const VIA_ACR: usize = 11;
pub const VIA_PCR: usize = 12;
pub const VIA_IFR: usize = 13;
pub const VIA_IER: usize = 14;
pub const VIA_PRA_NHS: usize = 15;

// =============================================================================
// IFR / IER bit masks — drivetypes.ts:232-239 (via.h:59-66)
// =============================================================================
pub const VIA_IM_IRQ: u8 = 128;
pub const VIA_IM_T1: u8 = 64;
pub const VIA_IM_T2: u8 = 32;
pub const VIA_IM_CB1: u8 = 16;
pub const VIA_IM_CB2: u8 = 8;
pub const VIA_IM_SR: u8 = 4;
pub const VIA_IM_CA1: u8 = 2;
pub const VIA_IM_CA2: u8 = 1;

// =============================================================================
// ACR masks — drivetypes.ts:245-269 (via.h:68-93)
// =============================================================================
pub const VIA_ACR_T1_FREE_RUN: u8 = 0x40;
pub const VIA_ACR_T1_PB7_USED: u8 = 0x80;
pub const VIA_ACR_T2_CONTROL: u8 = 0x20;
pub const VIA_ACR_T2_COUNTPB6: u8 = 0x20;
pub const VIA_ACR_SR_CONTROL: u8 = 0x1c;
pub const VIA_ACR_SR_OUT: u8 = 0x10;
pub const VIA_ACR_SR_DISABLED: u8 = 0x00;
pub const VIA_ACR_SR_IN_T2: u8 = 0x04;
pub const VIA_ACR_SR_IN_PHI2: u8 = 0x08;
pub const VIA_ACR_SR_IN_CB1: u8 = 0x0c;
pub const VIA_ACR_SR_OUT_FREE_T2: u8 = 0x10;
pub const VIA_ACR_SR_OUT_T2: u8 = 0x14;
pub const VIA_ACR_SR_OUT_PHI2: u8 = 0x18;
pub const VIA_ACR_SR_OUT_CB1: u8 = 0x1c;
pub const VIA_ACR_PA_LATCH: u8 = 0x01;
pub const VIA_ACR_PB_LATCH: u8 = 0x02;

// =============================================================================
// PCR masks — drivetypes.ts:275-303 (via.h:95-130)
// =============================================================================
pub const VIA_PCR_CA1_CONTROL: u8 = 0x01;
pub const VIA_PCR_CA2_CONTROL: u8 = 0x0e;
pub const VIA_PCR_CA2_I_OR_O: u8 = 0x08;
pub const VIA_PCR_CA2_INPUT: u8 = 0x00;
pub const VIA_PCR_CA2_LOW_OUTPUT: u8 = 0x0c;
pub const VIA_PCR_CA2_HIGH_OUTPUT: u8 = 0x0e;
pub const VIA_PCR_CB1_CONTROL: u8 = 0x10;
pub const VIA_PCR_CB1_POS_ACTIVE_EDGE: u8 = 0x10;
pub const VIA_PCR_CB2_CONTROL: u8 = 0xe0;
pub const VIA_PCR_CB2_I_OR_O: u8 = 0x80;
pub const VIA_PCR_CB2_INPUT: u8 = 0x00;
pub const VIA_PCR_CB2_LOW_OUTPUT: u8 = 0xc0;

// =============================================================================
// Signal lines — drivetypes.ts:309-315 (via.h:134-140)
// =============================================================================
pub const VIA_SIG_CA1: u8 = 0;
pub const VIA_SIG_CA2: u8 = 1;
pub const VIA_SIG_CB1: u8 = 2;
pub const VIA_SIG_CB2: u8 = 3;
pub const VIA_SIG_FALL: u8 = 0;
pub const VIA_SIG_RISE: u8 = 1;

// =============================================================================
// Shift state markers — drivetypes.ts:321-322 (via.h:172-173)
// =============================================================================
pub const START_SHIFTING: i32 = 0;
pub const FINISHED_SHIFTING: i32 = 16;

// =============================================================================
// Module-private constants — viacore.ts:141-151
// =============================================================================
// PORT OF: viacore.c:216 (#define FULL_CYCLE_2 2)
const FULL_CYCLE_2: u64 = 2;
// PORT OF: viacore.c:286 (#define SR_PHI2_FIRST_OFFSET 3)
const SR_PHI2_FIRST_OFFSET: u64 = 3;
// PORT OF: viacore.c:287 (#define SR_PHI2_NEXT_OFFSET 1)
const SR_PHI2_NEXT_OFFSET: u64 = 1;

/// Disabled / no-pending-alarm sentinel (alarm-context.ts:52 CLOCK_MAX =
/// CLOCK_NEVER = Number.MAX_SAFE_INTEGER). Any reachable clk is `<` this.
const CLOCK_MAX: u64 = u64::MAX;

// =============================================================================
// Alarm context — STRICT 1:1 port of alarm-context.ts (vice/src/alarm.c)
// =============================================================================

/// The 5 VIA alarms allocated by viacore_init (viacore.ts:1304-1333). Each maps
/// 1:1 to a `viacore_*_alarm` callback dispatched by [`AlarmContext::dispatch`].
/// VICE stores a `callback` function pointer on each `alarm_t`; Rust can't hold a
/// bare fn-ptr that re-borrows `ctx`, so the id selects the callback at dispatch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AlarmId {
    T1Zero,
    T2Zero,
    T2Underflow,
    T2Shift,
    Phi2Sr,
}

/// alarm.h:38-58 `struct alarm_s` / `alarm_t`. Field names verbatim. In the TS
/// each VIA alarm lives on the shared AlarmContext; here every ViaContext owns
/// its own context with exactly these 5 alarms, so an alarm IS identified by its
/// [`AlarmId`] (the linked-list `next`/`prev` of the TS is unnecessary because
/// the alarm set is fixed and known).
#[derive(Clone, Debug)]
pub struct Alarm {
    /// Index into context.pending_alarms; < 0 means not pending. alarm.h:50.
    pub pending_idx: i32,
}

impl Alarm {
    fn new() -> Self {
        Alarm { pending_idx: -1 }
    }
}

/// alarm.h:60-67 `struct pending_alarms_s`.
#[derive(Clone, Copy, Debug)]
pub struct PendingAlarm {
    pub alarm: AlarmId,
    pub clk: u64,
}

/// alarm.h:33 `ALARM_CONTEXT_MAX_PENDING_ALARMS 0x100` — here only 5 alarms ever
/// register, so a small fixed array suffices (kept 0x100-shaped per the TS).
const ALARM_CONTEXT_MAX_PENDING_ALARMS: usize = 0x100;

/// alarm.h:70-88 `struct alarm_context_s`. 1:1 port of alarm-context.ts.
#[derive(Clone, Debug)]
pub struct AlarmContext {
    /// The 5 registered alarms, indexed by [`AlarmId`] ordinal. Replaces the TS
    /// `alarms` linked list (fixed alarm set — see [`Alarm`]).
    pub alarms: [Alarm; 5],
    /// Pending alarm array. alarm.h:79.
    pub pending_alarms: Vec<PendingAlarm>,
    /// Number of valid entries in pending_alarms[0..num_pending_alarms-1]. alarm.h:80.
    pub num_pending_alarms: usize,
    /// Cached next-fire clk (CLOCK_MAX when none). alarm.h:83.
    pub next_pending_alarm_clk: u64,
    /// Cached pending_alarms[] index of the next-fire alarm (-1 when none). alarm.h:86.
    pub next_pending_alarm_idx: i32,
}

#[inline]
fn alarm_idx(id: AlarmId) -> usize {
    match id {
        AlarmId::T1Zero => 0,
        AlarmId::T2Zero => 1,
        AlarmId::T2Underflow => 2,
        AlarmId::T2Shift => 3,
        AlarmId::Phi2Sr => 4,
    }
}

impl AlarmContext {
    /// alarm.c:39-57 alarm_context_new / alarm_context_init.
    pub fn new() -> Self {
        AlarmContext {
            alarms: [
                Alarm::new(),
                Alarm::new(),
                Alarm::new(),
                Alarm::new(),
                Alarm::new(),
            ],
            pending_alarms: vec![
                PendingAlarm { alarm: AlarmId::T1Zero, clk: 0 };
                ALARM_CONTEXT_MAX_PENDING_ALARMS
            ],
            num_pending_alarms: 0,
            next_pending_alarm_clk: CLOCK_MAX,
            next_pending_alarm_idx: -1,
        }
    }

    /// alarm.c:330-366 `alarm_unset`. Removes the alarm from pending_alarms by
    /// swap-with-last; updates next_pending cache.
    pub fn alarm_unset(&mut self, id: AlarmId) {
        let ai = alarm_idx(id);
        let idx = self.alarms[ai].pending_idx;
        if idx < 0 {
            return; // Not pending.
        }
        let idx = idx as usize;

        if self.num_pending_alarms > 1 {
            self.num_pending_alarms -= 1;
            let last = self.num_pending_alarms;

            if last != idx {
                // alarm.c:184-193 — copy last → idx, fix moved alarm's pending_idx.
                let moved = self.pending_alarms[last];
                self.pending_alarms[idx] = moved;
                self.alarms[alarm_idx(moved.alarm)].pending_idx = idx as i32;
            }

            if self.next_pending_alarm_idx == idx as i32 {
                self.update_next_pending();
            } else if self.next_pending_alarm_idx == last as i32 {
                self.next_pending_alarm_idx = idx as i32;
            }
        } else {
            // alarm.c:200-204 — last pending alarm removed; reset.
            self.num_pending_alarms = 0;
            self.next_pending_alarm_clk = CLOCK_MAX;
            self.next_pending_alarm_idx = -1;
        }

        self.alarms[ai].pending_idx = -1;
    }

    /// alarm.h:110-129 `alarm_context_update_next_pending`. Slow-path linear scan.
    /// Note VICE uses `<=` so among equal clks the LAST in array order wins.
    pub fn update_next_pending(&mut self) {
        let mut next_pending_alarm_clk: u64 = CLOCK_MAX;
        let mut next_pending_alarm_idx: i32 = self.next_pending_alarm_idx;
        for i in 0..self.num_pending_alarms {
            let pending_clk = self.pending_alarms[i].clk;
            if pending_clk <= next_pending_alarm_clk {
                next_pending_alarm_clk = pending_clk;
                next_pending_alarm_idx = i as i32;
            }
        }
        self.next_pending_alarm_clk = next_pending_alarm_clk;
        self.next_pending_alarm_idx = next_pending_alarm_idx;
    }

    /// alarm.h:146-185 `alarm_set`. Schedule (or reschedule) `id` to fire at `cpu_clk`.
    pub fn alarm_set(&mut self, id: AlarmId, cpu_clk: u64) {
        let ai = alarm_idx(id);
        let idx = self.alarms[ai].pending_idx;

        if idx < 0 {
            // Not pending yet: add.
            let new_idx = self.num_pending_alarms;
            if new_idx >= ALARM_CONTEXT_MAX_PENDING_ALARMS {
                // alarm_log_too_many_alarms — return without scheduling.
                return;
            }
            self.pending_alarms[new_idx] = PendingAlarm { alarm: id, clk: cpu_clk };
            self.num_pending_alarms += 1;
            if cpu_clk < self.next_pending_alarm_clk {
                self.next_pending_alarm_clk = cpu_clk;
                self.next_pending_alarm_idx = new_idx as i32;
            }
            self.alarms[ai].pending_idx = new_idx as i32;
        } else {
            // Already pending: modify.
            let idx = idx as usize;
            self.pending_alarms[idx].clk = cpu_clk;
            if self.next_pending_alarm_clk > cpu_clk
                || idx as i32 == self.next_pending_alarm_idx
            {
                self.update_next_pending();
            }
        }
    }
}

impl Default for AlarmContext {
    fn default() -> Self {
        Self::new()
    }
}

/// alarm.c:330 helper — is alarm `id` pending? (`a->pending_idx >= 0`)
#[inline]
fn alarm_is_pending_id(ctx: &ViaContext, id: AlarmId) -> bool {
    ctx.alarm_context.alarms[alarm_idx(id)].pending_idx >= 0
}

// =============================================================================
// ViaBackend — the function-pointer hooks of via_context_t (drivetypes.ts:935-953)
// =============================================================================
//
// The TS stores 19 callback fields on `via_context_t` (store_pra, store_prb,
// store_pcr, store_acr, store_sr, store_t2l, read_pra, read_prb, set_int,
// restore_int, set_ca2, set_cb1, set_cb2, sr_underflow, reset, undump_*). Each
// hook receives `ctx` and reaches `ctx.prv.drive` (rotation) / `ctx.context`
// (the disk unit, for the int_status). Rust cannot store closures that re-borrow
// `ctx`; instead the viacore functions take `backend: &mut dyn ViaBackend` and
// call the hook methods on it. The backend owns the rotation / IEC / IntStatus
// state. A hook the VIA does not install (e.g. via2 has no set_cb1) keeps the
// trait default (a no-op), matching the TS `ctx.set_cb1?.()` null-skip.

/// Reset-time backend reset hook flag — VICE `reset` for VIA2 sets the LED and
/// updates UI; VIA1 has none. The `reset()` hook is invoked from viacore_reset.
pub trait ViaBackend {
    /// store_pra (via2d.c:180-192 / via1d1541). `ctx.store_pra?.(ctx, byte, oldpa, addr)`.
    fn store_pra(&mut self, _ctx: &mut ViaContext, _byte: u8, _oldpa: u8, _addr: usize) {}
    /// store_prb (via2d.c:199-355). `ctx.store_prb?.(ctx, byte, oldpb, addr)`.
    fn store_prb(&mut self, _ctx: &mut ViaContext, _byte: u8, _oldpb: u8, _addr: usize) {}
    /// store_pcr (via2d.c:369-396). Returns the (possibly modified) byte committed
    /// to via[VIA_PCR]; `None` ⇒ TS `undefined` (keep `v` as-is).
    fn store_pcr(&mut self, _ctx: &mut ViaContext, byte: u8, _addr: usize) -> Option<u8> {
        let _ = byte;
        None
    }
    /// store_acr (via2d.c:411-413 — empty for via2).
    fn store_acr(&mut self, _ctx: &mut ViaContext, _byte: u8) {}
    /// store_sr (via2d.c:415-417 — empty for via2).
    fn store_sr(&mut self, _ctx: &mut ViaContext, _byte: u8) {}
    /// store_t2l (via2d.c:419-421 — empty for via2).
    fn store_t2l(&mut self, _ctx: &mut ViaContext, _byte: u8) {}
    /// read_pra (via2d.c:463-484). Returns the PRA pin byte. `null` ⇒ 0xff.
    fn read_pra(&mut self, _ctx: &ViaContext, _addr: usize) -> Option<u8> {
        None
    }
    /// read_prb (via2d.c:486-512). Returns the PRB pin byte. `null` ⇒ 0xff.
    fn read_prb(&mut self, _ctx: &ViaContext) -> Option<u8> {
        None
    }
    /// set_int (via2d.c:112-121). `ctx.set_int?.(ctx, int_num, value, rclk)` →
    /// interrupt_set_irq(int_status, int_num, value, rclk).
    fn set_int(&mut self, _ctx: &ViaContext, _int_num: u32, _value: u32, _rclk: u64) {}
    /// restore_int (via2d.c:123-130). No-op for the headless drive (snapshot only).
    fn restore_int(&mut self, _ctx: &ViaContext, _int_num: u32, _value: u32) {}
    /// set_ca2 (via2d.c:72-93). `ctx.set_ca2?.(ctx, state)`.
    fn set_ca2(&mut self, _ctx: &ViaContext, _state: u32) {}
    /// set_cb1 (not installed for via2). `ctx.set_cb1?.(ctx, state)`.
    fn set_cb1(&mut self, _ctx: &mut ViaContext, _state: u32) {}
    /// Is set_cb1 installed? (TS tests `ctx.set_cb1 &&` / `if (!ctx.cb1_is_input)`
    /// — viacore_cache_cb12_io_status:1244 / do_shiftregister:1161.)
    fn has_set_cb1(&self) -> bool {
        false
    }
    /// set_cb2 (via2d.c:95-110). `ctx.set_cb2?.(ctx, state, offset)`.
    fn set_cb2(&mut self, _ctx: &ViaContext, _state: u32, _offset: u64) {}
    /// sr_underflow (not installed for via2). `ctx.sr_underflow?.(ctx)`.
    fn sr_underflow(&mut self, _ctx: &mut ViaContext) {}
    /// reset (via2d.c:423-431). `ctx.reset?.(ctx)`.
    fn reset(&mut self, _ctx: &mut ViaContext) {}
}

// =============================================================================
// ViaContext — STRICT 1:1 port of via_context_t (drivetypes.ts:837-954)
// =============================================================================
//
// Same field names (snake_case), same widths (u8 regs, u8/u16 derived state,
// u64 clocks). The function-pointer fields of the TS struct are NOT stored here
// (they move to the ViaBackend trait — see above); everything else is verbatim.
// `int_num` and `myname`/`my_module_name` are kept for parity. `prv`/`context`/
// `alarm_context` of the TS become: the backend (prv+context), and the owned
// `alarm_context` below.
#[derive(Clone, Debug)]
pub struct ViaContext {
    /// 16-register backing store (VIA_PRB..VIA_PRA_NHS). drivetypes.ts:838-839.
    pub via: [u8; 16],

    pub ifr: u8,
    pub ier: u8,

    /// T1 latch.
    pub tal: u16,

    /// T2 counter low / high.
    pub t2cl: u8,
    pub t2ch: u8,

    /// T1 reload-from-latch time.
    pub t1reload: u64,
    /// When T2 reached/last read 0000 (or xx00 in COUNTPB6 mode).
    pub t2zero: u64,
    /// T1: when alarm viacore_t1_zero_alarm() goes off.
    pub t1zero: u64,

    /// Set if T2 should IRQ at the first 0000 OR if it is in 8-bit mode.
    pub t2xx00: bool,

    /// 0x00 or 0x80.
    pub t1_pb7: u8,

    pub oldpa: u8,
    pub oldpb: u8,
    pub ila: u8,
    pub ilb: u8,

    pub ca2_out_state: bool,
    pub cb1_in_state: bool,
    pub cb1_out_state: bool,
    pub cb2_in_state: bool,
    pub cb2_out_state: bool,
    pub cb1_is_input: bool,
    pub cb2_is_input: bool,

    /// Shift-register helper (START_SHIFTING..FINISHED_SHIFTING). i32 because the
    /// TS does `ctx.shift_state++` past FINISHED_SHIFTING and masks `&= 0x0f`.
    pub shift_state: i32,

    /// Alarm context (owns the 5 T1/T2/SR alarms). The TS holds the 5 alarms as
    /// separate refs on the ctx; here they live in `alarm_context.alarms[]`,
    /// addressed by AlarmId. Whether an alarm "exists" (`if (ctx.t1_zero_alarm)`)
    /// is always true after viacore_init — modelled by `alarms_inited`.
    pub alarm_context: AlarmContext,
    /// True once viacore_init allocated the alarms (TS `ctx.t*_alarm !== null`).
    pub alarms_inited: bool,

    /// init to LOG_DEFAULT.
    pub log: i32,
    /// init to 0.
    pub read_clk: u64,
    /// init to 0.
    pub read_offset: u64,
    /// init to 0.
    pub last_read: u8,

    /// Each write to T2H allows one IRQ.
    pub t2_irq_allowed: bool,

    /// IK_* interrupt-line kind.
    pub irq_line: u32,

    pub int_num: u32,

    /// init to "DriveXViaY".
    pub myname: Option<String>,
    /// init to "VIAXDY".
    pub my_module_name: Option<String>,
    pub my_module_name_alt1: Option<String>,
    pub my_module_name_alt2: Option<String>,

    /// PL-6: shared CLOCK ref (= VICE clk_ptr->value). Read every access; the
    /// drive bus mirrors `core.clk` into this before each viacore call.
    pub clk: u64,
    /// PL-6: shared RMW-flag ref. The drive 6510 core sets this on a RMW store.
    pub rmw_flag: u32,
    /// 1 if CPU core does CLK++ before store. Per-instance.
    pub write_offset: u64,

    pub enabled: bool,
}

impl ViaContext {
    /// Allocate a zeroed via_context_t (via2d.c:625-696 calloc-equivalent). Field
    /// init mirrors the TS object literal; `viacore_setup_context` then seeds the
    /// power-on register values.
    pub fn new() -> Self {
        ViaContext {
            via: [0u8; 16],
            ifr: 0,
            ier: 0,
            tal: 0,
            t2cl: 0,
            t2ch: 0,
            t1reload: 0,
            t2zero: 0,
            t1zero: 0,
            t2xx00: false,
            t1_pb7: 0,
            oldpa: 0,
            oldpb: 0,
            ila: 0,
            ilb: 0,
            ca2_out_state: false,
            cb1_in_state: false,
            cb1_out_state: false,
            cb2_in_state: false,
            cb2_out_state: false,
            cb1_is_input: false,
            cb2_is_input: false,
            shift_state: 0,
            alarm_context: AlarmContext::new(),
            alarms_inited: false,
            log: 0,
            read_clk: 0,
            read_offset: 0,
            last_read: 0,
            t2_irq_allowed: false,
            irq_line: 0,
            int_num: 0,
            myname: None,
            my_module_name: None,
            my_module_name_alt1: None,
            my_module_name_alt2: None,
            clk: 0,
            rmw_flag: 0,
            write_offset: 0,
            enabled: false,
        }
    }
}

impl Default for ViaContext {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Module-private helper macros — viacore.ts:161-207 (viacore.c:106-127 IS_*)
// =============================================================================
// PORT OF: viacore.ts:161-163 (IS_CA2_INDINPUT)
#[inline]
fn is_ca2_indinput(ctx: &ViaContext) -> bool {
    (ctx.via[VIA_PCR] & 0x0a) == 0x02
}
// PORT OF: viacore.ts:165-167 (IS_CA2_HANDSHAKE)
#[inline]
fn is_ca2_handshake(ctx: &ViaContext) -> bool {
    (ctx.via[VIA_PCR] & 0x0c) == 0x08
}
// PORT OF: viacore.ts:169-171 (IS_CA2_PULSE_MODE)
#[inline]
fn is_ca2_pulse_mode(ctx: &ViaContext) -> bool {
    (ctx.via[VIA_PCR] & 0x0e) == 0x0a
}
// PORT OF: viacore.ts:173-175 (IS_CA2_TOGGLE_MODE)
#[inline]
fn is_ca2_toggle_mode(ctx: &ViaContext) -> bool {
    (ctx.via[VIA_PCR] & 0x0e) == 0x08
}
// PORT OF: viacore.ts:177-179 (IS_CB2_HANDSHAKE)
#[inline]
fn is_cb2_handshake(ctx: &ViaContext) -> bool {
    (ctx.via[VIA_PCR] & 0xc0) == 0x80
}
// PORT OF: viacore.ts:181-183 (IS_CB2_PULSE_MODE)
#[inline]
fn is_cb2_pulse_mode(ctx: &ViaContext) -> bool {
    (ctx.via[VIA_PCR] & 0xe0) == 0xa0
}
// PORT OF: viacore.ts:185-187 (IS_CB2_TOGGLE_MODE)
#[inline]
fn is_cb2_toggle_mode(ctx: &ViaContext) -> bool {
    (ctx.via[VIA_PCR] & 0xe0) == 0x80
}
// PORT OF: viacore.ts:189-191 (IS_PA_INPUT_LATCH)
#[allow(dead_code)]
#[inline]
fn is_pa_input_latch(ctx: &ViaContext) -> bool {
    (ctx.via[VIA_ACR] & VIA_ACR_PA_LATCH) != 0
}
// PORT OF: viacore.ts:193-195 (IS_PB_INPUT_LATCH)
#[inline]
fn is_pb_input_latch(ctx: &ViaContext) -> bool {
    (ctx.via[VIA_ACR] & VIA_ACR_PB_LATCH) != 0
}
// PORT OF: viacore.ts:197-199 (IS_SR_FREE_RUNNING)
#[inline]
fn is_sr_free_running(ctx: &ViaContext) -> bool {
    (ctx.via[VIA_ACR] & 0x1c) == 0x10
}
// PORT OF: viacore.ts:201-203 (IS_SR_T2_CONTROLLED(byte))
#[inline]
fn is_sr_t2_controlled(byte: u8) -> bool {
    (byte & 0x0c) == 0x04 || (byte & 0x1c) == 0x10
}
// PORT OF: viacore.ts:205-207 (IS_T2_TIMER(byte))
#[inline]
fn is_t2_timer(byte: u8) -> bool {
    (byte & VIA_ACR_T2_CONTROL) == 0x00
}

// =============================================================================
// Module-private IRQ helpers — viacore.ts:214-234 (viacore.c:198-214)
// =============================================================================

// PORT OF: viacore.ts:214-216 (via_restore_int)
pub fn via_restore_int(ctx: &mut ViaContext, backend: &mut dyn ViaBackend, value: u32) {
    backend.restore_int(ctx, ctx.int_num, value);
}

// PORT OF: viacore.ts:219-229 (update_myviairq_rclk)
pub fn update_myviairq_rclk(ctx: &mut ViaContext, backend: &mut dyn ViaBackend, rclk: u64) {
    backend.set_int(
        ctx,
        ctx.int_num,
        if (ctx.ifr & ctx.ier & 0x7f) != 0 { 1 } else { 0 },
        rclk,
    );
}

// PORT OF: viacore.ts:232-234 (update_myviairq)
pub fn update_myviairq(ctx: &mut ViaContext, backend: &mut dyn ViaBackend) {
    update_myviairq_rclk(ctx, backend, ctx.clk);
}

// =============================================================================
// T1 / T2 readout helpers — viacore.ts:245-281 (viacore.c:265-361)
// =============================================================================

// PORT OF: viacore.ts:245-254 (viacore_t1)
pub fn viacore_t1(ctx: &ViaContext, rclk: u64) -> u16 {
    if rclk < ctx.t1reload {
        let res = ctx.t1reload.wrapping_sub(rclk).wrapping_sub(FULL_CYCLE_2);
        return (res & 0xffff) as u16;
    }
    let full_cycle = ctx.tal as u64 + FULL_CYCLE_2;
    let time_past_last_reload = rclk - ctx.t1reload;
    let partial_cycle = time_past_last_reload % full_cycle;
    ((ctx.tal as u64).wrapping_sub(partial_cycle) & 0xffff) as u16
}

// PORT OF: viacore.ts:257-269 (viacore_t2)
pub fn viacore_t2(ctx: &ViaContext, rclk: u64) -> u16 {
    let t2: u16;
    if ctx.via[VIA_ACR] & VIA_ACR_T2_COUNTPB6 != 0 {
        t2 = (((ctx.t2ch as u16) << 8) | (ctx.t2cl as u16)) & 0xffff;
    } else {
        let mut t = (ctx.t2zero.wrapping_sub(rclk) & 0xffff) as u16;
        if ctx.t2xx00 {
            let t2hi = ctx.t2ch as u16;
            t = ((t2hi << 8) | (t & 0xff)) & 0xffff;
        }
        t2 = t;
    }
    t2
}

// PORT OF: viacore.ts:272-281 (update_via_t1_latch)
pub fn update_via_t1_latch(ctx: &mut ViaContext, rclk: u64) {
    if rclk >= ctx.t1reload {
        let full_cycle = ctx.tal as u64 + FULL_CYCLE_2;
        let time_past_last_reload = rclk - ctx.t1reload;
        let nuf = 1 + time_past_last_reload / full_cycle;
        ctx.t1reload += nuf * full_cycle;
    }
    ctx.tal = ((ctx.via[VIA_T1LL] as u16) | ((ctx.via[VIA_T1LH] as u16) << 8)) & 0xffff;
}

// =============================================================================
// Alarm-pending helpers — viacore.ts:288-376 (viacore.c:481-632)
// =============================================================================

// PORT OF: viacore.ts:288-294 (alarm_clk)
pub fn alarm_clk(ctx: &ViaContext, id: AlarmId) -> u64 {
    let a = &ctx.alarm_context.alarms[alarm_idx(id)];
    if a.pending_idx >= 0 {
        return ctx.alarm_context.pending_alarms[a.pending_idx as usize].clk;
    }
    0
}

// PORT OF: viacore.ts:297-316 (run_pending_alarms). Dispatches due alarms; each
// alarm callback re-schedules/unsets itself (1:1 with the TS dispatch loop).
pub fn run_pending_alarms(
    ctx: &mut ViaContext,
    backend: &mut dyn ViaBackend,
    clk: u64,
    offset: u64,
) {
    while clk > ctx.alarm_context.next_pending_alarm_clk {
        // alarm.h:131-144 alarm_context_dispatch: fire the cached next-pending
        // alarm, offset = u32(cpu_clk - next_pending_alarm_clk).
        let cpu_clk = (clk + offset) & 0xffff_ffff;
        let next_clk = ctx.alarm_context.next_pending_alarm_clk;
        let off = cpu_clk.wrapping_sub(next_clk) & 0xffff_ffff;
        let idx = ctx.alarm_context.next_pending_alarm_idx;
        let id = ctx.alarm_context.pending_alarms[idx as usize].alarm;
        dispatch_alarm(ctx, backend, id, off);
    }
}

// alarm.h:131-144 — invoke the right viacore_*_alarm callback by id.
#[inline]
fn dispatch_alarm(ctx: &mut ViaContext, backend: &mut dyn ViaBackend, id: AlarmId, offset: u64) {
    match id {
        AlarmId::T1Zero => viacore_t1_zero_alarm(ctx, backend, offset),
        AlarmId::T2Zero => viacore_t2_zero_alarm(ctx, backend, offset),
        AlarmId::T2Underflow => viacore_t2_underflow_alarm(ctx, backend, offset),
        AlarmId::T2Shift => viacore_t2_shift_alarm(ctx, backend, offset),
        AlarmId::Phi2Sr => viacore_phi2_sr_alarm(ctx, backend, offset),
    }
}

// PORT OF: viacore.ts:319-321 (alarm_is_pending)
#[inline]
fn alarm_is_pending(ctx: &ViaContext, id: AlarmId) -> bool {
    alarm_is_pending_id(ctx, id)
}

// PORT OF: viacore.ts:324-331 (alarm_set_if_not_pending)
fn alarm_set_if_not_pending(ctx: &mut ViaContext, id: AlarmId, cpu_clk: u64) {
    if !alarm_is_pending(ctx, id) {
        ctx.alarm_context.alarm_set(id, cpu_clk);
    }
}

// PORT OF: viacore.ts:334-346 (schedule_t2_zero_alarm)
fn schedule_t2_zero_alarm(ctx: &mut ViaContext, rclk: u64) {
    ctx.t2zero = (rclk + ctx.t2cl as u64) & 0xffff_ffff;
    ctx.t2xx00 = true;
    ctx.alarm_context.alarm_unset(AlarmId::T2Underflow);
    ctx.alarm_context.alarm_set(AlarmId::T2Zero, ctx.t2zero);
}

// PORT OF: viacore.ts:349-376 (setup_shifting)
fn setup_shifting(ctx: &mut ViaContext, rclk: u64) {
    let acr = ctx.via[VIA_ACR];
    match acr & VIA_ACR_SR_CONTROL {
        VIA_ACR_SR_DISABLED => {
            // Do not change state — viacore.c:588
        }
        VIA_ACR_SR_IN_T2 | VIA_ACR_SR_OUT_T2 | VIA_ACR_SR_IN_CB1 | VIA_ACR_SR_OUT_CB1 => {
            if ctx.shift_state == FINISHED_SHIFTING {
                ctx.shift_state = START_SHIFTING;
            }
        }
        VIA_ACR_SR_IN_PHI2 | VIA_ACR_SR_OUT_PHI2 => {
            if ctx.shift_state == FINISHED_SHIFTING {
                ctx.shift_state = START_SHIFTING;
                ctx.alarm_context.alarm_set(AlarmId::Phi2Sr, (rclk + 1) & 0xffff_ffff);
            }
        }
        VIA_ACR_SR_OUT_FREE_T2 => {
            ctx.shift_state &= 0x0f;
        }
        _ => {}
    }
}

// PORT OF: viacore.ts:379-401 (set_cb2_output_state)
fn set_cb2_output_state(ctx: &mut ViaContext, backend: &mut dyn ViaBackend, pcr: u8, offset: u64) {
    let mode = pcr & VIA_PCR_CB2_CONTROL;
    if (mode & VIA_PCR_CB2_I_OR_O) == VIA_PCR_CB2_INPUT {
        ctx.cb2_out_state = true;
        backend.set_cb2(ctx, 1, offset);
    } else {
        match mode {
            VIA_PCR_CB2_LOW_OUTPUT => {
                ctx.cb2_out_state = false;
            }
            // VIA_PCR_CB2_HIGH_OUTPUT, PULSE_OUTPUT, HANDSHAKE_OUTPUT, default
            _ => {
                ctx.cb2_out_state = true;
            }
        }
        backend.set_cb2(ctx, if ctx.cb2_out_state { 1 } else { 0 }, offset);
    }
}

// =============================================================================
// viacore_disable / viacore_reset — viacore.ts:408-467
// =============================================================================

// PORT OF: viacore.ts:408-416 (viacore_disable)
pub fn viacore_disable(ctx: &mut ViaContext) {
    ctx.alarm_context.alarm_unset(AlarmId::T1Zero);
    ctx.alarm_context.alarm_unset(AlarmId::T2Zero);
    ctx.alarm_context.alarm_unset(AlarmId::T2Underflow);
    ctx.alarm_context.alarm_unset(AlarmId::T2Shift);
    ctx.alarm_context.alarm_unset(AlarmId::Phi2Sr);
    ctx.enabled = false;
}

// PORT OF: viacore.ts:419-467 (viacore_reset)
pub fn viacore_reset(ctx: &mut ViaContext, backend: &mut dyn ViaBackend) {
    // port data/ddr (viacore.c:382-385)
    for i in 0..4 {
        ctx.via[i] = 0;
    }
    // omit shift register (10) (viacore.c:392-395)
    for i in 11..16 {
        ctx.via[i] = 0;
    }

    ctx.tal = 0xffff;
    ctx.t2cl = 0xff;
    ctx.t2ch = 0xff;
    ctx.t1reload = ctx.clk;
    ctx.t2zero = ctx.clk;

    ctx.read_clk = 0;

    ctx.ier = 0;
    ctx.ifr = 0;

    ctx.t1_pb7 = 0x80;

    ctx.shift_state = FINISHED_SHIFTING;
    ctx.t2_irq_allowed = false;

    ctx.t1zero = 0;
    ctx.t2xx00 = false;

    ctx.alarm_context.alarm_unset(AlarmId::T1Zero);
    ctx.alarm_context.alarm_unset(AlarmId::T2Zero);
    ctx.alarm_context.alarm_unset(AlarmId::T2Underflow);
    ctx.alarm_context.alarm_unset(AlarmId::T2Shift);
    ctx.alarm_context.alarm_unset(AlarmId::Phi2Sr);

    update_myviairq(ctx, backend);

    ctx.oldpa = 0;
    ctx.oldpb = 0;

    ctx.ca2_out_state = true;
    ctx.cb1_out_state = true;
    ctx.cb2_out_state = true;
    backend.set_ca2(ctx, if ctx.ca2_out_state { 1 } else { 0 });
    backend.set_cb2(ctx, if ctx.cb2_out_state { 1 } else { 0 }, 0);

    backend.reset(ctx);

    viacore_cache_cb12_io_status(ctx, backend);

    ctx.enabled = true;
}

// =============================================================================
// viacore_signal — viacore.ts:474-509
// =============================================================================

// PORT OF: viacore.ts:474-509 (viacore_signal)
pub fn viacore_signal(ctx: &mut ViaContext, backend: &mut dyn ViaBackend, line: u8, edge: u8) {
    match line {
        VIA_SIG_CA1 => {
            if (if edge != 0 { 1u8 } else { 0u8 }) == (ctx.via[VIA_PCR] & VIA_PCR_CA1_CONTROL) {
                if is_ca2_toggle_mode(ctx) && !ctx.ca2_out_state {
                    ctx.ca2_out_state = true;
                    backend.set_ca2(ctx, if ctx.ca2_out_state { 1 } else { 0 });
                }
                ctx.ifr |= VIA_IM_CA1;
                update_myviairq(ctx, backend);
                // MYVIA_NEED_LATCHING block — viacore.c:452-456 — disabled in VICE
            }
        }
        VIA_SIG_CA2 => {
            if (ctx.via[VIA_PCR] & VIA_PCR_CA2_I_OR_O) == VIA_PCR_CA2_INPUT {
                ctx.ifr |= if ((((edge as u32) << 2) ^ (ctx.via[VIA_PCR] as u32)) & 0x04) != 0 {
                    0
                } else {
                    VIA_IM_CA2
                };
                update_myviairq(ctx, backend);
            }
        }
        VIA_SIG_CB1 => {
            viacore_set_cb1(ctx, backend, if edge != 0 { 1 } else { 0 });
        }
        VIA_SIG_CB2 => {
            viacore_set_cb2(ctx, backend, if edge != 0 { 1 } else { 0 });
        }
        _ => {}
    }
}

// =============================================================================
// viacore_store — viacore.ts:516-811
// =============================================================================

// PORT OF: viacore.ts:516-811 (viacore_store)
pub fn viacore_store(ctx: &mut ViaContext, backend: &mut dyn ViaBackend, addr: u16, byte: u8) {
    if ctx.rmw_flag != 0 {
        ctx.clk = ctx.clk.wrapping_sub(1) & 0xffff_ffff;
        ctx.rmw_flag = 0;
        let last_read = ctx.last_read;
        viacore_store(ctx, backend, addr, last_read);
        ctx.clk = ctx.clk.wrapping_add(1) & 0xffff_ffff;
    }

    // stores have a one-cycle offset if CLK++ happens before store
    let rclk = ctx.clk.wrapping_sub(ctx.write_offset) & 0xffff_ffff;

    let mut a = (addr & 0xf) as usize;

    if a == VIA_PRB || (a >= VIA_T1CL && a <= VIA_IER) {
        run_pending_alarms(ctx, backend, rclk, ctx.write_offset);
    }

    let v = byte & 0xff;

    match a {
        VIA_PRA => {
            ctx.ifr &= !VIA_IM_CA1;
            if !is_ca2_indinput(ctx) {
                ctx.ifr &= !VIA_IM_CA2;
            }
            if is_ca2_handshake(ctx) {
                ctx.ca2_out_state = false;
                backend.set_ca2(ctx, 0);
                if is_ca2_pulse_mode(ctx) {
                    ctx.ca2_out_state = true;
                    backend.set_ca2(ctx, 1);
                }
            }
            if ctx.ier & (VIA_IM_CA1 | VIA_IM_CA2) != 0 {
                update_myviairq_rclk(ctx, backend, rclk);
            }
            // fall through
            ctx.via[VIA_PRA_NHS] = v;
            a = VIA_PRA;
            // fall through
            ctx.via[a] = v;
            {
                let out = (ctx.via[VIA_PRA] | !ctx.via[VIA_DDRA]) & 0xff;
                backend.store_pra(ctx, out, ctx.oldpa, a);
                ctx.oldpa = out;
            }
        }
        VIA_PRA_NHS => {
            ctx.via[VIA_PRA_NHS] = v;
            a = VIA_PRA;
            ctx.via[a] = v;
            {
                let out = (ctx.via[VIA_PRA] | !ctx.via[VIA_DDRA]) & 0xff;
                backend.store_pra(ctx, out, ctx.oldpa, a);
                ctx.oldpa = out;
            }
        }
        VIA_DDRA => {
            ctx.via[a] = v;
            let out = (ctx.via[VIA_PRA] | !ctx.via[VIA_DDRA]) & 0xff;
            backend.store_pra(ctx, out, ctx.oldpa, a);
            ctx.oldpa = out;
        }

        VIA_PRB => {
            ctx.ifr &= !VIA_IM_CB1;
            if (ctx.via[VIA_PCR] & 0xa0) != 0x20 {
                ctx.ifr &= !VIA_IM_CB2;
            }
            if is_cb2_handshake(ctx) {
                ctx.cb2_out_state = false;
                backend.set_cb2(ctx, 0, ctx.write_offset);
                if is_cb2_pulse_mode(ctx) {
                    ctx.cb2_out_state = true;
                    backend.set_cb2(ctx, 1, 0);
                }
            }
            if ctx.ier & (VIA_IM_CB1 | VIA_IM_CB2) != 0 {
                update_myviairq_rclk(ctx, backend, rclk);
            }
            // fall through
            ctx.via[a] = v;
            {
                let mut out = (ctx.via[VIA_PRB] | !ctx.via[VIA_DDRB]) & 0xff;
                if ctx.via[VIA_ACR] & VIA_ACR_T1_PB7_USED != 0 {
                    out = ((out & 0x7f) | ctx.t1_pb7) & 0xff;
                }
                backend.store_prb(ctx, out, ctx.oldpb, a);
                ctx.oldpb = out;
            }
        }

        VIA_DDRB => {
            ctx.via[a] = v;
            let mut out = (ctx.via[VIA_PRB] | !ctx.via[VIA_DDRB]) & 0xff;
            if ctx.via[VIA_ACR] & VIA_ACR_T1_PB7_USED != 0 {
                out = ((out & 0x7f) | ctx.t1_pb7) & 0xff;
            }
            backend.store_prb(ctx, out, ctx.oldpb, a);
            ctx.oldpb = out;
        }

        VIA_SR => {
            ctx.via[a] = v;
            setup_shifting(ctx, rclk);
            if ctx.ifr & VIA_IM_SR != 0 {
                ctx.ifr &= !VIA_IM_SR;
                update_myviairq_rclk(ctx, backend, rclk);
            }
            backend.store_sr(ctx, v);
        }

        // Timers
        VIA_T1CL | VIA_T1LL => {
            ctx.via[VIA_T1LL] = v;
            update_via_t1_latch(ctx, rclk);
        }

        VIA_T1CH => {
            ctx.via[VIA_T1LH] = v;
            update_via_t1_latch(ctx, rclk);
            ctx.t1reload = (rclk + 1 + ctx.tal as u64 + FULL_CYCLE_2) & 0xffff_ffff;
            ctx.t1zero = (rclk + 1 + ctx.tal as u64) & 0xffff_ffff;
            ctx.alarm_context.alarm_set(AlarmId::T1Zero, ctx.t1zero);
            ctx.t1_pb7 = 0;
            ctx.ifr &= !VIA_IM_T1;
            update_myviairq_rclk(ctx, backend, rclk);
        }

        VIA_T1LH => {
            ctx.via[a] = v;
            update_via_t1_latch(ctx, rclk);
            ctx.ifr &= !VIA_IM_T1;
            update_myviairq_rclk(ctx, backend, rclk);
        }

        VIA_T2LL => {
            ctx.via[VIA_T2LL] = v;
            backend.store_t2l(ctx, v);
        }

        VIA_T2CH => {
            ctx.via[VIA_T2LH] = v;
            ctx.t2cl = ctx.via[VIA_T2LL] & 0xff;
            ctx.t2ch = v & 0xff;
            if ctx.via[VIA_ACR] & VIA_ACR_T2_COUNTPB6 == 0 {
                schedule_t2_zero_alarm(ctx, (rclk + 1) & 0xffff_ffff);
            }
            ctx.ifr &= !VIA_IM_T2;
            update_myviairq_rclk(ctx, backend, rclk);
            ctx.t2_irq_allowed = true;
        }

        VIA_IFR => {
            ctx.ifr &= !v;
            update_myviairq_rclk(ctx, backend, rclk);
        }

        VIA_IER => {
            if v & VIA_IM_IRQ != 0 {
                ctx.ier |= v & 0x7f;
            } else {
                ctx.ier &= !v;
            }
            update_myviairq_rclk(ctx, backend, rclk);
        }

        VIA_ACR => {
            let old_acr = ctx.via[VIA_ACR];
            // PB7 toggle bit rising edge (viacore.c:857-862)
            if (old_acr ^ v) & VIA_ACR_T1_PB7_USED != 0 {
                if v & VIA_ACR_T1_PB7_USED != 0 {
                    ctx.t1_pb7 = 0x80;
                }
            }

            let mut t2_startup_delay: u64 = 0;
            let mut restart_t2_alarms: i32 = 0;

            // T2 mode change (viacore.c:889-925)
            if (old_acr ^ v) & VIA_ACR_T2_CONTROL != 0 {
                if v & VIA_ACR_T2_COUNTPB6 != 0 {
                    let stop = (viacore_t2(ctx, rclk).wrapping_sub(1)) & 0xffff;
                    ctx.t2cl = (stop & 0xff) as u8;
                    ctx.t2ch = ((stop >> 8) & 0xff) as u8;
                    ctx.alarm_context.alarm_unset(AlarmId::T2Zero);
                    ctx.t2xx00 = false;
                } else {
                    restart_t2_alarms += 1;
                    t2_startup_delay += 1;
                }
            }

            // SR mode change (viacore.c:928-966)
            match v & VIA_ACR_SR_CONTROL {
                VIA_ACR_SR_DISABLED => {
                    ctx.alarm_context.alarm_unset(AlarmId::Phi2Sr);
                    if ctx.ifr & VIA_IM_SR != 0 {
                        ctx.ifr &= !VIA_IM_SR;
                        update_myviairq_rclk(ctx, backend, rclk);
                    }
                    set_cb2_output_state(ctx, backend, ctx.via[VIA_PCR], ctx.write_offset);
                }
                VIA_ACR_SR_IN_T2 | VIA_ACR_SR_OUT_T2 | VIA_ACR_SR_OUT_FREE_T2 => {
                    ctx.alarm_context.alarm_unset(AlarmId::Phi2Sr);
                    restart_t2_alarms = if restart_t2_alarms != 0 {
                        restart_t2_alarms
                    } else if !is_sr_t2_controlled(ctx.via[VIA_ACR]) && is_t2_timer(v) {
                        1
                    } else {
                        0
                    };
                }
                VIA_ACR_SR_IN_PHI2 | VIA_ACR_SR_OUT_PHI2 => {
                    alarm_set_if_not_pending(
                        ctx,
                        AlarmId::Phi2Sr,
                        (rclk + SR_PHI2_FIRST_OFFSET) & 0xffff_ffff,
                    );
                }
                VIA_ACR_SR_IN_CB1 | VIA_ACR_SR_OUT_CB1 => {
                    ctx.alarm_context.alarm_unset(AlarmId::Phi2Sr);
                }
                _ => {}
            }

            if restart_t2_alarms != 0
                && !alarm_is_pending(ctx, AlarmId::T2Zero)
                && !alarm_is_pending(ctx, AlarmId::T2Underflow)
            {
                let current = viacore_t2(ctx, rclk);
                ctx.t2cl = (current & 0xff) as u8;
                ctx.t2ch = ((current >> 8) & 0xff) as u8;
                schedule_t2_zero_alarm(ctx, (rclk + t2_startup_delay) & 0xffff_ffff);
            }

            ctx.via[a] = v;
            viacore_cache_cb12_io_status(ctx, backend);
            backend.store_acr(ctx, v);
        }

        VIA_PCR => {
            let mut v = v;
            if (v & VIA_PCR_CA2_CONTROL) == VIA_PCR_CA2_LOW_OUTPUT {
                ctx.ca2_out_state = false;
            } else if (v & VIA_PCR_CA2_CONTROL) == VIA_PCR_CA2_HIGH_OUTPUT {
                ctx.ca2_out_state = true;
            } else {
                ctx.ca2_out_state = true;
            }
            backend.set_ca2(ctx, if ctx.ca2_out_state { 1 } else { 0 });

            if (ctx.via[VIA_ACR] & VIA_ACR_SR_CONTROL) == VIA_ACR_SR_DISABLED {
                set_cb2_output_state(ctx, backend, v, ctx.write_offset);
            }

            let ret = backend.store_pcr(ctx, v, a);
            if let Some(r) = ret {
                v = r & 0xff;
            }

            ctx.via[a] = v;
            viacore_cache_cb12_io_status(ctx, backend);
        }

        _ => {
            ctx.via[a] = v;
        }
    }
}

// =============================================================================
// viacore_read / viacore_peek — viacore.ts:818-974
// =============================================================================

// PORT OF: viacore.ts:818-919 (viacore_read)
pub fn viacore_read(ctx: &mut ViaContext, backend: &mut dyn ViaBackend, addr: u16) -> u8 {
    let a = (addr & 0xf) as usize;
    ctx.read_clk = ctx.clk;
    ctx.read_offset = 0;
    let rclk = ctx.clk;

    if a == VIA_PRB || (a >= VIA_T1CL && a <= VIA_IER) {
        run_pending_alarms(ctx, backend, rclk, 0);
    }

    match a {
        VIA_PRA => {
            ctx.ifr &= !VIA_IM_CA1;
            if (ctx.via[VIA_PCR] & 0x0a) != 0x02 {
                ctx.ifr &= !VIA_IM_CA2;
            }
            if is_ca2_handshake(ctx) {
                ctx.ca2_out_state = false;
                backend.set_ca2(ctx, 0);
                if is_ca2_pulse_mode(ctx) {
                    ctx.ca2_out_state = true;
                    backend.set_ca2(ctx, 1);
                }
            }
            if ctx.ier & (VIA_IM_CA1 | VIA_IM_CA2) != 0 {
                update_myviairq_rclk(ctx, backend, rclk);
            }
            let byte = backend.read_pra(ctx, a).unwrap_or(0xff) & 0xff;
            ctx.last_read = byte;
            byte
        }
        VIA_PRA_NHS => {
            let byte = backend.read_pra(ctx, a).unwrap_or(0xff) & 0xff;
            ctx.last_read = byte;
            byte
        }

        VIA_PRB => {
            ctx.ifr &= !VIA_IM_CB1;
            if (ctx.via[VIA_PCR] & 0xa0) != 0x20 {
                ctx.ifr &= !VIA_IM_CB2;
            }
            if ctx.ier & (VIA_IM_CB1 | VIA_IM_CB2) != 0 {
                update_myviairq_rclk(ctx, backend, rclk);
            }
            let pin = backend.read_prb(ctx).unwrap_or(0xff) & 0xff;
            let mut byte =
                ((pin & !ctx.via[VIA_DDRB]) | (ctx.via[VIA_PRB] & ctx.via[VIA_DDRB])) & 0xff;
            if ctx.via[VIA_ACR] & VIA_ACR_T1_PB7_USED != 0 {
                byte = ((byte & 0x7f) | ctx.t1_pb7) & 0xff;
            }
            ctx.last_read = byte;
            byte
        }

        VIA_T1CL => {
            ctx.ifr &= !VIA_IM_T1;
            update_myviairq_rclk(ctx, backend, rclk);
            ctx.last_read = (viacore_t1(ctx, rclk) & 0xff) as u8;
            ctx.last_read
        }
        VIA_T1CH => {
            ctx.last_read = ((viacore_t1(ctx, rclk) >> 8) & 0xff) as u8;
            ctx.last_read
        }

        VIA_T2CL => {
            ctx.ifr &= !VIA_IM_T2;
            update_myviairq_rclk(ctx, backend, rclk);
            ctx.last_read = (viacore_t2(ctx, rclk) & 0xff) as u8;
            ctx.last_read
        }
        VIA_T2CH => {
            ctx.last_read = ((viacore_t2(ctx, rclk) >> 8) & 0xff) as u8;
            ctx.last_read
        }

        VIA_SR => {
            setup_shifting(ctx, rclk);
            if ctx.ifr & VIA_IM_SR != 0 {
                ctx.ifr &= !VIA_IM_SR;
                update_myviairq_rclk(ctx, backend, rclk);
            }
            ctx.last_read = ctx.via[a];
            ctx.last_read
        }

        VIA_IFR => {
            let mut t = ctx.ifr & 0xff;
            if ctx.ifr & ctx.ier != 0 {
                t |= 0x80;
            } else {
                t &= !0x80;
            }
            ctx.last_read = t & 0xff;
            ctx.last_read
        }

        VIA_IER => {
            ctx.last_read = (ctx.ier | 0x80) & 0xff;
            ctx.last_read
        }

        _ => {
            ctx.last_read = ctx.via[a];
            ctx.via[a]
        }
    }
}

// PORT OF: viacore.ts:926-928 (viacore_read_ — MYVIA_TIMER_DEBUG alias)
pub fn viacore_read_(ctx: &mut ViaContext, backend: &mut dyn ViaBackend, addr: u16) -> u8 {
    viacore_read(ctx, backend, addr)
}

// PORT OF: viacore.ts:931-974 (viacore_peek)
pub fn viacore_peek(ctx: &mut ViaContext, backend: &mut dyn ViaBackend, addr: u16) -> u8 {
    let a = (addr & 0xf) as usize;
    match a {
        VIA_PRA | VIA_PRA_NHS => backend.read_pra(ctx, a).unwrap_or(0xff) & 0xff,
        VIA_PRB => {
            let pin = backend.read_prb(ctx).unwrap_or(0xff) & 0xff;
            let mut byte =
                ((pin & !ctx.via[VIA_DDRB]) | (ctx.via[VIA_PRB] & ctx.via[VIA_DDRB])) & 0xff;
            if ctx.via[VIA_ACR] & VIA_ACR_T1_PB7_USED != 0 {
                byte = ((byte & 0x7f) | ctx.t1_pb7) & 0xff;
            }
            byte
        }
        VIA_DDRA | VIA_DDRB => ctx.via[a],
        VIA_T1CL => (viacore_t1(ctx, ctx.clk) & 0xff) as u8,
        VIA_T1CH => ((viacore_t1(ctx, ctx.clk) >> 8) & 0xff) as u8,
        VIA_T1LL | VIA_T1LH => ctx.via[a],
        VIA_T2CL => (viacore_t2(ctx, ctx.clk) & 0xff) as u8,
        VIA_T2CH => ((viacore_t2(ctx, ctx.clk) >> 8) & 0xff) as u8,
        VIA_IFR => ctx.ifr & 0xff,
        VIA_IER => (ctx.ier | 0x80) & 0xff,
        _ => ctx.via[a],
    }
}

// =============================================================================
// viacore_set_cb1 / viacore_set_cb2 / viacore_set_sr — viacore.ts:981-1047
// =============================================================================

// PORT OF: viacore.ts:981-1019 (viacore_set_cb1)
pub fn viacore_set_cb1(ctx: &mut ViaContext, backend: &mut dyn ViaBackend, data: u32) {
    let data_bool = data != 0;
    if data_bool != ctx.cb1_in_state {
        if ctx.cb1_is_input {
            if !data_bool && ctx.shift_state == FINISHED_SHIFTING {
                ctx.shift_state = START_SHIFTING;
            }
            ctx.shift_state += 1;
            if data_bool {
                ctx.via[VIA_SR] =
                    ((ctx.via[VIA_SR] << 1) | if ctx.cb2_in_state { 1 } else { 0 }) & 0xff;
                if ctx.shift_state == FINISHED_SHIFTING {
                    viacore_set_sr(ctx, backend, ctx.via[VIA_SR]);
                    ctx.shift_state = START_SHIFTING;
                }
            }
        }
        ctx.cb1_in_state = data_bool;
    }

    let pcr = ctx.via[VIA_PCR];
    let edge = (pcr & VIA_PCR_CB1_CONTROL) == VIA_PCR_CB1_POS_ACTIVE_EDGE;
    if data_bool == edge {
        if is_cb2_toggle_mode(ctx) && !ctx.cb2_out_state {
            ctx.cb2_out_state = true;
            backend.set_cb2(ctx, 1, 0);
        }
        ctx.ifr |= VIA_IM_CB1;
        update_myviairq(ctx, backend);
        // MYVIA_NEED_LATCHING viacore.c:1494-1498 — disabled in VICE
        if is_pb_input_latch(ctx) {
            ctx.ilb = backend.read_prb(ctx).unwrap_or(0xff) & 0xff;
        }
    }
}

// PORT OF: viacore.ts:1022-1034 (viacore_set_cb2)
pub fn viacore_set_cb2(ctx: &mut ViaContext, backend: &mut dyn ViaBackend, data: u32) {
    let data_bool = data != 0;
    if ctx.cb2_is_input && data_bool != ctx.cb2_in_state {
        ctx.cb2_in_state = data_bool;
        let pcr = ctx.via[VIA_PCR];
        // viacore.c:1510 — edge = (pcr & VIA_PCR_CB2_INPUT_POS_ACTIVE_EDGE) != 0
        let edge = (pcr & 0x40) != 0;
        if data_bool == edge {
            ctx.ifr |= VIA_IM_CB2;
            update_myviairq(ctx, backend);
        }
    }
}

// PORT OF: viacore.ts:1037-1047 (viacore_set_sr)
pub fn viacore_set_sr(ctx: &mut ViaContext, backend: &mut dyn ViaBackend, data: u8) {
    if (ctx.via[VIA_ACR] & VIA_ACR_SR_OUT) == 0 && (ctx.via[VIA_ACR] & 0x0c) != 0 {
        ctx.via[VIA_SR] = data & 0xff;
        ctx.ifr |= VIA_IM_SR;
        update_myviairq(ctx, backend);
        ctx.shift_state = FINISHED_SHIFTING;
    }
}

// =============================================================================
// Alarm callbacks — viacore.ts:1054-1219
// =============================================================================

// PORT OF: viacore.ts:1054-1079 (viacore_t1_zero_alarm)
pub fn viacore_t1_zero_alarm(ctx: &mut ViaContext, backend: &mut dyn ViaBackend, offset: u64) {
    let rclk = ctx.clk.wrapping_sub(offset) & 0xffff_ffff;

    if ctx.via[VIA_ACR] & VIA_ACR_T1_FREE_RUN == 0 {
        // one-shot
        ctx.alarm_context.alarm_unset(AlarmId::T1Zero);
        ctx.t1zero = 0;
    } else {
        // continuous
        let full_cycle = ctx.tal as u64 + FULL_CYCLE_2;
        ctx.t1zero = (ctx.t1zero + full_cycle) & 0xffff_ffff;
        ctx.alarm_context.alarm_set(AlarmId::T1Zero, ctx.t1zero);
    }

    ctx.t1_pb7 ^= 0x80;
    ctx.ifr |= VIA_IM_T1;
    update_myviairq_rclk(ctx, backend, (rclk + 1) & 0xffff_ffff);
}

// PORT OF: viacore.ts:1082-1104 (viacore_t2_zero_alarm)
pub fn viacore_t2_zero_alarm(ctx: &mut ViaContext, backend: &mut dyn ViaBackend, offset: u64) {
    let rclk = ctx.clk.wrapping_sub(offset) & 0xffff_ffff;

    // T2 low underflow always decreases T2 high
    ctx.t2ch = ctx.t2ch.wrapping_sub(1) & 0xff;

    if ctx.t2ch == 0xff && ctx.t2_irq_allowed {
        ctx.ifr |= VIA_IM_T2;
        update_myviairq_rclk(ctx, backend, rclk);
        ctx.t2_irq_allowed = false;
    }

    ctx.alarm_context.alarm_unset(AlarmId::T2Zero);
    ctx.alarm_context
        .alarm_set(AlarmId::T2Underflow, (rclk + 1) & 0xffff_ffff);
}

// PORT OF: viacore.ts:1107-1149 (viacore_t2_underflow_alarm)
pub fn viacore_t2_underflow_alarm(ctx: &mut ViaContext, _backend: &mut dyn ViaBackend, offset: u64) {
    let rclk = ctx.clk.wrapping_sub(offset) & 0xffff_ffff;
    // TS: `let next_alarm = 0;` then every branch reassigns (viacore.ts:1113).
    #[allow(unused_assignments)]
    let mut next_alarm: u64 = 0;

    if (ctx.via[VIA_ACR] & 0x0c) == 0x04 {
        // 8-bit timer (SR-controlled)
        ctx.t2cl = ctx.via[VIA_T2LL] & 0xff;
        next_alarm = ctx.via[VIA_T2LL] as u64 + FULL_CYCLE_2;
        ctx.alarm_context
            .alarm_set(AlarmId::T2Shift, (rclk + 1) & 0xffff_ffff);
    } else if is_sr_free_running(ctx) {
        ctx.t2cl = ctx.via[VIA_T2LL] & 0xff;
        next_alarm = ctx.via[VIA_T2LL] as u64 + FULL_CYCLE_2;
        ctx.alarm_context
            .alarm_set(AlarmId::T2Shift, (rclk + 1) & 0xffff_ffff);
    } else {
        // 16-bit timer mode
        ctx.t2cl = 0xff;
        next_alarm = if ctx.t2ch != 0xff { 256 } else { 0 };
    }

    if next_alarm != 0 {
        ctx.t2zero = (ctx.t2zero + next_alarm) & 0xffff_ffff;
        ctx.t2xx00 = true;
        ctx.alarm_context.alarm_set(AlarmId::T2Zero, ctx.t2zero);
    } else {
        ctx.alarm_context.alarm_unset(AlarmId::T2Zero);
        ctx.t2xx00 = false;
    }
    ctx.alarm_context.alarm_unset(AlarmId::T2Underflow);
}

// PORT OF: viacore.ts:1152-1191 (do_shiftregister)
fn do_shiftregister(ctx: &mut ViaContext, backend: &mut dyn ViaBackend, offset: u64) {
    let rclk = ctx.clk.wrapping_sub(offset) & 0xffff_ffff;
    if ctx.shift_state >= FINISHED_SHIFTING {
        return;
    }

    let acr = ctx.via[VIA_ACR];
    let shift_out = (acr & VIA_ACR_SR_OUT) != 0;

    if (ctx.shift_state & 1) == 0 {
        // even: CB1 low (in shift-out modes)
        if !ctx.cb1_is_input {
            backend.set_cb1(ctx, 0);
        }
        if shift_out {
            let cb2 = ((ctx.via[VIA_SR] >> 7) & 1) as u32;
            ctx.via[VIA_SR] = ((ctx.via[VIA_SR] << 1) | (cb2 as u8)) & 0xff;
            ctx.cb2_out_state = cb2 != 0;
            backend.set_cb2(ctx, cb2, offset);
        }
    } else {
        // odd: CB1 high
        if !ctx.cb1_is_input {
            backend.set_cb1(ctx, 1);
        }
        if !shift_out {
            ctx.via[VIA_SR] =
                ((ctx.via[VIA_SR] << 1) | if ctx.cb2_in_state { 1 } else { 0 }) & 0xff;
        }
    }

    ctx.shift_state += 1;
    if ctx.shift_state == FINISHED_SHIFTING {
        if is_sr_free_running(ctx) {
            ctx.shift_state = START_SHIFTING;
        } else {
            ctx.ifr |= VIA_IM_SR;
            update_myviairq_rclk(ctx, backend, rclk);
            backend.sr_underflow(ctx);
        }
    }
}

// PORT OF: viacore.ts:1194-1203 (viacore_t2_shift_alarm)
pub fn viacore_t2_shift_alarm(ctx: &mut ViaContext, backend: &mut dyn ViaBackend, offset: u64) {
    do_shiftregister(ctx, backend, offset);
    ctx.alarm_context.alarm_unset(AlarmId::T2Shift);
}

// PORT OF: viacore.ts:1206-1219 (viacore_phi2_sr_alarm)
pub fn viacore_phi2_sr_alarm(ctx: &mut ViaContext, backend: &mut dyn ViaBackend, offset: u64) {
    let rclk = ctx.clk.wrapping_sub(offset) & 0xffff_ffff;
    do_shiftregister(ctx, backend, offset);
    ctx.alarm_context
        .alarm_set(AlarmId::Phi2Sr, (rclk + SR_PHI2_NEXT_OFFSET) & 0xffff_ffff);
}

// =============================================================================
// viacore_cache_cb12_io_status — viacore.ts:1226-1251
// =============================================================================

// PORT OF: viacore.ts:1226-1251 (viacore_cache_cb12_io_status)
pub fn viacore_cache_cb12_io_status(ctx: &mut ViaContext, backend: &mut dyn ViaBackend) {
    let acr = ctx.via[VIA_ACR];
    let pcr = ctx.via[VIA_PCR];

    let cb1_drives_shifting = (acr & VIA_ACR_SR_CONTROL & 0x0c) == VIA_ACR_SR_IN_CB1
        || (acr & VIA_ACR_SR_CONTROL) == VIA_ACR_SR_DISABLED;

    // VIA_ACR_SR_IN === 0x00 per via.h:80
    let sr_is_input =
        (acr & VIA_ACR_SR_OUT) == 0x00 && (acr & VIA_ACR_SR_CONTROL) != VIA_ACR_SR_DISABLED;

    let cb2_is_input = (pcr & VIA_PCR_CB2_I_OR_O) == VIA_PCR_CB2_INPUT;

    ctx.cb1_is_input = cb1_drives_shifting;
    ctx.cb2_is_input = sr_is_input || cb2_is_input;

    if backend.has_set_cb1() && !ctx.cb1_is_input && ctx.shift_state == FINISHED_SHIFTING {
        backend.set_cb1(ctx, 1);
    }
}

// =============================================================================
// viacore_setup_context / viacore_init — viacore.ts:1258-1340
// =============================================================================

// PORT OF: viacore.ts:1258-1287 (viacore_setup_context)
pub fn viacore_setup_context(ctx: &mut ViaContext) {
    ctx.read_clk = 0;
    ctx.read_offset = 0;
    ctx.last_read = 0;
    ctx.log = 0; // LOG_DEFAULT

    ctx.my_module_name_alt1 = None;
    ctx.my_module_name_alt2 = None;

    ctx.write_offset = 1;

    // assume all registers 0 at powerup (viacore.c:1843-1845)
    for i in 0..16 {
        ctx.via[i] = 0;
    }

    // timers and timer latches not 0 at powerup (viacore.c:1847-1850)
    ctx.via[4] = 0xff;
    ctx.via[6] = 0xff;
    ctx.via[5] = 223;
    ctx.via[7] = 223;
    ctx.via[8] = 0xff;
    ctx.via[9] = 0xff;

    // Not internal but external state, not set on reset (viacore.c:1853-1854)
    ctx.cb1_in_state = true;
    ctx.cb2_in_state = true;

    // ctx.sr_underflow = null; ctx.set_cb1 = null; — handled by the backend trait
    ctx.t2_irq_allowed = false;
}

// PORT OF: viacore.ts:1290-1340 (viacore_init). Allocates the 5 alarms; in this
// port the AlarmContext already owns them, so this just marks them inited and
// sets int_num (the TODO_PORT int_num=0 path).
pub fn viacore_init(ctx: &mut ViaContext) {
    // The 5 alarms (T1zero/T2zero/T2uflow/T2SR/SR) already live on
    // ctx.alarm_context.alarms[]; nothing to allocate. Mark inited.
    ctx.alarms_inited = true;
    // viacore.c:1892 — int_num. Backend keys off ctx.int_num (via2 sets it).
}

// =============================================================================
// VIA2 disk-controller backend — STRICT 1:1 port of via2d.ts
// =============================================================================
//
// PORT OF: src/runtime/headless/vice1541/via2d.ts (vice/src/drive/iecieee/via2d.c).
// The TS installs 19 callback fns onto via_context_t; here the same functions are
// the methods of [`Via2dBackend`], which carries the `drive_t` state the TS reaches
// via `ctx.prv.drive` (= the rotation model) and `ctx.context` (= the IntStatus for
// set_int). One method per VICE `static` fn, same name, same body.

use crate::drive_6510core::IntStatus;
use crate::rotation::{Rotation, BRA_MOTOR_ON};

// PORT OF: via2d.ts:196-197 (DRIVE_SOUND_MOTOR_ON / DRIVE_SOUND_MOTOR_OFF)
const DRIVE_SOUND_MOTOR_ON: u32 = 1;
const DRIVE_SOUND_MOTOR_OFF: u32 = 0;

/// VIA2 backend (= `drive_t` + diskunit context the via2d hooks reach). Holds the
/// rotation model (`ctx.prv.drive`), the drive number, the IntStatus pointer
/// (`ctx.context.cpu.int_status`), and the pending set-overflow latch
/// (`drive_cpu_set_overflow` flush). Built per drive-bus run.
pub struct Via2dBackend<'a> {
    /// `ctx.prv.drive` — the rotating GCR disk model. `None` ⇒ no disk mounted
    /// (the TS keeps `drv` live but pre-mount our rotation has `image == None`;
    /// the hooks early-return like VICE's null-guarded TS port when no image).
    pub drive: &'a mut Rotation,
    /// `via2p.number` — the drive unit number (speed-zone set arg).
    pub number: usize,
    /// `ctx.context.cpu.int_status` — set_int target (interrupt_set_irq).
    pub int: &'a mut IntStatus,
    /// drive_cpu_set_overflow latch — folded into the drive 6502 V flag by the
    /// run loop after the store completes (the bus borrow can't reach `cpu`).
    pub pending_set_overflow: bool,
    /// Whether a disk image is mounted (drives the TS `if (!drv) return` guard —
    /// here keyed on image presence, matching the distilled port's behaviour).
    pub has_image: bool,
}

impl<'a> Via2dBackend<'a> {
    /// drive_writeprotect_sense (via2d.ts:159-164 → drive.ts) — returns true if
    /// writeable. The rotation model returns 0x10 (writeable) / 0x00 (protected).
    #[inline]
    fn drive_writeprotect_sense(&mut self, clk: u64) -> bool {
        self.drive.writeprotect_sense(clk) != 0
    }
}

impl<'a> ViaBackend for Via2dBackend<'a> {
    // PORT OF: via2d.ts:207-222 (set_ca2)
    fn set_ca2(&mut self, ctx: &ViaContext, state: u32) {
        if !self.has_image {
            return; // TS-only: pre-mount; VICE always has drv live.
        }
        let drv = &mut *self.drive;
        let curr = ((drv.byte_ready_active >> 1) & 1) as u32;
        if state != curr {
            drv.rotate_disk(ctx.clk);
            drv.byte_ready_active &= !(1 << 1);
            drv.byte_ready_active |= (state as u8) << 1;
            if drv.byte_ready_edge != 0 {
                // drive_cpu_set_overflow(dc)
                self.pending_set_overflow = true;
                drv.byte_ready_edge = 0;
            }
        }
    }

    // PORT OF: via2d.ts:225-234 (set_cb2)
    fn set_cb2(&mut self, ctx: &ViaContext, state: u32, _offset: u64) {
        if !self.has_image {
            return;
        }
        let drv = &mut *self.drive;
        let curr = ((drv.read_write_mode as u8) & 1) as u32; // (read_write_mode>>5)&1 → bool
        if state != curr {
            drv.rotate_disk(ctx.clk);
            drv.read_write_mode = (state << 5) != 0;
        }
    }

    // PORT OF: via2d.ts:241-258 (set_int)
    fn set_int(&mut self, ctx: &ViaContext, _int_num: u32, value: u32, rclk: u64) {
        // VICE: interrupt_set_irq(dc->cpu->int_status, int_num, value, rclk).
        // The drive's VIA2 is int_num 1 (VIA1 = 0). The IntStatus per-source
        // index is fixed by the drive wire-up, so use ctx.int_num.
        self.int.set_irq(ctx.int_num as usize, value != 0, rclk);
    }

    // PORT OF: via2d.ts:265-275 (restore_int) — no-op for headless.
    fn restore_int(&mut self, _ctx: &ViaContext, _int_num: u32, _value: u32) {}

    // PORT OF: via2d.ts:355-368 (store_pra)
    fn store_pra(&mut self, ctx: &mut ViaContext, byte: u8, _oldpa: u8, _addr: usize) {
        if !self.has_image {
            return;
        }
        let drv = &mut *self.drive;
        drv.rotate_disk(ctx.clk);
        // VICE: GCR_write_value = byte. The Rust rotation has no write-value field
        // (D64 write path is out of scope here); the write value is unused on the
        // read-only LOAD path, so this is a no-op (NOT folded into gcr_read, which
        // would corrupt the read byte).
        let _ = byte;
        drv.byte_ready_level = 0;
    }

    // PORT OF: via2d.ts:382-487 (store_prb)
    fn store_prb(&mut self, ctx: &mut ViaContext, byte: u8, poldpb: u8, _addr: usize) {
        if !self.has_image {
            return;
        }
        let byte = byte & 0xff;
        let poldpb = poldpb & 0xff;

        // via2d.c:210 — rotation_rotate_disk(drv)
        let clk = ctx.clk;
        self.drive.rotate_disk(clk);

        // via2d.c:212-217 — LED status (PB.3) — headless: rotation has no led
        // fields; the LED is observation-only and has no behavioural impact on
        // the LOAD path. Omitted (matches the distilled port).

        // via2d.c:219-249 — stepper formula from current_half_track.
        let track_number = self.drive.current_half_track.wrapping_sub(2);
        let new_stepper_position = (byte & 3) as i32;
        let old_stepper_position = (track_number & 3) as i32;
        let mut step_count = (new_stepper_position - old_stepper_position) & 3;
        if step_count == 3 {
            step_count = -1;
        }

        // via2d.c:255-313 — process stepper motor if the drive motor is on.
        if byte & 0x4 != 0 {
            // via2d.c:307 — (step_count==1)||(step_count==-1) gate at this FIRST
            // call site only.
            if step_count == 1 || step_count == -1 {
                self.drive.move_head(step_count);
            }
        }

        // via2d.c:321-323 — zone bits ((poldpb ^ byte) & 0x60) changed.
        if (poldpb ^ byte) & 0x60 != 0 {
            self.drive.speed_zone_set(((byte >> 5) & 0x3) as usize);
        }

        // via2d.c:324 — #define PB_MOTOR_ON BRA_MOTOR_ON
        let pb_motor_on = BRA_MOTOR_ON;

        // via2d.c:325-352 — motor on/off edge handling.
        if (poldpb ^ byte) & pb_motor_on != 0 {
            // drive_sound_update — no-op headless.
            let _ = if byte & 4 != 0 {
                DRIVE_SOUND_MOTOR_ON
            } else {
                DRIVE_SOUND_MOTOR_OFF
            };
            let bra = self.drive.byte_ready_active;
            self.drive.byte_ready_active = (bra & !BRA_MOTOR_ON) | (byte & BRA_MOTOR_ON);
            if (byte & BRA_MOTOR_ON) != 0 {
                self.drive.begins(clk);
            } else {
                if self.drive.byte_ready_edge != 0 {
                    // drive_cpu_set_overflow(dc)
                    self.pending_set_overflow = true;
                    self.drive.byte_ready_edge = 0;
                }
            }

            // via2d.c:338-351 (bug #1083 "Primitive 7 Sins" workaround). On a
            // motor-on edge, if the stepper position changed and motor is now on,
            // call drive_move_head a SECOND time WITHOUT the ±1 gate.
            if new_stepper_position != old_stepper_position {
                if (byte & 0x04) != 0 {
                    self.drive.move_head(step_count);
                }
            }
        }

        // via2d.c:354 — byte_ready_level = 0 last.
        self.drive.byte_ready_level = 0;
    }

    // PORT OF: via2d.ts:507-511 (store_pcr) — OLDCODE=0 pass-through.
    fn store_pcr(&mut self, ctx: &mut ViaContext, byte: u8, _addr: usize) -> Option<u8> {
        if self.has_image {
            self.drive.rotate_disk(ctx.clk);
        }
        Some(byte & 0xff)
    }

    // PORT OF: via2d.ts:525-527 (store_acr) — empty.
    fn store_acr(&mut self, _ctx: &mut ViaContext, _byte: u8) {}
    // PORT OF: via2d.ts:530-532 (store_sr) — empty.
    fn store_sr(&mut self, _ctx: &mut ViaContext, _byte: u8) {}
    // PORT OF: via2d.ts:535-537 (store_t2l) — empty.
    fn store_t2l(&mut self, _ctx: &mut ViaContext, _byte: u8) {}

    // PORT OF: via2d.ts:563-576 (read_pra)
    fn read_pra(&mut self, ctx: &ViaContext, _addr: usize) -> Option<u8> {
        if !self.has_image {
            return None; // VICE drv always live; no image → 0xff (None).
        }
        // IF: add bus read delay — req_ref_cycles has no effect in the simple D64
        // engine (omitted, as in the distilled port).
        self.drive.byte_read(ctx.clk);
        // VICE: byte = ((GCR_read & ~DDRA) | (PRA & DDRA));
        let ddra = ctx.via[VIA_DDRA];
        let pra = ctx.via[VIA_PRA];
        let byte = ((self.drive.pra_pin() & !ddra) | (pra & ddra)) & 0xff;
        self.drive.byte_ready_level = 0;
        Some(byte)
    }

    // PORT OF: via2d.ts:588-604 (read_prb)
    fn read_prb(&mut self, ctx: &ViaContext) -> Option<u8> {
        if !self.has_image {
            return None;
        }
        let clk = ctx.clk;
        self.drive.rotate_disk(clk);
        let sync = self.drive.sync_found(); // already 0 or 0x80
        let wps = if self.drive_writeprotect_sense(clk) { 0x10 } else { 0 };
        let ddrb = ctx.via[VIA_DDRB];
        let prb = ctx.via[VIA_PRB];
        let byte = (((sync | wps | 0x6f) & !ddrb) | (prb & ddrb)) & 0xff;
        self.drive.byte_ready_level = 0;
        Some(byte)
    }

    // PORT OF: via2d.ts:545-551 (reset) — LED on; UI update (no-op headless).
    fn reset(&mut self, _ctx: &mut ViaContext) {
        // drv.led_status = 1; drive_update_ui_status() — observation-only.
    }
}
