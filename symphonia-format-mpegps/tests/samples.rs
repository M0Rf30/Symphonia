// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests with real files. They require the `RMPD_SAMPLES` environment variable to be set to the
//! directory with the sample files, and are skipped otherwise.
//!
//! Besides the program stream in the samples, the elementary streams of other samples are
//! multiplexed into program streams, and the packets demultiplexed must be identical to those of
//! the native demuxer of the elementary stream.

mod support;

use std::fs::File;

use symphonia_bundle_mp3::MpaReader;
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;
use symphonia_format_mpegps::MpegPsReader;

use support::*;

fn open_file(path: &std::path::Path) -> MediaSourceStream<'static> {
    MediaSourceStream::new(Box::new(File::open(path).unwrap()), Default::default())
}

fn ticks_to_pts(ticks: u64, rate: u64, base: u64) -> u64 {
    (base + ticks * 90_000 / rate) % (1 << 33)
}

#[test]
fn mp2_in_mpegps_sample() {
    let Some(path) = sample("unsupported/mp2_in_mpegps.mpg")
    else {
        return;
    };

    let open_reader = || -> Box<dyn FormatReader> {
        Box::new(MpegPsReader::try_new(open_file(&path), FormatOptions::default()).unwrap())
    };

    let mut reader = open_reader();
    assert_eq!(reader.tracks().len(), 1);

    let track = reader.tracks()[0].clone();
    let params = audio_params(&*reader, 0);

    assert_eq!(track.id, 0xc0);
    assert_eq!(params.codec, symphonia_core::codecs::audio::well_known::CODEC_ID_MP2);
    assert_eq!(params.sample_rate, Some(44_100));
    assert_eq!(params.channels.as_ref().unwrap().count(), 2);
    assert_eq!(track.start_ts.get(), 0);

    let packets = read_all(&mut *reader);

    // The timeline is contiguous, and matches the duration.
    let mut next = 0;
    for p in &packets {
        assert_eq!(p.pts.get(), next);
        assert_eq!(p.dur.get(), 1152);
        next += 1152;
    }

    assert_eq!(track.duration.unwrap().get() as i64, next);
    assert!((29.9..30.1).contains(&(next as f64 / 44_100.0)), "{}", next as f64 / 44_100.0);

    let decoded = decode_all(&params, &packets);
    assert!(decoded.iter().all(|d| d.len() == 2 * 1152));

    check_seeks(
        open_reader,
        0,
        &[0, 1, 50_000, 700_000, 100_000, next - 5000, next / 3],
        Some(0.0),
    );
}

#[test]
fn mpa_in_mpegps_matches_mpa_reader() {
    for name in
        ["mp3/mp3_cbr128.mp3", "mp2/mp2_192_st.mp2", "mp2/mp2_22k_lsf.mp2", "mp2/mp1_from_mpg.mp1"]
    {
        let Some(path) = sample(name)
        else {
            continue;
        };

        let mut native = MpaReader::try_new(open_file(&path), FormatOptions::default()).unwrap();
        let params = audio_params(&native, 0);
        let rate = u64::from(params.sample_rate.unwrap());
        let want = read_all(&mut native);

        let mut es = vec![];
        let mut starts = vec![];
        for p in &want {
            starts.push(es.len());
            es.extend_from_slice(&p.data);
        }

        let frame = u64::from(
            symphonia_common::mpeg::audio::mpa::parse_header_bytes(&want[0].data)
                .unwrap()
                .samples_per_frame,
        );

        for mpeg1 in [false, true] {
            let mut mux = PsMux::new(mpeg1);
            mux.pack(0);
            mux.system_header();

            let mut off = 0;
            let mut next_frame = 0;

            for (n, chunk) in chunks_of(&es, &[2000, 2028, 977]).enumerate() {
                if n % 2 == 0 {
                    mux.pack(n as u64 * 4000);
                }

                let mut pts = None;
                while next_frame < starts.len() && starts[next_frame] < off + chunk.len() {
                    if starts[next_frame] >= off && pts.is_none() {
                        pts = Some(ticks_to_pts(next_frame as u64 * frame, rate, 63_000));
                    }
                    next_frame += 1;
                }

                mux.pes(0xc0, pts, chunk);
                off += chunk.len();
            }

            let out = mux.out;
            let mut reader = open(out.clone());
            let got = read_all(&mut reader);
            assert_eq!(got.len(), want.len(), "{name} mpeg1={mpeg1}");

            for (i, (g, w)) in got.iter().zip(&want).enumerate() {
                assert_eq!(g.data, w.data, "{name} packet {i}");
                assert_eq!(g.dur.get(), frame, "{name} packet {i}");
                assert_eq!(g.pts.get(), i as i64 * frame as i64, "{name} packet {i}");
            }

            assert_eq!(reader.tracks()[0].duration.unwrap().get(), want.len() as u64 * frame);

            let n = want.len() as i64 * frame as i64;
            check_seeks(
                || Box::new(open(out.clone())),
                0,
                &[0, 777, n / 2, n / 3, n - 4000, n / 9],
                Some(0.0),
            );
        }
    }
}

#[test]
fn corrupt_samples_do_not_panic() {
    for name in ["unsupported/mp2_in_mpegps.mpg"] {
        let Some(path) = sample(name)
        else {
            continue;
        };

        // The start of the file, to keep the test short.
        let mut data = std::fs::read(&path).unwrap();
        data.truncate(300_000);

        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);

        for _ in 0..60 {
            let d = corrupt(&data, &mut rng);

            let Ok(mut reader) = MpegPsReader::try_new(
                MediaSourceStream::new(Box::new(std::io::Cursor::new(d)), Default::default()),
                FormatOptions::default(),
            )
            else {
                continue;
            };

            let mut n = 0;
            while let Ok(Some(_)) = reader.next_packet() {
                n += 1;
                assert!(n < 1_000_000);
            }

            for _ in 0..3 {
                let ts = symphonia_core::units::Timestamp::new(rng.below(3_000_000) as i64);
                let _ = reader.seek(
                    symphonia_core::formats::SeekMode::Accurate,
                    symphonia_core::formats::SeekTo::Timestamp { ts, track_id: 0xc0 },
                );
                let _ = reader.next_packet();
            }
        }
    }
}
