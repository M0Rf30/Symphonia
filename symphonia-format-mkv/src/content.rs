// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Undoing the `ContentEncodings` (compression) of the frames of a track.

use symphonia_core::errors::Result;

use crate::bzip2::bzip2_decompress;
use crate::inflate::zlib_decompress;
use crate::lzo::lzo1x_decompress;
use crate::segment::{
    COMP_ALGO_BZLIB, COMP_ALGO_HEADER_STRIPPING, COMP_ALGO_LZO, COMP_ALGO_ZLIB,
    ContentEncodingElement,
};

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
    /// Bzip2 compression (`ContentCompAlgo` bzlib).
    Bzip2,
    /// LZO1X compression (`ContentCompAlgo` lzo1x).
    Lzo,
}

/// Why the contents of a track cannot be recovered.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Unsupported {
    /// The contents are encrypted (`ContentEncryption`).
    Encrypted,
    /// The compression algorithm is unknown.
    UnknownCompression(u64),
}

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unsupported::Encrypted => f.write_str("the contents are encrypted"),
            Unsupported::UnknownCompression(algo) => {
                write!(f, "unknown content compression algorithm {algo}")
            }
        }
    }
}

/// The steps that recover the original contents of a track's frames.
#[derive(Debug, Default)]
pub(crate) struct ContentDecoder {
    steps: Vec<Step>,
    /// Set if the contents cannot be recovered (encryption, or an unknown compression algorithm).
    unsupported: Option<Unsupported>,
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
                (false, COMP_ALGO_BZLIB) => decoder.steps.push(Step::Bzip2),
                (false, COMP_ALGO_LZO) => decoder.steps.push(Step::Lzo),
                (true, _) => decoder.unsupported = Some(Unsupported::Encrypted),
                (false, algo) => decoder.unsupported = Some(Unsupported::UnknownCompression(algo)),
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

    /// Why the contents cannot be recovered, if they cannot.
    pub(crate) fn unsupported(&self) -> Option<Unsupported> {
        self.unsupported
    }

    /// `true` if the contents cannot be recovered.
    pub(crate) fn is_unsupported(&self) -> bool {
        self.unsupported.is_some()
    }

    /// `true` if the contents are not encoded.
    pub(crate) fn is_identity(&self) -> bool {
        self.steps.is_empty() && self.unsupported.is_none()
    }

    /// Recover the original contents of `data`.
    pub(crate) fn decode(&self, data: Box<[u8]>) -> Result<Box<[u8]>> {
        self.steps.iter().try_fold(data, |data, step| match step {
            Step::Prepend(prefix) if prefix.is_empty() => Ok(data),
            Step::Prepend(prefix) => Ok([&prefix[..], &data[..]].concat().into_boxed_slice()),
            Step::Zlib => zlib_decompress(&data).map(Vec::into_boxed_slice),
            Step::Bzip2 => bzip2_decompress(&data).map(Vec::into_boxed_slice),
            Step::Lzo => lzo1x_decompress(&data).map(Vec::into_boxed_slice),
        })
    }
}
