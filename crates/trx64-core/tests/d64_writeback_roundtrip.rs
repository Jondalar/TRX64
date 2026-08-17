//! Does a D64 survive our own GCR round trip untouched?
//!
//! The write-back serializes a WHOLE half-track: `write_dxx_half_track` decodes
//! every sector of the dirty track out of the GCR image and stores it into the
//! `.d64` bytes. So a drive that writes ONE sector rewrites the entire track, and
//! every OTHER sector on it is only as good as our encode→decode identity.
//!
//! That identity is what this checks, with no emulation in it at all: build the
//! GCR image from a real D64, hand a track straight back to the write-back, and
//! compare. Anything that differs was corrupted by the conversion, not by a write.
//!
//!   cargo test -p trx64-core --test d64_writeback_roundtrip -- --nocapture

use trx64_core::gcr::{GcrImage, WritebackKind};

const SAMPLES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/samples");

fn d64_sectors_per_track(t: u8) -> usize {
    match t {
        1..=17 => 21,
        18..=24 => 19,
        25..=30 => 18,
        _ => 17,
    }
}

fn track_offset(track: u8) -> usize {
    let mut off = 0usize;
    for t in 1..track {
        off += d64_sectors_per_track(t) * 256;
    }
    off
}

/// Round-trip every track of an image and report which sectors do not come back.
fn roundtrip(name: &str, bytes: &[u8]) -> usize {
    let img = GcrImage::from_d64(bytes);
    let mut damaged = 0usize;

    for track in 1..=35u8 {
        let mut out = bytes.to_vec();
        // The dirty half-track for `track` is `track * 2` (half-track numbering
        // starts at 2 for track 1), which is what the rotation engine records.
        img.write_half_track(WritebackKind::D64, &mut out, (track as usize) * 2, false);

        let base = track_offset(track);
        for sector in 0..d64_sectors_per_track(track) {
            let o = base + sector * 256;
            let a = &bytes[o..o + 256];
            let b = &out[o..o + 256];
            if a != b {
                let n = a.iter().zip(b).filter(|(x, y)| x != y).count();
                if damaged < 12 {
                    println!("  {name}: T{track}/S{sector} differs in {n} of 256 bytes");
                }
                damaged += 1;
            }
        }
    }
    damaged
}

#[test]
fn a_d64_survives_our_own_gcr_round_trip() {
    // A synthetic image first: every sector distinct, so a mix-up is visible.
    let mut synth = vec![0u8; 174_848];
    for (i, b) in synth.iter_mut().enumerate() {
        *b = ((i / 256) as u8) ^ ((i % 256) as u8);
    }
    let damaged = roundtrip("synthetic", &synth);
    assert_eq!(
        damaged, 0,
        "the write-back rewrites a WHOLE track, so every sector on it must decode \
         back to exactly what it was — {damaged} did not"
    );

    // ...and a real image, if one is around.
    let real = format!("{SAMPLES}/scramble_infinity.d64");
    if let Ok(bytes) = std::fs::read(&real) {
        let damaged = roundtrip("scramble", &bytes);
        assert_eq!(damaged, 0, "{damaged} sectors of a real D64 did not survive the round trip");
    } else {
        eprintln!("[skip] real-image half: {real} absent");
    }
}

/// A write-back must never turn a sector it cannot READ into rubbish in the image.
///
/// The write-back rewrites a WHOLE track, so every sector on it passes through the
/// GCR decode — including sectors the drive never touched. VICE writes a failed
/// decode and records the reason in the image's error-info map, creating the map
/// if there is none. We carry no map, so writing the failed decode was the
/// destructive half of that behaviour with the half that records it left out, and
/// it was silent.
///
/// What it cost: one game saving its high score turned four sectors of track 18 —
/// the BAM and three directory sectors — into GCR-looking rubbish. The BAM came
/// back with link $13$01 and DOS byte $12 where a BAM has $12$01 and 'A', and the
/// disk never booted again. Two runs of the same title against the same file:
/// the first worked, the second hung, because the first had eaten it.
#[test]
fn a_sector_that_cannot_be_decoded_keeps_its_bytes() {
    let mut d64 = vec![0u8; 174_848];
    for (i, b) in d64.iter_mut().enumerate() {
        *b = ((i / 256) as u8).wrapping_mul(7) ^ ((i % 256) as u8);
    }
    let track = 18u8;
    let base = track_offset(track);

    let mut img = GcrImage::from_d64(&d64);
    // Clean round trip first — the control. Without it a "nothing changed"
    // result below would prove nothing.
    let mut clean = d64.clone();
    img.write_half_track(WritebackKind::D64, &mut clean, (track as usize) * 2, false);
    assert_eq!(clean, d64, "the untouched track must round-trip byte-identically");

    // Now shred the GCR framing the way a stray write does: flip a run of bits in
    // the middle of the track. This is what a drive writing with its own framing
    // leaves behind for the sectors around it.
    let slot = (track as usize) * 2 - 2;
    let len = img.tracks[slot].data.len();
    for i in (len / 3)..(len / 3 + 400) {
        img.tracks[slot].data[i] ^= 0xff;
    }

    let mut out = d64.clone();
    let undecodable = img.write_half_track(WritebackKind::D64, &mut out, (track as usize) * 2, false);
    assert!(undecodable > 0, "the damage must actually make sectors undecodable");

    // Every sector of the track still holds exactly what it held before. The ones
    // that decoded decoded to the same bytes; the ones that did not were left
    // alone instead of being overwritten with a bad decode.
    let n = d64_sectors_per_track(track);
    for sector in 0..n {
        let o = base + sector * 256;
        assert_eq!(
            &out[o..o + 256],
            &d64[o..o + 256],
            "T{track}/S{sector} was changed by a write-back that could not read it"
        );
    }
    // ...and nothing outside the track moved either.
    assert_eq!(&out[..base], &d64[..base], "a track before this one changed");
    assert_eq!(&out[base + n * 256..], &d64[base + n * 256..], "a track after this one changed");
}

/// The other half of the contract: a sector the drive DID write must land.
/// A write-back that never writes anything would pass the test above.
#[test]
fn a_sector_the_drive_wrote_does_land_in_the_image() {
    let mut d64 = vec![0u8; 174_848];
    for (i, b) in d64.iter_mut().enumerate() {
        *b = ((i / 256) as u8) ^ ((i % 256) as u8);
    }
    let track = 18u8;
    let sector = 5u8;
    let base = track_offset(track);

    let mut img = GcrImage::from_d64(&d64);
    let slot = (track as usize) * 2 - 2;
    let fresh: Vec<u8> = (0..256).map(|i| (0xA0u16 + i as u16) as u8).collect();
    assert_eq!(
        trx64_core::gcr::gcr_write_sector(&mut img.tracks[slot], &fresh, sector),
        trx64_core::gcr::CBMDOS_FDC_ERR_OK,
        "the sector encode must succeed"
    );

    let mut out = d64.clone();
    let undecodable = img.write_half_track(WritebackKind::D64, &mut out, (track as usize) * 2, false);
    assert_eq!(undecodable, 0, "a well-formed track has no undecodable sector");

    let o = base + sector as usize * 256;
    assert_eq!(&out[o..o + 256], &fresh[..], "the written sector must reach the image");
    // and only that one moved
    for s in 0..d64_sectors_per_track(track) {
        if s == sector as usize { continue; }
        let q = base + s * 256;
        assert_eq!(&out[q..q + 256], &d64[q..q + 256], "T{track}/S{s} must be untouched");
    }
}
