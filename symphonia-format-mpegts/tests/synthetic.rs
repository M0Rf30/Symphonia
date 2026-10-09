// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests with synthetic transport streams.

mod support;

use std::io::Cursor;

use symphonia_core::errors::Error;
use symphonia_core::formats::probe::{Hint, Probe, Score, Scoreable};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackFlags};
use symphonia_core::io::{MediaSourceStream, ReadOnlySource, ScopedStream};
use symphonia_core::meta::MetadataOptions;
use symphonia_core::units::Timestamp;
use symphonia_format_mpegts::MpegTsReader;

use support::*;

/// MPEG-1 layer 2 frames of 128 kbps at 48 kHz: 384 bytes and 1152 samples. The payload of each
/// frame is filled with the frame index.
fn mpa_frames(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| {
            let mut f = vec![i as u8; 384];
            f[..4].copy_from_slice(&[0xff, 0xfd, 0x84, 0x04]);
            f
        })
        .collect()
}

/// MPEG-4 LC AAC ADTS frames of 48 kHz stereo. The payload of each frame is filled with the frame
/// index.
fn adts_frames(n: usize, payload_len: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| {
            let len = payload_len + 7;
            let mut f = vec![
                0xff,
                0xf1,
                (1 << 6) | (3 << 2),
                (2 << 6) | ((len >> 11) & 3) as u8,
                ((len >> 3) & 0xff) as u8,
                (((len & 7) << 5) | 0x1f) as u8,
                0xfc,
            ];
            f.extend(std::iter::repeat_n(i as u8, payload_len));
            f
        })
        .collect()
}

fn pts_of(frame: usize, samples: u64, rate: u64, base: u64) -> u64 {
    (base + frame as u64 * samples * 90_000 / rate) % (1 << 33)
}

/// Multiplex whole frames, `per_pes` at a time.
fn mux_frames(
    mux: &mut TsMux,
    pid: u16,
    frames: &[Vec<u8>],
    per_pes: usize,
    samples: u64,
    rate: u64,
    base: u64,
) {
    for (n, group) in frames.chunks(per_pes).enumerate() {
        let payload: Vec<u8> = group.concat();
        mux.pes(pid, 0xc0, Some(pts_of(n * per_pes, samples, rate, base)), &payload);
    }
}

#[test]
fn mpa_tracks_and_timestamps() {
    let frames = mpa_frames(100);
    let mut mux = TsMux::new(188);

    let spec = StreamSpec {
        stream_type: 0x03,
        pid: 0x101,
        descriptors: vec![0x0a, 0x04, b'I', b'T', b'A', 0],
    };
    mux.psi(0x1000, &[spec]);
    mux_frames(&mut mux, 0x101, &frames, 3, 1152, 48_000, 1_000_000);

    let mut reader = open(mux.out);
    assert_eq!(reader.tracks().len(), 1);

    let track = reader.tracks()[0].clone();
    assert_eq!(track.id, 0x101);
    assert_eq!(track.language.as_deref(), Some("ita"));
    assert!(track.flags.contains(TrackFlags::DEFAULT));
    assert_eq!(track.time_base.unwrap().denom.get(), 48_000);
    assert_eq!(track.duration.unwrap().get(), 100 * 1152);
    assert_eq!(track.num_frames, Some(100 * 1152));
    assert_eq!(reader.media_info().duration, track.duration);

    let params = audio_params(&reader, 0);
    assert_eq!(params.codec, symphonia_core::codecs::audio::well_known::CODEC_ID_MP2);
    assert_eq!(params.sample_rate, Some(48_000));

    let packets = read_all(&mut reader);
    assert_eq!(packets.len(), 100);

    for (i, p) in packets.iter().enumerate() {
        assert_eq!(p.pts.get(), i as i64 * 1152);
        assert_eq!(p.dur.get(), 1152);
        assert_eq!(p.data[..], frames[i][..]);
    }

    assert!(reader.next_packet().unwrap().is_none());
}

