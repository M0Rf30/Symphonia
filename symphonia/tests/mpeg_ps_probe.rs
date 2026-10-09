// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An MPEG program stream must be probed as such (not as raw MPEG audio), and MP3 files must still be. Uses
//! the samples in `$RMPD_SAMPLES`; skipped if absent.

use std::fs::File;
use std::path::PathBuf;

use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSourceStream;

fn samples_dir() -> PathBuf {
    std::env::var_os("RMPD_SAMPLES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/home/gianluca/rmpd-samples/samples"))
}

#[test]
fn mpeg_program_stream_is_not_probed_as_mpeg_audio() {
    let path = samples_dir().join("unsupported/mp2_in_mpegps.mpg");

    if !path.exists() {
        return;
    }

    let mss = MediaSourceStream::new(Box::new(File::open(path).unwrap()), Default::default());
    let result = symphonia::default::get_probe().probe(
        &Hint::new(),
        mss,
        Default::default(),
        Default::default(),
    );

    // Without the MPEG-PS reader the probe finds nothing; with it, the stream is an MPEG-PS.
    if let Ok(reader) = result {
        assert_eq!(reader.format_info().short_name, "mpegps");
    }
}

/// All tagged MP3s are probed as MP3 and begin at the first frame. Prints the first packet of each
/// (`cargo test -- --nocapture`) to compare before/after a change to the MPEG-PS detection.
#[test]
fn tagged_mp3s_are_probed_as_mp3() {
    let dir = samples_dir().join("tags");

    let Ok(entries) = std::fs::read_dir(&dir)
    else {
        return;
    };

    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "mp3"))
        .collect();
    paths.sort();

    for path in paths {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let mss = MediaSourceStream::new(Box::new(File::open(&path).unwrap()), Default::default());

        let mut reader = symphonia::default::get_probe()
            .probe(&Hint::new(), mss, Default::default(), Default::default())
            .unwrap_or_else(|e| panic!("{name}: probe failed: {e:?}"));

        let packet = reader.next_packet().unwrap().expect("a packet");

        println!(
            "TAGMP3 {name} {} frames={:?} first_pts={} len={}",
            reader.format_info().short_name,
            reader.tracks()[0].num_frames,
            packet.pts.get(),
            packet.data.len()
        );

        assert_eq!(reader.format_info().short_name, "mp3", "{name}");
    }
}
