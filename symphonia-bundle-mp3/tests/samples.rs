// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests that use real MPEG audio files. The files are located in the directory given by the
//! `RMPD_SAMPLES` environment variable (default: `/home/gianluca/rmpd-samples/samples`). Tests are
//! skipped if the files they need are not present.

#![allow(clippy::unwrap_used)]

use std::fs::File;
use std::path::PathBuf;

use symphonia_bundle_mp3::{MpaDecoder, MpaReader};
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::errors::Error;
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, Track};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::units::Timestamp;

fn sample_path(name: &str) -> Option<PathBuf> {
    let dir = std::env::var_os("RMPD_SAMPLES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/home/gianluca/rmpd-samples/samples"));

    let path = dir.join(name);

    if path.exists() { Some(path) } else { None }
}

fn open(path: &PathBuf, opts: FormatOptions) -> (MpaReader<'static>, MpaDecoder) {
    let file = File::open(path).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let reader = MpaReader::try_new(mss, opts).unwrap();
    let decoder = new_decoder(&reader.tracks()[0]);
    (reader, decoder)
}

fn new_decoder(track: &Track) -> MpaDecoder {
    let params = track.codec_params.as_ref().unwrap().audio().unwrap();
    MpaDecoder::try_new(params, &AudioDecoderOptions::default().gapless(false)).unwrap()
}

/// The output of a continuous decode of an entire stream.
struct Decoded {
    /// The timestamp of the first decoded frame.
    first_ts: i64,
    channels: usize,
    /// The interleaved samples.
    samples: Vec<f32>,
}

impl Decoded {
    fn frames(&self) -> usize {
        self.samples.len() / self.channels
    }

    /// Get `n` frames from the frame at timestamp `ts`.
    fn get(&self, ts: i64, n: usize) -> &[f32] {
        let start = (ts - self.first_ts) as usize * self.channels;
        &self.samples[start..start + n * self.channels]
    }
}

fn decode_all(path: &PathBuf) -> Decoded {
    let (mut reader, mut decoder) = open(path, FormatOptions::default());

    let mut first_ts = None;
    let mut channels = 0;
    let mut samples = Vec::new();

    while let Some(packet) = reader.next_packet().unwrap() {
        first_ts.get_or_insert(packet.pts.get());

        let decoded = decoder.decode(&packet).unwrap();
        channels = decoded.spec().channels().count();

        let mut buf = Vec::new();
        decoded.copy_to_vec_interleaved(&mut buf);
        samples.extend_from_slice(&buf);
    }

    Decoded { first_ts: first_ts.unwrap(), channels, samples }
}

/// Seek, then decode `n` frames starting from exactly `required_ts`.
fn seek_decode(
    reader: &mut MpaReader<'_>,
    decoder: &mut MpaDecoder,
    mode: SeekMode,
    required_ts: i64,
    n: usize,
) -> (i64, Vec<f32>) {
    let seeked = reader
        .seek(mode, SeekTo::Timestamp { ts: Timestamp::new(required_ts), track_id: 0 })
        .unwrap();

    assert_eq!(seeked.required_ts.get(), required_ts);
    assert!(seeked.actual_ts.get() <= required_ts, "seeked past the required timestamp");

    decoder.reset();

    let mut out = Vec::new();
    let mut channels = 0;
    let mut ts = None;
    let mut first_pkt_ts = None;

    while out.len() < n * channels.max(1) {
        let packet = match reader.next_packet().unwrap() {
            Some(packet) => packet,
            None => break,
        };

        let pkt_ts = packet.pts.get();

        // The first packet must be at the timestamp reported by the seek.
        first_pkt_ts.get_or_insert(pkt_ts);
        assert_eq!(first_pkt_ts, Some(seeked.actual_ts.get()));

        let mut samples = Vec::new();

        match decoder.decode(&packet) {
            Ok(decoded) => {
                channels = decoded.spec().channels().count();
                decoded.copy_to_vec_interleaved(&mut samples);
            }
            // Frames at the beginning may not be decodable (missing bit reservoir).
            Err(Error::DecodeError(_)) => {
                samples.resize(1152 * 2, 0.0);
                continue;
            }
            Err(err) => panic!("{err}"),
        }

        let n_frames = samples.len() / channels;
        let mut skip = 0;

        if pkt_ts + n_frames as i64 <= required_ts {
            continue;
        }

        if pkt_ts < required_ts {
            skip = (required_ts - pkt_ts) as usize * channels;
        }

        ts.get_or_insert(pkt_ts + (skip / channels) as i64);
        out.extend_from_slice(&samples[skip..]);
    }

    out.truncate(n * channels);

    (ts.unwrap(), out)
}

