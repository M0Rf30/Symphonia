// Symphonia DSD Format Demuxer
// Copyright (c) 2026 M0Rf30
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]

mod dff;
mod dff_info;
mod dsf;

pub use dff::DffReader;
pub use dsf::DsfReader;

pub use symphonia_core::codecs::audio::well_known::CODEC_ID_DSD;

pub const DSD64_RATE: u32 = 2822400;
pub const DSD128_RATE: u32 = 5644800;
pub const DSD256_RATE: u32 = 11289600;
pub const DSD512_RATE: u32 = 22579200;
