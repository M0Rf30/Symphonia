// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Demuxer tests on small CAF files made by FFmpeg 8: packet counts, timestamps, trimming,
//! duration and seeking.
//!
//! The fixtures in `tests/fixtures` are made from a sine (the stereo ones from
//! `aevalsrc=0.5*sin(2*PI*440*t)|0.4*sin(2*PI*660*t)` at 8 kHz for 0.5 s, the mono one from the
//! 440 Hz sine at 8 kHz for 1.2 s):
//!
//! ```sh
//! ffmpeg -bitexact -i src_8k_st.wav -c:a pcm_s16be pcm_s16be_st_8k.caf
//! ffmpeg -bitexact -i src_8k_st.wav -c:a adpcm_ima_qt ima4_st_8k.caf
//! ffmpeg -bitexact -i src_8k_mono_long.wav -c:a alac alac_mono_8k.caf
//! ```
//!
//! * `pcm_s16be_st_8k.caf`: 4000 stereo frames of linear PCM, which are packetized in packets of
//!   at most 1152 frames.
//! * `ima4_st_8k.caf`: 63 packets of 64 frames of 68 bytes, with 4000 valid frames (the 32
//!   frames after them are the remainder of the last packet).
//! * `alac_mono_8k.caf`: 3 variable size packets of 4096 frames, 9600 valid frames (the 2688
//!   frames after them are the remainder of the last packet).

use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_ADPCM_IMA_QT, CODEC_ID_ALAC, CODEC_ID_PCM_S16BE,
};
use symphonia_core::errors::{Error, SeekErrorKind};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::units::{Time, Timestamp};
use symphonia_format_caf::CafReader;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn open(name: &str) -> CafReader<'static> {
    let file = std::fs::File::open(format!("{FIXTURES}/{name}")).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    CafReader::try_new(mss, FormatOptions::default()).unwrap()
}

/// A packet: `(pts, dur, trim_start, trim_end, bytes)`.
type Pkt = (i64, u64, u64, u64, usize);

fn read_all(reader: &mut CafReader<'_>) -> Vec<Pkt> {
    std::iter::from_fn(|| reader.next_packet().unwrap())
        .map(|p| (p.pts.get(), p.dur.get(), p.trim_start.get(), p.trim_end.get(), p.data.len()))
        .collect()
}

fn seek(reader: &mut CafReader<'_>, ts: i64) -> Result<(i64, i64), Error> {
    let to = SeekTo::Timestamp { ts: Timestamp::new(ts), track_id: 0 };
    reader.seek(SeekMode::Accurate, to).map(|s| (s.required_ts.get(), s.actual_ts.get()))
}

fn is_out_of_range(result: Result<(i64, i64), Error>) -> bool {
    matches!(result, Err(Error::SeekError(SeekErrorKind::OutOfRange)))
}

#[test]
fn linear_pcm_is_split_into_packets_of_1152_frames() {
    let mut reader = open("pcm_s16be_st_8k.caf");

    let track = &reader.tracks()[0];
    let params = track.codec_params.as_ref().unwrap().audio().unwrap();
    assert_eq!(params.codec, CODEC_ID_PCM_S16BE);
    assert_eq!(params.sample_rate, Some(8000));
    assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(2));
    assert_eq!(params.bits_per_sample, Some(16));
    assert_eq!(track.num_frames, Some(4000));
    assert_eq!((track.delay, track.padding), (None, None));
    let tb = track.time_base.unwrap();
    assert_eq!(tb.calc_time(Timestamp::new(4000)).unwrap().as_secs_f64(), 0.5);
    assert_eq!(reader.media_info().duration.map(|d| d.get()), Some(4000));

    // 4 bytes per frame.
    assert_eq!(
        read_all(&mut reader),
        [
            (0, 1152, 0, 0, 4608),
            (1152, 1152, 0, 0, 4608),
            (2304, 1152, 0, 0, 4608),
            (3456, 544, 0, 0, 2176)
        ]
    );
    assert!(reader.next_packet().unwrap().is_none());
}

