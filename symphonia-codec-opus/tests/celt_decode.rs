// Symphonia
// Copyright (c) 2019-2022 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CELT decoder tests

use symphonia_codec_opus::OpusDecoder;
use symphonia_core::audio::Channels;
use symphonia_core::codecs::{CodecParameters, Decoder, CODEC_TYPE_OPUS};

#[test]
fn test_celt_mode_decoder_creation() {
    // Test that decoder can be created for CELT mode
    let mut params = CodecParameters::new();
    params
        .for_codec(CODEC_TYPE_OPUS)
        .with_sample_rate(48000)
        .with_channels(Channels::FRONT_LEFT | Channels::FRONT_RIGHT);

    let decoder = OpusDecoder::try_new(&params, &Default::default())
        .expect("Failed to create Opus decoder");

    let codec_params = decoder.codec_params();
    assert_eq!(codec_params.sample_rate, Some(48000));
    println!("CELT decoder created successfully");
}

#[test]
fn test_celt_decoder_reset() {
    // Test that decoder can be reset without panicking
    let mut params = CodecParameters::new();
    params
        .for_codec(CODEC_TYPE_OPUS)
        .with_sample_rate(48000)
        .with_channels(Channels::FRONT_LEFT | Channels::FRONT_RIGHT);

    let mut decoder = OpusDecoder::try_new(&params, &Default::default())
        .expect("Failed to create Opus decoder");

    // Reset should not panic
    decoder.reset();
    println!("CELT decoder reset successfully");
}
