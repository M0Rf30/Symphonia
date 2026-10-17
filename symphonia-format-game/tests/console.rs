// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! End-to-end tests of the Nintendo (ADP/DTK, AFC, AST) and Sony (VAG family) readers.
//!
//! The decoders are integer decoders. Synthetic streams of random, valid frames are built here and
//! the decoded samples are compared bit for bit with FFmpeg's demuxer of the same format. The
//! tests are skipped when `ffmpeg` is not on the `PATH`.

mod common;

use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_format_game::{AdpReader, AfcReader, AstReader, VagReader};
use symphonia_testutil::XorShift64;

use common::{decode, ffmpeg_decode, mss, to_s16le};

fn random_bytes(rng: &mut XorShift64, n: usize) -> Vec<u8> {
    (0..n).map(|_| (rng.next_u64() >> 24) as u8).collect()
}

/// Random PlayStation ADPCM frames: a valid predictor and shift, a flag of 0, random nibbles.
fn psx_frames(rng: &mut XorShift64, frames: usize) -> Vec<u8> {
    let mut v = Vec::new();
    for _ in 0..frames {
        let mut f = random_bytes(rng, 16);
        f[0] = (((rng.next_u64() >> 20) % 5) as u8) << 4 | ((rng.next_u64() >> 20) % 13) as u8;
        f[1] = 0;
        v.extend_from_slice(&f);
    }
    v
}

fn check(name: &str, data: &[u8], format: &str, mut reader: Box<dyn FormatReader + '_>) {
    let got = decode(reader.as_mut(), false);
    let Some(want) = ffmpeg_decode(data, format, "s16le")
    else {
        eprintln!("{name}: ffmpeg not available, skipping");
        return;
    };
    let got = to_s16le(&got.samples);
    assert!(!got.is_empty(), "{name}: no samples");
    assert_eq!(got.len(), want.len(), "{name}: length");
    assert!(got == want, "{name}: samples differ from FFmpeg");
}

#[test]
fn adp_matches_ffmpeg() {
    let mut rng = XorShift64(7);
    let mut data = Vec::new();
    for _ in 0..200 {
        let mut b = random_bytes(&mut rng, 32);
        // The header bytes are repeated, with valid shifts (the high nibble of the range).
        b[0] &= 0x0f;
        b[1] &= 0x0f;
        b[2] = b[0];
        b[3] = b[1];
        data.extend_from_slice(&b);
    }
    let reader = AdpReader::try_new(mss(&data), FormatOptions::default()).unwrap();
    check("adp", &data, "adp", Box::new(reader));
}

#[test]
fn afc_matches_ffmpeg() {
    let mut rng = XorShift64(11);
    let groups = 300usize;
    let mut data = vec![0u8; 32];
    data[0..4].copy_from_slice(&((groups * 18) as u32).to_be_bytes());
    data[4..8].copy_from_slice(&((groups * 16) as u32).to_be_bytes());
    data[8..10].copy_from_slice(&32000u16.to_be_bytes());
    data.extend_from_slice(&random_bytes(&mut rng, groups * 18));
    let reader = AfcReader::try_new(mss(&data), FormatOptions::default()).unwrap();
    check("afc", &data, "afc", Box::new(reader));
}

#[test]
fn ast_matches_ffmpeg() {
    let mut rng = XorShift64(13);
    let (channels, per_channel, blocks) = (2usize, 9 * 40, 3usize);
    let mut data = vec![0u8; 64];
    data[0..4].copy_from_slice(b"STRM");
    data[8..10].copy_from_slice(&0u16.to_be_bytes());
    data[10..12].copy_from_slice(&16u16.to_be_bytes());
    data[12..14].copy_from_slice(&(channels as u16).to_be_bytes());
    data[16..20].copy_from_slice(&32000u32.to_be_bytes());
    data[20..24].copy_from_slice(&((blocks * 40 * 16) as u32).to_be_bytes());
    data[32..36].copy_from_slice(&(per_channel as u32).to_be_bytes());
    for _ in 0..blocks {
        let mut h = vec![0u8; 32];
        h[0..4].copy_from_slice(b"BLCK");
        h[4..8].copy_from_slice(&(per_channel as u32).to_be_bytes());
        data.extend_from_slice(&h);
        data.extend_from_slice(&random_bytes(&mut rng, per_channel * channels));
    }
    let reader = AstReader::try_new(mss(&data), FormatOptions::default()).unwrap();
    check("ast", &data, "ast", Box::new(reader));
}

#[test]
fn vag_matches_ffmpeg() {
    let mut rng = XorShift64(17);
    let frames = 150usize;
    let mut data = vec![0u8; 0x30];
    data[0..4].copy_from_slice(b"VAGp");
    data[4..8].copy_from_slice(&3u32.to_be_bytes());
    data[12..16].copy_from_slice(&((frames * 16) as u32).to_be_bytes());
    data[16..20].copy_from_slice(&22050u32.to_be_bytes());
    data.extend_from_slice(&psx_frames(&mut rng, frames));
    let reader = VagReader::try_new(mss(&data), FormatOptions::default()).unwrap();
    check("vag", &data, "vag", Box::new(reader));
}

#[test]
fn svag_matches_ffmpeg() {
    let mut rng = XorShift64(19);
    let (frames, align) = (64usize, 16u32);
    let mut data = vec![0u8; 0x800];
    data[0..4].copy_from_slice(b"Svag");
    data[4..8].copy_from_slice(&((frames * 16 * 2) as u32).to_le_bytes());
    data[8..12].copy_from_slice(&44100u32.to_le_bytes());
    data[12..16].copy_from_slice(&2u32.to_le_bytes());
    data[16..20].copy_from_slice(&align.to_le_bytes());
    data.extend_from_slice(&psx_frames(&mut rng, frames * 2));
    let reader = VagReader::try_new(mss(&data), FormatOptions::default()).unwrap();
    check("svag", &data, "svag", Box::new(reader));
}

#[test]
fn msf_psx_matches_ffmpeg() {
    let mut rng = XorShift64(23);
    let frames = 100usize;
    let mut data = vec![0u8; 0x40];
    data[0..4].copy_from_slice(b"MSF\x01");
    data[4..8].copy_from_slice(&3u32.to_be_bytes());
    data[8..12].copy_from_slice(&1u32.to_be_bytes());
    data[12..16].copy_from_slice(&((frames * 16) as u32).to_be_bytes());
    data[16..20].copy_from_slice(&44100u32.to_be_bytes());
    data.extend_from_slice(&psx_frames(&mut rng, frames));
    let reader = VagReader::try_new(mss(&data), FormatOptions::default()).unwrap();
    check("msf", &data, "msf", Box::new(reader));
}