#[test]
fn pts_wrap_is_handled() {
    // The PTS wraps after 10 frames.
    let frames = mpa_frames(60);
    let base = (1u64 << 33) - 10 * 1152 * 90_000 / 48_000;
    let mut mux = TsMux::new(188);
    mux.psi(0x1000, &[StreamSpec { stream_type: 0x04, pid: 0x101, descriptors: vec![] }]);
    mux_frames(&mut mux, 0x101, &frames, 2, 1152, 48_000, base);

    let out = mux.out;
    let mut reader = open(out.clone());
    assert_eq!(reader.tracks()[0].duration.unwrap().get(), 60 * 1152);

    let packets = read_all(&mut reader);
    assert_eq!(packets.len(), 60);
    assert!(packets.iter().enumerate().all(|(i, p)| p.pts.get() == i as i64 * 1152));

    // Seeking across the wrap.
    for target in [0i64, 5 * 1152 + 1, 10 * 1152, 33 * 1152 + 5, 59 * 1152, 12 * 1152] {
        let seeked = reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp { ts: Timestamp::new(target), track_id: 0x101 },
            )
            .unwrap();

        let want_start = (target - 3 * 1152 - 8 * 1152).max(0) / 1152 * 1152;
        assert!(seeked.actual_ts.get() <= target);
        assert!(seeked.actual_ts.get() >= want_start - 1152, "target {target}");

        let p = reader.next_packet().unwrap().unwrap();
        assert_eq!(p.pts, seeked.actual_ts);
        assert_eq!(p.data[..], frames[(p.pts.get() / 1152) as usize][..]);
    }
}

#[test]
fn aac_adts_and_unsupported_streams() {
    let frames = adts_frames(80, 100);
    let mut mux = TsMux::new(188);

    mux.psi(
        0x1000,
        &[
            // A video stream, and unsupported audio streams, that are ignored.
            StreamSpec { stream_type: 0x1b, pid: 0x100, descriptors: vec![] },
            StreamSpec { stream_type: 0x81, pid: 0x102, descriptors: vec![] },
            StreamSpec { stream_type: 0x0f, pid: 0x101, descriptors: vec![] },
        ],
    );

    mux.pes(0x100, 0xe0, Some(90_000), &[0u8; 5000]);
    mux.pes(0x102, 0xbd, Some(90_000), &[0u8; 500]);
    mux_frames(&mut mux, 0x101, &frames, 4, 1024, 48_000, 90_000);

    let mut reader = open(mux.out);
    assert_eq!(reader.tracks().len(), 1);
    assert_eq!(reader.tracks()[0].id, 0x101);
    assert_eq!(audio_params(&reader, 0).sample_rate, Some(48_000));

    let packets = read_all(&mut reader);
    assert_eq!(packets.len(), 80);
    assert!(packets.iter().enumerate().all(|(i, p)| p.data[..] == frames[i][7..]));
}

#[test]
fn multiple_tracks_share_a_timeline() {
    let a = mpa_frames(50);
    let b = adts_frames(50, 60);
    let mut mux = TsMux::new(188);

    mux.psi(
        0x1000,
        &[
            StreamSpec { stream_type: 0x03, pid: 0x101, descriptors: vec![] },
            StreamSpec { stream_type: 0x0f, pid: 0x102, descriptors: vec![] },
        ],
    );

    // The second stream begins half a second later.
    for i in 0..25 {
        mux.pes(
            0x101,
            0xc0,
            Some(pts_of(i * 2, 1152, 48_000, 500_000)),
            &a[i * 2..i * 2 + 2].concat(),
        );
        mux.pes(
            0x102,
            0xc1,
            Some(pts_of(i * 2, 1024, 48_000, 500_000 + 45_000)),
            &b[i * 2..i * 2 + 2].concat(),
        );
    }

    let mut reader = open(mux.out);
    assert_eq!(reader.tracks().len(), 2);
    assert_eq!(reader.tracks()[0].start_ts.get(), 0);
    // The frames are on a grid of 1024 samples: 24000 is rounded to 23 frames.
    assert_eq!(reader.tracks()[1].start_ts.get(), 23 * 1024);
    assert!(reader.tracks()[0].flags.contains(TrackFlags::DEFAULT));
    assert!(!reader.tracks()[1].flags.contains(TrackFlags::DEFAULT));

    let packets = read_all(&mut reader);
    let t1: Vec<_> = packets.iter().filter(|p| p.track_id == 0x102).collect();
    assert_eq!(t1.len(), 50);
    assert_eq!(t1[0].pts.get(), 23 * 1024);
    assert_eq!(t1[1].pts.get(), 24 * 1024);
}

