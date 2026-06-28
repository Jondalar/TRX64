#!/usr/bin/env bash
# Build trx64cli for all three desktop OSes from a macOS (Apple Silicon) host and
# collect the binaries into dist/handout/.
#
#   macOS arm64    — native host build
#   Linux x86_64   — inside an amd64 Linux container (Apple `container`, emulated):
#                    a native x86_64 toolchain compiles reSID C++ + cpal/ALSA
#   Windows x86_64 — inside a native arm64 Linux container, cross-compiled to
#                    x86_64-pc-windows-gnu with mingw-w64 (reSID C++ via mingw g++)
#
# Requires: rustup (host), Apple `container` running. No host zig / no cross toolchains
# on the host — the containers carry them. ROMs are NOT bundled here (see the handout
# README); trx64cli needs C64 ROMs at runtime via --rom-dir.
set -euo pipefail
cd "$(dirname "$0")/.."

OUT="dist/handout"
IMG="docker.io/library/rust:bookworm"
VER="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"

rm -rf "$OUT"
mkdir -p "$OUT/macos-arm64" "$OUT/linux-x86_64" "$OUT/windows-x86_64"

echo "== macOS arm64 (native host) =="
cargo build --release -p trx64-cli
cp target/release/trx64cli "$OUT/macos-arm64/"
strip "$OUT/macos-arm64/trx64cli" 2>/dev/null || true

echo "== Linux x86_64 (amd64 container — native x86_64 build) =="
container run --rm -a amd64 -m 6g -c 4 -v "$PWD":/work -w /work "$IMG" bash -c '
  apt-get update -qq && apt-get install -y -qq libasound2-dev pkg-config >/dev/null 2>&1
  export CARGO_TARGET_DIR=/work/dist/linux-x86_64 CARGO_BUILD_JOBS=4
  cargo build --release -p trx64-cli
  strip /work/dist/linux-x86_64/release/trx64cli 2>/dev/null || true'
cp dist/linux-x86_64/release/trx64cli "$OUT/linux-x86_64/"

echo "== Windows x86_64 (arm64 container + mingw-w64 cross) =="
container run --rm -m 6g -c 4 -v "$PWD":/work -w /work "$IMG" bash -c '
  apt-get update -qq && apt-get install -y -qq gcc-mingw-w64-x86-64 g++-mingw-w64-x86-64 >/dev/null 2>&1
  rustup target add x86_64-pc-windows-gnu
  export CARGO_TARGET_DIR=/work/dist/windows-x86_64 CARGO_BUILD_JOBS=4 \
    CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc \
    CXX_x86_64_pc_windows_gnu=x86_64-w64-mingw32-g++ \
    AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar \
    CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
  cargo build --release -p trx64-cli --target x86_64-pc-windows-gnu
  x86_64-w64-mingw32-strip /work/dist/windows-x86_64/x86_64-pc-windows-gnu/release/trx64cli.exe 2>/dev/null || true'
cp dist/windows-x86_64/x86_64-pc-windows-gnu/release/trx64cli.exe "$OUT/windows-x86_64/"

cp scripts/dist-readme.md "$OUT/README.md" 2>/dev/null || true

echo "== done — dist/handout (build $VER) =="
ls -laR "$OUT"
echo
echo "ROMs are NOT included. Either drop a roms/ dir next to each binary, or run with"
echo "  trx64cli --rom-dir <path-to-c64-roms>   (kernal-901227-03.bin, basic-901226-01.bin, characters-901225-01.bin)"
