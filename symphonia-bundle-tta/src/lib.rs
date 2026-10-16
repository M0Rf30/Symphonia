// Symphonia TTA Bundle
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure Rust True Audio (TTA1) demuxer and decoder.
//!
//! The `.tta` reader ([`TtaReader`]) verifies the CRC-32 of the header and of the seek table, and
//! supports seeking to any frame. The decoder ([`TtaDecoder`]) decodes 8, 16 and 24-bit streams of
//! 1 to 16 channels. It follows FFmpeg's decoder, with which it is bit-exact; see `NOTICE`.
//!
//! The decoder is also used for the `A_TTA1` tracks of Matroska files. They have no header, so the
//! decoder is then configured from the sample rate, number of channels and bit depth of the codec
//! parameters.
//!
//! Password-protected (encrypted) streams are not supported.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]

mod bits;
mod decoder;
mod demuxer;
mod header;

pub use decoder::TtaDecoder;
pub use demuxer::TtaReader;

#[cfg(test)]
mod tests {
    /// `TtaReader` must implement `ProbeableFormat<'a>` for every lifetime `'a`, as required by
    /// the bound of `Probe::register_format`.
    #[test]
    fn register_format_is_generic_over_lifetime() {
        let mut probe = symphonia_core::formats::probe::Probe::default();
        probe.register_format::<crate::TtaReader<'_>>();
    }
}
