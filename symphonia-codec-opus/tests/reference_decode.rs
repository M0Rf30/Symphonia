// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Reference comparisons of the decoder and encoder that do not depend on the floating-point
//! bit patterns of the platform (the CELT MDCT and the SILK filters use the libm of the target):
//!
//! * The decoder output of the sample set is compared with libopus (through `ffmpeg -c:a
//!   libopus`, skipped when it is unavailable) by SNR and maximum difference. Set `RMPD_SAMPLES` to
//!   the sample directory: `RMPD_SAMPLES=/path/to/samples cargo test -p symphonia-codec-opus
//!   --features ogg --release --test reference_decode`. Packet loss concealment has no reference
//!   here; its output is checked for plausibility.
//! * The encoder always runs on synthetic signals: the packets are deterministic, and decode with
//!   this crate's decoder to the input signal within an SNR.

#![allow(clippy::unwrap_used)] // test/example code: a panic is the failure signal
use std::io::Cursor;
use std::path::Path;
use std::process::{Command, Stdio};

use symphonia_codec_opus::decoder::{OpusDecoder, SampleRate};
use symphonia_codec_opus::encoder::{BitrateMode, EncoderConfig, OpusEncoder, PRE_SKIP};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia_format_ogg::OggReader;
use symphonia_testutil::Lcg32;

fn synth(frames: usize, ch: usize, seed: u32) -> Vec<f32> {
    let mut rng = Lcg32(seed);
    let mut out = Vec::with_capacity(frames * ch);
    for i in 0..frames {
        let t = i as f64 / 48000.0;
        let env = 0.5 + 0.5 * (t * 3.0).sin();
        let tau = 2.0 * std::f64::consts::PI;
        let s = (0.3
            * env
            * ((tau * 440.0 * t).sin()
                + 0.5 * (tau * 3300.0 * t).sin()
                + 0.25 * (tau * 9100.0 * t).sin())) as f32
            + 0.05 * rng.next_f32();
        out.push(s);
        if ch == 2 {
            out.push(0.8 * s + 0.05 * rng.next_f32());
        }
    }
    out
}

fn encode_packets(cfg: EncoderConfig, pcm: &[f32]) -> Vec<Vec<u8>> {
    let mut enc = OpusEncoder::new(cfg).unwrap();
    let mut packets = enc.push(pcm);
    packets.extend(enc.finish());
    packets
}

/// `(channels, bitrate, mode, complexity, minimum round-trip SNR in dB)`.
const ENCODER_CASES: [(u8, u32, BitrateMode, u8, f64); 10] = [
    (1, 12_000, BitrateMode::Vbr, 9, 4.5),
    (1, 32_000, BitrateMode::Cbr, 10, 11.0),
    (1, 48_000, BitrateMode::ConstrainedVbr, 5, 12.5),
    (1, 64_000, BitrateMode::Vbr, 9, 14.0),
    (1, 96_000, BitrateMode::Vbr, 0, 17.5),
    (2, 24_000, BitrateMode::Vbr, 9, 8.0),
    (2, 64_000, BitrateMode::Cbr, 10, 11.0),
    (2, 96_000, BitrateMode::ConstrainedVbr, 5, 12.0),
    (2, 128_000, BitrateMode::Vbr, 9, 14.0),
    (2, 192_000, BitrateMode::Vbr, 0, 16.5),
];

#[test]
fn encoder_packets_decode_to_the_input() {
    for (n, (ch, bitrate, mode, complexity, min_snr)) in ENCODER_CASES.into_iter().enumerate() {
        let pcm = synth(48000 * 6, ch as usize, 7 + ch as u32);
        let mut cfg = EncoderConfig::new(ch, bitrate);
        cfg.mode = mode;
        cfg.complexity = complexity;
        let packets = encode_packets(cfg, &pcm);
        let label = format!("{ch}ch {bitrate} bps {mode:?} complexity {complexity}");

        // The encoder is deterministic.
        assert_eq!(packets, encode_packets(cfg, &pcm), "encoder output differs between runs");

        let decoded = decode_packets(ch, &packets);
        let snr = snr_db(&pcm, &decoded, ch as usize, PRE_SKIP as usize, 4 * 960);
        if std::env::var_os("GOLDEN_PRINT").is_some() {
            println!("case {n}: {label}: SNR {snr:.2} dB");
        }
        assert!(snr >= min_snr, "{label}: round-trip SNR {snr:.2} dB < {min_snr} dB");
    }
}

/// SNR in dB of `decoded` against `reference` (both interleaved, `channels` per frame), where
/// `decoded[i + delay]` corresponds to `reference[i]`; the first and last `skip` frames are
/// excluded.
fn snr_db(reference: &[f32], decoded: &[f32], channels: usize, delay: usize, skip: usize) -> f64 {
    let frames = reference.len() / channels;
    let (mut sig, mut err) = (0.0f64, 0.0f64);
    for i in skip..frames.saturating_sub(skip) {
        for c in 0..channels {
            let r = f64::from(reference[i * channels + c]);
            let d = f64::from(decoded[(i + delay) * channels + c]);
            sig += r * r;
            err += (r - d) * (r - d);
        }
    }
    10.0 * (sig / err.max(1e-30)).log10()
}

/// Decodes encoder packets of 20 ms with the crate's decoder.
fn decode_packets(ch: u8, packets: &[Vec<u8>]) -> Vec<f32> {
    let mut dec = OpusDecoder::try_new(SampleRate::Hz48000, ch).unwrap();
    let mut buf = vec![0.0f32; 5760 * ch as usize];
    let mut out = Vec::new();
    for p in packets {
        let n = dec.decode(Some(p), &mut buf, 5760).unwrap();
        out.extend_from_slice(&buf[..n * ch as usize]);
    }
    out
}

