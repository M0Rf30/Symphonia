// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Decodes real ATRAC3+ frames (WAV files with the ATRAC3+ GUID) through the WAV demuxer and
//! compares the PCM with the FFmpeg decoder. The committed `*.ref.s16` files are the output of
//! `ffmpeg -i <fixture> -f s16le`; when `ffmpeg` is on `PATH` the float output is compared too.
//!
//! Fixtures: 8 frames (2048 samples per channel) cut (`-c copy`) from the ATRAC3+ files at
//! https://samples.ffmpeg.org/A-codecs/ATRAC3+/ (`at3p_sample1.oma`, 256 kbps, from the start
//! and from frame 20; `0111019293.aa3`, 128 kbps; `sonateno14op27-2.aa3`, 48 kbps) and wrapped
//! in a WAVE_FORMAT_EXTENSIBLE container.

use std::path::PathBuf;
use std::process::Command;

use symphonia_codec_atrac3plus::Atrac3PlusDecoder;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_ATRAC3PLUS;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};
use symphonia_format_riff::WavReader;
use symphonia_testutil::{XorShift64, open_mss};

const FIXTURES: [&str; 4] =
    ["atrac3p_256k_start.wav", "atrac3p_256k_mid.wav", "atrac3p_128k.wav", "atrac3p_48k.wav"];

const FRAMES: usize = 8;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)
}

/// Opens `name` and returns its codec parameters and packets.
fn demux(name: &str) -> (AudioCodecParameters, Vec<Vec<u8>>) {
    let mss = open_mss(&fixture(name));
    let mut reader = WavReader::try_new(mss, FormatOptions::default()).unwrap();
    let track = reader.default_track(TrackType::Audio).unwrap();
    let Some(CodecParameters::Audio(params)) = &track.codec_params
    else {
        panic!("no audio")
    };
    let params = params.clone();
    let mut packets = Vec::new();
    while let Some(pkt) = reader.next_packet().unwrap() {
        packets.push(pkt.data.to_vec());
    }
    (params, packets)
}

fn decode_packets(params: &AudioCodecParameters, packets: &[Vec<u8>]) -> Vec<f32> {
    let mut dec = Atrac3PlusDecoder::try_new(params, &AudioDecoderOptions::default()).unwrap();
    let (mut out, mut tmp) = (Vec::new(), Vec::new());
    for data in packets {
        let pkt = PacketRef::new(0, Timestamp::new(0), Duration::new(0), data);
        dec.decode_ref(&pkt).unwrap().copy_to_vec_interleaved::<f32>(&mut tmp);
        out.extend_from_slice(&tmp);
    }
    out
}

fn to_s16(x: f32) -> i16 {
    (x * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

fn ffmpeg_f32(name: &str) -> Option<Vec<f32>> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(fixture(name))
        .args(["-f", "f32le", "-acodec", "pcm_f32le", "-"])
        .output()
        .ok()?;
    out.status.success().then(|| {
        out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect()
    })
}

#[test]
fn atrac3plus_matches_ffmpeg() {
    for name in FIXTURES {
        let (params, packets) = demux(name);
        assert_eq!(params.codec, CODEC_ID_ATRAC3PLUS);
        assert_eq!(params.channels.as_ref().unwrap().count(), 2);
        assert_eq!(packets.len(), FRAMES);

        let out = decode_packets(&params, &packets);
        assert_eq!(out.len(), FRAMES * 2048 * 2);

        // Against the committed reference (16-bit output of FFmpeg).
        let reference = std::fs::read(fixture(&name.replace(".wav", ".ref.s16"))).unwrap();
        let want: Vec<i16> =
            reference.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        assert_eq!(want.len(), out.len());
        let max = out
            .iter()
            .zip(&want)
            .map(|(&g, &w)| (i32::from(to_s16(g)) - i32::from(w)).abs())
            .max()
            .unwrap();
        // Single precision transforms with a different butterfly order: allow 1 LSB.
        assert!(max <= 1, "{name}: max deviation {max} LSB");

        // Against the live FFmpeg float output.
        if let Some(want) = ffmpeg_f32(name) {
            assert_eq!(want.len(), out.len());
            let (mut sig, mut err, mut peak) = (0.0f64, 0.0f64, 0.0f32);
            for (&g, &w) in out.iter().zip(&want) {
                sig += f64::from(w) * f64::from(w);
                err += f64::from(g - w) * f64::from(g - w);
                peak = peak.max((g - w).abs());
            }
            let snr = 10.0 * (sig / err.max(1.0e-30)).log10();
            println!("{name}: SNR {snr:.1} dB, max |diff| {peak:.3e}");
            assert!(snr > 100.0, "{name}: SNR {snr}");
            assert!(peak < 1.0e-5, "{name}: max {peak}");
        }
    }
}

#[test]
fn invalid_parameters_are_rejected() {
    let (params, packets) = demux("atrac3p_256k_start.wav");
    let opts = AudioDecoderOptions::default();

    let mut p = params.clone();
    p.sample_rate = None;
    assert!(Atrac3PlusDecoder::try_new(&p, &opts).is_err());

    let mut p = params.clone();
    p.channels = None;
    assert!(Atrac3PlusDecoder::try_new(&p, &opts).is_err());

    let mut p = params.clone();
    p.with_channels(symphonia_core::audio::layouts::CHANNEL_LAYOUT_5P0);
    assert!(Atrac3PlusDecoder::try_new(&p, &opts).is_err());

    // A start bit that is set, and an empty packet.
    let mut dec = Atrac3PlusDecoder::try_new(&params, &opts).unwrap();
    let mut data = packets[0].clone();
    data[0] |= 0x80;
    let pkt = PacketRef::new(0, Timestamp::new(0), Duration::new(0), &data);
    assert!(dec.decode_ref(&pkt).is_err());
    let pkt = PacketRef::new(0, Timestamp::new(0), Duration::new(0), &[]);
    let _ = dec.decode_ref(&pkt);
}

#[test]
fn corrupted_packets_do_not_panic() {
    let mut rng = XorShift64(0x1234_5678_9abc_def0);
    for name in FIXTURES {
        let (params, packets) = demux(name);
        let mut dec = Atrac3PlusDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
        for _ in 0..8 {
            for data in &packets {
                let mut data = data.clone();
                for _ in 0..8 {
                    let i = rng.below(data.len());
                    data[i] = rng.next_u64() as u8;
                }
                // Truncate some of the packets.
                if rng.below(4) == 0 {
                    data.truncate(rng.below(data.len()));
                }
                let pkt = PacketRef::new(0, Timestamp::new(0), Duration::new(0), &data);
                // Errors are fine, panics are not.
                let _ = dec.decode_ref(&pkt);
            }
        }
    }
}
