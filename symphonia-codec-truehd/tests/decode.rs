// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Decodes small streams produced by FFmpeg's `mlp` and `truehd` encoders. The expected hashes
//! were generated from the output of FFmpeg's decoder (`ffmpeg -f s32le`), so these tests pin
//! bit-exact output.

use symphonia_codec_truehd::TrueHdDecoder;
use symphonia_core::audio::{Audio, Channels, GenericAudioBufferRef, Position};
use symphonia_core::codecs::audio::well_known::CODEC_ID_TRUEHD;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::Packet;
use symphonia_core::units::{Duration, Timestamp};

/// Split an elementary stream into access units.
fn access_units(mut data: &[u8]) -> Vec<&[u8]> {
    let mut aus = Vec::new();

    while data.len() >= 4 {
        let len = usize::from(u16::from_be_bytes([data[0], data[1]]) & 0xfff) * 2;
        if len < 4 || len > data.len() {
            break;
        }
        let (au, rest) = data.split_at(len);
        aus.push(au);
        data = rest;
    }

    aus
}

fn new_decoder() -> TrueHdDecoder {
    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_TRUEHD);
    TrueHdDecoder::try_new(&params, &AudioDecoderOptions::default())
        .expect("decoder creation failed")
}

fn packet(data: &[u8]) -> Packet {
    Packet::new(0, Timestamp::new(0), Duration::new(0), data.to_vec())
}

fn fnv1a(hash: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *hash = (*hash ^ u64::from(b)).wrapping_mul(0x100000001b3);
    }
}

/// Decode a whole stream. Returns the FNV-1a hash of the interleaved little-endian samples, the
/// number of frames, and the channels and sample rate of the last decoded buffer.
fn decode_all(data: &[u8]) -> (u64, usize, Channels, u32) {
    let mut dec = new_decoder();
    let mut hash = 0xcbf29ce484222325;
    let mut frames = 0;
    let mut layout = None;

    for au in access_units(data) {
        match dec.decode(&packet(au)).expect("decode failed") {
            GenericAudioBufferRef::S32(buf) => {
                for i in 0..buf.frames() {
                    for c in 0..buf.num_planes() {
                        fnv1a(&mut hash, &buf.plane(c).expect("plane")[i].to_le_bytes());
                    }
                }
                frames += buf.frames();
                layout = Some((buf.spec().channels().clone(), buf.spec().rate()));
            }
            _ => panic!("expected an s32 buffer"),
        }
    }

    let (channels, rate) = layout.expect("no audio decoded");
    (hash, frames, channels, rate)
}

#[test]
fn mlp_stereo_16bit_is_bit_exact() {
    let (hash, frames, channels, rate) = decode_all(include_bytes!("data/stereo16.mlp"));

    assert_eq!(frames, 1600);
    assert_eq!(rate, 48000);
    assert_eq!(channels, Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT));
    assert_eq!(hash, 0x160cd267c7a00aab);
}

#[test]
fn truehd_5_1_24bit_is_bit_exact() {
    let (hash, frames, channels, rate) = decode_all(include_bytes!("data/surround24.thd"));

    assert_eq!(frames, 1600);
    assert_eq!(rate, 48000);
    assert_eq!(
        channels,
        Channels::Positioned(
            Position::FRONT_LEFT
                | Position::FRONT_RIGHT
                | Position::FRONT_CENTER
                | Position::LFE1
                | Position::SIDE_LEFT
                | Position::SIDE_RIGHT
        )
    );
    assert_eq!(hash, 0x5a6a46307663012a);
}

#[test]
fn access_units_before_major_sync_are_skipped() {
    let data = include_bytes!("data/surround24.thd");
    let aus = access_units(data);

    // The second access unit has no major sync. Without prior state it must produce no samples.
    let mut dec = new_decoder();
    let buf = dec.decode(&packet(aus[1])).unwrap();
    assert_eq!(buf.frames(), 0);
}

#[test]
fn corrupt_packets_do_not_panic() {
    let data = include_bytes!("data/surround24.thd");
    let aus = access_units(data);

    // Truncate every access unit at every length, and flip every bit of the first access unit
    // (which carries the major sync and restart headers).
    let mut dec = new_decoder();

    for au in &aus[..3] {
        for len in 0..au.len() {
            let _ = dec.decode(&packet(&au[..len]));
        }
    }

    for bit in 0..aus[0].len() * 8 {
        let mut au = aus[0].to_vec();
        au[bit / 8] ^= 1 << (bit % 8);
        let _ = dec.decode(&packet(&au));
        let _ = dec.decode(&packet(aus[1]));
    }
}

#[test]
fn rejects_other_codecs() {
    use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;

    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_AAC);
    assert!(TrueHdDecoder::try_new(&params, &AudioDecoderOptions::default()).is_err());
}
