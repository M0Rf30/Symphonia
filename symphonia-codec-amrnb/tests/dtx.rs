// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Discontinuous transmission: SID and no data frames.
//!
//! `dtx_nodata_nb.amr` is the FFmpeg FATE sample `amrnb-dtx-nodata`, a real stream with 6 SID
//! frames and 19 frames without data between speech frames. FFmpeg's native decoder fails on these
//! frames, the reference is the OpenCORE decoder (`libopencore_amrnb`, a fixed point
//! implementation of the 3GPP reference code of TS 26.073), used through FFmpeg if available.
//!
//! OpenCORE truncates its output to 13 bits, so the comparisons have a noise floor of a few LSBs
//! of a 13 bit signal (8 LSBs of 16 bits), and the quiet comfort noise of the fixture is within
//! that floor. The synthetic streams below have a loud comfort noise to test the level and the
//! spectrum of the noise.

mod common;

use common::*;

const FILE: &str = "dtx_nodata_nb.amr";

/// Frame type of a TOC byte.
fn frame_type(frame: &[u8]) -> u8 {
    (frame[0] >> 3) & 15
}

/// Decodes `data` (a `#!AMR\n` file) with the OpenCORE AMR-NB decoder through FFmpeg, `None` if it
/// is unavailable.
fn opencore_decode_bytes(data: &[u8]) -> Option<Vec<f32>> {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    let path = std::env::temp_dir().join(format!(
        "symphonia_amrnb_dtx_{}_{}.amr",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, data).unwrap();

    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-c:a", "libopencore_amrnb", "-i"])
        .arg(&path)
        .args(["-f", "f32le", "-"])
        .output();
    let _ = std::fs::remove_file(&path);

    let out = out.ok()?;
    if !out.status.success() {
        return None;
    }
    Some(out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect())
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|v| f64::from(*v) * f64::from(*v)).sum::<f64>() / x.len() as f64).sqrt()
}

/// Decodes all packets, returning the samples of each packet.
fn decode_packets(packets: &[Vec<u8>]) -> Vec<Vec<f32>> {
    let mut dec = decoder();
    packets
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let mut out = Vec::new();
            decode_packet(&mut dec, p, &mut out).unwrap_or_else(|e| panic!("frame {i}: {e}"));
            assert_eq!(out.len(), 160, "frame {i}");
            out
        })
        .collect()
}

