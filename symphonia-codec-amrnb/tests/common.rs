// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(dead_code, clippy::unwrap_used)]

use std::path::PathBuf;

use symphonia_codec_amrnb::AmrNbDecoder;
use symphonia_core::codecs::audio::well_known::CODEC_ID_AMR_NB;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::errors::Result;
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

/// Speech bytes (excluding the TOC byte) per frame type, as in an RFC 4867 `.amr` file.
const SIZES: [usize; 16] = [12, 13, 15, 17, 19, 20, 26, 31, 5, 5, 5, 5, 0, 0, 0, 0];

pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// Splits a `#!AMR\n` file into frames ([TOC][speech bits]).
pub fn frames(data: &[u8]) -> Vec<&[u8]> {
    let mut data = data.strip_prefix(b"#!AMR\n").expect("AMR magic");
    let mut out = Vec::new();
    while let Some(&toc) = data.first() {
        let n = 1 + SIZES[usize::from((toc >> 3) & 15)];
        if n > data.len() {
            break;
        }
        let (f, rest) = data.split_at(n);
        out.push(f);
        data = rest;
    }
    out
}

pub fn decoder() -> AmrNbDecoder {
    let mut p = AudioCodecParameters::new();
    p.for_codec(CODEC_ID_AMR_NB).with_sample_rate(8000);
    AmrNbDecoder::try_new(&p, &AudioDecoderOptions::default()).unwrap()
}

/// Decodes one packet, appending the f32 samples to `out`.
pub fn decode_packet(dec: &mut AmrNbDecoder, frame: &[u8], out: &mut Vec<f32>) -> Result<()> {
    let pkt = PacketRef::new(0, Timestamp::new(0), Duration::new(160), frame);
    let mut tmp = Vec::new();
    dec.decode_ref(&pkt)?.copy_to_vec_interleaved::<f32>(&mut tmp);
    out.extend_from_slice(&tmp);
    Ok(())
}

/// Decodes all frames of a file; frames that fail to decode are skipped.
pub fn decode_file(name: &str) -> Vec<f32> {
    let data = std::fs::read(fixture(name)).unwrap();
    let mut dec = decoder();
    let mut out = Vec::new();
    for f in frames(&data) {
        decode_packet(&mut dec, f, &mut out).unwrap();
    }
    out
}

/// Decodes a file with FFmpeg's native `amrnb` decoder; `None` if ffmpeg is unavailable.
pub fn ffmpeg_decode(name: &str) -> Option<Vec<f32>> {
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-c:a", "amrnb", "-i"])
        .arg(fixture(name))
        .args(["-f", "f32le", "-"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect())
}

/// Returns (SNR in dB, max absolute difference in 16-bit LSBs).
pub fn compare(a: &[f32], reference: &[f32]) -> (f64, f64) {
    let mut sig = 0.0f64;
    let mut err = 0.0f64;
    let mut max = 0.0f64;
    for (x, r) in a.iter().zip(reference) {
        let d = f64::from(*x) - f64::from(*r);
        sig += f64::from(*r) * f64::from(*r);
        err += d * d;
        max = max.max(d.abs());
    }
    let snr = if err == 0.0 { f64::INFINITY } else { 10.0 * (sig / err).log10() };
    (snr, max * 32768.0)
}
