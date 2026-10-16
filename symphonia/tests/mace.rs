#![allow(clippy::unwrap_used)]
// Test code: a panic is a test failure.
// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Decodes real MACE 3:1 and 6:1 files (AIFF-C, QuickTime, CAF) through the facade and compares
//! the output with FFmpeg's `mace3`/`mace6` decoders. MACE is integer-only, so the output must be
//! bit-exact.
//!
//! The reference PCM is pinned as an FNV-1a hash of the interleaved signed 16-bit samples (which
//! was verified equal to the `ffmpeg` output when generated). If `ffmpeg` is on `PATH`, the
//! output is also compared sample by sample with it.
//!
//! Fixture provenance: `bach_*` are `Bach3-1.aiff` and `Bach6-1.aiff`, `surge_*` are
//! `surge-1-8-MAC3.mov` and `surge-1-8-MAC6.mov` (OpenQT), all from
//! <https://samples.ffmpeg.org/A-codecs/MACE/>. `stereo_*.mov` are 3 s audio-only stream copies
//! of `mac3audio.mov` (22.05 kHz) and `cristinreel.mov` (44.1 kHz) from the same site. The `.caf`
//! files were stream-copied by `ffmpeg` from the AIFF-C and stereo QuickTime fixtures.

#![cfg(all(feature = "mace", feature = "aiff", feature = "isomp4", feature = "caf"))]

use std::fs::File;
use std::path::PathBuf;
use std::process::Command;

use symphonia::core::audio::{Audio, GenericAudioBufferRef};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::codecs::audio::well_known::{CODEC_ID_MACE3, CODEC_ID_MACE6};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia_testutil::Fnv;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../symphonia-codec-mace/tests/fixtures")
        .join(name)
}

struct Decoded {
    codec_is_mace3: bool,
    channels: usize,
    rate: u32,
    /// Interleaved samples.
    pcm: Vec<i16>,
}

fn decode(name: &str) -> Decoded {
    let path = fixture(name);
    let file = File::open(&path).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    hint.with_extension(path.extension().unwrap().to_str().unwrap());

    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .unwrap();

    let track = format.default_track(TrackType::Audio).unwrap();
    let track_id = track.id;
    let Some(CodecParameters::Audio(params)) = track.codec_params.clone()
    else {
        panic!("not an audio track");
    };
    assert!(params.codec == CODEC_ID_MACE3 || params.codec == CODEC_ID_MACE6);

    let mut decoder = symphonia::default::get_codecs()
        // The raw decode is compared with FFmpeg's: the edit list of the stream-copied fixtures is
        // not exact, and gapless trimming would cut audio that FFmpeg outputs.
        .make_audio_decoder(&params, &AudioDecoderOptions::default().gapless(false))
        .unwrap();

    let mut channels = 0;
    let mut pcm = vec![];

    while let Some(packet) = format.next_packet().unwrap() {
        if packet.track_id != track_id {
            continue;
        }

        if std::env::var("MACE_DBG").is_ok_and(|v| name.contains(&v)) {
            eprintln!(
                "pkt pts={:?} dur={:?} trim={:?}/{:?} len={}",
                packet.pts,
                packet.dur,
                packet.trim_start,
                packet.trim_end,
                packet.data.len()
            );
        }
        let GenericAudioBufferRef::S16(buf) = decoder.decode(&packet).unwrap()
        else {
            panic!("expected s16 output");
        };

        channels = buf.spec().channels().count();
        let frames = buf.frames();
        let planes: Vec<&[i16]> = (0..channels).map(|c| buf.plane(c).unwrap()).collect();
        for f in 0..frames {
            for plane in &planes {
                pcm.push(plane[f]);
            }
        }
    }

    Decoded {
        codec_is_mace3: params.codec == CODEC_ID_MACE3,
        channels,
        rate: params.sample_rate.unwrap(),
        pcm,
    }
}

