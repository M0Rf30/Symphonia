// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Timing and bit-exactness harness for the Opus decoder and encoder.
//!
//! Decodes every mono/stereo Ogg Opus file in a directory and encodes synthetic and real
//! signals with several configurations, printing an FNV-1a hash of all outputs together with the
//! elapsed time of each stage, so an optimisation can be proven output-identical.
//!
//! `cargo run -p symphonia-codec-opus --features ogg --release --example perf -- [dir] [reps]`

use std::io::Cursor;
use std::time::Instant;

use symphonia_codec_opus::decoder::{OpusDecoder, SampleRate};
use symphonia_codec_opus::encoder::{BitrateMode, EncoderConfig, OpusEncoder};
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia_format_ogg::OggReader;

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf29ce484222325)
    }
    fn bytes(&mut self, b: &[u8]) {
        for &x in b {
            self.0 = (self.0 ^ x as u64).wrapping_mul(0x100000001b3);
        }
    }
    /// Cheaper word-at-a-time variant for bulk PCM, so hashing does not dominate the timing.
    fn word(&mut self, w: u32) {
        self.0 = (self.0 ^ w as u64).wrapping_mul(0x100000001b3).rotate_left(23);
    }
}

/// Returns the channel count (as a one-byte first "packet") followed by the audio packets, for
/// mono/stereo mapping-family-0 streams only.
fn read_packets(path: &std::path::Path) -> Option<Vec<Vec<u8>>> {
    let data = std::fs::read(path).ok()?;
    let mss =
        MediaSourceStream::new(Box::new(Cursor::new(data)), MediaSourceStreamOptions::default());
    let mut reader = OggReader::try_new(mss, FormatOptions::default()).ok()?;
    let mut out = Vec::new();
    let track = reader.tracks().first()?;
    let extra = match track.codec_params.as_ref()? {
        symphonia_core::codecs::CodecParameters::Audio(a) => a.extra_data.as_ref()?.to_vec(),
        _ => return None,
    };
    if extra.len() < 19
        || &extra[..8] != b"OpusHead"
        || extra[18] != 0
        || !(1..=2).contains(&extra[9])
    {
        return None;
    }
    out.push(vec![extra[9]]);
    while let Ok(Some(p)) = reader.next_packet() {
        out.push(p.data.to_vec());
    }
    Some(out)
}

/// Decodes a file; returns (hash, pcm samples, interleaved pcm).
fn decode_file(packets: &[Vec<u8>], keep: bool, loss: bool) -> Option<(u64, usize, Vec<f32>, u8)> {
    // The Ogg demuxer consumes the identification and comment headers; recover the channel
    // count from the track parameters instead.
    let (ch, packets) = (packets.first()?.first().copied()?, &packets[1..]);
    if ch != 1 && ch != 2 {
        return None;
    }
    let mut dec = OpusDecoder::try_new(SampleRate::Hz48000, ch).ok()?;
    let mut buf = vec![0.0f32; 5760 * ch as usize];
    let mut h = Fnv::new();
    let mut total = 0;
    let mut pcm = Vec::new();
    for (idx, p) in packets.iter().enumerate() {
        // With `loss`, every 13th packet (from the 6th) is treated as lost, exercising PLC.
        let pk = if loss && idx % 13 == 5 { None } else { Some(p.as_slice()) };
        match dec.decode(pk, &mut buf, if pk.is_none() { 960 } else { 5760 }) {
            Ok(n) => {
                let s = &buf[..n * ch as usize];
                for v in s {
                    h.word(v.to_bits());
                }
                total += n;
                if keep {
                    pcm.extend_from_slice(s);
                }
            }
            Err(_) => h.bytes(b"err"),
        }
    }
    Some((h.0, total, pcm, ch))
}

struct Lcg(u32);
impl Lcg {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1664525).wrapping_add(1013904223);
        ((self.0 >> 8) as f32 / (1u32 << 23) as f32) - 1.0
    }
}

fn synth(frames: usize, ch: usize, seed: u32) -> Vec<f32> {
    let mut rng = Lcg(seed);
    let mut out = Vec::with_capacity(frames * ch);
    for i in 0..frames {
        let t = i as f64 / 48000.0;
        let env = 0.5 + 0.5 * (t * 3.0).sin();
        let s = (0.3
            * env
            * ((2.0 * std::f64::consts::PI * 440.0 * t).sin()
                + 0.5 * (2.0 * std::f64::consts::PI * 3300.0 * t).sin()
                + 0.25 * (2.0 * std::f64::consts::PI * 9100.0 * t).sin())) as f32
            + 0.05 * rng.next();
        out.push(s);
        if ch == 2 {
            out.push(0.8 * s + 0.05 * rng.next());
        }
    }
    out
}

