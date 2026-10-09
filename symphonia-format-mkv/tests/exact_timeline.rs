// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests of the sample accuracy of the timeline and of seeks of Vorbis and Opus in Matroska,
//! using real files, and a real decoder.
//!
//! The tests use ffmpeg (if it is installed) to create the files, and the files in the directory
//! named by the `RMPD_SAMPLES` environment variable. They are skipped if these are not available.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use symphonia_codec_vorbis::VorbisDecoder;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::FormatReader;
use symphonia_core::formats::prelude::*;
use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia_format_mkv::MkvReader;

fn open_path(path: &Path) -> MkvReader<'static> {
    let file = File::open(path).expect("file should exist");
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    MkvReader::try_new(mss, FormatOptions::default()).expect("file should open")
}

/// Get the path of a file of the sample set, if it exists.
fn sample(dir: &str, name: &str) -> Option<PathBuf> {
    let path = Path::new(&std::env::var_os("RMPD_SAMPLES")?).join(dir).join(name);

    if path.exists() {
        Some(path)
    } else {
        eprintln!("skipping: {} does not exist", path.display());
        None
    }
}

/// A file created by ffmpeg, that is removed afterwards.
struct Generated(PathBuf);

impl Drop for Generated {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Create a file with ffmpeg from 8 seconds of a signal that is rich enough for a codec to use
/// all of its block sizes: tones with a bursts of noise. Returns `None` if ffmpeg, or its
/// encoder, is not available.
fn generate(name: &str, rate: u32, channels: u32, encoder_args: &[&str]) -> Option<Generated> {
    let expr = "0.5*sin(2*PI*440*t)*(1+sin(2*PI*2*t))/2+0.4*random(0)*lt(mod(t,0.6),0.04)";
    let expr = (0..channels).map(|_| expr).collect::<Vec<_>>().join("|");
    let path = std::env::temp_dir().join(format!("symphonia-mkv-{}-{name}", std::process::id()));

    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("aevalsrc='{expr}':s={rate}:d=8"))
        .args(encoder_args)
        .arg(&path)
        .stdin(Stdio::null())
        .status()
        .ok()?;

    if status.success() {
        Some(Generated(path))
    } else {
        eprintln!("skipping: ffmpeg could not create {name}");
        None
    }
}

/// A small deterministic pseudo-random number generator.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

fn audio_params(reader: &MkvReader<'_>) -> symphonia_core::codecs::audio::AudioCodecParameters {
    match reader.tracks()[0].codec_params.clone() {
        Some(CodecParameters::Audio(params)) => params,
        _ => panic!("expected audio codec parameters"),
    }
}

/// Check that the packets of a stream have an exact, contiguous, timeline, which starts at 0.
fn assert_contiguous(packets: &[Packet]) {
    // The first packet of a Vorbis stream decodes to nothing. The audio starts with the second.
    assert!(packets[0].pts.get() < 0, "the first packet starts before 0");
    assert_eq!(packets[0].dur.get(), 0);
    assert_eq!(packets[0].trim_start.get() as i64, -packets[0].pts.get());
    assert_eq!(packets[1].pts.get(), 0);

    for pair in packets.windows(2) {
        assert_eq!(pair[1].pts, pair[0].pts.saturating_add(pair[0].block_dur()));
    }
}

/// Decode a whole Vorbis stream. Returns the packets, and the interleaved samples, which start at
/// frame 0 of the stream.
fn decode_continuously(path: &Path) -> (Vec<Packet>, Vec<f32>, usize) {
    let mut reader = open_path(path);
    let params = audio_params(&reader);
    let channels = params.channels.as_ref().expect("the channels are known").count();

    let mut decoder = VorbisDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

    let mut packets = Vec::new();
    let mut samples: Vec<f32> = Vec::new();
    let mut block: Vec<f32> = Vec::new();

    while let Some(packet) = reader.next_packet().expect("reading should not fail") {
        let buf = decoder.decode(&packet).expect("the packet should decode");

        // The duration of the packet is the number of frames that are decoded from it.
        assert_eq!(buf.frames() as u64, packet.dur.get(), "frames of packet at {}", packet.pts);

        buf.copy_to_vec_interleaved(&mut block);
        samples.extend_from_slice(&block);
        packets.push(packet);
    }

    assert_contiguous(&packets);

    let total: u64 = packets.iter().map(|packet| packet.dur.get()).sum();
    assert_eq!(samples.len() as u64, total * channels as u64);

    (packets, samples, channels)
}

