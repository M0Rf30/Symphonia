// Symphonia Shorten bundle
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure Rust Shorten (`.shn`) demuxer and decoder.
//!
//! Shorten is Tony Robinson's lossless audio codec (SoftSound, 1994). The decoder is ported from
//! FFmpeg's `libavcodec/shorten.c` and is bit exact with it; see `NOTICE`.
//!
//! The stream has no random access points: prediction and the running means carry over from
//! block to block. [`ShortenReader`] therefore seeks by reconstructing the stream up to the target
//! and handing the decoder state to it (see the `reader` module).

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]

mod bits;
mod core;
mod decoder;
mod header;
mod reader;

pub use decoder::ShortenDecoder;
pub use reader::ShortenReader;

#[cfg(test)]
mod tests {
    /// `ShortenReader` must implement `ProbeableFormat<'a>` for every lifetime `'a`, as
    /// `Probe::register_format` requires.
    #[test]
    fn register_format_is_generic_over_lifetime() {
        let mut probe = symphonia_core::formats::probe::Probe::default();
        probe.register_format::<crate::ShortenReader<'_>>();
    }
}
