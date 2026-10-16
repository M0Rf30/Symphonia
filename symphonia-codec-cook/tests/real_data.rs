// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Decodes real Cook streams from RealMedia files (the `real/AC-cook` samples of the FFmpeg
//! sample repository) and compares the output with the FFmpeg `cook` decoder
//! (`ffmpeg -i x.rm -f f32le`).
//!
//! Each `tests/data/*.cook` file holds a little-endian header (`u32` sample rate, `u16` channels,
//! `u16` frame size, `u16` extra data length), the extra data of the RealAudio header, and the
//! first frames (as `-c copy -f data` writes them). The matching `*.f32` file holds the FFmpeg
//! output for those frames.

use symphonia_codec_cook::CookDecoder;
use symphonia_core::audio::{Channels, Position, layouts};
use symphonia_core::codecs::audio::well_known::CODEC_ID_COOK;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

struct Fixture {
    rate: u32,
    channels: usize,
    extra: Vec<u8>,
    frame_size: usize,
    packets: Vec<u8>,
    reference: Vec<f32>,
}

fn load(name: &str) -> Fixture {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/");
    let data = std::fs::read(format!("{dir}{name}.cook")).unwrap();
    let reference = std::fs::read(format!("{dir}{name}.f32")).unwrap();
    let rate = u32::from_le_bytes(data[0..4].try_into().unwrap());
    let channels = usize::from(u16::from_le_bytes(data[4..6].try_into().unwrap()));
    let frame_size = usize::from(u16::from_le_bytes(data[6..8].try_into().unwrap()));
    let extra_len = usize::from(u16::from_le_bytes(data[8..10].try_into().unwrap()));
    Fixture {
        rate,
        channels,
        extra: data[10..10 + extra_len].to_vec(),
        frame_size,
        packets: data[10 + extra_len..].to_vec(),
        reference: reference
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect(),
    }
}

fn decoder(f: &Fixture) -> CookDecoder {
    let layout = match f.channels {
        1 => layouts::CHANNEL_LAYOUT_MONO,
        2 => layouts::CHANNEL_LAYOUT_STEREO,
        n => Channels::Positioned(Position::from_wave_channel_count(n as u32).unwrap()),
    };
    let mut p = AudioCodecParameters::new();
    p.for_codec(CODEC_ID_COOK)
        .with_sample_rate(f.rate)
        .with_channels(layout)
        .with_extra_data(f.extra.clone().into_boxed_slice());
    CookDecoder::try_new(&p, &AudioDecoderOptions::default()).unwrap()
}

fn decode(f: &Fixture) -> Vec<f32> {
    let mut dec = decoder(f);
    let mut out = Vec::new();
    let mut tmp = Vec::new();
    for chunk in f.packets.chunks_exact(f.frame_size) {
        let pkt = PacketRef::new(0, Timestamp::new(0), Duration::new(0), chunk);
        dec.decode_ref(&pkt).unwrap().copy_to_vec_interleaved::<f32>(&mut tmp);
        out.extend_from_slice(&tmp);
    }
    out
}

/// Returns (max absolute difference, SNR in dB).
fn compare(out: &[f32], reference: &[f32]) -> (f32, f64) {
    assert_eq!(out.len(), reference.len());
    let mut max = 0.0f32;
    let (mut sig, mut err) = (0.0f64, 0.0f64);
    for (&a, &b) in out.iter().zip(reference) {
        max = max.max((a - b).abs());
        sig += f64::from(b) * f64::from(b);
        err += f64::from(a - b) * f64::from(a - b);
    }
    (max, 10.0 * (sig / err.max(1e-30)).log10())
}

fn check(name: &str) {
    let f = load(name);
    let out = decode(&f);
    let (max, snr) = compare(&out, &f.reference);
    println!("{name}: {} samples, max abs diff {max:e}, SNR {snr:.1} dB", out.len());
    // The reference is single-precision with a different IMDCT; require agreement well below one
    // 16-bit LSB (3.05e-5).
    assert!(max < 1.0e-5, "{name}: max deviation {max}");
}

#[test]
fn mono_matches_ffmpeg() {
    check("mono_44k");
}

#[test]
fn mono_stereo_version_matches_ffmpeg() {
    check("mono_8k");
}

#[test]
fn stereo_matches_ffmpeg() {
    check("stereo_nonjoint");
}

#[test]
fn joint_stereo_matches_ffmpeg() {
    check("joint_32k");
    check("joint_96k");
}

#[test]
fn joint_stereo_512_matches_ffmpeg() {
    check("joint_22k");
}

#[test]
fn multichannel_matches_ffmpeg() {
    check("mc_5_1");
}
