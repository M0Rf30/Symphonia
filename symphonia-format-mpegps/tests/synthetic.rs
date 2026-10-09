// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests with synthetic program streams.

mod support;

use std::io::Cursor;

use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_MP2, CODEC_ID_PCM_S16BE, CODEC_ID_PCM_S24BE,
};
use symphonia_core::errors::Error;
use symphonia_core::formats::probe::{Hint, Probe, Score, Scoreable};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackFlags};
use symphonia_core::io::{MediaSourceStream, ReadOnlySource, ScopedStream};
use symphonia_core::meta::MetadataOptions;
use symphonia_core::units::Timestamp;
use symphonia_format_mpegps::MpegPsReader;

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

fn pts_of(frame: usize, samples: u64, rate: u64, base: u64) -> u64 {
    (base + frame as u64 * samples * 90_000 / rate) % (1 << 33)
}

fn mux_mpa(mux: &mut PsMux, stream_id: u8, frames: &[Vec<u8>], per_pes: usize, base: u64) {
    for (n, group) in frames.chunks(per_pes).enumerate() {
        if n % 2 == 0 {
            mux.pack(base + n as u64 * 3000);
        }
        mux.pes(stream_id, Some(pts_of(n * per_pes, 1152, 48_000, base)), &group.concat());
    }
}

#[test]
fn mpa_tracks_and_timestamps() {
    for mpeg1 in [false, true] {
        let frames = mpa_frames(100);
        let mut mux = PsMux::new(mpeg1);
        mux.pack(0);
        mux.system_header();
        mux_mpa(&mut mux, 0xc0, &frames, 3, 1_000_000);

        let mut reader = open(mux.out);
        assert_eq!(reader.tracks().len(), 1);

        let track = reader.tracks()[0].clone();
        assert_eq!(track.id, 0xc0);
        assert!(track.flags.contains(TrackFlags::DEFAULT));
        assert_eq!(track.time_base.unwrap().denom.get(), 48_000);
        assert_eq!(track.duration.unwrap().get(), 100 * 1152);
        assert_eq!(reader.media_info().duration, track.duration);

        let params = audio_params(&reader, 0);
        assert_eq!(params.codec, CODEC_ID_MP2);
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
}

#[test]
fn pts_wrap_is_handled_and_seeks() {
    let frames = mpa_frames(60);
    let base = (1u64 << 33) - 10 * 1152 * 90_000 / 48_000;
    let mut mux = PsMux::new(false);
    mux.pack(0);
    mux_mpa(&mut mux, 0xc0, &frames, 2, base);

    let mut reader = open(mux.out);
    assert_eq!(reader.tracks()[0].duration.unwrap().get(), 60 * 1152);

    let packets = read_all(&mut reader);
    assert_eq!(packets.len(), 60);
    assert!(packets.iter().enumerate().all(|(i, p)| p.pts.get() == i as i64 * 1152));

    for target in [0i64, 5 * 1152 + 1, 10 * 1152, 33 * 1152 + 5, 59 * 1152, 12 * 1152] {
        let seeked = reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp { ts: Timestamp::new(target), track_id: 0xc0 },
            )
            .unwrap();

        assert!(seeked.actual_ts.get() <= target);
        assert!(target - seeked.actual_ts.get() <= 3 * 1152 + 1152, "target {target}");

        let p = reader.next_packet().unwrap().unwrap();
        assert_eq!(p.pts, seeked.actual_ts);
        assert_eq!(p.data[..], frames[(p.pts.get() / 1152) as usize][..]);
    }

    // Past the end.
    assert!(
        reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp { ts: Timestamp::new(61 * 1152), track_id: 0xc0 }
            )
            .is_err()
    );
}

#[test]
fn multiple_audio_streams_and_ignored_streams() {
    let a = mpa_frames(40);
    let b = mpa_frames(40);

    let mut mux = PsMux::new(false);
    mux.pack(0);

    for i in 0..20 {
        mux.pack(i as u64 * 3000);
        // Video, a navigation packet (private stream 2), and AC-3 (private stream 1) are ignored.
        mux.pes(0xe0, Some(90_000), &[0u8; 1500]);
        mux.pes(0xbf, None, &[0u8; 100]);
        mux.pes(0xbd, Some(90_000), &[&[0x80u8, 1, 0, 2][..], &[0u8; 500][..]].concat());
        mux.pes(0xc0, Some(pts_of(i * 2, 1152, 48_000, 90_000)), &a[i * 2..i * 2 + 2].concat());
        mux.pes(
            0xc1,
            Some(pts_of(i * 2, 1152, 48_000, 90_000 + 4500)),
            &b[i * 2..i * 2 + 2].concat(),
        );
    }

    let mut reader = open(mux.out);
    assert_eq!(reader.tracks().len(), 2);
    assert_eq!(reader.tracks()[0].id, 0xc0);
    assert_eq!(reader.tracks()[1].id, 0xc1);
    assert_eq!(reader.tracks()[0].start_ts.get(), 0);
    // The frames are on a grid of 1152 samples: 2400 is rounded to 2 frames.
    assert_eq!(reader.tracks()[1].start_ts.get(), 2304);
    assert!(!reader.tracks()[1].flags.contains(TrackFlags::DEFAULT));

    let packets = read_all(&mut reader);
    assert_eq!(packets.iter().filter(|p| p.track_id == 0xc0).count(), 40);
    let t1: Vec<_> = packets.iter().filter(|p| p.track_id == 0xc1).collect();
    assert_eq!(t1.len(), 40);
    assert_eq!(t1[0].pts.get(), 2304);
    assert_eq!(t1[1].pts.get(), 2304 + 1152);
}

