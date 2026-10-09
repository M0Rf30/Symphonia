// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A minimal FLV muxer, to build test streams, and test helpers.

#![allow(dead_code)]

use std::io::Cursor;
use std::path::PathBuf;

use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::codecs::registry::CodecRegistry;
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::packet::Packet;
use symphonia_core::units::Timestamp;
use symphonia_format_flv::FlvReader;

pub struct FlvMux {
    pub out: Vec<u8>,
}

impl FlvMux {
    pub fn new() -> Self {
        let mut out = b"FLV\x01\x05".to_vec();
        out.extend_from_slice(&9u32.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        FlvMux { out }
    }

    pub fn tag(&mut self, ty: u8, ts: u32, data: &[u8]) {
        self.out.push(ty);
        self.out.extend_from_slice(&(data.len() as u32).to_be_bytes()[1..]);
        self.out.extend_from_slice(&ts.to_be_bytes()[1..]);
        self.out.push((ts >> 24) as u8);
        self.out.extend_from_slice(&[0, 0, 0]);
        self.out.extend_from_slice(data);
        self.out.extend_from_slice(&(11 + data.len() as u32).to_be_bytes());
    }

    /// Write an audio tag with the sound flags byte `flags`.
    pub fn audio(&mut self, ts: u32, flags: u8, payload: &[u8]) {
        let mut data = vec![flags];
        data.extend_from_slice(payload);
        self.tag(8, ts, &data);
    }

    /// Write an AAC audio tag.
    pub fn aac(&mut self, ts: u32, packet_type: u8, payload: &[u8]) {
        let mut data = vec![0xaf, packet_type];
        data.extend_from_slice(payload);
        self.tag(8, ts, &data);
    }

    pub fn video(&mut self, ts: u32, len: usize) {
        let mut data = vec![0x17, 1, 0, 0, 0];
        data.resize(len, 0x55);
        self.tag(9, ts, &data);
    }
}

pub fn amf_string(s: &str) -> Vec<u8> {
    let mut v = vec![0x02];
    v.extend_from_slice(&(s.len() as u16).to_be_bytes());
    v.extend_from_slice(s.as_bytes());
    v
}

pub fn amf_number(n: f64) -> Vec<u8> {
    let mut v = vec![0x00];
    v.extend_from_slice(&n.to_be_bytes());
    v
}

pub fn amf_prop(key: &str, value: &[u8]) -> Vec<u8> {
    let mut v = (key.len() as u16).to_be_bytes().to_vec();
    v.extend_from_slice(key.as_bytes());
    v.extend_from_slice(value);
    v
}

pub fn amf_strict_array(values: &[f64]) -> Vec<u8> {
    let mut v = vec![0x0a];
    v.extend_from_slice(&(values.len() as u32).to_be_bytes());
    for &n in values {
        v.extend(amf_number(n));
    }
    v
}

/// The `onMetaData` script data with properties.
pub fn on_metadata(props: &[Vec<u8>]) -> Vec<u8> {
    let mut v = amf_string("onMetaData");
    v.push(0x08);
    v.extend_from_slice(&(props.len() as u32).to_be_bytes());
    for p in props {
        v.extend_from_slice(p);
    }
    v.extend_from_slice(&[0, 0, 9]);
    v
}

pub fn mss(data: Vec<u8>) -> MediaSourceStream<'static> {
    MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default())
}

pub fn open(data: Vec<u8>) -> FlvReader<'static> {
    FlvReader::try_new(mss(data), FormatOptions::default()).unwrap()
}

pub fn read_all(reader: &mut dyn FormatReader) -> Vec<Packet> {
    let mut packets = vec![];

    while let Some(p) = reader.next_packet().unwrap() {
        packets.push(p);
    }

    packets
}

/// The path of a sample file, if the samples are available.
pub fn sample(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os("RMPD_SAMPLES")?).join(name);

    if path.exists() {
        Some(path)
    }
    else {
        eprintln!("skipping: {} not found", path.display());
        None
    }
}

/// Split the data into chunks of varying sizes, from `sizes`, cyclically.
pub fn chunks_of<'a>(data: &'a [u8], sizes: &'a [usize]) -> impl Iterator<Item = &'a [u8]> + 'a {
    let mut pos = 0;
    let mut i = 0;

    std::iter::from_fn(move || {
        if pos >= data.len() {
            return None;
        }

        let n = sizes[i % sizes.len()].min(data.len() - pos);
        i += 1;
        let c = &data[pos..pos + n];
        pos += n;
        Some(c)
    })
}

