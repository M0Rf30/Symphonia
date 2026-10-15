// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Signal to noise ratio of the decoder output against the PCM decoded by FFmpeg (`-f f32le`).
//!
//! * `xch_es.dts`: the first 0.1 s of a DTS-ES stream (core with the XCH channel extension).
//! * `xll_71.dts`: the first 0.1 s of a DTS-HD Master Audio 7.1 stream (core and XLL in an
//!   extension substream).

use symphonia_codec_dca::DcaDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::well_known::CODEC_ID_DCA;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

const CORE_SYNC: [u8; 4] = [0x7f, 0xfe, 0x80, 0x01];

/// Splits a raw stream into packets that start at a core sync word.
fn packets(data: &[u8]) -> Vec<&[u8]> {
    let starts: Vec<usize> =
        (0..data.len().saturating_sub(3)).filter(|&i| data[i..i + 4] == CORE_SYNC).collect();
    starts
        .iter()
        .enumerate()
        .map(|(n, &s)| &data[s..starts.get(n + 1).copied().unwrap_or(data.len())])
        .collect()
}

fn check(stream: &[u8], reference: &[u8], channels: usize) {
    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_DCA);
    let mut dec = DcaDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

    let mut out: Vec<f32> = Vec::new();
    for data in packets(stream) {
        let packet = PacketRef::new(0, Timestamp::new(0), Duration::new(0), data);
        match dec.decode_ref(&packet).unwrap() {
            GenericAudioBufferRef::F32(buf) => {
                assert_eq!(buf.spec().channels().count(), channels);
                for i in 0..buf.frames() {
                    for c in 0..channels {
                        out.push(buf.plane(c).unwrap()[i]);
                    }
                }
            }
            _ => panic!("expected f32 samples"),
        }
    }

    let refr: Vec<f32> =
        reference.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    assert_eq!(out.len(), refr.len(), "sample count");

    let sig: f64 = refr.iter().map(|&x| f64::from(x).powi(2)).sum();
    let err: f64 = refr.iter().zip(&out).map(|(&r, &t)| f64::from(r - t).powi(2)).sum();
    let snr = 10.0 * (sig / err.max(1e-30)).log10();
    assert!(snr > 90.0, "snr {snr} dB");
}

#[test]
fn verify_xch_matches_reference() {
    check(include_bytes!("data/xch_es.dts"), include_bytes!("data/xch_es.f32"), 7);
}

#[test]
fn verify_xll_matches_reference() {
    check(include_bytes!("data/xll_71.dts"), include_bytes!("data/xll_71.f32"), 8);
}

const EXSS_SYNC: [u8; 4] = [0x64, 0x58, 0x20, 0x25];

/// `lbr_stereo.dts`: the first 8 packets of the DTS Express (LBR) secondary audio stream of a
/// Blu-ray clip (stereo, 48 kHz, extension substream only).
#[test]
fn verify_lbr_matches_reference() {
    let stream = include_bytes!("data/lbr_stereo.dts");
    let reference = include_bytes!("data/lbr_stereo.f32");

    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_DCA);
    let mut dec = DcaDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

    let mut out: Vec<f32> = Vec::new();
    for data in stream.chunks(2048) {
        assert_eq!(data[..4], EXSS_SYNC);
        let packet = PacketRef::new(0, Timestamp::new(0), Duration::new(0), data);
        match dec.decode_ref(&packet).unwrap() {
            GenericAudioBufferRef::F32(buf) => {
                assert_eq!(buf.spec().channels().count(), 2);
                for i in 0..buf.frames() {
                    for c in 0..2 {
                        out.push(buf.plane(c).unwrap()[i]);
                    }
                }
            }
            _ => panic!("expected f32 samples"),
        }
    }

    let refr: Vec<f32> =
        reference.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    assert_eq!(out.len(), refr.len(), "sample count");

    let sig: f64 = refr.iter().map(|&x| f64::from(x).powi(2)).sum();
    let err: f64 = refr.iter().zip(&out).map(|(&r, &t)| f64::from(r - t).powi(2)).sum();
    let snr = 10.0 * (sig / err.max(1e-30)).log10();
    assert!(snr > 90.0, "snr {snr} dB");
}
