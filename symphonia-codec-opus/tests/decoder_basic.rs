// Symphonia
// Copyright (c) 2019-2022 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Basic Opus decoder tests

use symphonia_codec_opus::OpusDecoder;
use symphonia_core::audio::Channels;
use symphonia_core::codecs::{CodecParameters, Decoder, CODEC_TYPE_OPUS};

#[test]
fn test_decoder_creation() {
    // Create codec parameters for a basic stereo Opus stream
    let mut params = CodecParameters::new();
    params
        .for_codec(CODEC_TYPE_OPUS)
        .with_sample_rate(48000)
        .with_channels(Channels::FRONT_LEFT | Channels::FRONT_RIGHT);

    // Try to create the decoder
    let result = OpusDecoder::try_new(&params, &Default::default());
    assert!(result.is_ok(), "Failed to create Opus decoder");

    let decoder = result.unwrap();
    let codec_params = decoder.codec_params();
    assert_eq!(codec_params.sample_rate, Some(48000));
}

#[test]
fn test_decoder_reset() {
    let mut params = CodecParameters::new();
    params
        .for_codec(CODEC_TYPE_OPUS)
        .with_sample_rate(48000)
        .with_channels(Channels::FRONT_LEFT | Channels::FRONT_RIGHT);

    let mut decoder = OpusDecoder::try_new(&params, &Default::default())
        .expect("Failed to create decoder");

    // Reset should not panic
    decoder.reset();
}