#[test]
fn linear_pcm_seeks_to_packet_boundaries() {
    let mut reader = open("pcm_s16be_st_8k.caf");

    for (required, actual) in [(0, 0), (1151, 0), (1152, 1152), (2000, 1152), (3999, 3456)] {
        assert_eq!(seek(&mut reader, required).unwrap(), (required, actual));
        let packet = reader.next_packet().unwrap().unwrap();
        assert_eq!(packet.pts.get(), actual, "after a seek to {required}");
    }

    // After the end of the stream.
    while reader.next_packet().unwrap().is_some() {}
    assert_eq!(seek(&mut reader, 1200).unwrap(), (1200, 1152));
    assert_eq!(read_all(&mut reader).len(), 3);

    // By time: 0.25 s is frame 2000.
    let time = Time::try_from_secs_f64(0.25).unwrap();
    let seeked = reader.seek(SeekMode::Accurate, SeekTo::Time { time, track_id: None }).unwrap();
    assert_eq!((seeked.required_ts.get(), seeked.actual_ts.get()), (2000, 1152));

    assert!(is_out_of_range(seek(&mut reader, -1)));
    assert!(is_out_of_range(seek(&mut reader, 4001)));
}

#[test]
fn ima4_packets_are_constant_size_and_the_last_one_is_trimmed() {
    let mut reader = open("ima4_st_8k.caf");

    let track = &reader.tracks()[0];
    let params = track.codec_params.as_ref().unwrap().audio().unwrap();
    assert_eq!(params.codec, CODEC_ID_ADPCM_IMA_QT);
    assert_eq!(params.sample_rate, Some(8000));
    assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(2));
    assert_eq!(track.num_frames, Some(4000));
    assert_eq!((track.delay, track.padding), (None, Some(32)));

    let packets = read_all(&mut reader);
    assert_eq!(packets.len(), 63);

    for (n, &(pts, dur, trim_start, trim_end, bytes)) in packets.iter().enumerate() {
        assert_eq!(pts, 64 * n as i64, "packet {n}");
        assert_eq!(bytes, 68, "packet {n}");
        assert_eq!(trim_start, 0, "packet {n}");
        if n < 62 {
            assert_eq!((dur, trim_end), (64, 0), "packet {n}");
        }
        else {
            // The remainder of the last packet is trimmed.
            assert_eq!((dur, trim_end), (32, 32), "packet {n}");
        }
    }
    assert_eq!(packets.iter().map(|p| p.1).sum::<u64>(), 4000);

    // Seeks align down to a packet (64 frames); the last valid frame is in packet 62.
    assert_eq!(seek(&mut reader, 1000).unwrap(), (1000, 960));
    assert_eq!(reader.next_packet().unwrap().unwrap().pts.get(), 960);
    assert_eq!(seek(&mut reader, 3999).unwrap(), (3999, 3968));
    assert_eq!(read_all(&mut reader), [(3968, 32, 0, 32, 68)]);
    assert!(is_out_of_range(seek(&mut reader, 4033)));
}

#[test]
fn alac_packets_are_variable_size_and_the_last_one_is_trimmed() {
    let mut reader = open("alac_mono_8k.caf");

    let track = &reader.tracks()[0];
    let params = track.codec_params.as_ref().unwrap().audio().unwrap();
    assert_eq!(params.codec, CODEC_ID_ALAC);
    assert_eq!(params.sample_rate, Some(8000));
    assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(1));
    assert_eq!(track.num_frames, Some(9600));
    assert_eq!((track.delay, track.padding), (None, Some(2688)));
    assert_eq!(reader.media_info().duration.map(|d| d.get()), Some(9600));

    let packets = read_all(&mut reader);
    assert_eq!(packets.len(), 3);
    assert_eq!(
        packets.iter().map(|p| (p.0, p.1, p.2, p.3)).collect::<Vec<_>>(),
        [(0, 4096, 0, 0), (4096, 4096, 0, 0), (8192, 1408, 0, 2688),]
    );
    // The packet table gives every packet its own size.
    assert!(packets.iter().all(|p| p.4 > 0));
    assert!(packets.windows(2).any(|w| w[0].4 != w[1].4));

    // Seeks use the packet table. The last valid frame is in the third packet.
    assert_eq!(seek(&mut reader, 5000).unwrap(), (5000, 4096));
    assert_eq!(read_all(&mut reader).iter().map(|p| p.0).collect::<Vec<_>>(), [4096, 8192]);
    assert_eq!(seek(&mut reader, 9599).unwrap(), (9599, 8192));
    assert_eq!(seek(&mut reader, 0).unwrap(), (0, 0));
    assert_eq!(read_all(&mut reader), packets);
    assert!(is_out_of_range(seek(&mut reader, 12289)));
}
