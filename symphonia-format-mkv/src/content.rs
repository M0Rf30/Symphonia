// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Undoing the `ContentEncodings` (compression) of the frames of a track.

use symphonia_core::errors::Result;

use crate::inflate::zlib_decompress;
use crate::segment::{COMP_ALGO_HEADER_STRIPPING, COMP_ALGO_ZLIB, ContentEncodingElement};

/// The `ContentEncodingScope` bit of the frames of a track.
const SCOPE_FRAMES: u64 = 1;
/// The `ContentEncodingScope` bit of the `CodecPrivate` of a track.
const SCOPE_CODEC_PRIVATE: u64 = 2;

#[derive(Debug)]
enum Step {
    /// Header stripping: the bytes removed from the start of every frame.
    Prepend(Box<[u8]>),
    /// Zlib compression.
    Zlib,
}

/// The steps that recover the original contents of a track's frames.
#[derive(Debug, Default)]
pub(crate) struct ContentDecoder {
    steps: Vec<Step>,
    /// The contents cannot be recovered (encryption, or a compression algorithm that is not
    /// supported).
    unsupported: bool,
}

impl ContentDecoder {
    /// Create the decoder of the contents in `scope` from the encodings of a track.
    fn new(encodings: &[ContentEncodingElement], scope: u64) -> Self {
        let mut applicable: Vec<&ContentEncodingElement> =
            encodings.iter().filter(|enc| enc.scope & scope != 0).collect();

        // Encodings are undone in the reverse of the order they were applied in.
        applicable.sort_by_key(|enc| std::cmp::Reverse(enc.order));

        let mut decoder = ContentDecoder::default();

        for enc in applicable {
            match (enc.encrypted, enc.comp_algo) {
                (false, COMP_ALGO_ZLIB) => decoder.steps.push(Step::Zlib),
                (false, COMP_ALGO_HEADER_STRIPPING) => {
                    decoder.steps.push(Step::Prepend(enc.comp_settings.clone().unwrap_or_default()))
                }
                // Encryption, bzlib (1), lzo (2), and unknown algorithms.
                _ => decoder.unsupported = true,
            }
        }

        decoder
    }

    /// Create the decoder of the frames of a track.
    pub(crate) fn for_frames(encodings: &[ContentEncodingElement]) -> Self {
        Self::new(encodings, SCOPE_FRAMES)
    }

    /// Create the decoder of the `CodecPrivate` of a track.
    pub(crate) fn for_codec_private(encodings: &[ContentEncodingElement]) -> Self {
        Self::new(encodings, SCOPE_CODEC_PRIVATE)
    }

    /// `true` if the contents cannot be recovered.
    pub(crate) fn is_unsupported(&self) -> bool {
        self.unsupported
    }

    /// `true` if the contents are not encoded.
    pub(crate) fn is_identity(&self) -> bool {
        self.steps.is_empty() && !self.unsupported
    }

    /// Recover the original contents of `data`.
    pub(crate) fn decode(&self, data: Box<[u8]>) -> Result<Box<[u8]>> {
        self.steps.iter().try_fold(data, |data, step| match step {
            Step::Prepend(prefix) if prefix.is_empty() => Ok(data),
            Step::Prepend(prefix) => Ok([&prefix[..], &data[..]].concat().into_boxed_slice()),
            Step::Zlib => zlib_decompress(&data).map(Vec::into_boxed_slice),
        })
    }
}
