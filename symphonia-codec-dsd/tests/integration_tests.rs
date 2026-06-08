// Integration tests for DSD codec decoder

use symphonia_codec_dsd::{DsdDecoder, CODEC_ID_DSD};
use symphonia_core::audio::{Audio, Channels, GenericAudioBufferRef};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions, ChannelDataLayout};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

/// Build a `PacketRef` borrowing `data` for a single-track stream.
fn packet(data: &[u8]) -> PacketRef<'_> {
    PacketRef::new(0, Timestamp::new(0), Duration::new(data.len() as u64), data)
}

#[test]
fn test_decoder_creation_passthrough() {
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_DSD)
        .with_sample_rate(2822400)
        .with_channels(Channels::Discrete(2))
        .with_max_frames_per_packet(4096);

    let options = AudioDecoderOptions::default();
    let result = DsdDecoder::try_new(&params, &options);
    assert!(result.is_ok());
}

#[test]
fn test_decoder_missing_sample_rate() {
    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_DSD).with_channels(Channels::Discrete(2));

    let options = AudioDecoderOptions::default();
    let result = DsdDecoder::try_new(&params, &options);
    assert!(result.is_err());
}

#[test]
fn test_decoder_missing_channels() {
    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_DSD).with_sample_rate(2822400);

    let options = AudioDecoderOptions::default();
    let result = DsdDecoder::try_new(&params, &options);
    assert!(result.is_err());
}

#[test]
fn test_decoder_wrong_codec_type() {
    let mut params = AudioCodecParameters::new();
    // Leave the codec ID as the default (null) so it is not DSD.
    params.with_sample_rate(2822400).with_channels(Channels::Discrete(2));

    let options = AudioDecoderOptions::default();
    let result = DsdDecoder::try_new(&params, &options);
    assert!(result.is_err());
}

#[test]
fn test_decoder_passthrough_decode() {
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_DSD)
        .with_sample_rate(2822400)
        .with_channels(Channels::Discrete(2))
        .with_max_frames_per_packet(4096);

    let options = AudioDecoderOptions::default();
    let mut decoder = DsdDecoder::try_new(&params, &options).unwrap();

    // Create a test packet with the DSD silence pattern (0x55).
    let data: Vec<u8> = vec![0x55; 16]; // 8 samples per channel (stereo)
    let result = decoder.decode_ref(&packet(&data));
    assert!(result.is_ok());

    match result.unwrap() {
        GenericAudioBufferRef::U8(buf) => {
            assert_eq!(buf.spec().channels().count(), 2);
        }
        _ => panic!("Expected U8 buffer for pass-through mode"),
    }
}

#[test]
fn test_decoder_empty_packet() {
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_DSD)
        .with_sample_rate(2822400)
        .with_channels(Channels::Discrete(2))
        .with_max_frames_per_packet(4096);

    let options = AudioDecoderOptions::default();
    let mut decoder = DsdDecoder::try_new(&params, &options).unwrap();

    // Empty packet.
    let data: Vec<u8> = vec![];
    let result = decoder.decode_ref(&packet(&data));
    assert!(result.is_err()); // Should error on empty packet
}

#[test]
fn test_decoder_oversized_packet() {
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_DSD)
        .with_sample_rate(2822400)
        .with_channels(Channels::Discrete(2))
        .with_max_frames_per_packet(100); // Small buffer

    let options = AudioDecoderOptions::default();
    let mut decoder = DsdDecoder::try_new(&params, &options).unwrap();

    // Packet larger than buffer capacity.
    let data: Vec<u8> = vec![0x55; 1000];
    let result = decoder.decode_ref(&packet(&data));
    assert!(result.is_err()); // Should error on oversized packet
}

#[test]
fn test_decoder_reset() {
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_DSD)
        .with_sample_rate(2822400)
        .with_channels(Channels::Discrete(2))
        .with_max_frames_per_packet(4096);

    let options = AudioDecoderOptions::default();
    let mut decoder = DsdDecoder::try_new(&params, &options).unwrap();

    // Decode a packet.
    let data: Vec<u8> = vec![0x55; 16];
    let _ = decoder.decode_ref(&packet(&data));

    // Reset should clear state.
    decoder.reset();

    // Should still be able to decode after reset.
    let data2: Vec<u8> = vec![0xAA; 16];
    let result = decoder.decode_ref(&packet(&data2));
    assert!(result.is_ok());
}

