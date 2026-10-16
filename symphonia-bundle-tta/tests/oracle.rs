// Symphonia TTA integration tests
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bit-exactness against FFmpeg's TTA decoder. The tests are skipped when `ffmpeg` is not in the
//! `PATH`. `TTA_CORPUS` names a directory of further `.tta` files to compare.

mod common;

use common::*;
use symphonia_testutil::{XorShift64, sample_path};

fn compare(label: &str, data: Vec<u8>) {
    let ours = decode(data.clone(), false);

    let Some(theirs) = ffmpeg_pcm(&data, ours.bytes_per_sample)
    else {
        eprintln!("ffmpeg not found; skipping {label}");
        return;
    };

    assert_eq!(ours.errors, 0, "{label}: rejected packets");
    assert_eq!(ours.pcm.len(), theirs.len(), "{label}: length in bytes");
    assert!(ours.pcm == theirs, "{label}: the samples differ");
}

#[test]
fn fixtures_match_ffmpeg() {
    for name in FIXTURES {
        compare(name, fixture(name));
    }
}

#[test]
fn sample_with_tags_matches_ffmpeg() {
    let Some(path) = sample_path("unsupported/tta.tta")
    else {
        return;
    };
    compare("tta.tta", std::fs::read(path).expect("read"));
}

#[test]
fn corpus_matches_ffmpeg() {
    let Some(dir) = std::env::var_os("TTA_CORPUS")
    else {
        eprintln!("TTA_CORPUS not set; skipping");
        return;
    };

    let mut names: Vec<_> = std::fs::read_dir(dir)
        .expect("read corpus")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "tta"))
        .collect();
    names.sort();

    for path in names {
        compare(&path.display().to_string(), std::fs::read(&path).expect("read"));
    }
}

/// Flips bits in the frames: the data decodes to something else but never fails the reader, and
/// both decoders must agree, because the CRC of the frame is not checked.
#[test]
fn corrupt_frames_match_ffmpeg() {
    let mut rng = XorShift64(0x7a11_c0de_5eed_0001);

    for name in ["mono_s16_8k", "stereo_s16_8k", "stereo_u8_8k", "stereo_s24_8k", "ch6_s16_8k"] {
        let original = fixture(name);
        let first_frame = first_frame_pos(&original);

        for trial in 0..24 {
            let mut data = original.clone();

            for _ in 0..1 + rng.below(4) {
                let at = first_frame + rng.below(data.len() - first_frame);
                data[at] ^= 1 << rng.below(8);
            }

            compare_lenient(&format!("{name} trial {trial}"), data);
        }
    }
}

/// Like [`compare`], but a packet may be rejected, and ffmpeg then writes nothing for it.
fn compare_lenient(label: &str, data: Vec<u8>) {
    let ours = decode(data.clone(), false);

    let Some(theirs) = ffmpeg_pcm(&data, ours.bytes_per_sample)
    else {
        return;
    };

    assert_eq!(ours.pcm.len(), theirs.len(), "{label}: length in bytes");
    assert!(ours.pcm == theirs, "{label}: the samples differ");
}

#[test]
fn truncated_streams_match_ffmpeg() {
    for name in ["mono_s16_8k", "stereo_s16_8k", "ch3_s16_8k"] {
        let original = fixture(name);
        let first_frame = first_frame_pos(&original);

        for cut in [first_frame + 1, first_frame + 100, original.len() * 3 / 4, original.len() - 5] {
            let data = original[..cut].to_vec();
            compare_lenient(&format!("{name} cut at {cut}"), data);
        }
    }
}
