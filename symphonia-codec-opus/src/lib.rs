// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A pure-Rust, native Opus (RFC 6716) decoder.
//!
//! This is a faithful, from-scratch port of the reference `libopus` decoder (v1.5.2, BSD-3
//! license; see `NOTICE`) into idiomatic, safe Rust, targeting bit-exactness with the reference
//! for the SILK path (kept integer/fixed-point, as in libopus) and float CELT synthesis.
//! It is NOT a wrapper/binding around `libopus` or any other native/C library — see `NOTICE`
//! for provenance of ported algorithms.
//!
//! # Module map (mirrors libopus source layout 1:1 where practical)
//!
//! - [`range`]: the range entropy coder (`celt/entdec.c` / `celt/entenc.c`).
//! - [`packet`]: TOC byte and packet framing (`src/opus.c`).
//! - [`mapping`]: `OpusHead` and RFC 7845 channel mapping.
//! - [`silk`]: the SILK sub-decoder (`silk/*`), integer/fixed-point.
//! - [`celt`]: the CELT sub-decoder (`celt/*`), float.
//! - [`decoder`]: the top-level hybrid decoder (`src/opus_decoder.c`).
//! - [`multistream`]: the multistream/Ogg-mapping decoder (`src/opus_multistream_decoder.c`).
//! - [`audio_decoder`]: the Symphonia `AudioDecoder`/`RegisterableAudioDecoder` integration.

pub mod audio_decoder;
pub mod celt;
pub mod decoder;
pub mod mapping;
pub mod multistream;
pub mod packet;
pub mod range;
pub mod silk;

pub use audio_decoder::OpusAudioDecoder;
pub use decoder::OpusDecoder;
