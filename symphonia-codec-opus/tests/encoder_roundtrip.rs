// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Round-trip and bitstream-validity tests of the Opus encoder against this crate's own decoder.
//!
//! Run with `cargo test -p symphonia-codec-opus --features ogg --release`.

use std::io::Cursor;
use std::path::PathBuf;

use symphonia_codec_opus::decoder::{OpusDecoder, SampleRate};
use symphonia_codec_opus::encoder::ogg::OggOpusWriter;
use symphonia_codec_opus::encoder::{BitrateMode, EncoderConfig, OpusEncoder, PRE_SKIP};
use symphonia_codec_opus::mapping::OpusHead;
use symphonia_codec_opus::packet::{self, Bandwidth, OpusMode, Toc};
use symphonia_codec_opus::range::RangeDecoder;

use symphonia_core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia_format_ogg::OggReader;

const FRAME: usize = 960;

// -- signal helpers ----------------------------------------------------------------------------

/// Deterministic uniform noise in [-1, 1).
struct Lcg(u32);

impl Lcg {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1664525).wrapping_add(1013904223);
        ((self.0 >> 8) as f32 / (1u32 << 23) as f32) - 1.0
    }
}

/// A logarithmic sine sweep from `f0` to `f1` Hz over `frames` sample frames; interleaved. In
/// stereo the right channel is the same sweep, attenuated and phase shifted.
fn sweep(frames: usize, channels: usize, f0: f64, f1: f64, amp: f32) -> Vec<f32> {
    let dur = frames as f64 / 48000.0;
    let k = (f1 / f0).ln() / dur;
    let mut out = Vec::with_capacity(frames * channels);
    for i in 0..frames {
        let t = i as f64 / 48000.0;
        let phase = 2.0 * std::f64::consts::PI * f0 * ((k * t).exp() - 1.0) / k;
        out.push(amp * phase.sin() as f32);
        if channels == 2 {
            out.push(0.7 * amp * (phase + 0.6).sin() as f32);
        }
    }
    out
}

/// A synthetic "music" signal: decaying plucked notes with harmonics, panned, plus a little
/// noise. Interleaved.
fn music(frames: usize, channels: usize, seed: u32) -> Vec<f32> {
    let mut rng = Lcg(seed);
    let mut out = vec![0.0f32; frames * channels];
    let note_len = 12000;
    let freqs = [220.0, 261.63, 329.63, 392.0, 440.0, 523.25, 659.25, 196.0];
    let mut start = 0;
    let mut n = 0;
    while start < frames {
        let f = freqs[n % freqs.len()] * (1.0 + 0.01 * (n / 8) as f64);
        let pan = 0.2 + 0.6 * ((n * 7 % 10) as f32 / 10.0);
        for i in 0..note_len.min(frames - start) {
            let t = i as f64 / 48000.0;
            let env = (-3.0 * t).exp() as f32 * (1.0 - (-200.0 * t).exp()) as f32;
            let mut s = 0.0f32;
            for h in 1..=8 {
                s += (2.0 * std::f64::consts::PI * f * h as f64 * t).sin() as f32
                    / (h as f32).powf(1.3);
            }
            s *= 0.2 * env;
            let idx = (start + i) * channels;
            if channels == 2 {
                out[idx] += s * (1.0 - pan);
                out[idx + 1] += s * pan;
            }
            else {
                out[idx] += s;
            }
        }
        start += note_len / 2;
        n += 1;
    }
    for v in out.iter_mut() {
        *v += 0.003 * rng.next();
    }
    out
}

/// Decodes packets with the crate's own decoder.
fn decode_all(packets: &[Vec<u8>], channels: u8) -> Vec<f32> {
    let mut dec = OpusDecoder::try_new(SampleRate::Hz48000, channels).unwrap();
    let mut out = Vec::new();
    let mut buf = vec![0.0f32; 5760 * channels as usize];
    for (i, p) in packets.iter().enumerate() {
        let n = dec
            .decode(Some(p), &mut buf, 5760)
            .unwrap_or_else(|e| panic!("packet {i} failed to decode: {e:?}"));
        assert_eq!(n, FRAME, "packet {i} decodes to one 20 ms frame");
        out.extend_from_slice(&buf[..n * channels as usize]);
    }
    out
}

