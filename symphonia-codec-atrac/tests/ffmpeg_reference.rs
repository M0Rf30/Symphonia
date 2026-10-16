// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Decodes real ATRAC1 (Matroska) and ATRAC3 (WAV) files through the demuxers and compares the
//! PCM with the FFmpeg decoders. The committed `*.ref.s16` files are the output of
//! `ffmpeg -i <fixture> -f s16le`; when `ffmpeg` is on `PATH` the float output is compared too.
//!
//! Fixtures: 0.5 s clips cut (`-c copy`) from `Christmas1970.aea` (ATRAC1 stereo) and from
//! `mc_sich_at3_{066,105,132}.wav` (ATRAC3 joint stereo at 66 kbps, single channel units at 105
//! and 132 kbps), https://samples.ffmpeg.org/A-codecs/.

use std::path::PathBuf;
use std::process::Command;

use symphonia_codec_atrac::AtracDecoder;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_ATRAC1, CODEC_ID_ATRAC3};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};
use symphonia_format_mkv::MkvReader;
use symphonia_format_riff::WavReader;
use symphonia_testutil::{XorShift64, open_mss};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)
}

/// Opens `name` and returns its codec parameters and packets.
fn demux(name: &str) -> (AudioCodecParameters, Vec<Vec<u8>>) {
    let mss = open_mss(&fixture(name));
    let mut reader: Box<dyn FormatReader> = if name.ends_with(".mkv") {
        Box::new(MkvReader::try_new(mss, FormatOptions::default()).unwrap())
    }
    else {
        Box::new(WavReader::try_new(mss, FormatOptions::default()).unwrap())
    };
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
    let mut dec = AtracDecoder::try_new(params, &AudioDecoderOptions::default()).unwrap();
    let (mut out, mut tmp) = (Vec::new(), Vec::new());
    for data in packets {
        let pkt = PacketRef::new(0, Timestamp::new(0), Duration::new(0), data);
        dec.decode_ref(&pkt).unwrap().copy_to_vec_interleaved::<f32>(&mut tmp);
        out.extend_from_slice(&tmp);
    }
    out
}

fn decode(name: &str) -> Vec<f32> {
    let (params, packets) = demux(name);
    decode_packets(&params, &packets)
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

/// Returns the SNR in dB of `got` against `want` and the maximum absolute difference.
fn compare(got: &[f32], want: &[f32]) -> (f64, f32) {
    assert_eq!(got.len(), want.len());
    let (mut sig, mut err, mut max) = (0.0f64, 0.0f64, 0.0f32);
    for (&g, &w) in got.iter().zip(want) {
        sig += f64::from(w) * f64::from(w);
        err += f64::from(g - w) * f64::from(g - w);
        max = max.max((g - w).abs());
    }
    (10.0 * (sig / err.max(1.0e-30)).log10(), max)
}

fn check(name: &str, codec: symphonia_core::codecs::audio::AudioCodecId, samples: usize) {
    let (params, _) = demux(name);
    assert_eq!(params.codec, codec);
    assert_eq!(params.channels.as_ref().unwrap().count(), 2);

    let out = decode(name);
    assert_eq!(out.len(), samples);

    // Against the committed reference (16-bit output of FFmpeg).
    let reference =
        std::fs::read(fixture(&name.replace(".wav", ".ref.s16").replace(".mkv", ".ref.s16")))
            .unwrap();
    let want: Vec<i16> =
        reference.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
    assert_eq!(want.len(), out.len());
    let max =
        out.iter().zip(&want).map(|(&g, &w)| (i32::from(to_s16(g)) - i32::from(w)).abs()).max();
    // Single precision transforms with a different butterfly order: allow 1 LSB.
    assert!(max.unwrap() <= 1, "{name}: max deviation {} LSB", max.unwrap());

    // Against the live FFmpeg float output.
    if let Some(want) = ffmpeg_f32(name) {
        let (snr, max) = compare(&out, &want);
        println!("{name}: {} samples, SNR {snr:.1} dB, max |diff| {max:.3e}", out.len());
        assert!(snr > 100.0, "{name}: SNR {snr}");
        assert!(max < 1.0e-5, "{name}: max {max}");
    }
}

#[test]
fn atrac1_stereo_matches_ffmpeg() {
    check("atrac1_stereo.mkv", CODEC_ID_ATRAC1, 22528 * 2);
}

#[test]
fn atrac3_joint_stereo_matches_ffmpeg() {
    check("atrac3_js_66k.wav", CODEC_ID_ATRAC3, 22528 * 2);
}

#[test]
fn atrac3_single_channel_units_105k_matches_ffmpeg() {
    check("atrac3_105k.wav", CODEC_ID_ATRAC3, 22528 * 2);
}

#[test]
fn atrac3_single_channel_units_132k_matches_ffmpeg() {
    check("atrac3_132k.wav", CODEC_ID_ATRAC3, 22528 * 2);
}

#[test]
fn atrac1_channels_are_independent() {
    // A mono stream made of the first unit of every frame decodes to the first channel.
    let (params, packets) = demux("atrac1_stereo.mkv");
    let stereo = decode_packets(&params, &packets);

    let mono_packets: Vec<Vec<u8>> = packets.iter().map(|p| p[..212].to_vec()).collect();
    let mut mono_params = params.clone();
    mono_params.with_channels(symphonia_core::audio::layouts::CHANNEL_LAYOUT_MONO);
    let mono = decode_packets(&mono_params, &mono_packets);

    assert_eq!(mono.len() * 2, stereo.len());
    assert!(mono.iter().zip(stereo.chunks_exact(2)).all(|(&m, s)| m == s[0]));
}

#[test]
fn atrac1_multiple_frames_per_packet() {
    let (params, packets) = demux("atrac1_stereo.mkv");
    let single = decode_packets(&params, &packets);
    let joined = decode_packets(&params, &[packets.concat()]);
    assert_eq!(single, joined);
}

#[test]
fn atrac3_extensible_header_is_equivalent() {
    // Rewrite the `fmt ` chunk of a plain WAV file as WAVE_FORMAT_EXTENSIBLE with the ATRAC3
    // sub-format: base GUID with tag 0x0270 and the 14-byte extension after the 22 bytes.
    let plain = std::fs::read(fixture("atrac3_js_66k.wav")).unwrap();
    assert_eq!(&plain[12..16], b"fmt ");
    let fmt_len = u32::from_le_bytes(plain[16..20].try_into().unwrap()) as usize;
    assert_eq!(fmt_len, 32);
    let fmt = &plain[20..20 + fmt_len];
    let ext = &fmt[18..];
    assert_eq!(ext.len(), 14);

    let mut new_fmt = Vec::new();
    new_fmt.extend_from_slice(&0xfffe_u16.to_le_bytes());
    new_fmt.extend_from_slice(&fmt[2..16]);
    new_fmt.extend_from_slice(&(22u16 + 14).to_le_bytes());
    new_fmt.extend_from_slice(&0u16.to_le_bytes());
    new_fmt.extend_from_slice(&3u32.to_le_bytes());
    new_fmt.extend_from_slice(&[0x70, 0x02, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00]);
    new_fmt.extend_from_slice(&[0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71]);
    new_fmt.extend_from_slice(ext);

    let mut body = Vec::new();
    body.extend_from_slice(b"WAVEfmt ");
    body.extend_from_slice(&(new_fmt.len() as u32).to_le_bytes());
    body.extend_from_slice(&new_fmt);
    body.extend_from_slice(&plain[20 + fmt_len..]);
    let mut file = b"RIFF".to_vec();
    file.extend_from_slice(&(body.len() as u32).to_le_bytes());
    file.extend_from_slice(&body);

    let cursor = std::io::Cursor::new(file);
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());
    let mut reader = WavReader::try_new(mss, FormatOptions::default()).unwrap();
    let track = reader.default_track(TrackType::Audio).unwrap();
    let Some(CodecParameters::Audio(params)) = &track.codec_params
    else {
        panic!("no audio")
    };
    assert_eq!(params.codec, CODEC_ID_ATRAC3);
    let params = params.clone();
    let mut packets = Vec::new();
    while let Some(pkt) = reader.next_packet().unwrap() {
        packets.push(pkt.data.to_vec());
    }
    assert_eq!(decode_packets(&params, &packets), decode("atrac3_js_66k.wav"));
}