fn encode_hash(cfg: EncoderConfig, pcm: &[f32], h: &mut Fnv) -> usize {
    let mut enc = OpusEncoder::new(cfg).unwrap();
    let mut packets = enc.push(pcm);
    packets.extend(enc.finish());
    for p in &packets {
        h.bytes(&(p.len() as u32).to_le_bytes());
        h.bytes(p);
    }
    packets.len()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = args
        .get(1)
        .cloned()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("RMPD_SAMPLES").ok().map(|d| format!("{d}/opus")))
        .unwrap_or_else(|| "/home/gianluca/rmpd-samples/samples/opus".into());
    let reps: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
    // Optional third argument: `dec` or `enc` restricts the run to one direction.
    let mode = args.get(3).map(String::as_str).unwrap_or("both");
    let (do_dec, do_enc) = (mode != "enc", mode != "dec");

    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("sample dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "opus"))
        .collect();
    files.sort();

    // Decoder.
    let mut inputs = Vec::new();
    for f in &files {
        if let Some(pk) = read_packets(f) {
            inputs.push((f.file_name().unwrap().to_string_lossy().into_owned(), pk));
        }
    }
    let mut real: Vec<(Vec<f32>, u8)> = Vec::new();
    let mut total_h = Fnv::new();
    let mut samples = 0;
    let t0 = Instant::now();
    for r in 0..reps {
        for (name, pk) in inputs.iter().filter(|_| do_dec || r == 0) {
            let keep = r == 0
                && real.len() < 3
                && (name.contains("music") || name.contains("speech_mono_16"));
            if let Some((h, n, pcm, ch)) = decode_file(pk, keep, false) {
                if r == 0 {
                    println!("dec {name}: {h:016x} {n}");
                    total_h.bytes(&h.to_le_bytes());
                    samples += n;
                    if keep {
                        real.push((pcm, ch));
                    }
                }
            }
        }
    }
    let dt = t0.elapsed();
    println!(
        "DECODE hash {:016x} samples {samples} time/rep {:.1} ms",
        total_h.0,
        dt.as_secs_f64() * 1e3 / reps as f64
    );

    // Decoder with simulated packet loss (packet-loss concealment path), untimed.
    let mut plc_h = Fnv::new();
    for (name, pk) in &inputs {
        if let Some((h, n, _, _)) = decode_file(pk, false, true) {
            println!("plc {name}: {h:016x} {n}");
            plc_h.bytes(&h.to_le_bytes());
        }
    }
    println!("PLC hash {:016x}", plc_h.0);

    // Encoder.
    let mut signals: Vec<(String, Vec<f32>, u8)> = Vec::new();
    for ch in [1u8, 2] {
        signals.push((format!("synth{ch}"), synth(48000 * 6, ch as usize, 7 + ch as u32), ch));
    }
    for (i, (pcm, ch)) in real.iter().enumerate() {
        let take = (48000 * 8 * *ch as usize).min(pcm.len());
        signals.push((format!("real{i}"), pcm[..take].to_vec(), *ch));
    }
    let cfgs = |ch: u8| {
        let mut v = Vec::new();
        for (br, mode, cx) in [
            (24_000, BitrateMode::Vbr, 9),
            (64_000, BitrateMode::Cbr, 10),
            (96_000, BitrateMode::ConstrainedVbr, 5),
            (128_000, BitrateMode::Vbr, 9),
            (192_000, BitrateMode::Vbr, 0),
        ] {
            let mut c = EncoderConfig::new(ch, br * ch as u32 / 2);
            c.mode = mode;
            c.complexity = cx;
            v.push(c);
        }
        v
    };
    let mut enc_h = Fnv::new();
    let mut npk = 0;
    let mut frames = 0;
    let t1 = Instant::now();
    for r in 0..reps {
        for (name, pcm, ch) in signals.iter().filter(|_| do_enc || r == usize::MAX) {
            for cfg in cfgs(*ch) {
                let mut h = Fnv::new();
                npk += encode_hash(cfg, pcm, &mut h);
                frames += pcm.len() / *ch as usize;
                if r == 0 {
                    println!(
                        "enc {name} {} {:?} cx{}: {:016x}",
                        cfg.bitrate, cfg.mode, cfg.complexity, h.0
                    );
                    enc_h.bytes(&h.0.to_le_bytes());
                }
            }
        }
    }
    let dt = t1.elapsed();
    let _ = (npk, frames);
    println!(
        "ENCODE hash {:016x} time/rep {:.1} ms",
        enc_h.0,
        dt.as_secs_f64() * 1e3 / reps as f64
    );
}
