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

//! Demuxer for the AMR-NB and AMR-WB storage format (`.amr`, `.awb`) of RFC 4867 section 5.
//!
//! The file starts with a magic number (`#!AMR\n`, `#!AMR-WB\n`, or the multi-channel
//! `#!AMR_MC1.0\n` / `#!AMR-WB_MC1.0\n` followed by a 32-bit channel description), then frame
//! blocks of one speech frame per channel. Every frame is a TOC byte (`P FT(4) Q P P`) followed
//! by the speech bits, and the length of the frame follows from the frame type.
//!
//! # Packets
//!
//! A packet is one frame including its TOC byte, which is the layout of the samples of the
//! `samr`/`sawb` tracks of MP4 files (3GPP TS 26.244), so that the decoders see the same packets
//! from every container. For a multi-channel file a packet is a whole frame block: the frames
//! of the channels, each with its TOC byte, in channel order.
//!
//! A packet lasts 160 (AMR-NB, 8 kHz) or 320 (AMR-WB, 16 kHz) samples of the time base `1 /
//! sample rate`. Frame types that are not allowed in files are treated as garbage and skipped
//! one byte at a time until a valid TOC byte is found. A last frame that is cut short is
//! dropped.
//!
//! # Deviation from FFmpeg
//!
//! The channel count of multi-channel files is the low 4 bits of the big-endian channel
//! description field, as RFC 4867 section 5.2 specifies (reserved bits are ignored). FFmpeg's
//! `amr.c` reads the field as a little-endian integer, which does not read files that follow the
//! RFC.
//!
//! # Seeking
//!
//! Frames have variable sizes. The reader indexes the position of about every second of audio as
//! it reads, and seeks by scanning from the nearest indexed frame, skipping frame payloads. With
//! `FormatOptions::prebuild_seek_index` and a seekable source the file is scanned when opened, so
//! that the duration is known and the whole file is indexed.

mod reader;

pub use reader::AmrReader;
