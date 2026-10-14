// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Decodes IMC (WAVE_FORMAT 0x0401) and IAC (0x0402) streams and compares them with the output of
//! the FFmpeg `imc` / `iac` decoders (`ffmpeg -i x.wav -f s16le`, stored as `*.ref.s16`).
//!
//! The fixtures are the first seconds of samples from <https://samples.ffmpeg.org/A-codecs/IMC/>
//! (`imc-8000-2bit-mono.wav`, `imc-48000-2bit-mono.wav`, `STsKlassFist-1a.avi`) and
//! <https://samples.ffmpeg.org/A-codecs/Indeo_audio/> (`radar.avi`, `hvalen.avi`), remuxed to WAV
//! with `ffmpeg -c copy`. The decoder is float, so the comparison is by SNR and maximum error in
//! 16-bit LSBs rather than bit-exact.

use std::path::Path;

use symphonia_codec_imc::ImcDecoder;
use symphonia_core::audio::layouts::{CHANNEL_LAYOUT_MONO, CHANNEL_LAYOUT_STEREO};
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::well_known::{CODEC_ID_IAC, CODEC_ID_IMC};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;

struct Wav {
    tag: u16,
    channels: u16,
    rate: u32,
    block_align: usize,
    data: Vec<u8>,
}

fn parse_wav(bytes: &[u8]) -> Wav {
    assert_eq!(&bytes[..4], b"RIFF");
    let mut pos = 12;
    let (mut tag, mut channels, mut rate, mut block_align) = (0, 0, 0, 0);
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = &bytes[pos + 8..(pos + 8 + len).min(bytes.len())];
        if id == b"fmt " {
            tag = u16::from_le_bytes(body[0..2].try_into().unwrap());
            channels = u16::from_le_bytes(body[2..4].try_into().unwrap());
            rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            block_align = usize::from(u16::from_le_bytes(body[12..14].try_into().unwrap()));
        }
        else if id == b"data" {
            return Wav { tag, channels, rate, block_align, data: body.to_vec() };
        }
        pos += 8 + len + (len & 1);
    }
    panic!("no data chunk");
}

fn make_decoder(wav: &Wav) -> ImcDecoder {
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(if wav.tag == 0x402 { CODEC_ID_IAC } else { CODEC_ID_IMC })
        .with_sample_rate(wav.rate)
        .with_channels(if wav.channels == 2 { CHANNEL_LAYOUT_STEREO } else { CHANNEL_LAYOUT_MONO });
    ImcDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap()
}

/// Decodes a fixture. Returns interleaved f32 samples.
fn decode(name: &str) -> (Vec<f32>, Wav) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    let wav = parse_wav(&std::fs::read(path).unwrap());
    let mut dec = make_decoder(&wav);
    let mut out = Vec::new();
    for chunk in wav.data.chunks(wav.block_align) {
        let pkt = PacketRef::new(0, Default::default(), Default::default(), chunk);
        let GenericAudioBufferRef::F32(buf) = dec.decode_ref(&pkt).unwrap()
        else {
            panic!("expected f32 output");
        };
        let planes: Vec<&[f32]> =
            (0..buf.spec().channels().count()).map(|c| buf.plane(c).unwrap()).collect();
        for i in 0..buf.frames() {
            for p in &planes {
                out.push(p[i]);
            }
        }
    }
    (out, wav)
}

fn compare(name: &str, min_snr_db: f64, max_lsb: f64) {
    let (out, wav) = decode(name);
    let ref_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name.replace(".wav", ".ref.s16"));
    let reference: Vec<i16> = std::fs::read(ref_path)
        .unwrap()
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();

    assert_eq!(out.len(), reference.len(), "{name}: sample count");
    assert_eq!(usize::from(wav.channels) * 256 * (wav.data.len() / wav.block_align), out.len());

    let (mut sig, mut err, mut max, mut mismatches) = (0f64, 0f64, 0f64, 0usize);
    for (&a, &r) in out.iter().zip(reference.iter()) {
        // FFmpeg converts float to s16 by scaling with 32768, rounding and clipping.
        let a = (f64::from(a) * 32768.0).round().clamp(-32768.0, 32767.0);
        let r = f64::from(r);
        sig += r * r;
        err += (a - r).powi(2);
        max = max.max((a - r).abs());
        mismatches += usize::from(a != r);
    }
    let snr = 10.0 * (sig / err.max(1e-9)).log10();
    println!(
        "{name}: {} samples, SNR {snr:.1} dB, max diff {max} LSB, {mismatches} differ",
        out.len()
    );
    assert!(snr >= min_snr_db, "{name}: SNR {snr:.1} dB");
    assert!(max <= max_lsb, "{name}: max diff {max:.3} LSB");
}

#[test]
fn imc_8000_mono() {
    compare("imc_8000_mono.wav", 100.0, 1.0);
}

#[test]
fn imc_11025_mono() {
    compare("imc_11025_mono.wav", 100.0, 1.0);
}

#[test]
fn imc_48000_mono() {
    compare("imc_48000_mono.wav", 100.0, 1.0);
}

#[test]
fn iac_22050_stereo() {
    compare("iac_22050_stereo.wav", 100.0, 1.0);
}

#[test]
fn iac_22050_stereo_b() {
    compare("iac_22050_stereo_b.wav", 100.0, 1.0);
}

/// Truncated and corrupted blocks never panic; the decoder recovers afterwards.
#[test]
fn malformed_packets() {
    let (_, wav) = decode("iac_22050_stereo.wav");
    let mut dec = make_decoder(&wav);
    let good = &wav.data[..wav.block_align];

    // Truncated.
    for len in [0, 1, 63, 64, 127] {
        let pkt = PacketRef::new(0, Default::default(), Default::default(), &good[..len]);
        assert!(dec.decode_ref(&pkt).is_err());
    }

    // Garbage and bit flips.
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

    // After an error the next good packet decodes.
    let pkt = PacketRef::new(0, Default::default(), Default::default(), good);
    assert!(dec.decode_ref(&pkt).is_ok());
}

#[test]
fn rejects_bad_params() {
    let mut p = AudioCodecParameters::new();
    p.for_codec(CODEC_ID_IAC).with_sample_rate(200_000).with_channels(CHANNEL_LAYOUT_STEREO);
    assert!(ImcDecoder::try_new(&p, &AudioDecoderOptions::default()).is_err());

    let mut p = AudioCodecParameters::new();
    p.for_codec(CODEC_ID_IMC);
    assert!(ImcDecoder::try_new(&p, &AudioDecoderOptions::default()).is_err());
}
