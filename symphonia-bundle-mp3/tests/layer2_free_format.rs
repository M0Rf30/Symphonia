// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! MPEG-1 Layer II streams with the "free" bit-rate (bit-rate index 0).
//!
//! In a free-format Layer II frame the bit allocation table is not chosen by bit-rate: ISO/IEC
//! 11172-3 Tables 3-B.2a and 3-B.2b list "free format" next to the highest bit-rates, so table
//! 3-B.2a is used at 48 kHz and table 3-B.2b at 44.1 and 32 kHz, even if the frame length implies
//! a low bit-rate for which a fixed-rate frame would use Table 3-B.2a, 3-B.2c, or 3-B.2d.
//!
//! The fixtures (`layer2_free_*.mp2`) were encoded by TwoLAME 0.4.0 with `--freeformat` from pure
//! tones (440 Hz in the left channel, 523.25 Hz in the right channel when stereo), 0.2 s to 0.4 s
//! long. Neither ffmpeg nor mpg123 decodes all of them, so the decoded audio is checked against
//! the tones that were encoded.

use std::f64::consts::PI;
use std::io::Cursor;

use symphonia_bundle_mp3::{MpaDecoder, MpaReader};
use symphonia_core::codecs::audio::well_known::CODEC_ID_MP2;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;

/// The number of samples per channel in a Layer II frame.
const FRAME_LEN: usize = 1152;

/// Decode a stream. Returns the sample rate and the samples of each channel.
fn decode(stream: &[u8]) -> (u32, Vec<Vec<f32>>) {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(stream.to_vec())), Default::default());
    let mut reader = MpaReader::try_new(mss, FormatOptions::default()).unwrap();

    let params = reader.tracks()[0].codec_params.as_ref().unwrap().audio().unwrap();
    assert_eq!(params.codec, CODEC_ID_MP2);

    let mut decoder =
        MpaDecoder::try_new(params, &AudioDecoderOptions::default().gapless(false)).unwrap();

    let mut rate = 0;
    let mut channels: Vec<Vec<f32>> = Vec::new();

    while let Some(packet) = reader.next_packet().unwrap() {
        let decoded = decoder.decode(&packet).unwrap();

        rate = decoded.spec().rate();
        channels.resize(decoded.spec().channels().count(), Vec::new());
        assert_eq!(decoded.frames(), FRAME_LEN);

        let mut interleaved: Vec<f32> = Vec::new();
        decoded.copy_to_vec_interleaved(&mut interleaved);

        for frame in interleaved.chunks_exact(channels.len()) {
            for (channel, &sample) in channels.iter_mut().zip(frame) {
                channel.push(sample);
            }
        }
    }

    (rate, channels)
}

/// The signal-to-noise ratio, in dB, of `samples` with respect to the best fitting (least squares)
/// sinusoid of frequency `freq` Hz, with any phase and amplitude.
fn snr_to_sinusoid(samples: &[f32], rate: u32, freq: f64) -> f64 {
    let w = 2.0 * PI * freq / f64::from(rate);

    // Solve the 2x2 normal equations of x[n] = a sin(w n) + b cos(w n).
    let (mut ss, mut sc, mut cc, mut xs, mut xc, mut xx) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);

    for (n, &x) in samples.iter().enumerate() {
        let (s, c) = (w * n as f64).sin_cos();
        let x = f64::from(x);
        ss += s * s;
        sc += s * c;
        cc += c * c;
        xs += x * s;
        xc += x * c;
        xx += x * x;
    }

    let det = ss * cc - sc * sc;
    let a = (xs * cc - xc * sc) / det;
    let b = (xc * ss - xs * sc) / det;

    // The energy left after removing the fitted sinusoid.
    let residual = xx - (a * xs + b * xc);

    10.0 * (xx / residual.max(1e-30)).log10()
}

/// Decode a fixture and check that it is the encoded tones.
fn verify_tones(stream: &[u8], rate: u32, n_channels: usize) {
    let (decoded_rate, channels) = decode(stream);

    assert_eq!(decoded_rate, rate);
    assert_eq!(channels.len(), n_channels);

    for (ch, samples) in channels.iter().enumerate() {
        // The decoder starts from silence: skip the first frames.
        let freq = if ch == 0 { 440.0 } else { 523.25 };
        let tail = &samples[3 * FRAME_LEN..samples.len() - FRAME_LEN];
        let snr = snr_to_sinusoid(tail, rate, freq);

        // The tones are coded with 60 to 90 dB of SNR at these bit-rates. A stream parsed
        // with the wrong allocation table is noise (or fails to decode at all).
        assert!(snr > 50.0, "channel {ch}: SNR to the {freq} Hz tone is only {snr:.1} dB");
    }
}

/// 44.1 kHz stereo, 128 kbit/s (64 kbit/s per channel, for which a fixed-rate frame uses Table
/// 3-B.2a) is a free-format frame with Table 3-B.2b.
#[test]
fn verify_44100hz_stereo_uses_table_2b() {
    verify_tones(include_bytes!("fixtures/layer2_free_44100_st_128.mp2"), 44100, 2);
}

/// 32 kHz stereo, 64 kbit/s (32 kbit/s per channel, for which a fixed-rate frame uses Table
/// 3-B.2d) is a free-format frame with Table 3-B.2b.
#[test]
fn verify_32000hz_stereo_uses_table_2b() {
    verify_tones(include_bytes!("fixtures/layer2_free_32000_st_64.mp2"), 32000, 2);
}

/// 44.1 kHz mono, 64 kbit/s (for which a fixed-rate frame uses Table 3-B.2a).
#[test]
fn verify_44100hz_mono_uses_table_2b() {
    let (rate, channels) = decode(include_bytes!("fixtures/layer2_free_44100_mono_64.mp2"));

    assert_eq!(rate, 44100);
    assert_eq!(channels.len(), 1);

    let tail = &channels[0][3 * FRAME_LEN..channels[0].len() - FRAME_LEN];
    let snr = snr_to_sinusoid(tail, rate, 440.0);
    assert!(snr > 50.0, "SNR to the 440 Hz tone is only {snr:.1} dB");
}

/// 44.1 kHz joint stereo (intensity stereo from sub-band 4), 96 kbit/s, CRC protected. The CRC is
/// verified with the same table as the audio data.
#[test]
fn verify_44100hz_joint_stereo_with_crc() {
    verify_tones(include_bytes!("fixtures/layer2_free_44100_js_96.mp2"), 44100, 2);
}

/// High bit-rates, 384 kbit/s (192 kbit/s per channel): Table 3-B.2a at 48 kHz and Table 3-B.2b at
/// 44.1 kHz.
#[test]
fn verify_high_bitrates() {
    verify_tones(include_bytes!("fixtures/layer2_free_48000_st_384.mp2"), 48000, 2);
    verify_tones(include_bytes!("fixtures/layer2_free_44100_st_384.mp2"), 44100, 2);
}
