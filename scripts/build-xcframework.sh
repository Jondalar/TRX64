#!/usr/bin/env bash
#
# build-xcframework.sh — build TRX64FFI.xcframework from `trx64-ffi`.
#
# Produces a Swift-importable XCFramework wrapping the typed uniffi bindings:
#   1. cargo-build the `trx64-ffi` staticlib for each requested Apple target
#   2. run the BUNDLED uniffi-bindgen (--library mode) → Swift glue + modulemap
#   3. xcodebuild -create-xcframework with one slice per built target
#
# REALITY (verified 2026-06-27 on this machine):
#   - This is a Homebrew rust toolchain (no rustup). Only the HOST target
#     `aarch64-apple-darwin` has a std library installed, so ONLY the macOS arm64
#     slice can be built here. iOS / iOS-sim / tvOS slices need rustup +
#     `rustup target add aarch64-apple-ios aarch64-apple-ios-sim ...` (and the reSID
#     C++ must cross-compile for those archs via `cc`, usually OK but verify the
#     sim/tvOS slices for real). The script ATTEMPTS each target and reports
#     honestly which built vs were skipped — it never fabricates a slice.
#
# Usage:
#   scripts/build-xcframework.sh                 # build all attemptable slices
#   TARGETS="aarch64-apple-darwin" scripts/build-xcframework.sh   # subset
#
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CRATE="trx64-ffi"
LIB_BASENAME="libtrx64_ffi.a"
FRAMEWORK_NAME="TRX64FFI"
OUT="$ROOT/target/xcframework"
BINDINGS="$OUT/Bindings"        # generated Swift + headers + modulemap
SLICES="$OUT/slices"            # per-target headers dir for xcframework
mkdir -p "$OUT" "$BINDINGS" "$SLICES"

# Candidate Apple targets (host first). Override with $TARGETS.
DEFAULT_TARGETS=(
  aarch64-apple-darwin        # macOS arm64  (host — always attemptable)
  aarch64-apple-ios           # iOS device
  aarch64-apple-ios-sim       # iOS simulator (Apple silicon)
  aarch64-apple-tvos          # tvOS device
  aarch64-apple-tvos-sim      # tvOS simulator (Apple silicon)
)
read -r -a TGTS <<< "${TARGETS:-${DEFAULT_TARGETS[*]}}"

BUILT=()
SKIPPED=()

echo "== building $CRATE staticlib per target =="
for t in "${TGTS[@]}"; do
  echo "--- $t ---"
  if cargo build --release -p "$CRATE" --target "$t" 2>/tmp/trx64-ffi-$t.log; then
    BUILT+=("$t")
    echo "  OK"
  else
    SKIPPED+=("$t")
    reason="$(grep -m1 -iE "may not be installed|not find|error\[|std" /tmp/trx64-ffi-$t.log | head -1)"
    echo "  SKIP ($t): ${reason:-see /tmp/trx64-ffi-$t.log}"
  fi
done

if [ "${#BUILT[@]}" -eq 0 ]; then
  echo "ERROR: no target built — cannot create an XCFramework." >&2
  exit 1
fi

# Generate the Swift bindings ONCE from any built dylib (the API is target-agnostic).
# Use the bundled uniffi-bindgen so no global install is needed; it links the SAME
# uniffi version as the scaffolding.
echo "== generating Swift bindings (bundled uniffi-bindgen, --library mode) =="
HOST_T="${BUILT[0]}"
# Prefer the dylib for --library introspection; fall back to the staticlib.
LIBDIR="$ROOT/target/$HOST_T/release"
LIBFILE="$LIBDIR/libtrx64_ffi.dylib"
[ -f "$LIBFILE" ] || LIBFILE="$LIBDIR/$LIB_BASENAME"
cargo run --release -p "$CRATE" --bin uniffi-bindgen -- \
  generate --library "$LIBFILE" --language swift --out-dir "$BINDINGS"

# uniffi emits `<module>.swift`, `<module>FFI.h`, and `<module>FFI.modulemap`.
# Rename the modulemap to `module.modulemap` (xcframework convention) per slice dir.
echo "== assembling per-slice header dirs =="
LIB_OUT_ARGS=()
for t in "${BUILT[@]}"; do
  hdir="$SLICES/$t"
  rm -rf "$hdir"; mkdir -p "$hdir"
  cp "$BINDINGS"/*FFI.h "$hdir/" 2>/dev/null || true
  # modulemap: copy + normalize the filename.
  for mm in "$BINDINGS"/*.modulemap; do
    [ -f "$mm" ] && cp "$mm" "$hdir/module.modulemap"
  done
  LIB_OUT_ARGS+=( -library "$ROOT/target/$t/release/$LIB_BASENAME" -headers "$hdir" )
done

echo "== xcodebuild -create-xcframework =="
rm -rf "$OUT/$FRAMEWORK_NAME.xcframework"
if xcodebuild -create-xcframework "${LIB_OUT_ARGS[@]}" \
     -output "$OUT/$FRAMEWORK_NAME.xcframework"; then
  echo "OK: $OUT/$FRAMEWORK_NAME.xcframework"
else
  echo "ERROR: xcodebuild -create-xcframework failed." >&2
  exit 1
fi

echo
echo "== SUMMARY =="
echo "Swift bindings:  $BINDINGS/"
echo "XCFramework:     $OUT/$FRAMEWORK_NAME.xcframework"
echo "Built slices:    ${BUILT[*]}"
[ "${#SKIPPED[@]}" -gt 0 ] && echo "Skipped slices:  ${SKIPPED[*]}  (need rustup + 'rustup target add ...')"
echo
echo "The Swift app links the .a slice + imports the generated $BINDINGS/*.swift."
