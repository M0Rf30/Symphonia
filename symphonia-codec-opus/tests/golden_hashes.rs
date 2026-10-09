// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Golden hashes of full decodes and of encoder packets, pinned before the hot-path performance
//! work so that any later optimisation can be proven bit-identical.
//!
//! The encoder test uses synthetic signals and always runs. The decoder test needs the sample
//! set: `RMPD_SAMPLES=/path/to/samples cargo test -p symphonia-codec-opus --features ogg
//! --release --test golden_hashes` (it is skipped when the variable is unset). The same hashes
//! are printed by `cargo run -p symphonia-codec-opus --features ogg --release --example perf`.

use std::io::Cursor;
use std::path::PathBuf;

use symphonia_codec_opus::decoder::{OpusDecoder, SampleRate};
use symphonia_codec_opus::encoder::{BitrateMode, EncoderConfig, OpusEncoder};
use symphonia_core::codecs::CodecParameters;
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
    fn word(&mut self, w: u32) {
        self.0 = (self.0 ^ w as u64).wrapping_mul(0x100000001b3).rotate_left(23);
    }
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
        let tau = 2.0 * std::f64::consts::PI;
        let s = (0.3
            * env
            * ((tau * 440.0 * t).sin()
                + 0.5 * (tau * 3300.0 * t).sin()
                + 0.25 * (tau * 9100.0 * t).sin())) as f32
            + 0.05 * rng.next();
        out.push(s);
        if ch == 2 {
            out.push(0.8 * s + 0.05 * rng.next());
        }
    }
    out
}

fn encode_hash(cfg: EncoderConfig, pcm: &[f32]) -> u64 {
    let mut enc = OpusEncoder::new(cfg).unwrap();
    let mut packets = enc.push(pcm);
    packets.extend(enc.finish());
    let mut h = Fnv::new();
    for p in &packets {
        h.bytes(&(p.len() as u32).to_le_bytes());
        h.bytes(p);
    }
    h.0
}

/// `(channels, bitrate, mode, complexity, golden hash)`.
const ENCODER_GOLDEN: [(u8, u32, BitrateMode, u8, u64); 10] = [
    (1, 12_000, BitrateMode::Vbr, 9, 0xf2b95aa2cae15198),
    (1, 32_000, BitrateMode::Cbr, 10, 0x88fa8e93ba510905),
    (1, 48_000, BitrateMode::ConstrainedVbr, 5, 0x35bd7037b48f5d52),
    (1, 64_000, BitrateMode::Vbr, 9, 0xdca68e5d7a76ad43),
    (1, 96_000, BitrateMode::Vbr, 0, 0xe13aeb37964fa537),
    (2, 24_000, BitrateMode::Vbr, 9, 0x573c6f3e057e1381),
    (2, 64_000, BitrateMode::Cbr, 10, 0x272382bca12518f1),
    (2, 96_000, BitrateMode::ConstrainedVbr, 5, 0x2775c2b46df1f057),
    (2, 128_000, BitrateMode::Vbr, 9, 0x817ea1bcb35ce45d),
    (2, 192_000, BitrateMode::Vbr, 0, 0x90ab60b064ac44ad),
];

#[test]
fn encoder_packets_match_golden_hashes() {
    for (ch, bitrate, mode, complexity, golden) in ENCODER_GOLDEN {
        let pcm = synth(48000 * 6, ch as usize, 7 + ch as u32);
        let mut cfg = EncoderConfig::new(ch, bitrate);
        cfg.mode = mode;
        cfg.complexity = complexity;
        assert_eq!(
            encode_hash(cfg, &pcm),
            golden,
            "encoder output changed: {ch}ch {bitrate} bps {mode:?} complexity {complexity}"
        );
    }
}

/// Reads `(channels, audio packets)` of a mono/stereo, mapping family 0 Ogg Opus file.
fn read_packets(path: &std::path::Path) -> Option<(u8, Vec<Vec<u8>>)> {
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
    Some((extra[9], out))
}

fn decode_hash(ch: u8, packets: &[Vec<u8>], loss: bool) -> u64 {
    let mut dec = OpusDecoder::try_new(SampleRate::Hz48000, ch).unwrap();
    let mut buf = vec![0.0f32; 5760 * ch as usize];
    let mut h = Fnv::new();
    for (idx, p) in packets.iter().enumerate() {
        // With `loss`, every 13th packet (from the 6th) is lost, exercising PLC.
        let pk = if loss && idx % 13 == 5 { None } else { Some(p.as_slice()) };
        match dec.decode(pk, &mut buf, if pk.is_none() { 960 } else { 5760 }) {
            Ok(n) => {
                for v in &buf[..n * ch as usize] {
                    h.word(v.to_bits());
                }
            }
            Err(_) => h.bytes(b"err"),
        }
    }
    h.0
}

/// `(file, decode hash, decode-with-loss hash)`; the latter is `None` where not pinned.
const DECODER_GOLDEN: [(&str, u64, Option<u64>); 10] = [
    ("opus_cbr_96.opus", 0xacc86b5318996c09, None),
    ("opus_ffm_celt_fb.opus", 0x64966c2807266272, Some(0xcbd50a701ef3a2d1)),
    ("opus_ffm_fd120.opus", 0x95b12e412a40741f, None),
    ("opus_ffm_fd2.5.opus", 0x5b160a8edcb5bd27, None),
    ("opus_ffm_hybrid_st.opus", 0x961f3b202479473e, Some(0xd558f4472b7f6c50)),
    ("opus_ffm_silk_wb_st.opus", 0x5f1f9687d725a51f, Some(0xfe41bc7022bffee0)),
    ("opus_low_24.opus", 0x32a5010d21f3ba5a, None),
    ("opus_mono.opus", 0x74ac3c7bdde7eba1, Some(0x762e32be55ccb004)),
    ("opus_music_256.opus", 0xee66590788ec4e6b, Some(0x3b54d3203d79cfd7)),
    ("opus_speech_mono_8.opus", 0xdbbd62ee791d4f9a, None),
];

#[test]
fn decoder_output_matches_golden_hashes() {
    let Some(root) = std::env::var_os("RMPD_SAMPLES")
    else {
        eprintln!("RMPD_SAMPLES not set; skipping");
        return;
    };
    let dir = PathBuf::from(root).join("opus");
    for (name, golden, golden_loss) in DECODER_GOLDEN {
        let path = dir.join(name);
        let Some((ch, packets)) = read_packets(&path)
        else {
            eprintln!("{name}: not available; skipping");
            continue;
        };
        assert_eq!(decode_hash(ch, &packets, false), golden, "{name}: decoder output changed");
        if let Some(golden_loss) = golden_loss {
            assert_eq!(
                decode_hash(ch, &packets, true),
                golden_loss,
                "{name}: decoder output with packet loss changed"
            );
        }
    }
}
