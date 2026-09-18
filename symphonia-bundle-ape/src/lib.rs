// Symphonia APE Bundle
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]

mod decoder;
mod demuxer;

pub use decoder::ApeDecoder;
pub use demuxer::ApeReader;

use symphonia_core::errors::Error;

/// Map an `ape_decoder` crate error to a Symphonia `Error`.
///
/// `ape_decoder::ApeError` is `#[non_exhaustive]`, so a catch-all arm is required even though
/// every variant defined at the time of writing is handled explicitly.
fn map_ape_error(err: ape_decoder::ApeError) -> Error {
    match err {
        ape_decoder::ApeError::Io(e) => Error::IoError(e),
        ape_decoder::ApeError::UnsupportedVersion(_) => {
            Error::Unsupported("ape: unsupported file version")
        }
        ape_decoder::ApeError::InvalidChecksum => Error::DecodeError("ape: invalid checksum"),
        ape_decoder::ApeError::InvalidFormat(msg) => Error::DecodeError(msg),
        ape_decoder::ApeError::DecodingError(msg) => Error::DecodeError(msg),
        _ => Error::DecodeError("ape: unknown error"),
    }
}
