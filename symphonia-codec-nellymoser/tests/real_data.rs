// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Decodes a stream produced by the FFmpeg `nellymoser` encoder (1 s of a 440 Hz sine plus noise,
//! 22.05 kHz mono, 88 blocks of 64 bytes) and compares it with the output of the FFmpeg decoder
//! (`ffmpeg -i x.flv -f f32le`). `mix_22k.nm` holds the concatenated FLV audio tag payloads
//! (without the FLV sound-format byte).

use symphonia_codec_nellymoser::NellymoserDecoder;
use symphonia_core::codecs::audio::well_known::CODEC_ID_NELLYMOSER;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

const STREAM: &[u8] = include_bytes!("data/mix_22k.nm");
const REFERENCE: &[u8] = include_bytes!("data/mix_22k.ref.f32");

fn decoder() -> NellymoserDecoder {
    let mut p = AudioCodecParameters::new();
    p.for_codec(CODEC_ID_NELLYMOSER).with_sample_rate(22050);
    NellymoserDecoder::try_new(&p, &AudioDecoderOptions::default()).unwrap()
}

fn reference() -> Vec<f32> {
    REFERENCE.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect()
}

/// Decodes the stream in packets of `blocks` blocks.
fn decode(blocks: usize) -> Vec<f32> {
    let mut dec = decoder();
    let mut out = Vec::new();
    let mut tmp = Vec::new();
    for chunk in STREAM.chunks(blocks * 64) {
        let pkt = PacketRef::new(0, Timestamp::new(0), Duration::new(0), chunk);
        dec.decode_ref(&pkt).unwrap().copy_to_vec_interleaved::<f32>(&mut tmp);
        out.extend_from_slice(&tmp);
    }
    out
}

fn assert_matches_reference(out: &[f32]) {
    let reference = reference();
    assert_eq!(out.len(), reference.len());
    let max = out.iter().zip(&reference).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
    // The reference is single-precision with a different IMDCT; require agreement well below
    // one 16-bit LSB (3.05e-5).
    assert!(max < 1.0e-5, "max deviation {max}");
}

#[test]
fn one_block_per_packet_matches_ffmpeg() {
    assert_matches_reference(&decode(1));
}

#[test]
fn packets_beyond_64_blocks_are_decoded_completely() {
    // A single 88-block packet exceeds the former 64-block limit.
    assert_matches_reference(&decode(88));
}
