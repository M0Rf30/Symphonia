// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests with synthetic FLV files.

mod support;

use std::io::Cursor;

use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_AAC, CODEC_ID_MP2, CODEC_ID_PCM_ALAW, CODEC_ID_PCM_MULAW, CODEC_ID_PCM_S16BE,
    CODEC_ID_PCM_S16LE, CODEC_ID_PCM_U8,
};
use symphonia_core::errors::Error;
use symphonia_core::formats::probe::{Hint, Probe, Score, Scoreable};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackFlags};
use symphonia_core::io::{MediaSourceStream, ReadOnlySource, ScopedStream};
use symphonia_core::meta::MetadataOptions;
use symphonia_core::units::Timestamp;
use symphonia_format_flv::FlvReader;

use support::*;

/// AAC-LC, 44.1 kHz, stereo.
const ASC: [u8; 2] = [0x12, 0x10];

/// An AAC file of `n` frames, with frame `i` filled with `i`, and video tags between.
fn aac_flv(n: usize) -> Vec<u8> {
    let mut mux = FlvMux::new();
    mux.tag(18, 0, &on_metadata(&[amf_prop("duration", &amf_number(99.0))]));
    mux.aac(0, 0, &ASC);

    for i in 0..n {
        let ts = (i as u64 * 1024 * 1000 / 44_100) as u32;
        mux.video(ts, 200);
        mux.aac(ts, 1, &[i as u8; 50]);
    }

    mux.out
}

#[test]
fn aac_track_and_timestamps() {
    let mut reader = open(aac_flv(200));

    assert_eq!(reader.tracks().len(), 1);
    let track = reader.tracks()[0].clone();

    assert!(track.flags.contains(TrackFlags::DEFAULT));
    assert_eq!(track.time_base.unwrap().denom.get(), 44_100);
    assert_eq!(track.start_ts.get(), 0);
    // The duration is that of the last audio tag, not of the metadata.
    assert_eq!(track.duration.unwrap().get(), 200 * 1024);
    assert_eq!(reader.media_info().duration, track.duration);

    let params = audio_params(&reader, 0);
    assert_eq!(params.codec, CODEC_ID_AAC);
    assert_eq!(params.sample_rate, Some(44_100));
    assert_eq!(params.extra_data.as_deref(), Some(&ASC[..]));

    let packets = read_all(&mut reader);
    assert_eq!(packets.len(), 200);

    for (i, p) in packets.iter().enumerate() {
        assert_eq!(p.pts.get(), i as i64 * 1024);
        assert_eq!(p.dur.get(), 1024);
        assert_eq!(p.data[..], [i as u8; 50][..]);
    }

    assert!(reader.next_packet().unwrap().is_none());
}

#[test]
fn seeking_without_an_index() {
    let mut reader = open(aac_flv(3000));

    for target in [0i64, 1, 5 * 1024 + 7, 2999 * 1024, 1500 * 1024, 100 * 1024, 2000 * 1024 + 500] {
        let seeked = reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(target), track_id: 0 })
            .unwrap();

        assert_eq!(seeked.required_ts.get(), target);
        assert!(seeked.actual_ts.get() <= target);
        // A frame of preroll.
        assert!(target - seeked.actual_ts.get() <= 2 * 1024 + 1023, "target {target}");

        let p = reader.next_packet().unwrap().unwrap();
        assert_eq!(p.pts, seeked.actual_ts);
        assert_eq!(p.data[0], (p.pts.get() / 1024) as u8);
    }

    // Past the end.
    assert!(
        reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp { ts: Timestamp::new(4000 * 1024), track_id: 0 }
            )
            .is_err()
    );

    // The wrong track.
    assert!(
        reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(0), track_id: 7 })
            .is_err()
    );
}

