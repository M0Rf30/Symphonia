// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Dual mono (1+1) AC-3 with an LFE channel.
//!
//! `ac3_dualmono_lfe.ac3` is a synthetic syncframe (`data/gen_synthetic.py`) with channel 1 and
//! channel 2 carrying different `dynrng` and `dynrng2` words. The reference is FFmpeg's decoder
//! output. FFmpeg applies `dynrng2` to the LFE channel of a 1+1 programme (libavcodec/ac3dec.c
//! stores the two words in reverse order and maps the LFE channel to the second slot); ETSI TS
//! 102 366 clause 7.7.1.2 only assigns `dynrng` to channel 1 and `dynrng2` to channel 2 and
//! gives the LFE channel the programme's `dynrng`, as in every other audio coding mode, which is
//! what this decoder does. The reference LFE plane is rescaled accordingly.

mod common;

use common::{decode, max_delta, read_f32};
use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::audio::{AudioDecoderOptions, Downmix};

const STREAM: &str = "ac3_dualmono_lfe.ac3";
const DYNRNG1: u8 = 0x20;
const DYNRNG2: u8 = 0xa0;

/// The gain of a `dynrng` word (ETSI TS 102 366 clause 7.7.1.2, Table 7.29).
fn dynrng_gain(code: u8) -> f32 {
    let x = i32::from(code >> 5) - if code & 0x80 != 0 { 8 } else { 0 };
    let y = f32::from((code & 0x1f) | 0x20) / 64.0;
    2f32.powi(x + 1) * y
}

fn planes(interleaved: &[f32], n: usize) -> Vec<Vec<f32>> {
    (0..n).map(|c| interleaved.iter().skip(c).step_by(n).copied().collect()).collect()
}

#[test]
fn verify_dynrng_applies_per_channel_and_lfe_follows_dynrng() {
    let d = decode(STREAM, &AudioDecoderOptions::default());
    assert_eq!(
        d.channels,
        Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT | Position::LFE1)
    );

    let ours = planes(&d.samples, 3);
    let refr = planes(&read_f32("ac3_dualmono_lfe.f32"), 3);

    // Channel 1 and 2: bit-for-bit equal up to float rounding.
    for ch in 0..2 {
        assert!(max_delta(&ours[ch], &refr[ch]) < 1e-6, "channel {ch}");
    }

    // The LFE channel differs from FFmpeg by the ratio of the two gains.
    let ratio = dynrng_gain(DYNRNG1) / dynrng_gain(DYNRNG2);
    let lfe: Vec<f32> = refr[2].iter().map(|&x| x * ratio).collect();
    let peak = lfe.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
    assert!(peak > 1e-6, "the LFE channel carries signal, peak {peak}");
    assert!(max_delta(&ours[2], &lfe) < 1e-4 * peak, "{} (peak {peak})", max_delta(&ours[2], &lfe));
}

#[test]
fn verify_dual_mono_down_mix() {
    let discrete = planes(&decode(STREAM, &AudioDecoderOptions::default()).samples, 3);

    // Stereo: both channels are routed unchanged, the LFE channel is dropped.
    for mode in [Downmix::StereoLoRo, Downmix::StereoLtRt] {
        let d = decode(STREAM, &AudioDecoderOptions::default().downmix(mode));
        assert_eq!(d.samples.len(), discrete[0].len() * 2);
        let st = planes(&d.samples, 2);
        assert!(max_delta(&st[0], &discrete[0]) < 1e-7);
        assert!(max_delta(&st[1], &discrete[1]) < 1e-7);
    }

    // Mono: -6 dB from each channel (ETSI TS 102 366 clause 7.8.1).
    let d = decode(STREAM, &AudioDecoderOptions::default().downmix(Downmix::Mono));
    assert_eq!(d.channels, Channels::Positioned(Position::FRONT_CENTER));
    let expected: Vec<f32> =
        discrete[0].iter().zip(&discrete[1]).map(|(l, r)| 0.5 * (l + r)).collect();
    assert!(max_delta(&d.samples, &expected) < 1e-6);
}
