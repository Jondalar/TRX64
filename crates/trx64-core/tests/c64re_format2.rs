//! c64re_format2.rs — Spec 919, `.c64re` format 2: scenarios S1-S9.
//!
//! S10 (the board's Wasteland file), S11 (the firmware's own reader) and S12 (the board) are
//! by hand. Everything here is synthetic: no game data, no sample file.
//!
//! Run: cargo test -p trx64-core --test c64re_format2

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use trx64_core::c64re_cart::{self, FW_CART_TABLE};
use trx64_core::c64re_snapshot::{capture_runtime_checkpoint, restore_runtime_checkpoint};
use trx64_core::c64re_v2::{self, SEC_BLOCK, SEC_CARTRAM, SEC_CARTROM, SEC_COLOR, SEC_DRIVE, SEC_EEPROM, SEC_RAM, SEC_TRX64};
use trx64_core::cart::{BankInfo, MapperType};
use trx64_core::drive::{DiskImage, DiskKind};
use trx64_core::drive_snapshot::{capture_drive1541, capture_drive_disk_image};
use trx64_core::native_snapshot::{
    read_native_snapshot, ta_u8, ta_u8_decode, write_native_snapshot, write_native_snapshot_v1, NativeSnapshotMediaInput,
    WriteNativeSnapshotArgs,
};
use trx64_core::Machine;

// ── fixtures ────────────────────────────────────────────────────────────────────────────────

fn build_crt(hw: u16, exrom: u8, game: u8, name: &str, chips: &[(u16, u16, Vec<u8>)]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"C64 CARTRIDGE   ");
    v.extend_from_slice(&0x40u32.to_be_bytes());
    v.extend_from_slice(&0x0100u16.to_be_bytes());
    v.extend_from_slice(&hw.to_be_bytes());
    v.push(exrom);
    v.push(game);
    v.extend_from_slice(&[0u8; 6]);
    let mut nm = [0u8; 32];
    nm[..name.len().min(32)].copy_from_slice(&name.as_bytes()[..name.len().min(32)]);
    v.extend_from_slice(&nm);
    for (bank, load, data) in chips {
        v.extend_from_slice(b"CHIP");
        v.extend_from_slice(&(0x10 + data.len() as u32).to_be_bytes());
        v.extend_from_slice(&2u16.to_be_bytes());
        v.extend_from_slice(&bank.to_be_bytes());
        v.extend_from_slice(&load.to_be_bytes());
        v.extend_from_slice(&(data.len() as u16).to_be_bytes());
        v.extend_from_slice(data);
    }
    v
}

fn pattern(seed: u8, n: usize) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed)).collect()
}

fn bi() -> BankInfo {
    BankInfo {
        cpu_port_direction: 0x2f,
        cpu_port_value: 0x37,
        basic_visible: true,
        kernal_visible: true,
        io_visible: true,
        char_visible: false,
        cartridge_attached: true,
        cartridge_exrom: None,
        cartridge_game: None,
        phi1: 0xff,
        ram_under: 0,
    }
}

/// The daemon's capture, for a core test: the cartridge's `.crt` and its writable image next to the
/// checkpoint.
fn capture(m: &mut Machine) -> Value {
    let drive1541 = capture_drive1541(&mut m.drive8);
    let disk_blob = capture_drive_disk_image(&m.drive8);
    let clk = m.c64_core.clk;
    let (cart_bytes, cart_flash) = if m.cartridge.is_some() {
        let cb = m.cartridge_image.as_ref().map(|i| i.raw_bytes.clone());
        let cf = m.cartridge.as_mut().and_then(|c| c.writable_image(clk));
        (cb, cf)
    } else {
        (None, None)
    };
    let disk_path = m.drive8.get_attached_disk().and_then(|d| d.backing_path.clone()).unwrap_or_default();
    capture_runtime_checkpoint(m, &disk_path, "", Some(&drive1541), disk_blob.as_deref(), cart_bytes.as_deref(), cart_flash.as_deref())
}

fn args(cp: Value) -> WriteNativeSnapshotArgs {
    WriteNativeSnapshotArgs {
        checkpoint: cp,
        schema_version: 1,
        media: vec![],
        runtime_version: "trx64/919-test".into(),
        machine_model: "c64-pal".into(),
        provenance: None,
        pc: 0,
        cycle: 0,
    }
}

fn ef_crt() -> Vec<u8> {
    build_crt(32, 1, 0, "EF919", &[(0, 0x8000, pattern(1, 0x2000)), (0, 0xA000, pattern(2, 0x2000)), (1, 0x8000, pattern(3, 0x2000)), (1, 0xA000, pattern(4, 0x2000))])
}

fn machine_with(crt: &[u8]) -> Machine {
    let mut m = Machine::new();
    m.attach_cart_from_bytes(crt, "t919").expect("attach");
    m
}

