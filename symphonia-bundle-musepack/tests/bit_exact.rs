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
//! the sample directory (e.g. `RMPD_SAMPLES=/home/gianluca/rmpd-samples/samples`).

use std::fs::File;
use std::path::{Path, PathBuf};

use symphonia_bundle_musepack::{MpcDecoder, MpcReader};
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;

/// Fully decodes `path` (gapless on); returns `(hash, frames)`.
fn decode_hash(path: &Path) -> (u64, u64) {
    let file = File::open(path).expect("open");
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut reader = MpcReader::try_new(mss, FormatOptions::default()).expect("probe");
    let track = reader.tracks()[0].clone();
    let params = match track.codec_params.as_ref().unwrap() {
        CodecParameters::Audio(p) => p.clone(),
        _ => panic!("not audio"),
    };
    let mut decoder = MpcDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut frames = 0u64;
    while let Some(packet) = reader.next_packet().expect("next_packet") {
        let Ok(GenericAudioBufferRef::F32(b)) = decoder.decode(&packet)
        else {
            continue;
        };
        frames += b.frames() as u64;
        for ch in 0..b.spec().channels().count() {
            for s in b.plane(ch).unwrap() {
                hash = (hash ^ u64::from(s.to_bits())).wrapping_mul(0x100_0000_01b3);
            }
        }
    }
    (hash, frames)
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join(name)
}

#[test]
fn sv8_fixtures_decode_bit_exactly() {
    assert_eq!(decode_hash(&fixture("fixture_mono_q2.mpc")), (0xa6aa283e3bb500b1, 88200));
    assert_eq!(decode_hash(&fixture("fixture_stereo_q5.mpc")), (0x064c7cf021c91cb7, 88200));
}

#[test]
fn real_samples_decode_bit_exactly() {
    let Ok(dir) = std::env::var("RMPD_SAMPLES")
    else {
        eprintln!("RMPD_SAMPLES not set; skipping");
        return;
    };
    let dir = Path::new(&dir);
    let cases: &[(&str, u64, u64)] = &[
        // SV8.
        ("musepack/mpc8_thumb.mpc", 0xdea4bd8c6ac93d94, 1_323_000),
        ("musepack/mpc8_mono.mpc", 0xac6e2b524d5e73a8, 1_323_000),
        ("musepack/mpc8_sample_Choral.mpc", 0x73f914a55de2e55e, 3_747_456),
        ("musepack/mpc8_32k.mpc", 0xedc10d0c320d1cad, 960_000),
        // SV7.
        ("musepack/mpc7_user_stp_acoustic.mpc", 0x2fdccb9227ca5d3e, 10_221_215),
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
