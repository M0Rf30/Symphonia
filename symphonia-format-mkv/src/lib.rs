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
//! For other tracks (e.g., Vorbis, whose blocks have a varying duration) the timestamp of a
//! packet is only accurate to within the timestamp scale. For these tracks `actual_ts` may be
//! off from the true position of the packet by up to that precision (about 44 frames at 44.1kHz
//! with the default scale), and a caller cannot discard frames to `required_ts` more exactly than
//! that. Tracks of codecs that need state to be re-established after a seek (Opus, MPEG audio,
//! AAC, Vorbis) back the seek off by a pre-roll: the returned `actual_ts` is at or before
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
