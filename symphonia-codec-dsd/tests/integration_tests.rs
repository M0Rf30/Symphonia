// Integration tests for DSD codec decoder
#![allow(clippy::unwrap_used)]

use symphonia_codec_dsd::decimator::DecimationConfig;
use symphonia_codec_dsd::{CODEC_ID_DSD, CODEC_ID_DST, DsdDecoder, dst};
use symphonia_core::audio::{Audio, Channels, GenericAudioBufferRef};
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, ChannelDataLayout,
};
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

    // A packet larger than the declared maximum is decoded: the buffer grows.
    let data: Vec<u8> = vec![0x55; 1000];
    match decoder.decode_ref(&packet(&data)).unwrap() {
        GenericAudioBufferRef::U8(buf) => assert_eq!(buf.frames(), 500),
        _ => panic!("Expected U8 buffer for pass-through mode"),
    }
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
            assert_eq!(
                buf.frames(),
                2,
                "frames() must reflect real packet size, not buffer capacity"
            );
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

    // The delay of the decimation filters is dropped from the start of the stream.
    let delay = DecimationConfig::new(2822400, 352800).unwrap().delay_frames();

    let result = decoder.decode_ref(&packet(&d));
    assert!(result.is_ok(), "decode_ref failed: {:?}", result.err());

    match result.unwrap() {
        GenericAudioBufferRef::F32(buf) => {
            // 1024 bytes/ch * 8 bits / 8 (decimation) = 1024 PCM frames, less the delay.
            assert_eq!(buf.frames(), 1024 - delay, "unexpected PCM frames after 8x decimation");
            let samples = buf.plane(0).unwrap();

            assert!(!samples.is_empty(), "PCM output must not be empty");

            // Verify no NaN samples
            for (i, &s) in samples.iter().enumerate() {
                assert!(!s.is_nan(), "sample[{}] is NaN", i);
            }

            // The step signal (all-ones then all-zeros) must produce a non-silent output
            let min = samples.iter().cloned().fold(f32::INFINITY, f32::min);
            let max = samples.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
            assert!((max - min) > 0.1, "decimated output appears silent: min={}, max={}", min, max);
        }
        _ => panic!("Expected F32 buffer for PCM mode"),
    }
}

/// The frames of the PCM output of a stream: those of the packets, then the flush.
fn pcm_frames(decoder: &mut DsdDecoder, packets: &[Vec<u8>]) -> (usize, usize) {
    let mut decoded = 0;
    for data in packets {
        match decoder.decode_ref(&packet(data)).unwrap() {
            GenericAudioBufferRef::F32(buf) => decoded += buf.frames(),
            _ => panic!("Expected F32 buffer for PCM mode"),
        }
    }
    let flushed = match decoder.flush().unwrap() {
        Some(GenericAudioBufferRef::F32(buf)) => buf.frames(),
        Some(_) => panic!("Expected F32 buffer for PCM mode"),
        None => 0,
    };
    (decoded, flushed)
}

fn pcm_params(pcm_rate: u32) -> AudioCodecParameters {
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_DSD)
        .with_sample_rate(2822400)
        .with_channels(Channels::Discrete(2))
        .with_max_frames_per_packet(1024 * 8)
        .with_channel_data_layout(ChannelDataLayout::Interleaved);
    params.extra_data = Some(pcm_rate.to_le_bytes().to_vec().into_boxed_slice());
    params
}