fn encode_all(cfg: EncoderConfig, pcm: &[f32]) -> Vec<Vec<u8>> {
    let mut enc = OpusEncoder::new(cfg).unwrap();
    let mut packets = enc.push(pcm);
    packets.extend(enc.finish());
    packets
}

/// SNR in dB of `decoded` against `reference`, where `decoded[i + delay]` corresponds to
/// `reference[i]` (both interleaved, `channels` per frame). The first and last `skip` frames are
/// excluded.
fn snr_db(reference: &[f32], decoded: &[f32], channels: usize, delay: usize, skip: usize) -> f64 {
    let frames = reference.len() / channels;
    let mut sig = 0.0f64;
    let mut err = 0.0f64;
    for i in skip..frames.saturating_sub(skip) {
        for c in 0..channels {
            let r = reference[i * channels + c] as f64;
            let d = decoded[(i + delay) * channels + c] as f64;
            sig += r * r;
            err += (r - d) * (r - d);
        }
    }
    10.0 * (sig / err.max(1e-30)).log10()
}

fn mean_packet_size(packets: &[Vec<u8>]) -> f64 {
    packets.iter().map(|p| p.len()).sum::<usize>() as f64 / packets.len() as f64
}

// -- tests -------------------------------------------------------------------------------------

#[test]
fn sine_sweep_round_trip_snr() {
    // (channels, bitrate, mode, minimum SNR in dB)
    let cases = [
        (1usize, 64_000u32, BitrateMode::Vbr, 12.0f64),
        (1, 128_000, BitrateMode::Cbr, 18.0),
        (2, 96_000, BitrateMode::ConstrainedVbr, 12.0),
        (2, 128_000, BitrateMode::Vbr, 18.0),
        (2, 256_000, BitrateMode::Cbr, 25.0),
    ];
    for (ch, br, mode, min_snr) in cases {
        let pcm = sweep(48000 * 3, ch, 100.0, 15000.0, 0.5);
        let mut cfg = EncoderConfig::new(ch as u8, br);
        cfg.mode = mode;
        let packets = encode_all(cfg, &pcm);
        let decoded = decode_all(&packets, ch as u8);
        let snr = snr_db(&pcm, &decoded, ch, PRE_SKIP as usize, 4 * FRAME);
        eprintln!(
            "sweep ch={ch} br={br} {mode:?}: SNR {snr:.1} dB, mean {:.1} B",
            mean_packet_size(&packets)
        );
        assert!(snr >= min_snr, "sweep ch={ch} br={br} {mode:?}: SNR {snr:.1} dB < {min_snr}");
    }
}

#[test]
fn decoder_delay_is_pre_skip() {
    let pcm = music(48000 * 2, 1, 5);
    let packets = encode_all(EncoderConfig::new(1, 128_000), &pcm);
    let decoded = decode_all(&packets, 1);
    let best = (0..400usize)
        .map(|d| (d, snr_db(&pcm, &decoded, 1, d, 6 * FRAME)))
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .unwrap();
    assert_eq!(best.0, PRE_SKIP as usize, "delay with the best SNR ({:.1} dB)", best.1);
}

#[test]
fn music_snr_improves_with_bitrate() {
    let pcm = music(48000 * 4, 2, 11);
    let mut prev = f64::MIN;
    for br in [32_000u32, 64_000, 128_000, 256_000] {
        let packets = encode_all(EncoderConfig::new(2, br), &pcm);
        let decoded = decode_all(&packets, 2);
        let snr = snr_db(&pcm, &decoded, 2, PRE_SKIP as usize, 4 * FRAME);
        eprintln!(
            "music stereo br={br}: SNR {snr:.1} dB, mean {:.1} B",
            mean_packet_size(&packets)
        );
        assert!(snr > prev + 1.0, "SNR must grow with bitrate: {prev:.1} -> {snr:.1} at {br}");
        prev = snr;
    }
    assert!(prev > 20.0);
}