fn section<'a>(file: &'a [u8], lay: &c64re_v2::Layout, id: u8) -> Option<&'a [u8]> {
    lay.sections.iter().find(|s| s.id == id).map(|s| &file[s.off..s.off + s.len])
}

/// Rewrites a format 2 file: the header and the sections through `f`, then the digest.
fn rewrite(file: &[u8], f: impl FnOnce(&mut Value, &mut Vec<(u8, Vec<u8>)>)) -> Vec<u8> {
    let lay = c64re_v2::layout(file).expect("layout");
    let mut header = lay.header.clone();
    let mut secs: Vec<(u8, Vec<u8>)> = lay.sections.iter().map(|s| (s.id, file[s.off..s.off + s.len].to_vec())).collect();
    f(&mut header, &mut secs);
    assemble(&header, &secs)
}

fn pad4(n: usize) -> usize {
    (4 - (n & 3)) & 3
}

/// A format 2 container from the spec, written here and not by the code under test.
fn assemble(header: &Value, secs: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let h = serde_json::to_vec(header).unwrap();
    let mut body = Vec::new();
    body.extend_from_slice(&(h.len() as u32).to_le_bytes());
    body.extend_from_slice(&h);
    body.resize(body.len() + pad4(h.len()), 0);
    body.extend_from_slice(&(secs.len() as u16).to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    for (id, d) in secs {
        body.extend_from_slice(&[*id, 0, 0, 0]);
        body.extend_from_slice(&(d.len() as u32).to_le_bytes());
    }
    for (_, d) in secs {
        body.extend_from_slice(d);
        body.resize(body.len() + pad4(d.len()), 0);
    }
    resign(&body)
}

fn resign(body: &[u8]) -> Vec<u8> {
    let mut out = b"C64RESNP".to_vec();
    out.push(2);
    out.extend_from_slice(&Sha256::digest(body));
    out.extend_from_slice(body);
    out
}

// ── S1 ──────────────────────────────────────────────────────────────────────────────────────

#[test]
fn s1_format_1_still_loads() {
    let mut m = Machine::new();
    m.c64_core.reg_pc = 0x1234;
    m.c64_core.reg_a = 0x5A;
    m.ram[0x2000] = 0xC3;
    let cp = capture(&mut m);
    let v1 = write_native_snapshot_v1(args(cp.clone()));
    assert_eq!(v1[8], 1, "the fixture writes format 1");
    let r = read_native_snapshot(&v1).expect("format 1 still reads");
    assert_eq!(r.checkpoint, cp, "a format 1 file gives the checkpoint back as it always did");
    assert!(r.report.is_none());
    let mut m2 = Machine::new();
    restore_runtime_checkpoint(&mut m2, &r.checkpoint).expect("restore");
    assert_eq!((m2.c64_core.reg_pc, m2.c64_core.reg_a, m2.ram[0x2000]), (0x1234, 0x5A, 0xC3));
}

// ── S2 ──────────────────────────────────────────────────────────────────────────────────────

#[test]
fn s2_new_files_are_format_2_and_well_formed() {
    let mut m = machine_with(&ef_crt());
    let cp = capture(&mut m);
    let bytes = write_native_snapshot(args(cp));
    assert_eq!(&bytes[..8], b"C64RESNP");
    assert_eq!(bytes[8], 2, "byte 8 is the format");
    // the digest, by hand
    assert_eq!(bytes[9..41], Sha256::digest(&bytes[41..])[..], "sha256 of everything from byte 41");
    // the container, by hand: header, table, payloads in table order, each padded to 4
    let hl = u32::from_le_bytes(bytes[41..45].try_into().unwrap()) as usize;
    let header: Value = serde_json::from_slice(&bytes[45..45 + hl]).expect("header JSON");
    assert!(bytes[45..45 + hl].is_ascii(), "the header is ASCII");
    let mut o = 45 + hl + pad4(hl);
    let count = u16::from_le_bytes(bytes[o..o + 2].try_into().unwrap()) as usize;
    assert_eq!(&bytes[o + 2..o + 4], &[0, 0]);
    o += 4;
    let table: Vec<(u8, usize)> = (0..count)
        .map(|i| (bytes[o + i * 8], u32::from_le_bytes(bytes[o + i * 8 + 4..o + i * 8 + 8].try_into().unwrap()) as usize))
        .collect();
    o += count * 8;
    let mut ids = Vec::new();
    for (id, len) in &table {
        ids.push(*id);
        o += len;
        assert!(bytes[o..o + pad4(*len)].iter().all(|&b| b == 0), "padding is zero");
        o += pad4(*len);
    }
    assert_eq!(o, bytes.len(), "no bytes after the last section");
    assert_eq!(ids, vec![SEC_BLOCK, SEC_RAM, SEC_COLOR, SEC_CARTRAM, SEC_CARTROM, SEC_DRIVE, SEC_TRX64], "in order");
    let len_of = |id: u8| table.iter().find(|t| t.0 == id).unwrap().1;
    assert_eq!(len_of(SEC_BLOCK), 240);
    assert_eq!(len_of(SEC_RAM), 65536);
    assert_eq!(len_of(SEC_COLOR), 1024);
    assert_eq!(len_of(SEC_CARTRAM), 65536);
    assert_eq!(len_of(SEC_CARTROM), 1 << 20, "an EasyFlash is its whole 1 MiB chip");
    assert_eq!(len_of(SEC_DRIVE), 2092);
    // D13
    assert_eq!(header["kind"], "c64re");
    assert_eq!(header["version"], 2);
    assert_eq!(header["writer"], "trx64");
    assert_eq!(header["appVersion"], env!("CARGO_PKG_VERSION"));
    assert_eq!(header["machine"], "c64-pal");
    assert_eq!(header["blockWords"], 58);
    assert_eq!((header["bitstreamId"].as_u64(), header["caps"].as_u64(), header["caps2"].as_u64()), (Some(0), Some(0), Some(0)));
    assert_eq!(header["cartridgeType"], 17, "17 is the firmware's EasyFlash");
    assert_eq!(header["mediumBytes"], 1 << 20);
    assert_eq!(header["machineState"]["cartridge"]["type"], 17);
    assert_eq!(header["machineState"]["cartridge"]["require"], 64);
    assert_eq!(header["machineState"]["cartridge"]["prohibit"], 159);
    assert!(header["createdAt"].as_str().unwrap().ends_with('Z'));
    // and the container reader agrees
    assert!(c64re_v2::layout(&bytes).is_ok());
}

// ── S3 ──────────────────────────────────────────────────────────────────────────────────────

#[test]
fn s3_the_checkpoint_comes_back_exactly() {
    // plain, a banked cartridge, and a machine with a disk in drive A
    let mut plain = Machine::new();
    plain.c64_core.reg_pc = 0x0810;
    for i in 0..2000 {
        plain.ram[0x4000 + i] = (i * 3) as u8;
    }
    plain.ram[0xD800] = 0x5A; // RAM under the I/O is not the colour RAM
    let cp_plain = capture(&mut plain);

    let mut ef = machine_with(&ef_crt());
    let clk = ef.c64_core.clk;
    ef.cartridge.as_mut().unwrap().write(0xde02, 0x06, &bi(), clk);
    ef.cartridge.as_mut().unwrap().write(0xde00, 0x01, &bi(), clk);
    ef.cartridge.as_mut().unwrap().write(0xdf20, 0x5A, &bi(), clk);
    let cp_ef = capture(&mut ef);

    let mut disk = Machine::new();
    disk.drive8.attach_disk(DiskImage { kind: DiskKind::D64, bytes: vec![0u8; 174848], backing_path: Some("/nonexistent/919.d64".into()), read_only: false });
    let cp_disk = capture(&mut disk);

    for (name, cp) in [("plain", cp_plain), ("easyflash", cp_ef), ("disk", cp_disk)] {
        let r = read_native_snapshot(&write_native_snapshot(args(cp.clone()))).expect(name);
        assert_eq!(r.checkpoint, cp, "{name}: format 2 gives the checkpoint back exactly");
        assert!(r.report.is_none(), "{name}: a TRX64 file comes back whole, there is nothing to report");
        let mut m2 = Machine::new();
        restore_runtime_checkpoint(&mut m2, &r.checkpoint).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

// ── S4 ──────────────────────────────────────────────────────────────────────────────────────

fn chips_of(crt: &[u8]) -> Vec<(u16, u16, u16, usize)> {
    let mut v = Vec::new();
    let mut o = u32::from_be_bytes(crt[0x10..0x14].try_into().unwrap()) as usize;
    while o + 16 <= crt.len() && &crt[o..o + 4] == b"CHIP" {
        let len = u32::from_be_bytes(crt[o + 4..o + 8].try_into().unwrap()) as usize;
        v.push((
            u16::from_be_bytes(crt[o + 10..o + 12].try_into().unwrap()),
            u16::from_be_bytes(crt[o + 12..o + 14].try_into().unwrap()),
            u16::from_be_bytes(crt[o + 14..o + 16].try_into().unwrap()),
            o + 16,
        ));
        o += len;
    }
    v
}

#[test]
fn s4_the_cartridge_is_in_the_file_once() {
    let crt = ef_crt();
    let mut m = machine_with(&crt);
    // program one byte of bank 1's ROML through the AMD sequence (ultimax, bank 1)
    let clk = m.c64_core.clk;
    {
        let c = m.cartridge.as_mut().unwrap();
        c.write(0xde00, 0x01, &bi(), clk); // bank 1; EasyFlash boots in ultimax
        for (a, v) in [(0x8555u16, 0xaau8), (0x82aa, 0x55), (0x8555, 0xa0), (0x8123, 0x58)] {
            assert!(c.write(a, v, &bi(), clk));
        }
    }
    let cp = capture(&mut m);
    let written = ta_u8_decode(&cp["cartFlash"]).unwrap();
    assert_eq!(written[0x2000 + 0x123], pattern(3, 0x2000)[0x123] & 0x58, "the program ANDed into bank 1 ROML");
    let want = written[0x2000 + 0x123];
    let bytes = write_native_snapshot(args(cp.clone()));
    let lay = c64re_v2::layout(&bytes).unwrap();

    // section 5 holds the byte the game wrote: bank 1 ROML is at 1 * $4000
    let rom = section(&bytes, &lay, SEC_CARTROM).unwrap();
    assert_eq!(rom[0x4000 + 0x123], want);
    // the payload is in the file once: the TRX64 section carries neither cartBytes nor cartFlash
    let gz = section(&bytes, &lay, SEC_TRX64).unwrap();
    let mut raw = Vec::new();
    std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(gz), &mut raw).unwrap();
    let doc: Value = serde_json::from_slice(&raw).unwrap();
    assert!(doc["checkpoint"]["cartBytes"].is_null() && doc["checkpoint"]["cartFlash"].is_null());
    assert!(gz.len() < 200_000, "the TRX64 section is small ({} bytes): no second copy of the megabyte", gz.len());
    assert!(bytes.len() < (1 << 20) + 400_000, "file is {} bytes", bytes.len());

    // the restored machine reads it, and a CRT write-back keeps the original packet layout
    let r = read_native_snapshot(&bytes).unwrap();
    let mut m2 = Machine::new();
    restore_runtime_checkpoint(&mut m2, &r.checkpoint).unwrap();
    {
        let c = m2.cartridge.as_mut().unwrap();
        c.write(0xde00, 0x01, &bi(), 0);
        assert_eq!(c.read(0x8123, &bi(), 1_000_000), Some(want), "the restored machine reads the byte");
    }
    let clk2 = m2.c64_core.clk;
    let back = m2.cartridge.as_mut().unwrap().crt_image(clk2).expect("crt_image");
    assert_eq!(chips_of(&back), chips_of(&crt), "the write-back has the original packets");
    let (_, _, _, at) = chips_of(&back)[2];
    assert_eq!(back[at + 0x123], want);
}

// ── S5 ──────────────────────────────────────────────────────────────────────────────────────

#[test]
fn s5_a_disk_stands_alone() {
    let mut disk = vec![0u8; 174848];
    disk[0x16500] = 0x42;
    let mut m = Machine::new();
    m.drive8.attach_disk(DiskImage { kind: DiskKind::D64, bytes: disk.clone(), backing_path: Some("/nonexistent/s5.d64".into()), read_only: false });
    let cp = capture(&mut m);
    let mut a = args(cp);
    a.media = vec![NativeSnapshotMediaInput { role: "drive8".into(), format: "d64".into(), source_name: Some("s5.d64".into()), bytes: Some(disk.clone()), sha256: None }];
    let bytes = write_native_snapshot(a);
    // the path is in the header as on the board
    let lay = c64re_v2::layout(&bytes).unwrap();
    assert_eq!(lay.header["machineState"]["drive"]["image"], "/nonexistent/s5.d64");
    assert_eq!(lay.header["machineState"]["drive"]["mode"], "rw");
    // the image does not exist; the file alone brings the drive back
    assert!(!std::path::Path::new("/nonexistent/s5.d64").exists());
    let r = read_native_snapshot(&bytes).unwrap();
    assert_eq!(r.media.len(), 1);
    assert_eq!(r.media[0].bytes.as_deref(), Some(&disk[..]));
    let mut m2 = Machine::new();
    m2.drive8.attach_disk(DiskImage { kind: DiskKind::D64, bytes: r.media[0].bytes.clone().unwrap(), backing_path: None, read_only: false });
    restore_runtime_checkpoint(&mut m2, &r.checkpoint).unwrap();
    assert!(m2.drive8.get_attached_disk().is_some(), "the drive has its disk");
}

// ── S6 / S8: the board's file ───────────────────────────────────────────────────────────────

/// BLOCK bytes laid out from `trx_c64re_nodes.cc`: a CPU, a CIA1 with timer A latch 0x1234, a VIC
/// with sprite 0 at x=0x58, and an EasyFlash at bank 3.
fn board_block(cart: bool) -> Vec<u8> {
    let mut w = [0u32; 58];
    w[1] = 0x11 | (0x22 << 8) | (0x33 << 16) | (0xF0 << 24); // a x y sp
    w[2] = 0x0C12 | (0x24 << 16); // pc, p
    w[3] = 0x2F | (0x37 << 8) | (0x37 << 16); // port dir, value, as read
    w[4] = 0x58; // $D000: sprite 0 x
    // CIA1's hidden words start at word 34: timer A latch, timer A count, CRA, the ICR mask
    let h = 34;
    for (lo, n, v) in [(43usize, 16usize, 0x1234u32), (59, 16, 0x1200), (317, 8, 0x01), (311, 5, 0x01)] {
        for b in 0..n {
            let bit = lo + b;
            let (i, s) = (h + bit / 32, bit % 32);
            w[i] = (w[i] & !(1 << s)) | (((v >> b) & 1) << s);
        }
    }
    if cart {
        w[0] |= 0x08;
        w[26] = 3 | (0x06 << 8); // $DE00 = 3, $DE02 = 6
    }
    let mut out = Vec::new();
    for x in w {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.extend_from_slice(&119u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

fn board_file(cart: Option<(&[u8], u8)>, image: Option<&str>) -> Vec<u8> {
    let mut ram = vec![0u8; 65536];
    ram[0x0C12] = 0xEA;
    ram[0x3000] = 0x99;
    ram[0xD800] = 0x07; // RAM under the I/O
    let mut colour = vec![0u8; 1024];
    colour[5] = 0x0E;
    let mut secs = vec![(SEC_BLOCK, board_block(cart.is_some())), (SEC_RAM, ram), (SEC_COLOR, colour)];
    let mut header = json!({
        "kind": "c64re", "version": 2, "writer": "trx64u", "appVersion": "0.2.0", "machine": "c64-pal",
        "bitstreamId": 1, "caps": 7, "caps2": 3, "blockWords": 58, "createdAt": "2026-11-08T10:00:00Z",
        "frame": 1265, "cycles": 1265u64 * 19656 + 1, "pc": 0x0C12,
        "machineState": { "settings": { "uci": false, "reu": 0, "reuSize": 4, "sampler": false } },
    });
    if let Some((flat, fw)) = cart {
        secs.push((SEC_CARTRAM, vec![0u8; 65536]));
        secs.push((SEC_CARTROM, flat.to_vec()));
        header["cartridgeType"] = json!(fw);
        header["mediumBytes"] = json!(flat.len());
        header["machineState"]["cartridge"] = json!({ "type": fw, "bytes": flat.len(), "require": 64, "prohibit": 159, "source": "/SD/C64/nothing.crt" });
    }
    secs.push((SEC_DRIVE, vec![0u8; 2092]));
    if let Some(i) = image {
        header["machineState"]["drive"] = json!({ "image": i, "mode": "ro" });
    }
    assemble(&header, &secs)
}

#[test]
fn s6_a_synthetic_board_file_loads_what_it_holds_and_reports_the_rest() {
    let mut flat = vec![0xFFu8; 64 * 0x4000];
    flat[3 * 0x4000 + 0x10] = 0xAB; // bank 3 ROML
    flat[0x2000] = 0xCD; // bank 0 ROMH
    let file = board_file(Some((&flat, 17)), None);
    let lay = c64re_v2::layout(&file).unwrap();
    assert!(lay.sections.iter().all(|s| s.id != SEC_TRX64), "the board's file has no section 128");
    let r = read_native_snapshot(&file).expect("a board file reads");
    let cp = &r.checkpoint;
    assert_eq!((cp["cpu"]["pc"].as_i64(), cp["cpu"]["a"].as_i64(), cp["cpu"]["x"].as_i64(), cp["cpu"]["y"].as_i64(), cp["cpu"]["sp"].as_i64()), (Some(0x0C12), Some(0x11), Some(0x22), Some(0x33), Some(0xF0)));
    assert_eq!(cp["cpu"]["flags"], 0x24);
    assert_eq!(cp["cpu"]["cycles"], 1265u64 * 19656 + 1);
    assert_eq!(ta_u8_decode(&cp["ram"]).unwrap()[0x3000], 0x99);
    assert_eq!(ta_u8_decode(&cp["ram"]).unwrap()[0xD800], 0x07, "RAM under the I/O stays RAM");
    assert_eq!(cp["vic"]["color_ram"][5], 0x0E);
    assert_eq!(cp["vic"]["regs"][0], 0x58);
    assert_eq!((cp["vic"]["raster_line"].as_i64(), cp["vic"]["raster_cycle"].as_i64()), (Some(0), Some(1)), "the cycle count modulo the frame");
    assert_eq!(cp["cia1"]["ta_latch"], 0x1234);
    assert_eq!(cp["cia1"]["c_cia"][14], 0, "register image");
    assert_eq!(cp["cartState"]["currentBank"], 3);
    assert_eq!(cp["cartState"]["controlRegister"], 6);
    // the cartridge came out of section 5
    let flash = ta_u8_decode(&cp["cartFlash"]).unwrap();
    assert_eq!(flash[3 * 0x2000 + 0x10], 0xAB, "bank 3 in the lo flash");
    assert_eq!(flash[64 * 0x2000], 0xCD, "bank 0 ROMH is the first byte of the hi flash");

    // it restores into a machine, and the machine stands where the file says
    let mut m = Machine::new();
    restore_runtime_checkpoint(&mut m, &r.checkpoint).expect("restore");
    assert_eq!((m.c64_core.reg_pc, m.c64_core.reg_a, m.c64_core.reg_sp), (0x0C12, 0x11, 0xF0));
    assert_eq!(m.ram[0x3000], 0x99);
    assert_eq!(m.cartridge.as_ref().unwrap().get_state().current_bank, 3);
    assert_eq!(m.vic.regs[0], 0x58);
    assert_eq!(m.cia1.ta.latch, 0x1234);

    // D12: the report names what is not there
    let rep = r.report.as_ref().expect("a board file carries a report");
    let all = |v: &Vec<String>| v.join("|");
    assert!(all(&rep.absent).contains("SID"), "{rep:?}");
    assert!(all(&rep.absent).contains("VIC-II internals"), "{rep:?}");
    assert!(all(&rep.absent).contains("DRIVE8 mechanics"), "{rep:?}");
    assert!(all(&rep.loaded).contains("MAINCPU") && all(&rep.loaded).contains("C64CART"), "{rep:?}");
    assert!(all(&rep.coarse).contains("CIA1"), "{rep:?}");
    assert_eq!(rep.fidelity.as_str(), "partial");
}

#[test]
fn s8_an_image_that_is_not_there_leaves_the_drive_empty() {
    let file = board_file(None, Some("/SD/x.d64"));
    let r = read_native_snapshot(&file).expect("never refused for a missing image");
    assert!(r.media.is_empty(), "no drive8 media");
    let rep = r.report.unwrap();
    assert!(rep.absent.iter().any(|a| a.contains("/SD/x.d64") && a.contains("empty")), "{:?}", rep.absent);
    assert!(r.checkpoint["cartBytes"].is_null());
    // and when the image is there, it is mounted
    let dir = std::env::temp_dir().join(format!("trx64-919-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("here.d64");
    std::fs::write(&img, vec![0u8; 174848]).unwrap();
    let file = board_file(None, Some(img.to_str().unwrap()));
    let r = read_native_snapshot(&file).unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(r.media.len(), 1);
    assert_eq!(r.media[0].reference.role, "drive8");
    assert_eq!(r.media[0].bytes.as_ref().map(|b| b.len()), Some(174848));
}

// ── S7 ──────────────────────────────────────────────────────────────────────────────────────

#[test]
fn s7_refusals_by_name() {
    let mut m = Machine::new();
    let good = write_native_snapshot(args(capture(&mut m)));
    assert!(read_native_snapshot(&good).is_ok());

    // a bad digest
    let mut bad = good.clone();
    let last = bad.len() - 1;
    bad[last] ^= 1;
    assert!(read_native_snapshot(&bad).unwrap_err().contains("integrity"));
    let mut bad = good.clone();
    bad[45] ^= 1;
    assert!(read_native_snapshot(&bad).unwrap_err().contains("integrity"));

    // a block this reader cannot lay out
    let f = rewrite(&good, |h, _| h["blockWords"] = json!(247));
    let e = read_native_snapshot(&f).unwrap_err();
    assert!(e.contains("blockWords 247") && e.contains("58"), "{e}");
    let f = rewrite(&good, |h, _| {
        h.as_object_mut().unwrap().remove("blockWords");
    });
    assert!(read_native_snapshot(&f).unwrap_err().contains("blockWords"));

    // a section of the wrong length is named
    let f = rewrite(&good, |_, s| s.iter_mut().find(|x| x.0 == SEC_RAM).unwrap().1.truncate(1000));
    let e = read_native_snapshot(&f).unwrap_err();
    assert!(e.contains("RAM") && e.contains("65536"), "{e}");
    let f = rewrite(&good, |_, s| s.iter_mut().find(|x| x.0 == SEC_BLOCK).unwrap().1.push(0));
    assert!(read_native_snapshot(&f).unwrap_err().contains("BLOCK"));

    // a truncated table, and a section that runs past the end
    let lay = c64re_v2::layout(&good).unwrap();
    let table_at = lay.sections[0].off - lay.sections.len() * 8;
    let f = resign(&good[41..table_at + 12]);
    let e = read_native_snapshot(&f).unwrap_err();
    assert!(e.contains("section table") || e.contains("runs past"), "{e}");
    let mut body = good[41..].to_vec();
    let entry = table_at - 41 + 8 * 1 + 4; // the RAM entry's length
    body[entry..entry + 4].copy_from_slice(&0x00FF_FFFFu32.to_le_bytes());
    let e = read_native_snapshot(&resign(&body)).unwrap_err();
    assert!(e.contains("RAM") && e.contains("past the end"), "{e}");

    // bytes after the last section, and a header that is not JSON
    let mut body = good[41..].to_vec();
    body.extend_from_slice(&[0, 0, 0, 0]);
    assert!(read_native_snapshot(&resign(&body)).unwrap_err().contains("after the last section"));
    let mut body = good[41..].to_vec();
    body[4] = b'#';
    assert!(read_native_snapshot(&resign(&body)).unwrap_err().contains("not JSON"));

    // an unknown section id is skipped by its length
    let f = rewrite(&good, |_, s| {
        s.push((99, vec![7u8; 13]));
        s.sort_by_key(|x| x.0);
    });
    let r = read_native_snapshot(&f).expect("an unknown id is skipped");
    assert!(r.checkpoint["ram"].is_object());

    // another format, another magic
    let mut f = good.clone();
    f[8] = 9;
    assert!(read_native_snapshot(&f).unwrap_err().contains("format version 9"));
    let mut f = good.clone();
    f[0] = b'X';
    assert!(read_native_snapshot(&f).unwrap_err().contains("magic"));
}

// ── S9 ──────────────────────────────────────────────────────────────────────────────────────

#[test]
fn s9_the_type_table_maps_both_ways() {
    for (byte, mapper) in FW_CART_TABLE {
        assert_eq!(c64re_cart::mapper_of_fw(*byte), Some(*mapper), "{byte:02x} -> mapper");
        assert_eq!(c64re_cart::fw_of_mapper(*mapper, false), Some(*byte), "{mapper:?} -> byte");
    }
    // the two the spec names
    assert_eq!(c64re_cart::mapper_of_fw(0x11), Some(MapperType::EasyFlash));
    assert_eq!(c64re_cart::mapper_of_fw(0x2F), Some(MapperType::TwoMegabyter));
    // an Ocean with $A000 chips is the board's 0x69 and reads back as Ocean
    assert_eq!(c64re_cart::fw_of_mapper(MapperType::Ocean, true), Some(0x69));
    assert_eq!(c64re_cart::mapper_of_fw(0x69), Some(MapperType::Ocean));
    assert_eq!(c64re_cart::mapper_of_fw(0x00), None);
    assert_eq!(c64re_cart::fw_of_mapper(MapperType::Gmod4, false), None, "a type the firmware has no byte for");
}

// ── the cartridge, type by type ─────────────────────────────────────────────────────────────

fn flash_crt(m: MapperType) -> Vec<u8> {
    match m {
        MapperType::EasyFlash => build_crt(32, 1, 0, "EF", &[(0, 0x8000, pattern(1, 0x2000)), (0, 0xA000, pattern(2, 0x2000)), (5, 0x8000, pattern(3, 0x2000))]),
        MapperType::EasyFlashXl => build_crt(232, 1, 0, "EFXL", &[(0, 0x8000, pattern(1, 0x2000)), (200, 0xA000, pattern(2, 0x2000))]),
        MapperType::Gmod2 => build_crt(60, 0, 1, "GMOD2", &[(0, 0x8000, pattern(1, 0x2000)), (9, 0x8000, pattern(3, 0x2000))]),
        MapperType::MegaByter => build_crt(86, 0, 1, "MB", &[(0, 0x8000, pattern(1, 0x2000)), (100, 0x8000, pattern(3, 0x2000))]),
        MapperType::C64MegaCart => build_crt(88, 0, 1, "MC", &[(0, 0x8000, pattern(1, 0x2000)), (250, 0x8000, pattern(3, 0x2000))]),
        MapperType::TwoMegabyter => build_crt(89, 0, 0, "2MB", &[(0, 0x8000, pattern(1, 0x2000)), (0, 0xA000, pattern(2, 0x2000)), (77, 0x8000, pattern(3, 0x2000))]),
        MapperType::EasyFlash3 => build_crt(90, 1, 0, "EF3", &[(0, 0x8000, pattern(1, 0x2000)), (0, 0xA000, pattern(2, 0x2000)), (300, 0x8000, pattern(3, 0x2000))]),
        _ => unreachable!(),
    }
}

#[test]
fn every_flash_geometry_round_trips_exactly() {
    for mapper in [
        MapperType::EasyFlash,
        MapperType::EasyFlashXl,
        MapperType::Gmod2,
        MapperType::MegaByter,
        MapperType::C64MegaCart,
        MapperType::TwoMegabyter,
        MapperType::EasyFlash3,
    ] {
        let crt = flash_crt(mapper);
        let mut m = Machine::new();
        m.attach_cart_from_bytes(&crt, "geom").unwrap_or_else(|e| panic!("{mapper:?}: {e:?}"));
        assert_eq!(m.cartridge.as_ref().unwrap().mapper_type(), mapper);
        let cp = capture(&mut m);
        let bytes = write_native_snapshot(args(cp.clone()));
        let lay = c64re_v2::layout(&bytes).unwrap();
        let fw = c64re_cart::fw_of_mapper(mapper, false).unwrap();
        assert_eq!(lay.header["cartridgeType"], fw, "{mapper:?} has a section 5");
        let r = read_native_snapshot(&bytes).unwrap();
        assert_eq!(r.checkpoint["cartFlash"], cp["cartFlash"], "{mapper:?}: the flash comes back");
        assert_eq!(r.checkpoint["cartBytes"], cp["cartBytes"], "{mapper:?}: the .crt comes back");
        assert_eq!(r.checkpoint, cp, "{mapper:?}: the whole checkpoint");
        if mapper == MapperType::Gmod2 {
            assert!(lay.sections.iter().any(|s| s.id == SEC_EEPROM), "a GMod2 brings its EEPROM (section 7)");
        }
    }
}

#[test]
fn a_read_only_cartridge_is_the_file_exactly() {
    for (name, crt, mapper) in [
        ("ocean", build_crt(5, 0, 0, "OCEAN", &[(0, 0x8000, pattern(1, 0x2000)), (1, 0x8000, pattern(2, 0x2000)), (2, 0xA000, pattern(3, 0x2000))]), MapperType::Ocean),
        ("magicdesk", build_crt(19, 0, 1, "MD", &[(0, 0x8000, pattern(1, 0x2000)), (3, 0x8000, pattern(2, 0x2000))]), MapperType::MagicDesk),
        ("normal16k", build_crt(0, 0, 0, "N16", &[(0, 0x8000, pattern(1, 0x4000))]), MapperType::Normal16k),
        ("ultimax", build_crt(0, 1, 0, "UM", &[(0, 0xE000, pattern(1, 0x2000))]), MapperType::Ultimax),
        ("small chip", build_crt(5, 0, 0, "SMALL", &[(0, 0x8000, pattern(1, 0x1000))]), MapperType::Ocean),
    ] {
        let mut m = Machine::new();
        m.attach_cart_from_bytes(&crt, name).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(m.cartridge.as_ref().unwrap().mapper_type(), mapper);
        let cp = capture(&mut m);
        let bytes = write_native_snapshot(args(cp.clone()));
        let lay = c64re_v2::layout(&bytes).unwrap();
        assert!(lay.header.get("cartridgeType").is_some(), "{name}");
        let rom = section(&bytes, &lay, SEC_CARTROM).unwrap();
        assert_eq!(rom.len() % 0x4000, 0);
        let r = read_native_snapshot(&bytes).unwrap();
        assert_eq!(r.checkpoint, cp, "{name}");
    }
}

#[test]
fn a_cartridge_the_firmware_has_no_byte_for_stays_in_the_trx64_section() {
    // GMod4 (CRT 87): the board's table has nothing for it
    let crt = build_crt(87, 1, 0, "GMOD4", &[(0, 0x8000, pattern(9, 0x2000))]);
    let mut m = Machine::new();
    m.attach_cart_from_bytes(&crt, "gmod4").expect("attach");
    assert_eq!(m.cartridge.as_ref().unwrap().mapper_type(), MapperType::Gmod4);
    let cp = capture(&mut m);
    let bytes = write_native_snapshot(args(cp.clone()));
    let lay = c64re_v2::layout(&bytes).unwrap();
    assert!(lay.header.get("cartridgeType").is_none(), "cartridgeType is absent");
    assert!(lay.sections.iter().all(|s| s.id != SEC_CARTROM && s.id != SEC_CARTRAM), "no section 5, no section 4");
    let r = read_native_snapshot(&bytes).unwrap();
    assert_eq!(r.checkpoint, cp, "its bytes came back from the TRX64 section");
}

#[test]
fn a_toy_checkpoint_loses_nothing() {
    // no 64 KiB RAM, no cpu: everything that cannot go in a section stays in the TRX64 section
    let cp = json!({ "schemaVersion": 1, "cpu": { "pc": 0xabcd, "cycles": 123456 }, "ram": ta_u8(&[1, 2, 3, 4, 0xff]) });
    let bytes = write_native_snapshot(args(cp.clone()));
    assert_eq!(read_native_snapshot(&bytes).unwrap().checkpoint, cp);
}
