// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![cfg(feature = "mp3pro")]
#![allow(clippy::unwrap_used)]

//! Gapless playback with the mp3PRO decoder.
//!
//! The SBR output lags the core by 962 samples at the output rate (481 core samples). With
//! gapless playback enabled, the decoder removes the delay and padding of the stream (counted in
//! core samples, from the LAME tag) and this lag, so the audio it outputs is the audio between
//! the delay and padding, aligned with the core. The fixture is an MPEG-2 22.05 kHz mono LAME
//! stream (delay 1105, padding 542, 11025 frames): its output rate is 44.1 kHz.

use std::io::Cursor;

use symphonia_bundle_mp3::{Mp3ProDecoder, MpaReader};
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;

const MONO_22K: &[u8] = include_bytes!("fixtures/mp3_mpeg2_mono_22k.mp3");

/// The delay of the SBR output, in samples at the output rate.
const SBR_DELAY: usize = 962;

/// Decodes the stream. Returns the samples, and the delay and number of frames of the track in
/// core samples.
fn decode(gapless: bool) -> (Vec<f32>, usize, usize) {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(MONO_22K.to_vec())), Default::default());
    let mut reader = MpaReader::try_new(mss, FormatOptions::default()).unwrap();

    let track = &reader.tracks()[0];
    let (delay, frames) = (track.delay.unwrap() as usize, track.num_frames.unwrap() as usize);
    let params = track.codec_params.as_ref().unwrap().audio().unwrap().clone();

    let mut decoder =
        Mp3ProDecoder::try_new(&params, &AudioDecoderOptions::default().gapless(gapless)).unwrap();

    let mut samples = Vec::new();

    while let Some(packet) = reader.next_packet().unwrap() {
        let decoded = decoder.decode(&packet).unwrap();
        assert_eq!(decoded.spec().rate(), 44100);

        let mut buf = Vec::new();
        decoded.copy_to_vec_interleaved(&mut buf);
        samples.extend_from_slice(&buf);
    }

    (samples, delay, frames)
}

#[test]
fn verify_gapless_removes_delay_padding_and_sbr_delay() {
    let (full, delay, frames) = decode(false);
    let (trimmed, ..) = decode(true);

    assert_eq!((delay, frames), (1105, 11025));
    assert_eq!(full.len() % 1152, 0);

    // The padding is longer than the SBR delay, so all of the audio is decoded.
    assert_eq!(trimmed.len(), 2 * frames);

    let start = 2 * delay + SBR_DELAY;
    assert_eq!(trimmed, full[start..start + 2 * frames]);
}
