// Symphonia Musepack oracle-comparison tests
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// These tests compare this crate's decode output against the reference `mpcdec` CLI (built from
// libmpcdec, BSD-3-Clause; see NOTICE), which is the correct oracle for Musepack -- unlike
// `ffmpeg`, whose own Musepack decoder does not trim the encoder's synthesis-filter delay the
// same way (verified: shifting ffmpeg's PCM output back by exactly `SYNTH_DELAY` (481) samples
// realigns it with `mpcdec`'s output; ffmpeg is therefore not a valid gapless-correctness oracle
// for this codec and is not used here).
//
// Both tests are skipped (not failed) when the external tool/sample they need isn't available,
// so `cargo test` remains runnable without network access or the oracle build.

use std::fs::File;
use std::path::Path;
use std::process::Command;

use symphonia_bundle_musepack::{MpcDecoder, MpcReader};
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;

/// Locates the `mpcdec` oracle binary via `MPCDEC_BIN`, falling back to the well-known path used
/// during this crate's development (see the final report / NOTICE for how it was built).
fn find_mpcdec() -> Option<String> {
    if let Ok(p) = std::env::var("MPCDEC_BIN") {
        if Path::new(&p).is_file() {
            return Some(p);
        }
    }
    const DEV_PATH: &str = "/tmp/mpc_oracle/build/codec/mpcdec/mpcdec";
    if Path::new(DEV_PATH).is_file() {
        return Some(DEV_PATH.to_string());
    }
    None
}

/// Decodes `path` with this crate, returning interleaved `i16` PCM (rounded, clamped) and the
/// channel count.
fn decode_ours_s16(path: &Path) -> (Vec<i16>, usize) {
    let file = File::open(path).expect("open");
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut reader = MpcReader::try_new(mss, FormatOptions::default()).expect("probe");
    let track = reader.tracks()[0].clone();
    let params = match track.codec_params.as_ref().unwrap() {
        CodecParameters::Audio(p) => p.clone(),
        _ => panic!("not audio"),
    };
    let mut decoder = MpcDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
    let mut out = Vec::new();
    let mut channels = 0usize;
    while let Some(packet) = reader.next_packet().expect("next_packet") {
        let buf = match decoder.decode(&packet) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if let GenericAudioBufferRef::F32(b) = buf {
            channels = b.spec().channels().count();
            for i in 0..b.frames() {
                for ch in 0..channels {
                    let s = b.plane(ch).unwrap()[i];
                    out.push((s.clamp(-1.0, 1.0) * 32767.0).round() as i16);
                }
            }
        }
    }
    (out, channels)
}

/// Reads a canonical 44-byte-header PCM WAV (as produced by `mpcdec`) as interleaved `i16`.
fn load_wav_s16(path: &Path) -> Vec<i16> {
    let data = std::fs::read(path).expect("read wav");
    // Locate the `data` chunk rather than assuming a fixed 44-byte header.
    let mut pos = 12usize; // past "RIFF"+size+"WAVE"
    let mut data_off = None;
    let mut data_len = 0usize;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let len = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        if id == b"data" {
            data_off = Some(pos + 8);
            data_len = len;
            break;
        }
        pos += 8 + len + (len & 1);
    }
    let off = data_off.expect("no data chunk");
    data[off..off + data_len]
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

/// Reports `(max_abs_err, first_diff_index, snr_db)` for two equal-or-different-length i16
/// streams (compared over the overlapping prefix).
fn compare_s16(ours: &[i16], reference: &[i16]) -> (i32, Option<usize>, f64, usize) {
    let n = ours.len().min(reference.len());
    let mut max_err = 0i32;
    let mut first_diff = None;
    let mut sum_sq_err = 0f64;
    let mut sum_sq_ref = 0f64;
    for i in 0..n {
        let d = (i32::from(ours[i]) - i32::from(reference[i])).abs();
        if d > max_err {
            max_err = d;
        }
        if d != 0 && first_diff.is_none() {
            first_diff = Some(i);
        }
        sum_sq_err += f64::from(d) * f64::from(d);
        sum_sq_ref += f64::from(reference[i]) * f64::from(reference[i]);
    }
    let snr = if sum_sq_err > 0.0 { 10.0 * (sum_sq_ref / sum_sq_err).log10() } else { f64::INFINITY };
    (max_err, first_diff, snr, ours.len().abs_diff(reference.len()))
}

#[test]
fn matches_mpcdec_oracle_sv8() {
    let Some(mpcdec) = find_mpcdec()
    else {
        eprintln!("skipping: mpcdec oracle binary not found (set MPCDEC_BIN)");
        return;
    };

    let fixture = Path::new("tests/fixture_stereo_q5.mpc");
    let tmp_wav = std::env::temp_dir().join("musepack_oracle_test_stereo_q5.wav");

    let status = Command::new(&mpcdec)
        .arg(fixture)
        .arg(&tmp_wav)
        .status()
        .expect("run mpcdec");
    assert!(status.success(), "mpcdec failed");

    let (ours, _channels) = decode_ours_s16(fixture);
    let reference = load_wav_s16(&tmp_wav);

    let (max_err, first_diff, snr, len_diff) = compare_s16(&ours, &reference);
    eprintln!(
        "stereo_q5: ours_len={} ref_len={} len_diff={len_diff} max_abs_err={max_err} first_diff={:?} snr_db={snr:.2}",
        ours.len(),
        reference.len(),
        first_diff
    );

    assert_eq!(len_diff, 0, "sample count must match mpcdec exactly (gapless)");
    // Allow +/-1 LSB: both sides independently round an f32 sample to i16.
    assert!(max_err <= 1, "max abs error vs mpcdec must be <=1 LSB, was {max_err}");
    assert!(snr > 60.0, "SNR vs mpcdec too low: {snr:.2} dB");

    let _ = std::fs::remove_file(&tmp_wav);
}

