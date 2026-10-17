#![allow(clippy::unwrap_used)]

//! USAC (xHE-AAC) frequency-domain decoding.
//!
//! The fixtures are 48 kHz streams of 1024 sample frames from the Fraunhofer xHE-AAC encoder
//! (`libfdk-xhe-aac`), each a 16-bit big-endian length followed by a record, the first record being
//! the audio specific config. The references are the 16-bit output of libfdk-aac 3.0.0 with its
//! DRC normalisation and limiter disabled. The mono stream is 48 access units. The stereo one is
//! 410 access units covering mid/side stereo, TNS applied in the L/R domain, `fac_data()` in
//! frequency-domain frames (access units 266 to 269) and the complex stereo prediction with the
//! previous-frame downmix (access units 403 and 404); only the output frames listed in its `.ref`
//! file (a little-endian first frame and frame count, then those frames) are stored.

use symphonia_codec_aac::AacDecoder;
use symphonia_common::mpeg::audio::AudioSpecificConfig;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoderOptions};
use symphonia_core::codecs::registry::RegisterableAudioDecoder;
use symphonia_core::packet::Packet;
use symphonia_core::units::{Duration, Timestamp};

const FRAME: usize = 1024;

/// Decode all access units, returning the interleaved samples of every frame.
fn decode(bin: &[u8], channels: usize, out_frame: usize) -> Vec<f32> {
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

    let mut decoder =
        AacDecoder::try_registry_new(&params, &AudioDecoderOptions::default()).unwrap();
    let mut samples = vec![];

    for (i, au) in records.iter().enumerate() {
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
        assert_eq!(buf.frames(), out_frame);

        for f in 0..out_frame {
            for c in 0..channels {
                samples.push(buf.plane(c).unwrap()[f]);
            }
        }
    }

    samples
}

fn max_diff(reference: &[u8], samples: &[f32]) -> f32 {
    assert_eq!(reference.len(), samples.len() * 2);

    reference
        .chunks_exact(2)
        .zip(samples)
        .map(|(r, &s)| {
            let actual = (s * 32768.0).clamp(-32768.0, 32767.0).round();
            (actual - f32::from(i16::from_le_bytes([r[0], r[1]]))).abs()
        })
        .fold(0.0, f32::max)
}

#[test]
fn usac_fd_mono_matches_fdk() {
    let samples = decode(include_bytes!("fixtures/usac_fd_mono.bin"), 1, FRAME);
    let diff = max_diff(include_bytes!("fixtures/usac_fd_mono.s16"), &samples);

    assert!(diff <= 1.0, "maximum difference {diff} LSB, expected at most 1");
}

#[test]
fn usac_fd_stereo_matches_fdk() {
    let samples = decode(include_bytes!("fixtures/usac_fd_stereo.bin"), 2, FRAME);
    let reference = include_bytes!("fixtures/usac_fd_stereo.ref");

    let mut pos = 0;
    let mut windows = 0;
    while pos < reference.len() {
        let first = u32::from_le_bytes(reference[pos..pos + 4].try_into().unwrap()) as usize;
        let count = u32::from_le_bytes(reference[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let bytes = count * FRAME * 2 * 2;
        let pcm = &reference[pos + 8..pos + 8 + bytes];
        pos += 8 + bytes;
        windows += 1;

        let diff = max_diff(pcm, &samples[first * FRAME * 2..(first + count) * FRAME * 2]);
        assert!(diff <= 1.0, "frames {first}..: maximum difference {diff} LSB, expected at most 1");
    }
    assert_eq!(windows, 3);
}

/// A 2:1 eSBR mono stream (24 kHz core, 48 kHz output). The SBR tool is compared by signal to
/// noise ratio: the transposer and the envelope adjuster are floating point here and fixed point
/// in libfdk-aac.
#[test]
fn usac_esbr_mono_matches_fdk() {
    let samples = decode(include_bytes!("fixtures/usac_sbr_mono.bin"), 1, 2 * FRAME);
    let reference = include_bytes!("fixtures/usac_sbr_mono.s16");
    assert_eq!(reference.len(), samples.len() * 2);

    let mut signal = 0.0f64;
    let mut noise = 0.0f64;
    for (r, &s) in reference.chunks_exact(2).zip(&samples) {
        let r = f64::from(i16::from_le_bytes([r[0], r[1]]));
        let e = f64::from((s * 32768.0).clamp(-32768.0, 32767.0).round()) - r;
        signal += r * r;
        noise += e * e;
    }

    let snr = 10.0 * (signal / noise.max(1e-9)).log10();
    assert!(snr >= 45.0, "signal to noise ratio {snr:.1} dB, expected at least 45");
}
