// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Decodes the SMPTE ST 337 Dolby E stream `dolby_e/16-11.s337m` of the sample corpus (FFmpeg's
//! FATE sample `dolby_e/16-11`: 5.1, 25 fps, 16-bit words) and compares the output with FFmpeg's
//! decoder, when `ffmpeg` is installed.

use std::process::Command;

use symphonia_codec_dolby_e::DolbyEDecoder;
use symphonia_common::mpeg::es::EsParser;
use symphonia_common::mpeg::s337m::S337mEs;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};
use symphonia_testutil::sample_path;

const SAMPLE: &str = "dolby_e/16-11.s337m";

/// Decode the sample: the channels (planar, concatenated per channel) and the channel count.
fn decode(path: &std::path::Path) -> (Vec<Vec<f32>>, u32) {
    let data = std::fs::read(path).unwrap();

    let mut es = S337mEs::new();
    es.push(&data);

    let mut dec: Option<DolbyEDecoder> = None;
    let mut channels: Vec<Vec<f32>> = Vec::new();
    let mut rate = 0;
    let mut ts = 0;

    while let Some(frame) = es.next_frame(true) {
        let dec = dec.get_or_insert_with(|| {
            let params = es.info().unwrap().params.clone();
            DolbyEDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap()
        });

        let pkt = PacketRef::new(0, Timestamp::new(ts), Duration::new(frame.dur), &frame.data);
        ts += frame.dur as i64;

        let GenericAudioBufferRef::F32(buf) = dec.decode_ref(&pkt).unwrap()
        else {
            panic!("expected f32 samples")
        };

        rate = buf.spec().rate();

        if channels.is_empty() {
            channels = vec![Vec::new(); buf.spec().channels().count()];
        }

        for (c, plane) in channels.iter_mut().zip(buf.iter_planes()) {
            c.extend_from_slice(plane);
        }
    }

    (channels, rate)
}

#[test]
fn decode_matches_ffmpeg() {
    let Some(path) = sample_path(SAMPLE)
    else {
        return;
    };

    let (channels, rate) = decode(&path);
    assert_eq!(rate, 44800);
    assert_eq!(channels.len(), 6);
    assert_eq!(channels[0].len(), 256 * 1792);

    let Ok(out) = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&path)
        .args(["-f", "f32le", "-"])
        .output()
    else {
        eprintln!("ffmpeg not found; skipping");
        return;
    };

    assert!(out.status.success());

    let reference: Vec<f32> =
        out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();

    assert_eq!(reference.len(), 6 * channels[0].len());

    // FFmpeg outputs the channels interleaved, in the same order.
    let mut max_err = 0f32;
    let (mut sig, mut noise) = (0f64, 0f64);

    for (i, frame) in reference.chunks_exact(6).enumerate() {
        for (ch, &r) in frame.iter().enumerate() {
            let e = channels[ch][i] - r;
            max_err = max_err.max(e.abs());
            sig += f64::from(r) * f64::from(r);
            noise += f64::from(e) * f64::from(e);
        }
    }

    let snr = 10.0 * (sig / noise.max(1e-30)).log10();
    eprintln!("dolby_e 16-11: max error {max_err:e}, SNR {snr:.1} dB");
    assert!(max_err < 1e-5, "max error {max_err}");
}