/// With gapless playback the filter delay is dropped from the start, and `flush` returns the
/// frames at the end, so that a stream has one PCM frame per decimation ratio DSD samples. Without
/// it the delay is kept, and there is nothing to flush.
#[test]
fn test_pcm_gapless_frame_counts() {
    let packets: Vec<Vec<u8>> = (0..5).map(|n| vec![0x35 ^ n as u8; 1024 * 2]).collect();
    let total = 5 * 1024 * 8 / 64;

    let mut decoder =
        DsdDecoder::try_new(&pcm_params(44100), &AudioDecoderOptions::default()).unwrap();
    let delay = DecimationConfig::new(2822400, 44100).unwrap().delay_frames();
    assert_eq!(pcm_frames(&mut decoder, &packets), (total - delay, delay));
    // Nothing is left.
    assert!(decoder.flush().unwrap().is_none());

    // After a reset the stream starts over, as after a seek.
    decoder.reset();
    assert_eq!(pcm_frames(&mut decoder, &packets), (total - delay, delay));

    let mut options = AudioDecoderOptions::default();
    options.gapless = false;
    let mut decoder = DsdDecoder::try_new(&pcm_params(44100), &options).unwrap();
    assert_eq!(pcm_frames(&mut decoder, &packets), (total, 0));
}

/// The PCM of a stream does not depend on how the stream is divided into packets.
#[test]
fn test_pcm_independent_of_packet_size() {
    // Pseudo-random DSD, 2048 bytes for each of the 2 channels.
    let dsd: Vec<u8> = (0..4096u32).map(|n| (n.wrapping_mul(2654435761) >> 13) as u8).collect();

    let decode = |size: usize| -> [Vec<u32>; 2] {
        let mut decoder =
            DsdDecoder::try_new(&pcm_params(88200), &AudioDecoderOptions::default()).unwrap();
        let mut out = [Vec::new(), Vec::new()];
        for part in dsd.chunks(size * 2) {
            match decoder.decode_ref(&packet(part)).unwrap() {
                GenericAudioBufferRef::F32(buf) => {
                    for (ch, out) in out.iter_mut().enumerate() {
                        out.extend(buf.plane(ch).unwrap().iter().map(|s| s.to_bits()));
                    }
                }
                _ => panic!("Expected F32 buffer for PCM mode"),
            }
        }
        out
    };

    let reference = decode(2048);
    let delay = DecimationConfig::new(2822400, 88200).unwrap().delay_frames();
    assert_eq!(reference[0].len(), 2048 * 8 / 32 - delay);
    assert_ne!(reference[0], reference[1]);

    for size in [1, 7, 100, 512] {
        assert!(decode(size) == reference, "packets of {size} bytes per channel");
    }
}

fn dst_params(channels: u16, samples_per_frame: u64) -> AudioCodecParameters {
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_DST)
        .with_sample_rate(2822400)
        .with_channels(Channels::Discrete(channels))
        .with_max_frames_per_packet(samples_per_frame);
    params
}

/// A DST packet of an uncompressed frame (DSTCoded = 0) of `dsd`, with a CRC if `crc` is some.
fn raw_dst_packet(dsd: &[u8], crc: Option<u32>) -> Vec<u8> {
    let mut data = vec![crc.is_some() as u8];
    data.extend_from_slice(&crc.unwrap_or(0).to_be_bytes());
    data.push(0); // DSTCoded = 0, a dummy bit, and 6 zero stuffing bits.
    data.extend_from_slice(dsd);
    data
}

/// DST: an uncompressed frame is the DSD itself. The frame CRC is checked with `verify`.
#[test]
fn test_dst_raw_frame_and_crc() {
    // 2 channels of 4 bytes, interleaved.
    let dsd = [0x01, 0x80, 0x02, 0x40, 0x04, 0x20, 0x08, 0x10];
    let crc = dst::dst_frame_crc(&dsd);

    let mut decoder =
        DsdDecoder::try_new(&dst_params(2, 32), &AudioDecoderOptions::default()).unwrap();
    for packet_data in [raw_dst_packet(&dsd, None), raw_dst_packet(&dsd, Some(crc))] {
        match decoder.decode_ref(&packet(&packet_data)).unwrap() {
            GenericAudioBufferRef::U8(buf) => {
                assert_eq!(buf.frames(), 4);
                assert_eq!(buf.plane(0).unwrap(), &[0x01, 0x02, 0x04, 0x08]);
                assert_eq!(buf.plane(1).unwrap(), &[0x80, 0x40, 0x20, 0x10]);
            }
            _ => panic!("Expected U8 buffer for pass-through mode"),
        }
    }
    // Without verification the CRC is not checked and there is no verification result.
    assert!(decoder.decode_ref(&packet(&raw_dst_packet(&dsd, Some(crc ^ 1)))).is_ok());
    assert_eq!(decoder.finalize().verify_ok, None);

    let options = AudioDecoderOptions::default().verify(true);
    let mut decoder = DsdDecoder::try_new(&dst_params(2, 32), &options).unwrap();
    assert!(decoder.decode_ref(&packet(&raw_dst_packet(&dsd, Some(crc)))).is_ok());
    assert_eq!(decoder.finalize().verify_ok, Some(true));
    assert!(decoder.decode_ref(&packet(&raw_dst_packet(&dsd, Some(crc ^ 1)))).is_err());
    assert_eq!(decoder.finalize().verify_ok, Some(false));
}

