// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! MPEG Surround 2-1-2 of USAC (ISO/IEC 23003-3 7.11).
//!
//! This module is owned by the `UsacMps` workstream; the type below is the agreed coordination
//! interface, currently rejecting MPS212 so the core coder can land first.
#![allow(dead_code)] // coordination stub; owned by `UsacMps`

use symphonia_core::errors::{Result, unsupported_error};

use symphonia_common::mpeg::audio::usac::UsacMps212Config;

/// The MPS212 upmixer of a channel pair.
pub struct Mps212Decoder {
    _cfg: UsacMps212Config,
}

impl Mps212Decoder {
    pub fn new(cfg: UsacMps212Config) -> Self {
        Mps212Decoder { _cfg: cfg }
    }

    /// Decode one frame of MPS212 data and upmix the downmix into stereo. Not yet implemented by
    /// `UsacMps`.
    pub fn process(&mut self, _downmix: &[f32], _output: &mut [f32; 2]) -> Result<()> {
        unsupported_error("aac: usac mps212 decoding is not yet implemented")
    }
}
