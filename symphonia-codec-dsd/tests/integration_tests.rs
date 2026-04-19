// Integration tests for DSD codec decoder

use symphonia_codec_dsd::CODEC_TYPE_DSD;
use symphonia_core::audio::{AudioBufferRef, Layout};
use symphonia_core::codecs::{CodecParameters, Decoder, DecoderOptions};
use symphonia_core::formats::Packet;

#[test]
fn test_decoder_creation_passthrough() {
    let mut params = CodecParameters::new();
    params
        .for_codec(CODEC_TYPE_DSD)
        .with_sample_rate(2822400)
        .with_channels(Layout::Stereo.into_channels())
        .with_max_frames_per_packet(4096);

    let options = DecoderOptions::default();
    let result = symphonia_codec_dsd::DsdDecoder::try_new(&params, &options);
    assert!(result.is_ok());
}

#[test]
fn test_decoder_missing_sample_rate() {
    let mut params = CodecParameters::new();
    params
        .for_codec(CODEC_TYPE_DSD)
        .with_channels(Layout::Stereo.into_channels());

    let options = DecoderOptions::default();
    let result = symphonia_codec_dsd::DsdDecoder::try_new(&params, &options);
    assert!(result.is_err());
}

#[test]
fn test_decoder_missing_channels() {
    let mut params = CodecParameters::new();
    params
        .for_codec(CODEC_TYPE_DSD)
        .with_sample_rate(2822400);

    let options = DecoderOptions::default();
    let result = symphonia_codec_dsd::DsdDecoder::try_new(&params, &options);
    assert!(result.is_err());
}

#[test]
fn test_decoder_wrong_codec_type() {
    let mut params = CodecParameters::new();
    // Use a different codec type
    params
        .with_sample_rate(2822400)
        .with_channels(Layout::Stereo.into_channels());

    let options = DecoderOptions::default();
    let result = symphonia_codec_dsd::DsdDecoder::try_new(&params, &options);
    assert!(result.is_err());
}

#[test]
fn test_decoder_passthrough_decode() {
    let mut params = CodecParameters::new();
    params
        .for_codec(CODEC_TYPE_DSD)
        .with_sample_rate(2822400)
        .with_channels(Layout::Stereo.into_channels())
        .with_max_frames_per_packet(4096);

    let options = DecoderOptions::default();
    let mut decoder = symphonia_codec_dsd::DsdDecoder::try_new(&params, &options).unwrap();

    // Create a test packet with DSD silence pattern (0x55)
    let data: Vec<u8> = vec![0x55; 16]; // 8 samples per channel (stereo)
    let packet = Packet::new_from_boxed_slice(0, 0, 16, data.into_boxed_slice());

    let result = decoder.decode(&packet);
    assert!(result.is_ok());

    let buffer = result.unwrap();
    match buffer {
        AudioBufferRef::U8(ref buf) => {
            assert_eq!(buf.spec().channels.count(), 2);
            // Just verify it's a U8 buffer - frames might include buffer capacity
        }
        _ => panic!("Expected U8 buffer for pass-through mode"),
    }
}

#[test]
fn test_decoder_empty_packet() {
    let mut params = CodecParameters::new();
    params
        .for_codec(CODEC_TYPE_DSD)
        .with_sample_rate(2822400)
        .with_channels(Layout::Stereo.into_channels())
        .with_max_frames_per_packet(4096);

    let options = DecoderOptions::default();
    let mut decoder = symphonia_codec_dsd::DsdDecoder::try_new(&params, &options).unwrap();

    // Empty packet
    let data: Vec<u8> = vec![];
    let packet = Packet::new_from_boxed_slice(0, 0, 0, data.into_boxed_slice());

    let result = decoder.decode(&packet);
    assert!(result.is_err()); // Should error on empty packet
}

#[test]
fn test_decoder_oversized_packet() {
    let mut params = CodecParameters::new();
    params
        .for_codec(CODEC_TYPE_DSD)
        .with_sample_rate(2822400)
        .with_channels(Layout::Stereo.into_channels())
        .with_max_frames_per_packet(100); // Small buffer

    let options = DecoderOptions::default();
    let mut decoder = symphonia_codec_dsd::DsdDecoder::try_new(&params, &options).unwrap();

    // Packet larger than buffer capacity
    let data: Vec<u8> = vec![0x55; 1000];
    let packet = Packet::new_from_boxed_slice(0, 0, 1000, data.into_boxed_slice());

    let result = decoder.decode(&packet);
    assert!(result.is_err()); // Should error on oversized packet
}

