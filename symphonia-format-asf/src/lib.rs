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

//! A pure Rust ASF (Advanced Systems Format) demuxer for `.wma` and `.asf` files.
//!
//! Audio streams (WMA 1/2/Pro/Lossless/Voice, PCM) are exposed as tracks: the packets are the
//! media objects of the stream, with the `WAVEFORMATEX` of the stream in the codec parameter
//! `extra_data`. The content description and extended content description objects are mapped to
//! tags. Seeking uses the simple index object when present, and a scan of the data packets
//! otherwise. Video streams and DRM protected files are not supported.

mod meta;
mod objects;
mod packets;
mod reader;

pub use reader::AsfReader;
