// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests against real files. The files are not part of the repository: the tests look for them in
//! the directory in the `RMPD_SAMPLES_DIR` environment variable (by default
//! `/home/gianluca/rmpd-samples/samples`), and are skipped when a file is absent.

#![cfg(feature = "wav")]

use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;

use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::units::Time;
use symphonia_format_riff::WavReader;

fn sample_path(name: &str) -> Option<PathBuf> {
    let dir = std::env::var_os("RMPD_SAMPLES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/home/gianluca/rmpd-samples/samples"));

    let path = dir.join(name);

    if path.exists() {
        Some(path)
    }
    else {
        eprintln!("skipping: {} not found", path.display());
        None
    }
}

fn open(name: &str) -> Option<WavReader<'static>> {
    let path = sample_path(name)?;
    let mss = MediaSourceStream::new(Box::new(File::open(path).unwrap()), Default::default());
    Some(WavReader::try_new(mss, FormatOptions::default()).unwrap())
}

/// Reads all the packets from the current position. Returns a map from the timestamp of a packet,
/// to its data.
fn read_all(reader: &mut WavReader<'_>) -> (Vec<i64>, HashMap<i64, Box<[u8]>>) {
    let mut order = Vec::new();
    let mut packets = HashMap::new();

    while let Some(packet) = reader.next_packet().expect("no error at the end of the stream") {
        order.push(packet.pts.get());
        packets.insert(packet.pts.get(), packet.data);
    }

    (order, packets)
}

/// Seeks to various fractions of the track, and verifies that the first packet read afterwards is
/// the one the seek reported landing on, with the same data and timestamp as when read
/// sequentially.
fn check_seeks(name: &str, max_distance_secs: f64) {
    let Some(mut reader) = open(name)
    else {
        return;
    };

    let track = &reader.tracks()[0];
    let rate =
        f64::from(track.codec_params.as_ref().unwrap().audio().unwrap().sample_rate.unwrap());
    let num_frames = track.num_frames.unwrap();
    let duration = num_frames as f64 / rate;

    let (order, packets) = read_all(&mut reader);
    assert!(!order.is_empty());

    for fraction in [0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 0.99] {
        let secs = duration * fraction;
        let time = Time::try_from_secs_f64(secs).unwrap();

        let seeked = reader
            .seek(SeekMode::Accurate, SeekTo::Time { time, track_id: None })
            .unwrap_or_else(|e| panic!("{name}: seek to {secs:.3}s failed: {e:?}"));

        let actual_secs = seeked.actual_ts.get() as f64 / rate;
        assert!(
            (actual_secs - secs).abs() < max_distance_secs,
            "{name}: seek to {secs:.3}s landed at {actual_secs:.3}s"
        );

        let packet = reader
            .next_packet()
            .unwrap()
            .unwrap_or_else(|| panic!("{name}: no packet after seek to {secs:.3}s"));

        assert_eq!(packet.pts, seeked.actual_ts, "{name}: first packet after seek to {secs:.3}s");
        assert_eq!(
            Some(&packet.data),
            packets.get(&packet.pts.get()),
            "{name}: packet after seek to {secs:.3}s differs from the sequential read"
        );
    }
}

#[test]
fn adpcm_seeks_land_on_packets() {
    for name in [
        "wav/wav_ima_adpcm.wav",
        "wav/wav_ima_adpcm_mono.wav",
        "wav/wav_ima_adpcm_blk256.wav",
        "wav/wav_ms_adpcm.wav",
        "wav/wav_ms_adpcm_mono.wav",
    ] {
        // Packets are around 1000-2000 frames, so land within a fraction of a second.
        check_seeks(name, 0.5);
    }
}

#[test]
fn mpeg_in_wave_seeks_land_on_frames() {
    // One MPEG frame is 24-26 ms.
    check_seeks("wav/wav_mp3.wav", 0.03);
    check_seeks("mp2/mp2_in_wav.wav", 0.03);
}

#[test]
fn pcm_seeks_land_on_packets() {
    check_seeks("wav/wav_s16.wav", 0.03);
}

#[test]
fn mp2_in_wave_is_supported() {
    let Some(reader) = open("mp2/mp2_in_wav.wav")
    else {
        return;
    };

    let params = reader.tracks()[0].codec_params.as_ref().unwrap().audio().unwrap();
    assert_eq!(params.codec, symphonia_core::codecs::audio::well_known::CODEC_ID_MP2);
}

#[test]
fn stream_of_unknown_length_ends_with_no_packet() {
    let Some(mut reader) = open("wav/wav_streamed_unknown_len.wav")
    else {
        return;
    };

    let (order, _) = read_all(&mut reader);
    assert!(!order.is_empty());
}

#[test]
fn trailing_tags_are_read() {
    if let Some(mut reader) = open("tags/wav_riff_info_after_data.wav") {
        let mut found = Vec::new();
        let mut metadata = reader.metadata();
        loop {
            if let Some(revision) = metadata.current() {
                for tag in &revision.media.tags {
                    found.push((tag.raw.key.clone(), tag.raw.value.to_string()));
                }
            }
            if metadata.pop().is_none() {
                break;
            }
        }

        assert!(found.contains(&("INAM".to_string(), "Title After Data".to_string())), "{found:?}");
        assert!(
            found.contains(&("IART".to_string(), "Artist After Data".to_string())),
            "{found:?}"
        );

        // The audio is still read from the start.
        let packet = reader.next_packet().unwrap().unwrap();
        assert_eq!(packet.pts.get(), 0);
    }

    if let Some(mut reader) = open("tags/wav_id3_chunk.wav") {
        let mut found = Vec::new();
        let mut metadata = reader.metadata();
        loop {
            if let Some(revision) = metadata.current() {
                for tag in &revision.media.tags {
                    found.push(tag.raw.key.clone());
                }
            }
            if metadata.pop().is_none() {
                break;
            }
        }

        assert!(found.iter().any(|key| key == "TIT2"), "{found:?}");
        assert_eq!(reader.next_packet().unwrap().unwrap().pts.get(), 0);
    }
}

#[test]
fn bw64_and_wave64_are_supported() {
    for name in ["wav/wav_bw64.wav", "wav/wav_w64.w64"] {
        let Some(mut reader) = open(name)
        else {
            continue;
        };

        let num_frames = reader.tracks()[0].num_frames.unwrap();
        let (_, packets) = read_all(&mut reader);
        let bytes: usize = packets.values().map(|p| p.len()).sum();

        // Stereo, 16-bit.
        assert_eq!(bytes as u64, num_frames * 4, "{name}");
    }
}