#[test]
fn packets_are_valid_celt_only_20ms_and_sized_per_mode() {
    let pcm = music(48000 * 3, 2, 3);
    for (br, mode) in [
        (48_000u32, BitrateMode::Cbr),
        (128_000, BitrateMode::Cbr),
        (256_000, BitrateMode::Cbr),
        (48_000, BitrateMode::ConstrainedVbr),
        (128_000, BitrateMode::ConstrainedVbr),
        (128_000, BitrateMode::Vbr),
    ] {
        let mut cfg = EncoderConfig::new(2, br);
        cfg.mode = mode;
        let packets = encode_all(cfg, &pcm);
        let expected = br as f64 / 400.0;
        // 24 kb/s per channel and up select fullband automatically.
        let want_bw = Bandwidth::Fullband;
        for (i, p) in packets.iter().enumerate() {
            let toc = Toc::new(p[0]);
            assert_eq!(toc.mode(), OpusMode::CeltOnly);
            assert_eq!(toc.bandwidth(), want_bw, "packet {i}");
            assert!(toc.stereo());
            assert_eq!(toc.frame_code(), 0);
            assert_eq!(toc.samples_per_frame(48000), 960);
            let parsed = packet::parse(p).unwrap();
            assert_eq!(parsed.frames.len(), 1);
            assert!(p.len() <= 1276);
            if mode == BitrateMode::Cbr {
                assert_eq!(p.len() as f64, expected.round(), "CBR packet {i} size");
            }
        }
        let mean = mean_packet_size(&packets);
        eprintln!("br={br} {mode:?}: mean {mean:.1} B (target {expected:.1})");
        let tol = match mode {
            BitrateMode::Cbr => 0.0,
            BitrateMode::ConstrainedVbr => 0.06,
            BitrateMode::Vbr => 0.15,
        };
        assert!(
            (mean - expected).abs() <= expected * tol + 0.5,
            "{mode:?} at {br} bps: mean packet {mean:.1} B vs target {expected:.1} B"
        );
    }
}

/// The encoder's final range-coder state must equal the decoder's after every packet (the check
/// libopus' `opus_demo` uses): it catches any encoder/decoder desynchronisation, including in
/// transient, stereo, low-rate and silent frames.
#[test]
fn final_range_matches_decoder() {
    let mut rng = Lcg(77);
    let mut signals: Vec<(&str, usize, Vec<f32>)> = vec![
        ("music stereo", 2, music(48000 * 3, 2, 41)),
        ("music mono", 1, music(48000 * 3, 1, 42)),
        ("sweep", 2, sweep(48000 * 2, 2, 60.0, 21000.0, 0.8)),
        ("noise", 2, (0..48000 * 4).map(|_| rng.next() * 0.3).collect()),
    ];
    let mut clicks = vec![0.0f32; 48000 * 2 * 2];
    for pos in (2000..96000).step_by(6000) {
        for i in 0..250 {
            clicks[(pos + i) * 2] = 0.9 * rng.next() * (-(i as f32) / 50.0).exp();
            clicks[(pos + i) * 2 + 1] = 0.4 * rng.next() * (-(i as f32) / 50.0).exp();
        }
    }
    signals.push(("clicks with silence", 2, clicks));
    for (name, ch, pcm) in &signals {
        for (br, mode) in [
            (6_000u32, BitrateMode::Cbr),
            (24_000, BitrateMode::ConstrainedVbr),
            (64_000, BitrateMode::Vbr),
            (128_000, BitrateMode::Cbr),
            (320_000, BitrateMode::Vbr),
        ] {
            for complexity in [0u8, 4, 10] {
                let mut cfg = EncoderConfig::new(*ch as u8, br);
                cfg.mode = mode;
                cfg.complexity = complexity;
                let mut enc = OpusEncoder::new(cfg).unwrap();
                let mut dec = OpusDecoder::try_new(SampleRate::Hz48000, *ch as u8).unwrap();
                let mut out = vec![0.0f32; 5760 * ch];
                for (i, frame) in pcm.chunks_exact(FRAME * ch).enumerate() {
                    let p = enc.encode(frame).unwrap();
                    dec.decode(Some(&p), &mut out, 5760).unwrap();
                    assert_eq!(
                        dec.final_range(),
                        enc.final_range(),
                        "{name} ch={ch} {br} {mode:?} complexity {complexity}: final range mismatch in packet {i}"
                    );
                }
            }
        }
    }
}

#[test]
fn every_complexity_level_produces_decodable_streams() {
    let pcm = music(48000 * 2, 2, 17);
    let mut snrs = Vec::new();
    for complexity in 0..=10u8 {
        let mut cfg = EncoderConfig::new(2, 96_000);
        cfg.complexity = complexity;
        let packets = encode_all(cfg, &pcm);
        let decoded = decode_all(&packets, 2);
        let snr = snr_db(&pcm, &decoded, 2, PRE_SKIP as usize, 3 * FRAME);
        eprintln!(
            "complexity {complexity}: SNR {snr:.1} dB, mean {:.1} B",
            mean_packet_size(&packets)
        );
        assert!(snr > 10.0, "complexity {complexity}: SNR {snr:.1}");
        snrs.push(snr);
    }
}

