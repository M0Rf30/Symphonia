// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Decode benchmark.
//!
//! Usage: `cargo run --release --example decode_bench -- [-n RUNS] FILE...`
//!
//! For every file, decodes it fully `RUNS` times (default 3) and prints the minimum demux+decode
//! time and the speed relative to real time.

use std::fs::File;
use std::time::Instant;

use symphonia_bundle_ape::{ApeDecoder, ApeReader};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;

fn decode(path: &str) -> (u64, u32, f64) {
    let file = File::open(path).expect("open");
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut reader = ApeReader::try_new(mss, FormatOptions::default()).expect("probe");
    let Some(CodecParameters::Audio(params)) = reader.tracks()[0].codec_params.clone()
    else {
        panic!("not audio");
    };
    let rate = params.sample_rate.unwrap_or(44100);
    let mut decoder = ApeDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
    let mut frames = 0u64;
    let mut busy = std::time::Duration::ZERO;
    loop {
        let t = Instant::now();
        let Some(packet) = reader.next_packet().expect("next_packet")
        else {
            break;
        };
        if let Ok(buf) = decoder.decode_ref(&packet.as_packet_ref()) {
            frames += buf.frames() as u64;
        }
        busy += t.elapsed();
    }
    (frames, rate, busy.as_secs_f64())
}

fn main() {
    let mut runs = 3usize;
    let mut files = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-n" => runs = args.next().and_then(|v| v.parse().ok()).unwrap_or(3),
            _ => files.push(a),
        }
    }

    for path in files {
        let mut best = f64::INFINITY;
        let mut info = (0, 44100);
        for _ in 0..runs.max(1) {
            let (frames, rate, busy) = decode(&path);
            info = (frames, rate);
            best = best.min(busy);
        }
        let (frames, rate) = info;
        let secs = frames as f64 / f64::from(rate);
        println!("frames={frames} min={:.1}ms x{:.0}RT {path}", best * 1000.0, secs / best);
    }
}
