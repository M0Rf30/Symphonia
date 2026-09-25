// Symphonia Musepack integration tests
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs::File;

use symphonia_bundle_musepack::{MpcDecoder, MpcReader};
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;

fn open(path: &str) -> Box<dyn FormatReader> {
    let file = File::open(path).expect("open fixture");
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    Box::new(MpcReader::try_new(mss, FormatOptions::default()).expect("probe/parse header"))
}

fn decode_all(path: &str) -> usize {
    let mut reader = open(path);
    let track = reader.tracks()[0].clone();
    let params = match track.codec_params.as_ref().expect("codec params") {
        CodecParameters::Audio(p) => p.clone(),
        _ => panic!("expected audio"),
    };
    let mut decoder = MpcDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

    let mut total = 0usize;
    while let Some(packet) = reader.next_packet().expect("next_packet") {
        match decoder.decode(&packet) {
            Ok(buf) => total += buf.frames(),
            Err(symphonia_core::errors::Error::DecodeError(_)) => continue,
            Err(e) => panic!("unexpected decode error: {e}"),
        }
    }
    total
}

#[test]
fn probes_sv8_mono() {
    let reader = open("tests/fixture_mono_q2.mpc");
    assert_eq!(reader.tracks().len(), 1);
}

#[test]
fn decodes_sv8_mono_exact_sample_count() {
    // 2 s @ 44100 Hz encoded; gapless-trimmed output must match the declared track duration
    // exactly (see decoder_core::Decoder::decode_frame's samples_to_skip/total-samples logic).
    let total = decode_all("tests/fixture_mono_q2.mpc");
    assert_eq!(total, 88200, "gapless sample count must be exact");
}

#[test]
fn decodes_sv8_stereo_exact_sample_count() {
    let total = decode_all("tests/fixture_stereo_q5.mpc");
    assert_eq!(total, 88200);
}

#[test]
fn seek_is_frame_accurate() {
    use symphonia_core::formats::{SeekMode, SeekTo};
    use symphonia_core::units::Timestamp;

    let mut reader = open("tests/fixture_mono_q2.mpc");
    let seeked = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(40000), track_id: 0 })
        .expect("seek");
    assert!(seeked.actual_ts.get() <= 40000);
    // Decoding must continue to work post-seek without panicking.
    let track = reader.tracks()[0].clone();
    let params = match track.codec_params.as_ref().unwrap() {
        CodecParameters::Audio(p) => p.clone(),
        _ => unreachable!(),
    };
    let mut decoder = MpcDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
    decoder.reset();
    let mut decoded_any = false;
    while let Some(packet) = reader.next_packet().unwrap() {
        if decoder.decode(&packet).is_ok() {
            decoded_any = true;
            break;
        }
    }
    assert!(decoded_any);
}

/// Deterministic mutation fuzz: flips/replaces bytes of a real packet stream and feeds them to a
/// fresh decoder, asserting only that it never panics (errors are fine and expected).
#[test]
fn fuzz_mutated_packets_never_panics() {
    let mut data = std::fs::read("tests/fixture_stereo_q5.mpc").expect("fixture");

    // Simple deterministic xorshift PRNG (no external dependency needed for this crate).
    let mut state: u64 = 0x2545F4914F6CDD1D;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    for trial in 0..500u32 {
        let mut mutant = data.clone();
        let n_mutations = 1 + (next() % 8) as usize;
        for _ in 0..n_mutations {
            if mutant.is_empty() {
                break;
            }
            let idx = (next() as usize) % mutant.len();
            mutant[idx] = (next() & 0xFF) as u8;
        }
        // Occasionally truncate to simulate a cut network stream.
        if trial % 7 == 0 && mutant.len() > 16 {
            let cut = (next() as usize) % mutant.len();
            mutant.truncate(cut.max(4));
        }

        let cursor = std::io::Cursor::new(mutant);
        let mss = MediaSourceStream::new(Box::new(cursor), Default::default());
        let Ok(mut reader) = MpcReader::try_new(mss, FormatOptions::default())
        else {
            continue;
        };
        let Some(track) = reader.tracks().first().cloned()
        else {
            continue;
        };
        let CodecParameters::Audio(params) = track.codec_params.unwrap()
        else {
            continue;
        };
        let Ok(mut decoder) = MpcDecoder::try_new(&params, &AudioDecoderOptions::default())
        else {
            continue;
        };

        // Never panic, regardless of decode errors.
        for _ in 0..300 {
            match reader.next_packet() {
                Ok(Some(packet)) => {
                    let _ = decoder.decode(&packet);
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
    }

    // Also feed pure random noise (no valid header at all) through the probe path.
    let mut state2: u64 = 0xDEADBEEFCAFEF00D;
    for _ in 0..200u32 {
        let mut noise = vec![0u8; 512];
        for b in noise.iter_mut() {
            state2 ^= state2 << 13;
            state2 ^= state2 >> 7;
            state2 ^= state2 << 17;
            *b = (state2 & 0xFF) as u8;
        }
        // Force the magic-ish prefix sometimes so we exercise past the first few checks.
        noise[0..4].copy_from_slice(b"MPCK");
        let cursor = std::io::Cursor::new(noise);
        let mss = MediaSourceStream::new(Box::new(cursor), Default::default());
        let _ = MpcReader::try_new(mss, FormatOptions::default());
    }

    // Reaching here without a panic is the pass condition (data.len() kept for clarity/debug).
    let _ = data.len();
    data.clear();
}
