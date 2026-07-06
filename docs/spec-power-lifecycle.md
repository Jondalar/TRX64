# Spec 786 — Power Lifecycle: 3 guarded primitives, everything composes

**Status:** building · **Repo:** TRX64 core + daemon + cli + monitor; C64RE UI wiring
**Board:** `C64ReverseEngineeringMCP/specs/README.md` (#786)

## Problem

`reset` / `power` / `eject` / `insert` each had their own ad-hoc state
mutation. The cold power-cycle path (`session/reset {cold}`,
`/power on|off`, `media/mount crt`, `media/unmount cart`) called
`fill_power_on_ram() + cold_reset()`, but **`cold_reset` does NOT reset
the I/O chips** (VIC, CIA1, CIA2) — only `warm_reset` does. So a game's
armed raster-IRQ / CIA-timer / bitmap mode survived every "power cycle"
and re-hijacked the fresh KERNAL boot → garbage screen / no cursor /
"CRT jammed after reset". Additional drift: cold path dropped the
mounted disk (`drive8.cold_reset()` → `disk=None`); `/reset` defaulted
to cold (RAM wipe) instead of a warm RESET; monitor `reset` used a
separate `session.boot` path (reloads ROMs, drops disk).

Root fact: **fresh VIC/CIA chips only come from `Machine::new()`.**
`boot_from_dir` cold-resets an *existing* machine, so it cannot clear
stale chip state. Therefore "power on = full init" must **rebuild** the
machine, not re-boot it in place.

## Model — 3 primitives, guarded by a `powered` flag

`trx64_session::Session` gains `powered: bool` and a media registry that
survives a power-off (physical media persist: EasyFlash flash + disk
writes are transplanted, not rebuilt from bytes).

1. **`power_on(roms)`** — precondition `!powered`. `Machine::new()` +
   `boot_from_dir(roms)` (= byte-identical to daemon startup), then
   re-attach whatever media is registered (cart mapper + image
   transplanted → `cold_reset` re-vectors $FFFC through it; disk image
   re-attached). `powered = true`, `running = true`. No-op if already on.
2. **`power_off()`** — precondition `powered`. Flush disk writeback,
   move live media into the registry (flash/disk state intact), blank
   the machine (`Machine::new()`, no ROMs → truly dead), `running =
   false`, `powered = false`. No-op if already off.
3. **`warm_reset()`** — HW RESET line: `machine.warm_reset()` ($FCE2 via
   $FFFC, RAM + media preserved, VIC/CIA/CPU/IEC reset). No-op if off.

### Compositions (nothing else is bespoke)

| Action | = |
|---|---|
| Neustart / power on | `power_on` |
| Ausmachen | `power_off` |
| reset warm | `warm_reset` |
| reset cold | `power_off` → `power_on` |
| eject CART | `power_off` → registry drop cart → `power_on` |
| insert CART | `power_off` → registry set cart → `power_on` |
| monitor `reset` / `power` | the same primitives |

`power_on` re-attaches **whatever is registered**, so eject/insert are
just "off → mutate the registry → on". Disk survives a cart eject via
the registry. Media handlers no longer embed their own
`fill_power_on_ram + cold_reset`.

Disk mount/unmount stays a **live** device op (the 1541 has its own
power) — only CART insert/eject power-cycles the C64 (VICE-faithful).

## Surface

- **Core:** `Session::power_on/power_off/warm_reset` +
  `set_inserted_cart/clear_inserted_cart`.
- **Daemon WS:** new `session/power {op:"on"|"off"}`; `session/reset
  {mode:"soft"}` → `warm_reset`, `{mode:"cold"}` → `power_off`+`power_on`;
  `media/mount` (crt) + `media/unmount` (cart) compose off→registry→on;
  monitor verbs `power on|off` + `reset [warm|cold]`.
- **trx64-cli:** `/power on|off`, `/reset [warm|cold]` (default **warm**),
  `/eject`, `/mount` (unchanged method contracts).
- **C64RE UI:** `MachineControls` power toggle → `session/power`; Reset
  button stays `{mode:"soft"}`.

`cold_reset` itself is left untouched (low blast radius); the fix lives
in the new session-level rebuild path.
