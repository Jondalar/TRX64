# Spec 913 — An Action Replay V5 or V6 boots, freezes a running program and returns to it

**Status:** PARTLY BUILT (2026-10-16): the cartridge, the freeze machinery, the state and the synthetic tests are built; the corpus tests (S2, S4, S5 on the real images) are not yet.
**Repo:** TRX64 (a cartridge capability). C64RE gains nothing beyond what it already reads (the cart's type name in session state and the trace header).
**Number:** 913 (registry: C64RE `specs/README.md`).
**Origin:** owner, 2026-10-16, after an analysis of VICE's freezer cartridges: "Es wäre schon hilfreich Action Replay zu können .. lass uns das als single Spec einbauen und dann nur das erstmal". Retro Replay and Final Cartridge III are out (§4).
**Depends on:** nothing.

---

## §1 The problem

The owner works with freezer cartridges as a scener does: plug in an Action Replay, run a
program, press the freeze button, look at or change memory, then return to the program. TRX64
cannot do this. Mounting an Action Replay V5 or V6 image from
`~/Development/C64/Tools/Cartridges/Action_Replay/` fails: `infer_mapper_type`
(`crates/trx64-core/src/cart.rs:595-624`) does not map CRT type 1 and returns `None`. There is
no `.bin` geometry for it either.

The owner wants V5 and V6 only. Both are CRT type 1, VICE's `actionreplay.c`: 32 KiB ROM in four
8 KiB banks, 8 KiB RAM. The images that prove it: `Action_Replay_V5.0_5.0_1988_PAL`,
`Action_Replay_V5.0_5.0_1988_NTSC`, `Action_Replay_V5.0_5.1a_1988_PAL`,
`Action_Replay_V6.0_6.0_1989_PAL` (`.crt` and `.bin` each). V4.1/V4.2 are the same board and the
same CRT type, so they mount too, but nothing here is proven on them.

The parts:

- **P1 Mount.** Each V5/V6 image mounts as `.crt` and as `.bin`, as an Action Replay, with its four banks.
- **P2 Boot.** On power-on the cartridge comes up as on hardware: bank 0, 8K mode, its own
  start-up screen.
- **P3 Registers and memory map.** What the cartridge's program does to it works as on hardware:
  the `$DE00` register (GAME, EXROM, kill, bank, RAM, freeze-ack), the 8 KiB RAM at ROML and in
  IO2, the IO2 window onto ROM or RAM, mode $22 (ROML reads C64 RAM OR cart RAM), and the effect
  of a plain *read*: reading IO1 writes the register with the phi1 bus byte
  (`actionreplay.c:162-178`).
- **P4 Freeze.** Pressing the button stops a running program the way the hardware does: NMI,
  then a switch to ultimax so the NMI vector comes from the cartridge, then the freezer menu.
  Leaving the menu returns to the program at the place it was frozen.
- **P5 The machine's own tools keep working.** Checkpoint, rewind, `.c64re` dump/undump and
  traces carry the cartridge's full state (registers, active/frozen, cartridge RAM). A monitor
  peek or a `session/state` read never fires a read side effect. A capture's media identity
  names the family.
- **P6 Determinism.** The same image, the same inputs and the same press cycle give the same
  run on every fresh daemon.

What TRX64 is missing for this (read from the code, 2026-10-16):

1. A cartridge cannot drive NMI: `CartMapper` has no line output, `port_active` ignores the
   cartridge (`lib.rs:2282-2287`). The expansion NMI source is sampled every cycle
   (`c64_6510core.rs:983,1020`) and can be reused.
2. No freeze event: `cart_press_button` acts between runs and does not re-run the PLA
   (`lib.rs:1415-1428`).
3. A read that changes the configuration does not re-run the PLA: `cart_read`/`port_read`
   (`full.rs:904-1027`) never call `pla_config_changed`. Writes do (`full.rs:765-767`).
4. The phi1 bus byte a cartridge sees is hard-wired to `0xff` in `BankInfo` (`full.rs:371-389`);
   `vic.last_read_phi1` exists.
5. Cartridge RAM has no place in `CartState` / `CartStateSnapshot` (`c64re_snapshot.rs:1273-1300`,
   `:1444-1502`) and must not go through `writable_image` (that path auto-persists into the `.crt`).
6. The read-set lane (`CartSlot`, `cart.rs:203-235`) would count reads of cartridge RAM at $8000
   as ROM payload.

## §2 Scenarios

```gherkin
Scenario: an Action Replay boots to its own screen
  Given a fresh daemon and one Action Replay V5 or V6 image
  When  it is mounted and the machine runs 3 s
  Then  the screen shows that cartridge's start-up screen (fixed text per image)
  And   session/state names the cartridge by family, banks and sha256

Scenario: freeze and return
  Given a BASIC program printing an increasing counter, with the cartridge in and active
  When  the freeze button is pressed and the machine runs
  Then  the CPU took the NMI with the cartridge in ultimax and the freezer menu is on screen
  When  the menu's "continue/restart" key is typed
  Then  the counter runs on from the frozen value, and RAM below the program is as before

Scenario: rewind across a freeze
  Given a checkpoint taken while frozen (menu on screen, cartridge RAM written)
  When  the machine runs on, returns to the program, and the checkpoint is restored
  Then  the menu is on screen again, the cartridge state and RAM are those of the checkpoint

Scenario: a peek does nothing
  Given an Action Replay with a known register value
  When  the monitor reads $DE00-$DEFF and session/state is read
  Then  the register and the memory map are unchanged

Scenario: same press, same run
  Given the same image, program and press cycle on two fresh daemons
  Then  the traces are identical
```

| # | Solves | Scenario | Test | Where it runs |
|---|---|---|---|---|
| S1 | P1 | each V5/V6 image mounts as `.crt` and `.bin` as an Action Replay with 4 banks | `crates/trx64-core` unit tests on synthetic images + a corpus test that skips without the corpus | gate (synthetic), Mac (corpus) |
| S2 | P2 | each image boots to its start-up screen | daemon test over the corpus, screen text per image | Mac (Commodore/Datel ROMs are not checked in) |
| S3 | P3 | register, kill, RAM, IO2 window, mode $22, IO1 read side effect | unit tests on a synthetic ROM | gate |
| S4 | P4 | freeze and return on each V5/V6 image | daemon test, corpus | Mac |
| S5 | P5 | rewind across a freeze; `.c64re` dump/undump while frozen | daemon test, corpus | Mac |
| S6 | P5 | a peek does nothing | unit test | gate |
| S7 | P6 | same press, same run, over fresh daemons | daemon test, synthetic ROM with an NMI handler | gate |

## §3 Decision

**D1 One family: CRT type 1** (owner, 2026-10-16: "Action Replay 5 und AR6, das reicht, nicht
mehr"). A port of VICE's `actionreplay.c`, function by function, into
`crates/trx64-core/src/action_replay.rs`, with its `.bin` geometry (32 KiB).

**D2 Freeze machinery** (shared, built once, reused by any later freezer): `CartMapper` gains
`nmi_line()`, `freeze()` and `freeze_allowed()`; a scheduled freeze event (NMI asserted, the
cartridge's `freeze()` 3 cycles later, then the PLA) ported from `c64cart.c` /
`c64carthooks.c`; the cartridge's NMI joins the expansion NMI source; a read with a side effect
re-runs the PLA; the cartridge sees the real phi1 byte.

**D3 Freeze button.** The existing `cart/button` RPC and the monitor's `cart button`
(`trx64-daemon/src/main.rs:7924-7960`, `:6733`) carry a `freeze` button for this cartridge.

**D4 State.** Cartridge registers, active/frozen and the cartridge RAM go into `CartState` and
the checkpoint (RAM as bytes, not `Vec<i64>`); the read-set lane excludes cartridge RAM.

**Decided with the owner:**

- **Q1 Freeze delay — decided 2026-10-16 (owner: "Taktgenauer freeze").** The freeze takes
  effect at the cycle the button is pressed: no random delay. VICE's random 1..one-frame delay
  (`c64cart.c:1393`) would break P6; a later freeze is a later press. The rest of the sequence
  stays VICE's: NMI asserted, the cartridge's `freeze()` 3 cycles later (`c64cart.c:1369-1372`).
- **Q2 Scope — decided 2026-10-16:** V5 and V6 only (D1). VICE's release of the NMI comes from
  the freeze-ack bit, as in `actionreplay.c:131-160`; the never-released NMI of MK3/MK4 does not
  arise.

## §4 Out

- Action Replay MK1/MK2 (CRT type 50), MK3 and Disk Mate (35), MK4 (30): images on disk, not
  wanted (owner, 2026-10-16).
- Retro Replay, Nordic Replay, Final Cartridge III: no images on disk, and FC3 needs separate
  CPU/VIC memory configurations (a hot-path change). Later, by decision with the owner.
- Clockport, RR-Net: Retro Replay only.

## §5 Decisions taken while building

—
