// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! E-AC-3 reduced sample rates and transient pre-noise processing.
//!
//! Both fixtures are synthetic streams written by `data/gen_synthetic.py` from the syntax of
//! ETSI TS 102 366 Annex E. Every bin is coded with 16-bit mantissas, so the decoded audio does not
//! depend on the bit allocation.
//!
//! FFmpeg (libavcodec/ac3dec.c) rejects reduced sample rates ("Reduced sampling rate is not
//! implemented") and parses but ignores the transient pre-noise data. The reference PCM is
//! therefore FFmpeg's output for a twin of each stream that it can decode:
//!
//! * `eac3_reduced_rate.f32`: the reduced rate stream with `fscod` rewritten to 48 kHz. The
//!   six blocks of 256 samples and the decoding are the same; only the rate differs.
//! * `eac3_transient.f32`: the audio of the transient stream before pre-noise processing.

mod common;

use common::{Decoded, data, max_delta, read_f32};
use symphonia_codec_ac3::Ac3Decoder;
use symphonia_core::codecs::audio::well_known::CODEC_ID_EAC3;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::Packet;
use symphonia_core::units::{Duration, Timestamp};

fn decode_bytes(stream: Vec<u8>) -> Decoded {
    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_EAC3);
    let mut dec = Ac3Decoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
    let packet = Packet::new(0, Timestamp::new(0), Duration::new(0), stream);
    let buf = dec.decode(&packet).unwrap();
    let mut samples = Vec::new();
    buf.copy_to_vec_interleaved(&mut samples);
    Decoded { rate: buf.spec().rate(), channels: buf.spec().channels().clone(), samples }
}

#[test]
fn verify_reduced_sample_rates() {
    const FRAME: usize = 1200;
    let stream = data("eac3_reduced_rate.eac3");
    let refr = read_f32("eac3_reduced_rate.f32");

    for (fscod2, rate) in [(0u8, 24_000), (1, 22_050), (2, 16_000)] {
        let mut s = stream.clone();
        for frame in s.chunks_exact_mut(FRAME) {
            // fscod = 3, fscod2 in the two bits that follow.
            frame[4] = 0xc0 | (fscod2 << 4) | (frame[4] & 0x0f);
        }
        let d = decode_bytes(s);
        assert_eq!(d.rate, rate);
        assert_eq!(d.channels.count(), 2, "one full bandwidth channel and the LFE channel");
        assert!(max_delta(&d.samples, &refr) < 1e-6, "{rate} Hz: {}", max_delta(&d.samples, &refr));
    }
}

/// Fixture layout: three frames of 1536 stereo samples; frame 1 carries a correction for channel
/// 0 with `transprocloc` 250 and `transproclen` 40.
const FRAME_LEN: usize = 1536;
const TRANSIENT_LOC: usize = 250 * 4;
const TRANSIENT_LEN: usize = 40;
const TC1: usize = 256;
const TC2: usize = 128;

fn plane(interleaved: &[f32], ch: usize) -> Vec<f32> {
    interleaved.iter().skip(ch).step_by(2).copied().collect()
}

#[test]
fn verify_transient_pre_noise_processing() {
    let d = decode_bytes(data("eac3_transient.eac3"));
    assert_eq!(d.rate, 48_000);
    let refr = read_f32("eac3_transient.f32");
    assert_eq!(d.samples.len(), refr.len());

    // Channel 1 has no data and must be bit for bit what the reference decoder produced.
    assert!(max_delta(&plane(&d.samples, 1), &plane(&refr, 1)) < 1e-6);

    let out = plane(&d.samples, 0);
    let orig = plane(&refr, 0);

    // ETSI TS 102 366 clause E.3.7.2: the transient sits TRANSIENT_LOC samples into frame 1 and the
    // pre-noise starts with the block before the one holding it.
    let transloc = FRAME_LEN + TRANSIENT_LOC;
    let pnlen = TC1 + TRANSIENT_LOC % TC1;
    let tot_corr_len = pnlen + TRANSIENT_LEN + TC1;
    let start = transloc - tot_corr_len;
    let synth_start = transloc - (2 * TC1 + 2 * pnlen);

    for (i, (&o, &x)) in out.iter().zip(&orig).enumerate() {
        if i < start || i >= transloc {
            // Audio outside of the corrected range is not modified.
            assert!((o - x).abs() < 1e-6, "sample {i} modified");
            continue;
        }

        let s = i - start;
        let synth = orig[synth_start + s];
        if (TC1..tot_corr_len - TC2).contains(&s) {
            // The time scaling synthesis buffer overwrites the pre-noise.
            assert!((o - synth).abs() < 1e-6, "sample {i}: {o} != {synth}");
        }
        else {
            // Cross-fade with constant amplitude windows: a convex combination.
            let (lo, hi) = if x < synth { (x, synth) } else { (synth, x) };
            assert!(o >= lo - 1e-6 && o <= hi + 1e-6, "sample {i}: {o} outside [{lo}, {hi}]");
        }
    }

    // The correction must actually change the audio.
    assert!(max_delta(&out, &orig) > 1e-3);
}
