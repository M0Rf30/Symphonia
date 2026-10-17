// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Decodes `hca/*.hca` of the sample corpus with the reader and the decoder, and compares the
//! samples with FFmpeg's (which has the same algorithm; the transform is floating point, so the
//! comparison is by SNR). Skipped without `RMPD_SAMPLES` or `ffmpeg`.

use std::process::{Command, Stdio};

use symphonia_codec_hca::HcaDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_format_game::HcaReader;
use symphonia_testutil::{open_sample, sample_path};

fn decode(name: &str) -> Option<(Vec<f32>, usize)> {
    let mss = open_sample(format!("hca/{name}"))?;
    let mut reader = HcaReader::try_new(mss, FormatOptions::default()).unwrap();
    let Some(CodecParameters::Audio(params)) = reader.tracks()[0].codec_params.clone()
    else {
        panic!("not audio");
    };
    let mut dec = HcaDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

    let mut out = Vec::new();
    let mut channels = 0;
    while let Some(packet) = reader.next_packet().unwrap() {
        let GenericAudioBufferRef::F32(b) = dec.decode(&packet).unwrap()
        else {
            panic!("unexpected sample format");
        };
        channels = b.spec().channels().count();
        let mut v = Vec::new();
        b.copy_to_vec_interleaved::<f32>(&mut v);
        out.extend_from_slice(&v);
    }
    Some((out, channels))
}

fn ffmpeg_decode(name: &str) -> Option<Vec<f32>> {
    let path = sample_path(format!("hca/{name}"))?;
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-f", "f32le", "-"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| {
        out.stdout.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
    })
}

#[test]
fn matches_ffmpeg() {
    let dir = sample_path("hca");
    let Some(dir) = dir
    else {
        eprintln!("RMPD_SAMPLES not set or hca/ missing, skipping");
        return;
    };

    for entry in std::fs::read_dir(dir).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        if !name.ends_with(".hca") {
            continue;
        }
        let (Some((got, channels)), Some(want)) = (decode(&name), ffmpeg_decode(&name))
        else {
            eprintln!("{name}: skipping");
            continue;
        };
        assert_eq!(got.len(), want.len(), "{name}: length");

        let (mut sig, mut err, mut max) = (0.0f64, 0.0f64, 0.0f32);
        for (&g, &w) in got.iter().zip(&want) {
            sig += f64::from(w) * f64::from(w);
            err += f64::from(g - w) * f64::from(g - w);
            max = max.max((g - w).abs());
        }
        let snr = 10.0 * (sig / err.max(1e-30)).log10();
        eprintln!("{name}: {channels} ch, max error {max:e}, SNR {snr:.1} dB");
        assert!(sig > 0.0, "{name}: silent");
        assert!(snr >= 100.0, "{name}: SNR {snr} dB");
    }
}
