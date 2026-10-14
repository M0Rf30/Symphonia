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

//! Demuxers for raw (headerless) elementary streams of surround sound codecs, as written by
//! encoders and ripping tools:
//!
//! | Reader          | Codec                      | Typical extensions       |
//! |-----------------|----------------------------|--------------------------|
//! | [`Ac3Reader`]   | AC-3 and E-AC-3            | `.ac3`, `.eac3`, `.ec3`  |
//! | [`DtsReader`]   | DTS (core, and DTS-HD)     | `.dts`, `.dtshd`, `.dtsma` |
//! | [`TrueHdReader`]| Dolby TrueHD and MLP       | `.thd`, `.truehd`, `.mlp` |
//!
//! The streams are split into frames exactly as FFmpeg's parsers do (see
//! `symphonia_common::mpeg::surround`). The readers are found by scanning for the sync words of
//! the codec, and then confirmed by a chain of several consecutive frames, so that data that
//! merely contains a sync word is not claimed.
//!
//! The streams have no timestamps and no index. The timeline is the running sample count; the
//! duration of AC-3 and DTS streams is estimated from the average size of the first frames, and
//! TrueHD/MLP streams (which have a variable bit rate) have no known duration. The readers seek
//! by scanning forward from the nearest frame they have seen, so seeking into a part of a seekable
//! stream that was not yet read scans the stream up to it.

mod reader;

pub use reader::{Ac3Reader, DtsReader, TrueHdReader};
