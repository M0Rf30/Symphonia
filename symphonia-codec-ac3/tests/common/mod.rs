// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Helpers shared by the integration tests.

#![allow(dead_code, clippy::unwrap_used)]

use symphonia_codec_ac3::Ac3Decoder;
use symphonia_core::audio::Channels;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_AC3, CODEC_ID_EAC3};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::Packet;
use symphonia_core::units::{Duration, Timestamp};

/// Reads a fixture of `tests/data`.
pub fn data(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/data/{}", env!("CARGO_MANIFEST_DIR"), name))
        .expect("fixture is readable")
}

/// Reads a fixture holding little-endian `f32` samples.
pub fn read_f32(name: &str) -> Vec<f32> {
    data(name).chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
}

/// A decoded stream: the sample rate, channels and interleaved samples.
pub struct Decoded {
    pub rate: u32,
    pub channels: Channels,
    pub samples: Vec<f32>,
}

/// Decodes a whole elementary stream in a single packet.
pub fn decode(name: &str, opts: &AudioDecoderOptions) -> Decoded {
    let codec = if name.ends_with(".eac3") { CODEC_ID_EAC3 } else { CODEC_ID_AC3 };
    let mut params = AudioCodecParameters::new();
    params.for_codec(codec);

    let mut dec = Ac3Decoder::try_new(&params, opts).unwrap();
    let packet = Packet::new(0, Timestamp::new(0), Duration::new(0), data(name));
    let buf = dec.decode(&packet).unwrap();

    let mut samples = Vec::new();
    buf.copy_to_vec_interleaved(&mut samples);
    Decoded { rate: buf.spec().rate(), channels: buf.spec().channels().clone(), samples }
}

/// The largest absolute difference between two sample vectors of equal length.
pub fn max_delta(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len(), "sample counts differ");
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max)
}

/// The signal to noise ratio in dB of `test` against the reference `refr`.
pub fn snr_db(refr: &[f32], test: &[f32]) -> f64 {
    assert_eq!(refr.len(), test.len(), "sample counts differ");
    let sig: f64 = refr.iter().map(|&x| f64::from(x) * f64::from(x)).sum();
    let err: f64 =
        refr.iter().zip(test).map(|(&r, &t)| (f64::from(r) - f64::from(t)).powi(2)).sum();
    if err == 0.0 { f64::INFINITY } else { 10.0 * (sig / err).log10() }
}
