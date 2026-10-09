// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Decode and seek benchmark.
//!
//! Usage: `cargo run --release --example decode_bench -- [-n RUNS] [-s SEEKS] FILE...`
//!
//! For every file, decodes it fully `RUNS` times (default 3; the `.wvc` sibling is used if there
//! is one) and prints the minimum demux+decode time and the speed relative to real time. With
//! `-s`, it also times `SEEKS` seeks spread over the file (on a reader that has already been read
//! to the end once).

use std::fs::File;
use std::path::Path;
use std::time::Instant;

use symphonia_codec_wavpack::{WavPackDecoder, WavPackReader, with_sibling_correction};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::units::Timestamp;

fn open(path: &str) -> WavPackReader<'static> {
    let file = File::open(path).expect("open");
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let opts = with_sibling_correction(Path::new(path), FormatOptions::default());
    WavPackReader::try_new(mss, opts).expect("probe")
}

fn decoder(reader: &WavPackReader<'_>) -> (WavPackDecoder, u32) {
    let params = match reader.tracks()[0].codec_params.as_ref().unwrap() {
        CodecParameters::Audio(p) => p.clone(),
        _ => panic!("not audio"),
    };
    let rate = params.sample_rate.unwrap_or(44100);
    (WavPackDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap(), rate)
}

fn decode(path: &str) -> (u64, u32, f64) {
    let mut reader = open(path);
    let (mut decoder, rate) = decoder(&reader);
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

fn seeks(path: &str, n: u64) -> f64 {
    let mut reader = open(path);
    let (_, _) = decoder(&reader);
    let total = reader.tracks()[0].num_frames.unwrap_or(0) as i64;
    // Read the stream to the end once, as a player that has played the file would have.
    while reader.next_packet().expect("next_packet").is_some() {}

    let mut state = 0x2545_f491_4f6c_dd1du64;
    let t = Instant::now();
    for _ in 0..n {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let ts = (state % total.max(1) as u64) as i64;
        reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(ts), track_id: 0 })
            .expect("seek");
    }
    t.elapsed().as_secs_f64() / n as f64
}

fn main() {
    let mut runs = 3usize;
    let mut nseeks = 0u64;
    let mut files = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-n" => runs = args.next().and_then(|v| v.parse().ok()).unwrap_or(3),
            "-s" => nseeks = args.next().and_then(|v| v.parse().ok()).unwrap_or(0),
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
        let mut line = format!("frames={frames} min={:.1}ms x{:.0}RT", best * 1000.0, secs / best);
        if nseeks > 0 {
            line += &format!(" seek={:.3}ms", seeks(&path, nseeks) * 1000.0);
        }
        println!("{line} {path}");
    }
}
