// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Decodes iLBC through the CAF and MP4/MOV demuxers (files written by FFmpeg from the `.lbc`
//! fixtures) and compares with the PCM decoded from the raw `.lbc` frames.

use std::path::PathBuf;

use symphonia_codec_ilbc::IlbcDecoder;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_ILBC;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia_core::packet::PacketRef;
use symphonia_format_caf::CafReader;
use symphonia_format_isomp4::IsoMp4Reader;
use symphonia_testutil::open_mss;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// The expected PCM: raw frames decoded one by one with the mode declared.
fn raw(name: &str) -> Vec<i16> {
    let data = std::fs::read(fixture(name)).unwrap();
    let (bytes, samples) = if &data[..9] == b"#!iLBC20\n" { (38, 160) } else { (50, 240) };
    let mut params = symphonia_core::codecs::audio::AudioCodecParameters::new();
    params.for_codec(CODEC_ID_ILBC).with_sample_rate(8000).with_frames_per_block(samples);
    let mut dec = IlbcDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
    let (mut out, mut tmp) = (Vec::new(), Vec::new());
    for f in data[9..].chunks_exact(bytes) {
        let pkt = PacketRef::new(0, Default::default(), Default::default(), f);
        dec.decode_ref(&pkt).unwrap().copy_to_vec_interleaved::<i16>(&mut tmp);
        out.extend_from_slice(&tmp);
    }
    out
}

fn decode(mut reader: Box<dyn FormatReader + '_>) -> (Vec<i16>, u64) {
    let track = reader.default_track(TrackType::Audio).unwrap();
    let Some(CodecParameters::Audio(params)) = &track.codec_params
    else {
        panic!("no audio")
    };
    assert_eq!(params.codec, CODEC_ID_ILBC);
    assert_eq!(params.sample_rate, Some(8000));
    assert_eq!(params.channels.as_ref().unwrap().count(), 1);
    let params = params.clone();

    let mut dec = IlbcDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
    let (mut out, mut tmp) = (Vec::new(), Vec::new());
    let mut packets = 0;
    while let Some(pkt) = reader.next_packet().unwrap() {
        packets += 1;
        dec.decode(&pkt).unwrap().copy_to_vec_interleaved::<i16>(&mut tmp);
        out.extend_from_slice(&tmp);
    }
    (out, packets)
}

#[test]
fn verify_caf() {
    for (name, lbc, frames) in [("s20.caf", "s20.lbc", 163), ("s30.caf", "s30.lbc", 108)] {
        let r = CafReader::try_new(open_mss(&fixture(name)), FormatOptions::default()).unwrap();
        let (pcm, packets) = decode(Box::new(r));
        assert_eq!(packets, frames);
        assert!(pcm == raw(lbc), "{name}");
    }
}

#[test]
fn verify_mov() {
    for (name, lbc, frames) in [("s20.mov", "s20.lbc", 163), ("s30.mov", "s30.lbc", 108)] {
        let r = IsoMp4Reader::try_new(open_mss(&fixture(name)), FormatOptions::default()).unwrap();
        let (pcm, packets) = decode(Box::new(r));
        assert_eq!(packets, frames);
        assert!(pcm == raw(lbc), "{name}");
    }
}
