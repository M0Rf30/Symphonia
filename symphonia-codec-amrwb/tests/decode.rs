// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Decodes real AMR-WB streams in every mode and compares the output with FFmpeg's `amrwb`
//! decoder (live, when `ffmpeg` is on the `PATH`) and with committed reference PCM produced by
//! it. The decoder is floating point, so the comparison is made with an SNR / maximum error
//! tolerance rather than bit for bit.
//!
//! Fixtures (`tests/fixtures/*.awb`) are the first 160 frames of the FFmpeg FATE samples
//! `amrwb/seed-*.awb` and `amrwb/deus-23k85.awb` (<https://fate-suite.ffmpeg.org/amrwb/>), remuxed
//! to the `.awb` storage format with `ffmpeg -c copy`. `tests/fixtures/*.s16` hold the first 20
//! frames decoded by FFmpeg 9.0.2 (`ffmpeg -i x.awb -f s16le -`), 16 kHz mono.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use symphonia_codec_amrwb::AmrWbDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::well_known::CODEC_ID_AMR_WB;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

/// Frame sizes in bytes (including the TOC byte) of each frame type.
const FRAME_SIZES: [usize; 16] = [18, 24, 33, 37, 41, 47, 51, 59, 61, 6, 1, 1, 1, 1, 1, 1];

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// Splits a `.awb` file into frames.
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

fn packet(data: &[u8]) -> PacketRef<'_> {
    PacketRef::new(0, Timestamp::new(0), Duration::new(0), data)
}

fn decode_all(dec: &mut AmrWbDecoder, frames: &[&[u8]]) -> Vec<f32> {
    let mut out = Vec::new();
    for f in frames {
        let GenericAudioBufferRef::F32(buf) = dec.decode_ref(&packet(f)).unwrap()
        else {
            panic!("expected f32 output");
        };
        assert_eq!(buf.frames(), 320);
        out.extend_from_slice(buf.plane(0).unwrap());
    }
    out
}

/// Decodes with FFmpeg, if available.
fn ffmpeg_decode(path: &std::path::Path) -> Option<Vec<f32>> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-f", "f32le", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(out.stdout.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect())
}

/// Returns (max abs difference in 16-bit LSBs, SNR in dB).
fn compare(got: &[f32], want: &[f32]) -> (f32, f64) {
    assert_eq!(got.len(), want.len());
    let mut max = 0.0f32;
    let (mut sig, mut err) = (0.0f64, 0.0f64);
    for (&g, &w) in got.iter().zip(want) {
        max = max.max((g - w).abs() * 32768.0);
        sig += f64::from(w) * f64::from(w);
        err += f64::from(g - w) * f64::from(g - w);
    }
    let snr = if err == 0.0 { f64::INFINITY } else { 10.0 * (sig / err).log10() };
    (max, snr)
}

const SEEDS: [&str; 9] = [
    "seed-6k60",
    "seed-8k85",
    "seed-12k65",
    "seed-14k25",
    "seed-15k85",
    "seed-18k25",
    "seed-19k85",
    "seed-23k05",
    "seed-23k85",
];

const ALL: [&str; 10] = [
    "seed-6k60",
    "seed-8k85",
    "seed-12k65",
    "seed-14k25",
    "seed-15k85",
    "seed-18k25",
    "seed-19k85",
    "seed-23k05",
    "seed-23k85",
    "deus-23k85",
];

/// Maximum accepted difference to FFmpeg, in 16-bit LSBs. FFmpeg's own float summation order
/// (SIMD) changes the truncated excitation by one unit now and then; measured: <= 1.6.
const MAX_LSB: f32 = 2.0;
/// Minimum accepted SNR against FFmpeg, in dB. Measured: >= 87.
const MIN_SNR: f64 = 80.0;

#[test]
fn verify_all_modes_match_committed_reference() {
    for name in ALL {
        let data = std::fs::read(fixture(&format!("{name}.awb"))).unwrap();
        let fr = frames(&data);
        let want = std::fs::read(fixture(&format!("{name}.s16"))).unwrap();
        let want: Vec<f32> = want
            .chunks_exact(2)
            .map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])) / 32768.0)
            .collect();
        let got = decode_all(&mut decoder(), &fr[..want.len() / 320]);
        let (max, _) = compare(&got, &want);
        // The reference is rounded to 16 bits (up to 0.5 LSB, 1/sqrt(12) rms). The committed
        // excerpts are short (16 frames) and quiet, so the rounding noise limits their SNR (35 dB
        // for `deus-23k85`) and only the absolute bound applies; the full-file comparison with
        // FFmpeg below holds the SNR bound.
        assert!(max <= MAX_LSB + 0.5, "{name}: max diff {max} LSB");
    }
}

