// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Demuxer tests on small ISO/MP4 files made by FFmpeg 8: packet counts, timestamps, gapless
//! trimming, duration and seeking. The packet counts and the packet durations agree with
//! `ffprobe -show_packets`.
//!
//! The fixtures in `tests/fixtures` are made from sines (the stereo ones from
//! `aevalsrc=0.5*sin(2*PI*440*t)|0.4*sin(2*PI*660*t)`, the mono one from the 440 Hz sine):
//!
//! ```sh
//! # src_44k.wav: 0.5 s at 44.1 kHz, stereo. src_8k_st.wav: 0.5 s at 8 kHz, stereo.
//! # src_8k_mono_long.wav: 1.2 s at 8 kHz, mono.
//! ffmpeg -bitexact -i src_44k.wav -c:a aac -b:a 96k aac_st_44k.m4a
//! ffmpeg -bitexact -i src_8k_mono_long.wav -c:a alac alac_mono_8k.m4a
//! ffmpeg -bitexact -i src_8k_st.wav -c:a pcm_s16le -f mov pcm_st_8k.mov
//! ffmpeg -bitexact -i src_44k.wav -c:a aac -b:a 96k \
//!     -movflags +frag_keyframe+empty_moov+default_base_moof frag_aac_st_44k.mp4
//! ```
//!
//! * `aac_st_44k.m4a`: 23 AAC-LC packets of 1024 frames, 22050 valid frames. An edit list skips
//!   the 1024 priming frames, and the last packet has 478 frames of padding.
//! * `alac_mono_8k.m4a`: 3 packets of 4096, 4096 and 1408 frames.
//! * `pcm_st_8k.mov`: 4000 frames of 16-bit little-endian stereo PCM in one sample.
//! * `frag_aac_st_44k.mp4`: the AAC stream as a fragmented file with an empty `moov`.

use symphonia_core::codecs::audio::well_known::{CODEC_ID_AAC, CODEC_ID_ALAC, CODEC_ID_PCM_S16LE};
use symphonia_core::errors::{Error, SeekErrorKind};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::units::{Time, Timestamp};
use symphonia_format_isomp4::IsoMp4Reader;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn open(name: &str) -> IsoMp4Reader<'static> {
    let file = std::fs::File::open(format!("{FIXTURES}/{name}")).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    IsoMp4Reader::try_new(mss, FormatOptions::default()).unwrap()
}

/// A packet: `(pts, dur, trim_start, trim_end, bytes)`.
type Pkt = (i64, u64, u64, u64, usize);

fn read_all(reader: &mut IsoMp4Reader<'_>) -> Vec<Pkt> {
    std::iter::from_fn(|| reader.next_packet().unwrap())
        .map(|p| (p.pts.get(), p.dur.get(), p.trim_start.get(), p.trim_end.get(), p.data.len()))
        .collect()
}

fn seek(reader: &mut IsoMp4Reader<'_>, ts: i64) -> Result<(i64, i64), Error> {
    let track_id = reader.tracks()[0].id;
    let to = SeekTo::Timestamp { ts: Timestamp::new(ts), track_id };
    reader.seek(SeekMode::Accurate, to).map(|s| (s.required_ts.get(), s.actual_ts.get()))
}

fn is_out_of_range(result: Result<(i64, i64), Error>) -> bool {
    matches!(result, Err(Error::SeekError(SeekErrorKind::OutOfRange)))
}

#[test]
fn aac_track_has_the_edit_list_as_delay() {
    let reader = open("aac_st_44k.m4a");
    assert_eq!(reader.format_info().short_name, "isomp4");
    assert_eq!(reader.tracks().len(), 1);

    let track = &reader.tracks()[0];
    let params = track.codec_params.as_ref().unwrap().audio().unwrap();
    assert_eq!(params.codec, CODEC_ID_AAC);
    assert_eq!(params.sample_rate, Some(44100));
    assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(2));

    assert_eq!(track.time_base.unwrap().calc_time(Timestamp::new(44100)).unwrap().as_secs(), 1);
    assert_eq!(track.start_ts, Timestamp::ZERO);
    assert_eq!(track.delay, Some(1024));
    assert_eq!(track.num_frames, Some(22050));
    assert_eq!(reader.media_info().duration.map(|d| d.get()), Some(22050));
}

#[test]
fn aac_packets_cover_the_priming_valid_and_padding_frames() {
    let mut reader = open("aac_st_44k.m4a");
    let packets = read_all(&mut reader);
    assert_eq!(packets.len(), 23);

    // The first packet is the priming: it starts before zero and is trimmed entirely.
    assert_eq!((packets[0].0, packets[0].1, packets[0].2, packets[0].3), (-1024, 0, 1024, 0));

    // The next ones are 1024 frames each, in order.
    for (n, p) in packets[1..22].iter().enumerate() {
        assert_eq!((p.0, p.1, p.2, p.3), (n as i64 * 1024, 1024, 0, 0), "packet {}", n + 1);
    }

    // The last one has the 478 frames of padding trimmed.
    assert_eq!((packets[22].0, packets[22].1, packets[22].2, packets[22].3), (21504, 546, 0, 478));

    // The valid frames add up to the duration, the trimmed ones to the packets of 1024 frames.
    assert_eq!(packets.iter().map(|p| p.1).sum::<u64>(), 22050);
    assert_eq!(packets.iter().map(|p| p.1 + p.2 + p.3).sum::<u64>(), 23 * 1024);
    assert!(packets.iter().all(|p| p.4 > 0));
    assert!(reader.next_packet().unwrap().is_none());
}