#[test]
fn timeline_follows_pts_discontinuities() {
    // A gap of 1 second after frame 20.
    let frames = mpa_frames(40);
    let mut mux = TsMux::new(188);
    mux.psi(0x1000, &[StreamSpec { stream_type: 0x03, pid: 0x101, descriptors: vec![] }]);

    for (i, f) in frames.iter().enumerate() {
        let gap = if i >= 20 { 90_000 } else { 0 };
        mux.pes(0x101, 0xc0, Some(pts_of(i, 1152, 48_000, 0) + gap), f);
    }

    let mut reader = open(mux.out);
    let packets = read_all(&mut reader);

    assert_eq!(packets[19].pts.get(), 19 * 1152);
    // The gap is on the grid of frames of 1152 samples: 71040 is rounded to 62 frames.
    assert_eq!(packets[20].pts.get(), 62 * 1152);
    assert_eq!(packets[39].pts.get(), 81 * 1152);
}

#[test]
fn opus_trims_and_channels() {
    // Opus CELT FB 20 ms mono frames.
    let au = |trim: Option<u16>| {
        let mut au = vec![0x7f, 0xe0 | if trim.is_some() { 0x10 } else { 0 }, 20];
        if let Some(t) = trim {
            au.extend_from_slice(&t.to_be_bytes());
        }
        au.push(31 << 3);
        au.extend(std::iter::repeat_n(0u8, 19));
        au
    };

    let mut mux = TsMux::new(188);
    mux.psi(
        0x1000,
        &[StreamSpec { stream_type: 0x06, pid: 0x101, descriptors: opus_descriptors(2) }],
    );
    mux.pes(0x101, 0xc0, Some(90_000), &[au(Some(312)), au(None), au(None)].concat());
    mux.pes(0x101, 0xc0, Some(90_000 + 3 * 960 * 90_000 / 48_000), &au(None));

    let mut reader = open(mux.out);
    let params = audio_params(&reader, 0);
    assert_eq!(params.codec, symphonia_core::codecs::audio::well_known::CODEC_ID_OPUS);
    assert_eq!(params.channels.as_ref().unwrap().count(), 2);
    assert_eq!(&params.extra_data.as_ref().unwrap()[..8], b"OpusHead");

    let packets = read_all(&mut reader);
    assert_eq!(packets.len(), 4);
    assert_eq!(packets[0].trim_start.get(), 312);
    assert_eq!(packets[0].dur.get(), 960 - 312);
    assert_eq!(packets[0].block_dur().get(), 960);
    // The first sample that is not trimmed is at 0.
    assert_eq!(packets[0].pts.get(), 0);
    assert_eq!(packets[0].dts.get(), -312);
    assert_eq!(packets[1].pts.get(), 960 - 312);
    assert_eq!(packets[3].pts.get(), 3 * 960 - 312);
    assert_eq!(reader.tracks()[0].delay, Some(312));
    assert_eq!(reader.tracks()[0].start_ts.get(), 0);
    assert_eq!(reader.tracks()[0].duration.unwrap().get(), 4 * 960 - 312);
}

#[test]
fn recovers_from_garbage() {
    let frames = mpa_frames(60);
    let mut mux = TsMux::new(188);
    mux.psi(0x1000, &[StreamSpec { stream_type: 0x03, pid: 0x101, descriptors: vec![] }]);
    mux_frames(&mut mux, 0x101, &frames[..30], 2, 1152, 48_000, 0);

    let mut out = vec![0x12; 77];
    out.extend_from_slice(&mux.out);
    let junk_pos = out.len();

    // Junk in the middle of the stream (a multiple of the packet size, to not shift later
    // packets' PES contents... then also an unaligned amount).
    out.extend(std::iter::repeat_n(0xaa, 188 * 2 + 31));

    let mut mux = TsMux::new(188);
    for (n, group) in frames[30..].chunks(2).enumerate() {
        mux.pes(0x101, 0xc0, Some(pts_of(30 + n * 2, 1152, 48_000, 0)), &group.concat());
    }
    out.extend_from_slice(&mux.out);
    let _ = junk_pos;

    let mut reader = MpegTsReader::try_new(
        MediaSourceStream::new(Box::new(Cursor::new(out[77..].to_vec())), Default::default()),
        FormatOptions::default(),
    )
    .unwrap();

    let packets = read_all(&mut reader);

    // The frames around the junk may be lost, but the stream continues, and the timeline follows
    // the PTS.
    assert!(packets.len() >= 55, "{}", packets.len());
    assert_eq!(packets.last().unwrap().pts.get(), 59 * 1152);
    assert_eq!(packets.last().unwrap().data[4], 59);
}

