// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! End-to-end tests of the ADX reader and decoder.
//!
//! The decoder is an integer decoder, so its output is compared with FFmpeg's `adx` demuxer and
//! `adpcm_adx` decoder bit for bit. The fixtures in `tests/data` were encoded with FFmpeg's
//! `adpcm_adx` encoder from synthetic audio, and their decoded samples are pinned by hash
//! (verified equal to FFmpeg's output when they were made). The tests that run `ffmpeg` are
//! skipped when it is not on the `PATH`.

mod common;

use symphonia_core::codecs::CodecParameters;
use symphonia_core::formats::probe::{Hint, Probe};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::meta::MetadataOptions;
use symphonia_core::units::Timestamp;
use symphonia_format_game::AdxReader;
use symphonia_testutil::XorShift64;

use common::{Decoded, check_hash, decode, ffmpeg_decode, mss, to_s16le};

const STEREO: &[u8] = include_bytes!("data/stereo_44k.adx");
const MONO: &[u8] = include_bytes!("data/mono_22k.adx");

fn reader(data: &[u8]) -> AdxReader<'static> {
    AdxReader::try_new(mss(data), FormatOptions::default()).unwrap()
}

/// An ADX stream of random blocks: the scale of a block is below 0x800.
fn random_stream(
    channels: u8,
    rate: u32,
    cutoff: u16,
    total: u32,
    groups: usize,
    end_group: Option<usize>,
    seed: u64,
) -> Vec<u8> {
    let mut h = vec![0u8; 0x24];
    h[0] = 0x80;
    h[3] = 0x20;
    h[4..8].copy_from_slice(&[3, 18, 4, channels]);
    h[8..12].copy_from_slice(&rate.to_be_bytes());
    h[12..16].copy_from_slice(&total.to_be_bytes());
    h[16..18].copy_from_slice(&cutoff.to_be_bytes());
    h[0x12] = 3;
    h[0x1e..].copy_from_slice(b"(c)CRI");

    let mut rng = XorShift64(seed);
    for g in 0..groups {
        for _ in 0..channels {
            let mut block = [0u8; 18];
            for b in &mut block[2..] {
                *b = rng.next_u64() as u8;
            }
            let scale = rng.below(0x800) as u16;
            block[..2].copy_from_slice(&scale.to_be_bytes());
            // An end of audio block.
            if end_group == Some(g) {
                block[0] |= 0x80;
            }
            h.extend_from_slice(&block);
        }
    }

    h
}

fn check_params(reader: &dyn FormatReader, rate: u32, channels: usize, frames: u64) {
    let track = &reader.tracks()[0];
    let Some(CodecParameters::Audio(params)) = &track.codec_params
    else {
        panic!("no audio parameters");
    };

    assert_eq!(params.sample_rate, Some(rate));
    assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(channels));
    assert_eq!(params.extra_data.as_ref().map(|d| d.len()), Some(0x24));
    assert_eq!(track.num_frames, Some(frames));
    assert_eq!(track.delay, None);
    assert_eq!(track.padding, None);
    assert_eq!(reader.media_info().duration.map(|d| d.get()), Some(frames));
}

#[test]
fn stereo_44k() {
    let mut r = reader(STEREO);
    check_params(&r, 44100, 2, 33088);

    let d = decode(&mut r, false);
    assert_eq!((d.rate, d.channels, d.packets), (44100, 2, 9));
    assert_eq!(d.samples.len(), 33088 * 2);
    check_hash("stereo", &d.samples, 0xaf916b0b2796516a);

    if let Some(reference) = ffmpeg_decode(STEREO, "adx", "s16le") {
        assert_eq!(to_s16le(&d.samples), reference);
    }
}

#[test]
fn mono_22k() {
    let mut r = reader(MONO);
    check_params(&r, 22050, 1, 11040);

    let d = decode(&mut r, false);
    assert_eq!((d.rate, d.channels, d.packets), (22050, 1, 3));
    check_hash("mono", &d.samples, 0x644fa5c65973d05a);

    if let Some(reference) = ffmpeg_decode(MONO, "adx", "s16le") {
        assert_eq!(to_s16le(&d.samples), reference);
    }
}

