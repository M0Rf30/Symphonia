// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Decodes WMA v1 (0x0160) and v2 (0x0161) streams and compares them with the output of the
//! FFmpeg `wmav1` / `wmav2` decoders (`ffmpeg -i x.wav -f s16le`, stored as `*.ref.s16`).
//!
//! The fixtures were encoded with `ffmpeg -f lavfi -i aevalsrc=... -c:a wmav1|wmav2` (3 seconds of
//! swept tones and noise) into WAV. The decoder is float, so the comparison is by SNR and maximum
//! error in 16-bit LSBs rather than bit-exact.

use std::path::Path;

use symphonia_codec_wma::WmaDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::well_known::{CODEC_ID_WMAV1, CODEC_ID_WMAV2};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;

struct Wav {
    tag: u16,
    channels: usize,
    block_align: usize,
    /// The complete `WAVEFORMATEX`.
    format: Vec<u8>,
    data: Vec<u8>,
}

fn parse_wav(bytes: &[u8]) -> Wav {
    assert_eq!(&bytes[..4], b"RIFF");
    let mut pos = 12;
    let (mut tag, mut channels, mut block_align, mut format) = (0, 0, 0, Vec::new());
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = &bytes[pos + 8..(pos + 8 + len).min(bytes.len())];
        if id == b"fmt " {
            tag = u16::from_le_bytes(body[0..2].try_into().unwrap());
            channels = usize::from(u16::from_le_bytes(body[2..4].try_into().unwrap()));
            block_align = usize::from(u16::from_le_bytes(body[12..14].try_into().unwrap()));
            format = body.to_vec();
        }
        else if id == b"data" {
            return Wav { tag, channels, block_align, format, data: body.to_vec() };
        }
        pos += 8 + len + (len & 1);
    }
    panic!("no data chunk");
}

fn make_decoder(wav: &Wav, gapless: bool) -> WmaDecoder {
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(if wav.tag == 0x160 { CODEC_ID_WMAV1 } else { CODEC_ID_WMAV2 })
        .with_extra_data(wav.format.clone().into_boxed_slice());
    WmaDecoder::try_new(&params, &AudioDecoderOptions::default().gapless(gapless)).unwrap()
}

/// Decodes a fixture. Returns interleaved f32 samples.
fn decode(name: &str, gapless: bool) -> (Vec<f32>, Wav) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    let wav = parse_wav(&std::fs::read(path).unwrap());
    let mut dec = make_decoder(&wav, gapless);
    let mut out = Vec::new();
    for chunk in wav.data.chunks_exact(wav.block_align) {
        let pkt = PacketRef::new(0, Default::default(), Default::default(), chunk);
        let GenericAudioBufferRef::F32(buf) = dec.decode_ref(&pkt).unwrap()
        else {
            panic!("expected f32 output");
        };
        let planes: Vec<&[f32]> = (0..wav.channels).map(|c| buf.plane(c).unwrap()).collect();
        for i in 0..buf.frames() {
            for p in &planes {
                out.push(p[i]);
            }
        }
    }
    (out, wav)
}

fn compare(name: &str, frame_len: usize, min_snr_db: f64, max_lsb: f64) {
    let (out, wav) = decode(name, false);
    let ref_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name.replace(".wav", ".ref.s16"));
    let reference: Vec<i16> = std::fs::read(ref_path)
        .unwrap()
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();

    // FFmpeg drops the two frames of delay, and also outputs the last frame that is left in the
    // overlap buffer, which this decoder does not.
    let skip = 2 * frame_len * wav.channels;
    let out = &out[skip..];
    assert_eq!(out.len() + frame_len * wav.channels, reference.len(), "{name}: sample count");

    let (mut sig, mut err, mut max) = (0f64, 0f64, 0f64);
    for (&a, &r) in out.iter().zip(reference.iter()) {
        // FFmpeg converts float to s16 by scaling with 32768, rounding and clipping.
        let a = (f64::from(a) * 32768.0).round().clamp(-32768.0, 32767.0);
        let r = f64::from(r);
        sig += r * r;
        err += (a - r).powi(2);
        max = max.max((a - r).abs());
    }
    let snr = 10.0 * (sig / err.max(1e-9)).log10();
    println!("{name}: {} samples, SNR {snr:.1} dB, max diff {max} LSB", out.len());
    assert!(snr >= min_snr_db, "{name}: SNR {snr:.1} dB");
    assert!(max <= max_lsb, "{name}: max diff {max:.3} LSB");
}

#[test]
fn wmav1_22050_mono() {
    compare("wmav1_22050_mono.wav", 1024, 100.0, 1.0);
}

#[test]
fn wmav1_44100_stereo() {
    compare("wmav1_44100_stereo.wav", 2048, 100.0, 1.0);
}

#[test]
fn wmav2_44100_stereo() {
    compare("wmav2_44100_stereo.wav", 2048, 100.0, 1.0);
}

#[test]
fn wmav2_16000_mono() {
    compare("wmav2_16000_mono.wav", 512, 100.0, 1.0);
}

#[test]
fn wmav2_48000_stereo() {
    compare("wmav2_48000_stereo.wav", 2048, 100.0, 1.0);
}

#[test]
fn wmav2_22050_stereo() {
    compare("wmav2_22050_stereo.wav", 1024, 100.0, 1.0);
}

/// Gapless mode discards the two frames of delay.
#[test]
fn gapless_skips_delay() {
    let (full, _) = decode("wmav2_44100_stereo.wav", false);
    let (trimmed, _) = decode("wmav2_44100_stereo.wav", true);
    assert_eq!(full.len() - trimmed.len(), 2 * 2048 * 2);
    assert_eq!(&full[2 * 2048 * 2..], &trimmed[..]);
}

/// Truncated and corrupted blocks never panic; the decoder recovers afterwards.
#[test]
fn malformed_packets() {
    let (_, wav) = decode("wmav2_44100_stereo.wav", false);
    let mut dec = make_decoder(&wav, false);
    let good = &wav.data[..wav.block_align];

    for len in [0, 1, wav.block_align - 1] {
        let pkt = PacketRef::new(0, Default::default(), Default::default(), &good[..len]);
        assert!(dec.decode_ref(&pkt).is_err());
    }

    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    for _ in 0..2000 {
        let mut data = good.to_vec();
        for _ in 0..8 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let idx = (state >> 33) as usize % data.len();
            data[idx] ^= 1 << ((state >> 20) & 7);
        }
        let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);
        let _ = dec.decode_ref(&pkt);
    }
}

#[test]
fn rejects_bad_params() {
    let mut p = AudioCodecParameters::new();
    p.for_codec(CODEC_ID_WMAV2);
    assert!(WmaDecoder::try_new(&p, &AudioDecoderOptions::default()).is_err());

    p.with_extra_data(vec![0u8; 18].into_boxed_slice());
    assert!(WmaDecoder::try_new(&p, &AudioDecoderOptions::default()).is_err());
}