#[test]
fn stereo_edge_cases_decode() {
    let frames = 48000 * 2;
    let tone = sweep(frames, 1, 300.0, 6000.0, 0.4);
    let mut signals: Vec<(&str, Vec<f32>)> = Vec::new();
    // Hard-panned left, hard-panned right, anti-phase, identical channels.
    signals.push(("left only", tone.iter().flat_map(|&s| [s, 0.0]).collect()));
    signals.push(("right only", tone.iter().flat_map(|&s| [0.0, s]).collect()));
    signals.push(("anti-phase", tone.iter().flat_map(|&s| [s, -s]).collect()));
    signals.push(("dual mono", tone.iter().flat_map(|&s| [s, s]).collect()));
    for br in [24_000u32, 64_000, 192_000] {
        for (name, pcm) in &signals {
            let packets = encode_all(EncoderConfig::new(2, br), pcm);
            let decoded = decode_all(&packets, 2);
            let snr = snr_db(pcm, &decoded, 2, PRE_SKIP as usize, 4 * FRAME);
            eprintln!("stereo {name} br={br}: SNR {snr:.1} dB");
            // Even at 24 kb/s the decoded signal must resemble the input.
            assert!(snr > 3.0, "{name} @ {br}: SNR {snr:.1}");
        }
    }
}

#[test]
fn transients_use_short_blocks_and_decode() {
    // Silence with sharp clicks: the encoder must flag transient frames.
    let frames = 48000;
    let mut rng = Lcg(99);
    let mut pcm = vec![0.0f32; frames * 2];
    for pos in (5000..frames).step_by(7000) {
        for i in 0..200 {
            let env = (-(i as f32) / 30.0).exp();
            pcm[(pos + i) * 2] = 0.8 * env * rng.next();
            pcm[(pos + i) * 2 + 1] = 0.8 * env * rng.next();
        }
    }
    let packets = encode_all(EncoderConfig::new(2, 128_000), &pcm);
    let transient_frames = packets
        .iter()
        .filter(|p| {
            // silence flag (logp 15), post-filter flag (logp 1), transient flag (logp 3).
            let mut rd = RangeDecoder::new(&p[1..]);
            let silence = rd.dec_bit_logp(15);
            if silence {
                return false;
            }
            let _pf = rd.dec_bit_logp(1);
            rd.dec_bit_logp(3)
        })
        .count();
    eprintln!("transient frames: {transient_frames}/{}", packets.len());
    assert!(
        transient_frames >= 3,
        "clicks should be coded as transient frames, got {transient_frames}"
    );
    let decoded = decode_all(&packets, 2);
    let snr = snr_db(&pcm, &decoded, 2, PRE_SKIP as usize, FRAME);
    // Noise bursts are intrinsically hard to match sample by sample; libopus reaches the same
    // 7-8 dB on this signal. The test guards against gross breakage of the short-block path.
    assert!(snr > 5.0, "click train SNR {snr:.1}");
}

#[test]
fn digital_silence_is_cheap_and_does_not_disturb_neighbours() {
    let frames = 48000 * 2;
    let mut pcm = music(frames, 2, 21);
    // Zero out the middle second.
    for v in pcm[48000 * 2 / 2..48000 * 2 / 2 + 96000].iter_mut() {
        *v = 0.0;
    }
    let mut cfg = EncoderConfig::new(2, 128_000);
    cfg.mode = BitrateMode::ConstrainedVbr;
    let packets = encode_all(cfg, &pcm);
    let silent_frames = packets.iter().filter(|p| p.len() <= 3).count();
    assert!(silent_frames >= 40, "a second of silence should use tiny packets ({silent_frames})");
    let decoded = decode_all(&packets, 2);
    // The silent region (frames 24000..72000) decodes to (almost) exact zero, away from the edges.
    let mid = &decoded
        [(PRE_SKIP as usize + 24000 + 2 * FRAME) * 2..(PRE_SKIP as usize + 72000 - 2 * FRAME) * 2];
    let peak = mid.iter().fold(0.0f32, |a, &v| a.max(v.abs()));
    assert!(peak < 1e-3, "silence decoded with peak {peak}");
    // Audio after the silence is still intact.
    let tail_start = 72000 + 4 * FRAME;
    let r = &pcm[tail_start * 2..(tail_start + 16000) * 2];
    let d = &decoded
        [(tail_start + PRE_SKIP as usize) * 2..(tail_start + PRE_SKIP as usize + 16000) * 2];
    let snr = snr_db(r, d, 2, 0, 0);
    assert!(snr > 12.0, "audio after silence: SNR {snr:.1}");
}