#[test]
fn scrambled_and_error_packets_are_skipped() {
    let frames = mpa_frames(20);
    let mut mux = TsMux::new(188);
    mux.psi(0x1000, &[StreamSpec { stream_type: 0x03, pid: 0x101, descriptors: vec![] }]);
    mux_frames(&mut mux, 0x101, &frames, 1, 1152, 48_000, 0);
    let mut out = mux.out;

    // Make the packets of PES 10 scrambled, and those of PES 15 erroneous.
    let n = out.len() / 188;
    let mut pes_idx = 0;
    for p in 2..n {
        let pkt = &mut out[p * 188..(p + 1) * 188];
        if pkt[1] & 0x40 != 0 {
            pes_idx += 1;
        }
        if pes_idx == 11 {
            pkt[3] |= 0x80;
        }
        if pes_idx == 16 {
            pkt[1] |= 0x80;
        }
    }

    let mut reader = open(out);
    let packets = read_all(&mut reader);
    assert_eq!(packets.len(), 18);
    assert!(packets.iter().all(|p| p.data[4] != 10 && p.data[4] != 15));
}

#[test]
fn non_seekable_streams_can_only_seek_forward() {
    let frames = mpa_frames(200);
    let mut mux = TsMux::new(188);
    mux.psi(0x1000, &[StreamSpec { stream_type: 0x03, pid: 0x101, descriptors: vec![] }]);
    mux_frames(&mut mux, 0x101, &frames, 2, 1152, 48_000, 0);

    let mss = MediaSourceStream::new(
        Box::new(ReadOnlySource::new(Cursor::new(mux.out))),
        Default::default(),
    );

    let mut reader = MpegTsReader::try_new(mss, FormatOptions::default()).unwrap();

    // The duration is not known.
    assert_eq!(reader.tracks()[0].duration, None);

    for _ in 0..10 {
        reader.next_packet().unwrap().unwrap();
    }

    let target = 100 * 1152;
    let seeked = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(target), track_id: 0x101 })
        .unwrap();
    assert!(seeked.actual_ts.get() <= target);
    assert!(target - seeked.actual_ts.get() <= 12 * 1152);

    let err = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(1152), track_id: 0x101 })
        .unwrap_err();
    assert!(matches!(err, Error::SeekError(_)));
}

#[test]
fn probe_selects_the_transport_stream_reader() {
    let frames = mpa_frames(40);

    for size in [188, 192, 204] {
        let mut mux = TsMux::new(size);
        mux.psi(0x1000, &[StreamSpec { stream_type: 0x03, pid: 0x101, descriptors: vec![] }]);
        mux_frames(&mut mux, 0x101, &frames, 2, 1152, 48_000, 0);

        let mut probe = Probe::new();
        probe.register_format::<MpegTsReader<'_>>();

        // With a prefix of junk.
        for prefix in [0usize, 13] {
            let mut data = vec![0u8; prefix];
            data.extend_from_slice(&mux.out);

            let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
            let mut reader = probe
                .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
                .unwrap();

            assert_eq!(reader.format_info().short_name, "mpegts");
            assert_eq!(read_all(&mut *reader).len(), 40, "size {size} prefix {prefix}");
        }
    }
}

