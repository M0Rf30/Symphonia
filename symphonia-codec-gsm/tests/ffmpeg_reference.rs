// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Decodes GSM 06.10 fixtures (made with FFmpeg's `libgsm` / `libgsm_ms` encoders, see
//! `tests/fixtures/README.md`) and compares the PCM bit-exactly with the FFmpeg decoders
//! (`gsm`, `gsm_ms`): against a pinned FNV-1a hash always, and live when `ffmpeg` is on `PATH`.

use std::path::{Path, PathBuf};
use std::process::Command;

use symphonia_codec_gsm::GsmDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_GSM, CODEC_ID_GSM_MS};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia_core::packet::PacketRef;
use symphonia_format_riff::WavReader;
use symphonia_testutil::{Fnv, open_mss};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn push(buf: GenericAudioBufferRef<'_>, out: &mut Vec<i16>) {
    let GenericAudioBufferRef::S16(b) = buf
    else {
        panic!("expected s16 output")
    };
    let mut tmp = Vec::new();
    b.copy_to_vec_interleaved(&mut tmp);
    out.extend_from_slice(&tmp);
}

/// Decodes a WAV file through the WAV demuxer.
fn decode_wav(path: &Path) -> Vec<i16> {
    let mut reader = WavReader::try_new(open_mss(path), FormatOptions::default()).unwrap();
    let track = reader.default_track(TrackType::Audio).unwrap();
    let Some(CodecParameters::Audio(params)) = &track.codec_params
    else {
        panic!("no audio")
    };
    assert_eq!(params.codec, CODEC_ID_GSM_MS);
    assert_eq!(params.sample_rate, Some(8000));
    assert_eq!(params.channels.as_ref().unwrap().count(), 1);
    let params = params.clone();

    let mut dec = GsmDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
    let mut out = Vec::new();
    while let Some(pkt) = reader.next_packet().unwrap() {
        push(dec.decode(&pkt).unwrap(), &mut out);
    }
    out
}

/// Decodes a raw `.gsm` file (a sequence of 33-byte blocks), one block per packet.
fn decode_raw(path: &Path) -> Vec<i16> {
    let data = std::fs::read(path).unwrap();
    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_GSM).with_sample_rate(8000);
    let mut dec = GsmDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
    let mut out = Vec::new();
    for block in data.chunks_exact(33) {
        let pkt = PacketRef::new(0, Default::default(), Default::default(), block);
        push(dec.decode_ref(&pkt).unwrap(), &mut out);
    }
    out
}

/// Decodes with FFmpeg, returns `None` if ffmpeg is unavailable.
fn ffmpeg(path: &Path, raw_gsm: bool) -> Option<Vec<i16>> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error"]);
    if raw_gsm {
        cmd.args(["-f", "gsm"]);
    }
    let out = cmd.arg("-i").arg(path).args(["-f", "s16le", "-"]).output().ok()?;
    out.status
        .success()
        .then(|| out.stdout.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect())
}

/// Decodes a raw `.gsm` file with the libgsm reference tool `toast`, `None` if it is unavailable.
fn toast(path: &Path) -> Option<Vec<i16>> {
    let out = Command::new("toast").args(["-d", "-l", "-c"]).arg(path).output().ok()?;
    out.status
        .success()
        .then(|| out.stdout.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect())
}

fn max_diff(a: &[i16], b: &[i16]) -> Option<i32> {
    assert_eq!(a.len(), b.len(), "length differs");
    a.iter().zip(b).map(|(a, b)| (i32::from(*a) - i32::from(*b)).abs()).max()
}

/// `ffmpeg_exact` is false for fixtures that drive the decoder into 16-bit saturation: FFmpeg
/// wraps there while the standard (and libgsm, which this decoder matches) saturates.
fn check(name: &str, frames: usize, hash: u64, ffmpeg_exact: bool) {
    let path = fixture(name);
    let raw = name.ends_with(".gsm");
    let pcm = if raw { decode_raw(&path) } else { decode_wav(&path) };

    let mut fnv = Fnv::new();
    for s in &pcm {
        fnv.write(&s.to_le_bytes());
    }
    if std::env::var_os("GOLDEN_PRINT").is_some() {
        println!("{name}: {} samples, hash {:#018x}", pcm.len(), fnv.finish());
    }
    assert_eq!(pcm.len(), frames, "{name}: sample count");
    assert_eq!(fnv.finish(), hash, "{name}: hash");

    if let Some(reference) = ffmpeg(&path, raw) {
        let diff = max_diff(&pcm, &reference);
        if ffmpeg_exact {
            assert_eq!(diff, Some(0), "{name}: differs from ffmpeg");
        }
        else {
            assert_ne!(diff, Some(0), "{name}: expected the known FFmpeg saturation difference");
        }
    }

    // libgsm is the reference of the standard and is exact for every fixture.
    if raw {
        if let Some(reference) = toast(&path) {
            assert_eq!(max_diff(&pcm, &reference), Some(0), "{name}: differs from libgsm");
        }
    }
}

