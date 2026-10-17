#![allow(clippy::unwrap_used)]

//! AAC-LC with 960 sample frames (`frameLengthFlag` 1 of the audio specific config).
//!
//! The fixture is the first 8 access units of a 48 kHz stereo FDK stream of 1024 sample frames,
//! with the flag set in its audio specific config: the stream is valid with 960 sample frames
//! because the scale factor bands of the two lengths agree up to the highest band that the
//! encoder used, so the decoded audio is noise-like but exercises the 960/120 sample transforms,
//! windows and band tables, including an `ONLY_LONG`, `LONG_START`, `EIGHT_SHORT` and `LONG_STOP`
//! window sequence. The reference is the 16-bit output of ffmpeg (a 32-bit big-endian index of its
//! first sample, then the interleaved samples).

use symphonia_codec_aac::AacDecoder;
use symphonia_common::mpeg::audio::AudioSpecificConfig;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::Packet;
use symphonia_core::units::{Duration, Timestamp};

#[test]
fn aac_lc_960_matches_ffmpeg() {
    let bin = include_bytes!("fixtures/aac_lc_960_stereo.bin");
    let s16 = include_bytes!("fixtures/aac_lc_960_stereo.s16");

    let mut records = vec![];
    let mut pos = 0;
    while pos < bin.len() {
        let len = usize::from(u16::from_be_bytes([bin[pos], bin[pos + 1]]));
        records.push(&bin[pos + 2..pos + 2 + len]);
        pos += 2 + len;
    }
    let asc_bytes = records.remove(0);
    let asc = AudioSpecificConfig::read(asc_bytes).unwrap();
    assert_eq!(asc.samples, 960);

    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_AAC)
        .with_sample_rate(asc.output_sample_rate())
        .with_channels(asc.output_channels().unwrap())
        .with_extra_data(asc_bytes.into());

    let mut decoder = AacDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
    let mut samples = vec![];

    for (i, au) in records.iter().enumerate() {
        let packet = Packet::new(
            0,
            Timestamp::new((i * 960) as i64),
            Duration::new(960),
            au.to_vec().into_boxed_slice(),
        );

        let GenericAudioBufferRef::F32(buf) = decoder.decode(&packet).unwrap()
        else {
            panic!("expected f32 samples");
        };

        assert_eq!(buf.frames(), 960);

        for f in 0..buf.frames() {
            for c in 0..2 {
                samples.push(buf.plane(c).unwrap()[f]);
            }
        }
    }

    let reference: Vec<i16> =
        s16[4..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
    assert_eq!(reference.len(), samples.len());

    let mut max_diff = 0.0f32;
    for (&r, &s) in reference.iter().zip(&samples) {
        let actual = (s * 32768.0).clamp(-32768.0, 32767.0).round();
        max_diff = max_diff.max((actual - f32::from(r)).abs());
    }

    assert!(max_diff <= 1.0, "maximum difference {max_diff} LSB, expected at most 1");
}
