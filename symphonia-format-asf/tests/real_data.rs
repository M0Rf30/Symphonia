// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Demuxes WMA files in ASF (written by `ffmpeg -c:a wmav1|wmav2 x.wma`), decodes them with the
//! WMA decoder and compares the result with FFmpeg's decode of the same file
//! (`ffmpeg -i x.wma -f s16le`, stored as `*.ref.s16`).

use std::fs::File;
use std::path::Path;

use symphonia_codec_wma::WmaDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_WMAV1, CODEC_ID_WMAV2};
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::probe::Hint;
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia_core::meta::StandardTag;
use symphonia_core::units::Timestamp;
use symphonia_format_asf::AsfReader;

fn open(name: &str) -> AsfReader<'static> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    let mss = MediaSourceStream::new(
        Box::new(File::open(path).unwrap()),
        MediaSourceStreamOptions::default(),
    );
    AsfReader::try_new(mss, FormatOptions::default()).unwrap()
}

fn decode_all(name: &str) -> (Vec<f32>, usize, usize) {
    let mut reader = open(name);
    let track = reader.first_track(TrackType::Audio).unwrap();
    let id = track.id;
    let Some(CodecParameters::Audio(params)) = &track.codec_params
    else {
        panic!("no audio parameters");
    };
    let channels = params.channels.as_ref().unwrap().count();
    let frame_len = if params.sample_rate.unwrap() <= 22050 { 1024 } else { 2048 };
    let mut dec = WmaDecoder::try_new(params, &AudioDecoderOptions::default()).unwrap();

    let mut out = Vec::new();
    let mut last_pts = None;
    while let Some(packet) = reader.next_packet().unwrap() {
        assert_eq!(packet.track_id, id);
        if let Some(last) = last_pts {
            assert!(packet.pts.get() > last, "timestamps increase");
        }
        last_pts = Some(packet.pts.get());
        let GenericAudioBufferRef::F32(buf) = dec.decode(&packet).unwrap()
        else {
            panic!("expected f32 output");
        };
        let planes: Vec<&[f32]> = (0..channels).map(|c| buf.plane(c).unwrap()).collect();
        for i in 0..buf.frames() {
            for p in &planes {
                out.push(p[i]);
            }
        }
    }
    (out, channels, frame_len)
}

fn compare(name: &str) {
    let (out, channels, frame_len) = decode_all(&format!("{name}.wma"));
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{name}.ref.s16"));
    let reference: Vec<i16> = std::fs::read(path)
        .unwrap()
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();

    // FFmpeg does not drop the delay, and outputs the last frame, which the decoder does not.
    assert_eq!(out.len() + frame_len * channels, reference.len(), "{name}: sample count");

    let (mut sig, mut err, mut max) = (0f64, 0f64, 0f64);
    for (&a, &r) in out.iter().zip(reference.iter()) {
        let a = (f64::from(a) * 32768.0).round().clamp(-32768.0, 32767.0);
        let r = f64::from(r);
        sig += r * r;
        err += (a - r).powi(2);
        max = max.max((a - r).abs());
    }
    let snr = 10.0 * (sig / err.max(1e-9)).log10();
    println!("{name}: SNR {snr:.1} dB, max diff {max} LSB");
    assert!(snr >= 100.0, "{name}: SNR {snr:.1} dB");
    assert!(max <= 1.0, "{name}: max diff {max} LSB");
}

#[test]
fn stereo_v2() {
    compare("stereo_v2");
}

#[test]
fn mono_v1() {
    compare("mono_v1");
}

#[test]
fn track_parameters() {
    let reader = open("stereo_v2.wma");
    assert_eq!(reader.tracks().len(), 1);
    let track = &reader.tracks()[0];
    let Some(CodecParameters::Audio(params)) = &track.codec_params
    else {
        panic!("no audio parameters");
    };
    assert_eq!(params.codec, CODEC_ID_WMAV2);
    assert_eq!(params.sample_rate, Some(44100));
    assert_eq!(params.channels.as_ref().unwrap().count(), 2);
    // 4 seconds of audio.
    let frames = track.num_frames.unwrap();
    assert!((175_000..=179_000).contains(&frames), "{frames}");

    let reader = open("mono_v1.wma");
    let Some(CodecParameters::Audio(params)) = &reader.tracks()[0].codec_params
    else {
        panic!("no audio parameters");
    };
    assert_eq!(params.codec, CODEC_ID_WMAV1);
    assert_eq!(params.sample_rate, Some(22050));
    assert_eq!(params.channels.as_ref().unwrap().count(), 1);
}

#[test]
fn tags() {
    let mut reader = open("stereo_v2.wma");
    let meta = reader.metadata();
    let rev = meta.current().unwrap();
    let find = |want: &str| {
        rev.media.tags.iter().find_map(|t| match (&t.std, want) {
            (Some(StandardTag::TrackTitle(v)), "title") => Some(v.to_string()),
            (Some(StandardTag::Artist(v)), "artist") => Some(v.to_string()),
            (Some(StandardTag::Album(v)), "album") => Some(v.to_string()),
            _ => None,
        })
    };
    assert_eq!(find("title").as_deref(), Some("Test Title"));
    assert_eq!(find("artist").as_deref(), Some("Test Artist"));
    assert_eq!(find("album").as_deref(), Some("Test Album"));
}

#[test]
fn seek() {
    let mut reader = open("stereo_v2.wma");
    let id = reader.tracks()[0].id;
    let mut count = 0usize;
    while reader.next_packet().unwrap().is_some() {
        count += 1;
    }
    assert!(count > 50);

    for target in [0i64, 44100, 100_000, 170_000] {
        let seeked = reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp { ts: Timestamp::new(target), track_id: id },
            )
            .unwrap();
        assert_eq!(seeked.required_ts.get(), target);
        assert!(seeked.actual_ts.get() <= target, "{target}: {}", seeked.actual_ts.get());
        assert!(target - seeked.actual_ts.get() < 4096, "{target}: {}", seeked.actual_ts.get());
        let packet = reader.next_packet().unwrap().unwrap();
        assert_eq!(packet.pts, seeked.actual_ts);
    }
}

#[test]
fn rejects_garbage() {
    for data in [vec![], vec![0u8; 100], b"0&\xb2u\x8ef\xcf\x11".to_vec()] {
        let mss = MediaSourceStream::new(
            Box::new(std::io::Cursor::new(data)),
            MediaSourceStreamOptions::default(),
        );
        assert!(AsfReader::try_new(mss, FormatOptions::default()).is_err());
    }
    let _ = Hint::new();
}
