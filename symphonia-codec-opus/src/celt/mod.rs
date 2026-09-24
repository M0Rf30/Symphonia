// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The CELT sub-decoder. Ported from libopus `celt/*.c` (float build: `celt_sig`/`celt_norm`
//! are `f32`). Ported from libopus (BSD-3-Clause), see NOTICE.
//!
//! # Ownership (wave 1)
//!
//! This `celt/**` subtree is split between two wave-1 agents. Each file below lists its owner
//! and the C translation unit(s) it ports. `decoder.rs` is written purely against the function
//! signatures declared by the other files in this module (all defined in wave 0, bodies
//! `todo!()`), so both agents can work in parallel without touching each other's files.
//!
//! **"CeltBitstream"** owns: [`modes`], [`laplace`], [`quant_bands`], [`rate`], [`cwrs`], [`vq`],
//! [`bands`] — everything that reads range-coded symbols and produces/consumes normalized band
//! energy and shape (MDCT-domain, pre-synthesis).
//!
//! **"CeltSynthesis"** owns: [`kiss_fft`], [`mdct`], [`pitch`], [`lpc`], [`celt`], `decoder.rs`,
//! `mod.rs` (this file) — everything from denormalized MDCT bins through inverse MDCT, comb
//! filtering, and PLC (which needs pitch search/LPC extrapolation, not entropy decoding).

pub mod bands;
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
