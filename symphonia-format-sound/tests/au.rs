#![allow(clippy::unwrap_used)]
// Test code: a panic is a test failure.
// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
#![cfg(feature = "au")]

mod common;

use std::io::Cursor;

use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::*;
use symphonia_core::formats::probe::{Hint, Probe};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::meta::{MetadataOptions, StandardTag};
use symphonia_core::units::{Time, Timestamp};
use symphonia_format_sound::AuReader;

use common::{audio_params, drain, ffmpeg_pcm, fnv_of, mss};

fn probe() -> Probe {
    let mut probe = Probe::new();
    probe.register_format::<AuReader<'_>>();
    probe
}

/// Builds a big-endian AU file.
fn au_file(
    annotation: &[u8],
    data_size: u32,
    encoding: u32,
    rate: u32,
    channels: u32,
    data: &[u8],
) -> Vec<u8> {
    let mut file = b".snd".to_vec();

    for field in [24 + annotation.len() as u32, data_size, encoding, rate, channels] {
        file.extend_from_slice(&field.to_be_bytes());
    }

    file.extend_from_slice(annotation);
    file.extend_from_slice(data);
    file
}

fn open(file: Vec<u8>) -> Box<dyn FormatReader + 'static> {
    AuReader::try_new(mss(file), FormatOptions::default()).map(|r| Box::new(r) as _).unwrap()
}

/// 100 stereo frames of 16-bit samples with a distinct value in every sample.
fn stereo_s16() -> (Vec<u8>, Vec<u8>) {
    let data: Vec<u8> = (0..200i16).flat_map(|s| (s * 100 - 9000).to_be_bytes()).collect();
    (au_file(b"", data.len() as u32, 3, 44100, 2, &data), data)
}

#[test]
fn parses_header_and_frames() {
    let (file, data) = stereo_s16();
    let mut reader = open(file);

    let track = &reader.tracks()[0];
    let params = audio_params(track);

    assert_eq!(params.codec, CODEC_ID_PCM_S16BE);
    assert_eq!(params.sample_rate, Some(44100));
    assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(2));
    assert_eq!(track.num_frames, Some(100));

    let packets = drain(reader.as_mut());
    let bytes: Vec<u8> = packets.iter().flat_map(|p| p.data.to_vec()).collect();

    assert_eq!(bytes, data);
    assert_eq!(packets[0].pts.get(), 0);
    assert_eq!(packets[0].dur.get(), 100);
}

#[test]
fn encodings() {
    // (encoding, codec, bytes per frame of mono)
    let cases = [
        (1, CODEC_ID_PCM_MULAW, 1),
        (2, CODEC_ID_PCM_S8, 1),
        (3, CODEC_ID_PCM_S16BE, 2),
        (4, CODEC_ID_PCM_S24BE, 3),
        (5, CODEC_ID_PCM_S32BE, 4),
        (6, CODEC_ID_PCM_F32BE, 4),
        (7, CODEC_ID_PCM_F64BE, 8),
        (27, CODEC_ID_PCM_ALAW, 1),
    ];

    for (encoding, codec, width) in cases {
        let data = vec![0u8; width * 10];
        let reader = open(au_file(b"", data.len() as u32, encoding, 8000, 1, &data));
        let track = &reader.tracks()[0];

        assert_eq!(audio_params(track).codec, codec, "encoding {encoding}");
        assert_eq!(track.num_frames, Some(10), "encoding {encoding}");
    }
}

#[test]
fn g72x_encodings() {
    // 40 bytes of G.726 at 4, 3, 5 and 2 bits per sample: 80, 104, 64 and 160 frames. Samples of
    // 3 and 5 bits fill whole bytes in groups of 8, and a group that is not complete is not
    // counted.
    for (encoding, bits, frames) in [(23, 4, 80), (25, 3, 104), (26, 5, 64), (0x3732_3632, 2, 160)]
    {
        let data = vec![0u8; 40];
        let reader = open(au_file(b"", 40, encoding, 8000, 1, &data));
        let track = &reader.tracks()[0];
        let params = audio_params(track);

        assert_eq!(params.codec, CODEC_ID_ADPCM_G726LE, "encoding {encoding}");
        assert_eq!(params.bits_per_coded_sample, Some(bits), "encoding {encoding}");
        assert_eq!(track.num_frames, Some(frames), "encoding {encoding}");
    }

    // G.722 always has a 16 kHz sample rate. One byte is two samples.
    let reader = open(au_file(b"", 40, 24, 8000, 1, &[0; 40]));
    let params = audio_params(&reader.tracks()[0]);

    assert_eq!(params.codec, CODEC_ID_ADPCM_G722);
    assert_eq!(params.sample_rate, Some(16000));
    assert_eq!(reader.tracks()[0].num_frames, Some(80));
}