#[test]
fn probes_without_a_hint() {
    let probe = {
        let mut probe = Probe::new();
        probe.register_format::<AdxReader<'_>>();
        probe
    };

    let reader = probe
        .probe(&Hint::new(), mss(STEREO), FormatOptions::default(), MetadataOptions::default())
        .unwrap();
    assert_eq!(reader.format_info().short_name, "adx");
}

/// Streams of random blocks decode bit for bit like FFmpeg: all supported channel counts, and
/// filters of other cutoff frequencies and sample rates.
#[test]
fn random_streams_match_ffmpeg() {
    let cases = [(1, 32000, 500), (2, 48000, 1000), (3, 8000, 100), (4, 44100, 0), (6, 22050, 500)];

    for (n, &(channels, rate, cutoff)) in cases.iter().enumerate() {
        let data = random_stream(channels, rate, cutoff, 0, 300, None, 0x1234 + n as u64);
        let d = decode(&mut reader(&data), false);
        assert_eq!(d.samples.len(), 300 * 32 * usize::from(channels));

        if let Some(reference) = ffmpeg_decode(&data, "adx", "s16le") {
            assert_eq!(to_s16le(&d.samples), reference, "{channels} channels, {rate} Hz");
        }
    }
}

/// FFmpeg decodes up to the first group with an end of audio block.
#[test]
fn end_block_stops_like_ffmpeg() {
    for channels in [1, 2, 3] {
        let data = random_stream(channels, 44100, 500, 0, 300, Some(150), 99);
        let d = decode(&mut reader(&data), false);
        assert_eq!(d.samples.len(), 150 * 32 * usize::from(channels));

        if let Some(reference) = ffmpeg_decode(&data, "adx", "s16le") {
            assert_eq!(to_s16le(&d.samples), reference);
        }
    }
}

/// The samples after the total of the header are trimmed when decoding gaplessly.
#[test]
fn total_samples_trims_gapless() {
    let data = random_stream(2, 44100, 500, 5000, 200, None, 7);
    let r = reader(&data);
    assert_eq!(r.tracks()[0].num_frames, Some(5000));
    assert_eq!(r.tracks()[0].padding, Some(200 * 32 - 5000));

    let full = decode(&mut reader(&data), false);
    assert_eq!(full.samples.len(), 200 * 32 * 2);

    let trimmed = decode(&mut reader(&data), true);
    assert_eq!(trimmed.samples.len(), 5000 * 2);
    assert_eq!(trimmed.samples[..], full.samples[..5000 * 2]);
}

/// A seek followed by decoding gives the samples of a decode from the start, to within the
/// rounding error of the codec's filter, whose state is not in the stream and is rebuilt from a
/// preroll. A seek to the start is exact.
#[test]
fn seek_matches_continuous_decode() {
    let full: Decoded = decode(&mut reader(STEREO), false);

    for required in [0u64, 31, 4096, 20007, 33087] {
        let mut r = reader(STEREO);
        let seeked = r
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp { ts: Timestamp::new(required as i64), track_id: 0 },
            )
            .unwrap();
        assert!(seeked.actual_ts.get() as u64 <= required);
        assert_eq!(seeked.required_ts.get() as u64, required);

        let rest = decode(&mut r, false);
        let skip = (required - seeked.actual_ts.get() as u64) as usize * 2;
        let from = required as usize * 2;
        let (got, want) = (&rest.samples[skip..], &full.samples[from..]);
        assert_eq!(got.len(), want.len(), "seek to {required}");

        let worst = got.iter().zip(want).map(|(a, b)| (i32::from(*a) - i32::from(*b)).abs()).max();
        assert!(worst.unwrap_or(0) <= 16, "seek to {required}: {worst:?}");

        if seeked.actual_ts.get() == 0 {
            assert_eq!(got, want, "seek to {required}");
        }
    }

    // Seeking past the end is an error.
    let mut r = reader(STEREO);
    let past = SeekTo::Timestamp { ts: Timestamp::new(33088), track_id: 0 };
    assert!(r.seek(SeekMode::Accurate, past).is_err());
}
