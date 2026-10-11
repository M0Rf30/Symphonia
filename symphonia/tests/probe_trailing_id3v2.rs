// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests that an ID3v2 tag appended to a stream is read through the default probe, for every
//! container that is given the metadata found by the probe.

#![cfg(all(feature = "mp3", feature = "wav", feature = "flac", feature = "id3v2"))]

use std::io::Cursor;

use symphonia::core::formats::probe::{Hint, Probe};
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTag};

fn probe(data: Vec<u8>) -> Box<dyn FormatReader> {
    let mut probe = Probe::new();
    symphonia::default::register_enabled_formats(&mut probe);

    let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());

    probe
        .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
        .expect("the stream is probed")
}

/// All of the track titles in all of the metadata revisions of a format reader.
fn titles(format: &mut dyn FormatReader) -> Vec<String> {
    let mut metadata = format.metadata();
    let mut titles = Vec::new();

    loop {
        if let Some(revision) = metadata.current() {
            titles.extend(revision.media.tags.iter().filter_map(|tag| match &tag.std {
                Some(StandardTag::TrackTitle(title)) => Some(title.to_string()),
                _ => None,
            }));
        }

        if metadata.pop().is_none() {
            break;
        }
    }

    titles
}

fn syncsafe(n: usize) -> [u8; 4] {
    [((n >> 21) & 0x7f) as u8, ((n >> 14) & 0x7f) as u8, ((n >> 7) & 0x7f) as u8, (n & 0x7f) as u8]
}

/// An ID3v2 tag with a single TIT2 (title) frame. The tag has a footer if it is version 4.
fn id3v2_tag(major: u8, title: &str) -> Vec<u8> {
    let mut body = vec![3u8];
    body.extend_from_slice(title.as_bytes());

    let mut frame = b"TIT2".to_vec();
    if major == 4 {
        frame.extend_from_slice(&syncsafe(body.len()));
    }
    else {
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    }
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(&body);

    let flags = if major == 4 { 0x10 } else { 0 };

    let mut tag = vec![b'I', b'D', b'3', major, 0, flags];
    tag.extend_from_slice(&syncsafe(frame.len()));
    tag.extend_from_slice(&frame);

    if major == 4 {
        tag.extend_from_slice(&[b'3', b'D', b'I', major, 0, flags]);
        tag.extend_from_slice(&syncsafe(frame.len()));
    }

    tag
}

/// MPEG-1 layer 3, 128 kbps, 44.1 kHz frames (417 bytes, no padding).
fn mp3_frames() -> Vec<u8> {
    (0..32)
        .flat_map(|_| {
            let mut f = vec![0u8; 417];
            f[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x64]);
            f
        })
        .collect()
}

/// A 16-bit mono PCM WAVE stream.
fn wav() -> Vec<u8> {
    let samples = vec![0u8; 2 * 1000];

    let mut fmt = Vec::new();
    fmt.extend_from_slice(&1u16.to_le_bytes()); // PCM
    fmt.extend_from_slice(&1u16.to_le_bytes()); // Channels
    fmt.extend_from_slice(&8000u32.to_le_bytes()); // Sample rate
    fmt.extend_from_slice(&16000u32.to_le_bytes()); // Byte rate
    fmt.extend_from_slice(&2u16.to_le_bytes()); // Block align
    fmt.extend_from_slice(&16u16.to_le_bytes()); // Bits per sample

    let mut chunks = Vec::new();
    chunks.extend_from_slice(b"WAVE");
    chunks.extend_from_slice(b"fmt ");
    chunks.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    chunks.extend_from_slice(&fmt);
    chunks.extend_from_slice(b"data");
    chunks.extend_from_slice(&(samples.len() as u32).to_le_bytes());
    chunks.extend_from_slice(&samples);

    let mut wav = b"RIFF".to_vec();
    wav.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
    wav.extend_from_slice(&chunks);
    wav
}

