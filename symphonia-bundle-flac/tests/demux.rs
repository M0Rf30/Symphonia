// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Demuxer tests on small FLAC files: packet counts, timestamps, duration and seeking.
//!
//! The fixtures in `tests/fixtures` are 0.5 s of an 8 kHz stereo sine (4000 frames), encoded with
//! `flac` 1.5 with a fixed block size of 256 samples (15 blocks of 256 and a last one of 160):
//!
//! ```sh
//! ffmpeg -f lavfi -i "aevalsrc=0.5*sin(2*PI*440*t)|0.4*sin(2*PI*660*t):s=8000:d=0.5" \
//!     -c:a pcm_s16le src.wav
//! flac -s --no-seektable -b 256 --no-padding -o flac_noseektable.flac src.wav
//! flac -s -S 4x -b 256 --no-padding -o flac_seektable.flac src.wav
//! ```
//!
//! `flac_seektable.flac` has a SEEKTABLE with 4 points (samples 0, 768, 1792 and 2816).

use symphonia_bundle_flac::{FlacDecoder, FlacReader};
use symphonia_core::audio::GenericAudioBufferRef;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::errors::{Error, SeekErrorKind};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::units::{Duration, Time, Timestamp};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
const TOTAL_FRAMES: u64 = 4000;
const BLOCK: u64 = 256;
const BLOCKS: usize = 16;
const FILES: [&str; 2] = ["flac_noseektable.flac", "flac_seektable.flac"];

fn open(name: &str) -> FlacReader<'static> {
    let file = std::fs::File::open(format!("{FIXTURES}/{name}")).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    FlacReader::try_new(mss, FormatOptions::default()).unwrap()
}

fn seek_ts(reader: &mut FlacReader<'_>, mode: SeekMode, ts: i64) -> Result<(i64, i64), Error> {
    let to = SeekTo::Timestamp { ts: Timestamp::new(ts), track_id: 0 };
    reader.seek(mode, to).map(|s| (s.required_ts.get(), s.actual_ts.get()))
}

/// Decodes the packets from the current position to interleaved `i32` samples.
fn decode_rest(reader: &mut FlacReader<'_>) -> Vec<i32> {
    let track = reader.default_track(TrackType::Audio).unwrap();
    let Some(CodecParameters::Audio(params)) = track.codec_params.clone()
    else {
        panic!("no audio parameters");
    };
    let mut decoder = FlacDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

    let mut samples = Vec::new();
    let mut tmp = Vec::new();
    while let Some(packet) = reader.next_packet().unwrap() {
        let buf: GenericAudioBufferRef<'_> = decoder.decode(&packet).unwrap();
        buf.copy_to_vec_interleaved::<i32>(&mut tmp);
        samples.extend_from_slice(&tmp);
    }
    samples
}

#[test]
fn track_parameters_and_duration() {
    for name in FILES {
        let reader = open(name);
        let tracks = reader.tracks();
        assert_eq!(tracks.len(), 1, "{name}");

        let track = &tracks[0];
        let Some(CodecParameters::Audio(params)) = track.codec_params.as_ref()
        else {
            panic!("{name}: no audio parameters");
        };
        assert_eq!(params.sample_rate, Some(8000), "{name}");
        assert_eq!(params.bits_per_sample, Some(16), "{name}");
        assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(2), "{name}");

        assert_eq!(track.num_frames, Some(TOTAL_FRAMES), "{name}");
        assert_eq!(track.start_ts, Timestamp::ZERO, "{name}");
        let tb = track.time_base.unwrap();
        assert_eq!(tb.calc_time(Timestamp::new(TOTAL_FRAMES as i64)).unwrap().as_secs_f64(), 0.5);
        assert_eq!(reader.media_info().duration, Some(Duration::new(TOTAL_FRAMES)), "{name}");
    }
}

#[test]
fn packets_are_contiguous_blocks() {
    for name in FILES {
        let mut reader = open(name);
        let mut expected_pts = 0u64;
        let mut count = 0;

        while let Some(packet) = reader.next_packet().unwrap() {
            assert_eq!(packet.pts.get() as u64, expected_pts, "{name}: packet {count}");
            let expected_dur = if count + 1 == BLOCKS { TOTAL_FRAMES % BLOCK } else { BLOCK };
            assert_eq!(packet.dur.get(), expected_dur, "{name}: packet {count}");
            assert_eq!(packet.trim_start.get(), 0, "{name}");
            assert_eq!(packet.trim_end.get(), 0, "{name}");
            assert!(!packet.data.is_empty(), "{name}");
            expected_pts += packet.dur.get();
            count += 1;
        }

        assert_eq!(count, BLOCKS, "{name}: number of packets");
        assert_eq!(expected_pts, TOTAL_FRAMES, "{name}: total duration of the packets");
        assert!(reader.next_packet().unwrap().is_none(), "{name}: end of stream is sticky");
    }
}

