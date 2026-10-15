#![allow(clippy::unwrap_used)]

//! Malformed and unusual packets are decoded (as blank, guessed or erased frames), but never panic.

use symphonia_codec_evrc::EvrcDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::well_known::CODEC_ID_EVRC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;

fn decoder() -> EvrcDecoder {
    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_EVRC);
    EvrcDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap()
}

fn frames(decoder: &mut EvrcDecoder, data: &[u8]) -> Vec<f32> {
    let packet = PacketRef::new(0, Default::default(), Default::default(), data);
    let GenericAudioBufferRef::F32(buf) = decoder.decode_ref(&packet).unwrap()
    else {
        panic!("expected f32 samples");
    };
    buf.plane(0).unwrap().to_vec()
}

/// Random packets of every size, with every rate byte, always yield 160 finite samples.
#[test]
fn verify_random_packets() {
    let mut dec = decoder();
    let mut state = 0x1234_5678_9abc_def0u64;

    for round in 0..40000usize {
        let len = round % 40;
        let mut data = vec![0u8; len];
        for b in data.iter_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *b = (state >> 32) as u8;
        }
        // Most packets have a plausible rate byte.
        if let Some(first) = data.first_mut() {
            if round % 7 != 0 {
                *first %= 5;
            }
        }

        let out = frames(&mut dec, &data);
        assert_eq!(out.len(), 160);
        assert!(out.iter().all(|s| s.is_finite()), "round {round}, len {len}");
    }
}

/// Packets truncated at every length yield 160 samples.
#[test]
fn verify_truncated_packets() {
    let mut dec = decoder();
    let mut full = vec![4u8];
    full.extend((0..22).map(|i| (i * 37 + 11) as u8));
    for len in 0..=full.len() {
        assert_eq!(frames(&mut dec, &full[..len]).len(), 160);
    }
}

/// A run of erased packets stays bounded.
#[test]
fn verify_erasures_decay() {
    let mut dec = decoder();
    // Quarter-rate packets are erasures.
    let erased = [2u8, 0, 0, 0, 0, 0, 0];
    for _ in 0..50 {
        let out = frames(&mut dec, &erased);
        assert!(out.iter().all(|s| s.is_finite() && s.abs() < 100.0));
    }
}

/// The decoder is deterministic and `reset` restores the initial state.
#[test]
fn verify_reset() {
    let mut packets = vec![];
    let mut state = 7u64;
    for _ in 0..40 {
        let mut p = vec![4u8];
        for _ in 0..22 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            p.push((state >> 33) as u8);
        }
        packets.push(p);
    }

    let mut a = decoder();
    let first: Vec<Vec<f32>> = packets.iter().map(|p| frames(&mut a, p)).collect();

    a.reset();
    let second: Vec<Vec<f32>> = packets.iter().map(|p| frames(&mut a, p)).collect();
    assert_eq!(first, second);
}

#[test]
fn verify_rejects_other_codecs_and_stereo() {
    let params = AudioCodecParameters::new();
    assert!(EvrcDecoder::try_new(&params, &AudioDecoderOptions::default()).is_err());

    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_EVRC)
        .with_channels(symphonia_core::audio::layouts::CHANNEL_LAYOUT_STEREO);
    assert!(EvrcDecoder::try_new(&params, &AudioDecoderOptions::default()).is_err());
}