/// DVD LPCM PES payloads: 16-bit stereo 48 kHz where the sample of frame `i` is `i` in both
/// channels.
fn lpcm_payload(first_sample: usize, n: usize, first_au_ptr: u16) -> Vec<u8> {
    let mut p = vec![0xa0, 1];
    p.extend_from_slice(&first_au_ptr.to_be_bytes());
    p.extend_from_slice(&[0x00, 0x01, 0x80]);

    for i in first_sample..first_sample + n {
        p.extend_from_slice(&(i as i16).to_be_bytes());
        p.extend_from_slice(&(i as i16).to_be_bytes());
    }

    p
}

#[test]
fn lpcm_16_bit() {
    let mut mux = PsMux::new(false);
    mux.pack(0);

    // 503 samples per packet.
    for n in 0..100usize {
        if n % 2 == 0 {
            mux.pack(n as u64 * 2000);
        }
        mux.pes(
            0xbd,
            Some(45_000 + (n * 503 * 90_000 / 48_000) as u64),
            &lpcm_payload(n * 503, 503, 4),
        );
    }

    let out = mux.out;
    let mut reader = open(out.clone());

    let track = reader.tracks()[0].clone();
    assert_eq!(track.id, 0xbda0);
    assert_eq!(track.duration.unwrap().get(), 50_300);

    let params = audio_params(&reader, 0);
    assert_eq!(params.codec, CODEC_ID_PCM_S16BE);
    assert_eq!(params.sample_rate, Some(48_000));
    assert_eq!(params.channels.as_ref().unwrap().count(), 2);

    let packets = read_all(&mut reader);
    assert_eq!(packets.len(), 100);
    assert!(
        packets
            .iter()
            .enumerate()
            .all(|(i, p)| p.pts.get() == i as i64 * 503 && p.dur.get() == 503)
    );

    // Decoded, the sample values are the sample index.
    let decoded = decode_all(&params, &packets).concat();
    assert_eq!(decoded.len(), 2 * 50_300);
    assert!(
        decoded
            .chunks(2)
            .enumerate()
            .all(|(i, s)| s[0] == f32::from(i as i16) / 32768.0 && s[0] == s[1])
    );

    check_seeks(
        || Box::new(open(out.clone())),
        0,
        &[0, 1, 502, 503, 20_000, 49_000, 3000],
        Some(0.0),
    );
}

#[test]
fn lpcm_24_bit_with_packets_that_split_groups() {
    // 96 kHz stereo 24-bit, with a sample group (2 samples) of 12 bytes. The packets carry 100
    // bytes of audio, so the groups are split between packets. The first packet begins with the end
    // of an access unit of the previous packet (6 bytes).
    let n_groups = 400;
    let mut audio = vec![0xee; 6];

    for g in 0..n_groups {
        // The decoded value of a sample is the group, the index of the sample, then 0xa0 + index.
        let v: [[u8; 3]; 4] = [
            [g as u8, 0x01, 0xa0],
            [g as u8, 0x02, 0xa1],
            [g as u8, 0x03, 0xa2],
            [g as u8, 0x04, 0xa3],
        ];

        // The 16 bit words of s0c0, s0c1, s1c0, s1c1, then their low bytes.
        for s in &v {
            audio.extend_from_slice(&s[..2]);
        }
        for s in &v {
            audio.push(s[2]);
        }
    }

    let mut mux = PsMux::new(false);
    mux.pack(0);

    let mut pos = 0;
    while pos < audio.len() {
        let n = 100.min(audio.len() - pos);

        // The first access unit is 6 bytes after the audio of the first packet.
        let ptr: u16 = if pos == 0 { 4 + 6 } else { 4 + 1 };
        let mut payload = vec![0xa0, 1];
        payload.extend_from_slice(&ptr.to_be_bytes());
        payload.extend_from_slice(&[0x00, 0x80 | 0x10 | 0x01, 0x80]);
        payload.extend_from_slice(&audio[pos..pos + n]);

        let samples_before = pos.saturating_sub(6) / 6;
        mux.pes(0xbd, Some(90_000 + (samples_before * 90_000 / 96_000) as u64), &payload);
        pos += n;
    }

    let mut reader = open(mux.out);
    let params = audio_params(&reader, 0);
    assert_eq!(params.codec, CODEC_ID_PCM_S24BE);
    assert_eq!(params.sample_rate, Some(96_000));

    let packets = read_all(&mut reader);
    let data: Vec<u8> = packets.iter().flat_map(|p| p.data.iter().copied()).collect();
    let total: u64 = packets.iter().map(|p| p.dur.get()).sum();
    assert_eq!(total, 2 * n_groups as u64);
    assert_eq!(data.len(), 2 * n_groups * 2 * 3);

    for g in 0..n_groups {
        for s in 0..2 {
            for c in 0..2 {
                let i = (g * 4 + s * 2 + c) * 3;
                let ch = (s * 2 + c) as u8 + 1;
                assert_eq!(
                    &data[i..i + 3],
                    &[g as u8, ch, 0xa0 + (s * 2 + c) as u8],
                    "g={g} s={s} c={c}"
                );
            }
        }
    }

    // The timeline is continuous.
    let mut next = 0;
    for p in &packets {
        assert_eq!(p.pts.get(), next);
        next += p.dur.get() as i64;
    }
}

