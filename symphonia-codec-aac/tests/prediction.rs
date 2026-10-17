#![allow(clippy::unwrap_used)]

//! AAC Main profile prediction and long term prediction (LTP).
//!
//! The fixtures are the first 60 access units of streams made by the FFmpeg 4.3 AAC encoder: the
//! Main profile stream with prediction on music-like noise, and the LTP stream of three tones
//! with its LTP search forced on (the stock search never selects LTP), so most frames use it with
//! lags around 1090 samples. The references are the 16-bit output of the FFmpeg 4.3 decoder (a
//! 4 byte header, then the interleaved samples). The files are `u16` big-endian length records,
//! the first of which is the audio specific config.

use symphonia_codec_aac::AacDecoder;
use symphonia_common::mpeg::audio::AudioSpecificConfig;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::Packet;
use symphonia_core::units::{Duration, Timestamp};

/// Decode a fixture and return the maximum difference to the reference in LSB and the SNR in dB.
fn compare(bin: &[u8], s16: &[u8]) -> (f32, f64) {
    let mut records = vec![];
    let mut pos = 0;
    while pos < bin.len() {
        let len = usize::from(u16::from_be_bytes([bin[pos], bin[pos + 1]]));
        records.push(&bin[pos + 2..pos + 2 + len]);
        pos += 2 + len;
    }
    let asc_bytes = records.remove(0);
    let asc = AudioSpecificConfig::read(asc_bytes).unwrap();

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
            Timestamp::new((i * 1024) as i64),
            Duration::new(1024),
            au.to_vec().into_boxed_slice(),
        );

        let GenericAudioBufferRef::F32(buf) = decoder.decode(&packet).unwrap()
        else {
            panic!("expected f32 samples");
        };

        for f in 0..buf.frames() {
            for c in 0..2 {
                samples.push(buf.plane(c).unwrap()[f]);
            }
        }
    }

    let reference: Vec<i16> =
        s16[4..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
    assert_eq!(reference.len(), samples.len());

    let (mut max_diff, mut sig, mut err) = (0.0f32, 0.0f64, 0.0f64);
    for (&r, &s) in reference.iter().zip(&samples) {
        let actual = (s * 32768.0).clamp(-32768.0, 32767.0);
        max_diff = max_diff.max((actual.round() - f32::from(r)).abs());
        sig += f64::from(r).powi(2);
        err += f64::from(actual - f32::from(r)).powi(2);
    }

    (max_diff, 10.0 * (sig / err.max(1e-9)).log10())
}

#[test]
fn aac_ltp_matches_ffmpeg() {
    let (max_diff, _) = compare(
        include_bytes!("fixtures/aac_ltp_stereo.bin"),
        include_bytes!("fixtures/aac_ltp_stereo.s16"),
    );

    assert!(max_diff <= 1.0, "maximum difference {max_diff} LSB, expected at most 1");
}

#[test]
fn aac_main_prediction_matches_ffmpeg() {
    // The predictors round their state to 16 bit floats, so a one ulp difference in a
    // dequantised coefficient can change a later prediction by a unit of that precision.
    let (_, snr) = compare(
        include_bytes!("fixtures/aac_main_stereo.bin"),
        include_bytes!("fixtures/aac_main_stereo.s16"),
    );

    assert!(snr >= 40.0, "SNR {snr} dB, expected at least 40");
}
