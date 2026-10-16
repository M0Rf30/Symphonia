// Symphonia TAK Bundle
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]

mod bits;
mod decoder;
mod demuxer;
mod info;
mod tables;

pub use decoder::TakDecoder;
pub use demuxer::TakReader;