fn hash(pcm: &[i16]) -> u64 {
    let mut fnv = Fnv::new();
    for s in pcm {
        fnv.write(&s.to_le_bytes());
    }
    fnv.finish()
}

fn ffmpeg_reference(name: &str, channels: usize) -> Option<Vec<i16>> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(fixture(name))
        .args(["-map", "0:a:0", "-f", "s16le", "-ac", &channels.to_string(), "-"])
        .output()
        .ok()?;

    if !out.status.success() {
        return None;
    }

    Some(out.stdout.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect())
}

/// (file, MACE 3:1, channels, sample rate, frames, FNV-1a of the PCM)
static FIXTURES: [(&str, bool, usize, u32, usize, u64); 8] = [
    ("bach_mace3_mono.aif", true, 1, 22255, 257022, 0x8d7326d9f9e10a7f),
    ("bach_mace6_mono.aif", false, 1, 22255, 257028, 0x3b99385bf28eb50b),
    ("bach_mace6_mono.caf", false, 1, 22255, 257028, 0x3b99385bf28eb50b),
    ("surge_mace3_mono.mov", true, 1, 44100, 578298, 0xbd6be2597f4b7d13),
    ("surge_mace6_mono.mov", false, 1, 44100, 578298, 0x3d6368026378bfa9),
    ("stereo_mace3.mov", true, 2, 22050, 66150, 0xa6fad6a0607c65ed),
    ("stereo_mace3.caf", true, 2, 22050, 66150, 0xa6fad6a0607c65ed),
    ("stereo_mace6.mov", false, 2, 44100, 132300, 0xf8aa5ddd3e66bf25),
];

#[test]
fn verify_fixtures_match_reference() {
    let print = std::env::var_os("GOLDEN_PRINT").is_some();

    for (name, mace3, channels, rate, frames, expected_hash) in FIXTURES {
        let d = decode(name);
        let h = hash(&d.pcm);

        if print {
            println!("{name}: frames={} hash={h:#018x}", d.pcm.len() / d.channels);
        }
        else {
            assert_eq!(d.codec_is_mace3, mace3, "{name}");
            assert_eq!(d.channels, channels, "{name}");
            assert_eq!(d.rate, rate, "{name}");
            assert_eq!(d.pcm.len() / d.channels, frames, "{name}: frame count");
            assert_eq!(h, expected_hash, "{name}: pcm hash");
        }

        if let Some(reference) = ffmpeg_reference(name, d.channels) {
            // The decoded length must match, and the samples be identical.
            assert_eq!(d.pcm.len(), reference.len(), "{name}: length vs ffmpeg");
            let max_diff = d
                .pcm
                .iter()
                .zip(&reference)
                .map(|(a, b)| (i32::from(*a) - i32::from(*b)).abs())
                .max()
                .unwrap_or(0);
            assert_eq!(max_diff, 0, "{name}: differs from ffmpeg");
        }
    }
}

/// Truncated and garbage packets must be errors or silence, never a panic, and the decoder must
/// keep working afterwards.
#[test]
fn verify_malformed_input_does_not_panic() {
    use symphonia::core::audio::layouts::CHANNEL_LAYOUT_STEREO;
    use symphonia::core::codecs::audio::AudioCodecParameters;
    use symphonia::core::packet::Packet;

    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_MACE3).with_sample_rate(22050).with_channels(CHANNEL_LAYOUT_STEREO);
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .unwrap();

    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    for len in 0..64usize {
        let data: Vec<u8> = (0..len)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (state >> 56) as u8
            })
            .collect();
        let packet = Packet::new(0, Default::default(), Default::default(), data);
        match decoder.decode(&packet) {
            Ok(GenericAudioBufferRef::S16(b)) => assert_eq!(b.frames(), len / 4 * 6),
            Ok(_) => panic!("expected s16 output"),
            Err(_) => assert!(len < 4),
        }
    }
}
