// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests against real files. The files are not part of the repository: the tests look for them in
//! the directory in the `RMPD_SAMPLES_DIR` environment variable (by default
//! `/home/gianluca/rmpd-samples/samples`), and are skipped when a file is absent.

use std::fs::File;
use std::path::PathBuf;

use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_PCM_ALAW, CODEC_ID_PCM_MULAW, CODEC_ID_PCM_S8,
};
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;
use symphonia_format_caf::CafReader;

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

fn open(name: &str) -> Option<symphonia_core::errors::Result<CafReader<'static>>> {
    let path = sample_path(name)?;
    let mss = MediaSourceStream::new(Box::new(File::open(path).unwrap()), Default::default());
    Some(CafReader::try_new(mss, FormatOptions::default()))
}

#[test]
fn eight_bit_and_companded_pcm_are_supported() {
    for (name, codec) in [
        ("caf/caf_s8.caf", CODEC_ID_PCM_S8),
        ("caf/caf_alaw.caf", CODEC_ID_PCM_ALAW),
        ("caf/caf_ulaw.caf", CODEC_ID_PCM_MULAW),
    ] {
        let Some(reader) = open(name)
        else {
            continue;
        };

        let mut reader = reader.unwrap();
        let params = reader.tracks()[0].codec_params.as_ref().unwrap().audio().unwrap();
        assert_eq!(params.codec, codec, "{name}");

        let mut frames = 0;
        while let Some(packet) = reader.next_packet().unwrap() {
            frames += packet.dur.get();
        }
        assert_eq!(Some(frames), reader.tracks()[0].num_frames, "{name}");
    }
}

#[test]
fn info_chunk_with_huge_entry_count_is_survivable() {
    // The chunk claims 0xFFFFFFFF entries, which must not be used to allocate memory.
    let Some(result) = open("tagfuzz/cafbad_info_count_huge.caf")
    else {
        return;
    };

    // Whether the file is accepted is not important, only that there is no abort.
    let _ = result;
}
