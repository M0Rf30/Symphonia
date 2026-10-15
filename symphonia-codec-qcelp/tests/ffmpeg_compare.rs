#![allow(clippy::unwrap_used)]

//! The decoded output of QCP fixtures compared with the output of FFmpeg's `qcelp` decoder.
//!
//! The decoder is floating point and the reference decoder's filters add in a different order, so
//! the comparison has a tolerance: the committed 16-bit references must match within 1 LSB, and
//! the f32 output of `ffmpeg`, if it is installed, must match with a signal-to-noise ratio above
//! 100 dB.

use std::path::Path;
use std::process::Command;

use symphonia_codec_qcelp::QcelpDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::TrackType;
use symphonia_core::formats::probe::ProbeableFormat;
use symphonia_core::io::MediaSourceStream;
use symphonia_format_qcp::QcpReader;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

/// Decodes a QCP fixture to mono samples.
fn decode(name: &str) -> Vec<f32> {
    let file = std::fs::File::open(Path::new(FIXTURES).join(name)).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut reader = QcpReader::try_probe_new(mss, Default::default()).unwrap();

    let track = reader.default_track(TrackType::Audio).unwrap();
    let Some(CodecParameters::Audio(params)) = track.codec_params.clone()
    else {
        panic!("no audio parameters");
    };

    let mut decoder = QcelpDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
    let mut samples = vec![];

    while let Some(packet) = reader.next_packet().unwrap() {
        let GenericAudioBufferRef::F32(buf) = decoder.decode(&packet).unwrap()
        else {
            panic!("expected f32 samples");
        };
        assert_eq!(buf.frames(), 160);
        samples.extend_from_slice(buf.plane(0).unwrap());
    }

    samples
}

fn snr_db(test: &[f32], reference: &[f32]) -> f64 {
    let (mut signal, mut noise) = (0.0f64, 0.0f64);
    for (&t, &r) in test.iter().zip(reference) {
        signal += f64::from(r) * f64::from(r);
        noise += (f64::from(t) - f64::from(r)).powi(2);
    }
    if noise == 0.0 { 999.0 } else { 10.0 * (signal / noise).log10() }
}

#[test]
fn verify_committed_references() {
    for (name, reference) in [
        ("clap.qcp", "clap.ref.s16"),
        ("0036580847.QCP", "0036580847.ref.s16"),
        ("test8.qcp", "test8.ref.s16"),
    ] {
        let samples = decode(name);
        let bytes = std::fs::read(Path::new(FIXTURES).join(reference)).unwrap();
        assert_eq!(bytes.len(), samples.len() * 2, "{name}: length");

        let mut max_diff = 0.0f64;
        for (s, r) in samples.iter().zip(bytes.chunks_exact(2)) {
            let r = f64::from(i16::from_le_bytes([r[0], r[1]]));
            max_diff = max_diff.max((f64::from(*s) * 32768.0 - r).abs());
        }
        println!("{name}: {} samples, max diff {max_diff:.3} LSB", samples.len());
        assert!(max_diff <= 1.0, "{name}: max diff {max_diff} LSB");
    }
}

#[test]
fn verify_against_live_ffmpeg() {
    for name in
        ["clap.qcp", "0036580847.QCP", "test8.qcp", "13k_fixed_half.qcp", "13k_var_half.qcp"]
    {
        let Ok(output) = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(Path::new(FIXTURES).join(name))
            .args(["-f", "f32le", "-"])
            .output()
        else {
            eprintln!("ffmpeg not found, skipping");
            return;
        };
        if !output.status.success() {
            eprintln!("ffmpeg cannot decode {name}, skipping");
            return;
        }

        let reference: Vec<f32> = output
            .stdout
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();

        let samples = decode(name);
        assert_eq!(samples.len(), reference.len(), "{name}: length");

        let snr = snr_db(&samples, &reference);
        println!("{name}: {} samples, SNR {snr:.1} dB", samples.len());
        assert!(snr > 100.0, "{name}: SNR {snr} dB");
    }
}