pub fn registry() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    reg.register_audio_decoder::<symphonia_codec_aac::AacDecoder>();
    reg.register_audio_decoder::<symphonia_bundle_mp3::MpaDecoder>();
    reg.register_audio_decoder::<symphonia_codec_pcm::PcmDecoder>();
    reg
}

pub fn audio_params(reader: &dyn FormatReader, idx: usize) -> AudioCodecParameters {
    match reader.tracks()[idx].codec_params.clone() {
        Some(CodecParameters::Audio(p)) => p,
        _ => panic!("not an audio track"),
    }
}

pub fn decode(dec: &mut dyn AudioDecoder, packet: &Packet) -> Option<Vec<f32>> {
    let mut out = vec![];
    dec.decode(packet).ok()?.copy_to_vec_interleaved::<f32>(&mut out);
    Some(out)
}

/// Decode all packets with a new decoder.
pub fn decode_all(params: &AudioCodecParameters, packets: &[Packet]) -> Vec<Vec<f32>> {
    let mut dec = registry().make_audio_decoder(params, &AudioDecoderOptions::default()).unwrap();
    packets.iter().map(|p| decode(&mut *dec, p).unwrap_or_default()).collect()
}

/// Check that seeking to each of the timestamps in `targets` (in sequence, without re-opening)
/// reproduces the samples of a continuous decode at and after the timestamp. The samples must be
/// identical if `tolerance` is `Some(0.0)`, differ by at most `tolerance` if it is `Some`, and
/// only have the same length if it is `None`.
pub fn check_seeks(
    open: impl Fn() -> Box<dyn FormatReader>,
    idx: usize,
    targets: &[i64],
    tolerance: Option<f32>,
) {
    let mut reader = open();
    let params = audio_params(&*reader, idx);
    let track_id = reader.tracks()[idx].id;

    let all: Vec<Packet> =
        read_all(&mut *reader).into_iter().filter(|p| p.track_id == track_id).collect();
    let cont = decode_all(&params, &all);

    let mut dec = registry().make_audio_decoder(&params, &AudioDecoderOptions::default()).unwrap();

    for &target in targets {
        let seeked = reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(target), track_id })
            .unwrap_or_else(|e| panic!("seek to {target} failed: {e}"));

        assert_eq!(seeked.required_ts.get(), target);
        assert!(seeked.actual_ts.get() <= target, "actual {} > {target}", seeked.actual_ts);

        dec.reset();

        let mut first = true;
        let mut compared = 0;

        for _ in 0..120 {
            let Some(packet) = reader.next_packet().unwrap()
            else {
                break;
            };

            if packet.track_id != track_id {
                continue;
            }

            if first {
                assert_eq!(packet.pts, seeked.actual_ts, "first packet is the actual ts");
                first = false;
            }

            let got = decode(&mut *dec, &packet);

            if packet.pts.get() >= target {
                let want = &cont[all.iter().position(|p| p.pts == packet.pts).unwrap()];

                match (tolerance, got) {
                    (Some(tol), Some(got)) => {
                        assert_eq!(got.len(), want.len());
                        let diff =
                            got.iter().zip(want).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max);
                        assert!(diff <= tol, "target {target} pts {}: diff {diff}", packet.pts);
                    }
                    (None, Some(got)) => assert_eq!(got.len(), want.len()),
                    (_, None) => panic!("decode failed"),
                }

                compared += 1;
            }
        }

        assert!(
            compared > 10 || all.last().is_some_and(|p| p.pts.get() < target + 20_000),
            "target {target}"
        );
    }
}

/// A xorshift generator.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Corrupt the stream randomly: flip bytes, overwrite ranges, delete ranges, insert junk, truncate.
pub fn corrupt(data: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut d = data.to_vec();

    for _ in 0..1 + rng.below(8) {
        if d.len() < 32 {
            break;
        }

        let pos = rng.below(d.len() - 8);

        match rng.below(5) {
            0 => d[pos] ^= 1 << rng.below(8),
            1 => {
                let n = 1 + rng.below(300);
                let end = (pos + n).min(d.len());
                for b in d[pos..end].iter_mut() {
                    *b = rng.next() as u8;
                }
            }
            2 => {
                let n = 1 + rng.below(2000);
                d.drain(pos..(pos + n).min(d.len()));
            }
            3 => {
                let n = 1 + rng.below(500);
                let junk: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
                d.splice(pos..pos, junk);
            }
            _ => d.truncate(pos + 8),
        }
    }

    d
}
