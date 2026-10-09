// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A minimal transport stream muxer, to build test streams.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::Cursor;
use std::path::PathBuf;

use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::codecs::registry::CodecRegistry;
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::packet::Packet;
use symphonia_core::units::Timestamp;
use symphonia_format_mpegts::MpegTsReader;

use symphonia_common::mpeg::pes::write_timestamp;
use symphonia_format_mpegts::psi::build_section;

pub struct TsMux {
    pub out: Vec<u8>,
    /// The packet size: 188, 192 (BDAV), or 204.
    pub packet_size: usize,
    cc: HashMap<u16, u8>,
    pkts: u32,
}

pub struct StreamSpec {
    pub stream_type: u8,
    pub pid: u16,
    pub descriptors: Vec<u8>,
}

impl TsMux {
    pub fn new(packet_size: usize) -> Self {
        TsMux { out: vec![], packet_size, cc: HashMap::new(), pkts: 0 }
    }

    pub fn packet(&mut self, pid: u16, pusi: bool, payload: &[u8]) {
        assert!(payload.len() <= 184);

        let cc = self.cc.entry(pid).or_insert(0);
        let counter = *cc;
        *cc = (*cc + 1) & 0xf;

        if self.packet_size == 192 {
            // The time stamp.
            self.out.extend_from_slice(&(self.pkts.wrapping_mul(1000) & 0x3fff_ffff).to_be_bytes());
        }

        self.pkts += 1;

        let mut p = vec![0x47, (u8::from(pusi) << 6) | (pid >> 8) as u8, pid as u8];

        if payload.len() == 184 {
            p.push(0x10 | counter);
        }
        else {
            p.push(0x30 | counter);
            let af_len = 183 - payload.len();
            p.push(af_len as u8);

            if af_len > 0 {
                p.push(0x00);
                p.extend(std::iter::repeat(0xff).take(af_len - 1));
            }
        }

        p.extend_from_slice(payload);
        assert_eq!(p.len(), 188);
        self.out.extend_from_slice(&p);

        if self.packet_size == 204 {
            self.out.extend_from_slice(&[0u8; 16]);
        }
    }

    pub fn section(&mut self, pid: u16, section: &[u8]) {
        let mut payload = vec![0u8];
        payload.extend_from_slice(section);
        assert!(payload.len() <= 184);
        self.packet(pid, true, &payload);
    }

    pub fn psi(&mut self, pmt_pid: u16, streams: &[StreamSpec]) {
        let pat = build_section(0, 1, &[0x00, 0x01, 0xe0 | (pmt_pid >> 8) as u8, pmt_pid as u8]);
        self.section(0, &pat);

        let pcr_pid = streams[0].pid;
        let mut body = vec![0xe0 | (pcr_pid >> 8) as u8, pcr_pid as u8, 0xf0, 0x00];

        for s in streams {
            body.extend_from_slice(&[
                s.stream_type,
                0xe0 | (s.pid >> 8) as u8,
                s.pid as u8,
                0xf0 | (s.descriptors.len() >> 8) as u8,
                s.descriptors.len() as u8,
            ]);
            body.extend_from_slice(&s.descriptors);
        }

        let pmt = build_section(2, 1, &body);
        self.section(pmt_pid, &pmt);
    }

    pub fn pes(&mut self, pid: u16, stream_id: u8, pts: Option<u64>, payload: &[u8]) {
        let mut pes = vec![0, 0, 1, stream_id];
        let hdr_data: Vec<u8> = match pts {
            Some(pts) => write_timestamp(pts, 0b0010).to_vec(),
            None => vec![],
        };

        let len = 3 + hdr_data.len() + payload.len();
        assert!(len <= 0xffff);
        pes.extend_from_slice(&(len as u16).to_be_bytes());
        pes.push(0x80);
        pes.push(if pts.is_some() { 0x80 } else { 0x00 });
        pes.push(hdr_data.len() as u8);
        pes.extend_from_slice(&hdr_data);
        pes.extend_from_slice(payload);

        for (i, chunk) in pes.chunks(184).enumerate() {
            self.packet(pid, i == 0, chunk);
        }
    }
}

/// The descriptors of an Opus stream with `channels` channels.
pub fn opus_descriptors(channels: u8) -> Vec<u8> {
    vec![0x05, 0x04, b'O', b'p', b'u', b's', 0x7f, 0x02, 0x80, channels]
}

pub fn mss(data: Vec<u8>) -> MediaSourceStream<'static> {
    MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default())
}

pub fn open(data: Vec<u8>) -> MpegTsReader<'static> {
    MpegTsReader::try_new(mss(data), FormatOptions::default()).unwrap()
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
    reg.register_audio_decoder::<symphonia_codec_opus::OpusAudioDecoder>();
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
