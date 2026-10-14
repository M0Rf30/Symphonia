// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Decodes real TrueSpeech WAV files through the WAV demuxer and compares the PCM bit-exactly
//! with the FFmpeg decoder (pinned hash, and live when `ffmpeg` is on `PATH`).

use std::path::PathBuf;
use std::process::Command;

use symphonia_codec_truespeech::TrueSpeechDecoder;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_TRUESPEECH;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia_format_riff::WavReader;
use symphonia_testutil::{Fnv, open_mss};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn decode(name: &str) -> Vec<i16> {
    let mut reader =
        WavReader::try_new(open_mss(&fixture(name)), FormatOptions::default()).unwrap();
    let track = reader.default_track(TrackType::Audio).unwrap();
    let Some(CodecParameters::Audio(params)) = &track.codec_params
    else {
        panic!("no audio")
    };
    assert_eq!(params.codec, CODEC_ID_TRUESPEECH);
    assert_eq!(params.sample_rate, Some(8000));
    assert_eq!(params.channels.as_ref().unwrap().count(), 1);
    let params = params.clone();

    let mut dec = TrueSpeechDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
    let (mut out, mut tmp) = (Vec::new(), Vec::new());
    while let Some(pkt) = reader.next_packet().unwrap() {
        dec.decode(&pkt).unwrap().copy_to_vec_interleaved::<i16>(&mut tmp);
        out.extend_from_slice(&tmp);
    }
    out
}

fn ffmpeg(name: &str) -> Option<Vec<i16>> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(fixture(name))
        .args(["-f", "s16le", "-"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| out.stdout.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect())
}

fn check(name: &str, frames: usize, hash: u64) {
    let pcm = decode(name);
    assert_eq!(pcm.len(), frames);
    let mut fnv = Fnv::new();
    for s in &pcm {
        fnv.write(&s.to_le_bytes());
    }
    assert_eq!(fnv.finish(), hash, "{name}");
    if let Some(reference) = ffmpeg(name) {
        assert!(pcm == reference, "{name} differs from ffmpeg");
    }
}

#[test]
fn verify_a6() {
    check("a6.wav", 346080, 0xa8ec8d62d0a2ea5b);
}

#[test]
fn verify_tada() {
    check("tada.wav", 15360, 0xb90c10e2be9738ad);
}

#[test]
fn verify_a5() {
    check("truespeech_a5.wav", 152400, 0x8d388284e3f7a574);
}

#[test]
fn verify_truncated_and_garbage_packets() {
    let mut params = symphonia_core::codecs::audio::AudioCodecParameters::new();
    params.for_codec(CODEC_ID_TRUESPEECH).with_sample_rate(8000);
    let mut dec = TrueSpeechDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
    for len in [0usize, 1, 31] {
        let data = vec![0xa5u8; len];
        let pkt = symphonia_core::packet::PacketRef::new(
            0,
            Default::default(),
            Default::default(),
            &data,
        );
        assert!(dec.decode_ref(&pkt).is_err());
    }
    let data = vec![0xffu8; 64];
    let pkt =
        symphonia_core::packet::PacketRef::new(0, Default::default(), Default::default(), &data);
    assert!(dec.decode_ref(&pkt).is_ok());
}
