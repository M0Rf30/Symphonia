#![allow(clippy::unwrap_used)]

//! A reset USAC decoder decodes like a new one: the overlap, context, noise filling, eSBR and
//! MPEG Surround state are all restored.

use symphonia_codec_aac::AacDecoder;
use symphonia_common::mpeg::audio::AudioSpecificConfig;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::codecs::registry::RegisterableAudioDecoder;
use symphonia_core::packet::Packet;
use symphonia_core::units::{Duration, Timestamp};

const FRAME: usize = 1024;

/// The decoder of the stream `bin` (16-bit big-endian length prefixed records, the first being
/// the audio specific config), its channel count and its access units.
fn open(bin: &[u8]) -> (Box<dyn AudioDecoder>, usize, Vec<&[u8]>) {
    let mut records = vec![];
    let mut pos = 0;
    while pos < bin.len() {
        let len = usize::from(u16::from_be_bytes([bin[pos], bin[pos + 1]]));
        records.push(&bin[pos + 2..pos + 2 + len]);
        pos += 2 + len;
    }
    let asc_bytes = records.remove(0);
    let asc = AudioSpecificConfig::read(asc_bytes).unwrap();
    let channels = asc.output_channels().unwrap();

    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_AAC)
        .with_sample_rate(asc.output_sample_rate())
        .with_channels(channels.clone())
        .with_extra_data(asc_bytes.into());

    let decoder = AacDecoder::try_registry_new(&params, &AudioDecoderOptions::default()).unwrap();

    (decoder, channels.count(), records)
}

fn decode(decoder: &mut dyn AudioDecoder, channels: usize, i: usize, au: &[u8]) -> Vec<f32> {
    let packet = Packet::new(
        0,
        Timestamp::new((i * FRAME) as i64),
        Duration::new(FRAME as u64),
        au.to_vec().into_boxed_slice(),
    );

    let GenericAudioBufferRef::F32(buf) = decoder.decode(&packet).unwrap()
    else {
        panic!("expected f32 samples");
    };

    (0..channels).flat_map(|c| buf.plane(c).unwrap().to_vec()).collect()
}

/// Decode `warmup` access units, reset, then decode all of them and compare with a new decoder.
fn assert_reset_matches_new(bin: &[u8], warmup: usize) {
    let (mut fresh, channels, aus) = open(bin);
    let expected: Vec<Vec<f32>> =
        aus.iter().enumerate().map(|(i, au)| decode(fresh.as_mut(), channels, i, au)).collect();

    let (mut reused, _, _) = open(bin);
    for (i, au) in aus.iter().enumerate().take(warmup) {
        decode(reused.as_mut(), channels, i, au);
    }
    reused.reset();

    for (i, au) in aus.iter().enumerate() {
        let actual = decode(reused.as_mut(), channels, i, au);
        assert!(actual == expected[i], "access unit {i} differs after a reset");
    }
}

#[test]
fn reset_mps212_matches_new_decoder() {
    assert_reset_matches_new(include_bytes!("fixtures/usac_mps212.bin"), 12);
}

#[test]
fn reset_fd_matches_new_decoder() {
    assert_reset_matches_new(include_bytes!("fixtures/usac_fd_mono.bin"), 12);
    assert_reset_matches_new(include_bytes!("fixtures/usac_fd_stereo.bin"), 30);
}

#[test]
fn reset_esbr_and_lpd_match_new_decoder() {
    assert_reset_matches_new(include_bytes!("fixtures/usac_sbr_mono.bin"), 12);
    assert_reset_matches_new(include_bytes!("fixtures/usac_lpd_fd_mix.bin"), 12);
}
