// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Compares the decoders with FFmpeg (`real_144`, `real_288`).
//!
//! The `.pk` fixtures are the concatenated, deinterleaved coded frames of RealAudio files from
//! <https://samples.ffmpeg.org/real/> (20-byte frames for RealAudio 1.0, 38-byte frames for
//! RealAudio 2.0), truncated to a few seconds. The `.ref.s16` fixtures are the first samples of
//! `ffmpeg -i x.ra -f s16le -ar 8000 -ac 1`, i.e. FFmpeg's decoder output.
//!
//! The 14.4K decoder is integer-exact and must match bit for bit. The 28.8K decoder is
//! floating-point; its output is compared after conversion to 16 bits.

use symphonia_codec_ra::{Ra144Decoder, Ra288Decoder};
use symphonia_core::audio::GenericAudioBufferRef;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_RA10, CODEC_ID_RA20};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;

fn s16(bytes: &[u8]) -> Vec<i16> {
    bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect()
}

fn decode<D: AudioDecoder>(dec: &mut D, stream: &[u8], packet_len: usize) -> Vec<f64> {
    let mut out = Vec::new();
    for chunk in stream.chunks(packet_len) {
        let pkt = PacketRef::new(0, Default::default(), Default::default(), chunk);
        match dec.decode_ref(&pkt).unwrap() {
            GenericAudioBufferRef::S16(b) => {
                use symphonia_core::audio::Audio;
                assert_eq!(b.spec().channels().count(), 1);
                out.extend(b.plane(0).unwrap().iter().map(|&s| f64::from(s)));
            }
            GenericAudioBufferRef::F32(b) => {
                use symphonia_core::audio::Audio;
                assert_eq!(b.spec().rate(), 8000);
                out.extend(b.plane(0).unwrap().iter().map(|&s| f64::from(s)));
            }
            _ => panic!("unexpected sample format"),
        }
    }
    out
}

fn params(codec: symphonia_core::codecs::audio::AudioCodecId) -> AudioCodecParameters {
    let mut p = AudioCodecParameters::new();
    p.for_codec(codec).with_sample_rate(8000);
    p
}

fn check_144(stream: &[u8], reference: &[u8], packet_len: usize) {
    let mut dec =
        Ra144Decoder::try_new(&params(CODEC_ID_RA10), &AudioDecoderOptions::default()).unwrap();
    let out = decode(&mut dec, stream, packet_len);
    let reference = s16(reference);
    assert_eq!(out.len(), reference.len());
    for (i, (&a, &b)) in out.iter().zip(&reference).enumerate() {
        assert_eq!(a, f64::from(b), "sample {i} differs");
    }
}

/// Returns (max abs difference in 16-bit LSBs, SNR in dB) of the 28.8K output against FFmpeg.
fn compare_288(stream: &[u8], reference: &[u8], packet_len: usize) -> (f64, f64) {
    let mut dec =
        Ra288Decoder::try_new(&params(CODEC_ID_RA20), &AudioDecoderOptions::default()).unwrap();
    let out = decode(&mut dec, stream, packet_len);
    let reference = s16(reference);
    assert_eq!(out.len(), reference.len());

    let mut max = 0.0f64;
    let mut sig = 0.0f64;
    let mut noise = 0.0f64;
    for (&a, &b) in out.iter().zip(&reference) {
        // Same conversion as FFmpeg's float to s16: scale, round, clip.
        let a = (a * 32768.0).round().clamp(-32768.0, 32767.0);
        let d = a - f64::from(b);
        max = max.max(d.abs());
        sig += f64::from(b) * f64::from(b);
        noise += d * d;
    }
    let snr = if noise == 0.0 { f64::INFINITY } else { 10.0 * (sig / noise).log10() };
    (max, snr)
}

#[test]
fn ra144_speech_matches_ffmpeg_exactly() {
    let pk = include_bytes!("data/thankyou144.pk");
    let r = include_bytes!("data/thankyou144.ref.s16");
    // One frame per packet, and all 245 frames in a single packet.
    check_144(pk, r, 20);
    check_144(pk, r, 1000);
    check_144(pk, r, pk.len());
}

#[test]
fn ra144_music_matches_ffmpeg_exactly() {
    check_144(
        include_bytes!("data/drummers144.pk"),
        include_bytes!("data/drummers144.ref.s16"),
        20 * 7,
    );
}

#[test]
fn ra288_speech_matches_ffmpeg() {
    let (max, snr) = compare_288(
        include_bytes!("data/welcome288.pk"),
        include_bytes!("data/welcome288.ref.s16"),
        38,
    );
    eprintln!("welcome288: max {max} LSB, SNR {snr:.1} dB");
    assert!(max <= 1.0, "max difference {max} LSB");
    assert!(snr > 70.0, "SNR {snr}");
}

#[test]
fn ra288_music_matches_ffmpeg() {
    let pk = include_bytes!("data/lucky288.pk");
    let r = include_bytes!("data/lucky288.ref.s16");
    for packet_len in [38, 38 * 6, pk.len()] {
        let (max, snr) = compare_288(pk, r, packet_len);
        eprintln!("lucky288 ({packet_len}): max {max} LSB, SNR {snr:.1} dB");
        assert!(max <= 1.0, "max difference {max} LSB");
        assert!(snr > 70.0, "SNR {snr}");
    }
}