/// DST: invalid parameters and packets are errors, never panics.
#[test]
fn test_dst_invalid_input() {
    let options = AudioDecoderOptions::default();

    // The number of samples of a frame is required, a multiple of 8, and the channels at most 6.
    let mut params = dst_params(2, 32);
    params.max_frames_per_packet = None;
    assert!(DsdDecoder::try_new(&params, &options).is_err());
    assert!(DsdDecoder::try_new(&dst_params(2, 36), &options).is_err());
    assert!(DsdDecoder::try_new(&dst_params(7, 32), &options).is_err());

    let mut decoder = DsdDecoder::try_new(&dst_params(2, 32), &options).unwrap();
    // No frame, an invalid stuffing pattern, and arithmetic code data that is cut short.
    assert!(decoder.decode_ref(&packet(&[0, 0, 0, 0, 0])).is_err());
    assert!(decoder.decode_ref(&packet(&[0, 0, 0, 0, 0, 0x01, 0, 0, 0, 0])).is_err());
    assert!(decoder.decode_ref(&packet(&[0, 0, 0, 0, 0, 0xff, 0xff, 0xff])).is_err());
    assert!(decoder.decode_ref(&packet(&[0, 0, 0, 0])).is_err());

    // A frame that is too short is padded with DSD silence.
    match decoder.decode_ref(&packet(&[0, 0, 0, 0, 0, 0x00, 0x01, 0x02])).unwrap() {
        GenericAudioBufferRef::U8(buf) => {
            assert_eq!(buf.plane(0).unwrap(), &[0x01, 0x69, 0x69, 0x69]);
            assert_eq!(buf.plane(1).unwrap(), &[0x02, 0x69, 0x69, 0x69]);
        }
        _ => panic!("Expected U8 buffer for pass-through mode"),
    }
}

/// A frame of the pass-through output is one byte (8 DSD samples) per channel. The spec's rate is
/// the DSD rate / 8 so that `frames() / rate()` is the duration in seconds.
#[test]
fn test_passthrough_frames_are_bytes_at_dsd_rate_div_8() {
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_DSD)
        .with_sample_rate(2822400)
        .with_channels(Channels::Discrete(2))
        .with_max_frames_per_packet(4096 * 8)
        .with_channel_data_layout(ChannelDataLayout::Planar);

    let mut decoder = DsdDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

    // The codec parameters are unchanged: the sample rate is the rate of the 1-bit samples.
    assert_eq!(decoder.codec_params().sample_rate, Some(2822400));

    // 1 second of DSD64 is 2822400 samples per channel = 352800 bytes per channel. Use a packet of
    // 4096 bytes per channel: 4096 * 8 DSD samples.
    let data = vec![0x69u8; 4096 * 2];
    let decoded = decoder.decode_ref(&packet(&data)).unwrap();

    match decoded {
        GenericAudioBufferRef::U8(buf) => {
            assert_eq!(buf.frames(), 4096);
            assert_eq!(buf.spec().rate(), 352800);

            let secs = buf.frames() as f64 / f64::from(buf.spec().rate());
            let expected = (4096 * 8) as f64 / 2822400.0;
            assert!((secs - expected).abs() < 1e-12);
        }
        _ => panic!("Expected U8 buffer for pass-through mode"),
    }
}
