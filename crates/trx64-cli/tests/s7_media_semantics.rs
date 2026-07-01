//! CLI-FEEL S7 — mount-resume reconcile + `/eject` smart target + unified eject RAM
//! semantics. Drives the `Engine` verb layer on a single in-process machine (like
//! smoke.rs / namespaces.rs) and asserts the media behaviour end-to-end through the
//! real `media/mount` / `media/unmount` / `media/ingress` daemon RPCs.
//!
//! What S7 changed:
//!  1. `/mount <crt>` sets the cockpit host run flag true (the CRT power-cycle resumes
//!     the daemon; the pump must resume with it — the "CRT mount → C64 läuft weiter"
//!     verify).
//!  2. `/eject` sends role:"auto"; the daemon resolves it to the OCCUPIED slot — cart
//!     if inserted, else the disk on drive8 (never the wrong/absent slot).
//!  3. `media/ingress` kind:eject cart now power-cycles (RAM wiped), matching
//!     `media/unmount`'s cart branch — one eject RAM model, not two.
//!
//! ROM-gated: skips gracefully when the C64RE ROM bundle is absent.

use std::path::Path;

use trx64_cli::{boot_engine, default_rom_dir, Engine};

fn engine_or_skip() -> Option<Engine> {
    let rom_dir = default_rom_dir();
    if !Path::new(&rom_dir).join("kernal-901227-03.bin").exists() {
        eprintln!("[skip] s7_media: ROMs absent at {}", rom_dir.display());
        return None;
    }
    match boot_engine(&rom_dir) {
        Ok(e) => Some(e),
        Err(e) => {
            eprintln!("[skip] s7_media: boot failed: {e}");
            None
        }
    }
}

/// A minimal valid CRT (0x40 header + N CHIP packets), mirroring the cart_mapper_gate
/// fixture. `hw`=hardware type, `exrom`/`game`=header lines, chips=(bank, load, data).
fn build_crt(hw: u16, exrom: u8, game: u8, name: &str, chips: &[(u16, u16, Vec<u8>)]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"C64 CARTRIDGE   "); // 16-byte signature
    v.extend_from_slice(&0x40u32.to_be_bytes()); // headerLen @ 0x10
    v.extend_from_slice(&0x0100u16.to_be_bytes()); // version @ 0x14
    v.extend_from_slice(&hw.to_be_bytes()); // hardwareType @ 0x16
    v.push(exrom); // @ 0x18
    v.push(game); // @ 0x19
    v.extend_from_slice(&[0u8; 6]); // @ 0x1A-0x1F reserved
    let mut nm = [0u8; 32];
    let nb = name.as_bytes();
    nm[..nb.len().min(32)].copy_from_slice(&nb[..nb.len().min(32)]);
    v.extend_from_slice(&nm); // name @ 0x20-0x3F
    assert_eq!(v.len(), 0x40);
    for (bank, load, data) in chips {
        v.extend_from_slice(b"CHIP");
        v.extend_from_slice(&(0x10 + data.len() as u32).to_be_bytes()); // packetLen @ +4
        v.extend_from_slice(&0u16.to_be_bytes()); // chipType @ +8
        v.extend_from_slice(&bank.to_be_bytes()); // bank @ +10
        v.extend_from_slice(&load.to_be_bytes()); // loadAddr @ +12
        v.extend_from_slice(&(data.len() as u16).to_be_bytes()); // size @ +14
        v.extend_from_slice(data);
    }
    v
}

/// A standard 35-track / 683-block D64 (174848 bytes). Zero-filled is enough to mount
/// (from_d64 has no BAM validation — it GCR-encodes whatever bytes it gets).
fn zero_d64() -> Vec<u8> {
    vec![0u8; 174_848]
}

/// Write bytes to a uniquely-named temp file with the given suffix, return its path.
/// A process-global nonce keeps parallel tests (same PID) from sharing a filename.
fn temp_media(suffix: &str, bytes: &[u8]) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NONCE: AtomicU64 = AtomicU64::new(0);
    let n = NONCE.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!(
        "trx64_s7_{}_{}{}",
        std::process::id(),
        n,
        suffix
    ));
    std::fs::write(&p, bytes).unwrap();
    p
}

/// Read a single RAM byte via the monitor `m` dump (`>C:C000  42 …  B` → 0x42). Used
/// to prove the RAM wipe on eject.
fn read_byte(engine: &Engine, addr: u16) -> u8 {
    let out = engine.exec_line(&format!("m {addr:04x} {addr:04x}")).output;
    let line = out.lines().find(|l| l.trim_start().starts_with('>')).unwrap_or_else(|| {
        panic!("m dump had no data line, got: {out}");
    });
    // tokens: [">C:C000", "<byte>", "<ascii>"] — the first hex byte is the target addr.
    let tok = line.split_whitespace().nth(1).unwrap_or_else(|| panic!("no byte in: {line}"));
    u8::from_str_radix(tok, 16).unwrap_or_else(|_| panic!("bad hex byte '{tok}' in: {line}"))
}

