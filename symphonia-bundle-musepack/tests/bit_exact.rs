// Symphonia Musepack bit-exactness regression tests
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pins the exact bit pattern of every decoded `f32` sample.
//!
//! The golden hashes were recorded with the straightforward (pre-optimization) bit reader,
//! linear-scan Huffman decoding and lane-serial synthesis filter, so these tests guarantee that
//! performance work leaves the decoder output bit-identical. The hash is FNV-1a over the
//! little-endian bytes of the 32-bit words, in plane order per packet, as printed by
//! `examples/decode_bench.rs`.
//!
//! The fixture tests always run; the real-world sample tests run only if `RMPD_SAMPLES` points at
//! the sample directory (e.g. `RMPD_SAMPLES=/path/to/samples`).

use std::path::{Path, PathBuf};

use symphonia_bundle_musepack::{MpcDecoder, MpcReader};
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_testutil::{Fnv, open_mss, sample_dir};

/// Fully decodes `path` (gapless on); returns `(hash, frames)`.
fn decode_hash(path: &Path) -> (u64, u64) {
    let mss = open_mss(path);
    let mut reader = MpcReader::try_new(mss, FormatOptions::default()).expect("probe");
    let track = reader.tracks()[0].clone();
    let params = match track.codec_params.as_ref().expect("unexpected failure") {
        CodecParameters::Audio(p) => p.clone(),
        _ => panic!("not audio"),
    };
    let mut decoder =
        MpcDecoder::try_new(&params, &AudioDecoderOptions::default()).expect("unexpected failure");

    let mut hash = Fnv::new();
    let mut frames = 0u64;
    while let Some(packet) = reader.next_packet().expect("next_packet") {
        let Ok(GenericAudioBufferRef::F32(b)) = decoder.decode(&packet)
        else {
            continue;
        };
        frames += b.frames() as u64;
        for ch in 0..b.spec().channels().count() {
            for s in b.plane(ch).expect("unexpected failure") {
                hash.put(s.to_bits());
            }
        }
    }
    (hash.finish(), frames)
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join(name)
}

#[test]
fn sv8_fixtures_decode_bit_exactly() {
    assert_eq!(decode_hash(&fixture("fixture_mono_q2.mpc")), (0xa6aa283e3bb500b1, 88200));
    assert_eq!(decode_hash(&fixture("fixture_stereo_q5.mpc")), (0x064c7cf021c91cb7, 88200));
}

/// SV4 and SV5 files from the original encoders (`mppenc` 0.90-style SV4 and SV5), and the SV5
/// file rewritten as SV6 with a chosen last-frame sample count (which an SV5 encoder does not
/// write, so these are synthetic: frames up to the last one, then the 11-bit count). The hashes
/// are those of libmpcdec 1.2.6's output for the same files, which the decoder matches bit for
/// bit; the lengths follow its last-frame rules (the last one needs a silent flush frame when
/// its count is above 671).
#[test]
fn sv4_sv5_sv6_fixtures_decode_bit_exactly() {
    assert_eq!(decode_hash(&fixture("fixture_sv4_len5000.mpc")), (0x23486cfd0a9b3b63, 4127));
    assert_eq!(decode_hash(&fixture("fixture_sv5_len5000.mpc")), (0x1dd0780f17c43e92, 4127));
    assert_eq!(decode_hash(&fixture("fixture_sv6_len5000_m300.mpc")), (0x6e0bf2ce06fa82d3, 3756));
    assert_eq!(decode_hash(&fixture("fixture_sv6_len5000_m1000.mpc")), (0xb0152d0f45c47b27, 4456));
}

/// SV4-SV5 streams written by the original encoders with the features libmpcdec rejects, cut from
/// a 0.3 s (0.2 s for the CBR one) clip: intensity stereo (`-is`, from band 8), a superframe size
/// of 4 (11 frames in 3 groups, the last one partial) and a fixed 256 kbit/s bitrate (fixed-length
/// resolution codes). The hashes pin this decoder's output, which was checked against the original
/// `mppdec` of the era (16-bit output truncated from our `f32` matches it but for float rounding at
/// integer boundaries).
#[test]
fn sv4_sv5_encoder_feature_fixtures_decode_bit_exactly() {
    for (name, hash, frames) in [
        ("fixture_sv4_is.mpc", 0x1077d6669901c735, 12191),
        ("fixture_sv5_is.mpc", 0xcc05541c9836a311, 12191),
        ("fixture_sv5_cbr256.mpc", 0x9cba65c89b0d066a, 7583),
        ("fixture_sv4_frm4.mpc", 0x5737f42ad598c16, 12191),
        ("fixture_sv5_frm4.mpc", 0xce4f68ccd9d8f474, 12191),
    ] {
        assert_eq!(decode_hash(&fixture(name)), (hash, frames), "{name}");
    }
}

#[test]
fn real_samples_decode_bit_exactly() {
    let Some(dir) = sample_dir()
    else {
        eprintln!("RMPD_SAMPLES not set; skipping");
        return;
    };
    let cases: &[(&str, u64, u64)] = &[
        // SV8.
        ("musepack/mpc8_thumb.mpc", 0xdea4bd8c6ac93d94, 1_323_000),
        ("musepack/mpc8_mono.mpc", 0xac6e2b524d5e73a8, 1_323_000),
        ("musepack/mpc8_sample_Choral.mpc", 0x73f914a55de2e55e, 3_747_456),
        ("musepack/mpc8_32k.mpc", 0xedc10d0c320d1cad, 960_000),
        // SV7.
        ("musepack/mpc7_user_stp_acoustic.mpc", 0xffc187a094eaed85, 10_221_696),
        ("musepack/mpc7_sample_hungry.mpc", 0x31abde87b824171f, 9_049_320),
        ("tags/mpc_apev2_full.mpc", 0x0e933a570af1e177, 264_600),
    ];
    for &(name, hash, frames) in cases {
        let path = dir.join(name);
        if !path.is_file() {
            eprintln!("{name} missing; skipping");
            continue;
        }
        assert_eq!(decode_hash(&path), (hash, frames), "{name}");
    }
}