/// The packet timeline is exact, and seeks to many positions land on the frame: the audio decoded
/// after a seek is, to the bit, the audio of the continuous decode of the stream at the position
/// that the packets claim.
fn check_vorbis_seeks(path: &Path, num_seeks: usize) {
    let (all_packets, samples, channels) = decode_continuously(path);

    let total = samples.len() / channels;
    assert!(total > 8 * 4410, "the stream is too short for the test");

    let mut reader = open_path(path);
    let params = audio_params(&reader);
    let sample_rate = i64::from(params.sample_rate.expect("the sample rate is known"));
    let track_id = reader.tracks()[0].id;
    let mut decoder = VorbisDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

    // The seek pre-roll of Vorbis, 200ms, and a packet.
    let max_backoff = sample_rate / 5 + 2 * 2048 + 1;

    let mut rng = Lcg(2024);
    let mut targets = vec![0i64, 1, 100, 127, 128, 129, 1000, sample_rate / 5, total as i64 - 1];
    targets.extend((0..num_seeks).map(|_| (rng.next() % total as u64) as i64));
    targets.push(total as i64 / 2);

    let mut block: Vec<f32> = Vec::new();

    for target in targets {
        let seeked = reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(target), track_id })
            .expect("seek should succeed");

        assert_eq!(seeked.required_ts.get(), target);
        assert!(seeked.actual_ts.get() <= target, "the seek landed after {target}");
        assert!(
            target - seeked.actual_ts.get() <= max_backoff,
            "the seek landed too early for {target}: {}",
            seeked.actual_ts
        );

        // The position of the landing is the position of the packet in the continuous stream.
        assert!(all_packets.iter().any(|packet| packet.pts == seeked.actual_ts));

        decoder.reset();

        let mut covered = false;
        let mut first = true;

        while let Some(packet) = reader.next_packet().expect("reading should not fail") {
            if first {
                assert_eq!(packet.pts, seeked.actual_ts);
            }

            let buf = decoder.decode(&packet).expect("the packet should decode");

            if first {
                // The first packet after a reset only primes the decoder.
                assert_eq!(buf.frames(), 0);
                first = false;
            } else {
                assert_eq!(buf.frames() as u64, packet.dur.get());

                buf.copy_to_vec_interleaved(&mut block);

                let start = packet.pts.get() as usize * channels;
                assert!(
                    block[..] == samples[start..start + block.len()],
                    "the audio after the seek to {target}, at packet {}, differs from the stream",
                    packet.pts
                );

                if packet.pts.get() <= target && target < packet.pts.get() + buf.frames() as i64 {
                    covered = true;
                }
            }

            if packet.pts.get() > target + 4096 {
                break;
            }
        }

        assert!(covered, "no packet decoded after the seek to {target} contains it");
    }
}

#[test]
fn vorbis_in_mka_sample_seeks_are_sample_exact() {
    for (dir, name) in [
        ("vorbis", "vorbis_in_mka.mka"),
        ("vorbis", "vorbis_in_webm.webm"),
        ("mkv", "mka_vorbis_tags.mka"),
    ] {
        if let Some(path) = sample(dir, name) {
            check_vorbis_seeks(&path, 60);
        }
    }
}

#[test]
fn ffmpeg_vorbis_seeks_are_sample_exact() {
    for (name, rate, channels, args) in [
        ("v44.mka", 44100, 2, &["-c:a", "libvorbis", "-q:a", "4"][..]),
        ("v44-q9.mka", 44100, 2, &["-c:a", "libvorbis", "-q:a", "9"][..]),
        ("v22.mka", 22050, 1, &["-c:a", "libvorbis", "-q:a", "3"][..]),
        ("v48.webm", 48000, 2, &["-c:a", "libvorbis", "-q:a", "5"][..]),
        ("v96.mka", 96000, 2, &["-c:a", "libvorbis", "-q:a", "5"][..]),
        ("v8.mka", 8000, 1, &["-c:a", "libvorbis", "-q:a", "5"][..]),
    ] {
        let Some(file) = generate(name, rate, channels, args) else {
            return;
        };

        check_vorbis_seeks(&file.0, 80);
    }
}

