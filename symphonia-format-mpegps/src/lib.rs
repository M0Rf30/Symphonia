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

//! MPEG program stream (MPEG-PS, ISO/IEC 13818-1) and MPEG-1 system stream (ISO/IEC 11172-1) audio
//! demuxer, including the DVD-Video VOB.
//!
//! # Supported streams
//!
//! The reader finds the audio streams in the PES packets of the stream, and produces one track for
//! each audio stream with a supported codec. The video, subtitle, and data streams are ignored.
//!
//! | Stream ID         | Codec                                        |
//! |-------------------|----------------------------------------------|
//! | `0xc0` - `0xdf`   | MPEG-1/2 audio, layers 1, 2, 3               |
//! | `0xbd` / `0xa0`..`0xa7` | DVD-Video LPCM (16 and 24-bit, 48 and 96 kHz) |
//!
//! AC-3, DTS, and other audio carried in private stream 1 is ignored, as are 20-bit LPCM
//! streams. Both the MPEG-2 and the MPEG-1 forms of the pack and PES headers are supported.
//!
//! # Timeline
//!
//! The timeline of the tracks is in units of the sample rate of the codec. It begins at the
//! earliest first PTS of all of the audio streams, so the first packet of a stream has a
//! timestamp of 0 unless the stream begins later than another. The 33-bit PTS wraps are handled.
//!
//! The frames of an elementary stream are contiguous, so the timestamp of a packet is that of the
//! previous plus its duration, which is exact in samples. The PTS of the PES packets is only used
//! to anchor the timeline at the start, after a seek, and where it signals a discontinuity.
//!
//! # Seeking
//!
//! A program stream has no index. If the stream is seekable, the position of the PES packet
//! preceding the required timestamp is found by bisection of the PTS of the packets of the stream
//! being sought, and the stream is then scanned forward. The decoder pre-roll required to
//! reproduce a continuous decode is taken into account. A stream that is not seekable can only be
//! sought forward.
//!
//! The duration is found from the PTS of the last PES packets of the streams, if the stream is
//! seekable and its length is known.

mod demuxer;
mod lpcm;

pub use demuxer::MpegPsReader;
pub use lpcm::LpcmEs;
