#![allow(clippy::unwrap_used, dead_code)]
// Test code: a panic is a test failure.
// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Helpers shared by the tests of the readers.

use std::io::Cursor;
use std::path::Path;
use std::process::Command;

use symphonia_codec_adpcm::AdpcmDecoder;
use symphonia_codec_pcm::PcmDecoder;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_DPCM_8SVX_EXP, CODEC_ID_DPCM_8SVX_FIB};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::prelude::{FormatReader, Packet, SeekMode, SeekTo, Track};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::units::Timestamp;
use symphonia_testutil::Fnv;

/// A stream of an in-memory file.
pub fn mss(data: Vec<u8>) -> MediaSourceStream<'static> {
    MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default())
}

pub fn audio_params(track: &Track) -> &AudioCodecParameters {
    match track.codec_params.as_ref() {
        Some(CodecParameters::Audio(params)) => params,
        _ => panic!("track has no audio parameters"),
    }
}

/// Reads all the packets of a reader.
pub fn drain(reader: &mut dyn FormatReader) -> Vec<Packet> {
    let mut packets = Vec::new();

    while let Some(packet) = reader.next_packet().unwrap() {
        packets.push(packet);
    }

    packets
}

/// Decodes all the packets of the first track with the PCM decoder to interleaved 16-bit samples.
pub fn decode_s16(reader: &mut dyn FormatReader) -> Vec<i16> {
    let params = audio_params(&reader.tracks()[0]).clone();
    let mut decoder = PcmDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

    let mut samples = Vec::new();
    let mut tmp = Vec::new();

    while let Some(packet) = reader.next_packet().unwrap() {
        decoder.decode(&packet).unwrap().copy_to_vec_interleaved::<i16>(&mut tmp);
        samples.extend_from_slice(&tmp);
    }

    samples
}

/// Decodes `path` with FFmpeg to raw PCM of the sample format `fmt` (e.g. `s16le`), or returns
/// `None` (a skip) if FFmpeg is not installed.
pub fn ffmpeg_pcm(path: &Path, fmt: &str) -> Option<Vec<u8>> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-f", fmt, "-"])
        .output()
        .ok()?;

    assert!(out.status.success(), "ffmpeg failed: {}", String::from_utf8_lossy(&out.stderr));
    Some(out.stdout)
}

/// The FNV-1a hash of `bytes`.
pub fn fnv_of(bytes: &[u8]) -> u64 {
    let mut fnv = Fnv::new();
    fnv.write(bytes);
    fnv.finish()
}

/// A decoder of the codec of `params`: the ADPCM decoder for 8SVX compression, else PCM.
fn decoder_for(params: &AudioCodecParameters) -> Box<dyn AudioDecoder> {
    let opts = AudioDecoderOptions::default();

    if params.codec == CODEC_ID_DPCM_8SVX_FIB || params.codec == CODEC_ID_DPCM_8SVX_EXP {
        Box::new(AdpcmDecoder::try_new(params, &opts).unwrap())
    }
    else {
        Box::new(PcmDecoder::try_new(params, &opts).unwrap())
    }
}

/// Decodes the remaining packets of the first track to interleaved 16-bit samples.
pub fn decode_any_s16(reader: &mut dyn FormatReader) -> Vec<i16> {
    let params = audio_params(&reader.tracks()[0]).clone();
    let mut decoder = decoder_for(&params);

    let mut samples = Vec::new();
    let mut tmp = Vec::new();

    while let Some(packet) = reader.next_packet().unwrap() {
        decoder.decode(&packet).unwrap().copy_to_vec_interleaved::<i16>(&mut tmp);
        samples.extend_from_slice(&tmp);
    }

    samples
}

/// Decodes the remaining packets of the first track to interleaved 32-bit float samples.
pub fn decode_any_f32(reader: &mut dyn FormatReader) -> Vec<f32> {
    let params = audio_params(&reader.tracks()[0]).clone();
    let mut decoder = decoder_for(&params);

    let mut samples = Vec::new();
    let mut tmp = Vec::new();

    while let Some(packet) = reader.next_packet().unwrap() {
        decoder.decode(&packet).unwrap().copy_to_vec_interleaved::<f32>(&mut tmp);
        samples.extend_from_slice(&tmp);
    }

    samples
}

/// Decodes `path` with FFmpeg to the raw PCM format `fmt`. `input` are the arguments before the
/// input (the demuxer parameters of a headerless file). Returns `None` if FFmpeg is not installed.
pub fn ffmpeg_with(input: &[&str], path: &Path, fmt: &str) -> Option<Vec<u8>> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin"])
        .args(input)
        .arg("-i")
        .arg(path)
        .args(["-f", fmt, "-"])
        .output()
        .ok()?;

    assert!(out.status.success(), "ffmpeg failed: {}", String::from_utf8_lossy(&out.stderr));
    Some(out.stdout)
}

/// Seeks to a number of timestamps in a stream made by `open` and checks that the samples read
/// after each seek are the tail of a full decode, at the timestamp that was reached. Returns the
/// frames of the full decode.
pub fn check_seeks<F>(open: F, channels: usize, targets: &[u64]) -> usize
where
    F: Fn() -> Box<dyn FormatReader + 'static>,
{
    let full = decode_any_s16(open().as_mut());
    let frames = full.len() / channels;

    for &target in targets {
        let mut reader = open();

        let seeked = reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp { ts: Timestamp::new(target as i64), track_id: 0 },
            )
            .unwrap();

        let actual = seeked.actual_ts.get() as usize;
        assert!(actual as u64 <= target.min(frames as u64), "seek to {target} reached {actual}");

        let tail = decode_any_s16(reader.as_mut());
        assert_eq!(tail, full[actual * channels..], "samples after a seek to {target}");
    }

    frames
}
