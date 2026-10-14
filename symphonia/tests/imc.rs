#![allow(clippy::unwrap_used)]
// Test code: a panic is a test failure.
// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Decodes IMC (`WAVE_FORMAT` 0x0401) and IAC (0x0402) WAV files through the facade, so that the
//! WAV mapping, the packetization and the decoder are exercised together, and compares the output
//! with the FFmpeg `imc`/`iac` decoders (`ffmpeg -i x.wav -f s16le`, stored next to the fixtures
//! as `*.ref.s16`). The decoders are float, so the comparison allows a rounding difference of one
//! 16-bit LSB. See `symphonia-codec-imc/tests/real_data.rs` for the fixture provenance.

#![cfg(all(feature = "imc", feature = "wav"))]

use std::fs::File;
use std::path::PathBuf;

use symphonia::core::audio::{Audio, GenericAudioBufferRef};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::codecs::audio::well_known::{CODEC_ID_IAC, CODEC_ID_IMC};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../symphonia-codec-imc/tests/fixtures")
        .join(name)
}

/// (file, codec is IAC, channels, sample rate)
const FIXTURES: [(&str, bool, usize, u32); 5] = [
    ("imc_8000_mono.wav", false, 1, 8000),
    ("imc_11025_mono.wav", false, 1, 11025),
    ("imc_48000_mono.wav", false, 1, 48000),
    ("iac_22050_stereo.wav", true, 2, 22050),
    ("iac_22050_stereo_b.wav", true, 2, 22050),
];

fn decode(name: &str) -> (CodecParameters, Vec<f32>) {
    let file = File::open(fixture(name)).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    hint.with_extension("wav");

    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .unwrap();

    let track = format.default_track(TrackType::Audio).unwrap();
    let track_id = track.id;
    let codec_params = track.codec_params.clone().unwrap();
    let CodecParameters::Audio(params) = &codec_params
    else {
        panic!("not an audio track");
    };

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .unwrap();

    let mut pcm = vec![];
    while let Some(packet) = format.next_packet().unwrap() {
        if packet.track_id != track_id {
            continue;
        }

        let GenericAudioBufferRef::F32(buf) = decoder.decode(&packet).unwrap()
        else {
            panic!("expected f32 output");
        };

        let planes: Vec<&[f32]> =
            (0..buf.spec().channels().count()).map(|c| buf.plane(c).unwrap()).collect();
        for f in 0..buf.frames() {
            for plane in &planes {
                pcm.push(plane[f]);
            }
        }
    }

    (codec_params, pcm)
}

#[test]
fn wav_imc_iac_match_ffmpeg() {
    for (name, iac, channels, rate) in FIXTURES {
        let (params, pcm) = decode(name);
        let CodecParameters::Audio(params) = params
        else {
            unreachable!()
        };
        assert_eq!(params.codec, if iac { CODEC_ID_IAC } else { CODEC_ID_IMC }, "{name}");
        assert_eq!(params.channels.as_ref().unwrap().count(), channels, "{name}");
        assert_eq!(params.sample_rate, Some(rate), "{name}");

        let reference: Vec<i16> = std::fs::read(fixture(&name.replace(".wav", ".ref.s16")))
            .unwrap()
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();

        assert_eq!(pcm.len(), reference.len(), "{name}: sample count");

        for (i, (&a, &r)) in pcm.iter().zip(reference.iter()).enumerate() {
            let a = (f64::from(a) * 32768.0).round().clamp(-32768.0, 32767.0);
            assert!((a - f64::from(r)).abs() <= 1.0, "{name}: sample {i}: {a} vs {r}");
        }
    }
}
