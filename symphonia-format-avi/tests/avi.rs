#![allow(clippy::unwrap_used)]
// Test code: a panic is a test failure.
// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bit-exactness against FFmpeg. The comparisons are skipped when `ffmpeg` is not installed.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

use symphonia_bundle_mp3::MpaDecoder;
use symphonia_codec_adpcm::AdpcmDecoder;
use symphonia_codec_pcm::PcmDecoder;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::*;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::probe::{Hint, Probe};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::units::Timestamp;
use symphonia_format_avi::AviReader;

fn path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)
}

fn mss(data: Vec<u8>) -> MediaSourceStream<'static> {
    MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default())
}

fn open_bytes(data: Vec<u8>) -> Box<dyn FormatReader + 'static> {
    Box::new(AviReader::try_new(mss(data), FormatOptions::default()).unwrap())
}

fn open(name: &str) -> Box<dyn FormatReader + 'static> {
    open_bytes(std::fs::read(path(name)).unwrap())
}

fn decoder(reader: &dyn FormatReader, track: usize) -> Box<dyn AudioDecoder> {
    let Some(CodecParameters::Audio(params)) = reader.tracks()[track].codec_params.as_ref() else {
        panic!("not audio");
    };
    let opts = AudioDecoderOptions::default();

    match params.codec {
        CODEC_ID_ADPCM_MS | CODEC_ID_ADPCM_IMA_WAV => {
            Box::new(AdpcmDecoder::try_new(params, &opts).unwrap())
        }
        CODEC_ID_MP2 | CODEC_ID_MP3 => Box::new(MpaDecoder::try_new(params, &opts).unwrap()),
        _ => Box::new(PcmDecoder::try_new(params, &opts).unwrap()),
    }
}

/// Decodes the remaining packets of the first track to interleaved 16-bit samples.
fn decode_s16(reader: &mut dyn FormatReader) -> Vec<i16> {
    let mut dec = decoder(reader, 0);
    let mut out = Vec::new();
    let mut tmp = Vec::new();

    while let Some(packet) = reader.next_packet().unwrap() {
        dec.decode(&packet).unwrap().copy_to_vec_interleaved::<i16>(&mut tmp);
        out.extend_from_slice(&tmp);
    }

    out
}

fn ffmpeg_s16(path: &Path) -> Option<Vec<i16>> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error", "-nostdin"]);

    // The floating point layer II decoder (the fixed point one differs).
    if path.to_string_lossy().contains("mp2_") {
        cmd.args(["-c:a", "mp2float"]);
    }

    let out = cmd.arg("-i").arg(path).args(["-vn", "-f", "s16le", "-"]).output().ok()?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    Some(out.stdout.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect())
}

/// (file, codec, rate, channels, max difference to FFmpeg in LSBs)
const FILES: &[(&str, AudioCodecIdAlias, u32, usize, i32)] = &[
    ("s16_stereo.avi", CODEC_ID_PCM_S16LE, 22050, 2, 0),
    ("u8_mono.avi", CODEC_ID_PCM_U8, 22050, 1, 0),
    ("s24_stereo.avi", CODEC_ID_PCM_S24LE, 22050, 2, 0),
    // Only the float to 16-bit conversion differs (compared exactly as float below).
    ("f32_stereo.avi", CODEC_ID_PCM_F32LE, 22050, 2, 1),
    ("alaw_stereo.avi", CODEC_ID_PCM_ALAW, 8000, 2, 0),
    ("mulaw_mono.avi", CODEC_ID_PCM_MULAW, 8000, 1, 0),
    ("ms_stereo.avi", CODEC_ID_ADPCM_MS, 22050, 2, 0),
    ("ima_stereo.avi", CODEC_ID_ADPCM_IMA_WAV, 22050, 2, 0),
    ("mp3_stereo.avi", CODEC_ID_MP3, 22050, 2, 1),
    ("mp2_stereo.avi", CODEC_ID_MP2, 22050, 2, 1),
    ("video_s16.avi", CODEC_ID_PCM_S16LE, 22050, 1, 0),
];

type AudioCodecIdAlias = symphonia_core::codecs::audio::AudioCodecId;

#[test]
fn parameters() {
    for &(name, codec, rate, channels, _) in FILES {
        let reader = open(name);
        assert_eq!(reader.tracks().len(), 1, "{name}");

        let Some(CodecParameters::Audio(params)) = reader.tracks()[0].codec_params.as_ref() else {
            panic!("{name}: not audio");
        };

        assert_eq!(params.codec, codec, "{name}");
        assert_eq!(params.sample_rate, Some(rate), "{name}");
        assert_eq!(params.channels.as_ref().map(|c| c.count()), Some(channels), "{name}");
    }
}

#[test]
fn matches_ffmpeg() {
    for &(name, _, _, _, tolerance) in FILES {
        let Some(expected) = ffmpeg_s16(&path(name)) else {
            return;
        };

        let ours = decode_s16(open(name).as_mut());

        assert_eq!(ours.len(), expected.len(), "{name}: length");

        let worst = ours.iter().zip(&expected).map(|(a, b)| (i32::from(*a) - i32::from(*b)).abs());
        assert!(worst.max().unwrap_or(0) <= tolerance, "{name}: samples differ");
    }
}

