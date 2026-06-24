# reSID — bundled source (provenance)

These files are an **unmodified** copy of the reSID SID-emulation engine as
shipped inside VICE. They are bundled here (copied verbatim from
`C64ReverseEngineeringMCP/third_party/resid/`) so TRX64's `build.rs` compiles the
**SAME** reSID C++ that c64re compiles to WASM — making TRX64's audio PCM
byte-identical (modulo the documented WASM↔native libm-table LSB boundary) to
c64re's reSID. See `crates/trx64-core/src/resid_ffi.rs` + `tests/resid_oracle.rs`.

## TRX64 build

`build.rs` (cc crate) compiles the units listed in c64re's
`scripts/build-resid-wasm.mjs` — sid/voice/wave/envelope/filter8580new/extfilt/
pot/dac/version (filter.cc intentionally OMITTED under `NEW_8580_FILTER=1`) —
plus `resid_shim.cc`, with `-DVERSION="1.0-pre2"`, `-std=c++11`.

## resid_shim.cc — TRX64-local addition

`resid_shim.cc` is OUR flat-C ABI (GPL-3.0-or-later), copied verbatim from c64re
EXCEPT for one ADDITIVE symbol: `resid_reinit()` (full re-construct of the global
SID via placement-new). It changes NO existing function's behavior — it
reproduces c64re's "fresh WASM module per engine" semantics for TRX64's
single long-lived native global, so a TRX64 reset matches a fresh c64re module.
All other shim functions are byte-identical to c64re's.

## Source

- Engine: reSID, a MOS6581/8580 SID emulator by Dag Lem
- reSID subpackage version: **1.0-pre2**
- Vendored from: VICE **3.10** source tree, `src/resid/`
- VICE repo: `git@github.com:VICE-Team/svn-mirror.git`
- VICE commit: `e635822a93` ("Merge branch 'clean' into main")
- Imported: 2026-05-22

`siddefs.h` is the VICE-configured variant (config macros already resolved:
`RESID_INLINING 1`, `RESID_INLINE inline`, `NEW_8580_FILTER 1`,
`HAVE_BUILTIN_EXPECT 1`, `HAVE_LOG1P 1`). No `configure` step is needed for a
standalone emscripten build. The engine is self-contained — it includes only
standard C++ headers (`<cassert> <cmath> <cstdlib> <fstream> <iostream>`),
no VICE-external dependencies.

## License

reSID is **GPL-2.0-or-later** (`Copyright (C) 2010 Dag Lem <resid@nimrod.no>`).
The original GPL headers are preserved verbatim in every file. This project
(C64RE MCP) is **GPL-3.0-or-later**, which is compatible via reSID's
"version 2 ... or any later version" grant. See `/THIRD_PARTY_NOTICES.md`.

## Do not edit

Treat this directory as read-only vendored source. To update, re-copy from the
pinned VICE commit and update this file. Local fixes belong in the WASM wrapper
(`scripts/build-resid-wasm.*` / `src/runtime/headless/sid/resid-wasm-engine.ts`),
never in these files — keep them byte-identical to VICE for fidelity tracing.
