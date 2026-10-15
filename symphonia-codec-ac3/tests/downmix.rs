// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Down-mix of a 5.1 AC-3 stream (ETSI TS 102 366 clause 7.8.2).
//!
//! `downmix_5_1.ac3` holds two syncframes encoded by FFmpeg with `cmixlev` = 2 (0.5) and
//! `surmixlev` = 0 (0.707), so the test fails if the BSI levels are ignored. The stereo reference
//! is FFmpeg's decoder output with `-downmix stereo`. FFmpeg scales the mono mix by 0.707 where
//! the specification requires 0.5 ("a further scaling of 1/2", clause 7.8.2), so that reference
//! is rescaled. FFmpeg has no Lt/Rt mix: that mix is checked against the equations of the
//! specification applied to the discrete channels.

mod common;

use common::{decode, max_delta, read_f32, snr_db};
use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::audio::{AudioDecoderOptions, Downmix};

const STREAM: &str = "downmix_5_1.ac3";
const FRAMES: usize = 3072;

fn opts(mode: Downmix) -> AudioDecoderOptions {
    AudioDecoderOptions::default().downmix(mode)
}

#[test]
fn verify_discrete_output_is_default() {
    let d = decode(STREAM, &AudioDecoderOptions::default());
    assert_eq!(d.samples.len(), FRAMES * 6);
    assert_eq!(d.channels.count(), 6);
}

#[test]
fn verify_lo_ro_matches_ffmpeg() {
    let d = decode(STREAM, &opts(Downmix::StereoLoRo));
    assert_eq!(d.rate, 48_000);
    assert_eq!(d.channels, Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT));

    let refr = read_f32("downmix_5_1_stereo.f32");
    assert_eq!(d.samples.len(), refr.len());

    // The surround level is 0.707 here and 1/sqrt(2) in FFmpeg.
    let snr = snr_db(&refr, &d.samples);
    assert!(snr > 70.0, "SNR {snr} dB, max delta {}", max_delta(&refr, &d.samples));
}

#[test]
fn verify_mono_matches_ffmpeg() {
    let d = decode(STREAM, &opts(Downmix::Mono));
    assert_eq!(d.channels, Channels::Positioned(Position::FRONT_CENTER));

    // FFmpeg: (Lo + Ro) * 0.707. Specification: (Lo + Ro) * 0.5.
    let refr: Vec<f32> = read_f32("downmix_5_1_mono.f32")
        .iter()
        .map(|&x| x * (0.5 / std::f32::consts::FRAC_1_SQRT_2))
        .collect();
    let snr = snr_db(&refr, &d.samples);
    assert!(snr > 70.0, "SNR {snr} dB, max delta {}", max_delta(&refr, &d.samples));
}

#[test]
fn verify_lt_rt_matches_specification_equations() {
    let discrete = decode(STREAM, &AudioDecoderOptions::default()).samples;
    let d = decode(STREAM, &opts(Downmix::StereoLtRt));
    assert_eq!(d.channels, Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT));

    // Discrete order: FL, FR, FC, LFE1, SL, SR. Without Lt/Rt metadata the levels are 0.707 and
    // the sum of the coefficients of an output is 1 + 0.707 * 3.
    let k = 0.707f32;
    let norm = 1.0 / (1.0 + 3.0 * k);
    let mut expected = Vec::with_capacity(FRAMES * 2);
    for f in discrete.chunks_exact(6) {
        let (l, r, c, ls, rs) = (f[0], f[1], f[2], f[4], f[5]);
        expected.push((l + k * c - k * ls - k * rs) * norm);
        expected.push((r + k * c + k * ls + k * rs) * norm);
    }

    assert!(max_delta(&expected, &d.samples) < 1e-6, "{}", max_delta(&expected, &d.samples));
}

#[test]
fn verify_lo_ro_uses_stream_levels() {
    let discrete = decode(STREAM, &AudioDecoderOptions::default()).samples;
    let d = decode(STREAM, &opts(Downmix::StereoLoRo));

    // clev = 0.5 and slev = 0.707 from the BSI.
    let (clev, slev) = (0.5f32, 0.707f32);
    let norm = 1.0 / (1.0 + clev + slev);
    let mut expected = Vec::with_capacity(FRAMES * 2);
    for f in discrete.chunks_exact(6) {
        let (l, r, c, ls, rs) = (f[0], f[1], f[2], f[4], f[5]);
        expected.push((l + clev * c + slev * ls) * norm);
        expected.push((r + clev * c + slev * rs) * norm);
    }

    assert!(max_delta(&expected, &d.samples) < 1e-6, "{}", max_delta(&expected, &d.samples));
}