/// Validates SV7 decode against the real-world sample used during development. The sample
/// (`samples.ffmpeg.org/A-codecs/musepack/`, a commercial recording) is not redistributable and
/// is therefore never committed; this test only runs if a copy has been placed at
/// `MUSEPACK_SV7_SAMPLE` or the well-known dev path below, and is skipped otherwise.
#[test]
fn matches_mpcdec_oracle_sv7_real_sample() {
    let Some(mpcdec) = find_mpcdec()
    else {
        eprintln!("skipping: mpcdec oracle binary not found (set MPCDEC_BIN)");
        return;
    };

    let sample_path = std::env::var("MUSEPACK_SV7_SAMPLE")
        .unwrap_or_else(|_| "/tmp/sv7_samples/duran.mpc".to_string());
    let sample_path = Path::new(&sample_path);
    if !sample_path.is_file() {
        eprintln!("skipping: no real SV7 sample at {} (set MUSEPACK_SV7_SAMPLE)", sample_path.display());
        return;
    }

    let tmp_wav = std::env::temp_dir().join("musepack_oracle_test_sv7.wav");
    let status = Command::new(&mpcdec)
        .arg(sample_path)
        .arg(&tmp_wav)
        .status()
        .expect("run mpcdec");
    assert!(status.success(), "mpcdec failed");

    let (ours, _channels) = decode_ours_s16(sample_path);
    let reference = load_wav_s16(&tmp_wav);

    let (max_err, first_diff, snr, len_diff) = compare_s16(&ours, &reference);
    eprintln!(
        "SV7 real sample: ours_len={} ref_len={} len_diff={len_diff} max_abs_err={max_err} first_diff={:?} snr_db={snr:.2}",
        ours.len(),
        reference.len(),
        first_diff
    );

    assert_eq!(len_diff, 0, "sample count must match mpcdec exactly (gapless)");
    assert!(max_err <= 1, "max abs error vs mpcdec must be <=1 LSB, was {max_err}");
    assert!(snr > 60.0, "SNR vs mpcdec too low: {snr:.2} dB");

    let _ = std::fs::remove_file(&tmp_wav);
}

/// Verifies that decoding *after a seek* matches `mpcdec` decoding from the start, at the exact
/// sample the seek actually landed on (frame-accurate seeking may land at or before the
/// requested target -- see `demuxer::sv7`/`demuxer::sv8`'s `seek`). This directly checks the
/// "pre-roll" behaviour requested for review: there is no separate warm-up step because SV8 `AP`
/// blocks are independently decodable at their start (`is_key_frame`) and SV7 seeking resets SCF
/// state to the same neutral baseline `mpc_decoder_reset_scf` uses (see `decoder.rs::reset`), so
/// the check is that decode-after-seek reproduces the *same* reference samples decode-from-start
/// would produce at that position -- no discontinuity.
#[test]
fn seek_then_decode_matches_mpcdec_at_landed_sample() {
    let Some(mpcdec) = find_mpcdec()
    else {
        eprintln!("skipping: mpcdec oracle binary not found (set MPCDEC_BIN)");
        return;
    };

    use symphonia_core::formats::{SeekMode, SeekTo};
    use symphonia_core::units::Timestamp;

    let fixture = Path::new("tests/fixture_stereo_q5.mpc");
    let tmp_wav = std::env::temp_dir().join("musepack_oracle_test_seek.wav");
    let status = Command::new(&mpcdec).arg(fixture).arg(&tmp_wav).status().expect("run mpcdec");
    assert!(status.success());
    let reference = load_wav_s16(&tmp_wav);
    let _ = std::fs::remove_file(&tmp_wav);

    let file = File::open(fixture).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut reader = MpcReader::try_new(mss, FormatOptions::default()).unwrap();
    let track = reader.tracks()[0].clone();
    let params = match track.codec_params.as_ref().unwrap() {
        CodecParameters::Audio(p) => p.clone(),
        _ => panic!("not audio"),
    };
    let mut decoder = MpcDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();

    let target: i64 = 80_000;
    let seeked = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(target), track_id: 0 })
        .expect("seek");
    let landed = seeked.actual_ts.get();
    assert!(landed <= target, "frame-accurate seek must not land after the target");
    decoder.reset();

    let mut ours = Vec::new();
    while ours.len() < 20_000 {
        let Some(packet) = reader.next_packet().unwrap()
        else {
            break;
        };
        if let Ok(GenericAudioBufferRef::F32(b)) = decoder.decode(&packet) {
            for i in 0..b.frames() {
                for ch in 0..b.spec().channels().count() {
                    let s = b.plane(ch).unwrap()[i];
                    ours.push((s.clamp(-1.0, 1.0) * 32767.0).round() as i16);
                }
            }
        }
    }

    let channels = 2usize;
    let ref_start = (landed as usize) * channels;
    let ref_slice = &reference[ref_start..(ref_start + ours.len()).min(reference.len())];
    let (max_err, first_diff, snr, _) = compare_s16(&ours, ref_slice);
    eprintln!(
        "seek to {target}, landed {landed}: compared {} samples, max_abs_err={max_err} first_diff={:?} snr_db={snr:.2}",
        ours.len().min(ref_slice.len()),
        first_diff
    );
    assert!(max_err <= 1, "post-seek decode diverges from mpcdec-from-start: max_err={max_err}");
    assert!(snr > 60.0, "post-seek SNR too low: {snr:.2} dB");
}
