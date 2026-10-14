#![allow(clippy::unwrap_used)]

//! The decoded output of ADTS streams with SBR (HE-AAC v1/v2) and of the LC core, compared with the
//! output of ffmpeg. The references (`.s16`) are the first 8192 frames of ffmpeg's 16-bit output,
//! preceded by a 32-bit big-endian index of the first frame they hold.
//!
//! The comparison has a tolerance because the f32 output is not bit-for-bit portable: the FFT
//! backend (`rustfft` with or without SIMD, or `sbr-fft-qmf`) and the target round differently.
//! The LD/ELD fixtures (with and without SBR) are compared in `ld_eld.rs`.

use symphonia_codec_aac::{AacDecoder, AdtsReader};
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::TrackType;
use symphonia_core::formats::probe::ProbeableFormat;
use symphonia_core::io::MediaSourceStream;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

/// Decodes an ADTS fixture to interleaved samples, returning them and the channel count.
fn decode(name: &str) -> (Vec<f32>, usize) {
    let file = std::fs::File::open(std::path::Path::new(FIXTURES).join(name)).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut reader = AdtsReader::try_probe_new(mss, Default::default()).unwrap();

    let track = reader.default_track(TrackType::Audio).unwrap();
    let id = track.id;
    let Some(CodecParameters::Audio(params)) = track.codec_params.clone()
    else {
        panic!("no audio parameters");
    };

    let mut decoder = AacDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
    let mut samples = vec![];
    let mut channels = 0;

    while let Some(packet) = reader.next_packet().unwrap() {
        if packet.track_id != id {
            continue;
        }

        let GenericAudioBufferRef::F32(buf) = decoder.decode(&packet).unwrap()
        else {
            panic!("expected f32 samples");
        };

        channels = buf.spec().channels().count();

        for f in 0..buf.frames() {
            for c in 0..channels {
                samples.push(buf.plane(c).unwrap()[f]);
            }
        }
    }

    (samples, channels)
}

/// Checks a fixture against its reference: the SNR (dB) and the largest difference (LSBs).
fn check(name: &str, min_snr: f64, max_diff: f32) {
    let s16 =
        std::fs::read(std::path::Path::new(FIXTURES).join(name.replace(".aac", ".s16"))).unwrap();
    let first = u32::from_be_bytes([s16[0], s16[1], s16[2], s16[3]]) as usize;
    let reference: Vec<i16> =
        s16[4..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();

    let (samples, channels) = decode(name);
    let samples = &samples[first * channels..];
    assert!(samples.len() >= reference.len(), "{name}: the output is shorter than the reference");

    let mut signal = 0.0f64;
    let mut noise = 0.0f64;
    let mut diff = 0.0f32;

    for (&s, &r) in samples.iter().zip(&reference) {
        let expected = f32::from(r);
        let got = (s * 32768.0).clamp(-32768.0, 32767.0);
        signal += f64::from(expected).powi(2);
        noise += f64::from(got - expected).powi(2);
        diff = diff.max((got - expected).abs());
    }

    let snr = 10.0 * (signal / noise.max(1e-9)).log10();
    assert!(snr >= min_snr, "{name}: SNR {snr} dB, expected at least {min_snr} dB");
    assert!(diff <= max_diff, "{name}: maximum difference {diff} LSB, expected at most {max_diff}");
}

#[test]
fn he_aac_v1_matches_ffmpeg() {
    check("he_aac_v1_mono.aac", 75.0, 2.0);
    check("he_aac_v1_stereo.aac", 75.0, 2.0);
}

#[test]
fn he_aac_v2_matches_ffmpeg() {
    check("he_aac_v2.aac", 75.0, 2.0);
}

#[test]
fn aac_lc_matches_ffmpeg() {
    check("aac_lc_22k_mono.aac", 75.0, 2.0);
}
