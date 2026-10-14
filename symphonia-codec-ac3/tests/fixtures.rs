// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Decodes small raw AC-3 / E-AC-3 streams and checks the output against checksums of the output of
//! FFmpeg (with the known FFmpeg deviations from ETSI TS 102 366 corrected). The fixtures pin:
//!
//!  * `ac3_mono`: dither generator seed table and the AC-3 base decode path.
//!  * `eac3_spx_stereo`: order of the SPX channel flags and the SPX strategy fields.
//!  * `eac3_spx_mono_struct`: SPX band structure reset on every audio frame.
//!  * `eac3_aht_mono`: adaptive hybrid transform and the 32-entry VQ table for hebap 4.

use symphonia_codec_ac3::Ac3Decoder;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_AC3, CODEC_ID_EAC3};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::Packet;
use symphonia_core::units::{Duration, Timestamp};

// (file, channels, frames, sum of |x|, sum of x[i] * (i % 97 + 1)).
const FIXTURES: [(&str, usize, usize, f64, f64); 4] = [
    ("ac3_mono.ac3", 1, 3072, 841.9451, -7010.863),
    ("eac3_spx_mono_struct.eac3", 1, 3840, 2651.6120, 2150.492),
    ("eac3_spx_stereo.eac3", 2, 768, 291.9520, 3073.367),
    ("eac3_aht_mono.eac3", 1, 9216, 10973.8666, 14463.723),
];

fn data(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/data/{}", env!("CARGO_MANIFEST_DIR"), name))
        .expect("fixture is readable")
}

#[test]
fn verify_fixtures() {
    for (name, channels, frames, sum_abs, sum_w) in FIXTURES {
        let codec = if name.ends_with(".eac3") { CODEC_ID_EAC3 } else { CODEC_ID_AC3 };
        let mut params = AudioCodecParameters::new();
        params.for_codec(codec);

        let mut dec = Ac3Decoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

        let packet = Packet::new(0, Timestamp::new(0), Duration::new(0), data(name));
        let buf = dec.decode(&packet).unwrap();

        let mut samples = Vec::<f32>::new();
        buf.copy_to_vec_interleaved(&mut samples);

        assert_eq!(samples.len(), channels * frames, "{name}: sample count");

        let abs: f64 = samples.iter().map(|&x| f64::from(x).abs()).sum();
        let w: f64 =
            samples.iter().enumerate().map(|(i, &x)| f64::from(x) * ((i % 97) + 1) as f64).sum();

        assert!((abs - sum_abs).abs() < 1e-3 * sum_abs.max(1.0), "{name}: sum |x| = {abs}");
        assert!((w - sum_w).abs() < 1e-3 * sum_abs.max(1.0), "{name}: weighted sum = {w}");
    }
}

#[test]
fn verify_corrupt_input_does_not_panic() {
    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_EAC3);

    let mut dec = Ac3Decoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

    let mut stream = data("eac3_spx_stereo.eac3");
    // Deterministic corruption.
    let mut state = 0x1234_5678u32;
    for byte in stream.iter_mut() {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        if state >> 28 == 0 {
            *byte ^= (state >> 8) as u8;
        }
    }

    for end in [0, 1, 5, 100, stream.len() / 2, stream.len()] {
        let packet = Packet::new(0, Timestamp::new(0), Duration::new(0), stream[..end].to_vec());
        let _ = dec.decode(&packet);
    }
    let _ = CodecParameters::Audio(params);
}

fn decode_one(
    codec: symphonia_core::codecs::audio::AudioCodecId,
    stream: Vec<u8>,
) -> (u32, Vec<f32>) {
    let mut params = AudioCodecParameters::new();
    params.for_codec(codec);

    let mut dec = Ac3Decoder::try_new(&params, &AudioDecoderOptions::default()).expect("decoder");
    let packet = Packet::new(0, Timestamp::new(0), Duration::new(0), stream);
    let buf = dec.decode(&packet).expect("decodes");

    let mut samples = Vec::<f32>::new();
    buf.copy_to_vec_interleaved(&mut samples);
    (buf.spec().rate(), samples)
}

#[test]
fn verify_reduced_sample_rates() {
    // Rewrite `fscod`/`numblkscod` of the first syncframe as `fscod` 3 and `fscod2`.
    let stream = data("eac3_aht_mono.eac3");
    let frame_size = ((usize::from(stream[2] & 7) << 8 | usize::from(stream[3])) + 1) * 2;

    for (fscod2, rate) in [(0u8, 24000), (1, 22050), (2, 16000)] {
        let mut frame = stream[..frame_size].to_vec();
        frame[4] = 0xc0 | (fscod2 << 4) | (frame[4] & 0x0f);
        let (r, samples) = decode_one(CODEC_ID_EAC3, frame);
        assert_eq!(r, rate);
        assert_eq!(samples.len(), 1536);
    }
}

#[test]
fn verify_bsid_9_and_10_are_muted() {
    for bsid in [9u8, 10] {
        let mut stream = data("ac3_mono.ac3");
        // The fixture holds two syncframes, the second one truncated.
        for at in [5, 2304 + 5] {
            stream[at] = (bsid << 3) | (stream[at] & 7);
        }
        let (_, samples) = decode_one(CODEC_ID_AC3, stream);
        assert_eq!(samples.len(), 3072);
        assert!(samples.iter().all(|&s| s == 0.0));
    }
}