#[test]
fn hostile_input_never_breaks_the_stream() {
    let mut rng = Lcg(1234);
    let n = 48000;
    let mut cases: Vec<(&str, Vec<f32>)> = Vec::new();
    cases.push(("full-scale noise", (0..n * 2).map(|_| rng.next()).collect()));
    cases.push((
        "clipping square",
        (0..n * 2).map(|i| if (i / 200) % 2 == 0 { 1.0 } else { -1.0 }).collect(),
    ));
    cases.push(("over-range", (0..n * 2).map(|_| rng.next() * 40.0).collect()));
    cases.push(("DC", vec![0.9f32; n * 2]));
    cases.push((
        "NaN/inf",
        (0..n * 2)
            .map(|i| match i % 97 {
                0 => f32::NAN,
                1 => f32::INFINITY,
                2 => f32::NEG_INFINITY,
                _ => rng.next() * 0.1,
            })
            .collect(),
    ));
    cases.push(("tiny noise", (0..n * 2).map(|_| rng.next() * 1e-7).collect()));
    cases.push((
        "impulse train",
        (0..n * 2).map(|i| if i % 1920 == 0 { 1.0 } else { 0.0 }).collect(),
    ));
    for (name, pcm) in cases {
        for ch in [1usize, 2] {
            // Mono view of the same material.
            let input: Vec<f32> =
                if ch == 2 { pcm.clone() } else { pcm.chunks(2).map(|c| c[0]).collect() };
            for (br, mode) in [
                (6_000u32, BitrateMode::Cbr),
                (6_000, BitrateMode::Vbr),
                (12_000, BitrateMode::ConstrainedVbr),
                (32_000, BitrateMode::Cbr),
                (160_000, BitrateMode::ConstrainedVbr),
                (510_000, BitrateMode::Vbr),
                (510_000, BitrateMode::Cbr),
            ] {
                let mut cfg = EncoderConfig::new(ch as u8, br);
                cfg.mode = mode;
                let packets = encode_all(cfg, &input);
                for p in &packets {
                    assert!(p.len() <= 1276, "{name} @ {br}: packet of {} bytes", p.len());
                    assert!(p.len() >= 3, "{name} @ {br}: packet of {} bytes", p.len());
                }
                let decoded = decode_all(&packets, ch as u8);
                assert!(decoded.iter().all(|v| v.is_finite()), "{name} @ {br}: non-finite output");
            }
        }
    }
}

#[test]
fn runtime_reconfiguration() {
    let pcm = music(48000 * 3, 2, 8);
    let mut enc = OpusEncoder::new(EncoderConfig::new(2, 64_000)).unwrap();
    let mut packets = Vec::new();
    for (i, frame) in pcm.chunks(FRAME * 2).filter(|c| c.len() == FRAME * 2).enumerate() {
        match i {
            50 => enc.set_bitrate(160_000).unwrap(),
            100 => enc.set_mode(BitrateMode::Cbr).unwrap(),
            120 => enc.set_complexity(3).unwrap(),
            _ => {}
        }
        packets.push(enc.encode(frame).unwrap());
    }
    // Bandwidth was re-derived at the bitrate change (64k stereo -> fullband already, but the
    // CBR size must follow the new rate).
    assert!(packets[110..].iter().all(|p| p.len() == 400));
    let decoded = decode_all(&packets, 2);
    let snr = snr_db(&pcm, &decoded, 2, PRE_SKIP as usize, 4 * FRAME);
    assert!(snr > 15.0, "{snr}");
}

