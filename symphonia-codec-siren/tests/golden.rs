// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Decodes real Siren and MSN Siren streams and compares the PCM with the FFmpeg decoders.

use std::path::PathBuf;

use symphonia_codec_siren::SirenDecoder;
use symphonia_core::audio::GenericAudioBufferRef;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_MSN_SIREN, CODEC_ID_SIREN};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia_core::packet::PacketRef;
use symphonia_format_riff::WavReader;
use symphonia_testutil::open_mss;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn read_s16(name: &str) -> Vec<i16> {
    let data = std::fs::read(fixture(name)).unwrap();
    data.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect()
}

fn append(out: &mut Vec<f32>, buf: GenericAudioBufferRef<'_>) {
    let mut tmp = Vec::new();
    buf.copy_to_vec_interleaved::<f32>(&mut tmp);
    out.extend_from_slice(&tmp);
}

/// Decode `packets` of `size` bytes each.
fn decode_raw(
    codec: symphonia_core::codecs::audio::AudioCodecId,
    rate: u32,
    name: &str,
    size: usize,
) -> Vec<f32> {
    let mut params = AudioCodecParameters::new();
    params.for_codec(codec).with_sample_rate(rate);
    let mut dec = SirenDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

    let data = std::fs::read(fixture(name)).unwrap();
    let mut out = Vec::new();
    for chunk in data.chunks_exact(size) {
        let pkt = PacketRef::new(0, Default::default(), Default::default(), chunk);
        append(&mut out, dec.decode_ref(&pkt).unwrap());
    }
    out
}

/// Returns (SNR in dB, max difference in 16-bit LSBs) of `out` against `reference`.
fn compare(out: &[f32], reference: &[i16]) -> (f64, f64) {
    assert_eq!(out.len(), reference.len());
    let (mut sig, mut err, mut max) = (0.0f64, 0.0f64, 0.0f64);
    for (&o, &r) in out.iter().zip(reference) {
        let o = f64::from(o) * 32768.0;
        let d = o - f64::from(r);
        sig += f64::from(r) * f64::from(r);
        err += d * d;
        max = max.max(d.abs());
    }
    (10.0 * (sig / err.max(1e-9)).log10(), max)
}

#[test]
fn print_numbers() {
    let out = decode_raw(CODEC_ID_SIREN, 16000, "siren7_vivo_2s.bin", 40);
    println!("vivo: {:?}", compare(&out, &read_s16("siren7_vivo_2s.s16")));

    let mut reader =
        WavReader::try_new(open_mss(&fixture("msnsiren_2s.wav")), FormatOptions::default())
            .unwrap();
    let track = reader.default_track(TrackType::Audio).unwrap();
    let Some(CodecParameters::Audio(params)) = &track.codec_params
    else {
        panic!()
    };
    let params = params.clone();
    assert_eq!(params.codec, CODEC_ID_MSN_SIREN);
    let mut dec = SirenDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
    let mut out = Vec::new();
    while let Some(pkt) = reader.next_packet().unwrap() {
        append(&mut out, dec.decode(&pkt).unwrap());
    }
    println!("msn: {} {:?}", out.len(), compare(&out, &read_s16("msnsiren_2s.s16")));
}

#[test]
fn print_ref_numbers() {
    // The reference vectors are not part of the repository.
    let Some(dir) = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/Siren/vec"))
    else {
        return;
    };
    if !dir.is_dir() {
        return;
    }
    for (tag, rate) in [("s7", 16000u32), ("s14", 32000)] {
        for br in [24000usize, 32000, 48000] {
            let size = br / 400;
            let bits = std::fs::read(dir.join(format!("{tag}_{br}.bit"))).unwrap();
            let r: Vec<i16> = std::fs::read(dir.join(format!("{tag}_{br}.s16")))
                .unwrap()
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            let mut params = AudioCodecParameters::new();
            params.for_codec(CODEC_ID_SIREN).with_sample_rate(rate);
            let mut dec = SirenDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
            let mut out = Vec::new();
            for chunk in bits.chunks_exact(size) {
                let pkt = PacketRef::new(0, Default::default(), Default::default(), chunk);
                match dec.decode_ref(&pkt) {
                    Ok(b) => append(&mut out, b),
                    Err(e) => println!("err {e}"),
                }
            }
            println!(
                "{tag} {br}: {} vs {} {:?}",
                out.len(),
                r.len(),
                if out.len() == r.len() { compare(&out, &r) } else { (0.0, 0.0) }
            );
        }
    }
}
