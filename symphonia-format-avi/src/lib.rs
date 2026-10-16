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

//! A pure Rust AVI demuxer.
//!
//! Every audio stream with a supported `WAVEFORMATEX` tag (PCM, IEEE float, A-law, mu-law, MS and
//! IMA ADPCM, MPEG audio layers 1 to 3) is a track; video streams are skipped. Timestamps are
//! counted in samples from the packets themselves. OpenDML files (`RIFF AVIX` continuations) are
//! followed. Seeking rescans the `movi` list (the `idx1` index is not used).

mod mpeg;
mod reader;

pub use reader::AviReader;
