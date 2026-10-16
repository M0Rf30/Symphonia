// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! AC-4 audio decoder.
//!
//! AC-4 (ETSI TS 103 190-1/-2) is Dolby's fourth-generation audio codec used by ATSC 3.0 and
//! DVB broadcasts. This decoder follows the structure of FFmpeg's `ac4dec.c` (see `NOTICE`):
//! a frame is parsed from the TOC, the audio payload is Huffman-decoded into a quantized
//! spectrum, inverse-quantized, and reconstructed by an inverse MDCT; an optional A-SPX
//! (Spectral Band Replication) stage synthesizes the high band in the QMF domain.
//!
//! This crate is a work in progress.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]
#![allow(clippy::collapsible_else_if)]
// Index loops mirror the structure of the reference implementation.
#![allow(clippy::needless_range_loop)]
// The decoder is still being ported; large portions of the context are not yet read.
#![allow(dead_code)]

mod bit;
mod decoder;
mod mdct;
mod tables;
mod vlc;

pub use decoder::Ac4Decoder;