#[test]
fn atrac3_real_media_extradata_decodes_scrambled_frames() {
    // RealMedia layout: be32 version 4, be16 samples per frame (1024 * channels), be16 delay,
    // be16 coding mode (0x12 joint stereo); frames XORed with the key 53 7f 61 03.
    let (params, packets) = demux("atrac3_js_66k.wav");
    let want = decode_packets(&params, &packets);

    let extra: [u8; 10] = [0, 0, 0, 4, 0x08, 0x00, 0x08, 0x8e, 0x00, 0x12];
    let mut rm_params = params.clone();
    rm_params.with_extra_data(extra.to_vec().into_boxed_slice());

    let key = [0x53u8, 0x7f, 0x61, 0x03];
    let scrambled: Vec<Vec<u8>> = packets
        .iter()
        .map(|p| p.iter().enumerate().map(|(i, b)| b ^ key[i & 3]).collect())
        .collect();
    assert_eq!(decode_packets(&rm_params, &scrambled), want);
}

#[test]
fn invalid_parameters_are_rejected() {
    let (params, _) = demux("atrac3_js_66k.wav");
    let opts = AudioDecoderOptions::default();

    let mut p = params.clone();
    p.extra_data = None;
    assert!(AtracDecoder::try_new(&p, &opts).is_err());

    let mut p = params.clone();
    p.sample_rate = None;
    assert!(AtracDecoder::try_new(&p, &opts).is_err());

    let mut p = params.clone();
    p.with_extra_data(vec![0u8; 5].into_boxed_slice());
    assert!(AtracDecoder::try_new(&p, &opts).is_err());

    // A block align that doesn't match the coding mode and frame factor.
    let mut dec = AtracDecoder::try_new(&params, &opts).unwrap();
    let data = [0u8; 100];
    let pkt = PacketRef::new(0, Timestamp::new(0), Duration::new(0), &data);
    assert!(dec.decode_ref(&pkt).is_err());
    let pkt = PacketRef::new(0, Timestamp::new(0), Duration::new(0), &[]);
    assert!(dec.decode_ref(&pkt).is_err());
}

#[test]
fn corrupted_packets_do_not_panic() {
    let mut rng = XorShift64(0x1234_5678_9abc_def0);
    for name in ["atrac1_stereo.mkv", "atrac3_js_66k.wav", "atrac3_105k.wav", "atrac3_132k.wav"] {
        let (params, packets) = demux(name);
        let mut dec = AtracDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
        for _ in 0..4 {
            for data in &packets {
                let mut data = data.clone();
                for _ in 0..8 {
                    let i = rng.below(data.len());
                    data[i] = rng.next_u64() as u8;
                }
                let pkt = PacketRef::new(0, Timestamp::new(0), Duration::new(0), &data);
                // Errors are fine, panics are not.
                let _ = dec.decode_ref(&pkt);
            }
        }
    }
}
