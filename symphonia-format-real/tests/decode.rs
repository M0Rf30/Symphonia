// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Demuxes the RealMedia samples of `RMPD_SAMPLES/real` (skipped when absent), decodes them with
//! the RealAudio decoders and compares the PCM with the output of `ffmpeg` (skipped when
//! `ffmpeg` is not on `PATH`).

use std::process::Command;

use symphonia_codec_atrac::AtracDecoder;
use symphonia_codec_cook::CookDecoder;
use symphonia_codec_ra::{Ra144Decoder, Ra288Decoder};
use symphonia_codec_ralf::RalfDecoder;
use symphonia_codec_sipr::SiprDecoder;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_ATRAC3, CODEC_ID_COOK, CODEC_ID_RA10, CODEC_ID_RA20, CODEC_ID_RALF, CODEC_ID_SIPR,
};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia_format_real::RealReader;
use symphonia_testutil::{open_mss, sample_path};

fn make_decoder(params: &AudioCodecParameters) -> Box<dyn AudioDecoder> {
    let opts = AudioDecoderOptions::default().gapless(false);
    match params.codec {
        CODEC_ID_COOK => Box::new(CookDecoder::try_new(params, &opts).unwrap()),
        CODEC_ID_RA10 => Box::new(Ra144Decoder::try_new(params, &opts).unwrap()),
        CODEC_ID_RA20 => Box::new(Ra288Decoder::try_new(params, &opts).unwrap()),
        CODEC_ID_SIPR => Box::new(SiprDecoder::try_new(params, &opts).unwrap()),
        CODEC_ID_RALF => Box::new(RalfDecoder::try_new(params, &opts).unwrap()),
        CODEC_ID_ATRAC3 => Box::new(AtracDecoder::try_new(params, &opts).unwrap()),
        id => panic!("unexpected codec {id:?}"),
    }
}

/// Decodes the first audio track of `name`: returns the channel count and the f32 samples.
fn decode(name: &str) -> Option<(usize, Vec<f32>, std::path::PathBuf)> {
    let path = sample_path(format!("real/{name}"))?;
    let mut reader = RealReader::try_new(open_mss(&path), FormatOptions::default()).unwrap();
    let track = reader.default_track(TrackType::Audio).unwrap();
    let id = track.id;
    let Some(CodecParameters::Audio(params)) = track.codec_params.clone()
    else {
        panic!("not audio")
    };
    let channels = params.channels.as_ref().unwrap().count();
    let mut dec = make_decoder(&params);

    let (mut out, mut tmp) = (Vec::new(), Vec::new());
    while let Some(packet) = reader.next_packet().unwrap() {
        if packet.track_id != id {
            continue;
        }
        if let Ok(buf) = dec.decode(&packet) {
            buf.copy_to_vec_interleaved::<f32>(&mut tmp);
            out.extend_from_slice(&tmp);
        }
    }
    Some((channels, out, path))
}

fn ffmpeg_f32(path: &std::path::Path) -> Option<Vec<f32>> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-f", "f32le", "-acodec", "pcm_f32le", "-"])
        .output()
        .ok()?;
    out.status.success().then(|| {
        out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect()
    })
}

fn check(name: &str) {
    let Some((channels, got, path)) = decode(name)
    else {
        return;
    };
    let Some(want) = ffmpeg_f32(&path)
    else {
        eprintln!("ffmpeg unavailable; skipping");
        return;
    };

    assert!(channels > 0);
    assert_eq!(got.len(), want.len(), "{name}: sample count");

    let (mut sig, mut err) = (0.0f64, 0.0f64);
    for (&g, &w) in got.iter().zip(&want) {
        sig += f64::from(w) * f64::from(w);
        err += f64::from(g - w) * f64::from(g - w);
    }
    let snr = 10.0 * (sig / err.max(1.0e-30)).log10();
    println!("{name}: {} samples, SNR {snr:.1} dB", got.len());
    assert!(snr > 80.0, "{name}: SNR {snr}");
}

#[test]
fn ra144() {
    check("ra3_in_rm_file.rm");
}

#[test]
fn ra288() {
    check("A0000040.rm");
    check("luckynightmono2.ra");
}

#[test]
fn cook() {
    check("FUN_RM_32.rm");
    check("mc_sich_ra8_20.rm");
    check("multichannel.rmvb");
}

#[test]
fn sipr() {
    check("autahi-vox.rm");
    check("bastard.rm");
}

#[test]
fn ralf() {
    check("Wonderful_RA10_Lossless_706K_30s.rm");
}

#[test]
fn atrac3() {
    check("mc_sich_ra8_105.rm");
}
