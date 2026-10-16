// Symphonia Musepack SV4-SV6 oracle-comparison tests
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// These tests compare this crate's SV4-SV6 decode output against the historical reference decoder
// `mppdec` (Klemm 1.1, 2002), the last decoder that handles the SV4, SV5 and SV6 stream versions
// (libmpcdec, the `mpcdec` oracle used by `oracle.rs`, rejects every SV4-SV6 file by design).
//
// Both tests are skipped (not failed) when the external tool/sample they need isn't available,
// so `cargo test` remains runnable without the oracle build.

use std::fs::File;
use std::path::Path;
use std::process::Command;

use symphonia_bundle_musepack::{MpcDecoder, MpcReader};
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;

/// Locates the `mppdec` oracle binary via `MPPDEC_BIN`, falling back to a `PATH` lookup.
fn find_mppdec() -> Option<String> {
    if let Ok(p) = std::env::var("MPPDEC_BIN") {
        if Path::new(&p).is_file() {
            return Some(p);
        }
    }
    let path = std::env::var_os("PATH")?;
    ["mppdec", "mppdec64"]
        .into_iter()
        .flat_map(|n| {
            let exe = format!("{n}{}", std::env::consts::EXE_SUFFIX);
            std::env::split_paths(&path).map(|dir| dir.join(&exe)).collect::<Vec<_>>()
        })
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
}

/// Decodes `path` with this crate, returning interleaved `i16` PCM (rounded, clamped).
fn decode_ours_s16(path: &Path) -> Vec<i16> {
    let file = File::open(path).expect("open");
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut reader = MpcReader::try_new(mss, FormatOptions::default()).expect("probe");
    let track = reader.tracks()[0].clone();
    let params = match track.codec_params.as_ref().expect("unexpected failure") {
        CodecParameters::Audio(p) => p.clone(),
        _ => panic!("not audio"),
    };
    let mut decoder =
        MpcDecoder::try_new(&params, &AudioDecoderOptions::default()).expect("unexpected failure");
    let mut out = Vec::new();
    while let Some(packet) = reader.next_packet().expect("next_packet") {
        let buf = match decoder.decode(&packet) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if let GenericAudioBufferRef::F32(b) = buf {
            let channels = b.spec().channels().count();
            for i in 0..b.frames() {
                for ch in 0..channels {
                    let s = b.plane(ch).expect("unexpected failure")[i];
                    out.push((s.clamp(-1.0, 1.0) * 32767.0).round() as i16);
                }
            }
        }
    }
    out
}

/// Reads a PCM WAV (as produced by `mppdec`) as interleaved `i16`.
fn load_wav_s16(path: &Path) -> Vec<i16> {
    let data = std::fs::read(path).expect("read wav");
    let mut pos = 12usize; // past "RIFF"+size+"WAVE"
    let mut data_off = None;
    let mut data_len = 0usize;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let len = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().expect("unexpected failure"))
            as usize;
        if id == b"data" {
            data_off = Some(pos + 8);
            data_len = len;
            break;
        }
        pos += 8 + len + (len & 1);
    }
    let off = data_off.expect("no data chunk");
    data[off..off + data_len].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect()
}

/// Decodes `path` with `mppdec`, asserts success, and returns the `i16` PCM.
fn decode_mppdec_s16(mppdec: &str, path: &Path) -> Vec<i16> {
    let tmp_wav = std::env::temp_dir().join(format!("mppdec_oracle_{}.wav", std::process::id()));
    let status =
        Command::new(mppdec).arg("--silent").arg(path).arg(&tmp_wav).status().expect("run mppdec");
    assert!(status.success(), "mppdec failed");
    let pcm = load_wav_s16(&tmp_wav);
    let _ = std::fs::remove_file(&tmp_wav);
    pcm
}

/// Compares `ours` against `reference`, asserting the same sample count and no difference larger
/// than 1 LSB (both sides independently round an f32 sample to i16).
fn assert_matches(ours: &[i16], reference: &[i16], name: &str) {
    assert_eq!(
        ours.len(),
        reference.len(),
        "{name}: sample count must match mppdec exactly (gapless)"
    );
    let mut max_err = 0i32;
    let mut sum_sq_err = 0f64;
    let mut sum_sq_ref = 0f64;
    for (o, r) in ours.iter().zip(reference) {
        let d = i32::from(*o) - i32::from(*r);
        max_err = max_err.max(d.abs());
        sum_sq_err += f64::from(d) * f64::from(d);
        sum_sq_ref += f64::from(*r) * f64::from(*r);
    }
    let snr =
        if sum_sq_err > 0.0 { 10.0 * (sum_sq_ref / sum_sq_err).log10() } else { f64::INFINITY };
    eprintln!("{name}: max_abs_err={max_err} snr_db={snr:.2}");
    assert!(max_err <= 1, "{name}: max abs error vs mppdec must be <=1 LSB, was {max_err}");
    assert!(snr > 60.0, "{name}: SNR vs mppdec too low: {snr:.2} dB");
}

/// Committed fixtures exercise intensity stereo (SV4/SV5), a fixed 256 kbit/s CBR stream (SV5)
/// and the SV6 stream version. Their SV4/SV5/SV6 bitstream layouts are identical, so SV6 shares
/// the SV4/SV5 coverage, and the fixtures cross-check the historical decoder's SV6 path.
const FIXTURES: &[&str] = &[
    "fixture_sv4_is.mpc",
    "fixture_sv5_is.mpc",
    "fixture_sv5_cbr256.mpc",
    "fixture_sv6_len5000_m1000.mpc",
    "fixture_sv6_len5000_m300.mpc",
];

#[test]
fn matches_mppdec_oracle_on_committed_sv46_fixtures() {
    let Some(mppdec) = find_mppdec()
    else {
        eprintln!("skipping: mppdec oracle binary not found (set MPPDEC_BIN)");
        return;
    };

    for name in FIXTURES {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join(name);
        let ours = decode_ours_s16(&fixture);
        let reference = decode_mppdec_s16(&mppdec, &fixture);
        assert_matches(&ours, &reference, name);
    }
}

/// Validates SV4 and SV5 decode against Frank Klemm's real `test_sv4.mpc` / `test_sv5.mpc`
/// (archived with the mppdec 1.1 source; see the issue tracker). They are not redistributable and
/// are therefore never committed; this test only runs if the corpus (`RMPD_SAMPLES`) holds
/// `musepack/test_sv4.mpc` and `musepack/test_sv5.mpc`.
#[test]
fn matches_mppdec_oracle_on_real_sv45_samples() {
    let Some(mppdec) = find_mppdec()
    else {
        eprintln!("skipping: mppdec oracle binary not found (set MPPDEC_BIN)");
        return;
    };

    for name in ["musepack/test_sv4.mpc", "musepack/test_sv5.mpc"] {
        let Some(path) = symphonia_testutil::sample_path(name).filter(|p| p.is_file())
        else {
            eprintln!("skipping: no real SV4/SV5 sample ({name})");
            continue;
        };
        let ours = decode_ours_s16(&path);
        let reference = decode_mppdec_s16(&mppdec, &path);
        assert_matches(&ours, &reference, name);
    }
}
