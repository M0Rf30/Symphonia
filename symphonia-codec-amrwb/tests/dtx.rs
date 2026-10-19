// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Discontinuous transmission: SID (comfort noise) and NO_DATA frames, compared with the OpenCORE
//! AMR-WB decoder (3GPP TS 26.192 / 26.193 reference behaviour).
//!
//! FFmpeg's native `amrwb` decoder does not implement comfort noise, so the reference is
//! OpenCORE (`opencore-amr` 0.1.6, `ffmpeg -c:a libopencore_amrwb`), which is bit-exact fixed
//! point, whereas this decoder is floating point and bit-reordered after FFmpeg's.
//!
//! `fixtures/dtx_wb.awb` is a synthetic 277 frame stream: speech frames 0 to 99 of
//! `seed-12k65.awb` (12.65 kbit/s), interleaved with comfort noise: SID_FIRST and SID_UPDATE
//! frames with hand-picked and pseudo-random ISF / energy indices (some with the dithering flag
//! set), NO_DATA frames in between, and a 60 frame run without SID updates after which the noise
//! is faded out:
//!
//! | frames    | content                                                            |
//! |-----------|--------------------------------------------------------------------|
//! | 0..40     | speech                                                             |
//! | 40..85    | SID_FIRST, SID_UPDATE x3 (every 8 frames), NO_DATA                 |
//! | 85..115   | speech                                                             |
//! | 115..247  | SID_FIRST, SID_UPDATE x8 (every 8 frames), NO_DATA (60 in a row)   |
//! | 247..277  | speech                                                             |
//!
//! `fixtures/dtx_wb_opencore.rms` holds the RMS of every 20 ms frame of OpenCORE's output
//! (`ffmpeg -c:a libopencore_amrwb -i dtx_wb.awb -f s16le`), in 16-bit LSBs.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use symphonia_codec_amrwb::AmrWbDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::well_known::CODEC_ID_AMR_WB;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

/// Frame sizes in bytes (including the TOC byte) of each frame type.
const FRAME_SIZES: [usize; 16] = [18, 24, 33, 37, 41, 47, 51, 59, 61, 6, 1, 1, 1, 1, 1, 1];

/// Frames that are comfort noise in the reference decoder.
fn is_comfort_noise(frame: usize) -> bool {
    (40..85).contains(&frame) || (115..247).contains(&frame)
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn frames(data: &[u8]) -> Vec<&[u8]> {
    assert_eq!(&data[..9], b"#!AMR-WB\n");
    let mut data = &data[9..];
    let mut v = Vec::new();
    while !data.is_empty() {
        let n = FRAME_SIZES[usize::from((data[0] >> 3) & 0xf)];
        v.push(&data[..n]);
        data = &data[n..];
    }
    v
}

fn decoder() -> AmrWbDecoder {
    let mut p = AudioCodecParameters::new();
    p.for_codec(CODEC_ID_AMR_WB);
    AmrWbDecoder::try_new(&p, &AudioDecoderOptions::default()).unwrap()
}

fn decode_one(dec: &mut AmrWbDecoder, data: &[u8]) -> Vec<f32> {
    let pkt = PacketRef::new(0, Timestamp::new(0), Duration::new(0), data);
    let GenericAudioBufferRef::F32(buf) = dec.decode_ref(&pkt).unwrap()
    else {
        panic!("expected f32 output");
    };
    assert_eq!(buf.frames(), 320);
    buf.plane(0).unwrap().to_vec()
}

fn decode_all(dec: &mut AmrWbDecoder, frames: &[&[u8]]) -> Vec<Vec<f32>> {
    frames.iter().map(|f| decode_one(dec, f)).collect()
}

/// RMS in 16-bit LSBs.
fn rms(frame: &[f32]) -> f64 {
    (frame.iter().map(|&s| f64::from(s * 32768.0).powi(2)).sum::<f64>() / frame.len() as f64).sqrt()
}

fn stream() -> Vec<u8> {
    std::fs::read(fixture("dtx_wb.awb")).unwrap()
}

fn reference_rms() -> Vec<f64> {
    std::fs::read_to_string(fixture("dtx_wb_opencore.rms"))
        .unwrap()
        .lines()
        .map(|l| l.trim().parse().unwrap())
        .collect()
}

/// Decodes with the OpenCORE decoder through FFmpeg, if available.
fn opencore_decode(path: &Path) -> Option<Vec<f32>> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-c:a", "libopencore_amrwb", "-i"])
        .arg(path)
        .args(["-f", "s16le", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    Some(
        out.stdout
            .chunks_exact(2)
            .map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])) / 32768.0)
            .collect(),
    )
}

/// SNR of `got` against `want` in dB.
fn snr(got: &[f32], want: &[f32]) -> f64 {
    let (mut sig, mut err) = (0.0f64, 0.0f64);
    for (&g, &w) in got.iter().zip(want) {
        sig += f64::from(w) * f64::from(w);
        err += f64::from(g - w) * f64::from(g - w);
    }
    10.0 * (sig / err).log10()
}

#[test]
fn all_frames_decode() {
    let data = stream();
    let fr = frames(&data);
    assert_eq!(fr.len(), 277);

    // The SID frames used to fail with an "unsupported" error.
    let out = decode_all(&mut decoder(), &fr);

    for (i, frame) in out.iter().enumerate() {
        assert_eq!(frame.len(), 320);
        assert!(frame.iter().all(|s| s.is_finite() && s.abs() < 4.0), "frame {i}");
    }
}

