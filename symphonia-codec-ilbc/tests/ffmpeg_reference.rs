// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Decodes iLBC `.lbc` fixtures bit-exactly against the FFmpeg decoder (pinned hash, and live
//! when `ffmpeg` is on `PATH`). The FFmpeg decoder is fixed-point and so is this one.

use std::path::PathBuf;
use std::process::Command;

use symphonia_codec_ilbc::IlbcDecoder;
use symphonia_core::audio::{Audio, AudioBuffer, GenericAudioBufferRef};
use symphonia_core::codecs::audio::well_known::CODEC_ID_ILBC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_testutil::Fnv;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn params(samples: Option<u64>) -> AudioCodecParameters {
    let mut p = AudioCodecParameters::new();
    p.for_codec(CODEC_ID_ILBC).with_sample_rate(8000);
    if let Some(s) = samples {
        p.with_frames_per_block(s);
    }
    p
}

/// Decodes a `.lbc` file frame by frame. `declare` states the mode in the parameters, otherwise
/// the decoder must detect it from the packet length.
fn decode(name: &str, declare: bool) -> Vec<i16> {
    let data = std::fs::read(fixture(name)).unwrap();
    let (frame_bytes, samples) = match &data[..9] {
        b"#!iLBC20\n" => (38, 160),
        b"#!iLBC30\n" => (50, 240),
        _ => panic!("bad header"),
    };

    let mut dec =
        IlbcDecoder::try_new(&params(declare.then_some(samples)), &AudioDecoderOptions::default())
            .unwrap();

    let mut out = Vec::new();
    let mut tmp = Vec::new();
    for frame in data[9..].chunks_exact(frame_bytes) {
        let pkt = PacketRef::new(0, Default::default(), Default::default(), frame);
        dec.decode_ref(&pkt).unwrap().copy_to_vec_interleaved::<i16>(&mut tmp);
        assert_eq!(tmp.len(), samples as usize);
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

fn check(name: &str, samples: usize, hash: u64) {
    for declare in [true, false] {
        let pcm = decode(name, declare);
        assert_eq!(pcm.len(), samples);
        let mut fnv = Fnv::new();
        for s in &pcm {
            fnv.write(&s.to_le_bytes());
        }
        if std::env::var_os("GOLDEN_PRINT").is_some() {
            println!("{name}: {:#018x}", fnv.finish());
        }
        if let Some(reference) = ffmpeg(name) {
            assert!(pcm == reference, "{name} differs from ffmpeg");
        }
        assert_eq!(fnv.finish(), hash, "{name}");
    }
}

#[test]
fn verify_clean_20ms() {
    check("s20.lbc", 163 * 160, 0x2f8709207af98d9a);
}

#[test]
fn verify_clean_30ms() {
    check("s30.lbc", 108 * 240, 0xcccc668b788bd516);
}

#[test]
fn verify_concealed_20ms() {
    check("c20.lbc", 163 * 160, 0x9b5b31abedf967c7);
}

#[test]
fn verify_concealed_30ms() {
    check("c30.lbc", 108 * 240, 0x1f607bc5562d1d1d);
}

#[test]
fn verify_bit_errors_20ms() {
    check("n20.lbc", 163 * 160, 0x454d9e83a60c3aa2);
}

#[test]
fn verify_bit_errors_30ms() {
    check("n30.lbc", 108 * 240, 0x76e72b4e2464663f);
}

/// Several frames in one packet decode like the frames one by one, and a trailing partial frame
/// is ignored.
#[test]
fn verify_multi_frame_packets() {
    for (name, bytes, samples) in [("s20.lbc", 38, 160), ("s30.lbc", 50, 240)] {
        let data = std::fs::read(fixture(name)).unwrap();
        let single = decode(name, false);

        let mut dec =
            IlbcDecoder::try_new(&params(Some(samples as u64)), &AudioDecoderOptions::default())
                .unwrap();
        let mut out = Vec::new();
        let mut tmp = Vec::new();
        for chunk in data[9..].chunks(bytes * 5) {
            let mut packet = chunk.to_vec();
            if chunk.len() == bytes * 5 {
                packet.extend_from_slice(&[0xff; 7]);
            }
            let pkt = PacketRef::new(0, Default::default(), Default::default(), &packet);
            dec.decode_ref(&pkt).unwrap().copy_to_vec_interleaved::<i16>(&mut tmp);
            assert_eq!(tmp.len() % samples, 0);
            out.extend_from_slice(&tmp);
        }
        assert!(out == single, "{name}");
    }
}

#[test]
fn verify_reset_restarts_the_stream() {
    let data = std::fs::read(fixture("s20.lbc")).unwrap();
    let mut dec =
        IlbcDecoder::try_new(&params(Some(160)), &AudioDecoderOptions::default()).unwrap();
    let mut first = Vec::new();
    let mut tmp = Vec::new();
    for round in 0..2 {
        let mut out = Vec::new();
        for frame in data[9..].chunks_exact(38).take(10) {
            let pkt = PacketRef::new(0, Default::default(), Default::default(), frame);
            dec.decode_ref(&pkt).unwrap().copy_to_vec_interleaved::<i16>(&mut tmp);
            out.extend_from_slice(&tmp);
        }
        if round == 0 {
            first = out;
            dec.reset();
        }
        else {
            assert!(out == first);
        }
    }
}

#[test]
fn verify_malformed_packets() {
    let mut dec =
        IlbcDecoder::try_new(&params(Some(160)), &AudioDecoderOptions::default()).unwrap();
    for len in [0usize, 1, 37] {
        let data = vec![0xa5u8; len];
        let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);
        assert!(dec.decode_ref(&pkt).is_err());
    }

    // Undeclared mode: a length that fits neither frame size.
    let mut dec = IlbcDecoder::try_new(&params(None), &AudioDecoderOptions::default()).unwrap();
    let data = vec![0u8; 39];
    let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);
    assert!(dec.decode_ref(&pkt).is_err());

    // Wrong codec and multi-channel parameters are rejected.
    let mut bad = AudioCodecParameters::new();
    bad.for_codec(symphonia_core::codecs::audio::well_known::CODEC_ID_SPEEX);
    assert!(IlbcDecoder::try_new(&bad, &AudioDecoderOptions::default()).is_err());
    let mut stereo = params(None);
    stereo.with_channels(symphonia_core::audio::layouts::CHANNEL_LAYOUT_STEREO);
    assert!(IlbcDecoder::try_new(&stereo, &AudioDecoderOptions::default()).is_err());
}

/// Arbitrary (corrupt) frames must never panic, in either mode.
#[test]
fn verify_random_frames_do_not_panic() {
    for (samples, bytes) in [(160u64, 38usize), (240, 50)] {
        let mut dec =
            IlbcDecoder::try_new(&params(Some(samples)), &AudioDecoderOptions::default()).unwrap();
        let mut rng = symphonia_testutil::XorShift64(0x1234_5678_9abc_def1);
        let mut data = vec![0u8; bytes * 2];
        for _ in 0..20000 {
            for b in data.iter_mut() {
                *b = (rng.next_u64() >> 24) as u8;
            }
            let pkt = PacketRef::new(0, Default::default(), Default::default(), &data);
            let buf = dec.decode_ref(&pkt).unwrap();
            if let GenericAudioBufferRef::S16(b) = buf {
                let b: &AudioBuffer<i16> = b;
                assert_eq!(b.frames(), 2 * samples as usize);
            }
        }
    }
}