#[test]
fn aac_seeks_land_one_packet_before_for_the_pre_roll() {
    let mut reader = open("aac_st_44k.m4a");

    // The seek goes to the start of the packet before the one with the required timestamp, so
    // that the decoder is primed.
    for (required, actual) in
        [(0, -1024), (1023, -1024), (1024, 0), (5000, 3072), (8000, 6144), (22049, 20480)]
    {
        assert_eq!(seek(&mut reader, required).unwrap(), (required, actual));
        let packet = reader.next_packet().unwrap().unwrap();
        assert_eq!(packet.pts.get(), actual, "after a seek to {required}");
    }

    // The stream is read to its end after a seek.
    assert_eq!(read_all(&mut reader).last().unwrap().0, 21504);

    // By time: 0.1 s is frame 4410.
    let time = Time::try_from_secs_f64(0.1).unwrap();
    let to = SeekTo::Time { time, track_id: None };
    let seeked = reader.seek(SeekMode::Accurate, to).unwrap();
    assert_eq!((seeked.required_ts.get(), seeked.actual_ts.get()), (4410, 3072));

    // Back to the start after the end of the stream.
    assert_eq!(seek(&mut reader, 0).unwrap(), (0, -1024));
    assert_eq!(read_all(&mut reader).len(), 23);

    assert!(is_out_of_range(seek(&mut reader, -1024)));
    assert!(is_out_of_range(seek(&mut reader, 22050)));
    assert!(is_out_of_range(seek(&mut reader, 30000)));
}

#[test]
fn alac_packets_and_seeks() {
    let mut reader = open("alac_mono_8k.m4a");

    let track = &reader.tracks()[0];
    let params = track.codec_params.as_ref().unwrap().audio().unwrap();
    assert_eq!(params.codec, CODEC_ID_ALAC);
    assert_eq!(params.sample_rate, Some(8000));
    assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(1));
    assert_eq!(track.num_frames, Some(9600));
    assert_eq!(reader.media_info().duration.map(|d| d.get()), Some(9600));

    let packets = read_all(&mut reader);
    assert_eq!(
        packets.iter().map(|p| (p.0, p.1, p.2, p.3)).collect::<Vec<_>>(),
        [(0, 4096, 0, 0), (4096, 4096, 0, 0), (8192, 1408, 0, 0),]
    );
    assert!(packets.iter().all(|p| p.4 > 0));

    // ALAC packets are independent: a seek goes to the packet that holds the timestamp.
    for (required, actual) in [(0, 0), (4095, 0), (4096, 4096), (5000, 4096), (9599, 8192)] {
        assert_eq!(seek(&mut reader, required).unwrap(), (required, actual));
        assert_eq!(reader.next_packet().unwrap().unwrap().pts.get(), actual);
    }
    assert!(is_out_of_range(seek(&mut reader, 9601)));
}

#[test]
fn pcm_is_a_single_sample() {
    let mut reader = open("pcm_st_8k.mov");

    let track = &reader.tracks()[0];
    let params = track.codec_params.as_ref().unwrap().audio().unwrap();
    assert_eq!(params.codec, CODEC_ID_PCM_S16LE);
    assert_eq!(params.sample_rate, Some(8000));
    assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(2));
    assert_eq!(params.bits_per_sample, Some(16));
    assert_eq!(track.num_frames, Some(4000));
    assert_eq!(reader.media_info().duration.map(|d| d.get()), Some(4000));

    // 4 bytes per frame.
    assert_eq!(read_all(&mut reader), [(0, 4000, 0, 0, 16000)]);

    assert_eq!(seek(&mut reader, 3999).unwrap(), (3999, 0));
    assert_eq!(read_all(&mut reader).len(), 1);
    assert!(is_out_of_range(seek(&mut reader, 5000)));
}

#[test]
fn fragmented_file_has_the_same_packets() {
    let mut reader = open("frag_aac_st_44k.mp4");
    assert_eq!(reader.tracks().len(), 1);

    let packets = read_all(&mut reader);
    assert_eq!(packets.len(), 23);
    for (n, p) in packets[..22].iter().enumerate() {
        assert_eq!((p.0, p.1, p.2, p.3), (n as i64 * 1024, 1024, 0, 0), "packet {n}");
    }
    assert_eq!((packets[22].0, packets[22].1), (22528, 546));
    assert_eq!(packets.iter().map(|p| p.1).sum::<u64>(), 22 * 1024 + 546);
    // The packets are the ones of the file without fragments (the encoder is deterministic).
    let mut plain = open("aac_st_44k.m4a");
    let plain: Vec<usize> = read_all(&mut plain).iter().map(|p| p.4).collect();
    assert_eq!(packets.iter().map(|p| p.4).collect::<Vec<_>>(), plain);

    // Seeks use the packets of the fragments.
    for (required, actual) in [(0, 0), (1023, 0), (1024, 0), (5000, 3072), (22049, 20480)] {
        assert_eq!(seek(&mut reader, required).unwrap(), (required, actual));
        assert_eq!(reader.next_packet().unwrap().unwrap().pts.get(), actual);
    }
}