fn check_seeks(name: &str, modes: &[SeekMode]) {
    let Some(path) = sample_path(name)
    else {
        eprintln!("skipping {name}: sample not found");
        return;
    };

    let reference = decode_all(&path);
    let total = reference.frames() as i64;

    let (mut reader, mut decoder) = open(&path, FormatOptions::default());

    // Positions are fractions of the stream, and odd offsets to land in the middle of frames, and
    // on the boundaries of frames.
    let mut positions: Vec<i64> = vec![0, 1, 575, 576, 1151, 1152, 1153, 5000, 44100];

    for pct in [3, 10, 25, 37, 50, 66, 80, 90, 97] {
        positions.push(total * pct / 100 + (pct * 31) % 1152);
    }

    positions.push(total - 20_000);
    positions.push(total - 3000);

    // Include backwards seeks by reversing.
    let mut order = positions.clone();
    order.extend(positions.iter().rev());

    let mut n_checked = 0;

    for mode in modes {
        for &pos in &order {
            let required_ts = reference.first_ts.max(0) + pos;

            // Leave enough audio to compare after the seek position.
            let n = 8192.min((reference.first_ts + total - required_ts).max(0) as usize);

            if required_ts < reference.first_ts || n == 0 {
                continue;
            }

            let (ts, out) = seek_decode(&mut reader, &mut decoder, *mode, required_ts, n);

            assert_eq!(ts, required_ts, "{name}: {mode:?} @ {required_ts}");

            let expected = reference.get(ts, out.len() / reference.channels);
            n_checked += 1;

            if out != expected {
                let first_diff = out.iter().zip(expected).position(|(a, b)| a != b).unwrap();
                panic!(
                    "{name}: {mode:?} seek to {required_ts} differs from continuous decode from \
                     frame {} after the required position",
                    first_diff / reference.channels
                );
            }
        }
    }

    assert!(n_checked > 20, "{name}: too few seeks checked");
}

const BOTH: [SeekMode; 2] = [SeekMode::Accurate, SeekMode::Coarse];

#[test]
fn verify_seek_cbr() {
    check_seeks("mp3/mp3_cbr128.mp3", &BOTH);
    check_seeks("mp3/mp3_cbr320.mp3", &BOTH);
    check_seeks("mp3/mp3_cbr64_mono.mp3", &BOTH);
}

#[test]
fn verify_seek_vbr() {
    check_seeks("mp3/mp3_vbr_v2.mp3", &BOTH);
    check_seeks("mp3/mp3_vbr_v0.mp3", &BOTH);
    check_seeks("mp3/mp3_notag_vbr.mp3", &BOTH);
}

#[test]
fn verify_seek_mpeg2_and_25() {
    check_seeks("mp3/mp3_vbr_v9.mp3", &BOTH);
    check_seeks("mp3/mp3_22k_mono.mp3", &BOTH);
    check_seeks("mp3/mp3_24k.mp3", &BOTH);
    check_seeks("mp3/mp3_16k_mono.mp3", &BOTH);
    check_seeks("mp3/mp3_11k_mono.mp3", &BOTH);
    check_seeks("mp3/mp3_8k_mono.mp3", &BOTH);
}

#[test]
fn verify_seek_other_rates_and_modes() {
    check_seeks("mp3/mp3_48k.mp3", &BOTH);
    check_seeks("mp3/mp3_32k.mp3", &BOTH);
    check_seeks("mp3/mp3_allshort.mp3", &BOTH);
    check_seeks("mp3/mp3_joint.mp3", &BOTH);
    check_seeks("mp3/mp3_abr192.mp3", &BOTH);
    check_seeks("mp3/mp3_notag_cbr.mp3", &BOTH);
}

#[test]
fn verify_seek_layer2() {
    check_seeks("mp2/mp2_192_st.mp2", &BOTH);
    check_seeks("mp2/mp2_22k_lsf.mp2", &BOTH);
    check_seeks("mp2/mp1_from_mpg.mp1", &BOTH);
}

/// The number of frames decoded from a stream (with gapless disabled).
fn decoded_frames(name: &str) -> Option<(usize, MpaReader<'static>)> {
    let path = sample_path(name)?;
    let reference = decode_all(&path);
    let (reader, _) = open(&path, FormatOptions::default());
    Some((reference.frames(), reader))
}

#[test]
fn verify_cbr_without_info_tag_has_all_frames() {
    // Frame counts as decoded by ffmpeg.
    for (name, expected) in
        [("mp3/mp3_notag_cbr.mp3", 1_324_800), ("mp3/mp3_ffmpeg_noxing.mp3", 1_324_800)]
    {
        let Some(path) = sample_path(name)
        else {
            continue;
        };

        // With gapless enabled (the default) no frames may be trimmed when there is no tag.
        let file = File::open(&path).unwrap();
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut reader = MpaReader::try_new(mss, FormatOptions::default()).unwrap();
        let mut decoder = MpaDecoder::try_new(
            reader.tracks()[0].codec_params.as_ref().unwrap().audio().unwrap(),
            &AudioDecoderOptions::default(),
        )
        .unwrap();

        let mut n = 0;

        while let Some(packet) = reader.next_packet().unwrap() {
            n += decoder.decode(&packet).unwrap().frames();
        }

        assert_eq!(n, expected, "{name}");
    }
}