#[test]
fn unknown_size_reads_to_end() {
    let data = vec![0x11u8; 400];
    let mut reader = open(au_file(b"", u32::MAX, 3, 8000, 1, &data));

    assert_eq!(reader.tracks()[0].num_frames, Some(200));
    assert_eq!(drain(reader.as_mut()).iter().map(|p| p.data.len()).sum::<usize>(), 400);
}

#[test]
fn truncated_data_is_clamped() {
    // The header claims 1000 bytes, the file has 40, and the last frame is cut.
    let data = vec![0x22u8; 41];
    let mut reader = open(au_file(b"", 1000, 3, 8000, 1, &data));

    assert_eq!(reader.tracks()[0].num_frames, Some(20));

    // Trailing bytes that are not a whole frame are not returned.
    assert_eq!(drain(reader.as_mut()).iter().map(|p| p.data.len()).sum::<usize>(), 40);
}

#[test]
fn data_size_limits_data() {
    // Bytes after the data are not audio.
    let mut data = vec![0x33u8; 40];
    data.extend_from_slice(b"trailer-not-audio");
    let mut reader = open(au_file(b"", 40, 3, 8000, 1, &data));

    assert_eq!(drain(reader.as_mut()).iter().map(|p| p.data.len()).sum::<usize>(), 40);
}

#[test]
fn annotation_tags() {
    let annotation =
        b"title=Song\nartist=Someone\nalbum=Disc\ntrack=3/9\ngenre=Beep\nx-note=hi\0\0\0";
    let mut reader = open(au_file(annotation, 4, 3, 8000, 1, &[0; 4]));

    let mut metadata = reader.metadata();
    let revision = metadata.skip_to_latest().unwrap();

    let mut title = None;
    let mut artist = None;
    let mut album = None;
    let mut track = None;
    let mut genre = None;
    let mut raw = None;

    for tag in &revision.media.tags {
        match &tag.std {
            Some(StandardTag::TrackTitle(v)) => title = Some(v.to_string()),
            Some(StandardTag::Artist(v)) => artist = Some(v.to_string()),
            Some(StandardTag::Album(v)) => album = Some(v.to_string()),
            Some(StandardTag::TrackNumber(v)) => track = Some(*v),
            Some(StandardTag::Genre(v)) => genre = Some(v.to_string()),
            None if tag.raw.key == "x-note" => raw = Some(tag.raw.value.to_string()),
            _ => (),
        }
    }

    assert_eq!(title.as_deref(), Some("Song"));
    assert_eq!(artist.as_deref(), Some("Someone"));
    assert_eq!(album.as_deref(), Some("Disc"));
    assert_eq!(track, Some(3));
    assert_eq!(genre.as_deref(), Some("Beep"));
    assert_eq!(raw.as_deref(), Some("hi"));
}

#[test]
fn seek_by_timestamp_and_time() {
    // 5000 stereo frames, with a distinct value in every sample.
    let data: Vec<u8> = (0..10_000u32).flat_map(|s| (s as i16).to_be_bytes()).collect();
    let mut reader = open(au_file(b"", data.len() as u32, 3, 8000, 2, &data));

    // Packets have 1152 frames at most, and begin at multiples of that: a seek returns the start
    // of the packet that holds the timestamp.
    let seeked = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(3000), track_id: 0 })
        .unwrap();

    assert_eq!(seeked.required_ts.get(), 3000);
    assert_eq!(seeked.actual_ts.get(), 2304);

    let packet = reader.next_packet().unwrap().unwrap();

    assert_eq!(packet.pts.get(), 2304);
    assert_eq!(&packet.data[..], &data[2304 * 4..(2304 + 1152) * 4]);

    // 0.5 s at 8000 Hz is frame 4000.
    let seeked = reader
        .seek(
            SeekMode::Accurate,
            SeekTo::Time { time: Time::try_from_secs_f64(0.5).unwrap(), track_id: None },
        )
        .unwrap();

    assert_eq!(seeked.required_ts.get(), 4000);
    assert_eq!(seeked.actual_ts.get(), 3456);

    // The packets after the seek are the same as the packets without it, up to the last one.
    let packets = drain(reader.as_mut());

    assert_eq!(packets.iter().map(|p| p.dur.get()).sum::<u64>(), 5000 - 3456);
    assert_eq!(packets.last().unwrap().pts.get(), 4608);

    // Seeking to the end is allowed, and past it is not.
    assert!(
        reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(5000), track_id: 0 })
            .is_ok()
    );
    assert!(
        reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(5001), track_id: 0 })
            .is_err()
    );

    // And a seek to another track is not.
    assert!(
        reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(0), track_id: 1 })
            .is_err()
    );
}