#[test]
fn test_decoder_codec_params() {
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_DSD)
        .with_sample_rate(2822400)
        .with_channels(Channels::Discrete(2))
        .with_max_frames_per_packet(4096);

    let options = AudioDecoderOptions::default();
    let decoder = DsdDecoder::try_new(&params, &options).unwrap();

    let codec_params = decoder.codec_params();
    assert_eq!(codec_params.sample_rate, Some(2822400));
    assert_eq!(codec_params.channels.as_ref().unwrap().count(), 2);
}

#[test]
fn test_decoder_pcm_mode_creation() {
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_DSD)
        .with_sample_rate(2822400)
        .with_channels(Channels::Discrete(2))
        .with_max_frames_per_packet(4096);

    // Enable PCM mode with extra_data (first 4 bytes = output rate).
    let pcm_rate = 44100u32;
    params.extra_data = Some(pcm_rate.to_le_bytes().to_vec().into_boxed_slice());

    let options = AudioDecoderOptions::default();
    let result = DsdDecoder::try_new(&params, &options);
    assert!(result.is_ok());

    let decoder = result.unwrap();
    let codec_params = decoder.codec_params();

    // In PCM mode, codec params should reflect the PCM output rate.
    assert_eq!(codec_params.sample_rate, Some(44100));
}

#[test]
fn test_decoder_pcm_mode_invalid_rates() {
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_DSD)
        .with_sample_rate(2822400)
        .with_channels(Channels::Discrete(2))
        .with_max_frames_per_packet(4096);

    // Try an incompatible output rate (not evenly divisible).
    let pcm_rate = 45000u32; // 2822400 / 45000 is not an integer
    params.extra_data = Some(pcm_rate.to_le_bytes().to_vec().into_boxed_slice());

    let options = AudioDecoderOptions::default();
    let result = DsdDecoder::try_new(&params, &options);
    assert!(result.is_err());
}

#[test]
fn test_decoder_multi_channel() {
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_DSD)
        .with_sample_rate(2822400)
        .with_channels(Channels::Discrete(6)) // 6 channels
        .with_max_frames_per_packet(4096);

    let options = AudioDecoderOptions::default();
    let mut decoder = DsdDecoder::try_new(&params, &options).unwrap();

    // Create a packet with 6 channels.
    let data: Vec<u8> = vec![0x55; 60]; // 10 samples per channel (6 channels)
    let result = decoder.decode_ref(&packet(&data));
    assert!(result.is_ok());

    match result.unwrap() {
        GenericAudioBufferRef::U8(buf) => {
            assert_eq!(buf.spec().channels().count(), 6);
        }
        _ => panic!("Expected U8 buffer"),
    }
}

#[test]
fn test_passthrough_planar_separation() {
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_DSD)
        .with_sample_rate(2822400)
        .with_channels(Channels::Discrete(2))
        .with_max_frames_per_packet(4096)
        .with_channel_data_layout(ChannelDataLayout::Planar);

    let options = AudioDecoderOptions::default();
    let mut decoder = DsdDecoder::try_new(&params, &options).unwrap();

    // Planar: 8 bytes of ch0 (0xAA) followed by 8 bytes of ch1 (0xBB)
    let mut d = vec![0xAAu8; 8];
    d.extend(vec![0xBBu8; 8]);

    let result = decoder.decode_ref(&packet(&d));
    assert!(result.is_ok(), "decode_ref failed: {:?}", result.err());

    match result.unwrap() {
        GenericAudioBufferRef::U8(buf) => {
            assert_eq!(buf.frames(), 8, "frames() must equal sample_count, not capacity");
            for &s in buf.plane(0).unwrap() {
                assert_eq!(s, 0xAA, "ch0 should be 0xAA");
            }
            for &s in buf.plane(1).unwrap() {
                assert_eq!(s, 0xBB, "ch1 should be 0xBB");
            }
        }
        _ => panic!("Expected U8 buffer for pass-through mode"),
    }
}

