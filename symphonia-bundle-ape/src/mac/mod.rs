// Symphonia APE decoder core
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Monkey's Audio (APE) header parsing and frame decoding.
//!
//! This is the `ape-decoder` crate (by ombs.io, MIT OR Apache-2.0), vendored and trimmed down to
//! what the demuxer and decoder need: file/header/seek table parsing and the stateless-per-frame
//! decoder. See `NOTICE` in this directory for the attribution and licensing details.

pub mod bitreader;
pub mod crc;
pub mod entropy;
pub mod error;
pub mod format;
pub mod frame;
pub mod nn_filter;
pub mod predictor;
pub mod range_coder;
pub mod roll_buffer;
pub mod unprepare;