#[test]
fn explicit_bandwidths_limit_the_spectrum() {
    // A 15 kHz tone must disappear when only narrowband (4 kHz) is coded, and survive in
    // fullband.
    let tone: Vec<f32> = (0..48000)
        .map(|i| 0.5 * (2.0 * std::f32::consts::PI * 15000.0 * i as f32 / 48000.0).sin())
        .collect();
    let energy = |bw: Bandwidth| {
        let mut cfg = EncoderConfig::new(1, 96_000);
        cfg.bandwidth = Some(bw);
        let packets = encode_all(cfg, &tone);
        for p in &packets {
            assert_eq!(Toc::new(p[0]).bandwidth(), bw);
        }
        let decoded = decode_all(&packets, 1);
        decoded[24000..36000].iter().map(|v| (v * v) as f64).sum::<f64>() / 12000.0
    };
    let full = energy(Bandwidth::Fullband);
    let nb = energy(Bandwidth::Narrowband);
    let wb = energy(Bandwidth::Wideband);
    assert!(full > 0.05, "fullband energy {full}");
    assert!(nb < full * 1e-3 && wb < full * 1e-3, "nb {nb} wb {wb} full {full}");
}

// -- Ogg ---------------------------------------------------------------------------------------

#[test]
fn ogg_stream_is_well_formed_and_trimmed() {
    for frames in [48000usize, 48000 + 123, 1, 960, 959, 961, 96000 - 120] {
        let pcm = music(frames, 2, 31);
        let mut w = OggOpusWriter::new(
            Vec::new(),
            0xC0FFEE,
            EncoderConfig::new(2, 96_000),
            44100,
            &[("TITLE", "round trip"), ("ARTIST", "symphonia")],
        )
        .unwrap();
        // Feed in odd-sized chunks to exercise the streaming buffer.
        for chunk in pcm.chunks(2 * 777) {
            w.write_samples(chunk).unwrap();
        }
        let bytes = w.finish().unwrap();

        // Independent page parse.
        let mut pages = Vec::new();
        let mut rest = &bytes[..];
        while !rest.is_empty() {
            assert_eq!(&rest[..4], b"OggS");
            let nseg = rest[26] as usize;
            let body: usize = rest[27..27 + nseg].iter().map(|&l| l as usize).sum();
            let total = 27 + nseg + body;
            let granule = i64::from_le_bytes(rest[6..14].try_into().unwrap());
            pages.push((
                rest[5],
                granule,
                rest[27..27 + nseg].to_vec(),
                rest[27 + nseg..total].to_vec(),
            ));
            rest = &rest[total..];
        }
        assert_eq!(pages[0].0, 0x02, "BOS on the first page");
        assert_eq!(&pages[0].3[..8], b"OpusHead");
        assert_eq!(pages[0].1, 0);
        let head = OpusHead::parse(&pages[0].3).unwrap();
        assert_eq!(head.pre_skip, PRE_SKIP);
        assert_eq!(head.input_sample_rate, 44100);
        assert_eq!(&pages[1].3[..8], b"OpusTags");
        assert_eq!(pages[1].1, 0);
        let last = pages.last().unwrap();
        assert_eq!(last.0 & 0x04, 0x04, "EOS on the last page");
        assert_eq!(
            last.1,
            PRE_SKIP as i64 + frames as i64,
            "final granule position trims the tail"
        );
        let mut prev = 0;
        for p in &pages[2..] {
            if p.1 >= 0 {
                assert!(p.1 >= prev);
                prev = p.1;
            }
        }

        // Read back through the demuxer and decode.
        let mss = MediaSourceStream::new(
            Box::new(Cursor::new(bytes.clone())),
            MediaSourceStreamOptions::default(),
        );
        let mut reader = OggReader::try_new(mss, FormatOptions::default()).unwrap();
        assert!(reader.first_track(TrackType::Audio).is_some());
        let mut packets = Vec::new();
        while let Some(p) = reader.next_packet().unwrap() {
            packets.push(p.data.to_vec());
        }
        let decoded = decode_all(&packets, 2);
        let granule_total = last.1 as usize;
        assert!(decoded.len() / 2 >= granule_total, "decoder output covers the final granule");
        let out = &decoded[PRE_SKIP as usize * 2..granule_total * 2];
        assert_eq!(out.len() / 2, frames, "exactly the written number of samples after trimming");
        if frames > 20000 {
            let snr = snr_db(&pcm, &decoded, 2, PRE_SKIP as usize, 4 * FRAME);
            assert!(snr > 15.0, "ogg round trip SNR {snr:.1}");
        }
    }
}

