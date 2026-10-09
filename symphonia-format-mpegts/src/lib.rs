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

//! MPEG transport stream (MPEG-TS, ISO/IEC 13818-1) audio demuxer.
//!
//! # Supported streams
//!
//! The reader finds the audio streams of the programs of the stream using the program association
//! table (PAT) and the program map tables (PMT), and produces one track for each audio stream with
//! a supported codec. Video, subtitle, and data streams, and audio streams with an unsupported
//! codec, are ignored.
//!
//! | Stream type | Codec                            | Notes                              |
//! |-------------|----------------------------------|------------------------------------|
//! | `0x03/0x04` | MPEG-1/2 audio, layers 1, 2, 3   |                                    |
//! | `0x0f`      | AAC in ADTS                      |                                    |
//! | `0x11`      | AAC in LATM (with LOAS framing)  | Single program and layer           |
//! | `0x06`      | Opus                             | Registration or extension descriptor |
//!
//! The 188 byte packet, the 192 byte BDAV (M2TS), and the 204 byte (Reed-Solomon) packet formats
//! are supported. Scrambled packets are skipped.
//!
//! # Timeline
//!
//! The timeline of the tracks is in units of the sample rate of the codec (the core sample rate for
//! AAC with SBR, and 48 kHz for Opus). It begins at the earliest first PTS of all of the audio
//! streams, so the first packet of a stream has a timestamp of 0 unless the stream begins later
//! than another. The 33-bit PTS wraps are handled.
//!
//! The frames of an elementary stream are contiguous, so the timestamp of a packet is that of the
//! previous plus its duration, which is exact in samples. The PTS of the PES packets is only used
//! to anchor the timeline at the start, after a seek, and where it signals a discontinuity.
//!
//! # Seeking
//!
//! A transport stream has no index. If the stream is seekable, the position of the PES packet
//! preceding the required timestamp is found by bisection of the PTS of the packets of the stream
//! being sought, and the stream is then scanned forward. The decoder pre-roll required to
//! reproduce a continuous decode is taken into account. A stream that is not seekable can only be
//! sought forward.
//!
//! The duration is found from the PTS of the last PES packets of the streams, if the stream is
//! seekable and its length is known.

mod demuxer;
pub mod psi;

pub use demuxer::MpegTsReader;
