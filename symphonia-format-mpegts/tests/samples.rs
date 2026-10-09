// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests with real files. They require the `RMPD_SAMPLES` environment variable to be set to the
//! directory with the sample files, and are skipped otherwise.
//!
//! Besides the transport stream in the samples, the elementary streams of other samples are
//! multiplexed into transport streams, and the packets demultiplexed must be identical to those
//! of the native demuxer of the elementary stream.

mod support;

use std::fs::File;

use symphonia_bundle_mp3::MpaReader;
use symphonia_codec_aac::{AdtsReader, LoasReader};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::packet::Packet;
use symphonia_core::units::Timestamp;
use symphonia_format_mpegts::MpegTsReader;
use symphonia_format_ogg::OggReader;

use support::*;

fn open_file(path: &std::path::Path) -> MediaSourceStream<'static> {
    MediaSourceStream::new(Box::new(File::open(path).unwrap()), Default::default())
}

/// Multiplex the elementary stream `es` into PES packets of the sizes `sizes`, cyclically. The
/// PTS of each PES packet is that of the first frame (of those that begin at the `starts` offsets)
/// that starts in it.
fn mux_es(
    mux: &mut TsMux,
    pid: u16,
    stream_id: u8,
    es: &[u8],
    starts: &[usize],
    pts_of: impl Fn(usize) -> u64,
    sizes: &[usize],
    psi: &[StreamSpec],
) {
    let mut off = 0;
    let mut next_frame = 0;

    for (n, chunk) in chunks_of(es, sizes).enumerate() {
        if n % 32 == 0 {
            mux.psi(0x1000, psi);
        }

        let mut pts = None;

        while next_frame < starts.len() && starts[next_frame] < off + chunk.len() {
            if starts[next_frame] >= off && pts.is_none() {
                pts = Some(pts_of(next_frame));
            }
            next_frame += 1;
        }

        mux.pes(pid, stream_id, pts, chunk);
        off += chunk.len();
    }
}

fn ticks_to_pts(ticks: u64, rate: u64, base: u64) -> u64 {
    (base + ticks * 90_000 / rate) % (1 << 33)
}

fn assert_same_packets(got: &[Packet], want: &[Packet]) {
    assert_eq!(got.len(), want.len(), "number of packets");

    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        assert_eq!(g.data, w.data, "packet {i} data");
        assert_eq!(g.dur, w.dur, "packet {i} dur");
        assert_eq!(g.pts, w.pts, "packet {i} pts");
    }
}

#[test]
fn aac_in_mpegts_sample() {
    let Some(path) = sample("unsupported/aac_in_mpegts.ts")
    else {
        return;
    };

    let open_reader = || -> Box<dyn FormatReader> {
        Box::new(MpegTsReader::try_new(open_file(&path), FormatOptions::default()).unwrap())
    };

    let mut reader = open_reader();
    let track = reader.tracks()[0].clone();
    let params = audio_params(&*reader, 0);

    assert_eq!(reader.tracks().len(), 1);
    assert_eq!(track.id, 0x100);
    assert_eq!(params.sample_rate, Some(44_100));
    assert_eq!(params.channels.as_ref().unwrap().count(), 2);
    assert_eq!(track.start_ts.get(), 0);

    let packets = read_all(&mut *reader);
    assert_eq!(packets.len(), 1293);

    // The timeline is contiguous, and matches the duration.
    let mut next = 0;
    for p in &packets {
        assert_eq!(p.pts.get(), next);
        next += p.dur.get() as i64;
    }
    assert_eq!(track.duration.unwrap().get() as i64, next);
    assert_eq!(track.num_frames, Some(next as u64));

    // All of the packets decode.
    let decoded = decode_all(&params, &packets);
    assert!(decoded.iter().skip(1).all(|d| d.len() == 2 * 1024));

    check_seeks(
        open_reader,
        0,
        &[0, 1, 50_000, 700_000, 100_000, 1_323_000, 20_000_000 / 17],
        Some(0.2),
    );
}