/// A FLAC stream with a STREAMINFO block and the header of a single frame.
fn flac() -> Vec<u8> {
    let packed: u64 = (44100u64 << 44) | (1 << 41) | (15 << 36) | 44100;

    let mut streaminfo = Vec::new();
    streaminfo.extend_from_slice(&4096u16.to_be_bytes());
    streaminfo.extend_from_slice(&4096u16.to_be_bytes());
    streaminfo.extend_from_slice(&[0u8; 6]);
    streaminfo.extend_from_slice(&packed.to_be_bytes());
    streaminfo.extend_from_slice(&[0u8; 16]);

    let mut flac = b"fLaC".to_vec();
    // The last metadata block, of type STREAMINFO.
    flac.push(0x80);
    flac.extend_from_slice(&(streaminfo.len() as u32).to_be_bytes()[1..]);
    flac.extend_from_slice(&streaminfo);

    // The header of a frame (4096 samples, 44.1 kHz, independent stereo, 16 bits per sample, and
    // frame number 0) followed by its CRC-8.
    let header = [0xff, 0xf8, 0xc9, 0x18, 0x00];
    let crc8 = header.iter().fold(0u8, |mut crc, &b| {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x07 } else { crc << 1 };
        }
        crc
    });

    flac.extend_from_slice(&header);
    flac.push(crc8);
    flac.extend_from_slice(&[0u8; 64]);
    flac
}

fn id3v1() -> Vec<u8> {
    let mut v1 = vec![0u8; 128];
    v1[..3].copy_from_slice(b"TAG");
    v1
}

#[test]
fn id3v23_tag_without_a_footer_appended_to_mp3() {
    let stream = [mp3_frames(), id3v2_tag(3, "Appended v2.3")].concat();

    let mut format = probe(stream);
    assert_eq!(format.format_info().short_name, "mp3");
    assert_eq!(titles(format.as_mut()), ["Appended v2.3"]);
}

#[test]
fn id3v23_tag_without_a_footer_appended_to_mp3_before_id3v1() {
    let stream = [mp3_frames(), id3v2_tag(3, "Before v1"), id3v1()].concat();

    let mut format = probe(stream);
    assert_eq!(format.format_info().short_name, "mp3");
    assert_eq!(titles(format.as_mut()), ["Before v1"]);
}

#[test]
fn id3v24_tag_with_a_footer_appended_to_mp3_is_read_once() {
    let stream = [mp3_frames(), id3v2_tag(4, "Appended v2.4")].concat();

    let mut format = probe(stream);
    assert_eq!(format.format_info().short_name, "mp3");
    assert_eq!(titles(format.as_mut()), ["Appended v2.4"]);
}

#[test]
fn id3v24_tag_with_a_footer_appended_to_flac() {
    let stream = [flac(), id3v2_tag(4, "Appended to FLAC")].concat();

    let mut format = probe(stream);
    assert_eq!(format.format_info().short_name, "flac");
    assert_eq!(titles(format.as_mut()), ["Appended to FLAC"]);
}

#[test]
fn id3v24_tag_with_a_footer_appended_to_wav() {
    let stream = [wav(), id3v2_tag(4, "Appended to WAV")].concat();

    let mut format = probe(stream);
    assert_eq!(format.format_info().short_name, "wave");
    assert_eq!(titles(format.as_mut()), ["Appended to WAV"]);
}

#[test]
fn id3v23_tag_without_a_footer_appended_to_wav() {
    let stream = [wav(), id3v2_tag(3, "Appended v2.3 to WAV"), id3v1()].concat();

    let mut format = probe(stream);
    assert_eq!(format.format_info().short_name, "wave");
    assert_eq!(titles(format.as_mut()), ["Appended v2.3 to WAV"]);
}

#[test]
fn streams_without_an_appended_tag_have_no_titles() {
    for stream in [mp3_frames(), wav(), flac()] {
        let mut format = probe(stream);
        assert!(titles(format.as_mut()).is_empty());
    }
}

#[test]
fn prepended_tag_is_read_once() {
    let stream = [id3v2_tag(3, "Prepended"), mp3_frames()].concat();

    let mut format = probe(stream);
    assert_eq!(titles(format.as_mut()), ["Prepended"]);
}
