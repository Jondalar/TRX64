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
// configure step is needed.

use std::path::Path;

fn main() {
    let resid = Path::new("vendor/resid");

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
        .std("c++11")
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

    for u in units {
        build.file(resid.join(u));
    }
    // OUR flat-C shim (GPL-3, links GPL reSID) — the FFI ABI.
    build.file(resid.join("resid_shim.cc"));

    build.compile("resid");

    // Rebuild when any vendored source/header or the shim changes.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor/resid");
}
