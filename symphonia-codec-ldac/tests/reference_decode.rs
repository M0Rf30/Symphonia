// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Decodes LDAC frames produced by the AOSP `libldac` reference encoder and compares the PCM
//! against the output of the (bug-fixed) open-source `libldacdec` reference decoder. The
//! committed `*.ref.s16` files are that decoder's `s16` output, and the `*.ldac` files are
//! concatenated frames, one per packet.
//!
//! Fixtures: a sine sweep plus noise, at several sample rates, channel configurations and encode
//! quality modes (HQ, SQ and low-bit-rate modes that exercise every gradient mode and the
//! joint-coded 1-bit spectra). Each stream is 0.1 s.

use std::path::PathBuf;

use symphonia_codec_ldac::{FrameHeader, LdacDecoder};
use symphonia_core::codecs::audio::well_known::CODEC_ID_LDAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};
use symphonia_testutil::XorShift64;

const FIXTURES: [&str; 9] = [
    "s44_stereo_hq",
    "s44_stereo_sq",
    "s44_stereo_32b",
    "s44_stereo_26b",
    "s44_mono_hq",
    "s44_dual_hq",
    "s48_stereo_hq",
    "s88_stereo_hq",
    "s96_stereo_hq",
];

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// Splits a stream of concatenated LDAC frames into its individual frames.
fn split_frames(mut data: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();

    while let Some(header) = FrameHeader::parse(data) {
        let size = header.total_frame_size();

        if data.len() < size {
            break;
        }

        frames.push(data[..size].to_vec());
        data = &data[size..];
    }

    assert!(!frames.is_empty(), "no frames parsed");
    frames
}

fn decode(frames: &[Vec<u8>], channels: usize) -> Vec<f32> {
    let params = AudioCodecParameters { codec: CODEC_ID_LDAC, ..Default::default() };
    let mut dec = LdacDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

    let (mut out, mut tmp) = (Vec::new(), Vec::new());

    for data in frames {
        let pkt = PacketRef::new(0, Timestamp::new(0), Duration::new(0), data);
        dec.decode_ref(&pkt).unwrap().copy_to_vec_interleaved::<f32>(&mut tmp);
        out.extend_from_slice(&tmp);
    }

    assert_eq!(out.len() % channels, 0);
    out
}

fn to_s16(x: f32) -> i16 {
    (x * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

#[test]
fn ldac_matches_reference() {
    for name in FIXTURES {
        let frames = split_frames(&std::fs::read(fixture(&format!("{name}.ldac"))).unwrap());
        let header = FrameHeader::parse(&frames[0]).unwrap();
        let channels = header.channels();
        let samples_per_channel = header.samples_per_channel().unwrap();
        assert_eq!(frames.len() * samples_per_channel * channels, {
            let reference = std::fs::read(fixture(&format!("{name}.ref.s16"))).unwrap();
            reference.len() / 2
        });

        let out = decode(&frames, channels);

        let reference = std::fs::read(fixture(&format!("{name}.ref.s16"))).unwrap();
        let want: Vec<i16> =
            reference.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        assert_eq!(want.len(), out.len());

        let mut max_dev = 0i32;
        let (mut sig, mut err) = (0.0f64, 0.0f64);

        for (&got, &want) in out.iter().zip(&want) {
            let dev = (i32::from(to_s16(got)) - i32::from(want)).abs();
            max_dev = max_dev.max(dev);
            sig += f64::from(want) * f64::from(want);
            err += f64::from(got - f32::from(want) / 32768.0)
                * f64::from(got - f32::from(want) / 32768.0);
        }

        let snr = 10.0 * (sig / err.max(1.0e-30)).log10();
        println!("{name}: max deviation {max_dev} LSB, SNR {snr:.1} dB");
        // The decoder uses single-precision transforms where the reference uses double: a few LSB
        // of deviation is expected.
        assert!(max_dev <= 4, "{name}: max deviation {max_dev} LSB");
        assert!(snr > 80.0, "{name}: SNR {snr:.1} dB");
    }
}

#[test]
fn corrupted_packets_do_not_panic() {
    let mut rng = XorShift64(0x1234_5678_9abc_def0);

    for name in FIXTURES {
        let frames = split_frames(&std::fs::read(fixture(&format!("{name}.ldac"))).unwrap());
        let params = AudioCodecParameters { codec: CODEC_ID_LDAC, ..Default::default() };
        let mut dec = LdacDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

        for _ in 0..8 {
            let data = &frames[0];
            let mut corrupt = data.clone();
            let byte = rng.next_u64() as usize % corrupt.len();
            corrupt[byte] ^= (rng.next_u64() & 0xff) as u8;
            let pkt = PacketRef::new(0, Timestamp::new(0), Duration::new(0), &corrupt);
            let _ = dec.decode_ref(&pkt);
        }

        // A truncated and an empty packet.
        let pkt = PacketRef::new(0, Timestamp::new(0), Duration::new(0), &frames[0][..1]);
        let _ = dec.decode_ref(&pkt);
        let pkt = PacketRef::new(0, Timestamp::new(0), Duration::new(0), &[]);
        let _ = dec.decode_ref(&pkt);
    }
}