#[test]
fn test_decoder_reset() {
    let mut params = CodecParameters::new();
    params
        .for_codec(CODEC_TYPE_DSD)
        .with_sample_rate(2822400)
        .with_channels(Layout::Stereo.into_channels())
        .with_max_frames_per_packet(4096);

    let options = DecoderOptions::default();
    let mut decoder = symphonia_codec_dsd::DsdDecoder::try_new(&params, &options).unwrap();

    // Decode a packet
    let data: Vec<u8> = vec![0x55; 16];
    let packet = Packet::new_from_boxed_slice(0, 0, 16, data.into_boxed_slice());
    let _ = decoder.decode(&packet);

    // Reset should clear state
    decoder.reset();

    // Should still be able to decode after reset
    let data2: Vec<u8> = vec![0xAA; 16];
    let packet2 = Packet::new_from_boxed_slice(0, 0, 16, data2.into_boxed_slice());
    let result = decoder.decode(&packet2);
    assert!(result.is_ok());
}

#[test]
fn test_decoder_codec_params() {
    let mut params = CodecParameters::new();
    params
        .for_codec(CODEC_TYPE_DSD)
        .with_sample_rate(2822400)
        .with_channels(Layout::Stereo.into_channels())
        .with_max_frames_per_packet(4096);

    let options = DecoderOptions::default();
    let decoder = symphonia_codec_dsd::DsdDecoder::try_new(&params, &options).unwrap();

    let codec_params = decoder.codec_params();
    assert_eq!(codec_params.sample_rate, Some(2822400));
    assert_eq!(codec_params.channels.unwrap().count(), 2);
}

#[test]
fn test_decoder_pcm_mode_creation() {
    let mut params = CodecParameters::new();
    params
        .for_codec(CODEC_TYPE_DSD)
        .with_sample_rate(2822400)
        .with_channels(Layout::Stereo.into_channels())
        .with_max_frames_per_packet(4096);

    // Enable PCM mode with extra_data (first 4 bytes = output rate)
    let pcm_rate = 44100u32;
    let extra_data = pcm_rate.to_le_bytes().to_vec().into_boxed_slice();
    params.extra_data = Some(extra_data);

    let options = DecoderOptions::default();
    let result = symphonia_codec_dsd::DsdDecoder::try_new(&params, &options);
    assert!(result.is_ok());

    let decoder = result.unwrap();
    let codec_params = decoder.codec_params();

    // In PCM mode, codec params should reflect PCM output rate
    assert_eq!(codec_params.sample_rate, Some(44100));
}

#[test]
fn test_decoder_pcm_mode_invalid_rates() {
    let mut params = CodecParameters::new();
    params
        .for_codec(CODEC_TYPE_DSD)
        .with_sample_rate(2822400)
        .with_channels(Layout::Stereo.into_channels())
        .with_max_frames_per_packet(4096);

    // Try incompatible output rate (not evenly divisible)
    let pcm_rate = 45000u32; // 2822400 / 45000 is not an integer
    let extra_data = pcm_rate.to_le_bytes().to_vec().into_boxed_slice();
    params.extra_data = Some(extra_data);

    let options = DecoderOptions::default();
    let result = symphonia_codec_dsd::DsdDecoder::try_new(&params, &options);
    assert!(result.is_err());
}

#[test]
fn test_decoder_multi_channel() {
    let mut params = CodecParameters::new();
    params
        .for_codec(CODEC_TYPE_DSD)
        .with_sample_rate(2822400)
        .with_channels(Layout::FivePointOne.into_channels()) // 6 channels
        .with_max_frames_per_packet(4096);

    let options = DecoderOptions::default();
    let mut decoder = symphonia_codec_dsd::DsdDecoder::try_new(&params, &options).unwrap();

    // Create packet with 6 channels
    let data: Vec<u8> = vec![0x55; 60]; // 10 samples per channel (6 channels)
    let packet = Packet::new_from_boxed_slice(0, 0, 60, data.into_boxed_slice());

    let result = decoder.decode(&packet);
    assert!(result.is_ok());

    let buffer = result.unwrap();
    match buffer {
        AudioBufferRef::U8(ref buf) => {
            assert_eq!(buf.spec().channels.count(), 6);
            // Just verify it's a U8 buffer with correct channel count
        }
        _ => panic!("Expected U8 buffer"),
    }
}
