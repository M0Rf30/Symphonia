// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests with real files. They require the `RMPD_SAMPLES` environment variable to be set to the
//! directory with the sample files, and are skipped otherwise.
//!
//! Besides the FLV files in the samples, the elementary streams of other samples are multiplexed
//! into FLV files, and the packets demultiplexed must be identical to those of the native demuxer.

mod support;

use std::fs::File;

use symphonia_bundle_mp3::MpaReader;
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;
use symphonia_format_flv::FlvReader;
use symphonia_format_isomp4::IsoMp4Reader;

use support::*;

fn open_file(path: &std::path::Path) -> MediaSourceStream<'static> {
    MediaSourceStream::new(Box::new(File::open(path).unwrap()), Default::default())
}

fn ms(ts: u64, rate: u64) -> u32 {
    (ts * 1000 / rate) as u32
}

/// Check a sample FLV: contiguous timeline, duration, decode, and seeks.
fn check_sample(
    name: &str,
    codec: symphonia_core::codecs::audio::AudioCodecId,
    tolerance: Option<f32>,
) {
    let Some(path) = sample(name)
    else {
        return;
    };

    let open_reader = || -> Box<dyn FormatReader> {
        Box::new(FlvReader::try_new(open_file(&path), FormatOptions::default()).unwrap())
    };

    let mut reader = open_reader();
    assert_eq!(reader.tracks().len(), 1);

    let track = reader.tracks()[0].clone();
    let params = audio_params(&*reader, 0);

    assert_eq!(params.codec, codec, "{name}");
    assert_eq!(params.sample_rate, Some(44_100));
    assert_eq!(params.channels.as_ref().unwrap().count(), 2);
    assert_eq!(track.start_ts.get(), 0);

    let packets = read_all(&mut *reader);
    let mut next = 0;
    for p in &packets {
        assert_eq!(p.pts.get(), next, "{name}");
        next += p.dur.get() as i64;
    }

    // The duration is that of the audio.
    assert_eq!(track.duration.unwrap().get() as i64, next, "{name}");
    assert!((29.9..30.2).contains(&(next as f64 / 44_100.0)), "{name}: {}", next as f64 / 44_100.0);

    let decoded = decode_all(&params, &packets);
    assert!(decoded.iter().skip(2).all(|d| !d.is_empty()), "{name}");

    check_seeks(
        open_reader,
        0,
        &[0, 1, 50_000, 700_000, 100_000, next - 5000, next / 3],
        tolerance,
    );
}

#[test]
fn aac_in_flv_sample() {
    check_sample(
        "unsupported/aac_in_flv.flv",
        symphonia_core::codecs::audio::well_known::CODEC_ID_AAC,
        Some(0.2),
    );
}

#[test]
fn mp3_in_flv_sample() {
    check_sample(
        "unsupported/mp3_in_flv.flv",
        symphonia_core::codecs::audio::well_known::CODEC_ID_MP3,
        Some(0.0),
    );
}

#[test]
fn unsupported_codecs_are_rejected() {
    for name in ["unsupported/adpcm_swf_flv.flv", "unsupported/nellymoser_flv.flv"] {
        let Some(path) = sample(name)
        else {
            continue;
        };

        let err = FlvReader::try_new(open_file(&path), FormatOptions::default()).err().unwrap();
        assert!(matches!(err, symphonia_core::errors::Error::Unsupported(_)), "{name}: {err}");
    }
}