#[test]
fn verify_duration_excludes_trailing_tags() {
    for name in [
        "tags/mp3_apev2_full.mp3",
        "tags/apev2_on_mp3.mp3",
        "tags/id3v1_only.mp3",
        "tags/id3v2_4_at_end_footer.mp3",
    ] {
        let Some((n_decoded, reader)) = decoded_frames(name)
        else {
            continue;
        };

        let reported = reader.tracks()[0].num_frames.unwrap() as i64;

        assert!(
            (reported - n_decoded as i64).abs() <= 1152,
            "{name}: reported {reported}, decoded {n_decoded}"
        );
    }
}

#[test]
fn verify_trailing_id3v2_is_read() {
    let Some(path) = sample_path("tags/id3v2_4_at_end_footer.mp3")
    else {
        return;
    };

    let (mut reader, _) = open(&path, FormatOptions::default());

    let metadata = reader.metadata();
    let revision = metadata.current().expect("the appended tag is read");
    assert!(!revision.media.tags.is_empty());
}

#[test]
fn verify_duration_estimate_of_vbr_without_xing() {
    let Some((n_decoded, reader)) = decoded_frames("mp3/mp3_notag_vbr.mp3")
    else {
        return;
    };

    // The estimate samples the whole stream, so it should be much better than using the first
    // frame's bit-rate (which was 2.6% out).
    let reported = reader.tracks()[0].num_frames.unwrap() as f64;
    assert!((reported / n_decoded as f64 - 1.0).abs() < 0.015, "{reported} vs {n_decoded}");

    // Scanning the stream is exact.
    let path = sample_path("mp3/mp3_notag_vbr.mp3").unwrap();
    let (reader, _) = open(&path, FormatOptions::default().prebuild_seek_index(true));
    assert_eq!(reader.tracks()[0].num_frames, Some(n_decoded as u64));
}

#[test]
fn verify_free_format() {
    let Some(path) = sample_path("mp3/mp3_free_format.mp3")
    else {
        return;
    };

    let (reader, _) = open(&path, FormatOptions::default());
    let track = &reader.tracks()[0];
    let params = track.codec_params.as_ref().unwrap().audio().unwrap();

    assert_eq!(params.sample_rate, Some(44100));

    let decoded = decode_all(&path);

    // Dump the audio for external comparison, if requested.
    if let Some(dir) = std::env::var_os("MP3_TEST_DUMP_DIR") {
        let bytes: Vec<u8> = decoded.samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        std::fs::write(PathBuf::from(dir).join("free_format.f32"), bytes).unwrap();
    }

    assert_eq!(decoded.channels, 2);
    assert!(decoded.frames() > 1_000_000);

    // The audio must not be silent or garbage: it should resemble the audio of the same stream
    // at another position when seeking.
    let peak = decoded.samples.iter().fold(0f32, |a, &s| a.max(s.abs()));
    assert!(peak > 0.1 && peak <= 1.5, "peak = {peak}");

    check_seeks("mp3/mp3_free_format.mp3", &BOTH);
}

/// Decode `path` with gapless playback enabled, from `required_ts` (a timestamp or time) after a
/// seek. Returns the frames at and after the required timestamp, and the timestamp of the first.
fn gapless_seek_decode(
    reader: &mut MpaReader<'_>,
    decoder: &mut MpaDecoder,
    mode: SeekMode,
    to: SeekTo,
    required_ts: i64,
    n: usize,
) -> Vec<f32> {
    let seeked = reader.seek(mode, to).unwrap();
    assert_eq!(seeked.required_ts.get(), required_ts);
    assert!(seeked.actual_ts.get() <= required_ts);

    decoder.reset();

    let mut out = Vec::new();
    let mut first_pkt = true;

    while out.len() < n * 2 {
        let Some(packet) = reader.next_packet().unwrap()
        else {
            break;
        };

        // The packet at the seeked-to timestamp comes first.
        if first_pkt {
            assert_eq!(packet.pts, seeked.actual_ts);
            first_pkt = false;
        }

        let Ok(decoded) = decoder.decode(&packet)
        else {
            continue;
        };

        // The frames of the packet after trimming begin at the packet's timestamp plus the frames
        // trimmed from its start.
        let ts = packet.pts.get() + packet.trim_start.get() as i64;
        let mut samples = Vec::new();
        decoded.copy_to_vec_interleaved(&mut samples);
        assert_eq!(samples.len() / 2, packet.dur.get() as usize);

        let skip = (required_ts - ts).max(0) as usize;

        if skip * 2 < samples.len() {
            out.extend_from_slice(&samples[skip * 2..]);
        }
    }

    out.truncate(n * 2);
    out
}