#[test]
fn decoded_length_matches_the_stream() {
    for name in FILES {
        let mut reader = open(name);
        let samples = decode_rest(&mut reader);
        assert_eq!(samples.len() as u64, TOTAL_FRAMES * 2, "{name}");
        // The samples are scaled to 32 bits: the sine of the source has an amplitude of 0.5.
        let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!((0x3800_0000..=0x4000_0000).contains(&peak), "{name}: peak {peak:#x}");
    }
}

#[test]
fn accurate_seeks_land_on_block_boundaries() {
    // (requested, expected actual): the start of the block that holds the requested sample.
    let cases = [(0, 0), (1, 0), (255, 0), (256, 256), (1000, 768), (2816, 2816), (3999, 3840)];

    for name in FILES {
        let mut reader = open(name);
        for (required, actual) in cases {
            let seeked = seek_ts(&mut reader, SeekMode::Accurate, required).unwrap();
            assert_eq!(seeked, (required, actual), "{name}: seek to {required}");

            let packet = reader.next_packet().unwrap().unwrap();
            assert_eq!(packet.pts.get(), actual, "{name}: first packet after seek to {required}");
        }
    }
}

#[test]
fn seeks_in_both_directions_and_after_eof() {
    for name in FILES {
        let mut reader = open(name);
        while reader.next_packet().unwrap().is_some() {}

        // A seek backwards after the end of the stream restarts it.
        assert_eq!(seek_ts(&mut reader, SeekMode::Accurate, 2000).unwrap(), (2000, 1792));
        assert_eq!(reader.next_packet().unwrap().unwrap().pts.get(), 1792, "{name}");
        assert_eq!(seek_ts(&mut reader, SeekMode::Accurate, 3000).unwrap(), (3000, 2816));
        assert_eq!(seek_ts(&mut reader, SeekMode::Accurate, 100).unwrap(), (100, 0));

        let remaining = std::iter::from_fn(|| reader.next_packet().unwrap()).count();
        assert_eq!(remaining, BLOCKS, "{name}");
    }
}

#[test]
fn time_seek_uses_the_sample_rate() {
    for name in FILES {
        let mut reader = open(name);
        // 0.25 s is sample 2000, in the block starting at 1792.
        let to = SeekTo::Time { time: Time::try_from_secs_f64(0.25).unwrap(), track_id: None };
        let seeked = reader.seek(SeekMode::Accurate, to).unwrap();
        assert_eq!((seeked.required_ts.get(), seeked.actual_ts.get()), (2000, 1792), "{name}");
        assert_eq!(reader.next_packet().unwrap().unwrap().pts.get(), 1792, "{name}");
    }
}

#[test]
fn seeking_beyond_the_end_is_out_of_range() {
    for name in FILES {
        let mut reader = open(name);
        let err = seek_ts(&mut reader, SeekMode::Accurate, TOTAL_FRAMES as i64 + 1).unwrap_err();
        assert!(matches!(err, Error::SeekError(SeekErrorKind::OutOfRange)), "{name}: {err:?}");

        // The failed seek leaves the stream readable.
        assert_eq!(reader.next_packet().unwrap().unwrap().pts.get(), 0, "{name}");
    }
}

#[test]
fn decoding_after_a_seek_is_the_tail_of_the_stream() {
    for name in FILES {
        let full = decode_rest(&mut open(name));

        for target in [300, 1792, 3500] {
            let mut reader = open(name);
            let (_, actual) = seek_ts(&mut reader, SeekMode::Accurate, target).unwrap();
            let tail = decode_rest(&mut reader);
            assert_eq!(tail, full[actual as usize * 2..], "{name}: after a seek to {target}");
        }
    }
}

#[test]
fn seek_table_and_scan_agree() {
    // The two files hold the same audio: the seek point is not visible to the caller.
    let a = decode_rest(&mut open(FILES[0]));
    let b = decode_rest(&mut open(FILES[1]));
    assert_eq!(a, b);
}

#[test]
fn reset_after_a_seek_does_not_report_a_false_md5_mismatch() {
    let mut reader = open(FILES[0]);
    let track = reader.default_track(TrackType::Audio).unwrap();
    let Some(CodecParameters::Audio(params)) = track.codec_params.clone()
    else {
        panic!("no audio parameters");
    };
    let mut opts = AudioDecoderOptions::default();
    opts.verify = true;
    let mut decoder = FlacDecoder::try_new(&params, &opts).unwrap();

    // Decode a few blocks, then skip ahead as a seek would, resetting the decoder.
    for _ in 0..4 {
        let packet = reader.next_packet().unwrap().unwrap();
        decoder.decode(&packet).unwrap();
    }
    seek_ts(&mut reader, SeekMode::Accurate, 10 * BLOCK as i64).unwrap();
    decoder.reset();
    while let Some(packet) = reader.next_packet().unwrap() {
        decoder.decode(&packet).unwrap();
    }

    assert_ne!(decoder.finalize().verify_ok, Some(false));
}
