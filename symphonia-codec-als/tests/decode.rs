// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Decodes the FFmpeg FATE `als_*.mp4` samples (from `RMPD_SAMPLES`, skipped when absent) and
//! compares the output with the output of FFmpeg's `mp4als` decoder (`-f s16le`/`f32le`): the
//! samples must match bit for bit. The hashes are FNV-1a over the little-endian output bytes.
//!
//! Set `ALS_DUMP_DIR` to also write the raw decoded output of each sample there.

use symphonia_codec_als::AlsDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia_format_isomp4::IsoMp4Reader;
use symphonia_testutil::{Fnv, open_sample};

fn decode(name: &str, verify: bool) -> Option<(Vec<u8>, Option<bool>)> {
    let mss = open_sample(name)?;
    let mut reader = IsoMp4Reader::try_new(mss, FormatOptions::default()).unwrap();
    let track = reader.default_track(TrackType::Audio).unwrap();
    let id = track.id;
    let Some(CodecParameters::Audio(params)) = track.codec_params.clone()
    else {
        panic!("not audio");
    };
    let opts = AudioDecoderOptions::default().verify(verify).gapless(false);
    let mut dec = AlsDecoder::try_new(&params, &opts).unwrap();

    let mut out = Vec::new();
    while let Some(packet) = reader.next_packet().unwrap() {
        if packet.track_id != id {
            continue;
        }
        match dec.decode(&packet).unwrap() {
            GenericAudioBufferRef::S16(b) => {
                let mut v = Vec::new();
                b.copy_to_vec_interleaved::<i16>(&mut v);
                v.iter().for_each(|s| out.extend_from_slice(&s.to_le_bytes()));
            }
            GenericAudioBufferRef::S32(b) => {
                let mut v = Vec::new();
                b.copy_to_vec_interleaved::<i32>(&mut v);
                v.iter().for_each(|s| out.extend_from_slice(&s.to_le_bytes()));
            }
            GenericAudioBufferRef::F32(b) => {
                let mut v = Vec::new();
                b.copy_to_vec_interleaved::<f32>(&mut v);
                v.iter().for_each(|s| out.extend_from_slice(&s.to_le_bytes()));
            }
            _ => panic!("unexpected sample format"),
        }
    }

    Some((out, dec.finalize().verify_ok))
}

fn check(name: &str, len: usize, hash: u64) {
    let Some((out, _)) = decode(name, false)
    else {
        return;
    };
    if let Ok(dir) = std::env::var("ALS_DUMP_DIR") {
        std::fs::write(format!("{dir}/{}.raw", name.replace('/', "_")), &out).unwrap();
    }
    let mut h = Fnv::new();
    h.write(&out);
    assert_eq!((out.len(), h.finish()), (len, hash), "{name}");
}

#[test]
fn matches_ffmpeg() {
    for n in 0..6 {
        let name = format!("als/als_0{n}_2ch48k16b.mp4");
        check(&name, 2843636, 0x9e20827af0a69c6e);
    }
    // The last frame of this sample is cut off: FFmpeg outputs it as silence (16276 bytes of
    // zeros after the 22937600 bytes that match), the decoder drops it.
    check("als/als_07_2ch192k32bF.mp4", 22937600, 0xc8b3db63e409a62e);
    check("als/als_09_512ch2k16b.mp4", 3154944, 0xfa91d71dfd115dfd);
}

#[test]
fn crc_verification_passes() {
    for n in ["als_00_2ch48k16b", "als_03_2ch48k16b"] {
        if let Some((_, verified)) = decode(&format!("als/{n}.mp4"), true) {
            assert_eq!(verified, Some(true), "{n}");
        }
    }
}

/// Open `name` and return its decoder and the packets of the audio track.
fn open(name: &str) -> Option<(AlsDecoder, Vec<symphonia_core::packet::Packet>)> {
    let mss = open_sample(name)?;
    let mut reader = IsoMp4Reader::try_new(mss, FormatOptions::default()).unwrap();
    let track = reader.default_track(TrackType::Audio).unwrap();
    let id = track.id;
    let Some(CodecParameters::Audio(params)) = track.codec_params.clone()
    else {
        panic!("not audio");
    };
    let opts = AudioDecoderOptions::default().gapless(false);
    let dec = AlsDecoder::try_new(&params, &opts).unwrap();
    let mut packets = Vec::new();
    while let Some(p) = reader.next_packet().unwrap() {
        if p.track_id == id {
            packets.push(p);
        }
    }
    Some((dec, packets))
}

fn decode_s16(dec: &mut AlsDecoder, p: &symphonia_core::packet::Packet) -> Vec<i16> {
    match dec.decode(p).unwrap() {
        GenericAudioBufferRef::S16(b) => {
            let mut v = Vec::new();
            b.copy_to_vec_interleaved::<i16>(&mut v);
            v
        }
        _ => panic!("unexpected sample format"),
    }
}

#[test]
fn reset_forgets_the_history_of_the_previous_frames() {
    for n in 0..6 {
        let name = format!("als/als_0{n}_2ch48k16b.mp4");
        let Some((mut fresh, packets)) = open(&name)
        else {
            return;
        };
        let (mut dec, _) = open(&name).unwrap();
        let expected = decode_s16(&mut fresh, &packets[0]);

        // Decode a few frames, then start over with the first one.
        for p in &packets[..packets.len().min(5)] {
            decode_s16(&mut dec, p);
        }
        dec.reset();
        assert!(decode_s16(&mut dec, &packets[0]) == expected, "{name}");
    }
}

#[test]
fn frames_past_the_declared_samples_are_empty() {
    let Some((mut dec, packets)) = open("als/als_00_2ch48k16b.mp4")
    else {
        return;
    };
    for p in &packets {
        decode_s16(&mut dec, p);
    }
    // The stream is over: a further frame must not be a full frame of garbage.
    let extra = dec.decode(&packets[0]);
    match extra {
        Ok(GenericAudioBufferRef::S16(b)) => assert_eq!(b.frames(), 0),
        Ok(_) => panic!("unexpected sample format"),
        Err(_) => {}
    }
}
