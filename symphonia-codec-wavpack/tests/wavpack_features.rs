// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests of the features of the reference library that are less common: false stereo, sample
//! rates that are not in the standard table, DSD audio, the CRC checks, and the MD5 of audio
//! whose source was not little-endian WAV audio.
//!
//! The fixtures were made with `wavpack`/`wvunpack` 5.9.0 (`-m` where an MD5 is stored), and the
//! `*_ref.raw` files are the `wvunpack --raw` (PCM: `-w` with the header removed) output.
//!   - `false_stereo_16`: a stereo WAV with identical channels, in three blocks.
//!   - `rate_1mhz_md5`: a 1 MHz WAV, which has an `ID_SAMPLE_RATE` sub-block.
//!   - `aiff_be_md5`: an AIFF (big-endian samples) source.
//!   - `dsd_stereo_{high,fast}_md5`: a DSF (LSB-first, 4096 byte blocks) source in DSD modes 3
//!     and 1.

use std::fs::File;

use symphonia_codec_wavpack::{WavPackDecoder, WavPackReader};
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::units::Timestamp;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn open(path: &str) -> WavPackReader<'static> {
    let file = File::open(path).unwrap_or_else(|e| panic!("open {path}: {e}"));
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    WavPackReader::try_new(mss, FormatOptions::default()).expect("reader")
}

fn params(reader: &WavPackReader<'_>) -> AudioCodecParameters {
    match &reader.tracks()[0].codec_params {
        Some(CodecParameters::Audio(a)) => a.clone(),
        _ => panic!("no audio codec params"),
    }
}

fn append(out: &mut Vec<u8>, buf: &GenericAudioBufferRef<'_>) {
    let n = buf.frames();
    let nch = buf.spec().channels().count();
    match buf {
        GenericAudioBufferRef::S16(b) => {
            for i in 0..n {
                for ch in 0..nch {
                    out.extend_from_slice(
                        &b.plane(ch).expect("unexpected failure")[i].to_le_bytes(),
                    );
                }
            }
        }
        GenericAudioBufferRef::U8(b) => {
            for i in 0..n {
                for ch in 0..nch {
                    out.push(b.plane(ch).expect("unexpected failure")[i]);
                }
            }
        }
        _ => panic!("unexpected sample format"),
    }
}

/// Decodes a whole file. Returns the interleaved little-endian bytes, the number of packets that
/// failed to decode, and the result of the MD5 verification.
fn decode(path: &str) -> (Vec<u8>, usize, Option<bool>) {
    let mut reader = open(path);
    let mut opts = AudioDecoderOptions::default();
    opts.verify = true;
    let mut decoder = WavPackDecoder::try_new(&params(&reader), &opts).expect("decoder");

    let mut out = Vec::new();
    let mut errors = 0;
    while let Some(packet) = reader.next_packet().expect("next_packet") {
        match decoder.decode_ref(&packet.as_packet_ref()) {
            Ok(buf) => append(&mut out, &buf),
            Err(_) => errors += 1,
        }
    }
    (out, errors, decoder.finalize().verify_ok)
}

fn reference(name: &str) -> Vec<u8> {
    std::fs::read(fixture(&format!("{name}_ref.raw"))).expect("reference")
}

#[test]
fn false_stereo_is_decoded_as_two_identical_channels() {
    let (out, errors, _) = decode(&fixture("false_stereo_16.wv"));
    assert_eq!(errors, 0);
    assert_eq!(out, reference("false_stereo_16"));

    // The file is stereo, even though only one channel is coded.
    let reader = open(&fixture("false_stereo_16.wv"));
    assert_eq!(params(&reader).channels.unwrap().count(), 2);
}

#[test]
fn sample_rate_outside_of_the_standard_table() {
    let path = fixture("rate_1mhz_md5.wv");
    let reader = open(&path);
    assert_eq!(params(&reader).sample_rate, Some(1_000_000));

    let (out, errors, verify) = decode(&path);
    assert_eq!(errors, 0);
    assert_eq!(out, reference("rate_1mhz_md5"));
    assert_eq!(verify, Some(true));
}

