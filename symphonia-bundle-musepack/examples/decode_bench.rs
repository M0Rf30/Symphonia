// Symphonia Musepack demuxer+decoder
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Decode benchmark and bit-exactness fingerprint.
//!
//! Usage: `cargo run --release --example decode_bench -- [-n RUNS] FILE...`
//!
//! For every file, fully decodes it (gapless on) `RUNS` times (default 3), and prints an FNV-1a
//! 64-bit hash over the exact bit patterns of every decoded `f32` sample (so any change in the
//! output, however small, changes the hash), the number of frames, and the minimum wall-clock
//! decode time with the corresponding speed relative to real time.

use std::fs::File;
use std::time::Instant;

use symphonia_bundle_musepack::{MpcDecoder, MpcReader};
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;

fn decode(path: &str) -> (u64, u64, u32, f64) {
    let file = File::open(path).expect("open");
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut reader = MpcReader::try_new(mss, FormatOptions::default()).expect("probe");
    let track = reader.tracks()[0].clone();
    let params = match track.codec_params.as_ref().unwrap() {
        CodecParameters::Audio(p) => p.clone(),
        _ => panic!("not audio"),
    };
    let rate = params.sample_rate.unwrap_or(44100);
    let mut decoder = MpcDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut frames = 0u64;
    // Time spent demuxing + decoding only (hashing is excluded).
    let mut busy = std::time::Duration::ZERO;
    loop {
        let t = Instant::now();
        let Some(packet) = reader.next_packet().expect("next_packet")
        else {
            break;
        };
        let res = decoder.decode(&packet);
        busy += t.elapsed();
        let buf = match res {
            Ok(b) => b,
            Err(_) => continue,
        };
        if let GenericAudioBufferRef::F32(b) = buf {
            let channels = b.spec().channels().count();
            frames += b.frames() as u64;
            for ch in 0..channels {
                for s in b.plane(ch).unwrap() {
                    hash = (hash ^ u64::from(s.to_bits())).wrapping_mul(0x100_0000_01b3);
                }
            }
        }
    }
    (hash, frames, rate, busy.as_secs_f64())
}

fn main() {
    let mut runs = 3usize;
    let mut files = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "-n" {
            runs = args.next().and_then(|v| v.parse().ok()).unwrap_or(3);
        }
        else {
            files.push(a);
        }
    }

    for path in files {
        let mut best = f64::INFINITY;
        let mut info = (0, 0, 44100);
        for _ in 0..runs.max(1) {
            let (hash, frames, rate, busy) = decode(&path);
            info = (hash, frames, rate);
            best = best.min(busy);
        }
        let (hash, frames, rate) = info;
        let secs = frames as f64 / f64::from(rate);
        println!(
            "{hash:016x} frames={frames} min={:.1}ms x{:.0}RT {path}",
            best * 1000.0,
            secs / best
        );
    }
}
