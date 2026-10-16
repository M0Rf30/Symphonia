// Symphonia TAK integration tests
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bit-exactness against FFmpeg's TAK decoder. The tests are skipped when `ffmpeg` is not in the
//! `PATH`. The fixtures of `tests/data` were encoded with Takc 2.3.3 (presets 0 to 4M, frame size
//! limits 512 to 2048, 1 to 6 channels, 8, 16 and 24 bit); `has-tags.tak` and `silence-44-s.tak`
//! come from FFmpeg's FATE samples.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use symphonia_bundle_tak::{TakDecoder, TakReader};
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::errors::Error;
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::units::Timestamp;
use symphonia_testutil::{Fnv, XorShift64};

const FIXTURES: &[&str] = &[
    "c1_u8_44_p2_f512",
    "c1_s16_22_p0",
    "c1_s16_44_p4m_f512",
    "c1_s24_44_p2_f1024",
    "c2_u8_44_p1",
    "c2_s16_8_p3_f512",
    "c2_s16_44_p2",
    "c2_s16_44_p2_md5",
    "c2_s16_96_p4",
    "c2_s24_44_p3m_f512",
    "c3_s16_44_p2_f1024",
    "c4_s16_44_p2",
    "c5_s16_44_p1",
    "c6_s16_44_p2_f512",
    "has-tags",
    "silence-44-s",
];

