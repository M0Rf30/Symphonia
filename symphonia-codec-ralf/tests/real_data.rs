// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Decoding of real RALF data against the output of FFmpeg's `ralf` decoder.

mod common;

use common::*;
use symphonia_codec_ralf::RalfDecoder;
use symphonia_core::codecs::audio::well_known::CODEC_ID_RALF;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};

/// 9 independent units (10 packets, 1 split frame) of a RealAudio 10 Lossless file: stereo,
/// 44.1 kHz. They cover all four stereo modes, all block lengths, and 8 of the filter precisions
/// of the sample's own 1821 filtered channel blocks. The file is
/// `Wonderful_RA10_Lossless_706K_30s.rm` of <https://samples.ffmpeg.org/real/AC-ralf/>.
const FIXTURE: &[u8] = include_bytes!("data/ralf_stereo.rm");

/// The number of frames and the FNV-1a hash of the interleaved little-endian output of
/// `ffmpeg -i ralf_stereo.rm -f s16le -`, FFmpeg 8.
const FIXTURE_FRAMES: usize = 29120;
const FIXTURE_FNV: u64 = 0xc8de5d2a5cc1707d;

#[test]
fn fixture_matches_ffmpeg() {
    let stream = read_rm(FIXTURE);

    assert_eq!(stream.extra_data.len(), 24);
    assert_eq!(&stream.extra_data[..4], b"LSD:");

    let mut samples = Vec::new();
    let mut frames = 0;

    for (i, d) in decode_packets(&stream.extra_data, &stream.packets).into_iter().enumerate() {
        let d = d.unwrap_or_else(|| panic!("packet {i} failed to decode"));

        // The first half of a split frame has no output; the second has all of it.
        assert_eq!(d.frames == 0, stream.packets[i].1.len() == 8192, "packet {i}");

        frames += d.frames;
        samples.extend(d.samples);
    }

    assert_eq!(frames, FIXTURE_FRAMES);
    assert_eq!(samples.len(), 2 * FIXTURE_FRAMES);
    assert_eq!(fnv_s16le(&samples), FIXTURE_FNV);
}

#[test]
fn fixture_has_split_frames() {
    let stream = read_rm(FIXTURE);
    let halves = stream.packets.iter().filter(|p| p.1.len() == 8192).count();
    assert_eq!(halves, 1);
}

#[test]
fn reset_drops_the_first_half_of_a_split_frame() {
    let stream = read_rm(FIXTURE);
    let first = stream.packets.iter().position(|p| p.1.len() == 8192).unwrap();

    let mut dec = decoder(&stream.extra_data);

    // The first half, then a reset (as after a seek) in place of the second half.
    let (ts, payload) = &stream.packets[first];
    assert_eq!(decode_one(&mut dec, *ts, payload).unwrap().frames, 0);
    dec.reset();

    // The second half alone is not a frame: it is rejected, and must not be completed with
    // the dropped first half.
    let (ts, payload) = &stream.packets[first + 1];
    assert!(decode_one(&mut dec, *ts, payload).is_none());

    // The decoder still works.
    let (ts, payload) = &stream.packets[first + 2];
    assert!(decode_one(&mut dec, *ts, payload).unwrap().frames > 0);
}

#[test]
fn codec_parameters_come_from_the_extra_data() {
    let stream = read_rm(FIXTURE);

    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_RALF)
        .with_sample_rate(8000)
        .with_extra_data(stream.extra_data.clone().into_boxed_slice());

    let dec = RalfDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

    assert_eq!(dec.codec_params().sample_rate, Some(44100));
    assert_eq!(dec.codec_params().channels.as_ref().map(|c| c.count()), Some(2));
    // The frame size is that of the header, raised to one second.
    assert_eq!(dec.codec_params().max_frames_per_packet, Some(44100));
}

#[test]
fn invalid_extra_data_is_rejected() {
    let stream = read_rm(FIXTURE);
    let good = stream.extra_data;

    let try_new = |extra_data: Option<Vec<u8>>| {
        let mut params = AudioCodecParameters::new();
        params.for_codec(CODEC_ID_RALF);

        if let Some(extra_data) = extra_data {
            params.with_extra_data(extra_data.into_boxed_slice());
        }

        RalfDecoder::try_new(&params, &AudioDecoderOptions::default()).is_ok()
    };

    assert!(try_new(Some(good.clone())));
    assert!(!try_new(None));
    // Too short.
    assert!(!try_new(Some(good[..23].to_vec())));

    let patched = |at: usize, bytes: &[u8]| {
        let mut d = good.clone();
        d[at..at + bytes.len()].copy_from_slice(bytes);
        Some(d)
    };

    // Not an `LSD:` header, or an unknown version.
    assert!(!try_new(patched(0, b"LSE:")));
    assert!(!try_new(patched(4, &[1, 4])));
    // The channel count must be 1 or 2.
    assert!(try_new(patched(8, &[0, 1])));
    assert!(!try_new(patched(8, &[0, 0])));
    assert!(!try_new(patched(8, &[0, 3])));
    // The sample rate must be 8000 to 96000 Hz.
    assert!(try_new(patched(12, &8000u32.to_be_bytes())));
    assert!(try_new(patched(12, &96000u32.to_be_bytes())));
    assert!(!try_new(patched(12, &7999u32.to_be_bytes())));
    assert!(!try_new(patched(12, &96001u32.to_be_bytes())));
    // The frame size must be 1 to 2^20.
    assert!(try_new(patched(16, &1u32.to_be_bytes())));
    assert!(try_new(patched(16, &(1u32 << 20).to_be_bytes())));
    assert!(!try_new(patched(16, &0u32.to_be_bytes())));
    assert!(!try_new(patched(16, &((1u32 << 20) + 1).to_be_bytes())));
}

#[test]
fn malformed_packets_are_rejected() {
    let stream = read_rm(FIXTURE);
    let mut dec = decoder(&stream.extra_data);

    // Shorter than 5 bytes.
    assert!(decode_one(&mut dec, 0, &[]).is_none());
    assert!(decode_one(&mut dec, 0, &[0, 8, 0, 0]).is_none());
    // A block table longer than the packet.
    assert!(decode_one(&mut dec, 0, &[0xff, 0xff, 0, 0, 0, 0]).is_none());
    // The first block is larger than the packet. (A packet that only loses its last block still
    // decodes, as in FFmpeg, which outputs as many blocks as it can.)
    let (ts, payload) = &stream.packets[0];
    let table_bytes = (usize::from(u16::from_be_bytes([payload[0], payload[1]])) + 7) >> 3;
    assert!(decode_one(&mut dec, *ts, &payload[..table_bytes + 3]).is_none());
    // A table of no blocks is not an error, but has no output.
    assert_eq!(decode_one(&mut dec, 0, &[0, 0, 0, 0, 0]).unwrap().frames, 0);

    // The decoder is not left in a bad state.
    assert!(decode_one(&mut dec, *ts, payload).unwrap().frames > 0);
}
