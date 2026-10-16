// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]

//! Demuxers for simple sampled-sound file formats:
//!
//! | Reader           | Format                       | Feature   | Typical extensions       |
//! |------------------|------------------------------|-----------|--------------------------|
//! | [`AuReader`]     | Sun/NeXT AU                  | `au`      | `.au`, `.snd`            |
//! | [`VocReader`]    | Creative Voice File          | `voc`     | `.voc`                   |
//! | [`IffReader`]    | IFF 8SVX and 16SV            | `iff`     | `.iff`, `.8svx`, `.16sv` |
//! | [`RawPcmReader`] | headerless PCM               | `raw-pcm` | -                        |
//!
//! The AU, VOC and IFF readers are found by probing. A headerless PCM file cannot be recognised
//! from its content, so [`RawPcmReader`] is not a probeable format: it is created with
//! [`RawPcmReader::try_new`] from an explicit [`RawPcmParams`].

#[cfg(feature = "au")]
mod au;
mod common;
#[cfg(feature = "iff")]
mod iff;
#[cfg(feature = "raw-pcm")]
mod raw;
#[cfg(feature = "voc")]
mod voc;

#[cfg(feature = "au")]
pub use au::AuReader;
#[cfg(feature = "iff")]
pub use iff::IffReader;
#[cfg(feature = "raw-pcm")]
pub use raw::{RawPcmParams, RawPcmReader, RawPcmSampleFormat};
#[cfg(feature = "voc")]
pub use voc::VocReader;