/// Read the samples of a 32-bit float WAVE file.
fn read_f32_wav(path: &std::path::Path) -> Vec<f32> {
    let data = std::fs::read(path).unwrap();
    let pos = data.windows(4).position(|w| w == b"data").unwrap();
    data[pos + 8..].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
}

#[test]
fn verify_free_format_seeks_match_independent_decoder() {
    let Some(path) = sample_path("mp3/mp3_free_format.mp3")
    else {
        return;
    };

    // Use the output of `mpg123`, if it is installed, as the reference. Otherwise use a continuous
    // decode.
    let wav = std::env::temp_dir().join("symphonia_mp3_free_format_ref.wav");

    let mpg123 = std::process::Command::new("mpg123")
        .args(["-q", "--float", "-w"])
        .arg(&wav)
        .arg(&path)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);

    let opts = AudioDecoderOptions::default();

    let (mut reader, _) = open(&path, FormatOptions::default());
    let mut decoder = MpaDecoder::try_new(
        reader.tracks()[0].codec_params.as_ref().unwrap().audio().unwrap(),
        &opts,
    )
    .unwrap();

    let num_frames = reader.tracks()[0].num_frames.unwrap() as i64;

    // The continuous decode with gapless playback.
    let mut reference: Vec<f32> = Vec::new();

    while let Some(packet) = reader.next_packet().unwrap() {
        let decoded = decoder.decode(&packet).unwrap();
        let mut samples = Vec::new();
        decoded.copy_to_vec_interleaved(&mut samples);
        reference.extend_from_slice(&samples);
    }

    assert_eq!(reference.len() as i64 / 2, num_frames);

    // The independent decoder agrees with the continuous decode (to within 16-bit rounding).
    if mpg123 {
        let independent = read_f32_wav(&wav);
        let n = independent.len().min(reference.len());
        assert!(n > 2 * 1_000_000);

        let err = independent[..n]
            .iter()
            .zip(&reference[..n])
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);

        assert!(err < 1e-3, "max error vs mpg123 = {err}");

        reference = independent;
    }

    let total = reference.len() as i64 / 2;

    let positions =
        [0, 1000, total / 4, total / 2, total * 9 / 10, total - 44100, total / 3 + 17, 5];

    for mode in [SeekMode::Accurate, SeekMode::Coarse] {
        for pos in positions.into_iter().chain([total / 2, 0]) {
            // Seek by time as well as by timestamp. A time may not convert back to the exact
            // timestamp.
            let tb = reader.tracks()[0].time_base.unwrap();
            let time = tb.calc_time(Timestamp::new(pos)).unwrap();
            let time_ts = tb.calc_timestamp(time).unwrap().get();

            for (to, pos) in [
                (SeekTo::Timestamp { ts: Timestamp::new(pos), track_id: 0 }, pos),
                (SeekTo::Time { time, track_id: Some(0) }, time_ts),
            ] {
                let n = 8192.min((total - pos) as usize);
                let out = gapless_seek_decode(&mut reader, &mut decoder, mode, to, pos, n);
                let expected = &reference[pos as usize * 2..(pos as usize + n) * 2];

                assert_eq!(out.len(), expected.len(), "{mode:?} @ {pos}");

                let err = out.iter().zip(expected).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);

                assert!(err < 1e-3, "{mode:?} @ {pos}: max error vs reference = {err}");
            }
        }
    }
}

/// Run the seek checks on every MPEG audio file in the samples directory. Files that cannot be
/// decoded without error are skipped. This takes a while, so it is only run on request.
#[test]
#[ignore]
fn verify_seek_all_samples() {
    let Some(root) = sample_path("mp3")
    else {
        return;
    };

    let root = root.parent().unwrap().to_path_buf();
    let mut failures = Vec::new();
    let mut n_files = 0;

    for dir in ["mp3", "mp2", "tags"] {
        let Ok(entries) = std::fs::read_dir(root.join(dir))
        else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();

            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

            if !["mp3", "mp2", "mp1"].contains(&ext) {
                continue;
            }

            let name = format!("{dir}/{}", path.file_name().unwrap().to_str().unwrap());

            let result = std::panic::catch_unwind(|| check_seeks(&name, &BOTH));

            n_files += 1;

            if result.is_err() {
                failures.push(name);
            }
        }
    }

    eprintln!("checked {n_files} files");
    assert!(failures.is_empty(), "failed: {failures:#?}");
}
