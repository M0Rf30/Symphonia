// Symphonia Shorten integration tests
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bit-exactness of the Shorten decoder.
//!
//! The fixtures of `tests/data` were encoded by the reference encoder (shorten 3.6.1, Tony
//! Robinson) from synthetic signals. The golden FNV-1a hashes in [`GOLDEN`] are those of the
//! samples the reference decoder (`shorten -x`) wrote, as little-endian integers of the sample
//! type. FFmpeg, which only handles streams that start with a WAVE header and 8 or 16 bit
//! signed little-endian (or unsigned 8-bit) samples, is a second oracle where it is installed;
//! those tests are skipped when `ffmpeg` is not in the `PATH`.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use symphonia_bundle_shorten::{ShortenDecoder, ShortenReader};
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::units::Timestamp;
use symphonia_testutil::{Fnv, XorShift64};

/// `(fixture, bytes per sample, channels, frames, FNV-1a of the reference decoder's samples)`.
const GOLDEN: &[(&str, usize, usize, u64, u64)] = &[
    ("s16lh_c2_", 2, 2, 6000, 0x4260fa1f70d4106f),
    ("s16lh_c1_b_128", 2, 1, 6000, 0x31c127f036cdf6c5),
    ("s16hl_c2_p_8_b_512", 2, 2, 6000, 0xd6497b22752e02df),
    ("s16lh_c2_p_32_b_1024", 2, 2, 6000, 0x4bd3035a3e571f4a),
    ("u8_c1_v_2", 1, 1, 6000, 0x5678e8439305b03d),
    ("s8_c2_m_0", 1, 2, 6000, 0x1149bc6f4041c457),
    ("u16lh_c2_p_4", 2, 2, 6000, 0xcf6f05d39c3358cf),
    ("u16hl_c1_m_8_b_64", 2, 1, 6000, 0x61aaaf021bf5e377),
    ("s16lh_c3_p_2", 2, 3, 6000, 0x80c29919a4f4be43),
    ("s16lh_c2_q_4_r_10", 2, 2, 6000, 0xabd631b11cc35cc5),
    ("s16lh_c6_p_1", 2, 6, 6000, 0x409c1dc1f48f404e),
];

/// The fixtures FFmpeg can decode: they start with a WAVE header, and are 16-bit or unsigned
/// 8-bit.
const FFMPEG_FIXTURES: &[&str] = &[
    "s16lh_c2_",
    "s16lh_c1_b_128",
    "s16lh_c2_p_32_b_1024",
    "u8_c1_v_2",
    "s16lh_c3_p_2",
    "s16lh_c2_q_4_r_10",
    "s16lh_c6_p_1",
];

/// Truncated real streams (CD audio, and 8-bit unsigned).
const REAL_FIXTURES: &[&str] = &["real_cd_head", "real_u8_head"];

fn path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(format!("{name}.shn"))
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(path(name)).unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

fn open(data: Vec<u8>) -> ShortenReader<'static> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
    ShortenReader::try_new(mss, FormatOptions::default()).expect("open")
}

fn params(reader: &dyn FormatReader) -> AudioCodecParameters {
    match reader.tracks()[0].codec_params.as_ref().expect("codec params") {
        CodecParameters::Audio(p) => p.clone(),
        _ => panic!("expected audio"),
    }
}

/// The decoded stream as little-endian interleaved integers of the sample type.
struct Decoded {
    bytes_per_sample: usize,
    channels: usize,
    frames: u64,
    pcm: Vec<u8>,
    /// Packets the decoder rejected.
    errors: usize,
}

impl Decoded {
    fn fnv(&self) -> u64 {
        let mut fnv = Fnv::new();
        fnv.write(&self.pcm);
        fnv.finish()
    }
}

