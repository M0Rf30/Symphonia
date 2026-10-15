// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! NTT TwinVQ (`.vqf`) demuxer.
//!
//! A VQF file starts with `TWIN` and an eight character version string, then a big-endian
//! header size and a list of chunks: `COMM` (channels, bit rate, sample rate), metadata chunks
//! (`NAME`, `AUTH`, `COMT`, ...) and finally `DATA`, after which the bitstream follows to the
//! end of the file. The layout follows FFmpeg's `libavformat/vqf.c` (see `NOTICE`).
//!
//! The bitstream is a sequence of frames that are not byte aligned. [`VqfReader`] returns one
//! packet per frame in the form `symphonia-codec-twinvq` expects: a byte with the number of bits
//! to skip in the following byte, the last byte of the previous packet, and then the bytes
//! holding the frame. The first 12 bytes of the `COMM` chunk are the codec extra data.
//!
//! Packets are fixed-size frames, so seeking is sample accurate to a frame.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]
// Disable to better express the specification.
#![allow(clippy::collapsible_else_if)]

mod reader;

pub use reader::VqfReader;