// -- real audio --------------------------------------------------------------------------------

/// Minimal Ogg Opus reader for the test fixtures: returns (channels, audio packets).
fn read_ogg_opus(path: &std::path::Path) -> Option<(u8, Vec<Vec<u8>>)> {
    let data = std::fs::read(path).ok()?;
    let mut packets: Vec<Vec<u8>> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut pos = 0usize;
    while pos + 27 <= data.len() && &data[pos..pos + 4] == b"OggS" {
        let nseg = data[pos + 26] as usize;
        let lacing = &data[pos + 27..pos + 27 + nseg];
        let mut off = pos + 27 + nseg;
        for &l in lacing {
            cur.extend_from_slice(&data[off..off + l as usize]);
            off += l as usize;
            if l < 255 {
                packets.push(std::mem::take(&mut cur));
            }
        }
        pos = off;
    }
    let head = OpusHead::parse(packets.first()?).ok()?;
    Some((head.channel_count, packets.split_off(2)))
}

fn fixture_dir() -> PathBuf {
    std::env::var_os("OPUS_ENCODER_FIXTURE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/home/gianluca/Scaricati/nicotine"))
}

#[test]
fn real_music_round_trip() {
    let dir = fixture_dir();
    let Ok(entries) = std::fs::read_dir(&dir)
    else {
        eprintln!("skipping: fixture directory {dir:?} not found");
        return;
    };
    let album = entries
        .flatten()
        .map(|e| e.path())
        .find(|p| p.file_name().map_or(false, |n| n.to_string_lossy().starts_with("1968")));
    let Some(album) = album
    else {
        eprintln!("skipping: no 1968* album in {dir:?}");
        return;
    };
    let mut tracks: Vec<PathBuf> = std::fs::read_dir(&album)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |e| e == "opus"))
        .collect();
    tracks.sort();
    assert!(!tracks.is_empty());
    for track in tracks {
        let (channels, packets) = read_ogg_opus(&track).expect("fixture parses");
        // Decode ~12 s from the middle of the track as the reference signal.
        let mid = packets.len() / 2;
        let slice = &packets[mid.saturating_sub(100)..(mid + 500).min(packets.len())];
        let mut dec = OpusDecoder::try_new(SampleRate::Hz48000, 2).unwrap();
        let mut reference = Vec::new();
        let mut buf = vec![0.0f32; 5760 * 2];
        // Run the decoder through the 80 ms pre-roll but only keep what follows.
        let mut kept_from = 0;
        for (i, p) in slice.iter().enumerate() {
            let n = dec.decode(Some(p), &mut buf, 5760).unwrap();
            if i >= 8 {
                if kept_from == 0 {
                    kept_from = reference.len();
                }
                // Mono fixtures are widened to stereo for a uniform test.
                if channels == 2 {
                    reference.extend_from_slice(&buf[..n * 2]);
                }
                else {
                    for k in 0..n {
                        reference.push(buf[k * 2]);
                        reference.push(buf[k * 2 + 1]);
                    }
                }
            }
        }
        let frames = reference.len() / 2;
        assert!(frames > 48000 * 8, "fixture too short: {frames} frames");
        let mut last = f64::MIN;
        for (br, min_snr) in [(64_000u32, 8.0), (128_000, 12.0), (192_000, 16.0)] {
            let mut cfg = EncoderConfig::new(2, br);
            cfg.mode = BitrateMode::ConstrainedVbr;
            let enc_packets = encode_all(cfg, &reference);
            let decoded = decode_all(&enc_packets, 2);
            let snr = snr_db(&reference, &decoded, 2, PRE_SKIP as usize, 4 * FRAME);
            let mean = mean_packet_size(&enc_packets);
            eprintln!(
                "{}: {br} bps -> mean {:.0} bps, SNR {snr:.1} dB",
                track.file_name().unwrap().to_string_lossy(),
                mean * 400.0
            );
            assert!(snr >= min_snr, "{track:?} @ {br}: SNR {snr:.1} < {min_snr}");
            assert!(snr > last, "SNR must grow with bitrate");
            assert!(
                (mean * 400.0 - br as f64).abs() < br as f64 * 0.08,
                "rate {:.0} vs {br}",
                mean * 400.0
            );
            last = snr;
        }
    }
}
