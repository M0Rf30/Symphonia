// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The CELT sub-decoder. Ported from libopus `celt/*.c` (float build: `celt_sig`/`celt_norm`
//! are `f32`). Ported from libopus (BSD-3-Clause), see NOTICE.
//!
//! Modules group by stage: [`modes`], [`laplace`], [`quant_bands`], [`rate`], [`cwrs`], [`vq`]
//! and [`bands`] read range-coded symbols and produce normalized band energy and shape;
//! [`kiss_fft`], [`mdct`], [`pitch`], [`lpc`], [`celt`] and `decoder.rs` cover denormalization,
//! inverse MDCT, comb filtering and packet loss concealment.

pub mod bands;
#[allow(clippy::module_inception)]
pub mod celt;
pub mod cwrs;
pub mod decoder;
pub mod kiss_fft;
pub mod laplace;
pub mod lpc;
pub mod mdct;
pub mod modes;
pub mod pitch;
pub mod quant_bands;
pub mod rate;
pub mod vq;

pub use decoder::CeltDecoder;