#[test]
fn aac_in_flv_matches_isomp4_reader() {
    for name in ["aac/aac_lc_fdk_m4a.m4a", "aac/heaac_v1_fdk_m4a.m4a", "aac/aac_lc_fdk_mono.m4a"] {
        let Some(path) = sample(name)
        else {
            continue;
        };

        let mut native = IsoMp4Reader::try_new(open_file(&path), FormatOptions::default()).unwrap();
        let params = audio_params(&native, 0);
        let asc = params.extra_data.clone().unwrap();
        // The timeline of FLV is in decoded frames, like that of ISO MP4: at the output rate of
        // the stream, twice the core rate of the SBR streams (the time stamps of the tags are in
        // time, so those are in core frames).
        let rate = u64::from(native.tracks()[0].time_base.unwrap().denom.get());
        let ratio = if name.contains("heaac") { 2 } else { 1 };
        let core_rate = rate / ratio;
        let want = read_all(&mut native);

        let mut mux = FlvMux::new();
        mux.tag(18, 0, &on_metadata(&[amf_prop("duration", &amf_number(1.0))]));
        mux.aac(0, 0, &asc);

        for (i, p) in want.iter().enumerate() {
            mux.video(ms(i as u64 * 1024, core_rate), 100);
            mux.aac(ms(i as u64 * 1024, core_rate), 1, &p.data);
        }

        let out = mux.out;
        let mut reader = open(out.clone());
        let got = read_all(&mut reader);
        assert_eq!(got.len(), want.len(), "{name}");

        for (i, (g, w)) in got.iter().zip(&want).enumerate() {
            assert_eq!(g.data, w.data, "{name} packet {i}");
            assert_eq!(g.dur.get(), 1024 * ratio, "{name} packet {i}");
            assert_eq!(g.pts.get(), (i as u64 * 1024 * ratio) as i64, "{name} packet {i}");
        }

        let track = reader.tracks()[0].clone();
        assert_eq!(u64::from(track.time_base.unwrap().denom.get()), rate);
        assert_eq!(track.duration.unwrap().get(), want.len() as u64 * 1024 * ratio);

        let rparams = audio_params(&reader, 0);
        assert_eq!(rparams.extra_data, params.extra_data);
        assert_eq!(rparams.sample_rate, params.sample_rate);

        // Seeks reproduce a continuous decode, including HE-AAC.
        let n = want.len() as i64 * 1024 * ratio as i64;
        check_seeks(
            || Box::new(open(out.clone())),
            0,
            &[0, 1023, 1024, n / 3, n / 7, n - 5000, n / 2 + 77],
            Some(0.0),
        );
    }
}

#[test]
fn mp3_in_flv_matches_mpa_reader() {
    for name in ["mp3/mp3_cbr128.mp3", "mp3/mp3_24k.mp3", "mp3/mp3_cbr64_mono.mp3"] {
        let Some(path) = sample(name)
        else {
            continue;
        };

        let mut native = MpaReader::try_new(open_file(&path), FormatOptions::default()).unwrap();
        let params = audio_params(&native, 0);
        let rate = u64::from(params.sample_rate.unwrap());
        let want = read_all(&mut native);
        let frame = u64::from(
            symphonia_common::mpeg::audio::mpa::parse_header_bytes(&want[0].data)
                .unwrap()
                .samples_per_frame,
        );

        let mut mux = FlvMux::new();
        for (i, p) in want.iter().enumerate() {
            mux.audio(ms(i as u64 * frame, rate), 0x2f, &p.data);
        }

        let out = mux.out;
        let mut reader = open(out.clone());
        let got = read_all(&mut reader);
        assert_eq!(got.len(), want.len(), "{name}");

        for (i, (g, w)) in got.iter().zip(&want).enumerate() {
            assert_eq!(g.data, w.data, "{name} packet {i}");
            assert_eq!(g.pts.get(), i as i64 * frame as i64, "{name} packet {i}");
        }

        assert_eq!(reader.tracks()[0].duration.unwrap().get(), want.len() as u64 * frame);
        assert_eq!(audio_params(&reader, 0).sample_rate, params.sample_rate);

        let n = want.len() as i64 * frame as i64;
        check_seeks(
            || Box::new(open(out.clone())),
            0,
            &[0, 777, n / 2, n / 3, n - 4000, n / 9],
            Some(0.0),
        );
    }
}

#[test]
fn corrupt_samples_do_not_panic() {
    for name in ["unsupported/aac_in_flv.flv", "unsupported/mp3_in_flv.flv"] {
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

            let Ok(mut reader) = FlvReader::try_new(
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
                    symphonia_core::formats::SeekTo::Timestamp { ts, track_id: 0 },
                );
                let _ = reader.next_packet();
            }
        }
    }
}
