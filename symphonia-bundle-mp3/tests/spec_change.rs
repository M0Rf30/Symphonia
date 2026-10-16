// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Streams whose signal specification (sample rate or channel count) changes mid-stream, such as
//! concatenated MP3 files. The decoder reports `ResetRequired` at the change and then decodes the
//! new stream from a clean state, like the decode of that stream on its own.

use std::io::Cursor;

#[cfg(feature = "mp3pro")]
use symphonia_bundle_mp3::Mp3ProDecoder;
use symphonia_bundle_mp3::{MpaDecoder, MpaReader};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::errors::Error;
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;

/// 44.1 kHz stereo MPEG-1 Layer III, 128 kbps.
const STEREO_44K: &[u8] = include_bytes!("fixtures/mp3_cbr128_st_44k_notag.mp3");
/// 22.05 kHz mono MPEG-2 Layer III.
const MONO_22K: &[u8] = include_bytes!("fixtures/mp3_mpeg2_mono_22k.mp3");
/// 44.1 kHz mono MPEG-1 Layer III.
#[cfg(feature = "mp3pro")]
const MONO_44K: &[u8] = include_bytes!("fixtures/mp3_mpeg1_mono_44k.mp3");

/// The samples of one run of frames with the same signal specification.
#[derive(Debug, PartialEq)]
struct Run {
    rate: u32,
    channels: usize,
    samples: Vec<f32>,
}

fn reader(stream: &[u8]) -> MpaReader<'static> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(stream.to_vec())), Default::default());
    MpaReader::try_new(mss, FormatOptions::default()).unwrap()
}

/// Creates the decoder under test from the codec parameters of the stream.
type MakeDecoder = fn(&AudioCodecParameters) -> Box<dyn AudioDecoder>;

fn mpa(params: &AudioCodecParameters) -> Box<dyn AudioDecoder> {
    Box::new(MpaDecoder::try_new(params, &AudioDecoderOptions::default().gapless(false)).unwrap())
}

#[cfg(feature = "mp3pro")]
fn mp3pro(params: &AudioCodecParameters) -> Box<dyn AudioDecoder> {
    Box::new(
        Mp3ProDecoder::try_new(params, &AudioDecoderOptions::default().gapless(false)).unwrap(),
    )
}

/// Decode a stream into runs. A `ResetRequired` ends the current run. If `retry` is set, the
/// packet that caused it is decoded again, otherwise it is discarded. Returns the runs, the number
/// of `ResetRequired` errors, and the codec parameters of the decoder after the last packet.
fn decode_runs(
    make: MakeDecoder,
    stream: &[u8],
    retry: bool,
) -> (Vec<Run>, usize, (Option<u32>, Option<usize>)) {
    let mut reader = reader(stream);
    let mut decoder = make(reader.tracks()[0].codec_params.as_ref().unwrap().audio().unwrap());

    let mut runs: Vec<Run> = Vec::new();
    let mut resets = 0;
    // A reset always starts a new run, even if the output specification stays the same.
    let mut split = false;

    while let Some(packet) = reader.next_packet().unwrap() {
        let mut result = decoder.decode(&packet).map(|_| ());

        if let Err(Error::ResetRequired) = result {
            resets += 1;
            split = true;
            // The decoded audio must not be left over from before the reset.
            assert_eq!(decoder.last_decoded().frames(), 0);
            result = if retry { decoder.decode(&packet).map(|_| ()) } else { Ok(()) };

            if !retry {
                continue;
            }
        }
        result.unwrap();

        let decoded = decoder.last_decoded();
        let (rate, channels) = (decoded.spec().rate(), decoded.spec().channels().count());

        if split || runs.last().is_none_or(|run| run.rate != rate || run.channels != channels) {
            runs.push(Run { rate, channels, samples: Vec::new() });
            split = false;
        }

        let mut buf = Vec::new();
        decoded.copy_to_vec_interleaved(&mut buf);
        runs.last_mut().unwrap().samples.extend_from_slice(&buf);
    }

    let params = decoder.codec_params();
    (runs, resets, (params.sample_rate, params.channels.as_ref().map(|c| c.count())))
}

fn concat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

