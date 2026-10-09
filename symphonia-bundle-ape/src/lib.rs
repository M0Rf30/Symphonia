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
mod mac;

pub use decoder::ApeDecoder;
pub use demuxer::ApeReader;

use symphonia_core::errors::Error;

/// Map an APE core error to a Symphonia `Error`.
fn map_ape_error(err: mac::error::ApeError) -> Error {
    match err {
        mac::error::ApeError::Io(e) => Error::IoError(e),
        mac::error::ApeError::UnsupportedVersion(_) => {
            Error::Unsupported("ape: unsupported file version")
        }
        mac::error::ApeError::InvalidChecksum => Error::DecodeError("ape: invalid checksum"),
        mac::error::ApeError::InvalidFormat(msg) => Error::DecodeError(msg),
        mac::error::ApeError::DecodingError(msg) => Error::DecodeError(msg),
    }
}
