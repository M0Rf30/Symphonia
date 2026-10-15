#![allow(clippy::unwrap_used)]
// Test code: a panic is a test failure.
// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests of the AMR reader with the files of `tests/fixtures` (see the README there).

use std::io::Cursor;
use std::path::PathBuf;
use std::process::Command;

use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_AMR_NB, CODEC_ID_AMR_WB};
use symphonia_core::errors::Error;
use symphonia_core::formats::probe::{Hint, Probe};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::meta::MetadataOptions;
use symphonia_core::units::Timestamp;
use symphonia_format_amr::AmrReader;
use symphonia_testutil::XorShift64;

const NB_FILES: &[&str] = &["sine_nb.amr", "sample2_nb.amr", "dtx_nodata_nb.amr"];
const WB_FILES: &[&str] = &["synthetic_wb.awb"];

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn open(data: Vec<u8>, prebuild: bool) -> symphonia_core::errors::Result<AmrReader<'static>> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
    AmrReader::try_new(mss, FormatOptions::default().prebuild_seek_index(prebuild))
}

/// All packets as (pts, dur, data).
fn packets(reader: &mut AmrReader<'_>) -> Vec<(i64, u64, Vec<u8>)> {
    let mut out = Vec::new();
    while let Some(p) = reader.next_packet().unwrap() {
        out.push((p.pts.get(), p.dur.get(), p.data.to_vec()));
    }
    out
}

fn header_len(data: &[u8]) -> usize {
    for (magic, extra) in
        [(&b"#!AMR\n"[..], 0), (b"#!AMR-WB\n", 0), (b"#!AMR_MC1.0\n", 4), (b"#!AMR-WB_MC1.0\n", 4)]
    {
        if data.starts_with(magic) {
            return magic.len() + extra;
        }
    }
    panic!("no magic");
}

#[test]
fn packets_are_the_frames_with_toc() {
    for name in NB_FILES.iter().chain(WB_FILES) {
        let data = std::fs::read(fixture(name)).unwrap();
        let wb = data.starts_with(b"#!AMR-WB");
        let mut reader = open(data.clone(), false).unwrap();

        let params = match &reader.tracks()[0].codec_params {
            Some(CodecParameters::Audio(p)) => p.clone(),
            _ => panic!("no audio params"),
        };
        assert_eq!(params.codec, if wb { CODEC_ID_AMR_WB } else { CODEC_ID_AMR_NB });
        assert_eq!(params.sample_rate, Some(if wb { 16000 } else { 8000 }));
        assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(1));

        let pkts = packets(&mut reader);
        let step = if wb { 320 } else { 160 };

        // The file is exactly the header and the packets.
        let joined: Vec<u8> = pkts.iter().flat_map(|p| p.2.iter().copied()).collect();
        assert_eq!(joined, data[header_len(&data)..], "{name}");

        for (i, p) in pkts.iter().enumerate() {
            assert_eq!(p.0, (i * step) as i64);
            assert_eq!(p.1, step as u64);
        }
    }
}

#[test]
fn multichannel_files() {
    for (name, channels, wb) in
        [("synthetic_nb_mc2.amr", 2, false), ("synthetic_wb_mc3.awb", 3, true)]
    {
        let data = std::fs::read(fixture(name)).unwrap();
        let mut reader = open(data.clone(), false).unwrap();

        let Some(CodecParameters::Audio(params)) = &reader.tracks()[0].codec_params
        else {
            panic!("no audio params");
        };
        assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(channels));
        assert_eq!(params.codec, if wb { CODEC_ID_AMR_WB } else { CODEC_ID_AMR_NB });

        let pkts = packets(&mut reader);
        let joined: Vec<u8> = pkts.iter().flat_map(|p| p.2.iter().copied()).collect();
        assert_eq!(joined, data[header_len(&data)..]);
        assert_eq!(pkts.len(), if wb { 40 } else { 60 });
        assert_eq!(pkts[1].0, if wb { 320 } else { 160 });
    }
}

/// Compare the packet sizes and timestamps with ffprobe's. Skipped without ffprobe.
#[test]
fn packets_match_ffprobe() {
    for name in NB_FILES.iter().chain(WB_FILES) {
        let path = fixture(name);
        let Ok(out) = Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "packet=pts,size", "-of", "csv=p=0"])
            .arg(&path)
            .output()
        else {
            return;
        };
        let text = String::from_utf8(out.stdout).unwrap();
        let expected: Vec<(i64, usize)> = text
            .lines()
            .map(|l| {
                let (pts, size) = l.split_once(',').unwrap();
                (pts.parse().unwrap(), size.parse().unwrap())
            })
            .collect();

        let mut reader = open(std::fs::read(&path).unwrap(), false).unwrap();
        let ours: Vec<(i64, usize)> =
            packets(&mut reader).iter().map(|p| (p.0, p.2.len())).collect();

        assert_eq!(ours, expected, "{name}");
    }
}

