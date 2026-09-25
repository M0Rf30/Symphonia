// Symphonia Musepack Bundle
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure Rust Musepack (SV7/SV8, `.mpc`) demuxer and decoder.
//!
//! Ported from the reference `libmpcdec` (BSD-3-Clause); see `NOTICE` for attribution and the
//! per-file porting convention.
//!
//! # Memory use (SV7)
//!
//! SV7 streams are not byte-aligned (see `demuxer::sv7` for why), so this crate buffers an
//! entire SV7 stream's audio-region bytes in memory once, at open time (capped at 512 MiB). SV8
//! streams are demuxed incrementally, packet by packet, with no such limitation.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]

mod bits;
mod cnk;
mod decoder;
mod decoder_core;
mod demuxer;
mod huffman;
mod requant;
mod synth;

pub use decoder::MpcDecoder;
pub use demuxer::MpcReader;

#[cfg(test)]
mod tests {
    /// Regression test: `MpcReader` must implement `ProbeableFormat<'a>` for *every* lifetime
    /// `'a` (not just one specific lifetime), matching `Probe::register_format`'s
    /// `for<'a> P: ProbeableFormat<'a>` bound. An earlier version of this crate used
    /// `impl<'s> ProbeableFormat<'s> for MpcReader<'s>` (a named, reused lifetime), which some
    /// rustc versions reject here as "not general enough"; the fix is the elided
    /// `impl ProbeableFormat<'_> for MpcReader<'_>` form used by the other bundle crates.
    #[test]
    fn register_format_is_generic_over_lifetime() {
        let mut probe = symphonia_core::formats::probe::Probe::default();
        probe.register_format::<crate::MpcReader<'_>>();
    }
}
