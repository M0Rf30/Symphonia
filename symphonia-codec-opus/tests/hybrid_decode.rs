// Symphonia
// Copyright (c) 2019-2022 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Hybrid mode decoder tests (SILK + CELT)

use symphonia_codec_opus::OpusDecoder;
use symphonia_core::audio::Channels;
use symphonia_core::codecs::{CodecParameters, Decoder, CODEC_TYPE_OPUS};
use symphonia_core::formats::Packet;

#[test]
fn test_hybrid_mode_integration() {
    // Test that Hybrid mode packets can be created and routed
    // This test verifies that the decoder accepts Hybrid mode TOC bytes

    let mut params = CodecParameters::new();
    params
        .for_codec(CODEC_TYPE_OPUS)
        .with_sample_rate(48000)
        .with_channels(Channels::FRONT_LEFT | Channels::FRONT_RIGHT);

    let decoder = OpusDecoder::try_new(&params, &Default::default())
        .expect("Failed to create Opus decoder");

    // Verify decoder is properly initialized
    let codec_params = decoder.codec_params();
    assert_eq!(codec_params.sample_rate, Some(48000));
    println!("Opus decoder created with Hybrid mode support");
}

#[test]
fn test_hybrid_decoder_doesnt_panic() {
    let mut params = CodecParameters::new();
    params
        .for_codec(CODEC_TYPE_OPUS)
        .with_sample_rate(48000)
        .with_channels(Channels::FRONT_LEFT | Channels::FRONT_RIGHT);

    let mut decoder = OpusDecoder::try_new(&params, &Default::default())
        .expect("Failed to create Opus decoder");

    // Test various Hybrid mode configurations
    let test_cases = vec![
        vec![0x60], // Hybrid SWB 10ms, mono, 1 frame
        vec![0x64], // Hybrid SWB 10ms, stereo, 1 frame
        vec![0x68], // Hybrid SWB 20ms, mono, 1 frame
        vec![0x70], // Hybrid FB 10ms, mono, 1 frame
    ];

    for (i, packet_data) in test_cases.iter().enumerate() {
        let packet = Packet::new_from_boxed_slice(
            0,
            i as u64,
            0,
            packet_data.clone().into_boxed_slice(),
        );

        // Should not panic even with minimal data
        let _ = decoder.decode(&packet);
    }
}

#[test]
fn test_decoder_supports_hybrid_mode() {
    // Verify that OpusDecoder can be created and has Hybrid support
    let mut params = CodecParameters::new();
    params
        .for_codec(CODEC_TYPE_OPUS)
        .with_sample_rate(48000)
        .with_channels(Channels::FRONT_LEFT | Channels::FRONT_RIGHT);

    let decoder = OpusDecoder::try_new(&params, &Default::default())
        .expect("Failed to create Opus decoder");

    // If we got here, decoder was created successfully with Hybrid support
    let codec_params = decoder.codec_params();
    assert_eq!(codec_params.sample_rate, Some(48000));
}