#[test]
fn float_matches_ffmpeg_exactly() {
    let name = "f32_stereo.avi";

    let Some(out) = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path(name))
        .args(["-f", "f32le", "-"])
        .output()
        .ok()
    else {
        return;
    };

    let mut reader = open(name);
    let mut dec = decoder(reader.as_ref(), 0);
    let mut ours = Vec::new();
    let mut tmp = Vec::new();

    while let Some(packet) = reader.next_packet().unwrap() {
        dec.decode(&packet).unwrap().copy_to_vec_interleaved::<f32>(&mut tmp);
        ours.extend(tmp.iter().flat_map(|s| s.to_le_bytes()));
    }

    assert_eq!(ours, out.stdout);
}

#[test]
fn seeking() {
    for &(name, _, _, channels, _) in FILES {
        let full = decode_s16(open(name).as_mut());
        let frames = (full.len() / channels) as u64;

        for target in [0, 1, 777, frames / 2, frames - 1] {
            let mut reader = open(name);
            let id = reader.tracks()[0].id;

            let seeked = reader
                .seek(
                    SeekMode::Accurate,
                    SeekTo::Timestamp { ts: Timestamp::new(target as i64), track_id: id },
                )
                .unwrap();

            let actual = seeked.actual_ts.get() as usize;
            assert!(actual as u64 <= target, "{name}: seek to {target} reached {actual}");

            // Decoders with a history (MPEG audio, ADPCM predictor state) restart at the packet.
            if name.starts_with("mp") {
                continue;
            }

            let tail = decode_s16(reader.as_mut());
            assert_eq!(tail, full[actual * channels..], "{name}: after a seek to {target}");
        }
    }
}

#[test]
fn probes() {
    let data = std::fs::read(path("s16_stereo.avi")).unwrap();

    let mut probe = Probe::new();
    probe.register_format::<AviReader<'_>>();

    let reader =
        probe.probe(&Hint::new(), mss(data), FormatOptions::default(), Default::default()).unwrap();

    assert_eq!(reader.format_info().short_name, "avi");
}

// --- A hand-built OpenDML file ---

fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = id.to_vec();
    v.extend((body.len() as u32).to_le_bytes());
    v.extend(body);
    if body.len() & 1 == 1 {
        v.push(0);
    }
    v
}

fn list(kind: &[u8; 4], form: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut inner = form.to_vec();
    inner.extend(body);
    chunk(kind, &inner)
}

/// A file of one PCM stream and one stream of an unsupported video, with the audio in two RIFF
/// chunks.
fn opendml() -> (Vec<u8>, Vec<i16>) {
    let samples: Vec<i16> = (0..400).map(|i| (i * 37 % 2000 - 1000) as i16).collect();
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();

    let mut strh = b"auds".to_vec();
    strh.extend([0; 4]);
    strh.extend([0; 4 + 2 + 2 + 4]);
    strh.extend(1u32.to_le_bytes());
    strh.extend(8000u32.to_le_bytes());
    strh.extend(0u32.to_le_bytes());
    strh.extend((samples.len() as u32).to_le_bytes());
    strh.extend([0; 8]);
    strh.extend(2u32.to_le_bytes());
    strh.extend([0; 8]);

    let mut strf = Vec::new();
    strf.extend(1u16.to_le_bytes());
    strf.extend(1u16.to_le_bytes());
    strf.extend(8000u32.to_le_bytes());
    strf.extend(16000u32.to_le_bytes());
    strf.extend(2u16.to_le_bytes());
    strf.extend(16u16.to_le_bytes());

    let mut vids = b"vids".to_vec();
    vids.extend([0; 52]);

    let strl_v =
        list(b"LIST", b"strl", &[chunk(b"strh", &vids), chunk(b"strf", &[0; 40])].concat());
    let strl_a = list(b"LIST", b"strl", &[chunk(b"strh", &strh), chunk(b"strf", &strf)].concat());
    let hdrl = list(b"LIST", b"hdrl", &[chunk(b"avih", &[0; 56]), strl_v, strl_a].concat());

    let half = bytes.len() / 2;
    let movi1 = list(
        b"LIST",
        b"movi",
        &[chunk(b"00dc", &[1, 2, 3]), chunk(b"01wb", &bytes[..half])].concat(),
    );
    let movi2 =
        list(b"LIST", b"movi", &[chunk(b"JUNK", &[0; 5]), chunk(b"01wb", &bytes[half..])].concat());

    let riff = |form: &[u8; 4], body: &[u8]| {
        let mut v = b"RIFF".to_vec();
        v.extend(((body.len() + 4) as u32).to_le_bytes());
        v.extend(form);
        v.extend(body);
        v
    };

    let mut file = riff(b"AVI ", &[hdrl, movi1].concat());
    file.extend(riff(b"AVIX", &movi2));

    (file, samples)
}

#[test]
fn opendml_continuation_and_seek() {
    let (file, samples) = opendml();

    let mut reader = open_bytes(file.clone());
    assert_eq!(reader.tracks().len(), 1);
    assert_eq!(reader.tracks()[0].id, 1);
    assert_eq!(decode_s16(reader.as_mut()), samples);

    // A target in the second RIFF chunk.
    let mut reader = open_bytes(file);
    let seeked = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(300), track_id: 1 })
        .unwrap();
    let actual = seeked.actual_ts.get() as usize;
    assert!(actual <= 300 && actual >= 200);
    assert_eq!(decode_s16(reader.as_mut()), samples[actual..]);
}