#[test]
fn seeking_with_a_keyframe_index() {
    // An index with a (valid) entry, and entries that do not point to tags, that are ignored.
    let n = 3000;
    let plain = aac_flv(n);

    // Build the file again with an index, with positions that are found by scanning the plain file.
    let mut positions = vec![];
    let mut pos = 13;
    while pos + 11 <= plain.len() {
        let size = usize::from(plain[pos + 1]) << 16
            | usize::from(plain[pos + 2]) << 8
            | usize::from(plain[pos + 3]);
        if plain[pos] == 9 {
            positions.push(pos);
        }
        pos += 11 + size + 4;
    }

    // The metadata tag is longer by the index, so the positions are shifted.
    let times: Vec<f64> =
        (0..n).map(|i| (i as u64 * 1024 * 1000 / 44_100) as f64 / 1000.0).collect();

    let build = |shift: usize| {
        let pick: Vec<usize> = (0..n).step_by(100).collect();
        let meta = on_metadata(&[amf_prop(
            "keyframes",
            &[
                vec![0x03],
                amf_prop(
                    "filepositions",
                    &amf_strict_array(
                        &pick.iter().map(|&i| (positions[i] + shift) as f64).collect::<Vec<_>>(),
                    ),
                ),
                amf_prop(
                    "times",
                    &amf_strict_array(&pick.iter().map(|&i| times[i]).collect::<Vec<_>>()),
                ),
                vec![0, 0, 9],
            ]
            .concat(),
        )]);

        let mut mux = FlvMux::new();
        mux.tag(18, 0, &meta);
        mux.out.extend_from_slice(&{
            // The tags after the original metadata tag.
            let size =
                usize::from(plain[14]) << 16 | usize::from(plain[15]) << 8 | usize::from(plain[16]);
            plain[13 + 11 + size + 4..].to_vec()
        });
        mux
    };

    // The shift is the size difference of the metadata tags.
    let orig_meta = {
        let size =
            usize::from(plain[14]) << 16 | usize::from(plain[15]) << 8 | usize::from(plain[16]);
        11 + size + 4
    };
    let probe_mux = build(0);
    let new_meta = {
        let size = usize::from(probe_mux.out[14]) << 16
            | usize::from(probe_mux.out[15]) << 8
            | usize::from(probe_mux.out[16]);
        11 + size + 4
    };
    let out = build(new_meta - orig_meta).out;

    let mut reader = open(out);
    assert_eq!(reader.tracks()[0].duration.unwrap().get(), n as u64 * 1024);

    for target in [1234 * 1024 + 1, 5 * 1024, 2900 * 1024, 100 * 1024] {
        let seeked = reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(target), track_id: 0 })
            .unwrap();

        assert!(seeked.actual_ts.get() <= target);
        assert!(target - seeked.actual_ts.get() <= 3 * 1024 + 1023);

        let p = reader.next_packet().unwrap().unwrap();
        assert_eq!(p.data[0], (p.pts.get() / 1024) as u8);
    }
}

#[test]
fn pcm_formats() {
    // 22.05 kHz mono 16-bit little-endian, 441 samples per tag (20 ms).
    let mut mux = FlvMux::new();
    for n in 0..100u32 {
        let mut payload = vec![];
        for i in 0..441u32 {
            payload.extend_from_slice(&((n * 441 + i) as i16).to_le_bytes());
        }
        mux.audio(n * 20, (3 << 4) | (2 << 2) | 2, &payload);
    }

    let mut reader = open(mux.out);
    let params = audio_params(&reader, 0);
    assert_eq!(params.codec, CODEC_ID_PCM_S16LE);
    assert_eq!(params.sample_rate, Some(22_050));
    assert_eq!(params.channels.as_ref().unwrap().count(), 1);

    let packets = read_all(&mut reader);
    assert_eq!(packets.len(), 100);
    assert!(
        packets
            .iter()
            .enumerate()
            .all(|(i, p)| p.pts.get() == i as i64 * 441 && p.dur.get() == 441)
    );
    assert_eq!(reader.tracks()[0].duration.unwrap().get(), 44_100);

    let decoded = decode_all(&params, &packets).concat();
    assert_eq!(decoded.len(), 44_100);
    assert!(decoded.iter().enumerate().all(|(i, s)| *s == f32::from(i as i16) / 32768.0));

    check_seeks(|| Box::new(open(reader_bytes(100))), 0, &[0, 5, 441, 20_000, 44_000], Some(0.0));

    fn reader_bytes(tags: u32) -> Vec<u8> {
        let mut mux = FlvMux::new();
        for n in 0..tags {
            let mut payload = vec![];
            for i in 0..441u32 {
                payload.extend_from_slice(&((n * 441 + i) as i16).to_le_bytes());
            }
            mux.audio(n * 20, (3 << 4) | (2 << 2) | 2, &payload);
        }
        mux.out
    }

    // 8-bit stereo, and big-endian 16-bit.
    for (flags, codec, ch) in [
        ((3 << 4) | (3 << 2) | 1, CODEC_ID_PCM_U8, 2),
        ((0 << 4) | (3 << 2) | 2 | 1, CODEC_ID_PCM_S16BE, 2),
    ] {
        let mut mux = FlvMux::new();
        mux.audio(0, flags, &[0u8; 400]);
        mux.audio(10, flags, &[0u8; 400]);
        let reader = open(mux.out);
        let params = audio_params(&reader, 0);
        assert_eq!(params.codec, codec);
        assert_eq!(params.sample_rate, Some(44_100));
        assert_eq!(params.channels.as_ref().unwrap().count(), ch);
    }

    // G.711.
    for (format, codec) in [(7u8, CODEC_ID_PCM_ALAW), (8, CODEC_ID_PCM_MULAW)] {
        let mut mux = FlvMux::new();
        mux.audio(0, (format << 4) | 2, &[0u8; 160]);
        mux.audio(20, (format << 4) | 2, &[0u8; 160]);
        let mut reader = open(mux.out);
        let params = audio_params(&reader, 0);
        assert_eq!(params.codec, codec);
        assert_eq!(params.sample_rate, Some(8000));
        let packets = read_all(&mut reader);
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[1].pts.get(), 160);
    }
}

