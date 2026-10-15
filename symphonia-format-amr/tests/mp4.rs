#![allow(clippy::unwrap_used)]
// Test code: a panic is a test failure.
// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `.amr` files and the `samr`/`sawb` tracks of 3GP files (written by FFmpeg with `-c copy`
//! from the same `.amr` files) must hand the decoders the same packets: one frame, with its TOC
//! byte, per packet.

use std::fs::File;
use std::path::PathBuf;

use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_AMR_NB, CODEC_ID_AMR_WB};
use symphonia_core::formats::probe::{Hint, Probe};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::meta::MetadataOptions;
use symphonia_core::units::Timestamp;
use symphonia_format_amr::AmrReader;
use symphonia_format_isomp4::IsoMp4Reader;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn open(name: &str) -> Box<dyn FormatReader + 'static> {
    let mut probe = Probe::new();
    probe.register_format::<AmrReader<'_>>();
    probe.register_format::<IsoMp4Reader<'_>>();

    let mss =
        MediaSourceStream::new(Box::new(File::open(fixture(name)).unwrap()), Default::default());
    probe.probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default()).unwrap()
}

fn packets(reader: &mut dyn FormatReader) -> Vec<(i64, u64, Vec<u8>)> {
    let mut out = Vec::new();
    while let Some(p) = reader.next_packet().unwrap() {
        out.push((p.pts.get(), p.dur.get(), p.data.to_vec()));
    }
    out
}

#[test]
fn amr_and_3gp_packets_are_identical() {
    for (amr, gp3, wb) in [
        ("sine_nb.amr", "sine_nb.3gp", false),
        ("dtx_nodata_nb.amr", "dtx_nodata_nb.3gp", false),
        ("synthetic_wb.awb", "synthetic_wb.3gp", true),
    ] {
        let mut a = open(amr);
        let mut g = open(gp3);

        assert_eq!(g.format_info().short_name, "isomp4");

        let Some(CodecParameters::Audio(params)) = &g.tracks()[0].codec_params
        else {
            panic!("no audio params");
        };
        eprintln!("{params:?}");
        assert_eq!(params.codec, if wb { CODEC_ID_AMR_WB } else { CODEC_ID_AMR_NB });
        assert_eq!(params.sample_rate, Some(if wb { 16000 } else { 8000 }));
        assert_eq!(params.channels, Some(Channels::Positioned(Position::FRONT_LEFT)));
        assert_eq!(params.max_frames_per_packet, Some(if wb { 320 } else { 160 }));
        // The damr box: vendor, version, mode set, mode change period, frames per sample. FFmpeg
        // does not write it into `sawb` entries.
        if wb {
            assert_eq!(params.extra_data, None);
        }
        else {
            assert_eq!(params.extra_data.as_deref().map(<[u8]>::len), Some(9));
            assert_eq!(params.extra_data.as_deref().map(|d| d[8]), Some(1));
        }

        let pa = packets(&mut *a);
        let pg = packets(&mut *g);
        assert_eq!(pa.len(), pg.len(), "{gp3}");
        assert_eq!(pa, pg, "{gp3}");

        // Timestamps are in samples in both containers.
        let rate = if wb { 16000 } else { 8000 };
        assert_eq!(a.tracks()[0].time_base, g.tracks()[0].time_base);
        assert_eq!(g.tracks()[0].time_base.unwrap().calc_time(Timestamp::new(rate)).unwrap().as_secs_f64(), 1.0);
    }
}

#[test]
fn seeking_in_3gp() {
    let mut reader = open("sine_nb.3gp");
    let all = packets(&mut *reader);
    let id = reader.tracks()[0].id;

    for idx in [0, 1, 17, 100, all.len() - 1, 3] {
        let ts = all[idx].0 + 5;
        let seeked = reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(ts), track_id: id })
            .unwrap();
        assert_eq!(seeked.actual_ts.get(), all[idx].0);
        let p = reader.next_packet().unwrap().unwrap();
        assert_eq!(&*p.data, &all[idx].2[..]);
    }
}
