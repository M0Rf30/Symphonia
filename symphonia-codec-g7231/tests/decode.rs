// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Decodes raw G.723.1 frame streams (the format of `ffmpeg -f g723_1`) and compares the output
//! with the FFmpeg `g723_1` decoder. The decoder is fixed-point, so the output must match bit for
//! bit. See `tests/fixtures/README.md` for the provenance of the streams.
//!
//! Each fixture is checked against a pinned FNV-1a hash of the FFmpeg output and, when `ffmpeg` is
//! on `PATH`, against the live FFmpeg output.

use std::path::Path;
use std::process::Command;

use symphonia_codec_g7231::G7231Decoder;
use symphonia_core::codecs::audio::well_known::CODEC_ID_G723_1;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

const FRAME_SIZE: [usize; 4] = [24, 20, 4, 1];

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)).unwrap()
}

fn decoder() -> G7231Decoder {
    let mut p = AudioCodecParameters::new();
    p.for_codec(CODEC_ID_G723_1).with_sample_rate(8000);
    G7231Decoder::try_new(&p, &AudioDecoderOptions::default()).unwrap()
}

fn packet(data: &[u8]) -> PacketRef<'_> {
    PacketRef::new(0, Timestamp::new(0), Duration::new(0), data)
}

/// Splits a raw stream into frames using the size given by the two LSBs of the first byte.
fn split_frames(mut data: &[u8]) -> Vec<&[u8]> {
    let mut v = Vec::new();
    while let Some(&b) = data.first() {
        let n = FRAME_SIZE[usize::from(b & 3)].min(data.len());
        v.push(&data[..n]);
        data = &data[n..];
    }
    v
}

/// Decodes the stream with `per_packet` frames in each packet.
fn decode(data: &[u8], per_packet: usize) -> Vec<i16> {
    let mut dec = decoder();
    let mut out = Vec::new();
    let mut tmp = Vec::new();

    let frames = split_frames(data);
    for group in frames.chunks(per_packet) {
        let bytes: Vec<u8> = group.concat();
        dec.decode_ref(&packet(&bytes)).unwrap().copy_to_vec_interleaved::<i16>(&mut tmp);
        out.extend_from_slice(&tmp);
    }
    out
}

fn fnv(samples: &[i16]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for s in samples {
        for b in s.to_le_bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// Decodes `name` with the FFmpeg decoder, or returns `None` if `ffmpeg` is not available.
fn ffmpeg_reference(name: &str) -> Option<Vec<i16>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-f", "g723_1", "-i"])
        .arg(path)
        .args(["-f", "s16le", "-"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout.chunks_exact(2).map(|b| i16::from_le_bytes(b.try_into().unwrap())).collect())
}

fn check(name: &str, frames: usize, hash: u64) {
    let data = fixture(name);
    let out = decode(&data, 1);
    assert_eq!(out.len(), frames * 240, "{name}: sample count");
    assert_eq!(fnv(&out), hash, "{name}: hash of the FFmpeg output");

    if let Some(reference) = ffmpeg_reference(name) {
        assert_eq!(out, reference, "{name}: differs from live ffmpeg");
    }
}

/// 500 frames of 6.3 kbit/s speech (`g723_1.wav` from samples.ffmpeg.org).
#[test]
fn real_6300_matches_ffmpeg() {
    check("real_6300.g723", 500, 0x72c3_9893_9715_e60f);
}

/// 379 frames of 5.3 kbit/s speech (`format-0x42-speechtest-MSG5.3.asf` from samples.ffmpeg.org).
#[test]
fn real_5300_matches_ffmpeg() {
    check("real_5300.g723", 379, 0xe943_2d03_4939_9466);
}

/// 234 frames encoded by the FFmpeg `g723_1` encoder.
#[test]
fn encoded_6300_matches_ffmpeg() {
    check("enc_6300.g723", 234, 0xadde_f5aa_7eb4_a19a);
}

/// Rate switches, SID frames, untransmitted frames and erased frames (forbidden pitch lag or gain
/// codes) after active, SID and untransmitted frames.
#[test]
fn sid_untransmitted_and_erased_frames_match_ffmpeg() {
    check("mixed.g723", 190, 0x9c17_81d4_6059_664a);
}

/// Random frames of every type: exercises extreme parameter values and many erasures.
#[test]
fn random_frames_match_ffmpeg() {
    check("random.g723", 500, 0x47fa_576c_d471_5cc6);
    check("random2.g723", 300, 0xe086_f3df_4da4_6199);
}

/// A packet holding several frames decodes exactly like the same frames in separate packets.
#[test]
fn multi_frame_packets() {
    for name in ["real_6300.g723", "mixed.g723", "random.g723"] {
        let data = fixture(name);
        let single = decode(&data, 1);
        for n in [2, 3, 7, 1000] {
            assert_eq!(decode(&data, n), single, "{name}: {n} frames per packet");
        }
    }
}

/// Decoding after a reset equals decoding from a fresh decoder.
#[test]
fn reset_restores_initial_state() {
    let data = fixture("mixed.g723");
    let frames = split_frames(&data);

    let mut dec = decoder();
    let mut a = Vec::new();
    for f in &frames[..40] {
        dec.decode_ref(&packet(f)).unwrap().copy_to_vec_interleaved::<i16>(&mut a);
    }
    dec.reset();

    let mut tmp = Vec::new();
    let mut after = Vec::new();
    for f in &frames {
        dec.decode_ref(&packet(f)).unwrap().copy_to_vec_interleaved::<i16>(&mut tmp);
        after.extend_from_slice(&tmp);
    }
    assert_eq!(after, decode(&data, 1));
}

#[test]
fn truncated_packets_are_rejected() {
    let data = fixture("real_6300.g723");
    let mut dec = decoder();

    // Empty packet.
    assert!(dec.decode_ref(&packet(&[])).is_err());
    // A 6.3 kbit/s frame cut short.
    assert!(dec.decode_ref(&packet(&data[..23])).is_err());
    // A SID frame cut short.
    assert!(dec.decode_ref(&packet(&[0x02, 0, 0])).is_err());

    // A complete frame followed by a partial one: only the complete frame is decoded.
    let mut tmp = Vec::new();
    dec.decode_ref(&packet(&data[..30])).unwrap().copy_to_vec_interleaved::<i16>(&mut tmp);
    assert_eq!(tmp.len(), 240);

    // The decoder still works.
    dec.decode_ref(&packet(&data[..24])).unwrap();
}

/// Arbitrary garbage must never panic and always yield one frame per whole frame of input.
#[test]
fn garbage_never_panics() {
    let mut dec = decoder();
    let mut state = 0x1234_5678_9abc_def0u64;
    let mut tmp = Vec::new();
    for len in 1..200usize {
        let mut data = vec![0u8; len];
        for b in &mut data {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *b = (state >> 24) as u8;
        }
        match dec.decode_ref(&packet(&data)) {
            Ok(buf) => {
                buf.copy_to_vec_interleaved::<i16>(&mut tmp);
                assert_eq!(tmp.len() % 240, 0);
                assert!(!tmp.is_empty());
            }
            Err(_) => assert!(data.len() < FRAME_SIZE[usize::from(data[0] & 3)]),
        }
    }
}

#[test]
fn rejects_stereo() {
    let mut p = AudioCodecParameters::new();
    p.for_codec(CODEC_ID_G723_1)
        .with_sample_rate(8000)
        .with_channels(symphonia_core::audio::layouts::CHANNEL_LAYOUT_STEREO);
    assert!(G7231Decoder::try_new(&p, &AudioDecoderOptions::default()).is_err());
}