/// Reads `(channels, pre-skip, audio packets)` of a mono/stereo, mapping family 0 Ogg Opus file.
fn read_packets(path: &std::path::Path) -> Option<(u8, usize, Vec<Vec<u8>>)> {
    let data = std::fs::read(path).ok()?;
    let mss =
        MediaSourceStream::new(Box::new(Cursor::new(data)), MediaSourceStreamOptions::default());
    let mut reader = OggReader::try_new(mss, FormatOptions::default()).ok()?;
    let extra = match reader.tracks().first()?.codec_params.as_ref()? {
        CodecParameters::Audio(a) => a.extra_data.as_ref()?.to_vec(),
        _ => return None,
    };
    if extra.len() < 19
        || &extra[..8] != b"OpusHead"
        || extra[18] != 0
        || !(1..=2).contains(&extra[9])
    {
        return None;
    }
    let mut out = Vec::new();
    while let Ok(Some(p)) = reader.next_packet() {
        out.push(p.data.to_vec());
    }
    Some((extra[9], usize::from(u16::from_le_bytes([extra[10], extra[11]])), out))
}

/// Decodes every packet to interleaved `f32`. With `loss`, every 13th packet (from the 6th) is
/// lost, exercising PLC. A packet that fails to decode contributes no samples.
fn decode_all(ch: u8, packets: &[Vec<u8>], loss: bool) -> Vec<f32> {
    let mut dec = OpusDecoder::try_new(SampleRate::Hz48000, ch).unwrap();
    let mut buf = vec![0.0f32; 5760 * ch as usize];
    let mut out = Vec::new();
    for (idx, p) in packets.iter().enumerate() {
        let pk = if loss && idx % 13 == 5 { None } else { Some(p.as_slice()) };
        let capacity = if pk.is_none() { dec.last_packet_duration() } else { 5760 };
        if let Ok(n) = dec.decode(pk, &mut buf, capacity) {
            out.extend_from_slice(&buf[..n * ch as usize]);
        }
    }
    out
}

/// Decodes `path` to interleaved `f32` at 48 kHz with libopus through ffmpeg, which drops the
/// pre-skip. Returns `None` if ffmpeg or libopus is unavailable.
fn libopus_pcm(path: &Path) -> Option<Vec<f32>> {
    let out = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-nostats",
            "-hide_banner",
            "-c:a",
            "libopus",
            "-request_sample_fmt",
            "flt",
            "-i",
        ])
        .arg(path)
        .args(["-map", "0:a:0", "-c:a", "pcm_f32le", "-f", "f32le", "-"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| {
        out.stdout.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
    })
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

/// Minimum SNR (dB) of the decoder output against libopus.
const MIN_SNR_DB: f64 = 100.0;
/// Maximum sample difference against libopus.
const MAX_DIFF: f32 = 1e-4;

/// The files of the sample set: mono and stereo, mapping family 0.
const DECODER_FILES: [&str; 10] = [
    "opus_cbr_96.opus",
    "opus_ffm_celt_fb.opus",
    "opus_ffm_fd120.opus",
    "opus_ffm_fd2.5.opus",
    "opus_ffm_hybrid_st.opus",
    "opus_ffm_silk_wb_st.opus",
    "opus_low_24.opus",
    "opus_mono.opus",
    "opus_music_256.opus",
    "opus_speech_mono_8.opus",
];

#[test]
fn decoder_output_matches_libopus() {
    let Some(dir) = symphonia_testutil::sample_dir().map(|d| d.join("opus"))
    else {
        eprintln!("RMPD_SAMPLES not set; skipping");
        return;
    };
    let print = std::env::var_os("GOLDEN_PRINT").is_some();
    for name in DECODER_FILES {
        let path = dir.join(name);
        let Some((ch, pre_skip, packets)) = read_packets(&path)
        else {
            eprintln!("{name}: not available; skipping");
            continue;
        };
        let ch = usize::from(ch);
        let ours = decode_all(ch as u8, &packets, false);
        assert!(ours.len() > pre_skip * ch, "{name}: no output");
        let ours = &ours[pre_skip * ch..];

        // Packet loss concealment: the stream keeps its length and its level.
        let lossy = decode_all(ch as u8, &packets, true);
        assert!(lossy.iter().all(|v| v.is_finite()), "{name}: PLC output is not finite");
        let (r, rl) = (rms(ours), rms(&lossy[pre_skip * ch..]));
        assert!(rl > 0.25 * r && rl < 4.0 * r, "{name}: PLC level {rl} vs {r}");

        let Some(reference) = libopus_pcm(&path)
        else {
            eprintln!("libopus not available through ffmpeg; skipping {name}");
            continue;
        };
        // ffmpeg also trims the end of the stream to its granule position, by less than a packet.
        let n = ours.len().min(reference.len());
        assert!(
            n + 5760 * ch >= ours.len(),
            "{name}: libopus output is {} samples",
            reference.len()
        );
        let (mut signal, mut noise, mut diff) = (0.0f64, 0.0f64, 0.0f32);
        for (&s, &x) in ours[..n].iter().zip(&reference[..n]) {
            signal += f64::from(x).powi(2);
            noise += f64::from(s - x).powi(2);
            diff = diff.max((s - x).abs());
        }
        let snr = 10.0 * (signal / noise.max(1e-30)).log10();
        if print {
            println!("{name}: SNR {snr:.2} dB max {diff:.6}");
        }
        assert!(snr >= MIN_SNR_DB, "{name}: SNR {snr:.2} dB < {MIN_SNR_DB} dB");
        assert!(diff <= MAX_DIFF, "{name}: max difference {diff} > {MAX_DIFF}");
    }
}
