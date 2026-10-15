// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Decodes streams produced by `elc3` of liblc3 and compares them with the 16-bit output of
//! `dlc3` of the same library.

use std::path::{Path, PathBuf};

use symphonia_codec_lc3::{Lc3Decoder, delay_samples};
use symphonia_core::audio::Channels;
use symphonia_core::codecs::audio::well_known::CODEC_ID_LC3;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

/// The content of a `.lc3` file (`lc3bin.c`).
struct Lc3File {
    sample_rate: u32,
    channels: usize,
    frame_us: u32,
    hr_mode: bool,
    nsamples: usize,
    blocks: Vec<Vec<u8>>,
}

fn parse_lc3(data: &[u8]) -> Lc3File {
    let u16_at = |o: usize| usize::from(u16::from_le_bytes([data[o], data[o + 1]]));

    assert_eq!(&data[..2], &[0x1c, 0xcc]);
    let hdr_size = u16_at(2);

    let mut blocks = Vec::new();
    let mut pos = hdr_size;
    while pos + 2 <= data.len() {
        let n = u16_at(pos);
        pos += 2;
        if n == 0 || pos + n > data.len() {
            break;
        }
        blocks.push(data[pos..pos + n].to_vec());
        pos += n;
    }

    Lc3File {
        sample_rate: u16_at(4) as u32 * 100,
        channels: u16_at(8),
        frame_us: u16_at(10) as u32 * 10,
        hr_mode: hdr_size >= 20 && u16_at(18) != 0,
        nsamples: u16_at(14) | (u16_at(16) << 16),
        blocks,
    }
}

fn extra_data(f: &Lc3File) -> Box<[u8]> {
    let mut v = Vec::new();
    v.extend_from_slice(&((f.frame_us / 10) as u16).to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&u16::from(f.hr_mode).to_le_bytes());
    v.into()
}

fn make_decoder(f: &Lc3File) -> Lc3Decoder {
    let mut p = AudioCodecParameters::new();
    p.for_codec(CODEC_ID_LC3)
        .with_sample_rate(f.sample_rate)
        .with_channels(Channels::Discrete(f.channels as u16))
        .with_extra_data(extra_data(f));
    Lc3Decoder::try_new(&p, &AudioDecoderOptions::default()).unwrap()
}

/// Decodes the blocks to interleaved samples, like `dlc3`: the delay is dropped, and the output
/// is truncated to the length of the stream. Blocks are replaced by empty ones (lost) when
/// listed in `lost`.
fn decode_s16(f: &Lc3File, lost: &[usize]) -> Vec<i16> {
    let mut dec = make_decoder(f);
    let delay = delay_samples(f.frame_us, f.sample_rate, f.hr_mode).unwrap() as usize;

    let mut out = Vec::new();
    let mut tmp: Vec<f32> = Vec::new();
    for (i, b) in f.blocks.iter().enumerate() {
        let data: &[u8] = if lost.contains(&i) { &[] } else { b };
        let pkt = PacketRef::new(0, Timestamp::new(0), Duration::new(0), data);
        let buf = dec.decode_ref(&pkt).unwrap();
        buf.copy_to_vec_interleaved::<f32>(&mut tmp);
        out.extend(tmp.iter().map(|&s| to_s16(s)));
    }

    let start = delay * f.channels;
    out.drain(..start.min(out.len()));
    out.truncate(f.nsamples * f.channels);
    out
}

/// Converts a sample like `store_s16` of liblc3.
fn to_s16(s: f32) -> i16 {
    let x = s * 32768.0;
    let v = if x >= 0.0 { (x + 0.5) as i32 } else { (x - 0.5) as i32 };
    v.clamp(-32768, 32767) as i16
}

/// Reads the samples of a 16-bit WAVE file written by `dlc3`.
fn read_wav_s16(data: &[u8]) -> Vec<i16> {
    assert_eq!(&data[..4], b"RIFF");
    let mut pos = 12;
    while pos + 8 <= data.len() {
        let n = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        if &data[pos..pos + 4] == b"data" {
            let end = (pos + 8 + n).min(data.len());
            return data[pos + 8..end]
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect();
        }
        pos += 8 + n + (n & 1);
    }
    panic!("no data chunk");
}

fn max_error(a: &[i16], b: &[i16]) -> i32 {
    assert_eq!(a.len(), b.len(), "length mismatch");
    a.iter().zip(b).map(|(&x, &y)| (i32::from(x) - i32::from(y)).abs()).max().unwrap_or(0)
}

/// The directory of the streams: `LC3_SWEEP_DIR` if set (every `.lc3` file in it must have a
/// `.ref.wav` file), else the files of the repository.
fn sweep_dir() -> PathBuf {
    std::env::var_os("LC3_SWEEP_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data"))
}

/// Returns the largest error and the number of samples that differ.
fn check(dir: &Path, name: &str) -> (i32, usize, usize) {
    let lc3 = std::fs::read(dir.join(format!("{name}.lc3"))).unwrap();
    let reference = read_wav_s16(&std::fs::read(dir.join(format!("{name}.ref.wav"))).unwrap());
    let got = decode_s16(&parse_lc3(&lc3), &[]);
    let ndiff = (0..got.len()).filter(|&i| got[i] != reference[i]).count();
    (max_error(&got, &reference), ndiff, got.len())
}

/// The decoder is a port of liblc3 in single precision, and a different order of the float
/// operations moves a sample across a rounding boundary of the 16-bit conversion now and then.
/// This is the most that was seen in a sweep of 178 streams (8 to 96 kHz, 2.5 to 10 ms, mono and
/// stereo, 16 to 400 kbps, normal and high resolution) against `dlc3`: an error of 1, in 0.17%
/// of the samples.
const MAX_ERROR: i32 = 1;
const MAX_DIFFERING: f64 = 0.003;

#[test]
fn sweep() {
    let dir = sweep_dir();

    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let n = e.unwrap().file_name().into_string().unwrap();
            n.strip_suffix(".lc3").map(str::to_owned)
        })
        .collect();
    names.sort();
    assert!(!names.is_empty());

    for name in &names {
        let (e, ndiff, n) = check(&dir, name);
        println!("{name}: max error {e}, {ndiff} of {n} samples differ");
        assert!(e <= MAX_ERROR, "{name}: error {e}");
        assert!((ndiff as f64) <= MAX_DIFFERING * n as f64, "{name}: {ndiff} of {n} differ");
    }
}
