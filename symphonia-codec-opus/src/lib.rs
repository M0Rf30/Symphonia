// Symphonia Opus Codec
// SPDX-License-Identifier: MPL-2.0

//! Opus audio codec decoder
//!
//! This module implements a pure Rust Opus decoder based on the reference
//! implementation from xiph.org. Opus is a lossy audio codec designed for
//! interactive speech and music transmission over the Internet.

mod entdec;
mod celt_constants;
mod cwrs;
mod laplace;
mod quant_bands;
mod bands;

pub use entdec::RangeDecoder;
