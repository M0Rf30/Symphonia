// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Decodes real ATRAC9 superframes (`.at9` RIFF WAVE files) through the WAV demuxer and compares
//! the PCM with the FFmpeg decoder. The committed `*.ref.s16` files are the output of
//! `ffmpeg -i <fixture> -f s16le`; when `ffmpeg` is on `PATH` the float output is compared too.
//!
//! The stereo fixture is 64 superframes cut from `sample.at9` (48 kHz, 144 kbps, four-frame
//! superframes with band extension), the stream used by FFmpeg's `fate-atrac9-1` test. The
//! dual-mono fixture is the same data with the channel configuration relabelled to two single
//! channel blocks: it exercises the single-channel block path (the FFmpeg WAV demuxer always
//! reports two channels for this data, so both sides are compared as stereo).

use std::path::PathBuf;
use std::process::Command;

use symphonia_codec_atrac9::Atrac9Decoder;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_ATRAC9;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};
use symphonia_format_riff::WavReader;
use symphonia_testutil::open_mss;

const FIXTURES: [(&str, usize); 2] = [("atrac9_stereo_48k.wav", 2), ("atrac9_dualmono_48k.wav", 2)];

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)
}

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
    let mut dec = Atrac9Decoder::try_new(params, &AudioDecoderOptions::default()).unwrap();
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
fn atrac9_matches_ffmpeg() {
    for &(name, channels) in &FIXTURES {
        let (params, packets) = demux(name);
        assert_eq!(params.codec, CODEC_ID_ATRAC9);

        let out = decode_packets(&params, &packets);
        assert_eq!(out.len(), 64 * 4 * 256 * channels);

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
    let (params, _) = demux("atrac9_stereo_48k.wav");
    let opts = AudioDecoderOptions::default();

    let mut p = params.clone();
    p.extra_data = None;
    assert!(Atrac9Decoder::try_new(&p, &opts).is_err());

    let mut p = params.clone();
    p.codec = symphonia_core::codecs::audio::well_known::CODEC_ID_PCM_F32LE;
    assert!(Atrac9Decoder::try_new(&p, &opts).is_err());

    let mut p = params.clone();
    let bad = vec![1u8, 0, 0, 0, 0xff, 0, 0, 0, 0, 0, 0, 0];
    p.extra_data = Some(bad.into());
    assert!(Atrac9Decoder::try_new(&p, &opts).is_err());
}

#[test]
fn corrupted_packets_do_not_panic() {
    let (params, packets) = demux("atrac9_stereo_48k.wav");
    let mut dec = Atrac9Decoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
    // A deterministic perturbation.
    let mut x = 0x1234_5678_9abc_def0u64;
    for data in &packets {
        let mut data = data.clone();
        for i in (0..data.len()).step_by(37) {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
            data[i] ^= (x >> 32) as u8;
        }
        let pkt = PacketRef::new(0, Timestamp::new(0), Duration::new(0), &data);
        let _ = dec.decode_ref(&pkt);
    }
}
