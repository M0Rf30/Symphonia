// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests that the probe markers and scores of the FLV, MPEG-PS, and MPEG-TS readers do not take
//! over streams of other formats.
//!
//! The test requires the `all` feature, and the `RMPD_SAMPLES` environment variable to be set to
//! the directory with the sample files. It is skipped otherwise.

#![cfg(all(
    feature = "flv",
    feature = "mpegps",
    feature = "mpegts",
    feature = "mp3",
    feature = "aac"
))]

use std::fs::File;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use symphonia::core::formats::probe::{Hint, Probe};
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::default::formats::*;

/// The formats of the new readers.
const NEW_FORMATS: [&str; 3] = ["flv", "mpegps", "mpegts"];

/// The samples that are in the new formats, and the format they must be read by.
const EXPECTED: [(&str, &str); 6] = [
    ("unsupported/aac_in_flv.flv", "flv"),
    ("unsupported/mp3_in_flv.flv", "flv"),
    ("unsupported/mp2_in_mpegps.mpg", "mpegps"),
    ("unsupported/aac_in_mpegts.ts", "mpegts"),
    // The files of unsupported audio in FLV are claimed by the FLV reader, which does not open
    // them.
    ("unsupported/adpcm_swf_flv.flv", ""),
    ("unsupported/nellymoser_flv.flv", ""),
];

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.path());

    for entry in entries {
        let path = entry.path();

        if path.is_dir() {
            files(&path, out);
        }
        else if path.is_file() {
            out.push(path);
        }
    }
}

/// A probe with the readers that were registered before the new readers were added.
fn baseline_probe() -> Probe {
    let mut probe = Probe::new();

    probe.register_format::<AdtsReader<'_>>();
    probe.register_format::<LoasReader<'_>>();
    probe.register_format::<ApeReader<'_>>();
    probe.register_format::<CafReader<'_>>();
    probe.register_format::<DsfReader<'_>>();
    probe.register_format::<DffReader<'_>>();
    probe.register_format::<FlacReader<'_>>();
    probe.register_format::<IsoMp4Reader<'_>>();
    probe.register_format::<MpcReader<'_>>();
    probe.register_format::<MpaReader<'_>>();
    probe.register_format::<AiffReader<'_>>();
    probe.register_format::<WavReader<'_>>();
    probe.register_format::<OggReader<'_>>();
    probe.register_format::<MkvReader<'_>>();
    probe.register_format::<WavPackReader<'_>>();

    probe.register_metadata::<symphonia::default::meta::ApeReader<'_>>();
    probe.register_metadata::<symphonia::default::meta::Id3v1Reader<'_>>();
    probe.register_metadata::<symphonia::default::meta::Id3v2Reader<'_>>();

    probe
}

/// Probe the file, and return the short name of the format reader selected, or the error.
fn probe_file(probe: &Probe, path: &Path) -> Result<&'static str, String> {
    let file = File::open(path).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    probe
        .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
        .map(|r| r.format_info().short_name)
        .map_err(|e| e.to_string())
}

/// Probe the file, which must not be a path, and return the short name of the format reader.
fn probe_bytes(probe: &Probe, data: Vec<u8>) -> Result<&'static str, String> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());

    probe
        .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
        .map(|r| r.format_info().short_name)
        .map_err(|e| e.to_string())
}

