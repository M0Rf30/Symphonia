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
//! | Stream type            | Codec                                | Notes                                    |
//! |------------------------|--------------------------------------|------------------------------------------|
//! | `0x03/0x04`            | MPEG-1/2 audio, layers 1, 2, 3       |                                          |
//! | `0x0f`                 | AAC in ADTS                          |                                          |
//! | `0x11`                 | AAC in LATM (with LOAS framing)      | Single program and layer                 |
//! | `0x80`                 | Blu-ray LPCM, 16 and 24-bit          | 48, 96, 192 kHz; up to 7.1               |
//! | `0x81`                 | AC-3                                 | ATSC and Blu-ray                         |
//! | `0x87`                 | E-AC-3                               | ATSC                                     |
//! | `0x84`, `0xa1`         | E-AC-3                               | Blu-ray programs (`HDMV`/`HDPR`)         |
//! | `0x82`, `0x85`, `0x86` | DTS, DTS-HD (core and extension)     | Blu-ray                                  |
//! | `0x8a`                 | DTS                                  |                                          |
//! | `0xa2`                 | DTS-HD (secondary)                   | Blu-ray programs (`HDMV`/`HDPR`)         |
//! | `0x83`                 | Dolby TrueHD, and the AC-3 stream    | Blu-ray: two tracks, see below           |
//! | `0x06`                 | Opus                                 | Registration or extension descriptor     |
//! | `0x06`                 | AC-3, E-AC-3, DTS                    | DVB descriptors `0x6a`, `0x7a`, `0x7b`, or registration descriptors `AC-3`, `EAC3`, `DTS1`, `DTS2`, `DTS3` |
//! | `0x06`                 | AAC (ADTS, LATM), MPEG audio         | Only without descriptors; identified from the data |
//!
//! A user private stream type (`0x80` or above) that is not listed is identified by the same
//! descriptors as PES private data. The HLS SAMPLE-AES stream types (`0xc1` AC-3, `0xc2` E-AC-3,
//! and `0xcf` AAC) are recognised, but their audio frames are encrypted: no track is produced and
//! a warning is logged. The same applies to Dolby AC-4 (identified by the DVB AC-4 descriptor, an
//! extension descriptor with the tag extension `0x15`), for which there is no decoder.
//!
//! The PID of a Blu-ray TrueHD stream (`0x83`) carries a TrueHD stream and an AC-3 version of the
//! same audio, in PES packets that are distinguished by the `stream_id_extension` of their PES
//! extension (`0x76` is the AC-3 stream). The reader produces a track for each, as FFmpeg does. The
//! TrueHD track has the track ID of the PID, and the AC-3 track has that of the PID plus `0x2000`.
//! The AC-3 track is dropped if the PID has no PES packets of it.
//!
//! The 188 byte packet, the 192 byte BDAV (M2TS), and the 204 byte (Reed-Solomon) packet formats
//! are supported. Scrambled packets are skipped. Duplicate packets are dropped, and a PES packet
//! that lost transport stream packets (a break in the continuity counter) is discarded.
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

mod bdlpcm;
mod demuxer;
pub mod psi;
mod sniff;

pub use demuxer::MpegTsReader;
