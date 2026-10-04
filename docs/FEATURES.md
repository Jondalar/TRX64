# Features

One line per feature, as of 0.12.3. Details: [`README.md`](../README.md), monitor:
[`MONITOR.md`](../MONITOR.md), Swift API: [`crates/trx64-ffi/API.md`](../crates/trx64-ffi/API.md).

## Machine

- C64 models as rows of `crates/trx64-core/models.toml`: PAL 6569, NTSC 6567R8, PAL-N 6572 (`--model c64-pal|c64-ntsc|c64-paln`, `--video pal|ntsc`).
- C64C and first-revision rows are listed and refused by name (custom-IC glue, KERNAL rev1/rev2 missing).
- CIA 1 and CIA 2 ported from VICE's CIA core: the 6526 of the C64 rows (the 6526A is there for the C64C rows), its interrupt delay line, shift register and time-of-day clock on the model's mains; the keyboard matrix as VICE solves it, ghost keys included.
- `model <row>` / `session/model`: switch a running machine at the next frame boundary; snapshots and checkpoints record the model.
- `--machine c64|u64|128` (daemon): `u64` is the Ultimate 64, Elite II and C64 Ultimate: speed register at `$D031`, CPU to 63× PHI2 — the menu's 64 MHz, as measured (`--speed-table u64ii`, default) or 48 MHz (`u64`, first generation, unmeasured); 1 MHz for 2^22 PHI2 cycles (4.26 s) after a reset, as on the device.
- `$D031`: speed index in bits 0-3, bit 7 set = no badline stalls (at every speed, 1 MHz included). A program's write and a menu change (`Machine::set_u64_turbo`) set the same state; the later one wins. After a reset in "U64 Turbo Registers" mode `$D031` reads `$00` — 1 MHz with stalls — until one of them sets a speed; "Off" and "Manual" read `$FF` and run the menu speed.
- Under turbo, IRQ and NMI are taken by the 6502's own rule at the turbo clock (two CPU cycles, then the end of the instruction), the IRQ line arriving a quarter PHI2 cycle late as measured; CIA timers, TOD and the raster stay on PHI2.
- Turbo access costs as measured on a C64 Ultimate: RAM, ROM, VIC, SID and colour RAM at the turbo clock (a VIC/SID read or SID write needs two slots of the Ultimate's 64-slot grid, so it costs an extra CPU cycle at 63×, sometimes at 32-47×); CIA 1/2, IO1 and IO2 one PHI2 bus cycle each, IO1/IO2 issued earlier than a CIA, a write earlier than a read.
- `$D030`/`$D031` decode in every `$40` mirror; "TurboEnable Bit" mode (`$D030` loads the menu speed or 1 MHz); `$D07A`/`$D07B` (and `$D0FA`/`$D0FB`) switch to 1 MHz / the menu speed; `$D0BC` (and `$D03C`/`$D07C`/`$D0FC`) reads `$01` with SuperCPU Detect. Any change to the Ultimate's settings (`set_u64_turbo`, a model switch) re-applies the menu speed, and in TurboEnable mode sets the enable bit.
- The turbo state rides in `.c64re` dumps and the checkpoint ring; an older `u64` dump restores as a reset leaves it, with a note.
- `128`: the VIC-IIe `$D02F`/`$D030` pair with VICE's read-back masks, for turbo probes.
- Power on/off, cold and warm reset (`/power`, `/reset cold|warm`, `session/power`, `session/reset`).
- Pacing: realtime at the model's frame rate, warp (8×), fixed ratio (`/warp`, `session/set_pacing`).
- One machine per process, shared by every client; `project/set` moves the daemon to another project.

## Video

- VIC-II per cycle, ported from VICE x64sc; colodore palette.
- Canvas 384×272 PAL, 384×247 NTSC; PNG screenshots (`session/screenshot`).
- Colour registers (`$D020`-`$D02E`: border, background, multicolour, sprite colours) change per VIC pixel under U64 turbo, sampled once per pixel on the grid measured on a C64 Ultimate.
- Native window (`trx64cli --window`, `/window`), aspect-locked resizing.
- `vic/inspect`, `vic/line_trace` (one raster line cycle by cycle, replayed in a clone), `vic/frame_map`.
- `bitmap` renders a RAM range as hires, charset or sprites.

## Sound

- reSID, 6581, mono 44.1 kHz: in the window (cpal), on the WebSocket stream, and pulled over the FFI.
- `audio/export`: a stretch of SID output to WAV.
- Several SIDs through an address decode table (`set_sid_map`, Rust API); the daemon, window and FFI play the first one.

## Cartridges

- 10 CRT hardware types: generic 8K/16K/Ultimax, Ocean, Magic Desk, Magic Desk 16, EasyFlash, EasyFlash 3, GMod2, GMod4, MegaByter, C64MegaCart.
- EasyFlash 3 (CRT type 90, mnemonic `ef3`): one 8 MB MX29LV640EB with eight slots of 64 banks, selected with `$DE01`; the cartridge lines and boot flag from `$DE02`; the 256-byte IO2 RAM; the version register at `$DE08`; the mode register at `$DE0F` for EasyFlash with and without a reset and for off. The three buttons — Menu, Reset, Special — are `cart button <menu|reset|special>` in the monitor, the `cart/button` call and `cartButton` in the Swift API; a cartridge without buttons refuses by name. The chip has its 8 KB boot blocks and its 64 KB sectors, answers the status polling of a program or erase in progress, and runs the EF3's own EAPI driver unchanged. KERNAL, AR/RR/NP, SS5 and C128 modes are not emulated: a program that selects one gets the cartridge switched off, and the cart status says so (`ef3Mode.notEmulated`). USB reads as no data, not ready.
- Type 232: a 4 MB EasyFlash for development, TRX64 only. GMod3 is parsed and refused.
- Flash writes (EasyFlash, EasyFlash 3, GMod2, MegaByter, C64MegaCart, GMod4 SPI) and the GMod2 EEPROM survive reset and snapshots.
- `savecrt` writes the flash back to the `.crt`; `swapcrt` swaps the image with no reset. Mounting power-cycles, ejecting cold-resets.
- Raw `.bin` images in the sandbox (`sandbox --cart --cart-type`).
- REU 1700/1764/1750 and oversized up to 16 MB (`--reu`), GeoRAM (`--georam`), preload with `--reu-image`.
- Ultimate Command Interface on the `u64` machine (`uci`).

## Drives and IEC

- 1541 with its own 6502, VIAs and GCR rotation ported from VICE; `.d64` 35-42 tracks with or without error bytes, `.g64`.
- 1581 with a 2 MHz 6502, 8520 CIA and WD1772 with an MFM surface; `.d81` 80-83 tracks. The 1581 DOS ROM is not included; it is looked up in every ROM directory, and a position cannot become a 1581, or power on as one, without it (the error names the file and the directories).
- Two drive positions, A at unit 8 and B off at 9; each a 1541 or a 1581, type changed only while off (`session/drive_type`).
- Per drive: power, reset pulse or held reset, stopped clock, unit jumpers 8-11 (`drivepower`, `session/drive_*`).
- Disk writes go back to the host file (`.d64`, `.g64`, `.d81`); media type read from file content.
- Disk swap live, no reset (`/mount`, `media/swap`, `eject <unit>`); a 1541 eject darkens the write-protect sensor, so the DOS sees the disk change.
- `runtime/swap_disk_and_continue`: eject, settle, insert, type the confirm key, compare screens.
- Host folder as an IEC device at unit 8-11 (`device/folder_attach`): LOAD, SAVE, directory, subdirectories, scratch, rename, read-only option.
- The folder device refuses `M-W`/`M-E`/`M-R`, block commands, `U1`/`U2`; Ultimate or VICE bus timing.

## Input

- Host keyboard mapped by character; ESC = ←, `^` = CTRL, Tab = RUN/STOP, host Control = C=, F2/F4/F6/F8 = SHIFT + F1/F3/F5/F7; Cmd and RESTORE not mapped.
- Joysticks on both ports (`/joystick port1|port2`: WASD + Space; `session/joystick_set`).
- POT lines for paddles, the 1351 mouse and extra fire buttons: the host sets the byte (`pot`, `session/pot_set`, `session/pot_clear`, `set_pot`).
- Typed text and held keys (`session/type`, `session/key_down`, `key_up`).

## Time machine

- `.c64re` full machine snapshots with media embedded (`dump`, `undump`, `snapshot/*`).
- VICE `.vsf` in and out (`trx64cli convert-vsf`, `convert-c64re`, `vsf/load`, `vsf/save`).
- Checkpoint ring of full machine states, 60 s default (`cadence`, `window`); rewind transport (`play back|fwd`, `frame ±N`, `goto`, F9-F12).
- Marks: named, pinned points that survive cutting the future (`mark`, `marks`, `goto <name>`).
- Always-on reverse ring, 10 s default (`rstep`, `whowrote`, `chis`, `revdepth`); `ringdump`/`ringload` carry it with its marks as a `.c64rering`.
- Checkpoint diffs (`diff`, `trx64cli diff`), sandbox runs (`sandbox/run`, `trx64cli sandbox`), overlays and candidates (`runtime/overlay_run`, `runtime/candidate_*`), scenarios (`runtime/scenario_*`, `batch/*`).

## Debugging

- Monitor based on VICE, same verbs in the cockpit, over WebSocket and through the FFI (`monitor/exec`).
- Bank lens `cpu|ram|rom|io|cart` on `m`/`d`; `device drive<unit>` reads a drive CPU.
- Breakpoints (`bk`), observers on exec, load or store with conditions and actions (`obs`).
- Step into, over, until, return; flow focus on main, IRQ, NMI or BRK (`focus`, `sf`, `nf`, `flow`, `bt`); JAM auto-break with `triage`, `traprules`.
- Traces of CPU, drive CPU, IEC, VIC, memory, drive mechanism and cartridge reads to `.c64retrace`, indexed into DuckDB (`trace`, `tracedb`, `traceindex`).
- `tracering` builds a trace after the fact from the reverse ring; `map`, `taint`, `swimlane` analyse a trace.
- Bus and device reports: `iec`, `io`, `drive`, `cart`, `folder`, `pot`, `reu`, `georam`, `uci`, `turbo`.
- `trx64cli disasm`: static disassembly of a PRG or raw image, no ROMs.

## Embedding

- `trx64-daemon`: JSON-RPC 2.0 over WebSocket (`--port`, `--bind`), binary video and audio frames, `--headless` for command-driven use; `--idle-exit <s>` ends an unused daemon (no request, no A/V subscriber, no recording trace), persisting its media first, and `daemon/keep_alive` holds it.
- `trx64cli`: terminal cockpit that links the runtime in-process; `mon "<cmd>"` for one-shot use.
- Rust: `trx64-core::Machine`, `trx64-monitor` as a library.
- `IecDevice`: a host's own device on the serial bus at slots 4-11 (`attach_iec_device`).
- `FdcController`: a host's own controller in place of a 1581's WD1772 (`attach_fdc_controller`).
- `ExpansionDevice`: a host's own expansion port device; REU and GeoRAM on host-owned RAM (`attach_reu_borrowed`).
- Swift via uniffi (`trx64-ffi`): session, run, input, media, both drives, folder devices, trace, checkpoints, reverse debug, snapshots, events; `call` reaches every other JSON-RPC method.

## Platforms

- macOS, Linux and Windows, each x86_64 and arm64; `trx64cli` and `trx64-daemon` in each archive.
- Homebrew tap `jondalar/trx64` (macOS and Linux, x86_64 and arm64).
- C64 ROMs are not included (`trx64cli --rom-dir`, or `~/.trx64/roms` for both binaries).