#[test]
fn mpeg_audio_frames() {
    // MPEG-1 layer 2 frames of 128 kbps at 48 kHz: 384 bytes and 1152 samples. Tags with one frame
    // and two frames.
    let frame = |i: u8| {
        let mut f = vec![i; 384];
        f[..4].copy_from_slice(&[0xff, 0xfd, 0x84, 0x04]);
        f
    };

    let mut mux = FlvMux::new();
    let mut ts = 0u64;
    let mut frame_idx = 0u8;
    for tag in 0..30 {
        let n = if tag % 3 == 0 { 2 } else { 1 };
        let payload: Vec<u8> = (0..n).flat_map(|k| frame(frame_idx + k)).collect();
        mux.audio((ts * 1000 / 48_000) as u32, 0x2f, &payload);
        ts += 1152 * u64::from(n);
        frame_idx += n;
    }

    let mut reader = open(mux.out);
    let params = audio_params(&reader, 0);
    assert_eq!(params.codec, CODEC_ID_MP2);
    assert_eq!(params.sample_rate, Some(48_000));

    let packets = read_all(&mut reader);
    assert_eq!(packets.len(), usize::from(frame_idx));

    for (i, p) in packets.iter().enumerate() {
        assert_eq!(p.pts.get(), i as i64 * 1152);
        assert_eq!(p.data[..], frame(i as u8)[..]);
    }

    assert_eq!(reader.tracks()[0].duration.unwrap().get(), u64::from(frame_idx) * 1152);
}

#[test]
fn duration_from_metadata_when_not_seekable() {
    let data = aac_flv(100);

    let mss = MediaSourceStream::new(
        Box::new(ReadOnlySource::new(Cursor::new(data))),
        Default::default(),
    );
    let mut reader = FlvReader::try_new(mss, FormatOptions::default()).unwrap();

    // 99 s at 44.1 kHz.
    assert_eq!(reader.tracks()[0].duration.unwrap().get(), 99 * 44_100);

    let first = reader.next_packet().unwrap().unwrap();
    assert_eq!(first.pts.get(), 0);

    // Forward seeks only.
    let seeked = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(50 * 1024), track_id: 0 })
        .unwrap();
    assert!(seeked.actual_ts.get() <= 50 * 1024);
    assert!(50 * 1024 - seeked.actual_ts.get() <= 1024 + 1023);

    let err = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(1024), track_id: 0 })
        .unwrap_err();
    assert!(matches!(err, Error::SeekError(_)));
}

#[test]
fn tag_timestamp_jitter_and_discontinuities() {
    // Tags with time stamps that are rounded to 5 ms are not discontinuities, but a gap of 1 s is.
    let mut mux = FlvMux::new();
    mux.aac(0, 0, &ASC);

    for i in 0..50u64 {
        let ts = (i * 1024 * 1000 / 44_100 + 2) / 5 * 5;
        let gap = if i >= 30 { 1000 } else { 0 };
        mux.aac((ts + gap) as u32, 1, &[i as u8; 10]);
    }

    let packets = read_all(&mut open(mux.out));
    assert_eq!(packets.len(), 50);
    assert_eq!(packets[29].pts.get(), 29 * 1024);
    // The gap, on the grid of frames.
    assert_eq!(packets[30].pts.get(), (30 * 1024 + 44_100 + 512) / 1024 * 1024);
}

#[test]
fn tags_after_the_audio_format_with_a_different_format_are_ignored() {
    let mut mux = FlvMux::new();
    mux.aac(0, 0, &ASC);
    mux.aac(0, 1, &[1; 10]);
    mux.audio(23, (2 << 4) | 0x0f, &[0xff, 0xfd, 0x84, 0x04]);
    mux.aac(23, 1, &[2; 10]);

    let packets = read_all(&mut open(mux.out));
    assert_eq!(packets.len(), 2);
}

