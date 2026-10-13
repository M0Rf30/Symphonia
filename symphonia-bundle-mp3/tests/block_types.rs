// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Joint stereo streams in which the channels of a granule have different block types, and streams
//! with mixed blocks.
//!
//! No encoder produces these streams, so the fixtures are synthetic: MPEG1 layer 3 streams at 44.1
//! kHz, joint stereo, with randomly generated Huffman coded spectra, scale-factors, and block types
//! following the long, start, short, end sequence of each channel independently. The mode extension
//! cycles through all four combinations of mid-side and intensity stereo. A fixture (`.mp3`) is
//! decoded and compared with the decoded audio of ffmpeg (`.s16`, interleaved 16-bit samples, the
//! float decoder rounded to 16 bits), which agrees with mpg123 to within 1 LSB.

use std::io::Cursor;

use symphonia_bundle_mp3::{MpaDecoder, MpaReader};
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;

/// Decode a stream, and compare it with a reference of 16-bit samples.
fn verify_matches_reference(stream: &[u8], reference: &[u8]) {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(stream.to_vec())), Default::default());
    let mut reader = MpaReader::try_new(mss, FormatOptions::default()).unwrap();

    let params = reader.tracks()[0].codec_params.as_ref().unwrap().audio().unwrap();
    let mut decoder =
        MpaDecoder::try_new(params, &AudioDecoderOptions::default().gapless(false)).unwrap();

    let mut samples: Vec<f32> = Vec::new();
    let mut n_packets = 0;

    while let Some(packet) = reader.next_packet().unwrap() {
        let decoded = decoder.decode(&packet).unwrap();
        assert_eq!(decoded.spec().channels().count(), 2);

        let mut interleaved: Vec<f32> = Vec::new();
        decoded.copy_to_vec_interleaved(&mut interleaved);
        samples.extend_from_slice(&interleaved);

        n_packets += 1;
    }

    let reference: Vec<i16> =
        reference.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();

    assert_eq!(samples.len(), reference.len());
    assert_eq!(samples.len(), n_packets * 1152 * 2);

    // The decoder and ffmpeg both decode in 32-bit floats. The difference between the rounded
    // reference and the decoded sample is half a LSB at most (plus the float precision), and
    // anything beyond that is a different decode.
    let mut worst = (0usize, 0.0f32);

    for (i, (&sample, &reference)) in samples.iter().zip(&reference).enumerate() {
        let delta = (sample * 32768.0 - f32::from(reference)).abs();

        if delta > worst.1 {
            worst = (i, delta);
        }
    }

    assert!(
        worst.1 <= 0.6,
        "decoded sample {} (packet {}) is {} LSBs from the reference",
        worst.0,
        worst.0 / (1152 * 2),
        worst.1
    );
}

/// The channels of a granule in a joint stereo stream may have different block types (long, start,
/// short, and end blocks), in each of the mid-side and intensity stereo modes.
#[test]
fn verify_joint_stereo_with_differing_block_types() {
    verify_matches_reference(
        include_bytes!("fixtures/joint_stereo_block_types.mp3"),
        include_bytes!("fixtures/joint_stereo_block_types.s16"),
    );
}

/// Mixed blocks (a long block for the first two sub-bands, and short blocks for the rest) in
/// each of the mid-side and intensity stereo modes, in either or both of the channels.
#[test]
fn verify_mixed_blocks() {
    verify_matches_reference(
        include_bytes!("fixtures/joint_stereo_mixed_blocks.mp3"),
        include_bytes!("fixtures/joint_stereo_mixed_blocks.s16"),
    );
}