#[test]
fn verify_ms_speech() {
    check("ms_speech.wav", 32000, 0xb8e80c5cf35020b0, true);
}

#[test]
fn verify_ms_loud() {
    check("ms_loud.wav", 16000, 0x863c0027782c437c, true);
}

#[test]
fn verify_ms_noise() {
    check("ms_noise.wav", 8000, 0x5aaba035198b17a6, false);
}

#[test]
fn verify_ms_sweep() {
    check("ms_sweep.wav", 16000, 0x5f942b380a1f164e, true);
}

#[test]
fn verify_fr_speech() {
    check("fr_speech.gsm", 32000, 0xb8e80c5cf35020b0, true);
}

#[test]
fn verify_fr_loud() {
    check("fr_loud.gsm", 16000, 0x863c0027782c437c, true);
}

#[test]
fn verify_fr_noise() {
    check("fr_noise.gsm", 8000, 0x5aaba035198b17a6, false);
}

#[test]
fn verify_fr_sweep() {
    check("fr_sweep.gsm", 16000, 0x5f942b380a1f164e, true);
}

/// The real-world 30 s Microsoft GSM sample of the RMPD corpus (`RMPD_SAMPLES`).
#[test]
fn verify_sample_wav_gsm_ms() {
    let Some(path) = symphonia_testutil::sample_path("wav/wav_gsm_ms.wav")
    else {
        return;
    };
    let pcm = decode_wav(&path);
    assert_eq!(pcm.len(), 240000);
    if let Some(reference) = ffmpeg(&path, false) {
        assert_eq!(max_diff(&pcm, &reference), Some(0));
    }
}

/// Random but well-framed blocks exercise the error-concealing paths of the standard (long-term
/// lags outside 40..=120 keep the previous lag) and 16-bit saturation everywhere. Only libgsm
/// defines the behaviour there, FFmpeg differs, so the output is pinned and compared with `toast`.
#[test]
fn verify_random_blocks_against_libgsm() {
    let mut rng = symphonia_testutil::XorShift64(0x6d73_6721_5eed_c0de);
    let mut data = vec![0u8; 33 * 600];
    for block in data.chunks_exact_mut(33) {
        for b in block.iter_mut() {
            *b = (rng.next_u64() >> 24) as u8;
        }
        block[0] = 0xd0 | (block[0] & 0x0f);
    }

    let path =
        std::env::temp_dir().join(format!("symphonia_gsm_random_{}.gsm", std::process::id()));
    std::fs::write(&path, &data).unwrap();
    let pcm = decode_raw(&path);
    let reference = toast(&path);
    std::fs::remove_file(&path).ok();

    let mut fnv = Fnv::new();
    for s in &pcm {
        fnv.write(&s.to_le_bytes());
    }
    if std::env::var_os("GOLDEN_PRINT").is_some() {
        println!("random: {} samples, hash {:#018x}", pcm.len(), fnv.finish());
    }
    assert_eq!(pcm.len(), 600 * 160);
    assert_eq!(fnv.finish(), 0x423b15141d522225);
    if let Some(reference) = reference {
        assert_eq!(max_diff(&pcm, &reference), Some(0));
    }
}

/// A packet of several blocks decodes exactly like the blocks as separate packets.
#[test]
fn verify_multi_block_packets() {
    for (codec, name, block_len) in
        [(CODEC_ID_GSM, "fr_speech.gsm", 33usize), (CODEC_ID_GSM_MS, "ms_speech.wav", 65)]
    {
        let mut data = std::fs::read(fixture(name)).unwrap();
        if codec == CODEC_ID_GSM_MS {
            // Skip the RIFF header: the data follows the `data` chunk id and size.
            let pos = data.windows(4).position(|w| w == b"data").unwrap() + 8;
            data.drain(..pos);
        }
        // 100 blocks: divisible by the packet sizes below.
        data.truncate(block_len * 100);
        let mut params = AudioCodecParameters::new();
        params.for_codec(codec).with_sample_rate(8000);

        let decode = |chunk_blocks: usize| {
            let mut dec = GsmDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
            let mut out = Vec::new();
            for chunk in data.chunks_exact(block_len * chunk_blocks) {
                let pkt = PacketRef::new(0, Default::default(), Default::default(), chunk);
                push(dec.decode_ref(&pkt).unwrap(), &mut out);
            }
            out
        };

        let single = decode(1);
        assert_eq!(single, decode(20), "{name}");
        assert_eq!(single, decode(50), "{name}");
    }
}
