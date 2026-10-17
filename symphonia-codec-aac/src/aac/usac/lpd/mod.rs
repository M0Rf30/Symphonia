// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The linear-prediction-domain coder of USAC (ACELP/TCX).
//!
//! This module is owned by the `UsacLpd` workstream. `LpdChannelState` is the persistent
//! per-channel state the core decoder keeps for every channel, whether it is frequency- or
//! linear-prediction-domain coded.
#![allow(dead_code)] // coordination stub; owned by `UsacLpd`

/// The persistent per-channel state of the linear-prediction-domain coder.
///
/// Owned and fleshed out by `UsacLpd`.
#[derive(Default)]
pub struct LpdChannelState;

impl LpdChannelState {
    pub fn new() -> Self {
        LpdChannelState
    }

    pub fn reset(&mut self) {
        // No state yet; `UsacLpd` adds the ACELP/TCX/LPC/FAC state.
    }
}
