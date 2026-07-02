# Spec — Cross-platform TRX64: Linux + Windows

**Status:** PROPOSED (planning, not built). **Repo:** TRX64.
**Motivation:** the moment the macOS app is shown to the scene, the demand for
Windows + Linux starts. The Apple app stays Apple (SwiftUI), but the *runtime* is
portable Rust — Linux/Windows should be a build-and-ship task, not a rewrite.

## What ships on Linux/Windows — the daemon, not the XCFramework

The Swift `XCFramework` (uniffi → Swift) is **Apple-only**. On Linux/Windows the
artifact is the **`trx64-daemon` WS binary**:

```
Linux/Windows box:
  trx64-daemon (cross-built)  ──WS JSON-RPC──  C64RE (Node, cross-platform)  ──  browser UI
```

So the **whole C64RE workbench runs on Linux/Windows** with zero UI work: the daemon
is the only native piece; C64RE is Node, and its UI is a browser app (platform-
agnostic). The native app is the Apple extra; the daemon is the universal core.

A native Linux/Windows GUI later (if anyone wants one) would talk WS, or get its own
C-ABI lib — **not** the Swift bindings.

## The one real obstacle: cross-compiling reSID (C++)

Everything else is portable: the Rust core, tokio/tungstenite (WS), the trace/
checkpoint stack. The blocker is `trx64-core/build.rs` compiling vendored **reSID
C++** (+ the cartridge device cores) — `cc` needs a cross C/C++ toolchain + sysroot
for each target. Two viable paths:

### Path A — GitHub Actions native matrix (RECOMMENDED for releases)
Build **natively** on each OS runner — no cross-compile pain at all:

| runner | target | toolchain |
|---|---|---|
| `ubuntu-latest` | `x86_64-unknown-linux-gnu` | system gcc/clang (native) |
| `windows-latest` | `x86_64-pc-windows-msvc` | MSVC (native — and this is how Windows users expect a `.exe`) |
| `macos-latest` | `aarch64-apple-darwin` | native (the daemon; the xcframework is a separate Apple job) |

Native builds sidestep the reSID cross-compile entirely (each runner has its own
working C++ toolchain). Releases = the three daemon binaries attached to a GitHub
release. **This is almost certainly the right answer** — it also gives Windows the
MSVC `.exe` users expect, which a macOS cross-build cannot produce.

### Path B — local macOS cross (for dev / no CI)
From the dev Mac, via **`cargo-zigbuild`** (Zig as the cross C/C++ toolchain +
sysroot — handles reSID's C++ cleanly):
```
brew install zig && cargo install cargo-zigbuild
rustup target add x86_64-unknown-linux-gnu x86_64-pc-windows-gnu
cargo zigbuild --release -p trx64-daemon --target x86_64-unknown-linux-gnu
cargo zigbuild --release -p trx64-daemon --target x86_64-pc-windows-gnu
```
- Linux: clean with zig. (`cross` / Docker is the fallback if zig chokes on a C++ TU.)
- Windows: only the **`-gnu`** (MinGW) target cross-builds from macOS; **`-msvc`
  cannot** (needs Windows) → for an MSVC `.exe`, use Path A.

## Targets
- `x86_64-unknown-linux-gnu` (primary Linux)
- `aarch64-unknown-linux-gnu` (ARM Linux — optional, same path)
- `x86_64-pc-windows-msvc` (primary Windows — via CI) **or** `x86_64-pc-windows-gnu`
  (cross from mac)

## Risks / unknowns to verify
1. **reSID + the cartridge C++** under each target's compiler — the DSP, AM29F040B
   flash, m93c86 EEPROM, SPI-flash C cores. Native (Path A) makes this a non-issue;
   zig (Path B) very likely works but each C++ TU must be confirmed.
2. **Windows path/FS behaviour** — the daemon's project dir, media paths, auto-persist
   `.c64re`/`.c64retrace` writes, the trace sidecar (Node) invocation. Audit the
   path-handling for `\` vs `/` + temp-dir assumptions.
3. **WS bind** on Windows (firewall prompt on first `127.0.0.1` listen) — cosmetic.
4. **The trace-read sidecar** shells to Node — fine on Linux/Windows if Node present;
   the native Rust DuckDB reader (`docs/spec-trace-read-duckdb-native.md`) removes
   that dependency for a truly standalone Win/Linux daemon.

## Acceptance
- `trx64-daemon` builds + runs on Linux x86_64 and Windows; `session/create` →
  `debug/run` → `session/state` works over WS.
- The conformance oracle (or a subset) runs the cross-built daemon as the candidate
  and stays green (the runtime behaves identically across OSes).
- A GitHub release carries the three daemon binaries (macOS arm64, Linux x86_64,
  Windows x86_64).

## Out of scope
- A native Linux/Windows GUI (talk WS, or a later platform C-ABI lib).
- Windows-MSVC cross from macOS (use CI).
- The Apple XCFramework (separate Apple job).