#[test]
fn verify_spec_change_requires_reset_and_restarts_cleanly() {
    let (alone_a, resets, _) = decode_runs(mpa, STEREO_44K, true);
    assert_eq!(resets, 0);
    let (alone_b, resets, _) = decode_runs(mpa, MONO_22K, true);
    assert_eq!(resets, 0);
    let (a, b) = (&alone_a[0], &alone_b[0]);
    assert_eq!((a.rate, a.channels, b.rate, b.channels), (44100, 2, 22050, 1));

    for retry in [true, false] {
        let (runs, resets, params) = decode_runs(mpa, &concat(&[STEREO_44K, MONO_22K]), retry);

        assert_eq!(resets, 1, "retry={retry}");
        assert_eq!(runs.len(), 2, "retry={retry}");
        assert_eq!(params, (Some(22050), Some(1)));

        // The first run is the first stream (the last frame may be lost at the junction).
        assert_eq!((runs[0].rate, runs[0].channels), (44100, 2));
        assert!(a.samples.starts_with(&runs[0].samples));
        assert!(runs[0].samples.len() + 2 * 1152 >= a.samples.len());

        // The second run is the second stream. If the decode was retried, it is decoded as if it
        // were on its own. Otherwise the frame that caused the reset is lost, so the following
        // frame lacks its overlap and bit reservoir, and only the spec and length are checked.
        assert_eq!((runs[1].rate, runs[1].channels), (22050, 1));

        if retry {
            assert!(b.samples.ends_with(&runs[1].samples));
            assert!(runs[1].samples.len() + 2 * 576 >= b.samples.len());
        }
        else {
            assert!(runs[1].samples.len() + 3 * 576 >= b.samples.len());
        }
    }
}

#[test]
fn verify_channel_count_change_requires_reset() {
    // Mono at 22.05 kHz, then stereo at 44.1 kHz, then mono again.
    let (runs, resets, _) = decode_runs(mpa, &concat(&[MONO_22K, STEREO_44K, MONO_22K]), true);
    assert_eq!(resets, 2);
    let layout: Vec<_> = runs.iter().map(|run| (run.rate, run.channels)).collect();
    assert_eq!(layout, [(22050, 1), (44100, 2), (22050, 1)]);
}

/// The core of an mp3PRO stream with SBR (MPEG-2 at 22.05 kHz) and the core of a plain MP3 stream
/// (MPEG-1 at 44.1 kHz) output the same specification, 44.1 kHz mono, but the second stream cannot
/// continue the decoder state of the first.
#[cfg(feature = "mp3pro")]
#[test]
fn verify_mp3pro_core_change_with_same_output_spec_requires_reset() {
    let (alone, resets, _) = decode_runs(mp3pro, MONO_44K, true);
    assert_eq!(resets, 0);
    let (sbr, resets, _) = decode_runs(mp3pro, MONO_22K, true);
    assert_eq!(resets, 0);
    assert_eq!(alone.len() + sbr.len(), 2);
    assert_eq!(
        (alone[0].rate, alone[0].channels, sbr[0].rate, sbr[0].channels),
        (44100, 1, 44100, 1)
    );

    let (runs, resets, params) = decode_runs(mp3pro, &concat(&[MONO_22K, MONO_44K]), true);
    assert_eq!(resets, 1);
    assert_eq!(runs.len(), 2);
    assert_eq!(params, (Some(44100), Some(1)));

    assert!(sbr[0].samples.starts_with(&runs[0].samples));
    assert!(runs[0].samples.len() + 2 * 1152 >= sbr[0].samples.len());

    // The plain stream is decoded as if it were on its own.
    assert!(alone[0].samples.ends_with(&runs[1].samples));
    assert!(runs[1].samples.len() + 2 * 1152 >= alone[0].samples.len());
}

/// The output rate of an mp3PRO stream is twice the core rate, and the decoder reports the
/// change of the core specification like [`MpaDecoder`].
#[cfg(feature = "mp3pro")]
#[test]
fn verify_mp3pro_spec_change_requires_reset() {
    let (runs, resets, params) = decode_runs(mp3pro, &concat(&[STEREO_44K, MONO_22K]), true);
    assert_eq!(resets, 1);
    let layout: Vec<_> = runs.iter().map(|run| (run.rate, run.channels)).collect();
    assert_eq!(layout, [(44100, 2), (44100, 1)]);
    assert_eq!(params, (Some(22050), Some(1)));

    // Decoded as if it were on its own.
    let (alone, ..) = decode_runs(mp3pro, MONO_22K, true);
    assert!(alone[0].samples.ends_with(&runs[1].samples));
    assert!(runs[1].samples.len() + 4 * 576 >= alone[0].samples.len());
}
