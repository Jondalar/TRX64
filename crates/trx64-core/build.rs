// build.rs — compile the vendored GPL reSID C++ + the c64re flat-C shim into a
// static lib that TRX64 FFIs. This is the SAME source c64re compiles to WASM
// (third_party/resid/ + resid_shim.cc), so TRX64 audio is byte-identical to
// c64re's reSID. See crates/trx64-core/vendor/resid/PROVENANCE.md (GPL).
//
// Unit list + flags mirror scripts/build-resid-wasm.mjs exactly:
//   - RESID_UNITS (filter.cc OMITTED: NEW_8580_FILTER=1 in siddefs.h selects
//     filter8580new.{h,cc}; compiling filter.cc too duplicates reSID::Filter).
//   - -DVERSION="1.0-pre2" (version.cc needs it as a C string literal).
//   - -std=c++11, -O3, -I<vendor/resid>.
// siddefs.h is the VICE-configured variant (macros pre-resolved), so no
// configure step is needed — with ONE caveat: it was configured by gcc, so the
// resolved `HAVE_BUILTIN_EXPECT 1` is wrong for MSVC. See the is_msvc branch
// below and resid_msvc_prelude.h.

use std::path::{Path, PathBuf};

fn main() {
    let resid = Path::new("vendor/resid");
    let is_msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");

    // reSID compile units (verbatim from build-resid-wasm.mjs RESID_UNITS).
    let units = [
        "sid.cc",
        "voice.cc",
        "wave.cc",
        "envelope.cc",
        "filter8580new.cc",
        "extfilt.cc",
        "pot.cc",
        "dac.cc",
        "version.cc",
    ];

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .include(resid)
        // version.cc: `resid_version_string = VERSION;` needs a C string literal.
        .define("VERSION", "\"1.0-pre2\"")
        // Match emscripten's FP semantics as closely as native clang allows, to
        // minimize the WASM↔native resampler rounding gap: no FMA contraction
        // (WASM has no fused multiply-add by default), strict IEEE rounding.
        .flag_if_supported("-ffp-contract=off")
        .flag_if_supported("-fno-fast-math")
        // Quiet the vendored reSID's benign warnings — it is read-only VICE source.
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-variable")
        .warnings(false);

    if is_msvc {
        // MSVC has no `-std:c++11` (it starts at c++14) and answers the flag with a
        // D9002 warning per translation unit; its default is already a C++11 superset.
        //
        // `/FI` force-includes our prelude ahead of every unit to neutralise reSID's
        // gcc-only `__builtin_expect` branch hints WITHOUT editing the vendored
        // siddefs.h — see resid_msvc_prelude.h for the full reasoning. Absolute path
        // because cl resolves /FI against its own working directory, not ours.
        let prelude: PathBuf = [env!("CARGO_MANIFEST_DIR"), "resid_msvc_prelude.h"]
            .iter()
            .collect();
        build.flag(format!("/FI{}", prelude.display()));
    } else {
        build.std("c++11");
    }

    for u in units {
        build.file(resid.join(u));
    }
    // OUR flat-C shim (GPL-3, links GPL reSID) — the FFI ABI.
    build.file(resid.join("resid_shim.cc"));

    build.compile("resid");

    // Rebuild when any vendored source/header or the shim changes.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor/resid");
    println!("cargo:rerun-if-changed=resid_msvc_prelude.h");
}
