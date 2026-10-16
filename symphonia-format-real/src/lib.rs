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

//! RealMedia (`.rm`, `.rmvb`, `.ra`) audio demuxer.
//!
//! Reads the RealAudio 1.0 (`.ra` v3), 2.0 (v4) and 3.0 (v5) headers, the RealMedia
//! container (`PROP`/`MDPR`/`DATA`/`INDX` chunks), including interleaved `cook`, `atrc`,
//! `sipr` streams, which are de-interleaved before they are returned as packets.

mod header;
mod reader;
mod stream;

pub use reader::RealReader;
