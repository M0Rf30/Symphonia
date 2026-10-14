#![allow(clippy::unwrap_used)]
// Test code: a panic is a test failure.
// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests of the readers with the real elementary streams of the sample set. Skipped if
//! `RMPD_SAMPLES` is not set.

use std::fs::File;
use std::path::PathBuf;

use symphonia_core::codecs::CodecParameters;
use symphonia_core::formats::probe::{Hint, Probe};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::meta::MetadataOptions;
use symphonia_core::units::Timestamp;
use symphonia_format_es::{Ac3Reader, DtsReader, TrueHdReader};

fn probe() -> Probe {
    let mut probe = Probe::new();
    probe.register_format::<Ac3Reader<'_>>();
    probe.register_format::<DtsReader<'_>>();
    probe.register_format::<TrueHdReader<'_>>();
    probe
}

fn open(name: &str) -> Option<Box<dyn FormatReader + 'static>> {
    let root = PathBuf::from(std::env::var_os("RMPD_SAMPLES")?);
    let file = File::open(root.join(name)).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    Some(
        probe()
            .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
            .unwrap(),
    )
}

struct Expect {
    file: &'static str,
    format: &'static str,
    packets: u64,
    samples: u64,
    rate: u32,
    channels: usize,
}

static EXPECTED: [Expect; 7] = [
    Expect {
        file: "unsupported/ac3_stereo.ac3",
        format: "ac3",
        packets: 938,
        samples: 938 * 1536,
        rate: 48000,
        channels: 2,
    },
    Expect {
        file: "unsupported/ac3_5_1.ac3",
        format: "ac3",
        packets: 938,
        samples: 938 * 1536,
        rate: 48000,
        channels: 6,
    },
    Expect {
        file: "unsupported/eac3_stereo.eac3",
        format: "ac3",
        packets: 938,
        samples: 938 * 1536,
        rate: 48000,
        channels: 2,
    },
    Expect {
        file: "unsupported/eac3_5_1.eac3",
        format: "ac3",
        packets: 938,
        samples: 938 * 1536,
        rate: 48000,
        channels: 6,
    },
    Expect {
        file: "unsupported/dts_5_1.dts",
        format: "dts",
        packets: 2813,
        samples: 2813 * 512,
        rate: 48000,
        channels: 6,
    },
    Expect {
        file: "unsupported/mlp_stereo.mlp",
        format: "truehd",
        packets: 36000,
        samples: 36000 * 40,
        rate: 48000,
        channels: 2,
    },
    Expect {
        file: "unsupported/truehd_5_1.thd",
        format: "truehd",
        packets: 36000,
        samples: 36000 * 40,
        rate: 48000,
        channels: 6,
    },
];

#[test]
fn samples_are_split_into_the_frames_ffprobe_finds() {
    for e in &EXPECTED {
        let Some(mut reader) = open(e.file)
        else {
            eprintln!("skipping: RMPD_SAMPLES is not set");
            return;
        };

        assert_eq!(reader.format_info().short_name, e.format, "{}", e.file);

        let CodecParameters::Audio(params) = reader.tracks()[0].codec_params.clone().unwrap()
        else {
            panic!("not audio");
        };

        assert_eq!(params.sample_rate, Some(e.rate), "{}", e.file);
        assert_eq!(params.channels.as_ref().unwrap().count(), e.channels, "{}", e.file);

        let mut packets = 0;
        let mut samples = 0;

        while let Some(packet) = reader.next_packet().unwrap() {
            // Timestamps are contiguous.
            assert_eq!(packet.pts.get() as u64, samples, "{} packet {packets}", e.file);
            packets += 1;
            samples += packet.dur.get();
        }

        assert_eq!(packets, e.packets, "{}", e.file);
        assert_eq!(samples, e.samples, "{}", e.file);
    }
}

#[test]
fn seeking_lands_on_a_frame_before_the_timestamp() {
    for e in &EXPECTED {
        let Some(mut reader) = open(e.file)
        else {
            return;
        };

        let target = (e.samples / 2) as i64 + 7;

        let seeked = reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(target), track_id: 0 })
            .unwrap();

        assert!(seeked.actual_ts.get() <= target, "{}", e.file);
        assert!(target - seeked.actual_ts.get() < 128 * 40 + 1536 * 2, "{}", e.file);

        let packet = reader.next_packet().unwrap().unwrap();
        assert_eq!(packet.pts, seeked.actual_ts, "{}", e.file);

        // The frame following the seek is the same as that of a read from the start.
        let mut full = open(e.file).unwrap();
        let reference = loop {
            let p = full.next_packet().unwrap().unwrap();
            if p.pts == packet.pts {
                break p;
            }
        };
        assert_eq!(reference.data, packet.data, "{}", e.file);

        // Seek backwards.
        let seeked = reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(3), track_id: 0 })
            .unwrap();
        assert_eq!(seeked.actual_ts.get(), 0, "{}", e.file);
    }
}

const THD: &[u8] = include_bytes!("../../symphonia-codec-truehd/tests/data/surround24.thd");

fn open_bytes(data: &[u8], opts: FormatOptions) -> Box<dyn FormatReader + 'static> {
    let mss =
        MediaSourceStream::new(Box::new(std::io::Cursor::new(data.to_vec())), Default::default());
    probe().probe(&Hint::new(), mss, opts, MetadataOptions::default()).unwrap()
}

#[test]
fn truehd_duration_is_exact_only_with_a_prebuilt_index() {
    let reader = open_bytes(THD, FormatOptions::default());
    assert_eq!(reader.tracks()[0].num_frames, None);

    let mut reader = open_bytes(THD, FormatOptions::default().prebuild_seek_index(true));
    assert_eq!(reader.tracks()[0].num_frames, Some(1600));

    // Seeking uses the index, and still lands on a frame at or before the timestamp.
    let to = SeekTo::Timestamp { ts: Timestamp::new(1000), track_id: 0 };
    let seeked = reader.seek(SeekMode::Accurate, to).unwrap();
    assert!(seeked.actual_ts.get() <= 1000);
    assert!(reader.next_packet().unwrap().is_some());
}
