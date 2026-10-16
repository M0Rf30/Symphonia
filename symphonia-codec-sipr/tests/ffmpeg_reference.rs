// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Compares the decoder with FFmpeg's `sipr` decoder.
//!
//! The `.pk` fixtures are 50 consecutive, deinterleaved packets of each mode. `16k`, `8k5` and
//! `6k5` are taken from RealMedia files (`ffmpeg -c copy -f data`); `5k0` has no encoder, and is
//! random data, which is a valid stream for this codec. The `.f32` fixtures are the output of
//! FFmpeg's decoder (`-f f32le`) for them. The decoders are floating point and compiled with
//! other rounding, so the output is compared with a tolerance.

use symphonia_codec_sipr::SiprDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::well_known::CODEC_ID_SIPR;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;

fn decoder(rate: u32) -> SiprDecoder {
    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_SIPR).with_sample_rate(rate);
    SiprDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap()
}

fn decode(dec: &mut SiprDecoder, packet: &[u8]) -> Option<Vec<f32>> {
    let pkt = PacketRef::new(0, Default::default(), Default::default(), packet);
    match dec.decode_ref(&pkt).ok()? {
        GenericAudioBufferRef::F32(b) => Some(b.plane(0).unwrap().to_vec()),
        _ => panic!("expected f32 output"),
    }
}

fn check(name: &str, rate: u32, len: usize, frames_per_packet: usize) {
    let pk = std::fs::read(format!("tests/data/{name}.pk")).unwrap();
    let rf = std::fs::read(format!("tests/data/{name}.f32")).unwrap();
    let reference: Vec<f32> =
        rf.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();

    let mut dec = decoder(rate);
    let mut out = Vec::new();

    for packet in pk.chunks_exact(len) {
        out.extend(decode(&mut dec, packet).unwrap());
    }

    assert_eq!(out.len(), reference.len());
    assert_eq!(out.len() % frames_per_packet, 0);

    let mut max_diff = 0.0f32;
    let (mut err, mut sig) = (0.0f64, 0.0f64);

    for (a, b) in out.iter().zip(&reference) {
        max_diff = max_diff.max((a - b).abs());
        err += f64::from(a - b).powi(2);
        sig += f64::from(*b).powi(2);
    }

    eprintln!("{name}: max diff {max_diff:e}, snr {:.1} dB", 10.0 * (sig / err.max(1e-30)).log10());
    assert!(max_diff < 1e-3, "{name}: max diff {max_diff}");
    assert!(10.0 * (sig / err.max(1e-30)).log10() > 60.0, "{name}: snr too low");
}

#[test]
fn mode_16k_matches_ffmpeg() {
    check("16k", 16000, 20, 1);
}

#[test]
fn mode_8k5_matches_ffmpeg() {
    check("8k5", 8000, 19, 1);
}

#[test]
fn mode_6k5_matches_ffmpeg() {
    check("6k5", 8000, 29, 2);
}

#[test]
fn mode_5k0_matches_ffmpeg() {
    check("5k0", 8000, 37, 2);
}

#[test]
fn invalid_packets_are_rejected() {
    let mut dec = decoder(8000);
    assert!(decode(&mut dec, &[0; 20]).is_none());
    assert!(decode(&mut dec, &[0; 18]).is_none());
    assert!(decode(&mut dec, &[]).is_none());
    assert!(decode(&mut dec, &[0; 19]).is_some());

    let mut dec = decoder(16000);
    assert!(decode(&mut dec, &[0; 19]).is_none());
    assert!(decode(&mut dec, &[0; 20]).is_some());
}

#[test]
fn random_packets_decode_to_finite_samples() {
    // Any bit pattern is a valid frame; the output must stay finite and bounded.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 24) as u8
    };

    for (rate, len) in [(16000, 20), (8000, 19), (8000, 29), (8000, 37)] {
        let mut dec = decoder(rate);

        for _ in 0..2000 {
            let packet: Vec<u8> = (0..len).map(|_| next()).collect();
            let out = decode(&mut dec, &packet).unwrap();
            assert!(out.iter().all(|s| s.is_finite()), "len {len}");
        }
    }
}

#[test]
fn reset_restarts_the_decoder() {
    let pk = std::fs::read("tests/data/8k5.pk").unwrap();
    let mut dec = decoder(8000);

    let first = decode(&mut dec, &pk[..19]).unwrap();
    decode(&mut dec, &pk[19..38]).unwrap();
    dec.reset();

    assert_eq!(decode(&mut dec, &pk[..19]).unwrap(), first);
}