fn push_pcm(
    pcm: &mut Vec<u8>,
    buf: &GenericAudioBufferRef<'_>,
    skip: usize,
) -> (usize, usize, usize) {
    macro_rules! interleave {
        ($b:expr, $bytes:expr, $conv:expr) => {{
            let channels = $b.spec().channels().count();
            for i in skip..$b.frames() {
                for c in 0..channels {
                    let v = $b.plane(c).expect("plane")[i];
                    pcm.extend_from_slice(&$conv(v));
                }
            }
            ($bytes, channels, $b.frames() - skip)
        }};
    }

    match buf {
        GenericAudioBufferRef::U8(b) => interleave!(b, 1, |v: u8| [v]),
        GenericAudioBufferRef::S8(b) => interleave!(b, 1, |v: i8| [v as u8]),
        GenericAudioBufferRef::S16(b) => interleave!(b, 2, |v: i16| v.to_le_bytes()),
        GenericAudioBufferRef::U16(b) => interleave!(b, 2, |v: u16| v.to_le_bytes()),
        _ => panic!("unexpected sample format"),
    }
}

fn decode_reader(reader: &mut ShortenReader<'_>) -> Decoded {
    let mut dec =
        ShortenDecoder::try_new(&params(reader), &AudioDecoderOptions::default()).expect("decoder");
    let mut out =
        Decoded { bytes_per_sample: 0, channels: 0, frames: 0, pcm: Vec::new(), errors: 0 };

    while let Ok(Some(packet)) = reader.next_packet() {
        match dec.decode(&packet) {
            Ok(buf) => {
                let (bytes, channels, frames) =
                    push_pcm(&mut out.pcm, &buf, packet.trim_start.get() as usize);
                out.bytes_per_sample = bytes;
                out.channels = channels;
                out.frames += frames as u64;
            }
            Err(symphonia_core::errors::Error::DecodeError(_)) => out.errors += 1,
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    out
}

fn decode(data: Vec<u8>) -> Decoded {
    decode_reader(&mut open(data))
}

/// What FFmpeg decodes `data` to (`u8` or `s16le`), or `None` when it is not installed.
fn ffmpeg_pcm(data: &[u8], bytes_per_sample: usize) -> Option<Vec<u8>> {
    let fmt = if bytes_per_sample == 1 { "u8" } else { "s16le" };

    let mut fnv = Fnv::new();
    fnv.write(data);
    let name = format!("symphonia-shn-{}-{:016x}.shn", std::process::id(), fnv.finish());
    let path = std::env::temp_dir().join(name);
    std::fs::write(&path, data).expect("write temp");

    let out = Command::new("ffmpeg")
        .args(["-v", "quiet", "-nostdin", "-i"])
        .arg(&path)
        .args(["-f", fmt, "-"])
        .stdin(Stdio::null())
        .output();

    let _ = std::fs::remove_file(&path);
    out.ok().filter(|o| !o.stdout.is_empty() || o.status.success()).map(|o| o.stdout)
}

fn compare_ffmpeg(label: &str, data: Vec<u8>) {
    let ours = decode(data.clone());
    let Some(theirs) = ffmpeg_pcm(&data, ours.bytes_per_sample) else {
        eprintln!("ffmpeg not found; skipping {label}");
        return;
    };
    assert_eq!(ours.pcm.len(), theirs.len(), "{label}: length in bytes");
    assert!(ours.pcm == theirs, "{label}: the samples differ");
}

/// For streams cut inside a block. FFmpeg decodes the cut block from the bytes that follow the
/// data in memory, so whether it emits that block at all varies from run to run; everything
/// before it must be identical, and nothing may come after it.
fn compare_ffmpeg_cut(label: &str, data: Vec<u8>) {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(data.clone())), Default::default());
    let ours = match ShortenReader::try_new(mss, FormatOptions::default()) {
        Ok(mut reader) => decode_reader(&mut reader),
        // A stream cut inside its header cannot be opened; it then decodes to nothing.
        Err(_) => {
            Decoded { bytes_per_sample: 2, channels: 0, frames: 0, pcm: Vec::new(), errors: 0 }
        }
    };
    let Some(theirs) = ffmpeg_pcm(&data, ours.bytes_per_sample) else {
        eprintln!("ffmpeg not found; skipping {label}");
        return;
    };
    let block = 256 * ours.channels * ours.bytes_per_sample;
    assert!(theirs.len() >= ours.pcm.len(), "{label}: we decode more than FFmpeg");
    assert!(theirs.len() - ours.pcm.len() <= block, "{label}: FFmpeg decodes more than a block");
    assert!(theirs[..ours.pcm.len()] == ours.pcm[..], "{label}: the samples differ");
}

