# Features

One line per feature, as of 0.9.2. Details: [`README.md`](../README.md), monitor:
[`MONITOR.md`](../MONITOR.md), Swift API: [`crates/trx64-ffi/API.md`](../crates/trx64-ffi/API.md).

## Machine

- C64 models as rows of `crates/trx64-core/models.toml`: PAL 6569, NTSC 6567R8, PAL-N 6572 (`--model c64-pal|c64-ntsc|c64-paln`, `--video pal|ntsc`).
- C64C and first-revision rows are listed and refused by name (6526A CIA, custom-IC glue, KERNAL rev1/rev2 missing).
- `model <row>` / `session/model`: switch a running machine at the next frame boundary; snapshots and checkpoints record the model.
- `--machine c64|u64|128` (daemon): `u64` is the Ultimate 64, Elite II and C64 Ultimate: speed register at `$D031`, CPU to 64 MHz (`--speed-table u64ii`, default) or 48 MHz (`u64`); 1 MHz for 2.06 s after a reset, as on the device.
- `128`: the VIC-IIe `$D02F`/`$D030` pair with VICE's read-back masks, for turbo probes.
- Power on/off, cold and warm reset (`/power`, `/reset cold|warm`, `session/power`, `session/reset`).
- Pacing: realtime at the model's frame rate, warp (8×), fixed ratio (`/warp`, `session/set_pacing`).
- One machine per process, shared by every client; `project/set` moves the daemon to another project.

## Video

- VIC-II per cycle, ported from VICE x64sc; colodore palette.
- Canvas 384×272 PAL, 384×247 NTSC; PNG screenshots (`session/screenshot`).
- Border colour (`$D020`) changes per VIC pixel under U64 turbo.
- Native window (`trx64cli --window`, `/window`), aspect-locked resizing.
- `vic/inspect`, `vic/line_trace` (one raster line cycle by cycle, replayed in a clone), `vic/frame_map`.
- `bitmap` renders a RAM range as hires, charset or sprites.

## Sound

- reSID, 6581, mono 44.1 kHz: in the window (cpal), on the WebSocket stream, and pulled over the FFI.
- `audio/export`: a stretch of SID output to WAV.
- Several SIDs through an address decode table (`set_sid_map`, Rust API); the daemon, window and FFI play the first one.

## Cartridges

- 9 CRT hardware types: generic 8K/16K/Ultimax, Ocean, Magic Desk, Magic Desk 16, EasyFlash, GMod2, GMod4, MegaByter, C64MegaCart.
- Type 232: a 4 MB EasyFlash for development, TRX64 only. GMod3 is parsed and refused.
- Flash writes (EasyFlash, GMod2, MegaByter, C64MegaCart, GMod4 SPI) and the GMod2 EEPROM survive reset and snapshots.
- `savecrt` writes the flash back to the `.crt`; `swapcrt` swaps the image with no reset. Mounting power-cycles, ejecting cold-resets.
- Raw `.bin` images in the sandbox (`sandbox --cart --cart-type`).
- REU 1700/1764/1750 and oversized up to 16 MB (`--reu`), GeoRAM (`--georam`), preload with `--reu-image`.
- Ultimate Command Interface on the `u64` machine (`uci`).

## Drives and IEC

- 1541 with its own 6502, VIAs and GCR rotation ported from VICE; `.d64` 35-42 tracks with or without error bytes, `.g64`.
- 1581 with a 2 MHz 6502, 8520 CIA and WD1772 with an MFM surface; `.d81` 80-83 tracks. The 1581 DOS ROM is not included.
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

- `trx64-daemon`: JSON-RPC 2.0 over WebSocket (`--port`, `--bind`), binary video and audio frames, `--headless` for command-driven use.
- `trx64cli`: terminal cockpit that links the runtime in-process; `mon "<cmd>"` for one-shot use.
- Rust: `trx64-core::Machine`, `trx64-monitor` as a library.
- `IecDevice`: a host's own device on the serial bus at slots 4-11 (`attach_iec_device`).
- `FdcController`: a host's own controller in place of a 1581's WD1772 (`attach_fdc_controller`).
- `ExpansionDevice`: a host's own expansion port device; REU and GeoRAM on host-owned RAM (`attach_reu_borrowed`).
- Swift via uniffi (`trx64-ffi`): session, run, input, media, both drives, folder devices, trace, checkpoints, reverse debug, snapshots, events; `call` reaches every other JSON-RPC method.

## Platforms

- macOS arm64, Linux x86_64 and arm64, Windows x86_64 and arm64; `trx64cli` and `trx64-daemon` in each archive.
- Homebrew tap `jondalar/tap/trx64`.
- C64 ROMs are not included (`trx64cli --rom-dir`, or `~/.trx64/roms` for both binaries).