#[test]
fn comfort_noise_level_matches_opencore() {
    let data = stream();
    let fr = frames(&data);
    let want = reference_rms();
    assert_eq!(want.len(), fr.len());

    let out = decode_all(&mut decoder(), &fr);

    let mut checked = 0;
    let mut max_dev = 0.0f64;

    for (i, frame) in out.iter().enumerate() {
        if !is_comfort_noise(i) || want[i] < 200.0 {
            continue;
        }
        let dev = (rms(frame) - want[i]).abs() / want[i];
        // The worst frame (57, the first after a SID update with a different spectrum) deviates
        // by 9%, the others by under 4%.
        assert!(dev < 0.12, "frame {i}: rms {} vs OpenCORE {} ({dev:.3})", rms(frame), want[i]);
        max_dev = max_dev.max(dev);
        checked += 1;
    }

    eprintln!("{checked} comfort noise frames compared, max deviation {max_dev:.3}");
    assert!(checked > 100);

    // The noise must not be silent (a decoder that emits zeros for SID frames fails here).
    assert!(out[41..47].iter().all(|f| rms(f) > 500.0));
}

#[test]
fn comfort_noise_matches_opencore_waveform() {
    let dir = fixture("dtx_wb.awb");
    let Some(want) = opencore_decode(&dir)
    else {
        eprintln!("ffmpeg with libopencore_amrwb not available, skipping");
        return;
    };

    let data = stream();
    let fr = frames(&data);
    let got: Vec<f32> = decode_all(&mut decoder(), &fr).concat();
    assert_eq!(got.len(), want.len());

    // The high band (6 to 7 kHz) of the comfort noise is generated from a different random
    // sequence (FFmpeg's `AVLFG` here, OpenCORE's linear congruential generator there), and the
    // reference is fixed point, so the waveforms cannot be identical.
    let mut worst = f64::INFINITY;
    for f in (40..85).chain(115..247) {
        let (g, w) = (&got[f * 320..(f + 1) * 320], &want[f * 320..(f + 1) * 320]);
        let w_rms = rms(w);
        if w_rms < 200.0 {
            continue;
        }
        let s = snr(g, w);
        // Frame 57 and 129 follow a spectrum change within the SID period.
        assert!(s > 12.0, "frame {f}: SNR {s:.1} dB");
        worst = worst.min(s);
    }
    eprintln!("worst comfort noise frame SNR {worst:.1} dB");

    // The whole faded and steady noise runs.
    let steady = |r: std::ops::Range<usize>| {
        snr(&got[r.start * 320..r.end * 320], &want[r.start * 320..r.end * 320])
    };
    let s = steady(130..180);
    assert!(s > 25.0, "SNR {s:.1} dB");
    let s = steady(186..247);
    assert!(s > 20.0, "SNR {s:.1} dB");
}

#[test]
fn sid_with_bad_quality_reuses_previous_parameters() {
    let data = stream();
    let fr = frames(&data);

    // Frame 56 is a SID_UPDATE; damage it: the parameters of the previous SID are used.
    assert_eq!((fr[56][0] >> 3) & 0xf, 9);
    let mut bad = fr[56].to_vec();
    bad[0] &= !0x4;

    let mut dec = decoder();
    decode_all(&mut dec, &fr[..56]);
    let out = decode_one(&mut dec, &bad);

    assert!(out.iter().all(|s| s.is_finite()));
    // Continuing noise, at about the level of the preceding frame.
    let level = rms(&out);
    assert!(level > 500.0 && level < 10_000.0, "rms {level}");
}

#[test]
fn truncated_sid_is_an_error_and_keeps_state() {
    let data = stream();
    let fr = frames(&data);

    let reference = decode_all(&mut decoder(), &fr[..50]);

    let mut dec = decoder();
    let mut out = decode_all(&mut dec, &fr[..48]);
    // A good SID whose payload is cut short.
    assert!(
        dec.decode_ref(&PacketRef::new(0, Timestamp::new(0), Duration::new(0), &fr[48][..4]))
            .is_err()
    );
    out.extend(decode_all(&mut dec, &fr[48..50]));
    assert_eq!(out, reference);
}

#[test]
fn no_data_while_speaking_does_not_start_comfort_noise() {
    let data = stream();
    let fr = frames(&data);

    // NO_DATA in the middle of speech (the decoder is not in DTX) is silence, and the following
    // speech decodes as if the frame had not been lost.
    let reference = decode_all(&mut decoder(), &fr[..10]);

    let mut dec = decoder();
    let mut out = decode_all(&mut dec, &fr[..5]);
    let silent = decode_one(&mut dec, fr[41]);
    assert!(silent.iter().all(|&s| s == 0.0));
    out.extend(decode_all(&mut dec, &fr[5..10]));
    assert_eq!(out, reference);
}

#[test]
fn comfort_noise_is_deterministic_across_reset() {
    let data = stream();
    let fr = frames(&data);

    let mut dec = decoder();
    let a = decode_all(&mut dec, &fr[..100]);
    dec.reset();
    let b = decode_all(&mut dec, &fr[..100]);
    assert_eq!(a, b);
}