#[test]
fn fixtures_match_reference_decoder() {
    for &(name, bytes, channels, frames, hash) in GOLDEN {
        let out = decode(fixture(name));
        assert_eq!(out.errors, 0, "{name}: rejected packets");
        assert_eq!(out.bytes_per_sample, bytes, "{name}: sample size");
        assert_eq!(out.channels, channels, "{name}: channels");
        assert_eq!(out.frames, frames, "{name}: frames");
        assert_eq!(out.fnv(), hash, "{name}: samples");
    }
}

#[test]
fn fixtures_match_ffmpeg() {
    for name in FFMPEG_FIXTURES {
        compare_ffmpeg(name, fixture(name));
    }
}

#[test]
fn truncated_real_streams_match_ffmpeg() {
    for name in REAL_FIXTURES {
        compare_ffmpeg(name, fixture(name));
    }
}

#[test]
fn truncated_streams_match_ffmpeg() {
    for name in ["s16lh_c2_", "s16lh_c3_p_2", "u8_c1_v_2"] {
        let original = fixture(name);
        for cut in [60, original.len() / 3, original.len() * 3 / 4, original.len() - 3] {
            compare_ffmpeg_cut(&format!("{name} cut at {cut}"), original[..cut].to_vec());
        }
    }
}

/// Reads the whole fixture, then seeks to a spread of timestamps and checks that the samples that
/// follow are those of the sequential decode.
#[test]
fn seeking_reproduces_the_sequential_decode() {
    let mut rng = XorShift64(0x5eed_0511_a75e_ed01);

    for &(name, bytes, channels, frames, _) in GOLDEN {
        let reference = decode(fixture(name));
        let stride = bytes * channels;

        let mut targets = vec![0, 1, frames / 2, frames - 1];
        targets.extend((0..6).map(|_| rng.below(frames as usize) as u64));

        for target in targets {
            let mut reader = open(fixture(name));
            // Read ahead first so that the seek has to go backwards.
            while let Ok(Some(_)) = reader.next_packet() {}

            let seeked = reader
                .seek(
                    SeekMode::Accurate,
                    SeekTo::Timestamp { ts: Timestamp::new(target as i64), track_id: 0 },
                )
                .expect("seek");
            assert_eq!(seeked.actual_ts.get(), target as i64, "{name}: seek {target}");

            let tail = decode_reader(&mut reader);
            // The first packet carries the samples before the target; the decoder trims them.
            let expected = &reference.pcm[target as usize * stride..];
            assert_eq!(tail.pcm.len(), expected.len(), "{name}: tail length after seek {target}");
            assert!(tail.pcm == expected, "{name}: samples after seek {target}");
        }
    }
}

/// Bit flips in the stream: the decoders must agree on what is recoverable.
#[test]
fn corrupt_streams_match_ffmpeg() {
    let mut rng = XorShift64(0x5417_0000_c0de_0002);

    for name in ["s16lh_c2_", "u8_c1_v_2", "s16lh_c3_p_2"] {
        let original = fixture(name);
        for trial in 0..24 {
            let mut data = original.clone();
            for _ in 0..1 + rng.below(3) {
                let at = 48 + rng.below(data.len() - 48);
                data[at] ^= 1 << rng.below(8);
            }

            let ours = decode(data.clone());
            let Some(theirs) = ffmpeg_pcm(&data, ours.bytes_per_sample.max(1)) else {
                return;
            };
            // Anything a decoder rejects is dropped, so the comparison is on the common prefix:
            // the decoders must not disagree on the samples they both produce.
            let n = ours.pcm.len().min(theirs.len());
            assert!(ours.pcm[..n] == theirs[..n], "{name} trial {trial}: the samples differ");
        }
    }
}