#[test]
fn score_rejects_other_data() {
    let score = |data: &[u8]| {
        let mut mss =
            MediaSourceStream::new(Box::new(Cursor::new(data.to_vec())), Default::default());
        MpegTsReader::score(ScopedStream::new(&mut mss, 16 * 1024)).unwrap()
    };

    // Random data with sync bytes that are not one packet apart.
    let mut data = vec![0u8; 4000];
    for i in (0..4000).step_by(100) {
        data[i] = 0x47;
    }
    assert!(matches!(score(&data), Score::Unsupported));

    // An MPEG audio stream.
    let frames = mpa_frames(20).concat();
    assert!(matches!(score(&frames), Score::Unsupported));

    // Two sync bytes one packet apart, in data that is longer, are not enough. This is common in
    // data that has many 0x47 bytes.
    let mut data = vec![0x11u8; 4000];
    data[0] = 0x47;
    data[3] = 0x10;
    data[188] = 0x47;
    data[191] = 0x10;
    assert!(matches!(score(&data), Score::Unsupported));

    // A transport stream.
    let mut mux = TsMux::new(188);
    mux.psi(0x1000, &[StreamSpec { stream_type: 0x03, pid: 0x101, descriptors: vec![] }]);
    mux_frames(&mut mux, 0x101, &mpa_frames(20), 1, 1152, 48_000, 0);
    assert!(matches!(score(&mux.out), Score::Supported(255)));

    // A short transport stream, which is entirely packets.
    assert!(matches!(score(&mux.out[..188 * 3]), Score::Supported(64)));
    assert!(matches!(score(&mux.out[..188 * 3 + 5]), Score::Supported(64)));
    // A single packet is not enough.
    assert!(matches!(score(&mux.out[..188]), Score::Unsupported));
}

#[test]
fn streams_without_supported_audio_are_rejected() {
    let mut mux = TsMux::new(188);
    mux.psi(0x1000, &[StreamSpec { stream_type: 0x1b, pid: 0x100, descriptors: vec![] }]);
    for _ in 0..20 {
        mux.pes(0x100, 0xe0, Some(90_000), &[0u8; 3000]);
    }

    let mss = MediaSourceStream::new(Box::new(Cursor::new(mux.out)), Default::default());
    assert!(MpegTsReader::try_new(mss, FormatOptions::default()).is_err());
}

/// Read the stream, and seek in it, without panicking or running forever.
fn exercise(data: Vec<u8>, track_id: u32, rng: &mut Rng) {
    let Ok(mut reader) = MpegTsReader::try_new(
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
    let mut mux = TsMux::new(188);
    mux.psi(
        0x1000,
        &[
            StreamSpec { stream_type: 0x0f, pid: 0x101, descriptors: vec![] },
            StreamSpec { stream_type: 0x03, pid: 0x102, descriptors: vec![] },
            StreamSpec { stream_type: 0x06, pid: 0x103, descriptors: opus_descriptors(2) },
        ],
    );

    let a = adts_frames(60, 100);
    let b = mpa_frames(60);

    for i in 0..30 {
        if i % 8 == 0 {
            let spec = [
                StreamSpec { stream_type: 0x0f, pid: 0x101, descriptors: vec![] },
                StreamSpec { stream_type: 0x03, pid: 0x102, descriptors: vec![] },
                StreamSpec { stream_type: 0x06, pid: 0x103, descriptors: opus_descriptors(2) },
            ];
            mux.psi(0x1000, &spec);
        }
        mux.pes(
            0x101,
            0xc0,
            Some(pts_of(i * 2, 1024, 48_000, 90_000)),
            &a[i * 2..i * 2 + 2].concat(),
        );
        mux.pes(
            0x102,
            0xc1,
            Some(pts_of(i * 2, 1152, 48_000, 90_000)),
            &b[i * 2..i * 2 + 2].concat(),
        );
        let mut au = vec![0x7f, 0xe0, 20, 31 << 3];
        au.extend(std::iter::repeat_n(0u8, 19));
        mux.pes(0x103, 0xc2, Some(90_000 + i as u64 * 1800), &au);
    }

    let mut rng = Rng(0x1234_5678_9abc_def1);

    for _ in 0..300 {
        exercise(corrupt(&mux.out, &mut rng), 0x101 + rng.below(3) as u32, &mut rng);
    }
}
