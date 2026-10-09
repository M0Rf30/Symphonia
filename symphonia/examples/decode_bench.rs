//! Decode benchmark and bit-exactness fingerprint.
//!
//! Usage: `cargo run --release --features all --example decode_bench -- [-n RUNS] FILE...`
//!
//! For every file, fully decodes the default audio track `RUNS` times (default 3) and prints an
//! FNV-1a 64-bit hash over every decoded sample converted to `f32`, `i32`, and `u32` (so changes to
//! the decoders or to the sample format converters change the hash), the number of frames, and the
//! minimum wall-clock time spent demuxing+decoding and converting (hashing excluded).

use std::fs::File;
use std::time::{Duration, Instant};

use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    fn put(&mut self, v: u32) {
        self.0 = (self.0 ^ u64::from(v)).wrapping_mul(0x100_0000_01b3);
    }
}

struct Outcome {
    hash: u64,
    frames: u64,
    rate: u32,
    decode: Duration,
    convert: Duration,
}

fn decode(path: &str) -> Option<Outcome> {
    let file = File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut format = symphonia::default::get_probe()
        .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
        .ok()?;
    let track = format.default_track(TrackType::Audio)?;
    let track_id = track.id;
    let params = track.codec_params.as_ref()?.audio()?.clone();
    let rate = params.sample_rate.unwrap_or(44100);
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .ok()?;

    let mut hash = Fnv::new();
    let (mut decode, mut convert) = (Duration::ZERO, Duration::ZERO);
    let mut frames = 0u64;
    let (mut f, mut i, mut u) = (Vec::<f32>::new(), Vec::<i32>::new(), Vec::<u32>::new());

    loop {
        let t = Instant::now();
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            _ => break,
        };
        if packet.track_id != track_id {
            continue;
        }
        let res = decoder.decode(&packet);
        decode += t.elapsed();
        let Ok(buf) = res
        else {
            hash.put(0xdead);
            continue;
        };
        let n = buf.samples_interleaved();
        frames += buf.frames() as u64;
        f.resize(n, 0.0);
        i.resize(n, 0);
        u.resize(n, 0);
        let t = Instant::now();
        buf.copy_to_slice_interleaved(&mut f);
        convert += t.elapsed();
        buf.copy_to_slice_interleaved(&mut i);
        buf.copy_to_slice_interleaved(&mut u);
        hash.put(n as u32);
        for k in 0..n {
            hash.put(f[k].to_bits());
            hash.put(i[k] as u32);
            hash.put(u[k]);
        }
    }
    Some(Outcome { hash: hash.0, frames, rate, decode, convert })
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

    let (mut tot_decode, mut tot_convert) = (0.0, 0.0);
    for path in files {
        let mut best: Option<(f64, f64)> = None;
        let mut info = None;
        for _ in 0..runs.max(1) {
            let Some(r) = decode(&path)
            else {
                break;
            };
            let (d, c) = (r.decode.as_secs_f64(), r.convert.as_secs_f64());
            best = Some(best.map_or((d, c), |(bd, bc)| (bd.min(d), bc.min(c))));
            info = Some((r.hash, r.frames, r.rate));
        }
        match (best, info) {
            (Some((d, c)), Some((hash, frames, rate))) => {
                tot_decode += d;
                tot_convert += c;
                let secs = frames as f64 / f64::from(rate);
                println!(
                    "{hash:016x} frames={frames} dec={:.2}ms conv={:.2}ms x{:.0}RT {path}",
                    d * 1000.0,
                    c * 1000.0,
                    secs / d.max(1e-9)
                );
            }
            _ => println!("{:016x} frames=0 FAILED {path}", 0),
        }
    }
    println!("TOTAL dec={:.1}ms conv={:.1}ms", tot_decode * 1000.0, tot_convert * 1000.0);
}
