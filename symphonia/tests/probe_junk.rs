// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests that the probe finds a stream after junk: zero padding beyond Symphonia's default probe
//! depth, and `ID3` bytes that are not a readable tag.

#![cfg(all(feature = "mp3", feature = "mpegps", feature = "id3v2"))]

use std::io::Cursor;

mod common;

use common::mp3_frames;
use symphonia::core::formats::FormatOptions;
use symphonia::core::formats::probe::{Hint, Probe, ProbeOptions};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

fn probe(max_probe_depth: u32) -> Probe {
    let mut probe =
        Probe::new_with_options(&ProbeOptions { max_probe_depth, ..ProbeOptions::default() });
    symphonia::default::register_enabled_formats(&mut probe);
    probe
}

fn probe_bytes(probe: &Probe, data: Vec<u8>) -> Result<&'static str, String> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());

    probe
        .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
        .map(|r| r.format_info().short_name)
        .map_err(|e| e.to_string())
}

fn after(prefix: Vec<u8>, stream: Vec<u8>) -> Vec<u8> {
    [prefix, stream].concat()
}

#[test]
fn mp3_after_zero_padding_beyond_the_default_depth() {
    let data = after(vec![0; 2 * 1024 * 1024], mp3_frames());

    assert_eq!(probe_bytes(&probe(4 * 1024 * 1024), data.clone()), Ok("mp3"));
    // The depth still bounds the scan.
    assert!(probe_bytes(&probe(1024 * 1024), data).is_err());
}

#[test]
fn program_stream_after_zero_padding() {
    // MPEG-1 layer 2, 128 kbps, 48 kHz frames in PES packets after a pack header. The zero run
    // ends right before the pack header start code, which itself begins with zeros.
    let mut ps =
        vec![0x00, 0x00, 0x01, 0xba, 0x44, 0x00, 0x04, 0x00, 0x04, 0x01, 0x01, 0x89, 0xc3, 0xf8];
    for n in 0..20u8 {
        let payload: Vec<u8> = (0..2u8)
            .flat_map(|i| {
                let mut f = vec![2 * n + i; 384];
                f[..4].copy_from_slice(&[0xff, 0xfd, 0x84, 0x04]);
                f
            })
            .collect();
        ps.extend_from_slice(&[0x00, 0x00, 0x01, 0xc0]);
        ps.extend_from_slice(&((payload.len() + 3) as u16).to_be_bytes());
        ps.extend_from_slice(&[0x80, 0x00, 0x00]);
        ps.extend_from_slice(&payload);
    }

    assert_eq!(probe_bytes(&probe(1024 * 1024), after(vec![0; 64 * 1024], ps)), Ok("mpegps"));
}

#[test]
fn id3_bytes_with_an_invalid_header_are_not_a_tag() {
    for header in [
        // Unsupported major version.
        *b"ID3\x07\x00\x00\x00\x00\x00\x10",
        // Size is not syncsafe.
        *b"ID3\x03\x00\x00\x00\x00\x80\x10",
        // ID3v2.2 compression.
        *b"ID3\x02\x00\x40\x00\x00\x00\x10",
    ] {
        let mut junk = vec![0x55; 1000];
        junk.extend_from_slice(&header);
        junk.extend_from_slice(&[0x55; 1000]);

        assert_eq!(probe_bytes(&probe(1024 * 1024), after(junk, mp3_frames())), Ok("mp3"));
    }
}

#[test]
fn unreadable_tag_is_skipped() {
    // A valid ID3v2.3 tag header with the extended header flag set, but an invalid extended
    // header size: the header scores, then reading the tag fails.
    let mut tag = b"ID3\x03\x00\x40\x00\x00\x00\x14".to_vec();
    tag.extend_from_slice(&[0x00, 0x00, 0x00, 0x07]);
    tag.extend_from_slice(&[0x00; 16]);

    assert_eq!(probe_bytes(&probe(1024 * 1024), after(tag, mp3_frames())), Ok("mp3"));
}