#[test]
fn rejects_bad_headers() {
    let (file, _) = stereo_s16();

    // Wrong magic.
    let mut bad = file.clone();
    bad[0] = b'X';
    assert!(AuReader::try_new(mss(bad), FormatOptions::default()).is_err());

    // Zero rate.
    assert!(AuReader::try_new(mss(au_file(b"", 4, 3, 0, 1, &[0; 4])), Default::default()).is_err());

    // Zero channels.
    assert!(
        AuReader::try_new(mss(au_file(b"", 4, 3, 8000, 0, &[0; 4])), Default::default()).is_err()
    );

    // Unsupported encoding: 8 is fragmented sample data.
    assert!(
        AuReader::try_new(mss(au_file(b"", 4, 8, 8000, 1, &[0; 4])), Default::default()).is_err()
    );

    // Header cut short.
    assert!(AuReader::try_new(mss(file[..20].to_vec()), Default::default()).is_err());
}

#[test]
fn is_probed() {
    let (file, _) = stereo_s16();

    let mut hint = Hint::new();
    hint.with_extension("au");

    let reader = probe()
        .probe(&hint, mss(file), FormatOptions::default(), MetadataOptions::default())
        .unwrap();

    assert_eq!(reader.format_info().short_name, "au");
    assert!(matches!(reader.tracks()[0].codec_params, Some(CodecParameters::Audio(_))));
}

#[test]
fn non_seekable_source() {
    // A source that is not seekable can be read forwards.
    struct NoSeek(Cursor<Vec<u8>>);

    impl std::io::Read for NoSeek {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl std::io::Seek for NoSeek {
        fn seek(&mut self, _: std::io::SeekFrom) -> std::io::Result<u64> {
            Err(std::io::ErrorKind::Unsupported.into())
        }
    }

    impl symphonia_core::io::MediaSource for NoSeek {
        fn is_seekable(&self) -> bool {
            false
        }

        fn byte_len(&self) -> Option<u64> {
            None
        }
    }

    let (file, data) = stereo_s16();
    let mss = MediaSourceStream::new(Box::new(NoSeek(Cursor::new(file))), Default::default());
    let mut reader = AuReader::try_new(mss, FormatOptions::default()).unwrap();

    let bytes: Vec<u8> = drain(&mut reader).iter().flat_map(|p| p.data.to_vec()).collect();

    assert_eq!(bytes, data);
}

/// Decodes the sample with the PCM decoder and compares it with FFmpeg's decode, and a golden
/// hash.
#[test]
fn sample_matches_ffmpeg() {
    let Some(path) = symphonia_testutil::sample_path("unsupported/au_s16.au")
    else {
        return;
    };

    let reader = AuReader::try_new(symphonia_testutil::open_mss(&path), Default::default())
        .map(|r| Box::new(r) as Box<dyn FormatReader>)
        .unwrap();

    let track = &reader.tracks()[0];
    let params = audio_params(track);

    assert_eq!(params.codec, CODEC_ID_PCM_S16BE);
    assert_eq!(params.sample_rate, Some(44100));
    assert_eq!(track.num_frames, Some(1_323_000));

    let Some(expected) = ffmpeg_pcm(&path, "s16le")
    else {
        return;
    };

    let mut reader = reader;
    let ours = common::decode_s16(reader.as_mut());

    assert_eq!(ours.len() * 2, expected.len());
    assert!(ours.iter().flat_map(|s| s.to_le_bytes()).eq(expected.iter().copied()));
    assert_eq!(fnv_of(&expected), 5570782212356629021);
}
