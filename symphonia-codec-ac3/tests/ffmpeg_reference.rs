// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Signal to noise ratio of the decoder output against the PCM decoded by FFmpeg (`-f f32le`).
//!
//! * `enc_ac3_51` and `enc_eac3_51`: 5.1 streams made by FFmpeg's `ac3` and `eac3` encoders
//!   (0.2 s, 48 kHz).
//! * `real_eac3_dep_71`: the first 0.1 s of a 7.1 E-AC-3 stream whose last channels are carried by
//!   a dependent substream.

mod common;

use common::{decode, read_f32, snr_db};
use symphonia_core::codecs::audio::AudioDecoderOptions;

fn check(stream: &str, reference: &str, channels: usize) {
    let out = decode(stream, &AudioDecoderOptions::default());
    assert_eq!(out.channels.count(), channels, "{stream}: channels");
    let refr = read_f32(reference);
    assert_eq!(out.samples.len(), refr.len(), "{stream}: sample count");
    let snr = snr_db(&refr, &out.samples);
    assert!(snr > 90.0, "{stream}: snr {snr} dB");
}

#[test]
fn verify_ac3_encoder_stream() {
    check("enc_ac3_51.ac3", "enc_ac3_51.f32", 6);
}

#[test]
fn verify_eac3_encoder_stream() {
    check("enc_eac3_51.eac3", "enc_eac3_51.f32", 6);
}

#[test]
fn verify_eac3_dependent_substream() {
    check("real_eac3_dep_71.eac3", "real_eac3_dep_71.f32", 8);
}
