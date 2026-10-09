// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]

//! Flash Video (FLV) audio demuxer.
//!
//! # Supported audio
//!
//! An FLV file has at most one audio stream, whose codec is that of the first audio tag. The video
//! and script data tags are skipped.
//!
//! | Sound format | Codec                                                  |
//! |--------------|--------------------------------------------------------|
//! | 2, 14        | MPEG audio layer 3 (MP3), the frames are found in the tag |
//! | 10           | AAC, with the audio specific config of the sequence header |
//! | 0            | Linear PCM (taken to be big-endian), 8-bit unsigned, or 16-bit signed |
//! | 3            | Linear PCM, little-endian, 8-bit unsigned, or 16-bit signed |
//! | 7, 8         | G.711 A-law, mu-law (8 kHz)                            |
//!
//! SWF ADPCM, Nellymoser, Speex, and the "enhanced RTMP" extended audio header are not supported.
//! A file with such audio is not opened.
//!
//! # Timeline
//!
//! The timeline is in units of the sample rate of the codec (the core sample rate for AAC with
//! SBR), and begins at the time stamp of the first audio tag. The tag time stamps have a precision
//! of 1 ms, so they are only used to anchor the timeline at the start, after a seek, and where
//! they signal a discontinuity. Otherwise, the timestamp of a packet is that of the previous plus
//! its duration, which is exact in samples.
//!
//! # Duration and seeking
//!
//! The duration is found from the time stamp of the last audio tag, if the stream is seekable.
//! Otherwise, the `duration` of the `onMetaData` script data is used, if present.
//!
//! The position to seek to is found by bisection of the time stamps of the audio tags (FLV tags
//! can be found and verified at any position, using the size of the previous tag that follows
//! every tag), starting from the nearest entry of the seek index that precedes the position. The
//! seek index is the keyframe index of the `onMetaData` script data, if present, and the tags that
//! have been read (or all of them, if requested using
//! [`FormatOptions::prebuild_seek_index`](symphonia_core::formats::FormatOptions)). The decoder
//! pre-roll required to reproduce a continuous decode is taken into account. A stream that is not
//! seekable can only be sought forward.

mod amf;
mod demuxer;
mod es;

pub use demuxer::FlvReader;