fn fixture(name: &str) -> Vec<u8> {
    let path: PathBuf =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(format!("{name}.tak"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

fn open(data: Vec<u8>) -> Result<TakReader<'static>, Error> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
    TakReader::try_new(mss, FormatOptions::default())
}

struct Decoded {
    channels: usize,
    /// Interleaved samples, left aligned in 32 bits like `ffmpeg -f s32le`.
    pcm: Vec<u8>,
    errors: usize,
}

fn decode_reader(reader: &mut TakReader<'_>, verify: bool) -> Decoded {
    let CodecParameters::Audio(params) =
        reader.tracks()[0].codec_params.clone().expect("codec params")
    else {
        panic!("expected audio")
    };
    let mut dec = TakDecoder::try_new(&params, &AudioDecoderOptions::default().verify(verify))
        .expect("decoder");

    let mut out = Decoded { channels: 0, pcm: Vec::new(), errors: 0 };
    while let Ok(Some(packet)) = reader.next_packet() {
        match dec.decode(&packet) {
            Ok(GenericAudioBufferRef::S32(buf)) => {
                out.channels = buf.spec().channels().count();
                for i in 0..buf.frames() {
                    for c in 0..out.channels {
                        out.pcm.extend_from_slice(&buf.plane(c).expect("plane")[i].to_le_bytes());
                    }
                }
            }
            Ok(_) => panic!("unexpected sample format"),
            Err(Error::DecodeError(_)) => out.errors += 1,
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
    out
}

fn decode(data: Vec<u8>, verify: bool) -> Decoded {
    decode_reader(&mut open(data).expect("open"), verify)
}

/// What FFmpeg decodes `data` to (`s32le`), or `None` when it is not installed.
fn ffmpeg_pcm(data: &[u8]) -> Option<Vec<u8>> {
    let mut fnv = Fnv::new();
    fnv.write(data);
    let name = format!("symphonia-tak-{}-{:016x}.tak", std::process::id(), fnv.finish());
    let path = std::env::temp_dir().join(name);
    std::fs::write(&path, data).expect("write temp");
    let out = Command::new("ffmpeg")
        .args(["-v", "quiet", "-nostdin", "-i"])
        .arg(&path)
        .args(["-f", "s32le", "-"])
        .stdin(Stdio::null())
        .output();
    let _ = std::fs::remove_file(&path);
    out.ok().filter(|o| !o.stdout.is_empty() || o.status.success()).map(|o| o.stdout)
}

#[test]
fn fixtures_match_ffmpeg() {
    for name in FIXTURES {
        let data = fixture(name);
        let ours = decode(data.clone(), true);
        assert_eq!(ours.errors, 0, "{name}: rejected packets");
        assert!(!ours.pcm.is_empty(), "{name}: no samples");

        let Some(theirs) = ffmpeg_pcm(&data)
        else {
            eprintln!("ffmpeg not found; skipping {name}");
            return;
        };
        assert_eq!(ours.pcm.len(), theirs.len(), "{name}: length in bytes");
        assert!(ours.pcm == theirs, "{name}: the samples differ");
    }
}

/// Frames are independent, so the samples after a seek are those of the sequential decode.
#[test]
fn seeking_reproduces_the_sequential_decode() {
    let mut rng = XorShift64(0x7a4b_5eed_0001_c0de);

    for name in FIXTURES {
        let data = fixture(name);
        let reference = decode(data.clone(), false);
        let stride = reference.channels * 4;
        let frames = (reference.pcm.len() / stride) as u64;

        let mut targets = vec![0, 1, frames / 2, frames - 1];
        targets.extend((0..4).map(|_| rng.below(frames as usize) as u64));

        for target in targets {
            let mut reader = open(data.clone()).expect("open");
            while let Ok(Some(_)) = reader.next_packet() {}

            let seeked = reader
                .seek(
                    SeekMode::Accurate,
                    SeekTo::Timestamp { ts: Timestamp::new(target as i64), track_id: 0 },
                )
                .unwrap_or_else(|e| panic!("{name}: seek {target}: {e}"));
            let actual = seeked.actual_ts.get() as u64;
            assert!(actual <= target, "{name}: seek {target} landed at {actual}");

            let tail = decode_reader(&mut reader, false);
            assert!(
                tail.pcm == reference.pcm[actual as usize * stride..],
                "{name}: samples after seek {target}"
            );
        }
    }
}

/// A stream cut anywhere. A frame cut short is rejected by this decoder (and by the CRC check);
/// FFmpeg decodes it from the zeros that follow the data. Every frame decoded here must be the
/// frame FFmpeg decodes.
#[test]
fn truncated_streams_match_ffmpeg() {
    for name in ["c2_s16_44_p2_md5", "c6_s16_44_p2_f512", "c1_s24_44_p2_f1024"] {
        let original = fixture(name);
        for cut in
            [original.len() / 5, original.len() / 2, original.len() * 4 / 5, original.len() - 3]
        {
            let data = original[..cut].to_vec();
            let ours = match open(data.clone()) {
                Ok(mut reader) => decode_reader(&mut reader, true),
                Err(_) => Decoded { channels: 0, pcm: Vec::new(), errors: 0 },
            };
            let Some(theirs) = ffmpeg_pcm(&data)
            else {
                return;
            };
            assert!(ours.pcm.len() <= theirs.len(), "{name} cut at {cut}: more samples");
            assert!(
                ours.pcm[..] == theirs[..ours.pcm.len()],
                "{name} cut at {cut}: the samples differ"
            );
        }
    }
}

/// Bit flips in the frames. FFmpeg does not check the frame CRC by default, so this decoder does
/// not either (`verify` is off): a frame that is not rejected for another reason decodes to the
/// same samples. Where both decode every frame, the output is identical.
#[test]
fn corrupt_streams_match_ffmpeg() {
    let mut rng = XorShift64(0x7a4b_c0de_0000_0002);
    let mut compared = 0;

    for name in ["c2_s16_44_p2_md5", "c4_s16_44_p2", "c1_s16_44_p4m_f512"] {
        let original = fixture(name);
        for trial in 0..24 {
            let mut data = original.clone();
            for _ in 0..1 + rng.below(3) {
                let at = 64 + rng.below(data.len() - 64);
                data[at] ^= 1 << rng.below(8);
            }

            let Ok(mut reader) = open(data.clone())
            else {
                continue;
            };
            let ours = decode_reader(&mut reader, false);
            let Some(theirs) = ffmpeg_pcm(&data)
            else {
                return;
            };
            if ours.errors == 0 && ours.pcm.len() == theirs.len() {
                assert!(ours.pcm == theirs, "{name} trial {trial}: the samples differ");
                compared += 1;
            }
        }
    }
    assert!(compared > 20, "only {compared} corrupt streams were comparable");
}