#[test]
fn all_frames_decode() {
    let data = std::fs::read(fixture(FILE)).unwrap();
    let frames = frames(&data);
    assert_eq!(frames.len(), 360);
    assert_eq!(frames.iter().filter(|f| frame_type(f) == 8).count(), 6);
    assert_eq!(frames.iter().filter(|f| frame_type(f) == 15).count(), 19);

    let packets: Vec<Vec<u8>> = frames.iter().map(|f| f.to_vec()).collect();
    let out: Vec<f32> = decode_packets(&packets).concat();
    assert_eq!(out.len(), 360 * 160);
    assert!(out.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
}

#[test]
fn fixture_comfort_noise_matches_opencore() {
    let Some(reference) = opencore_decode_bytes(&std::fs::read(fixture(FILE)).unwrap())
    else {
        eprintln!("ffmpeg with libopencore_amrnb not available, skipping");
        return;
    };
    assert_eq!(reference.len(), 360 * 160);

    let data = std::fs::read(fixture(FILE)).unwrap();
    let frames = frames(&data);
    let out = decode_file(FILE);

    // The comfort noise of this stream is very quiet (the SID frames signal silence), so it is
    // within the 13 bit truncation of the reference. Speech frames are not compared: the reference
    // decoder itself differs from FFmpeg's decoder by 10 dB SNR on the speech of this stream.
    for (i, f) in frames.iter().enumerate() {
        if matches!(frame_type(f), 8 | 15) {
            let (_, max) =
                compare(&out[i * 160..(i + 1) * 160], &reference[i * 160..(i + 1) * 160]);
            assert!(max < 16.0, "frame {i}: max difference {max:.1} LSB");
        }
    }
}

/// Appends `value` as `bits` bits, MSB first.
fn push_bits(bits: &mut Vec<bool>, value: u32, n: u32) {
    for i in (0..n).rev() {
        bits.push(value >> i & 1 == 1);
    }
}

/// Builds a SID frame (RFC 4867 storage format) of the speech mode `mode` from the codebook
/// indices: the LSF predictor init (3 bits), the three LSF indices (8, 9 and 9 bits) and the
/// energy index (6 bits). `update` sets the SID type indicator (STI).
fn sid_frame(update: bool, lsf: [u32; 4], energy: u32, mode: u8, quality: bool) -> Vec<u8> {
    let mut bits = Vec::new();
    push_bits(&mut bits, lsf[0], 3);
    push_bits(&mut bits, lsf[1], 8);
    push_bits(&mut bits, lsf[2], 9);
    push_bits(&mut bits, lsf[3], 9);
    push_bits(&mut bits, energy, 6);
    bits.push(update);
    // The mode indication is sent LSB first.
    for i in 0..3 {
        bits.push(mode >> i & 1 == 1);
    }
    bits.resize(40, false);

    let mut out = vec![(8 << 3) | (u8::from(quality) << 2)];
    out.extend(bits.chunks(8).map(|c| c.iter().fold(0u8, |acc, b| acc << 1 | u8::from(*b))));
    out
}

/// Codebook indices of two real SID frames of `dtx_nodata_nb.amr`. Random indices give unrealistic
/// filters, nearly unstable ones, for which the fixed point reference is not meaningful.
const LSF_A: [u32; 4] = [1, 54, 120, 109];
const LSF_B: [u32; 4] = [1, 88, 120, 109];

fn no_data_frame() -> Vec<u8> {
    vec![(15 << 3) | 4]
}

/// A stream of 4.75 kbit/s speech frames of `sample2.amr` with loud comfort noise in between:
///
/// ```text
/// [0, 30) speech, 30 SID first, 31-32 no data, 33 SID update, 34-43 no data, 44 SID update,
/// 45-51 no data, 52 SID update, 53-112 no data (the noise is muted after 50 empty frames),
/// 113-142 speech, 143 SID update, 144-148 no data, 149-168 speech
/// ```
fn synthetic_stream() -> Vec<Vec<u8>> {
    let data = std::fs::read(fixture("sample2.amr")).unwrap();
    let speech: Vec<Vec<u8>> = frames(&data).iter().map(|f| f.to_vec()).collect();
    assert!(speech.len() >= 80);

    let mut out = Vec::new();
    out.extend_from_slice(&speech[..30]);
    out.push(sid_frame(false, LSF_A, 0, 7, true));
    out.extend(std::iter::repeat_n(no_data_frame(), 2));
    out.push(sid_frame(true, LSF_B, 40, 7, true));
    out.extend(std::iter::repeat_n(no_data_frame(), 10));
    out.push(sid_frame(true, LSF_A, 50, 7, true));
    out.extend(std::iter::repeat_n(no_data_frame(), 7));
    out.push(sid_frame(true, LSF_B, 30, 7, true));
    out.extend(std::iter::repeat_n(no_data_frame(), 60));
    out.extend_from_slice(&speech[30..60]);
    out.push(sid_frame(true, LSF_A, 45, 3, true));
    out.extend(std::iter::repeat_n(no_data_frame(), 5));
    out.extend_from_slice(&speech[60..80]);
    out
}

fn amr_file(packets: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"#!AMR\n".to_vec();
    for p in packets {
        out.extend_from_slice(p);
    }
    out
}

#[test]
fn synthetic_stream_decodes() {
    let packets = synthetic_stream();
    assert_eq!(packets.len(), 169);

    let out = decode_packets(&packets);
    assert!(out.iter().flatten().all(|s| s.is_finite() && s.abs() <= 1.0));

    // The comfort noise has the level signalled by the SID frames, it is not silence.
    for i in [31, 32, 34, 40, 46, 51, 144, 146] {
        assert!(rms(&out[i]) * 32768.0 > 100.0, "frame {i}: {}", rms(&out[i]) * 32768.0);
    }
}

/// The RMS in 16-bit LSBs of frames of `synthetic_stream` decoded by the OpenCORE decoder.
const OPENCORE_RMS: [(usize, f64); 9] = [
    (31, 2029.5),
    (32, 3756.5),
    (36, 376.5),
    (40, 388.7),
    (46, 574.5),
    (51, 1260.9),
    (57, 164.4),
    (144, 807.5),
    (146, 897.7),
];

#[test]
fn synthetic_comfort_noise_level() {
    let out = decode_packets(&synthetic_stream());

    for (i, expected) in OPENCORE_RMS {
        let level = rms(&out[i]) * 32768.0;
        assert!(
            (level / expected - 1.0).abs() < 0.03,
            "frame {i}: RMS {level:.1} LSB, expected {expected:.1} LSB"
        );
    }
}

#[test]
fn synthetic_comfort_noise_matches_opencore() {
    let packets = synthetic_stream();
    let Some(reference) = opencore_decode_bytes(&amr_file(&packets))
    else {
        eprintln!("ffmpeg with libopencore_amrnb not available, skipping");
        return;
    };
    assert_eq!(reference.len(), packets.len() * 160);

    let out = decode_packets(&packets);

    for (i, p) in packets.iter().enumerate() {
        let ft = frame_type(p);
        let r = &reference[i * 160..(i + 1) * 160];
        let level = rms(r) * 32768.0;

        // The speech frames after the comfort noise differ: the reference mutes the first speech
        // frame after muted noise, and the speech decoder itself deviates from the reference.
        if !matches!(ft, 8 | 15) || level < 100.0 {
            continue;
        }

        // The noise has the level and the waveform of the reference: the pseudo-noise generators
        // run in lock step. The two frames after the first change of the spectrum are looser: the
        // synthesis memory of the previous frame rings through the new filter, and the post-filter
        // sections run in another order than in the reference.
        let ours = rms(&out[i]) * 32768.0;
        let (snr, max) = compare(&out[i], r);
        let (tolerance, min_snr) = if (34..=35).contains(&i) { (0.15, 15.0) } else { (0.05, 20.0) };
        assert!(
            (ours / level - 1.0).abs() < tolerance,
            "frame {i}: RMS {ours:.1}, reference {level:.1}"
        );
        assert!(snr > min_snr, "frame {i}: SNR {snr:.1} dB, max difference {max:.1} LSB");
    }
}

#[test]
fn no_data_outside_dtx_is_silent_and_keeps_state() {
    let data = std::fs::read(fixture("sample2.amr")).unwrap();
    let speech: Vec<Vec<u8>> = frames(&data).iter().map(|f| f.to_vec()).collect();

    // A frame without data or a lost speech frame in speech outputs silence.
    for ft in [14u8, 15] {
        let mut packets = speech[..10].to_vec();
        packets.push(vec![(ft << 3) | 4]);
        packets.extend_from_slice(&speech[10..20]);
        let with = decode_packets(&packets);

        assert!(with[10].iter().all(|s| *s == 0.0), "ft {ft}");

        // And the speech decoder is not disturbed.
        let without = decode_packets(&speech[..20]);
        assert_eq!(&with[..10], &without[..10]);
        assert_eq!(&with[11..], &without[10..], "ft {ft}");
    }
}

#[test]
fn leading_no_data_is_comfort_noise() {
    // The decoder starts in DTX like the reference: a stream that starts with a SID update and
    // frames without data produces noise (here with a large energy index).
    let mut packets = vec![sid_frame(true, LSF_B, 50, 7, true)];
    packets.extend(std::iter::repeat_n(no_data_frame(), 5));
    let out = decode_packets(&packets);

    for (i, frame) in out.iter().enumerate() {
        assert!(rms(frame) * 32768.0 > 100.0, "frame {i}");
    }
}

#[test]
fn sid_with_bad_quality_decodes() {
    // A SID frame marked as damaged (Q = 0) reuses the old noise parameters.
    let mut packets = vec![sid_frame(true, LSF_B, 50, 7, true)];
    packets.push(sid_frame(true, LSF_A, 10, 7, false));
    packets.push(no_data_frame());
    let out = decode_packets(&packets);

    // The level is that of the first SID frame (energy index 50), not that of the damaged frame
    // (energy index 10 is 2^10 times quieter).
    let (first, damaged) = (rms(&out[0]), rms(&out[1]));
    assert!(first * 32768.0 > 100.0);
    assert!(damaged > first * 0.5 && damaged < first * 2.0, "{first} {damaged}");
}

#[test]
fn reserved_frame_types_fail() {
    let mut dec = decoder();
    for ft in 9u8..=13 {
        let mut out = Vec::new();
        let packet = [(ft << 3) | 4, 0, 0, 0, 0, 0];
        assert!(decode_packet(&mut dec, &packet, &mut out).is_err(), "ft {ft}");
    }
}
