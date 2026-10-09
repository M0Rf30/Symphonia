// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Matroska / WebM demuxer.
//!
//! # Timestamps and seeking
//!
//! Audio tracks use a timebase of `1 / sample rate`, so packet timestamps, durations, trims, and
//! the timestamps returned by `seek` are in frames. However, the timestamps of blocks in a
//! Matroska file are only as precise as the segment's timestamp scale (usually 1ms). For audio
//! tracks with a constant block duration (PCM, and codecs with a `DefaultDuration`, or a constant
//! block size in their codec private data such as FLAC and ALAC) the exact, frame accurate, start
//! of every block is recovered from the first block of the track, so `required_ts` and `actual_ts`
//! are exact.
//!
//! The same is done for Vorbis and Opus, whose packets carry their own duration (the block sizes
//! of the packet and of the one before it, and the TOC byte, respectively): the exact timeline is
//! the running sum of the durations of the packets, anchored to the first block of the stream. To
//! be able to seek to an exact position the demuxer needs the timeline at the target. The stream
//! is scanned once, on the first seek, to index the exact position of every block, so the first
//! seek in a seekable stream reads the whole file. If the stream is not seekable, or the scan
//! fails, the timestamp of the packet after a seek is only accurate to within the timestamp scale
//! (about 44 frames at 44.1kHz with the default scale).
//!
//! Tracks of codecs that need state to be re-established after a seek (Opus, MPEG audio, AAC,
//! Vorbis) back the seek off by a pre-roll: the returned `actual_ts` is at or before
//! `required_ts`, and the caller should decode (and discard) the packets from `actual_ts` before
//! presenting audio from `required_ts`.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]

mod codecs;
mod demuxer;
mod ebml;
mod lacing;
mod schema;
mod segment;
mod tags;
mod timeline;

pub use crate::demuxer::MkvReader;

pub mod sub_fields {
    //! Key name constants for sub-fields of MKV tags and chapters.
    //!
    //! For the exact meaning of these fields, and the format of their values, please consult the
    //! official Matroska specification.

    pub const TAG_LANGUAGE: &str = "LANGUAGE";
    pub const TAG_LANGUAGE_BCP47: &str = "LANGUAGE_BCP47";

    pub const CHAPTER_TITLE_COUNTRY: &str = "CHAPTER_TITLE_COUNTRY";
    pub const CHAPTER_TITLE_LANGUAGE: &str = "CHAPTER_TITLE_LANGUAGE";
    pub const CHAPTER_TITLE_LANGUAGE_BCP47: &str = "CHAPTER_TITLE_LANGUAGE_BCP47";

    pub const EDITION_TITLE_LANGUAGE_BCP47: &str = "EDITION_TITLE_LANGUAGE_BCP47";
}
