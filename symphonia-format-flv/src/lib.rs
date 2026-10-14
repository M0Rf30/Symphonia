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
//! | 1            | SWF ADPCM, a tag is one or more blocks of 4096 frames   |
//! | 4, 5, 6      | Nellymoser, mono, 16 kHz, 8 kHz, or (format 6) the rate of the sound rate field |
//! | 11           | Speex, 16 kHz mono, a tag is one 20 ms frame, without a header |
//! | 9            | Enhanced RTMP audio (see below)                        |
//!
//! ## Enhanced RTMP
//!
//! The enhanced audio tag header of [Enhanced RTMP v2](https://github.com/veovera/enhanced-rtmp)
//! (sound format 9) is read, with the FourCCs `mp4a` (AAC), `.mp3`, `Opus` (the identification
//! header of the sequence start is the codec extra data, and its pre-skip the delay of the track,
//! which is trimmed from the first packet), `fLaC` (the sequence start is the STREAMINFO block,
//! with or without the `fLaC` marker and the block header), `ac-3`, and `ec-3`. An AC-3 or E-AC-3
//! tag is a packet of one or more syncframes; the duration is found from their headers. A
//! sequence start of Opus without data is taken to be of mono or stereo audio, depending on the
//! first packet.
//!
//! ModEx wrappers are skipped (the sub-millisecond timestamp offset is not used), and the
//! multichannel configuration is not used, as the codecs signal their channels. A multitrack tag
//! is read as the audio of the track of the first tag that configures the stream, and the other
//! tracks are ignored. Tags of other codecs than the first are ignored.
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
mod frames;

pub use demuxer::FlvReader;
