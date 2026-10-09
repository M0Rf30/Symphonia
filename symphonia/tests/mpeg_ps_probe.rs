// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An MPEG program stream must not be probed as raw MPEG audio. Uses the sample in
//! `$RMPD_SAMPLES/unsupported/mp2_in_mpegps.mpg`; skipped if absent.

use std::fs::File;
use std::path::PathBuf;

use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSourceStream;

#[test]
fn mpeg_program_stream_is_not_probed_as_mpeg_audio() {
    let dir = std::env::var_os("RMPD_SAMPLES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/home/gianluca/rmpd-samples/samples"));
    let path = dir.join("unsupported/mp2_in_mpegps.mpg");

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

    if let Ok(reader) = result {
        panic!("probed as {}", reader.format_info().short_name);
    }
}