#[test]
fn new_readers_do_not_take_over_other_formats() {
    let Some(root) = std::env::var_os("RMPD_SAMPLES").map(PathBuf::from)
    else {
        eprintln!("skipping: RMPD_SAMPLES is not set");
        return;
    };

    let mut paths = vec![];
    files(&root, &mut paths);
    assert!(!paths.is_empty());

    let new_probe = symphonia::default::get_probe();
    let old_probe = baseline_probe();

    let baseline_names: Vec<&str> = [
        "aac", "loas", "ape", "caf", "dsf", "dff", "flac", "isomp4", "mpc", "mp3", "mp2", "mp1",
        "aiff", "wav", "ogg", "mkv", "wavpack",
    ]
    .to_vec();

    let mut n_changed = 0;
    let mut n_new = 0;
    let mut n_files = 0;

    for path in &paths {
        let rel = path.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");

        let new = probe_file(new_probe, path);
        let old = probe_file(&old_probe, path);

        n_files += 1;

        let expected = EXPECTED.iter().find(|(p, _)| *p == rel).map(|(_, f)| *f);

        if let Some(format) = expected {
            // The new formats are selected for their own files.
            match (&new, format) {
                (Ok(name), f) if !f.is_empty() => assert_eq!(*name, f, "{rel}"),
                // The reader that claims the file, but does not open it, fails the probe.
                (Err(_), "") => (),
                other => panic!("{rel}: unexpected probe result {other:?}"),
            }

            n_changed += 1;
            continue;
        }

        // For all other files, the new readers are not selected unless the file was not read before
        // (a program stream that the MPEG audio reader rejects is such a file).
        if let (Ok(name), true) =
            (&new, NEW_FORMATS.iter().any(|f| new.as_ref().is_ok_and(|n| n == f)))
        {
            assert!(old.is_err(), "{rel} was taken over by {name} from {}", old.as_ref().unwrap());
            eprintln!("{rel} is now read by {name} (was: {})", old.as_ref().unwrap_err());
            n_new += 1;
            continue;
        }

        // Whenever a file was read before, it is read by the same reader.
        match (&old, &new) {
            (Ok(old), Ok(new)) => assert_eq!(old, new, "{rel}: reader changed from {old} to {new}"),
            (Ok(old), Err(e)) if baseline_names.contains(old) => {
                panic!("{rel}: was read by {old}, and now fails: {e}")
            }
            _ => (),
        }
    }

    eprintln!("probed {n_files} files, {n_changed} are in the new formats, {n_new} are newly read");
}

/// An MPEG program stream pack header, then a PES packet for the audio stream with MPEG audio
/// frames in it.
fn ps_with_mpa() -> Vec<u8> {
    // MPEG-1 layer 2, 128 kbps, 48 kHz: 384 byte frames.
    let frame = |i: u8| {
        let mut f = vec![i; 384];
        f[..4].copy_from_slice(&[0xff, 0xfd, 0x84, 0x04]);
        f
    };

    let mut ps =
        vec![0x00, 0x00, 0x01, 0xba, 0x44, 0x00, 0x04, 0x00, 0x04, 0x01, 0x01, 0x89, 0xc3, 0xf8];

    for n in 0..20u8 {
        let payload: Vec<u8> = [frame(2 * n), frame(2 * n + 1)].concat();
        ps.extend_from_slice(&[0x00, 0x00, 0x01, 0xc0]);
        ps.extend_from_slice(&((payload.len() + 3) as u16).to_be_bytes());
        ps.extend_from_slice(&[0x80, 0x00, 0x00]);
        ps.extend_from_slice(&payload);
    }

    ps
}

#[test]
fn program_streams_are_claimed_by_the_program_stream_reader() {
    let ps = ps_with_mpa();

    // With the program stream reader, the stream is read by it, and not as MPEG audio.
    assert_eq!(probe_bytes(symphonia::default::get_probe(), ps.clone()), Ok("mpegps"));

    // With a junk prefix that contains a pack header start code of a stream with junk after it,
    // the stream is also found.
    let mut prefixed = vec![0x13; 100];
    prefixed.extend_from_slice(&ps);
    assert_eq!(probe_bytes(symphonia::default::get_probe(), prefixed), Ok("mpegps"));

    // Without it, the MPEG audio reader (which rejects program streams) does not take it over.
    assert!(probe_bytes(&baseline_probe(), ps.clone()).is_err());

    // The same MPEG audio frames without the program stream are still read as MPEG audio.
    let frames: Vec<u8> = (0..40u8)
        .flat_map(|i| {
            let mut f = vec![i; 384];
            f[..4].copy_from_slice(&[0xff, 0xfd, 0x84, 0x04]);
            f
        })
        .collect();

    assert_eq!(probe_bytes(symphonia::default::get_probe(), frames), Ok("mp2"));
}

#[test]
fn mpeg_audio_frames_that_look_like_other_formats_are_unaffected() {
    // Data that contains the markers of the new readers, in MPEG audio.
    let frame = |i: u8| {
        let mut f = vec![i; 384];
        f[..4].copy_from_slice(&[0xff, 0xfd, 0x84, 0x04]);
        // A transport stream sync byte every 188 bytes, a pack header, and FLV header.
        for k in (10..384).step_by(188) {
            f[k] = 0x47;
            f[k + 1] = 0x40;
        }
        f[300..304].copy_from_slice(&[0x00, 0x00, 0x01, 0xba]);
        f[310..314].copy_from_slice(b"FLV\x01");
        f
    };

    let data: Vec<u8> = (0..40u8).flat_map(frame).collect();
    assert_eq!(probe_bytes(symphonia::default::get_probe(), data), Ok("mp2"));
}
