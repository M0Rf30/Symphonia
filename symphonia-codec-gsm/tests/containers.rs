// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Decodes the same 4 s of speech (see `tests/fixtures/README.md`) from every container that
//! maps GSM 06.10 and checks that the PCM is the one of the raw stream (`ffmpeg_reference.rs`).

use std::io::Cursor;
use std::path::PathBuf;

use symphonia_codec_gsm::GsmDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_GSM, CODEC_ID_GSM_MS};
use symphonia_core::codecs::audio::{AudioCodecId, AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia_core::io::MediaSourceStream;
use symphonia_format_caf::CafReader;
use symphonia_format_isomp4::IsoMp4Reader;
use symphonia_format_mkv::MkvReader;
use symphonia_format_riff::AiffReader;
use symphonia_testutil::{Fnv, open_mss};

/// The FNV-1a hash of the 32000 samples of `speech`, as pinned by `ffmpeg_reference.rs`.
const SPEECH_HASH: u64 = 0xb8e80c5cf35020b0;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// Decodes the audio track of `reader` and checks that it is the speech fixture.
fn verify(mut reader: Box<dyn FormatReader + '_>, codec: AudioCodecId, packets: usize) {
    let track = reader.default_track(TrackType::Audio).unwrap();
    let Some(CodecParameters::Audio(params)) = &track.codec_params
    else {
        panic!("no audio")
    };
    assert_eq!(params.codec, codec);
    assert_eq!(params.sample_rate, Some(8000));
    assert_eq!(params.channels.as_ref().unwrap().count(), 1);
    let params = params.clone();

    let mut dec = GsmDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
    let (mut fnv, mut samples, mut count) = (Fnv::new(), 0, 0);
    let mut tmp: Vec<i16> = Vec::new();

    while let Some(pkt) = reader.next_packet().unwrap() {
        let GenericAudioBufferRef::S16(buf) = dec.decode(&pkt).unwrap()
        else {
            panic!("expected s16 output")
        };
        buf.copy_to_vec_interleaved(&mut tmp);
        for s in &tmp {
            fnv.write(&s.to_le_bytes());
        }
        samples += tmp.len();
        count += 1;
    }

    assert_eq!(samples, 32000);
    assert_eq!(fnv.finish(), SPEECH_HASH);
    assert_eq!(count, packets);
}

fn caf(name: &str, codec: AudioCodecId, packets: usize) {
    let reader = CafReader::try_new(open_mss(&fixture(name)), FormatOptions::default()).unwrap();
    verify(Box::new(reader), codec, packets);
}

fn mov(name: &str, codec: AudioCodecId, packets: usize) {
    let reader = IsoMp4Reader::try_new(open_mss(&fixture(name)), FormatOptions::default()).unwrap();
    verify(Box::new(reader), codec, packets);
}

#[test]
fn caf_agsm() {
    caf("fr_speech.caf", CODEC_ID_GSM, 200);
}

#[test]
fn caf_ms_gsm() {
    caf("ms_speech.caf", CODEC_ID_GSM_MS, 100);
}

#[test]
fn quicktime_agsm() {
    mov("fr_speech.mov", CODEC_ID_GSM, 200);
}

#[test]
fn quicktime_ms_gsm() {
    mov("ms_speech.mov", CODEC_ID_GSM_MS, 100);
}

#[test]
fn matroska_ms_acm() {
    let reader =
        MkvReader::try_new(open_mss(&fixture("ms_speech.mkv")), FormatOptions::default()).unwrap();
    verify(Box::new(reader), CODEC_ID_GSM_MS, 100);
}

/// AIFF-C `GSM ` (FFmpeg cannot write it): the blocks of the raw stream in an `SSND` chunk.
#[test]
fn aifc_gsm() {
    fn chunk(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = tag.to_vec();
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
        if payload.len() % 2 == 1 {
            out.push(0);
        }
        out
    }

    let raw = std::fs::read(fixture("fr_speech.gsm")).unwrap();

    // 1 channel, the number of frames, 16 bits, 8 kHz (80-bit extended), 'GSM ' with an empty
    // compression name.
    let mut comm = 1u16.to_be_bytes().to_vec();
    comm.extend_from_slice(&32000u32.to_be_bytes());
    comm.extend_from_slice(&16u16.to_be_bytes());
    comm.extend_from_slice(&[0x40, 0x0b, 0xfa, 0, 0, 0, 0, 0, 0, 0]);
    comm.extend_from_slice(b"GSM \0\0");

    let mut ssnd = vec![0u8; 8];
    ssnd.extend_from_slice(&raw);

    let body = [b"AIFC".to_vec(), chunk(b"COMM", &comm), chunk(b"SSND", &ssnd)].concat();
    let mut file = b"FORM".to_vec();
    file.extend_from_slice(&(body.len() as u32).to_be_bytes());
    file.extend_from_slice(&body);

    let mss = MediaSourceStream::new(Box::new(Cursor::new(file)), Default::default());
    let reader = AiffReader::try_new(mss, FormatOptions::default()).unwrap();
    // The reader groups whole blocks into packets: 200 blocks make 28 packets of 7 blocks and a
    // last one of 4.
    verify(Box::new(reader), CODEC_ID_GSM, 29);
}
