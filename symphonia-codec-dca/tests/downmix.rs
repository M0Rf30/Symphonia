// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Downmix of a 48 kHz 5.1 core stream that carries embedded stereo downmix coefficients (FFmpeg
//! FATE sample `dcadec-suite/core_51_24_48_768_1`, core substream only). The references are the
//! output of FFmpeg with `-downmix stereo` and without a downmix, as interleaved f32 samples.

use symphonia_codec_dca::DcaDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::well_known::CODEC_ID_DCA;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, Downmix,
};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

const STREAM: &[u8] = include_bytes!("data/core_51_dmix.dts");
const STEREO_REF: &[u8] = include_bytes!("data/core_51_dmix_stereo.f32");
const NATIVE_REF: &[u8] = include_bytes!("data/core_51_dmix_native.f32");
const FRAME_SIZE: usize = 1024;

/// Decode the stream to planar PCM.
fn decode(downmix: Downmix) -> Vec<Vec<f32>> {
    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_DCA);
    let opts = AudioDecoderOptions::default().downmix(downmix);
    let mut dec = DcaDecoder::try_new(&params, &opts).unwrap();

    let mut pcm: Vec<Vec<f32>> = Vec::new();

    for data in STREAM.chunks(FRAME_SIZE) {
        let packet = PacketRef::new(0, Timestamp::new(0), Duration::new(0), data);
        let GenericAudioBufferRef::F32(buf) = dec.decode_ref(&packet).unwrap()
        else {
            panic!("expected f32 samples");
        };
        let n = buf.spec().channels().count();
        pcm.resize(n, Vec::new());
        for (c, dst) in pcm.iter_mut().enumerate() {
            dst.extend_from_slice(buf.plane(c).unwrap());
        }
    }

    pcm
}

fn reference(bytes: &[u8], nch: usize) -> Vec<Vec<f32>> {
    let samples: Vec<f32> =
        bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    (0..nch).map(|c| samples.iter().skip(c).step_by(nch).copied().collect()).collect()
}

fn snr_db(pcm: &[Vec<f32>], refs: &[Vec<f32>]) -> f64 {
    let (mut sig, mut err) = (0.0f64, 0.0f64);
    for (a, b) in pcm.iter().flatten().zip(refs.iter().flatten()) {
        sig += f64::from(*b).powi(2);
        err += f64::from(a - b).powi(2);
    }
    10.0 * (sig / err.max(1e-30)).log10()
}

#[test]
fn verify_embedded_stereo_downmix_matches_reference() {
    for mode in [Downmix::StereoLoRo, Downmix::StereoLtRt] {
        let pcm = decode(mode);
        let refs = reference(STEREO_REF, 2);
        assert_eq!(pcm.len(), 2);
        assert_eq!(pcm[0].len(), refs[0].len());
        let snr = snr_db(&pcm, &refs);
        assert!(snr > 100.0, "{mode:?}: snr {snr} dB");
    }
}

#[test]
fn verify_no_downmix_keeps_all_channels() {
    let pcm = decode(Downmix::None);
    let refs = reference(NATIVE_REF, 6);
    assert_eq!(pcm.len(), 6);
    assert!(snr_db(&pcm, &refs) > 100.0);
}

#[test]
fn verify_mono_downmix_follows_standard_equation() {
    // ITU-R BS.775 mono: M = 1/sqrt(2) (L + R) + C + 0.5 (Ls + Rs). The native reference is in
    // the order FL, FR, FC, LFE, SL, SR.
    let mono = decode(Downmix::Mono);
    assert_eq!(mono.len(), 1);
    let native = reference(NATIVE_REF, 6);
    let k = std::f32::consts::FRAC_1_SQRT_2;
    let expected: Vec<f32> = (0..native[0].len())
        .map(|i| {
            k * (native[0][i] + native[1][i]) + native[2][i] + 0.5 * (native[4][i] + native[5][i])
        })
        .collect();
    assert!(snr_db(&mono, &[expected]) > 100.0);
}