#[test]
fn test_passthrough_interleaved_separation() {
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_DSD)
        .with_sample_rate(2822400)
        .with_channels(Channels::Discrete(2))
        .with_max_frames_per_packet(4096)
        .with_channel_data_layout(ChannelDataLayout::Interleaved);

    let options = AudioDecoderOptions::default();
    let mut decoder = DsdDecoder::try_new(&params, &options).unwrap();

    // Interleaved: AA BB AA BB ... (8 pairs = 16 bytes)
    let mut d = Vec::new();
    for _ in 0..8 {
        d.push(0xAA);
        d.push(0xBB);
    }

    let result = decoder.decode_ref(&packet(&d));
    assert!(result.is_ok(), "decode_ref failed: {:?}", result.err());

    match result.unwrap() {
        GenericAudioBufferRef::U8(buf) => {
            assert_eq!(buf.frames(), 8, "frames() must equal sample_count, not capacity");
            for &s in buf.plane(0).unwrap() {
                assert_eq!(s, 0xAA, "ch0 should be 0xAA");
            }
            for &s in buf.plane(1).unwrap() {
                assert_eq!(s, 0xBB, "ch1 should be 0xBB");
            }
        }
        _ => panic!("Expected U8 buffer for pass-through mode"),
    }
}

#[test]
fn test_passthrough_frame_count_not_capacity() {
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_DSD)
        .with_sample_rate(2822400)
        .with_channels(Channels::Discrete(2))
        .with_max_frames_per_packet(4096)
        .with_channel_data_layout(ChannelDataLayout::Interleaved);

    let options = AudioDecoderOptions::default();
    let mut decoder = DsdDecoder::try_new(&params, &options).unwrap();

    // 4 interleaved bytes -> 2 frames per channel (4 / 2 channels)
    let d: Vec<u8> = vec![0x55, 0x55, 0x55, 0x55];
    let result = decoder.decode_ref(&packet(&d));
    assert!(result.is_ok(), "decode_ref failed: {:?}", result.err());

    match result.unwrap() {
        GenericAudioBufferRef::U8(buf) => {
            // Pre-fix, this would have been 4096 (the buffer capacity).
            assert_eq!(buf.frames(), 2, "frames() must reflect real packet size, not buffer capacity");
        }
        _ => panic!("Expected U8 buffer for pass-through mode"),
    }
}

#[test]
fn test_pcm_produces_decimated_signal() {
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_DSD)
        .with_sample_rate(2822400)
        .with_channels(Channels::Discrete(2))
        .with_max_frames_per_packet(4096 * 8)
        .with_channel_data_layout(ChannelDataLayout::Planar);

    // PCM output rate 352800 Hz: decimation = 2822400 / 352800 = 8x
    params.extra_data = Some(352800u32.to_le_bytes().to_vec().into_boxed_slice());

    let options = AudioDecoderOptions::default();
    let mut decoder = DsdDecoder::try_new(&params, &options).unwrap();

    // Each channel: 512 bytes of 0xFF (all-ones) then 512 bytes of 0x00 (all-zeros) = 1024 bytes/ch
    let mut ch = vec![0xFFu8; 512];
    ch.extend(vec![0x00u8; 512]);
    let mut d = ch.clone();
    d.extend_from_slice(&ch);

    let result = decoder.decode_ref(&packet(&d));
    assert!(result.is_ok(), "decode_ref failed: {:?}", result.err());

    match result.unwrap() {
        GenericAudioBufferRef::F32(buf) => {
            // 1024 bytes/ch * 8 bits / 8 (decimation) = 1024 PCM frames
            assert_eq!(buf.frames(), 1024, "expected 1024 PCM frames after 8x decimation");

            let samples = buf.plane(0).unwrap();

            assert!(!samples.is_empty(), "PCM output must not be empty");

            // Verify no NaN samples
            for (i, &s) in samples.iter().enumerate() {
                assert!(!s.is_nan(), "sample[{}] is NaN", i);
            }

            // The step signal (all-ones then all-zeros) must produce a non-silent output
            let min = samples.iter().cloned().fold(f32::INFINITY, f32::min);
            let max = samples.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
            assert!(
                (max - min) > 0.1,
                "decimated output appears silent: min={}, max={}",
                min, max
            );
        }
        _ => panic!("Expected F32 buffer for PCM mode"),
    }
}