#[test]
fn adts_in_mpegts_matches_adts_reader() {
    for name in
        ["aac/aac_lc_fdk_adts.aac", "aac/heaac_v1_fdk_adts.aac", "aac/aac_lc_fdk_adts_crc.aac"]
    {
        let Some(path) = sample(name)
        else {
            continue;
        };
        let es = std::fs::read(&path).unwrap();

        let mut native = AdtsReader::try_new(open_file(&path), FormatOptions::default()).unwrap();
        let rate = audio_params(&native, 0).sample_rate.unwrap();
        let want = read_all(&mut native);

        // The offsets of the frames.
        let mut starts = vec![];
        let mut pos = 0;
        while pos + 7 <= es.len() && es[pos] == 0xff && es[pos + 1] & 0xf6 == 0xf0 {
            starts.push(pos);
            pos += (usize::from(es[pos + 3] & 3) << 11)
                | (usize::from(es[pos + 4]) << 3)
                | usize::from(es[pos + 5] >> 5);
        }
        assert_eq!(starts.len(), want.len(), "{name}");

        // An arbitrary start PTS near the wrap.
        let base = (1u64 << 33) - 45_000;
        let specs = [StreamSpec { stream_type: 0x0f, pid: 0x101, descriptors: vec![] }];

        for sizes in [&[1400usize][..], &[333, 5000, 17, 2900]] {
            let mut mux = TsMux::new(188);
            mux_es(
                &mut mux,
                0x101,
                0xc0,
                &es,
                &starts,
                |k| ticks_to_pts(k as u64 * 1024, u64::from(rate), base),
                sizes,
                &specs,
            );

            let mut reader = open(mux.out.clone());
            let got = read_all(&mut reader);

            // The last frame is only complete if the stream ends on the frame boundary, which it
            // does.
            assert_same_packets(&got, &want);

            let track = &reader.tracks()[0];
            assert_eq!(track.duration.unwrap().get(), want.len() as u64 * 1024, "{name}");
        }

        // Seeking is as exact as the native demuxer's.
        let mut mux = TsMux::new(188);
        mux_es(
            &mut mux,
            0x101,
            0xc0,
            &es,
            &starts,
            |k| ticks_to_pts(k as u64 * 1024, u64::from(rate), base),
            &[1400],
            &specs,
        );

        let out = mux.out;
        let n = want.len() as i64 * 1024;
        check_seeks(
            || Box::new(open(out.clone())),
            0,
            &[0, 1023, 1024, n / 3, n / 7, n - 5000, n / 2 + 77],
            Some(0.0),
        );
    }
}

#[test]
fn latm_in_mpegts_matches_loas_reader() {
    let Some(path) = sample("aac/aac_lc_fdk_latm.aac")
    else {
        return;
    };
    let es = std::fs::read(&path).unwrap();

    let mut native = LoasReader::try_new(open_file(&path), FormatOptions::default()).unwrap();
    let rate = audio_params(&native, 0).sample_rate.unwrap();
    let want = read_all(&mut native);

    // The offsets of the LOAS frames.
    let mut starts = vec![];
    let mut pos = 0;
    while pos + 3 <= es.len() && es[pos] == 0x56 && es[pos + 1] & 0xe0 == 0xe0 {
        starts.push(pos);
        pos += 3 + (usize::from(es[pos + 1] & 0x1f) << 8 | usize::from(es[pos + 2]));
    }
    assert_eq!(starts.len(), want.len());

    let specs = [StreamSpec { stream_type: 0x11, pid: 0x101, descriptors: vec![] }];
    let mut mux = TsMux::new(188);
    mux_es(
        &mut mux,
        0x101,
        0xc0,
        &es,
        &starts,
        |k| ticks_to_pts(k as u64 * 1024, u64::from(rate), 90_000),
        &[1500, 400],
        &specs,
    );

    let mut reader = open(mux.out.clone());
    let got = read_all(&mut reader);
    assert_same_packets(&got, &want);

    let out = mux.out;
    let n = want.len() as i64 * 1024;
    check_seeks(|| Box::new(open(out.clone())), 0, &[0, n / 2, n / 5, n - 2048], Some(0.0));
}

