// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Decodes the FFmpeg `A-codecs/QDM2` samples (from `RMPD_SAMPLES`, in the `qdesign` directory,
//! skipped when absent) and compares the output with FFmpeg's own `qdm2` decoder, run live
//! (skipped when `ffmpeg` is not on `PATH`). The decoder is floating point internally, so the
//! comparison is an SNR / maximum error tolerance, not bit for bit (`symphonia-codec-amrwb`'s
//! `tests/decode.rs` uses the same pattern).
//!
//! The six fixtures are `samples.ffmpeg.org/A-codecs/QDM2/*.mov`, unmodified. FFmpeg's `mov`
//! demuxer trims leading/trailing samples per the edit list the same way our decoder does with
//! `gapless(true)`, but by a slightly different amount at the very end of the file (at most 33
//! frames out of millions, in the files tested): the comparison aligns on the shorter of the two
//! and requires the length difference to be small.
//!
//! QDMC is not covered here: its container mapping is exercised by `qdmc_extra_data_is_accepted`
//! below, but its synthesis is not bit- or SNR-correct against FFmpeg yet (see issue #16); adding
//! a correctness test for it is left for when that is fixed.

use std::process::{Command, Stdio};

use symphonia_codec_qdesign::QdesignDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_QDMC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia_format_isomp4::IsoMp4Reader;
use symphonia_testutil::{open_sample, sample_path};

/// Decodes the default audio track of `qdesign/{name}` with Symphonia, as interleaved `i16`.
fn decode(name: &str) -> Option<Vec<i16>> {
    let mss = open_sample(format!("qdesign/{name}"))?;
    let mut reader = IsoMp4Reader::try_new(mss, FormatOptions::default()).unwrap();
    let track = reader.default_track(TrackType::Audio).unwrap();
    let id = track.id;
    let Some(CodecParameters::Audio(params)) = track.codec_params.clone()
    else {
        panic!("not audio");
    };
    let opts = AudioDecoderOptions::default().gapless(true);
    let mut dec = QdesignDecoder::try_new(&params, &opts).unwrap();

    let mut out = Vec::new();
    while let Ok(Some(packet)) = reader.next_packet() {
        if packet.track_id != id {
            continue;
        }
        // As FFmpeg, skip the packets that are too short to hold a frame.
        let b = match dec.decode(&packet) {
            Ok(GenericAudioBufferRef::S16(b)) => b,
            Ok(_) => panic!("unexpected sample format"),
            Err(e) => {
                eprintln!("{name}: {e}");
                continue;
            }
        };
        let mut v = Vec::new();
        b.copy_to_vec_interleaved::<i16>(&mut v);
        out.extend_from_slice(&v);
    }
    Some(out)
}

/// Decodes the default audio track of `qdesign/{name}` with FFmpeg, as interleaved `i16`, if
/// `ffmpeg` is on `PATH`.
fn ffmpeg_decode(name: &str) -> Option<Vec<i16>> {
    let path = sample_path(format!("qdesign/{name}"))?;
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-f", "s16le", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(out.stdout.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect())
}

/// Returns (max abs difference in 16-bit LSBs, SNR in dB) over the common prefix of `got` and
/// `want`, and the absolute difference in their lengths (in frames).
fn compare(got: &[i16], want: &[i16]) -> (i32, f64, usize) {
    let n = got.len().min(want.len());
    let mut max = 0i32;
    let (mut sig, mut err) = (0.0f64, 0.0f64);
    for (&g, &w) in got[..n].iter().zip(&want[..n]) {
        let d = i32::from(g) - i32::from(w);
        max = max.max(d.abs());
        sig += f64::from(w) * f64::from(w);
        err += f64::from(d) * f64::from(d);
    }
    let snr = if err == 0.0 { f64::INFINITY } else { 10.0 * (sig / err).log10() };
    (max, snr, got.len().abs_diff(want.len()))
}

/// Maximum accepted difference to FFmpeg, in 16-bit LSBs. FFmpeg's own float summation order
/// (SIMD) changes rounding by one unit now and then; measured: <= 1.
const MAX_LSB: i32 = 2;
/// Minimum accepted SNR against FFmpeg, in dB. Measured: >= 98.7.
const MIN_SNR: f64 = 90.0;
/// Maximum accepted length difference, in frames, from the edit-list trim rounding described in
/// the module docs. Measured: <= 1874 (on a ~5M-frame file).
const MAX_LEN_DIFF: usize = 4000;

#[test]
fn qdm2_matches_ffmpeg() {
    let mut any_ran = false;

    for name in [
        "switzler084d_dl.mov",
        "startupsound.mov",
        "reason_300.mov",
        "doom-tlr_m480.mov",
        "menace_640qt4.mov",
        "the_recruit_m480.mov",
    ] {
        let Some(got) = decode(name)
        else {
            eprintln!("RMPD_SAMPLES not set or {name} missing, skipping");
            continue;
        };
        let Some(want) = ffmpeg_decode(name)
        else {
            eprintln!("ffmpeg not available, skipping {name}");
            continue;
        };

        let (max, snr, len_diff) = compare(&got, &want);
        eprintln!(
            "{name}: {} samples, max diff {max} LSB, SNR {snr:.1} dB, length diff {len_diff}",
            got.len().min(want.len())
        );
        assert!(max <= MAX_LSB, "{name}: max diff {max} LSB");
        assert!(snr >= MIN_SNR, "{name}: SNR {snr} dB");
        assert!(len_diff <= MAX_LEN_DIFF, "{name}: length diff {len_diff} frames");
        any_ran = true;
    }

    if !any_ran {
        eprintln!("no QDM2 samples and/or ffmpeg available, test did not verify anything");
    }
}

/// The `QDCA` atom FFmpeg writes directly inside a `glbl` atom when remuxing a QDMC track,
/// without the `wave`/`frma` wrapper QuickTime itself uses (see `symphonia-format-isomp4`'s
/// `AtomType::GlobalHeader` handling). Mono, 22050 Hz, arbitrary bit rate/group size, FFT size 64
/// (order 7, the smallest `Qdmc::new` accepts).
const QDMC_BARE_QDCA_EXTRA_DATA: [u8; 48] = [
    0, 0, 0, 0x2c, b'Q', b'D', b'C', b'A', 0, 0, 0, 1, // size, tag, version
    0, 0, 0, 1, // channels
    0, 0, 0x56, 0x22, // sample rate (22050)
    0, 0, 0x40, 0, // bit rate
    0, 0, 0, 0x14, // group size
    0, 0, 0, 0x40, // FFT size (64)
    0, 0, 0, 0, // checksum size
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // padding so the buffer is at least 48 bytes long
];

/// A `QDesignDecoder` accepts extra data that is a bare `QDCA` atom (as produced by the `glbl`
/// atom path), not only one wrapped in `frma`.
#[test]
fn qdmc_extra_data_is_accepted() {
    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_QDMC).with_extra_data(QDMC_BARE_QDCA_EXTRA_DATA.to_vec().into());
    QdesignDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
}
