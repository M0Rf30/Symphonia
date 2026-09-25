// Symphonia
// Copyright (c) 2019-2024 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Deterministic mutation fuzz test for the float/hybrid v4/v5 decode path.
//!
//! rmpd feeds this decoder untrusted network streams, so malformed input (truncated
//! blocks, corrupted sub-block lengths, flipped flag bits) must never panic. This test
//! takes real encoded blocks and both bit-flips and truncates them at deterministic
//! (seeded) positions, then only requires that decoding returns cleanly (`Ok` or `Err`,
//! observed via `catch_unwind`) instead of panicking or hanging.

use std::fs::File;
use std::io::Read;
use std::panic::{self, AssertUnwindSafe};

use symphonia_codec_wavpack::{WavPackDecoder, WavPackReader};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;

/// A small xorshift PRNG so the mutation sequence is reproducible without pulling in a
/// `rand` dev-dependency.
struct Xorshift(u64);

impl Xorshift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{}", env!("CARGO_MANIFEST_DIR"), name);
    let mut buf = Vec::new();
    File::open(&path).unwrap_or_else(|e| panic!("open {path}: {e}")).read_to_end(&mut buf).unwrap();
    buf
}

/// Fully decode `data` as a WavPack file, ignoring any decode errors (only panics fail
/// the test). Silences `log`/`debug!` noise from malformed-input error paths.
fn try_decode_all(data: Vec<u8>) {
    let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(data)), Default::default());
    let mut reader = match WavPackReader::try_new(mss, FormatOptions::default()) {
        Ok(r) => r,
        Err(_) => return,
    };

    let codec_params: AudioCodecParameters = match reader.tracks().first().and_then(|t| t.codec_params.clone()) {
        Some(CodecParameters::Audio(a)) => a,
        _ => return,
    };
    let mut decoder = match WavPackDecoder::try_new(&codec_params, &AudioDecoderOptions::default()) {
        Ok(d) => d,
        Err(_) => return,
    };

    loop {
        let packet = match reader.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(_) => break,
        };
        // A decode error is an expected outcome for mutated input; only a panic fails
        // this test.
        let _ = decoder.decode_ref(&packet.as_packet_ref());
    }
}

fn fuzz_fixture(name: &str, iterations: u64, seed: u64) {
    let original = fixture(name);
    let mut rng = Xorshift(seed | 1);

    for i in 0..iterations {
        let mut data = original.clone();

        // Deterministic mix of mutation kinds so both "bit rot" and "truncated stream"
        // classes of malformed input are exercised.
        match i % 3 {
            0 => {
                // Flip a handful of random bits.
                let flips = 1 + (rng.next() % 4);
                for _ in 0..flips {
                    if data.is_empty() {
                        break;
                    }
                    let pos = (rng.next() as usize) % data.len();
                    let bit = (rng.next() % 8) as u32;
                    data[pos] ^= 1 << bit;
                }
            }
            1 => {
                // Truncate to a random prefix length.
                if !data.is_empty() {
                    let len = 1 + (rng.next() as usize) % data.len();
                    data.truncate(len);
                }
            }
            _ => {
                // Overwrite a random contiguous run with random bytes (simulates a
                // corrupted sub-block length/flags word).
                if !data.is_empty() {
                    let start = (rng.next() as usize) % data.len();
                    let run = 1 + (rng.next() as usize) % 8;
                    for j in 0..run {
                        if start + j >= data.len() {
                            break;
                        }
                        data[start + j] = (rng.next() & 0xff) as u8;
                    }
                }
            }
        }

        let result = panic::catch_unwind(AssertUnwindSafe(|| try_decode_all(data.clone())));
        assert!(
            result.is_ok(),
            "panic decoding mutated {name} (iteration {i}, {} bytes)",
            data.len()
        );
    }
}

#[test]
fn fuzz_float_lossless_no_panics() {
    fuzz_fixture("float_lossless.wv", 400, 0x5EED_F10A);
}

#[test]
fn fuzz_float_hybrid_lossy_no_panics() {
    fuzz_fixture("float_hybrid_lossy.wv", 400, 0x5EED_F10B);
}

#[test]
fn fuzz_int_hybrid_lossy_no_panics() {
    fuzz_fixture("int_hybrid_lossy.wv", 400, 0x5EED_F10C);
}

#[test]
fn fuzz_float_mono_lossless_no_panics() {
    fuzz_fixture("float_mono_lossless.wv", 400, 0x5EED_F10D);
}

#[test]
fn fuzz_random_garbage_no_panics() {
    // Not derived from a real file at all: pure random bytes with the "wvpk" magic
    // stamped in occasionally, to probe the header/sub-block parsers directly.
    let mut rng = Xorshift(0x5EED_F10E);
    for i in 0..200u64 {
        let len = 32 + (rng.next() as usize) % 4096;
        let mut data = vec![0u8; len];
        for b in data.iter_mut() {
            *b = (rng.next() & 0xff) as u8;
        }
        if len >= 4 {
            data[0..4].copy_from_slice(b"wvpk");
        }
        let result = panic::catch_unwind(AssertUnwindSafe(|| try_decode_all(data.clone())));
        assert!(result.is_ok(), "panic decoding random garbage (iteration {i}, {len} bytes)");
    }
}