#[test]
fn verify_all_modes_match_ffmpeg() {
    for name in ALL {
        let path = fixture(&format!("{name}.awb"));
        let data = std::fs::read(&path).unwrap();
        let fr = frames(&data);
        assert_eq!(fr.len(), 160);
        let got = decode_all(&mut decoder(), &fr);
        let Some(want) = ffmpeg_decode(&path)
        else {
            eprintln!("ffmpeg not available, skipping {name}");
            continue;
        };
        let (max, snr) = compare(&got, &want);
        eprintln!("{name}: {} samples, max diff {max:.3} LSB, SNR {snr:.1} dB", got.len());
        assert!(max <= MAX_LSB, "{name}: max diff {max} LSB");
        assert!(snr >= MIN_SNR, "{name}: SNR {snr} dB");
    }
}

/// Builds a TOC byte.
fn toc(frame_type: u8, good: bool) -> u8 {
    (frame_type << 3) | if good { 4 } else { 0 }
}

#[test]
fn verify_lost_frames_are_silent_and_keep_state() {
    let data = std::fs::read(fixture("seed-12k65.awb")).unwrap();
    let fr = frames(&data);

    let reference = decode_all(&mut decoder(), &fr[..8]);

    let mut dec = decoder();
    let mut out = decode_all(&mut dec, &fr[..4]);
    let mut damaged = fr[4].to_vec();
    damaged[0] = toc(2, false);
    for p in [&[toc(15, true)][..], &[toc(14, true)], &[toc(14, false)], &damaged] {
        let silent = decode_all(&mut dec, &[p]);
        assert_eq!(silent.len(), 320);
        assert!(silent.iter().all(|&s| s == 0.0));
    }
    out.extend(decode_all(&mut dec, &fr[4..8]));
    assert_eq!(out, reference, "state must be untouched by lost frames");
}

#[test]
fn verify_malformed_packets_are_errors() {
    let data = std::fs::read(fixture("seed-23k85.awb")).unwrap();
    let fr = frames(&data);
    let mut dec = decoder();

    assert!(dec.decode_ref(&packet(&[])).is_err());
    // Reserved frame types.
    for ft in 10..14 {
        assert!(dec.decode_ref(&packet(&[toc(ft, true)])).is_err());
    }
    // Every speech mode truncated by one byte or reduced to a TOC byte.
    for (mode, size) in FRAME_SIZES.iter().take(9).enumerate() {
        let mut p = vec![toc(mode as u8, true)];
        p.resize(*size - 1, 0x55);
        assert!(dec.decode_ref(&packet(&p)).is_err(), "mode {mode}");
        assert_eq!(dec.last_decoded().frames(), 0, "buffer must be cleared after an error");
        assert!(dec.decode_ref(&packet(&p[..1])).is_err(), "mode {mode}");
    }
    // The decoder is still usable and unaffected afterwards.
    let a = decode_all(&mut dec, &fr[..4]);
    let b = decode_all(&mut decoder(), &fr[..4]);
    assert_eq!(a, b);
}

#[test]
fn verify_extra_trailing_bytes_are_ignored() {
    let data = std::fs::read(fixture("seed-8k85.awb")).unwrap();
    let fr = frames(&data);
    let padded: Vec<Vec<u8>> = fr[..4]
        .iter()
        .map(|f| {
            let mut v = f.to_vec();
            v.extend_from_slice(&[0xff; 3]);
            v
        })
        .collect();
    let padded: Vec<&[u8]> = padded.iter().map(Vec::as_slice).collect();
    assert_eq!(decode_all(&mut decoder(), &padded), decode_all(&mut decoder(), &fr[..4]));
}