#[test]
fn md5_of_big_endian_source_is_verified() {
    let (out, errors, verify) = decode(&fixture("aiff_be_md5.wv"));
    assert_eq!(errors, 0);
    assert_eq!(out, reference("aiff_be_md5"));
    assert_eq!(verify, Some(true));
}

#[test]
fn dsd_is_decoded_to_bytes_and_md5_of_lsb_first_blocks_is_verified() {
    for name in ["dsd_stereo_high_md5", "dsd_stereo_fast_md5"] {
        let path = fixture(&format!("{name}.wv"));

        let reader = open(&path);
        let p = params(&reader);
        assert_eq!(p.channels.as_ref().unwrap().count(), 2, "{name}");
        // The track is in DSD bits: 2.8224 MHz, one bit per sample.
        assert_eq!(p.sample_rate, Some(2_822_400), "{name}");
        assert_eq!(p.bits_per_sample, Some(1), "{name}");
        assert_eq!(
            reader.tracks()[0].num_frames,
            Some(if name.contains("high") { 32768 } else { 16384 })
        );

        let (out, errors, verify) = decode(&path);
        assert_eq!(errors, 0, "{name}");
        assert_eq!(out, reference(name), "{name}");
        assert_eq!(verify, Some(true), "{name}");
    }
}

#[test]
fn dsd_seeks_by_bits() {
    let path = fixture("dsd_stereo_high_md5.wv");
    let (all, _, _) = decode(&path);

    let mut reader = open(&path);
    let mut decoder =
        WavPackDecoder::try_new(&params(&reader), &AudioDecoderOptions::default()).unwrap();

    // The 1000th byte of each channel (of 4096 per channel) is the bit 8000.
    let to = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(8000), track_id: 0 })
        .expect("seek");
    assert!(to.actual_ts.get() <= 8000);

    let mut out = Vec::new();
    while let Some(packet) = reader.next_packet().unwrap() {
        append(&mut out, &decoder.decode_ref(&packet.as_packet_ref()).unwrap());
    }
    let skip = (to.actual_ts.get() / 8) as usize * 2;
    assert_eq!(out, all[skip..]);
}

#[test]
fn corrupt_audio_is_a_decode_error() {
    let path = fixture("false_stereo_16.wv");
    let original = std::fs::read(&path).unwrap();

    // Flip a bit in the audio of the second and third blocks (a stream whose first block is
    // bad is not opened at all, as in libwavpack).
    for offset in [1200usize, 1500, 2500] {
        let mut data = original.clone();
        data[offset] ^= 0x10;

        let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(data)), Default::default());
        let mut reader = WavPackReader::try_new(mss, FormatOptions::default()).expect("reader");
        let mut decoder =
            WavPackDecoder::try_new(&params(&reader), &AudioDecoderOptions::default()).unwrap();

        let mut failures = 0;
        while let Some(packet) = reader.next_packet().expect("next_packet") {
            if decoder.decode_ref(&packet.as_packet_ref()).is_err() {
                failures += 1;
            }
        }
        assert!(failures > 0, "a flipped bit at {offset} went unnoticed");
    }
}

#[test]
fn truncated_stream_yields_the_complete_blocks_only() {
    let original = std::fs::read(fixture("false_stereo_16.wv")).unwrap();
    let reference = reference("false_stereo_16");

    for cut in [original.len() / 2, original.len() - 1, original.len() - 100] {
        let data = original[..cut].to_vec();
        let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(data)), Default::default());
        let mut reader = WavPackReader::try_new(mss, FormatOptions::default()).expect("reader");
        let mut decoder =
            WavPackDecoder::try_new(&params(&reader), &AudioDecoderOptions::default()).unwrap();

        let mut out = Vec::new();
        while let Some(packet) = reader.next_packet().expect("next_packet") {
            append(&mut out, &decoder.decode_ref(&packet.as_packet_ref()).expect("decode"));
        }
        assert!(out.len() < reference.len());
        assert_eq!(out, reference[..out.len()]);
    }
}
