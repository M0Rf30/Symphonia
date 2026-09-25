// Symphonia
// Copyright (c) 2019-2024 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Integration tests for WavPack v4/v5 multichannel (>2 channel) decoding.
//!
//! WavPack stores a multichannel file as several interleaved mono/stereo "streams"
//! sharing a common `block_samples` span (`INITIAL_BLOCK` .. `FINAL_BLOCK`); this reader
//! merges each such "block group" into one packet and the decoder interleaves every
//! stream's samples into the final N-channel output, mirroring WavPack's own
//! `unpack_samples_interleave()`.
//!
//! Fixtures were generated with the reference `wavpack`/`wvunpack` 5.9.0 (dbry/WavPack,
//! BSD-3-Clause) CLI from short (50ms) multichannel white/pink noise WAVs produced by
//! `ffmpeg`'s `anoisesrc` + `amerge` filters, e.g.:
//!   `wavpack noise4ch.wav -o multi4ch_int16_lossless.wv`             (4.0, 2 streams)
//!   `wavpack -b3 noise4ch.wav -o multi4ch_int16_hybrid_lossy.wv`     (4.0, hybrid, no .wvc)
//!   `wavpack noise6ch.wav -o multi6ch_51_int16_lossless.wv`         (5.1, 3 streams)
//!   `wavpack noise3ch.wav -o multi3ch_mixed_int16_lossless.wv`      (2.1: 1 stereo +
//!                                                                     1 mono stream)
//! The `*_ref.raw` files are `wvunpack -r -o *_ref.raw *.wv` (raw little-endian
//! interleaved PCM/float, no header) and serve as the bit-exactness oracle.

use std::fs::File;

use symphonia_codec_wavpack::{WavPackDecoder, WavPackReader};
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;

fn decode_to_raw(path: &str) -> (Vec<u8>, usize) {
    let file = File::open(path).unwrap_or_else(|e| panic!("open {path}: {e}"));
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut reader = WavPackReader::try_new(mss, FormatOptions::default()).expect("reader");

    let codec_params: AudioCodecParameters = match &reader.tracks()[0].codec_params {
        Some(CodecParameters::Audio(a)) => a.clone(),
        _ => panic!("no audio codec params"),
    };
    let num_channels = codec_params.channels.as_ref().map(|c| c.count()).unwrap_or(0);
    let mut decoder =
        WavPackDecoder::try_new(&codec_params, &AudioDecoderOptions::default()).expect("decoder");

    let mut out = Vec::new();
    while let Some(packet) = reader.next_packet().expect("next_packet") {
        let buf = decoder.decode_ref(&packet.as_packet_ref()).expect("decode");
        append_buf(&mut out, &buf);
    }
    (out, num_channels)
}

fn append_buf(out: &mut Vec<u8>, buf: &GenericAudioBufferRef<'_>) {
    let n = buf.frames();
    let nch = buf.spec().channels().count();
    match buf {
        GenericAudioBufferRef::S16(b) => {
            for i in 0..n {
                for ch in 0..nch {
                    out.extend_from_slice(&b.plane(ch).unwrap()[i].to_le_bytes());
                }
            }
        }
        GenericAudioBufferRef::F32(b) => {
            for i in 0..n {
                for ch in 0..nch {
                    out.extend_from_slice(&b.plane(ch).unwrap()[i].to_le_bytes());
                }
            }
        }
        _ => panic!("unhandled sample format in test harness"),
    }
}

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{}", env!("CARGO_MANIFEST_DIR"), name)
}

fn check_bit_exact(wv: &str, raw_ref: &str, expect_channels: usize) {
    let (mine, num_channels) = decode_to_raw(&fixture(wv));
    assert_eq!(num_channels, expect_channels, "{wv}: unexpected channel count");
    let reference = std::fs::read(fixture(raw_ref)).expect("read reference raw");
    assert_eq!(mine.len(), reference.len(), "{wv}: decoded length mismatch");
    assert_eq!(mine, reference, "{wv}: decoded bytes differ from wvunpack oracle");
}

#[test]
fn quad_4ch_int16_lossless_bit_exact_vs_wvunpack() {
    check_bit_exact("multi4ch_int16_lossless.wv", "multi4ch_int16_lossless_ref.raw", 4);
}

#[test]
fn quad_4ch_int16_hybrid_lossy_bit_exact_vs_wvunpack() {
    check_bit_exact("multi4ch_int16_hybrid_lossy.wv", "multi4ch_int16_hybrid_lossy_ref.raw", 4);
}

#[test]
fn quad_4ch_float32_lossless_bit_exact_vs_wvunpack() {
    check_bit_exact("multi4ch_float32_lossless.wv", "multi4ch_float32_lossless_ref.raw", 4);
}

#[test]
fn surround_5p1_int16_lossless_bit_exact_vs_wvunpack() {
    check_bit_exact("multi6ch_51_int16_lossless.wv", "multi6ch_51_int16_lossless_ref.raw", 6);
}

#[test]
fn mixed_mono_stereo_3ch_lossless_bit_exact_vs_wvunpack() {
    // 3 channels = one stereo stream (FL/FR) + one mono stream (LFE), exercising the
    // channel-count-mismatched-stream (2ch + 1ch) interleaving path.
    check_bit_exact("multi3ch_mixed_int16_lossless.wv", "multi3ch_mixed_int16_lossless_ref.raw", 3);
}