#[test]
fn ffmpeg_vorbis_has_short_and_long_blocks() {
    let Some(file) = generate("blocks.mka", 44100, 2, &["-c:a", "libvorbis", "-q:a", "4"]) else {
        return;
    };

    let mut reader = open_path(&file.0);
    let mut durations = std::collections::BTreeSet::new();

    while let Some(packet) = reader.next_packet().unwrap() {
        durations.insert(packet.dur.get());
    }

    // 0 (the first packet), short-short (128), long-long (1024), and the transitions (576).
    assert!(durations.contains(&128) && durations.contains(&1024), "{durations:?}");
    assert!(durations.contains(&576), "{durations:?}");
}

/// The seek pre-rolls of Opus streams in frames (1.5 s, and 10 s with SILK or Hybrid packets).
const OPUS_CELT_PREROLL: i64 = 72_000;
const OPUS_SILK_PREROLL: i64 = 480_000;

/// Check the timeline of an Opus stream is exact, and seeks land on the packet at the pre-roll
/// before the target. Returns the number of frames of the stream.
fn check_opus_file(path: &Path) -> u64 {
    let mut reader = open_path(path);
    let track_id = reader.tracks()[0].id;
    let mut packets = Vec::new();

    while let Some(packet) = reader.next_packet().unwrap() {
        packets.push(packet);
    }

    // The delay is trimmed from the first packet, and the padding from the last.
    assert_eq!(packets[0].pts.get(), -i64::from(reader.tracks()[0].delay.unwrap()));
    assert_eq!(packets[1].pts.get(), packets[0].dur.get() as i64);

    for pair in packets.windows(2) {
        assert_eq!(pair[1].pts, pair[0].pts.saturating_add(pair[0].block_dur()));
    }

    let total: u64 = packets.iter().map(|packet| packet.dur.get()).sum();

    // Seeks land on a packet, before the pre-roll.
    let mut rng = Lcg(11);
    let mut silk = false;

    for _ in 0..100 {
        let target = (rng.next() % total) as i64;
        let seeked = reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(target), track_id })
            .unwrap();

        let at = |preroll: i64| {
            let found = packets.iter().rev().find(|packet| packet.pts.get() <= target - preroll);
            found.unwrap_or(&packets[0])
        };

        // The pre-roll grows to that of SILK once a SILK or Hybrid packet is found by a seek.
        let expected = if silk {
            at(OPUS_SILK_PREROLL)
        }
        else if seeked.actual_ts == at(OPUS_CELT_PREROLL).pts {
            at(OPUS_CELT_PREROLL)
        }
        else {
            silk = true;
            at(OPUS_SILK_PREROLL)
        };

        assert_eq!(seeked.actual_ts, expected.pts, "{}: seek to {target}", path.display());

        let packet = reader.next_packet().unwrap().unwrap();
        assert_eq!((packet.pts, packet.dur), (expected.pts, expected.dur));
    }

    total
}

/// Opus packet durations are exact, so is the timeline, and so are the seeks.
#[test]
fn ffmpeg_opus_timeline_and_seeks_are_sample_exact() {
    for (name, args) in [
        ("o20.mka", &["-c:a", "libopus", "-b:a", "64k"][..]),
        ("o40.mka", &["-c:a", "libopus", "-b:a", "64k", "-frame_duration", "40"][..]),
        ("o10.webm", &["-c:a", "libopus", "-b:a", "64k", "-frame_duration", "10"][..]),
        ("o60.mka", &["-c:a", "libopus", "-b:a", "64k", "-frame_duration", "60"][..]),
    ] {
        let Some(file) = generate(name, 48000, 2, args) else {
            return;
        };

        assert_eq!(check_opus_file(&file.0), 8 * 48000, "{name}");
    }
}

#[test]
fn opus_samples_have_an_exact_timeline() {
    for (dir, name) in [
        ("opus", "opus_in_mka.mka"),
        ("opus", "opus_in_webm.webm"),
        ("opus", "opus_in_mka_6ch.mka"),
        ("opus", "opus_in_mka_mono_voip.mka"),
        ("mkv", "mka_opus_tags.mka"),
        ("mkv", "webm_opus_only.webm"),
    ] {
        if let Some(path) = sample(dir, name) {
            // The samples are 30s long.
            assert_eq!(check_opus_file(&path), 30 * 48000, "{name}");
        }
    }
}
