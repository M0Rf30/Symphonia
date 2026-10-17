// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Enhanced SBR of USAC (harmonic transposer, PVC, inter-TES).
//!
//! This module is owned by the `UsacEsbr` workstream; the decoder below is the agreed
//! coordination interface, currently rejecting eSBR so the core coder can land first.
#![allow(dead_code)] // coordination stub; owned by `UsacEsbr`

use symphonia_core::errors::{Result, unsupported_error};

use symphonia_common::mpeg::audio::usac::{UsacSbrConfig, UsacSbrRatio};

/// The eSBR decoder of a channel pair or single channel.
pub struct EsbrDecoder {
    _cfg: UsacSbrConfig,
    _ratio: UsacSbrRatio,
    _channels: usize,
}

impl EsbrDecoder {
    pub fn new(cfg: UsacSbrConfig, ratio: UsacSbrRatio, channels: usize) -> Self {
        EsbrDecoder { _cfg: cfg, _ratio: ratio, _channels: channels }
    }

    /// Decode one frame of SBR data and process the core time-domain samples into the full-rate
    /// output. Not yet implemented by `UsacEsbr`.
    #[allow(clippy::too_many_arguments)]
    pub fn process(&mut self, _core_samples: &[f32], _output: &mut [f32]) -> Result<()> {
        unsupported_error("aac: usac esbr decoding is not yet implemented")
    }
}