#[test]
fn mpa_in_mpegts_matches_mpa_reader() {
    for (name, stream_type) in
        [("mp3/mp3_cbr128.mp3", 0x03), ("mp2/mp2_192_st.mp2", 0x04), ("mp3/mp3_24k.mp3", 0x03)]
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

        let specs = [StreamSpec { stream_type, pid: 0x101, descriptors: vec![] }];
        let frame = u64::from(
            symphonia_common::mpeg::audio::mpa::parse_header_bytes(&want[0].data)
                .unwrap()
                .samples_per_frame,
        );

        for (size, ps) in [(188usize, 1usize), (192, 2), (204, 3)] {
            let mut mux = TsMux::new(size);
            mux_es(
                &mut mux,
                0x101,
                0xc0,
                &es,
                &starts,
                |k| ticks_to_pts(k as u64 * frame, rate, 3_600_000),
                &[2000 * ps, 800],
                &specs,
            );

            let mut reader = open(mux.out.clone());
            let got = read_all(&mut reader);
            assert_eq!(got.len(), want.len(), "{name}");

            for (i, (g, w)) in got.iter().zip(&want).enumerate() {
                assert_eq!(g.data, w.data, "{name} packet {i}");
                assert_eq!(g.dur.get(), frame, "{name} packet {i}");
                assert_eq!(g.pts.get(), i as i64 * frame as i64, "{name} packet {i}");
            }

            assert_eq!(reader.tracks()[0].duration.unwrap().get(), want.len() as u64 * frame);
        }

        let mut mux = TsMux::new(188);
        mux_es(
            &mut mux,
            0x101,
            0xc0,
            &es,
            &starts,
            |k| ticks_to_pts(k as u64 * frame, rate, 0),
            &[1800],
            &specs,
        );

        let out = mux.out;
        let n = want.len() as i64 * frame as i64;
        check_seeks(
            || Box::new(open(out.clone())),
            0,
            &[0, 500, n / 2, n / 3, n - 4000, n / 9],
            Some(0.0),
        );
    }
}

#[test]
fn opus_in_mpegts_matches_ogg_reader() {
    for name in ["opus/opus_cbr_96.opus", "opus/opus_ch3.opus", "opus/opus_ch6.opus"] {
        let Some(path) = sample(name)
        else {
            continue;
        };

        let mut native = OggReader::try_new(open_file(&path), FormatOptions::default()).unwrap();
        let channels = audio_params(&native, 0).channels.as_ref().unwrap().count() as u8;
        let want = read_all(&mut native);

        let mut es = vec![];
        let mut starts = vec![];
        let mut ts = vec![];
        let mut t = 0u64;

        for p in &want {
            starts.push(es.len());
            ts.push(t);
            t += p.block_dur().get();

            // The access unit.
            es.extend_from_slice(&[0x7f, 0xe0]);
            let mut n = p.data.len();
            while n >= 255 {
                es.push(255);
                n -= 255;
            }
            es.push(n as u8);
            es.extend_from_slice(&p.data);
        }

        let specs =
            [StreamSpec { stream_type: 0x06, pid: 0x101, descriptors: opus_descriptors(channels) }];

        let mut mux = TsMux::new(188);
        mux_es(
            &mut mux,
            0x101,
            0xc0,
            &es,
            &starts,
            |k| ticks_to_pts(ts[k], 48_000, 90_000),
            &[900, 1700],
            &specs,
        );

        let mut reader = open(mux.out.clone());
        let track = reader.tracks()[0].clone();
        assert_eq!(
            audio_params(&reader, 0).channels.as_ref().unwrap().count(),
            usize::from(channels)
        );
        assert_eq!(track.time_base.unwrap().denom.get(), 48_000);

        let got = read_all(&mut reader);
        assert_eq!(got.len(), want.len(), "{name}");

        for (i, (g, w)) in got.iter().zip(&want).enumerate() {
            assert_eq!(g.data, w.data, "{name} packet {i}");
            assert_eq!(g.dur, w.block_dur(), "{name} packet {i}");
            assert_eq!(g.pts.get() as u64, ts[i], "{name} packet {i}");
        }

        // The packets decode.
        let params = audio_params(&reader, 0);
        let decoded = decode_all(&params, &got);
        assert!(decoded.iter().all(|d| !d.is_empty()), "{name}");

        let out = mux.out;
        let n = t as i64;
        check_seeks(|| Box::new(open(out.clone())), 0, &[0, n / 2, n / 3, n - 10_000], None);
    }
}

#[test]
fn seek_by_time() {
    let Some(path) = sample("unsupported/aac_in_mpegts.ts")
    else {
        return;
    };

    let mut reader = MpegTsReader::try_new(open_file(&path), FormatOptions::default()).unwrap();
    let seeked = reader
        .seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time: symphonia_core::units::Time::try_from_secs_f64(10.0).unwrap(),
                track_id: None,
            },
        )
        .unwrap();

    assert!(seeked.actual_ts.get() <= 441_000);
    assert!(441_000 - seeked.actual_ts.get() < 60 * 1024);

    // Past the end.
    assert!(
        reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp { ts: Timestamp::new(100_000_000), track_id: 0x100 }
            )
            .is_err()
    );
}

#[test]
fn corrupt_samples_do_not_panic() {
    for name in ["unsupported/aac_in_mpegts.ts"] {
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

            let Ok(mut reader) = MpegTsReader::try_new(
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
                    symphonia_core::formats::SeekTo::Timestamp { ts, track_id: 0x100 },
                );
                let _ = reader.next_packet();
            }
        }
    }
}
