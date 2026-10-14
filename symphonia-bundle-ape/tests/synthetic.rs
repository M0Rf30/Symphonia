// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end tests of the demuxer and the decoder on small APE files that are built here: their
//! frames are the silent frames that need no entropy coded data, so that the layout of the file
//! (what ends the last frame, what a truncated frame yields) can be tested without a sample.

use std::io::Cursor;

use symphonia_bundle_ape::{ApeDecoder, ApeReader};
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::checksum::Crc32Ieee;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions, Monitor};

const BLOCKS_PER_FRAME: u32 = 1000;
const FINAL_FRAME_BLOCKS: u32 = 10;

/// The checksum of the Monkey's Audio frames: the IEEE CRC-32 less its lowest bit.
fn ape_crc(data: &[u8]) -> u32 {
    let mut crc = Crc32Ieee::new(!0);
    crc.process_buf_bytes(data);
    !crc.crc() >> 1
}

/// The frame of `blocks` blocks of 16-bit stereo silence: the checksum, with the flag of the special
/// codes, the special codes of both channels silent, and the bytes that the range coder starts
/// with.
fn silent_frame(blocks: u32) -> Vec<u8> {
    let pcm = vec![0u8; blocks as usize * 4];
    let mut frame = Vec::new();
    frame.extend_from_slice(&(ape_crc(&pcm) | 0x8000_0000).to_le_bytes());
    frame.extend_from_slice(&3u32.to_le_bytes());
    frame.extend_from_slice(&[0; 16]);
    frame
}

fn le32(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}

/// A file of the current format (version 3990) of silent 16-bit stereo frames, with `tail` after
/// the frames.
fn silent_file(frames: u32, tail: &[u8]) -> Vec<u8> {
    let frame_data: Vec<Vec<u8>> = (0..frames)
        .map(|i| silent_frame(if i + 1 == frames { FINAL_FRAME_BLOCKS } else { BLOCKS_PER_FRAME }))
        .collect();
    let frame_bytes: usize = frame_data.iter().map(Vec::len).sum();

    let seek_table_bytes = frames * 4;
    let first_frame = 52 + 24 + seek_table_bytes;

    let mut file = Vec::new();
    file.extend_from_slice(b"MAC ");
    file.extend_from_slice(&3990u16.to_le_bytes());
    file.extend_from_slice(&0u16.to_le_bytes());
    file.extend_from_slice(&le32(52)); // descriptor bytes
    file.extend_from_slice(&le32(24)); // header bytes
    file.extend_from_slice(&le32(seek_table_bytes));
    file.extend_from_slice(&le32(0)); // header data bytes
    file.extend_from_slice(&le32(frame_bytes as u32));
    file.extend_from_slice(&le32(0)); // frame data bytes (high)
    file.extend_from_slice(&le32(0)); // terminating data bytes
    file.extend_from_slice(&[0u8; 16]); // md5
    file.extend_from_slice(&2000u16.to_le_bytes()); // compression level
    file.extend_from_slice(&0u16.to_le_bytes()); // format flags
    file.extend_from_slice(&le32(BLOCKS_PER_FRAME));
    file.extend_from_slice(&le32(FINAL_FRAME_BLOCKS));
    file.extend_from_slice(&le32(frames));
    file.extend_from_slice(&16u16.to_le_bytes());
    file.extend_from_slice(&2u16.to_le_bytes());
    file.extend_from_slice(&le32(44100));

    let mut at = first_frame;
    for frame in &frame_data {
        file.extend_from_slice(&le32(at));
        at += frame.len() as u32;
    }
    for frame in &frame_data {
        file.extend_from_slice(frame);
    }
    file.extend_from_slice(tail);
    file
}

/// Decode a file: the number of blocks, whether they are all silent, and the error that ended
/// the decoding.
fn decode(file: Vec<u8>) -> (u64, bool, Option<String>) {
    let mss =
        MediaSourceStream::new(Box::new(Cursor::new(file)), MediaSourceStreamOptions::default());
    let mut reader = ApeReader::try_new(mss, FormatOptions::default()).expect("valid header");
    let Some(CodecParameters::Audio(params)) = reader.tracks()[0].codec_params.clone()
    else {
        panic!("no audio codec params");
    };
    let mut decoder =
        ApeDecoder::try_new(&params, &AudioDecoderOptions::default()).expect("valid parameters");

    let (mut blocks, mut silent) = (0, true);
    loop {
        match reader.next_packet() {
            Ok(Some(packet)) => {
                let buf = decoder.decode_ref(&packet.as_packet_ref()).expect("valid frame");
                let GenericAudioBufferRef::S32(buf) = buf
                else {
                    panic!("unexpected sample format");
                };
                blocks += buf.frames() as u64;
                for ch in 0..buf.spec().channels().count() {
                    silent &= buf.plane(ch).expect("plane").iter().all(|&s| s == 0);
                }
            }
            Ok(None) => return (blocks, silent, None),
            Err(e) => return (blocks, silent, Some(e.to_string())),
        }
    }
}