#[test]
fn non_seekable_streams_can_only_seek_forward() {
    let frames = mpa_frames(200);
    let mut mux = PsMux::new(false);
    mux.pack(0);
    mux_mpa(&mut mux, 0xc0, &frames, 2, 0);

    let mss = MediaSourceStream::new(
        Box::new(ReadOnlySource::new(Cursor::new(mux.out))),
        Default::default(),
    );

    let mut reader = MpegPsReader::try_new(mss, FormatOptions::default()).unwrap();
    assert_eq!(reader.tracks()[0].duration, None);

    for _ in 0..10 {
        reader.next_packet().unwrap().unwrap();
    }

    let target = 100 * 1152;
    let seeked = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(target), track_id: 0xc0 })
        .unwrap();
    assert!(seeked.actual_ts.get() <= target);
    assert!(target - seeked.actual_ts.get() <= 12 * 1152);

    let err = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(1152), track_id: 0xc0 })
        .unwrap_err();
    assert!(matches!(err, Error::SeekError(_)));
}

#[test]
fn probe_selects_the_program_stream_reader() {
    for mpeg1 in [false, true] {
        let mut mux = PsMux::new(mpeg1);
        mux.pack(0);
        mux_mpa(&mut mux, 0xc0, &mpa_frames(40), 2, 0);

        let mut probe = Probe::new();
        probe.register_format::<MpegPsReader<'_>>();

        for prefix in [0usize, 13] {
            let mut data = vec![0x55u8; prefix];
            data.extend_from_slice(&mux.out);

            let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
            let mut reader = probe
                .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
                .unwrap();

            assert_eq!(reader.format_info().short_name, "mpegps");
            assert_eq!(read_all(&mut *reader).len(), 40, "mpeg1={mpeg1} prefix={prefix}");
        }
    }
}

#[test]
fn score() {
    let score = |data: &[u8]| {
        let mut mss =
            MediaSourceStream::new(Box::new(Cursor::new(data.to_vec())), Default::default());
        MpegPsReader::score(ScopedStream::new(&mut mss, 16 * 1024)).unwrap()
    };

    // A program stream with audio.
    let mut mux = PsMux::new(false);
    mux.pack(0);
    mux_mpa(&mut mux, 0xc0, &mpa_frames(20), 2, 0);
    assert!(matches!(score(&mux.out), Score::Supported(255)));

    // A program stream with only video is claimed with a low score.
    let mut mux = PsMux::new(false);
    mux.pack(0);
    for _ in 0..10 {
        mux.pes(0xe0, Some(0), &[0u8; 1500]);
    }
    assert!(matches!(score(&mux.out), Score::Supported(100)));

    // The pack header start code in other data.
    let mut data = vec![0, 0, 1, 0xba, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
    data.extend(std::iter::repeat_n(0x33, 500));
    assert!(matches!(score(&data), Score::Unsupported));

    // An MPEG audio stream.
    assert!(matches!(score(&mpa_frames(20).concat()), Score::Unsupported));
}

#[test]
fn streams_without_supported_audio_are_rejected() {
    let mut mux = PsMux::new(false);
    mux.pack(0);
    for _ in 0..20 {
        mux.pes(0xe0, Some(90_000), &[0u8; 3000]);
        mux.pes(0xbd, Some(90_000), &[&[0x80u8, 1, 0, 2][..], &[0u8; 500][..]].concat());
    }

    let mss = MediaSourceStream::new(Box::new(Cursor::new(mux.out)), Default::default());
    assert!(MpegPsReader::try_new(mss, FormatOptions::default()).is_err());
}

/// Read the stream, and seek in it, without panicking or running forever.
fn exercise(data: Vec<u8>, track_id: u32, rng: &mut Rng) {
    let Ok(mut reader) = MpegPsReader::try_new(
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
    let mut mux = PsMux::new(false);
    mux.pack(0);
    mux_mpa(&mut mux, 0xc0, &mpa_frames(60), 2, 0);

    for n in 0..30usize {
        mux.pes(
            0xbd,
            Some(45_000 + (n * 503 * 90_000 / 48_000) as u64),
            &lpcm_payload(n * 503, 503, 4),
        );
    }

    let mut rng = Rng(0x1234_5678_9abc_def1);

    for _ in 0..300 {
        let track = if rng.below(2) == 0 { 0xc0 } else { 0xbda0 };
        exercise(corrupt(&mux.out, &mut rng), track, &mut rng);
    }
}
