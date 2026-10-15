// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Demuxes `.spdif` files written by FFmpeg's `spdif` muxer, and compares the packets with the
//! `framecrc` output of FFmpeg's `spdif` demuxer for the same files: the size and Adler-32 of
//! every packet, and the timestamps and durations of the frames (which FFmpeg does not give for
//! DTS and E-AC-3).

use std::io::Cursor;
use std::path::PathBuf;

use symphonia_core::codecs::CodecParameters;
use symphonia_core::formats::probe::{Hint, Probe};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::meta::MetadataOptions;
use symphonia_core::units::Timestamp;
use symphonia_format_spdif::SpdifReader;

fn data(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name);
    std::fs::read(path).unwrap()
}

fn mss(data: Vec<u8>) -> MediaSourceStream<'static> {
    MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default())
}

/// The checksum of FFmpeg's `framecrc` muxer: Adler-32 that starts from 0 (`av_adler32_update(0,
/// ...)`), not from 1.
fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (0u32, 0u32);
    for &x in data {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// One packet of a `framecrc` file.
struct Frame {
    pts: i64,
    dur: i64,
    size: usize,
    crc: u32,
}

/// Parses a `framecrc` file: the time base (the denominator), and the frames.
fn framecrc(name: &str) -> (i64, Vec<Frame>) {
    let text = String::from_utf8(data(name)).unwrap();
    let mut tb = 0;
    let mut frames = Vec::new();

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("#tb 0: 1/") {
            tb = rest.trim().parse().unwrap();
        }
        else if !line.starts_with('#') {
            let f: Vec<&str> = line.split(',').map(str::trim).collect();
            frames.push(Frame {
                pts: f[2].parse().unwrap(),
                dur: f[3].parse().unwrap(),
                size: f[4].parse().unwrap(),
                crc: u32::from_str_radix(f[5].trim_start_matches("0x"), 16).unwrap(),
            });
        }
    }

    (tb, frames)
}

fn check(name: &str, codec_name: &str, check_times: bool) {
    let (tb, frames) = framecrc(&format!("{name}.framecrc"));
    assert!(!frames.is_empty());

    let mut reader = SpdifReader::try_new(mss(data(&format!("{name}.spdif"))), Default::default())
        .unwrap_or_else(|e| panic!("{name}: {e}"));

    let Some(CodecParameters::Audio(params)) = reader.tracks()[0].codec_params.clone()
    else {
        panic!("{name}: no audio parameters");
    };
    let rate = i64::from(params.sample_rate.unwrap());
    eprintln!("{name}: {codec_name} {rate} Hz, {} frames", frames.len());

    for (i, f) in frames.iter().enumerate() {
        let packet = reader.next_packet().unwrap().unwrap_or_else(|| panic!("{name}: ends at {i}"));
        assert_eq!(packet.data.len(), f.size, "{name}: size of packet {i}");
        assert_eq!(adler32(&packet.data), f.crc, "{name}: crc of packet {i}");

        if check_times && f.dur > 0 {
            // FFmpeg's time base is 1/90000, with rounded timestamps.
            let ts = (f.pts * rate + tb / 2) / tb;
            assert!((packet.pts.get() - ts).abs() <= 1, "{name}: pts of packet {i}");
            let dur = (f.dur * rate + tb / 2) / tb;
            assert!(
                (packet.dur.get() as i64 - dur).abs() <= 1,
                "{name}: dur of packet {i}: {} vs {dur}",
                packet.dur.get()
            );
        }
    }

    assert!(reader.next_packet().unwrap().is_none(), "{name}: more packets than FFmpeg");
}

#[test]
fn ac3() {
    check("ac3", "ac3", true);
}

#[test]
fn eac3() {
    check("eac3", "eac3", true);
}

#[test]
fn dts() {
    check("dts", "dts", false);
}

#[test]
fn mpeg_audio() {
    check("mp2", "mp2", true);
    check("mp2_lsf", "mp2", true);
    check("mp3", "mp3", true);
    check("mp3_lsf", "mp3", true);
}

#[test]
fn aac_adts() {
    check("aac_adts", "aac", true);
}

#[test]
fn probes_and_seeks() {
    let mut probe = Probe::new();
    probe.register_format::<SpdifReader<'_>>();

    let (_, frames) = framecrc("ac3.framecrc");

    let mut reader = probe
        .probe(
            &Hint::new(),
            mss(data("ac3.spdif")),
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .unwrap();
    assert_eq!(reader.format_info().short_name, "spdif");

    // AC-3 frames of 1536 samples.
    let seeked = reader
        .seek(
            SeekMode::Accurate,
            SeekTo::Timestamp { ts: Timestamp::new(3 * 1536 + 7), track_id: 0 },
        )
        .unwrap();
    assert_eq!(seeked.actual_ts.get(), 3 * 1536);

    let packet = reader.next_packet().unwrap().unwrap();
    assert_eq!(packet.pts.get(), 3 * 1536);
    assert_eq!(adler32(&packet.data), frames[3].crc);
}

#[test]
fn garbage_is_not_a_stream() {
    let junk: Vec<u8> = (0..4096u32).map(|i| (i * 7 + 3) as u8).collect();
    assert!(SpdifReader::try_new(mss(junk), Default::default()).is_err());
}