#[test]
fn recovers_from_garbage() {
    let plain = aac_flv(300);
    let mut out = plain[..plain.len() / 2].to_vec();
    out.extend(std::iter::repeat_n(0x77, 1237));
    out.extend_from_slice(&plain[plain.len() / 2..]);

    let mut reader = open(out);
    let packets = read_all(&mut reader);

    assert!(packets.len() >= 295, "{}", packets.len());
    let last = packets.last().unwrap();
    assert_eq!(last.data[0], 299u32 as u8);
    assert_eq!(last.pts.get(), 299 * 1024);
}

#[test]
fn prebuilt_seek_index() {
    let data = aac_flv(500);
    let opts = FormatOptions::default().prebuild_seek_index(true).seek_index_fill_period_ms(100);
    let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
    let mut reader = FlvReader::try_new(mss, opts).unwrap();

    let seeked = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(400 * 1024), track_id: 0 })
        .unwrap();
    assert!(400 * 1024 - seeked.actual_ts.get() <= 2 * 1024 + 1023);
    assert_eq!(reader.next_packet().unwrap().unwrap().pts, seeked.actual_ts);
}

#[test]
fn probe_and_score() {
    let data = aac_flv(50);

    let mut probe = Probe::new();
    probe.register_format::<FlvReader<'_>>();

    for prefix in [0usize, 5] {
        let mut d = vec![0u8; prefix];
        d.extend_from_slice(&data);

        let mss = MediaSourceStream::new(Box::new(Cursor::new(d)), Default::default());
        let mut reader = probe
            .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
            .unwrap();

        assert_eq!(reader.format_info().short_name, "flv");
        assert_eq!(read_all(&mut *reader).len(), 50);
    }

    let score = |data: &[u8]| {
        let mut mss =
            MediaSourceStream::new(Box::new(Cursor::new(data.to_vec())), Default::default());
        FlvReader::score(ScopedStream::new(&mut mss, 16 * 1024)).unwrap()
    };

    assert!(matches!(score(&data), Score::Supported(255)));

    // Reserved flag bits, and a non-zero first previous tag size.
    let mut bad = data.clone();
    bad[4] = 0xff;
    assert!(matches!(score(&bad), Score::Unsupported));

    let mut bad = data.clone();
    bad[12] = 1;
    assert!(matches!(score(&bad), Score::Unsupported));

    assert!(matches!(score(b"FLV"), Score::Unsupported));
}

#[test]
fn files_without_audio_are_rejected() {
    let mut mux = FlvMux::new();
    mux.tag(18, 0, &on_metadata(&[]));

    for i in 0..20 {
        mux.video(i * 40, 1000);
    }

    let mss = MediaSourceStream::new(Box::new(Cursor::new(mux.out)), Default::default());
    assert!(FlvReader::try_new(mss, FormatOptions::default()).is_err());

    // Unsupported audio.
    let mut mux = FlvMux::new();
    mux.audio(0, 0x1f, &[0u8; 100]);
    let mss = MediaSourceStream::new(Box::new(Cursor::new(mux.out)), Default::default());
    let err = FlvReader::try_new(mss, FormatOptions::default()).err().unwrap();
    assert!(matches!(err, Error::Unsupported(_)));
}

/// Read the stream, and seek in it, without panicking or running forever.
fn exercise(data: Vec<u8>, track_id: u32, rng: &mut Rng) {
    let Ok(mut reader) = FlvReader::try_new(
        MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default()),
        FormatOptions::default(),
    )
    else {
        return;
    };

    let mut n = 0;

    while let Ok(Some(_)) = reader.next_packet() {
        n += 1;

        if n > 100_000 {
            panic!("too many packets");
        }
    }

    for _ in 0..3 {
        let ts = Timestamp::new(rng.below(1_000_000) as i64);
        let _ = reader.seek(SeekMode::Accurate, SeekTo::Timestamp { ts, track_id });
        let _ = reader.next_packet();
    }
}

#[test]
fn corrupt_streams_do_not_panic() {
    let data = aac_flv(300);
    let mut rng = Rng(0x1234_5678_9abc_def1);

    for _ in 0..400 {
        exercise(corrupt(&data, &mut rng), 0, &mut rng);
    }

    // And MP3 and PCM.
    let mut mux = FlvMux::new();
    for n in 0..100u32 {
        mux.audio(n * 20, (3 << 4) | (2 << 2) | 2, &[n as u8; 882]);
    }
    for _ in 0..200 {
        exercise(corrupt(&mux.out, &mut rng), 0, &mut rng);
    }
}
