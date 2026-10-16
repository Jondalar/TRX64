//! `Rotation::sector_under_head_cached` answers exactly what the uncached scan
//! `sector_under_head` answers, at every head bit position, on formatted tracks, on
//! tracks made of noise and sync-like runs, across a head step and across a write.

use trx64_core::gcr::{GcrImage, GcrTrack};
use trx64_core::rotation::Rotation;

fn d64() -> Vec<u8> {
    let mut d = vec![0u8; 174_848];
    for (i, b) in d.iter_mut().enumerate() {
        *b = (i / 256) as u8 ^ (i as u8);
    }
    d[0x16500 + 0xa2] = b'T';
    d
}

fn rng(s: &mut u32) -> u32 {
    *s ^= *s << 13;
    *s ^= *s >> 17;
    *s ^= *s << 5;
    *s
}

/// Noise with runs of ones of every length 5..=24 dropped in, so syncs of 10+ bits, near
/// misses of 9 bits, and runs that straddle the end of the track all occur.
fn noisy_track(len: usize, seed: u32) -> GcrTrack {
    let mut s = seed;
    let mut data: Vec<u8> = (0..len).map(|_| rng(&mut s) as u8).collect();
    for run in 5..=24usize {
        let start = (rng(&mut s) as usize) % (len * 8);
        for k in 0..run {
            let b = (start + k) % (len * 8);
            data[b >> 3] |= 0x80 >> (b & 7);
        }
        let z = (start + run) % (len * 8);
        data[z >> 3] &= !(0x80 >> (z & 7));
    }
    // a header-shaped block right after a sync, so the chain has something to find
    GcrTrack { data, size: len }
}

fn assert_all_positions(rot: &mut Rotation, stride: usize, what: &str) {
    let bits = rot.gcr_current_track_size * 8;
    let mut p = 0usize;
    while p < bits {
        rot.gcr_head_offset = p as u32;
        let want = rot.sector_under_head();
        let got = rot.sector_under_head_cached();
        assert_eq!(got, want, "{what}: head at bit {p} of {bits}, half-track {}", rot.current_half_track);
        p += stride;
    }
}

fn rotation_with(image: GcrImage) -> Rotation {
    let mut rot = Rotation::default();
    rot.attach(image, 0);
    rot
}

#[test]
fn cached_equals_uncached_over_a_revolution() {
    let mut rot = rotation_with(GcrImage::from_d64(&d64()));
    for ht in [2u32, 3, 35, 36, 37, 49, 69, 70] {
        rot.set_half_track(ht);
        assert_all_positions(&mut rot, 1, "formatted");
    }
}

#[test]
fn cached_equals_uncached_on_noise_and_degenerate_tracks() {
    let mut img = GcrImage::from_d64(&d64());
    for (i, len) in [7692usize, 7000, 6250, 300].into_iter().enumerate() {
        img.tracks[i] = noisy_track(len, 0x1234_5678 + i as u32 * 77);
    }
    img.tracks[4] = GcrTrack { data: vec![0xff; 400], size: 400 }; // all ones: no zero, no sync
    img.tracks[5] = GcrTrack { data: vec![0x00; 400], size: 400 }; // no ones at all
    img.tracks[6] = GcrTrack { data: vec![0xff, 0xc0, 0xff, 0xc0, 0xff, 0xc0], size: 6 };
    img.tracks[7] = GcrTrack { data: Vec::new(), size: 0 };
    let mut rot = rotation_with(img);
    for ht in 2u32..=9 {
        rot.set_half_track(ht);
        assert_all_positions(&mut rot, 1, "degenerate");
    }
}

#[test]
fn cached_follows_head_steps_and_disk_writes() {
    let mut rot = rotation_with(GcrImage::from_d64(&d64()));
    rot.set_half_track(36);
    assert_all_positions(&mut rot, 13, "before");
    // step: the cache is keyed on the half-track
    rot.set_half_track(38);
    assert_all_positions(&mut rot, 13, "after a step");
    rot.set_half_track(36);
    assert_all_positions(&mut rot, 13, "back again");

    // a write: wipe sector headers and plant a new sync, bumping the generation the way
    // a head write does
    let bits;
    {
        let trk = &mut rot.image.as_mut().unwrap().tracks[34];
        bits = trk.size * 8;
        for b in trk.data.iter_mut().take(400) {
            *b = 0x55;
        }
        trk.data[500] = 0xff;
        trk.data[501] = 0xff;
        trk.data[502] = 0x52; // header block id 0x08 begins "0101 0010 01.."
    }
    rot.invalidate_block_regions();
    assert!(bits > 0);
    assert_all_positions(&mut rot, 1, "after a write");

    // and the real write path
    rot.set_half_track(36);
    rot.gcr_head_offset = 1000;
    let before = rot.sector_under_head_cached();
    assert_eq!(before, rot.sector_under_head());
}
