// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Compares the decoder with FFmpeg's native `amrnb` decoder (skipped without `ffmpeg`).

mod common;

use common::*;

fn check(name: &str) {
    let Some(reference) = ffmpeg_decode(name)
    else {
        eprintln!("ffmpeg not available, skipping");
        return;
    };
    let out = decode_file(name);
    assert_eq!(out.len(), reference.len(), "{name}: sample count");
    let (snr, max) = compare(&out, &reference);
    eprintln!("{name}: {} samples, SNR {snr:.1} dB, max diff {max:.2} LSB", out.len());
    assert!(snr > 60.0, "{name}: SNR {snr}");
}

#[test]
fn modes() {
    for m in 0..8 {
        check(&format!("enc_mode{m}.amr"));
    }
}

#[test]
fn full() {
    check("_full_sample.amr");
    check("sample2.amr");
}
