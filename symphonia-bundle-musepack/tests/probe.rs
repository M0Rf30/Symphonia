// Symphonia Musepack integration tests
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_bundle_musepack::{MpcDecoder, MpcReader};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;

fn open(path: &str) -> Box<dyn FormatReader> {
    let mss = symphonia_testutil::open_mss(std::path::Path::new(path));
    Box::new(MpcReader::try_new(mss, FormatOptions::default()).expect("probe/parse header"))
}

fn decode_all(path: &str) -> usize {
    let mut reader = open(path);
    let track = reader.tracks()[0].clone();
    let params = match track.codec_params.as_ref().expect("codec params") {
        CodecParameters::Audio(p) => p.clone(),
        _ => panic!("expected audio"),
    };
    let mut decoder =
        MpcDecoder::try_new(&params, &AudioDecoderOptions::default()).expect("unexpected failure");

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
    let params = match track.codec_params.as_ref().expect("unexpected failure") {
        CodecParameters::Audio(p) => p.clone(),
        _ => unreachable!(),
    };
    let mut decoder =
        MpcDecoder::try_new(&params, &AudioDecoderOptions::default()).expect("unexpected failure");
    decoder.reset();
    let mut decoded_any = false;
    while let Some(packet) = reader.next_packet().expect("unexpected failure") {
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
    for fixture in [
        "tests/fixture_stereo_q5.mpc",
        "tests/fixture_sv4_len5000.mpc",
        "tests/fixture_sv5_len5000.mpc",
        "tests/fixture_sv6_len5000_m1000.mpc",
    ] {
        fuzz_fixture(fixture);
    }
}

fn fuzz_fixture(path: &str) {
    let mut data = std::fs::read(path).expect("fixture");

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
        let CodecParameters::Audio(params) = track.codec_params.expect("unexpected failure")
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
        // (SV4-SV6 have no magic: take a random header with an SV4, SV5 or SV6 version field.)
        match state2 % 4 {
            0 => noise[0..4].copy_from_slice(b"MPCK"),
            version => {
                let w = u32::from_le_bytes([noise[0], noise[1], noise[2], noise[3]]);
                let w = (w & !(0x3FF << 11)) | ((version as u32 + 3) << 11);
                noise[0..4].copy_from_slice(&w.to_le_bytes());
            }
        }
        let cursor = std::io::Cursor::new(noise);
        let mss = MediaSourceStream::new(Box::new(cursor), Default::default());
        let _ = MpcReader::try_new(mss, FormatOptions::default());
    }

    // Reaching here without a panic is the pass condition (data.len() kept for clarity/debug).
    let _ = data.len();
    data.clear();
}

/// SV4-SV6 streams have no magic number, so the probe finds them by a two-byte marker and
/// validates the frames; it must pick them out of the full probe, and not pick up other data.
#[test]
fn probe_finds_sv4_sv5_sv6_streams() {
    let mut probe = symphonia_core::formats::probe::Probe::default();
    probe.register_format::<MpcReader<'_>>();
    let hint = symphonia_core::formats::probe::Hint::new();

    for (name, frames) in [
        ("tests/fixture_sv4_len5000.mpc", 4127),
        ("tests/fixture_sv5_len5000.mpc", 4127),
        ("tests/fixture_sv6_len5000_m300.mpc", 3756),
        ("tests/fixture_sv4_is.mpc", 12191),
        ("tests/fixture_sv5_is.mpc", 12191),
        ("tests/fixture_sv5_cbr256.mpc", 7583),
        ("tests/fixture_sv4_frm4.mpc", 12191),
        ("tests/fixture_sv5_frm4.mpc", 12191),
    ] {
        let mss = symphonia_testutil::open_mss(std::path::Path::new(name));
        let reader = probe
            .probe(&hint, mss, FormatOptions::default(), Default::default())
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(reader.tracks()[0].num_frames, Some(frames), "{name}");
    }

    // Corrupting a frame's length makes the stream fail the validation.
    let mut data = std::fs::read("tests/fixture_sv5_len5000.mpc").expect("fixture");
    data[10] ^= 0x55;
    let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(data)), Default::default());
    assert!(probe.probe(&hint, mss, FormatOptions::default(), Default::default()).is_err());

    // Plain noise with the right version field is not a stream either.
    let mut state: u64 = 0x1234_5678_9ABC_DEF1;
    let mut noise = vec![0u8; 4096];
    for b in noise.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *b = state as u8;
    }
    noise[0..4].copy_from_slice(&0x0000_2A01u32.to_le_bytes()); // SV5, mid/side off, block size 1
    let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(noise)), Default::default());
    assert!(probe.probe(&hint, mss, FormatOptions::default(), Default::default()).is_err());
}
