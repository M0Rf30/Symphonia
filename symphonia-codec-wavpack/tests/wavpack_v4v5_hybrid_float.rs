// Symphonia
// Copyright (c) 2019-2024 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Integration tests for WavPack v4/v5 IEEE-float and hybrid (lossy) decoding.
//!
//! Fixtures were generated with the reference `wavpack`/`wvunpack` (dbry/WavPack 5.9.0,
//! BSD-3-Clause) CLI tools from short (50ms) white/pink noise WAVs, e.g.:
//!   `wavpack -b3 int16_stereo.wav -o int_hybrid_lossy.wv`      (hybrid, lossy, no .wvc)
//!   `wavpack -b4 float32_stereo.wav -o float_hybrid_lossy.wv`  (hybrid, lossy, no .wvc)
//!   `wavpack float32_stereo.wav -o float_lossless.wv`          (lossless, exercises the
//!                                                                ID_WVX_BITSTREAM path)
//!   `wavpack float32_mono.wav -o float_mono_lossless.wv`
//! The `*_ref.raw` files are `wvunpack -r -o *_ref.raw *.wv` (raw little-endian PCM/float,
//! no header) and serve as the bit-exactness oracle: `wvunpack`'s hybrid-lossy decode is
//! deterministic integer math with no correction file involved, so the same bytes are
//! expected from this decoder.

use std::fs::File;

use symphonia_codec_wavpack::{WavPackDecoder, WavPackReader};
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;

/// Decode an entire `.wv` file to raw interleaved little-endian bytes matching
/// `wvunpack -r`'s output layout for the same file.
fn decode_to_raw(path: &str) -> Vec<u8> {
    let file = File::open(path).unwrap_or_else(|e| panic!("open {path}: {e}"));
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut reader = WavPackReader::try_new(mss, FormatOptions::default()).expect("reader");

    let codec_params: AudioCodecParameters = match &reader.tracks()[0].codec_params {
        Some(CodecParameters::Audio(a)) => a.clone(),
        _ => panic!("no audio codec params"),
    };
    let mut decoder =
        WavPackDecoder::try_new(&codec_params, &AudioDecoderOptions::default()).expect("decoder");

    let mut out = Vec::new();
    while let Some(packet) = reader.next_packet().expect("next_packet") {
        let buf = decoder.decode_ref(&packet.as_packet_ref()).expect("decode");
        append_buf(&mut out, &buf);
    }
    out
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
        GenericAudioBufferRef::S32(b) => {
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

fn check_bit_exact(wv: &str, raw_ref: &str) {
    let mine = decode_to_raw(&fixture(wv));
    let reference = std::fs::read(fixture(raw_ref)).expect("read reference raw");
    assert_eq!(mine.len(), reference.len(), "{wv}: decoded length mismatch");
    assert_eq!(mine, reference, "{wv}: decoded bytes differ from wvunpack oracle");
}

#[test]
fn float_lossless_stereo_bit_exact_vs_wvunpack() {
    // Exercises the ID_WVX_BITSTREAM ("extension") path: the noise source has full-width
    // float mantissas, so `wvx_len` in the block is non-zero (verified during development
    // with WAVPACK_TRACE=1).
    check_bit_exact("float_lossless.wv", "float_lossless_ref.raw");
}

#[test]
fn float_lossless_mono_bit_exact_vs_wvunpack() {
    check_bit_exact("float_mono_lossless.wv", "float_mono_lossless_ref.raw");
}

#[test]
fn int_hybrid_lossy_bit_exact_vs_wvunpack() {
    // `-b3`: genuinely lossy (wavpack reports "(lossy)" for this fixture), no .wvc file.
    // WavPack's hybrid-lossy decode is deterministic integer math, so this is still
    // expected to match `wvunpack` exactly even without the correction stream.
    check_bit_exact("int_hybrid_lossy.wv", "int_hybrid_lossy_ref.raw");
}

#[test]
fn float_hybrid_lossy_bit_exact_vs_wvunpack() {
    check_bit_exact("float_hybrid_lossy.wv", "float_hybrid_lossy_ref.raw");
}
