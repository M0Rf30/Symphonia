// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Decodes the AC-4 streams of `ac4/*.ac4` (sync-framed elementary streams) from the sample
//! corpus and compares them with `ac4/*.f32`, the interleaved output of FFmpeg's AC-4 decoder
//! for the same file. The decoder is floating point, so the comparison is by SNR. Skipped
//! without `RMPD_SAMPLES`.

use symphonia_codec_ac4::Ac4Decoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::well_known::CODEC_ID_AC4;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::Packet;
use symphonia_core::units::{Duration, Timestamp};
use symphonia_testutil::{XorShift64, sample_path};

/// Splits sync-framed AC-4 data (`0xAC40`/`0xAC41`, 16-bit size, optional 24-bit escape).
fn frames(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 4 <= data.len() {
        let mut size = usize::from(u16::from_be_bytes([data[pos + 2], data[pos + 3]]));
        let mut hdr = 4;
        if size == 0xffff {
            if pos + 7 > data.len() {
                break;
            }
            size = (usize::from(data[pos + 4]) << 16)
                | (usize::from(data[pos + 5]) << 8)
                | usize::from(data[pos + 6]);
            hdr = 7;
        }
        let crc = if data[pos + 1] == 0x41 { 2 } else { 0 };
        let end = pos + hdr + size + crc;
        if end > data.len() {
            break;
        }
        out.push(&data[pos..end]);
        pos = end;
    }
    out
}

/// Decodes the framed stream `data`, returning the interleaved samples and the channel count.
fn decode(data: &[u8]) -> (Vec<f32>, usize) {
    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_AC4);
    let mut dec = Ac4Decoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

    let mut out = Vec::new();
    let mut channels = 0;
    for (i, frame) in frames(data).into_iter().enumerate() {
        let packet = Packet::new(0, Timestamp::new(i as i64), Duration::new(1), frame);
        let Ok(GenericAudioBufferRef::F32(b)) = dec.decode(&packet)
        else {
            continue;
        };
        channels = b.spec().channels().count();
        let mut v = Vec::new();
        b.copy_to_vec_interleaved::<f32>(&mut v);
        out.extend_from_slice(&v);
    }
    (out, channels)
}

fn check(name: &str, min_snr: f64) {
    let (Some(src), Some(refp)) =
        (sample_path(format!("ac4/{name}.ac4")), sample_path(format!("ac4/{name}.f32")))
    else {
        eprintln!("RMPD_SAMPLES not set or ac4/{name} missing, skipping");
        return;
    };
    let data = std::fs::read(src).unwrap();
    let want: Vec<f32> = std::fs::read(refp)
        .unwrap()
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let (got, channels) = decode(&data);
    eprintln!("{name}: {channels} ch, {} samples (reference {})", got.len(), want.len());
    assert_eq!(got.len(), want.len(), "{name}: length");

    let (mut sig, mut err, mut max) = (0.0f64, 0.0f64, 0.0f32);
    for (&g, &w) in got.iter().zip(&want) {
        sig += f64::from(w) * f64::from(w);
        err += f64::from(g - w) * f64::from(g - w);
        max = max.max((g - w).abs());
    }
    let snr = 10.0 * (sig / err.max(1e-30)).log10();
    eprintln!("{name}: max error {max:e}, SNR {snr:.1} dB");
    assert!(sig > 0.0, "{name}: silent");
    assert!(snr >= min_snr, "{name}: SNR {snr} dB");
}

#[test]
fn stereo_matches_ffmpeg() {
    check("KDAF_st", 120.0);
}

#[test]
fn stereo_aspx_matches_ffmpeg() {
    check("aspx_st", 120.0);
}

#[test]
fn surround_matches_ffmpeg() {
    check("KDAF_51", 120.0);
}

/// Decoding damaged frames must fail or succeed, never panic.
#[test]
fn corrupt_frames_do_not_panic() {
    let mut rng = XorShift64(0xac4);
    for name in ["KDAF_st", "aspx_st", "KDAF_51"] {
        let Some(src) = sample_path(format!("ac4/{name}.ac4"))
        else {
            eprintln!("RMPD_SAMPLES not set or ac4/{name} missing, skipping");
            return;
        };
        let data = std::fs::read(src).unwrap();
        let mut params = AudioCodecParameters::new();
        params.for_codec(CODEC_ID_AC4);
        let mut dec = Ac4Decoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

        for (i, frame) in frames(&data).into_iter().take(300).enumerate() {
            let mut damaged = frame.to_vec();
            for _ in 0..1 + rng.below(8) {
                let at = rng.below(damaged.len());
                damaged[at] ^= 1 << rng.below(8);
            }
            let packet = Packet::new(0, Timestamp::new(i as i64), Duration::new(1), damaged);
            let _ = dec.decode(&packet);
        }
    }
}

/// A packet cut short must be rejected rather than decoded from zero-filled reads.
#[test]
fn truncated_frame_is_an_error() {
    let Some(src) = sample_path("ac4/KDAF_st.ac4")
    else {
        eprintln!("RMPD_SAMPLES not set or ac4/KDAF_st missing, skipping");
        return;
    };
    let data = std::fs::read(src).unwrap();
    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_AC4);
    let mut dec = Ac4Decoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

    let frames = frames(&data);
    assert!(dec.decode(&Packet::new(0, Timestamp::new(0), Duration::new(1), frames[0])).is_ok());
    let cut = &frames[1][..frames[1].len() - 8];
    let packet = Packet::new(0, Timestamp::new(1), Duration::new(1), cut);
    assert!(dec.decode(&packet).is_err());
}
