// resid_msvc_prelude.h — MSVC-only prelude for the vendored reSID build.
//
// WHY THIS FILE EXISTS
//
// Upstream reSID resolves `HAVE_BUILTIN_EXPECT` with a configure probe that
// compiles `int main() { __builtin_expect(0, 0); }`. Our vendored copy is the
// VICE-*configured* variant, so that probe's answer is baked in as
//
//     #define HAVE_BUILTIN_EXPECT 1                        (siddefs.h:32)
//     #define likely(x)   __builtin_expect(!!(x), 1)        (siddefs.h:48)
//
// which is right for gcc/clang/emscripten and wrong for MSVC, where
// `__builtin_expect` does not exist. On Windows the build died with ~50 of
//
//     vendor\resid\wave.h(144): error C3861: '__builtin_expect': identifier not found
//
// The obvious fix would be a `_MSC_VER` guard in siddefs.h — but that file is
// byte-identical to VICE 3.10 and to c64re's WASM copy, and PROVENANCE.md keeps
// it that way on purpose so the three can be diffed forever. So instead of
// editing it, this prelude is force-included (`/FI`) ahead of every translation
// unit on MSVC only:
//
//   1. it pulls siddefs.h in first, which sets the RESID_SIDDEFS_H include
//      guard, so the vendor sources' own `#include "siddefs.h"` becomes a no-op;
//   2. it then redefines the two macros to the same fallback siddefs.h itself
//      uses when the configure probe fails (`#define likely(x) (x)`).
//
// This is a no-op for fidelity: `likely`/`unlikely` are branch *hints* — they
// steer the optimizer's layout and change no value the SID produces. Audio stays
// byte-identical to the WASM engine, and gcc/clang builds never see this file.

#pragma once

// Relative to THIS file rather than via -I, so the path is unambiguous and an
// editor's clangd resolves it too. Sets RESID_SIDDEFS_H.
#include "vendor/resid/siddefs.h"

#undef likely
#undef unlikely
#define likely(x) (x)
#define unlikely(x) (x)