/// (1) `/mount <crt>` attaches the cartridge AND resumes the cockpit pump (the CRT
/// power-cycle cold-boots + runs; the host run flag must follow the daemon's resume).
#[test]
fn mount_crt_attaches_and_resumes() {
    let Some(engine) = engine_or_skip() else { return };
    // Start from a paused machine so the resume is unambiguous (not pre-set true).
    engine.exec_line("/pause");
    assert!(!engine.is_running(), "precondition: machine paused before mount");

    let crt = build_crt(0, 0, 1, "S7CART", &[(0, 0x8000, vec![0xEAu8; 0x2000])]);
    let path = temp_media(".crt", &crt);

    let r = engine.exec_line(&format!("/mount {}", path.display()));
    assert!(r.output.contains("MOUNT"), "mount output: {}", r.output);

    // cart attached — the cold-boot marker: session/cart_status is non-null.
    let status = engine.rpc("session/cart_status", serde_json::json!({})).unwrap();
    assert!(!status.is_null(), "cart attached after /mount, got: {status}");

    // resume reconcile — the host run flag followed the CRT power-cycle.
    assert!(engine.is_running(), "cockpit pump resumed after CRT mount");

    let _ = std::fs::remove_file(&path);
}

/// (2) `/eject` after a DISK mount ejects the disk (drive8), not a (absent) cartridge.
#[test]
fn eject_targets_disk_when_no_cart() {
    let Some(engine) = engine_or_skip() else { return };
    let d64 = zero_d64();
    let path = temp_media(".d64", &d64);

    let m = engine.exec_line(&format!("/mount {}", path.display()));
    assert!(m.output.contains("MOUNT"), "disk mount output: {}", m.output);
    // Disk is on drive8 (session/list carries diskPath).
    let listed = engine.rpc("session/list", serde_json::json!({})).unwrap();
    let disk_before = listed[0]["diskPath"].as_str().unwrap_or("");
    assert!(!disk_before.is_empty(), "disk mounted on drive8: {listed}");

    // No cart present → /eject must target the disk (drive8), not the empty cart slot.
    let e = engine.exec_line("/eject");
    assert!(e.output.contains("drive8"), "eject targets drive8 disk, got: {}", e.output);

    let after = engine.rpc("session/list", serde_json::json!({})).unwrap();
    assert_eq!(after[0]["diskPath"].as_str().unwrap_or(""), "", "disk ejected: {after}");

    let _ = std::fs::remove_file(&path);
}

/// (2b) `/eject` with a CARTRIDGE present targets the cart (not drive8) — the smart
/// target's other arm.
#[test]
fn eject_targets_cart_when_present() {
    let Some(engine) = engine_or_skip() else { return };
    let crt = build_crt(0, 0, 1, "S7CART", &[(0, 0x8000, vec![0xEAu8; 0x2000])]);
    let path = temp_media(".crt", &crt);

    engine.exec_line(&format!("/mount {}", path.display()));
    assert!(
        !engine.rpc("session/cart_status", serde_json::json!({})).unwrap().is_null(),
        "cart present before eject"
    );

    let e = engine.exec_line("/eject");
    assert!(e.output.contains("cartridge"), "eject targets the cart, got: {}", e.output);
    assert!(
        engine.rpc("session/cart_status", serde_json::json!({})).unwrap().is_null(),
        "cart removed after /eject"
    );

    let _ = std::fs::remove_file(&path);
}

/// (3) `media/ingress` kind:eject cart now WIPES RAM (full power-cycle), matching
/// `media/unmount`'s cart branch. Fill $C000, eject the cart via ingress, confirm the
/// byte reverted to the power-on pattern (0xFF at $C000) — proof the RAM was wiped, not
/// kept (the pre-S7 keepRam behaviour left the fill byte in place).
#[test]
fn ingress_cart_eject_wipes_ram() {
    let Some(engine) = engine_or_skip() else { return };
    let crt = build_crt(0, 0, 1, "S7CART", &[(0, 0x8000, vec![0xEAu8; 0x2000])]);
    let path = temp_media(".crt", &crt);

    engine.exec_line(&format!("/mount {}", path.display()));
    // Write a distinctive byte into main RAM ($C000 is RAM in every C64 memconfig).
    engine.exec_line("f c000 c000 42");
    assert_eq!(read_byte(&engine, 0xC000), 0x42, "fill wrote the marker byte");

    // Eject the cartridge through the ingress boundary (the branch S7 unified).
    let ej = engine
        .rpc("media/ingress", serde_json::json!({ "kind": "eject", "role": "cartridge" }))
        .unwrap();
    assert_eq!(ej["ok"], serde_json::json!(true), "ingress eject ok: {ej}");

    // RAM was wiped by the power-cycle — the marker is gone, replaced by the VICE
    // power-on pattern. fill_power_on_ram is `addr & 0x40 ? 0xFF : 0x00`, and
    // $C000 & 0x40 == 0, so $C000 reverts to 0x00 (the pre-S7 keepRam path left 0x42).
    let after = read_byte(&engine, 0xC000);
    assert_ne!(after, 0x42, "cart eject wiped RAM (marker gone)");
    assert_eq!(after, 0x00, "cart eject left the power-on RAM pattern at $C000");

    let _ = std::fs::remove_file(&path);
}