/// Small xorshift generator, so the test is deterministic without extra dependencies.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn verify_random_frames_never_panic_and_stay_finite() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut dec = decoder();
    for i in 0..6000 {
        let ft = (rng.next() % 16) as u8;
        let len = 1 + (rng.next() % 70) as usize;
        let mut p: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        // Mostly well-formed TOC bytes, sometimes arbitrary ones.
        p[0] = if i % 8 == 0 { rng.next() as u8 } else { toc(ft, true) | (p[0] & 0x83) };
        if let Ok(GenericAudioBufferRef::F32(buf)) = dec.decode_ref(&packet(&p)) {
            assert!(buf.plane(0).unwrap().iter().all(|s| s.is_finite()));
        }
    }
    // Mode switches on every frame, with full-length random payloads.
    for i in 0..3000 {
        let mode = (i * 7 % 9) as usize;
        let mut p: Vec<u8> = (0..FRAME_SIZES[mode]).map(|_| rng.next() as u8).collect();
        p[0] = toc(mode as u8, true);
        let GenericAudioBufferRef::F32(buf) = dec.decode_ref(&packet(&p)).unwrap()
        else {
            panic!("expected f32 output");
        };
        assert!(buf.plane(0).unwrap().iter().all(|s| s.is_finite()));
    }
}

#[test]
fn verify_mode_switching_is_deterministic_and_close_to_ffmpeg() {
    // Interleave frames of all nine modes, then compare with FFmpeg decoding the same sequence.
    let files: Vec<Vec<u8>> =
        SEEDS.iter().map(|n| std::fs::read(fixture(&format!("{n}.awb"))).unwrap()).collect();
    let all: Vec<Vec<&[u8]>> = files.iter().map(|d| frames(d)).collect();
    let mut mixed: Vec<u8> = b"#!AMR-WB\n".to_vec();
    let mut seq: Vec<&[u8]> = Vec::new();
    for i in 0..60 {
        let f = all[i % 9][i];
        mixed.extend_from_slice(f);
        seq.push(f);
    }
    let got = decode_all(&mut decoder(), &seq);
    assert_eq!(got, decode_all(&mut decoder(), &seq));

    let path = std::env::temp_dir().join(format!("amrwb-mixed-{}.awb", std::process::id()));
    std::fs::write(&path, &mixed).unwrap();
    let want = ffmpeg_decode(&path);
    std::fs::remove_file(&path).ok();
    if let Some(want) = want {
        let (max, snr) = compare(&got, &want);
        eprintln!("mode switching: max diff {max:.3} LSB, SNR {snr:.1} dB");
        assert!(max <= 8.0, "max diff {max} LSB");
        assert!(snr >= 60.0, "SNR {snr} dB");
    }
}

#[test]
fn verify_gapless_trim_and_reset() {
    let data = std::fs::read(fixture("seed-8k85.awb")).unwrap();
    let fr = frames(&data);

    let mut dec = decoder();
    let first = decode_all(&mut dec, &fr[..3]);
    dec.reset();
    assert_eq!(decode_all(&mut dec, &fr[..3]), first, "reset restores the initial state");

    let mut p = AudioCodecParameters::new();
    p.for_codec(CODEC_ID_AMR_WB);
    let mut opts = AudioDecoderOptions::default();
    opts.gapless = true;
    let mut dec = AmrWbDecoder::try_new(&p, &opts).unwrap();
    let mut pk = PacketRef::new(0, Timestamp::new(0), Duration::new(200), fr[0]);
    pk.trim_start = Duration::new(100);
    pk.trim_end = Duration::new(20);
    let GenericAudioBufferRef::F32(buf) = dec.decode_ref(&pk).unwrap()
    else {
        panic!("expected f32 output");
    };
    assert_eq!(buf.frames(), 200);
    assert_eq!(buf.plane(0).unwrap(), &first[100..300]);
}

#[test]
fn verify_codec_params() {
    let mut p = AudioCodecParameters::new();
    p.for_codec(CODEC_ID_AMR_WB);
    let dec = AmrWbDecoder::try_new(&p, &AudioDecoderOptions::default()).unwrap();
    let out = dec.codec_params();
    assert_eq!(out.sample_rate, Some(16000));
    assert_eq!(out.channels.as_ref().map(|c| c.count()), Some(1));
    assert_eq!(out.max_frames_per_packet, Some(320));

    // Wrong codec and stereo are rejected.
    let mut bad = AudioCodecParameters::new();
    bad.for_codec(symphonia_core::codecs::audio::well_known::CODEC_ID_AMR_NB);
    assert!(AmrWbDecoder::try_new(&bad, &AudioDecoderOptions::default()).is_err());
    let mut st = AudioCodecParameters::new();
    st.for_codec(CODEC_ID_AMR_WB)
        .with_channels(symphonia_core::audio::layouts::CHANNEL_LAYOUT_STEREO);
    assert!(AmrWbDecoder::try_new(&st, &AudioDecoderOptions::default()).is_err());
}