#[test]
fn duration_with_prebuilt_index() {
    for name in NB_FILES {
        let data = std::fs::read(fixture(name)).unwrap();
        let n = packets(&mut open(data.clone(), false).unwrap()).len();

        let mut reader = open(data, true).unwrap();
        assert_eq!(reader.tracks()[0].num_frames, Some(n as u64 * 160));
        // Reading starts at the first frame after the scan.
        assert_eq!(packets(&mut reader).len(), n);
    }
}

#[test]
fn seeking() {
    for prebuild in [false, true] {
        for name in NB_FILES.iter().chain(WB_FILES) {
            let data = std::fs::read(fixture(name)).unwrap();
            let all = packets(&mut open(data.clone(), false).unwrap());
            let step = all[0].1 as i64;
            let total = all.len() as i64 * step;

            let mut reader = open(data, prebuild).unwrap();
            let mut rng = XorShift64(0x1234);

            // Random targets in both directions, then the edges.
            let mut targets: Vec<i64> = (0..40).map(|_| rng.below(total as usize) as i64).collect();
            targets.extend([0, 1, step - 1, step, total - 1]);

            for ts in targets {
                let seeked = reader
                    .seek(
                        SeekMode::Accurate,
                        SeekTo::Timestamp { ts: Timestamp::new(ts), track_id: 0 },
                    )
                    .unwrap();
                let idx = (ts / step) as usize;
                assert_eq!(seeked.actual_ts.get(), idx as i64 * step, "{name} ts={ts}");
                assert_eq!(seeked.required_ts.get(), ts);

                // The packets after the seek are the ones of the sequential read.
                for expect in all[idx..].iter().take(3) {
                    let p = reader.next_packet().unwrap().unwrap();
                    assert_eq!(p.pts.get(), expect.0);
                    assert_eq!(&*p.data, &expect.2[..]);
                }
            }

            // After the end.
            let res = reader.seek(
                SeekMode::Accurate,
                SeekTo::Timestamp { ts: Timestamp::new(total + step), track_id: 0 },
            );
            assert!(matches!(res, Err(Error::SeekError(_))), "{name}");

            // The reader continues to work.
            let res = reader
                .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(0), track_id: 0 });
            assert!(res.is_ok());
            assert_eq!(reader.next_packet().unwrap().unwrap().pts.get(), 0);
        }
    }
}

#[test]
fn garbage_is_skipped() {
    let data = std::fs::read(fixture("sine_nb.amr")).unwrap();
    let expected = packets(&mut open(data.clone(), false).unwrap());

    // Insert garbage with invalid frame types (FT 10) between the third and fourth frames.
    let at = 6 + expected[..3].iter().map(|p| p.2.len()).sum::<usize>();
    let mut bad = data.clone();
    bad.splice(at..at, [0x50, 0x50, 0x50]);

    let got = packets(&mut open(bad, false).unwrap());

    assert_eq!(got.len(), expected.len());
    for (g, e) in got.iter().zip(&expected) {
        assert_eq!(g.2, e.2);
    }
}

#[test]
fn truncated_last_frame_is_dropped() {
    let data = std::fs::read(fixture("sine_nb.amr")).unwrap();
    let n = packets(&mut open(data.clone(), false).unwrap()).len();

    let mut cut = data;
    cut.truncate(cut.len() - 3);
    let got = packets(&mut open(cut.clone(), false).unwrap());
    assert_eq!(got.len(), n - 1);

    // A file with only a header has no packets.
    let mut reader = open(b"#!AMR\n".to_vec(), false).unwrap();
    assert!(reader.next_packet().unwrap().is_none());
}

#[test]
fn rejects_other_data() {
    for data in [&b""[..], b"#!AM", b"#!AMR", b"#!AMR_MC1.0\n\0\0\0\0", b"RIFF....WAVE"] {
        assert!(open(data.to_vec(), false).is_err());
    }
}

#[test]
fn random_garbage_does_not_panic() {
    let mut rng = XorShift64(99);
    let header = b"#!AMR_MC1.0\n\0\0\0\x03";

    for round in 0..200 {
        let mut data = if round % 2 == 0 { b"#!AMR-WB\n".to_vec() } else { header.to_vec() };
        data.extend((0..rng.below(400)).map(|_| rng.below(256) as u8));

        if let Ok(mut reader) = open(data, round % 3 == 0) {
            while let Ok(Some(_)) = reader.next_packet() {}
            let _ = reader.seek(
                SeekMode::Accurate,
                SeekTo::Timestamp { ts: Timestamp::new(1000), track_id: 0 },
            );
        }
    }
}

#[test]
fn probe_finds_files() {
    let mut probe = Probe::new();
    probe.register_format::<AmrReader<'_>>();

    for name in NB_FILES.iter().chain(WB_FILES).chain(&["synthetic_nb_mc2.amr"]) {
        let file = std::fs::File::open(fixture(name)).unwrap();
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        hint.with_extension("amr");
        let reader =
            probe.probe(&hint, mss, FormatOptions::default(), MetadataOptions::default()).unwrap();
        assert_eq!(reader.format_info().short_name, "amr");
    }

    let junk = MediaSourceStream::new(Box::new(Cursor::new(vec![7u8; 4096])), Default::default());
    assert!(
        probe
            .probe(&Hint::new(), junk, FormatOptions::default(), MetadataOptions::default())
            .is_err()
    );
}
