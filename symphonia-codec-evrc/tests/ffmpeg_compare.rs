#![allow(clippy::unwrap_used)]

//! The decoded output of QCP fixtures compared with the output of FFmpeg's `evrc` decoder.
//!
//! The decoder mirrors the floating point operations of the reference decoder, so the output must
//! match to within 1e-4: the committed 16-bit reference within 1 LSB, and the f32 output of
//! `ffmpeg`, if it is installed, sample by sample.

use std::path::Path;
use std::process::Command;

use symphonia_codec_evrc::EvrcDecoder;
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

    let mut decoder = EvrcDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
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

#[test]
fn verify_committed_reference() {
    let samples = decode("entersandman_head.qcp");
    let bytes = std::fs::read(Path::new(FIXTURES).join("entersandman_head.ref.s16")).unwrap();
    assert_eq!(bytes.len(), samples.len() * 2, "length");

    let mut max_diff = 0.0f64;
    for (s, r) in samples.iter().zip(bytes.chunks_exact(2)) {
        let r = f64::from(i16::from_le_bytes([r[0], r[1]]));
        max_diff = max_diff.max((f64::from(*s) * 32768.0 - r).abs());
    }
    println!("{} samples, max diff {max_diff:.3} LSB", samples.len());
    assert!(max_diff <= 1.0, "max diff {max_diff} LSB");
}

#[test]
fn verify_against_live_ffmpeg() {
    for name in ["entersandman_head.qcp", "lord_of_the_flies_head.qcp"] {
        let Ok(output) = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(Path::new(FIXTURES).join(name))
            .args(["-f", "f32le", "-"])
            .output()
        else {
            eprintln!("ffmpeg not found, skipping");
            return;
        };
        if output.stdout.is_empty() {
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

        let max_diff =
            samples.iter().zip(&reference).map(|(&s, &r)| (s - r).abs()).fold(0.0f32, f32::max);
        println!("{name}: {} samples, max diff {max_diff:e}", samples.len());
        assert!(max_diff < 1e-4, "{name}: max diff {max_diff}");
    }
}