const ALL_BLOCKS: u64 = 2 * BLOCKS_PER_FRAME as u64 + FINAL_FRAME_BLOCKS as u64;

#[test]
fn decodes_every_frame() {
    assert_eq!(decode(silent_file(3, &[])), (ALL_BLOCKS, true, None));
}

#[test]
fn tags_after_the_frames_do_not_end_the_last_frame() {
    // An ID3v1 tag.
    let mut id3v1 = b"TAG".to_vec();
    id3v1.resize(128, 0);
    assert_eq!(decode(silent_file(3, &id3v1)), (ALL_BLOCKS, true, None));

    // An APEv2 tag of one item, which has a footer only.
    let mut item = Vec::new();
    item.extend_from_slice(&le32(3));
    item.extend_from_slice(&le32(0));
    item.extend_from_slice(b"Key\0abc");
    let mut apev2 = item.clone();
    apev2.extend_from_slice(b"APETAGEX");
    apev2.extend_from_slice(&le32(2000));
    apev2.extend_from_slice(&le32(item.len() as u32 + 32));
    apev2.extend_from_slice(&le32(1));
    apev2.extend_from_slice(&le32(0));
    apev2.extend_from_slice(&[0; 8]);
    assert_eq!(decode(silent_file(3, &apev2)), (ALL_BLOCKS, true, None));

    // Both.
    apev2.extend_from_slice(&id3v1);
    assert_eq!(decode(silent_file(3, &apev2)), (ALL_BLOCKS, true, None));
}

#[test]
fn a_corrupt_tag_does_not_shorten_the_last_frame() {
    // A footer that claims a tag larger than the file.
    let mut footer = b"APETAGEX".to_vec();
    footer.extend_from_slice(&le32(2000));
    footer.extend_from_slice(&le32(100_000));
    footer.extend_from_slice(&le32(1));
    footer.extend_from_slice(&le32(0));
    footer.extend_from_slice(&[0; 8]);
    assert_eq!(decode(silent_file(3, &footer)), (ALL_BLOCKS, true, None));
}

#[test]
fn a_truncated_file_decodes_the_frames_that_it_holds() {
    let file = silent_file(3, &[]);
    let frame = silent_frame(BLOCKS_PER_FRAME).len();

    // Cut at every length from the start of the data on: whatever the cut, the frames before the
    // cut are decoded and the decoding ends with an error instead of a panic.
    let data_start = file.len() - 2 * frame - silent_frame(FINAL_FRAME_BLOCKS).len();
    for len in data_start..file.len() {
        // What a cut frame decodes to is not defined.
        let (blocks, _, error) = decode(file[..len].to_vec());
        let whole_frames = ((len - data_start) / frame).min(2) as u64;
        assert!(blocks >= whole_frames * BLOCKS_PER_FRAME as u64, "cut at {len}: {blocks}");
        assert!(blocks <= ALL_BLOCKS, "cut at {len}: {blocks}");
        assert!(error.is_some(), "cut at {len}");
    }
}

#[test]
fn parses_the_header_of_the_oldest_format() {
    // Version 3.80: the header, the peak level and the number of seek elements, the seek table, and
    // the seek bit table.
    let mut file = b"MAC ".to_vec();
    file.extend_from_slice(&3800u16.to_le_bytes());
    file.extend_from_slice(&2000u16.to_le_bytes()); // compression level
    file.extend_from_slice(&(0x2u16 | 0x4).to_le_bytes()); // has seek elements, has peak level
    file.extend_from_slice(&2u16.to_le_bytes()); // channels
    file.extend_from_slice(&le32(44100));
    file.extend_from_slice(&le32(0)); // WAV header bytes
    file.extend_from_slice(&le32(0)); // terminating bytes
    file.extend_from_slice(&le32(2)); // total frames
    file.extend_from_slice(&le32(500)); // final frame blocks
    file.extend_from_slice(&le32(0)); // peak level
    file.extend_from_slice(&le32(2)); // seek elements
    file.extend_from_slice(&le32(64));
    file.extend_from_slice(&le32(128));
    file.extend_from_slice(&[0, 0]); // seek bits
    file.extend_from_slice(&[0; 256]);

    let mss =
        MediaSourceStream::new(Box::new(Cursor::new(file)), MediaSourceStreamOptions::default());
    let reader = ApeReader::try_new(mss, FormatOptions::default()).expect("valid header");
    let track = &reader.tracks()[0];
    let Some(CodecParameters::Audio(params)) = &track.codec_params
    else {
        panic!("no audio codec params");
    };

    // The frames of a level below 4000 are 9216 blocks before version 3.90.
    assert_eq!(track.num_frames, Some(9216 + 500));
    assert_eq!(params.sample_rate, Some(44100));
    assert_eq!(params.bits_per_sample, Some(16));
    assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(2));
}
