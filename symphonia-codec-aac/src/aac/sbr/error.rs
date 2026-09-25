// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Local error type for the ported SBR tool.
//!
//! The upstream `oxideav-aac` 0.1.7 sources (see `symphonia-codec-aac/
//! NOTICE`) return a crate-wide `Error` enum with one variant per failure
//! kind; only six of its variants are reachable from the decode-side SBR
//! modules (`Error::Sbr*`, see the grep in the port notes). This module
//! defines exactly those six variants, under the **same names**, so every
//! ported `Error::SbrXxx` call site is source-identical to upstream — only
//! the `use crate::{Error, Result}` line at the top of each file changed
//! (to `use super::error::{SbrError as Error, SbrResult as Result}`).

use symphonia_core::errors::Error as CoreError;

/// SBR-specific decode error (see the [module docs](self)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SbrError {
    /// A frequency-band derivation input was out of range (§4.6.18.3.6
    /// band tables, patch construction, limiter bands, envelope/noise
    /// band-index bookkeeping).
    SbrFreqBandInvalid,
    /// No Huffman codeword matched within the maximum code length, or a
    /// bit reader ran out of bits while parsing a header/envelope field.
    SbrHuffInvalid,
    /// A malformed time/frequency grid (`sbr_grid()`, the derived time
    /// grid, or a coupled channel's raw envelope/noise row length).
    SbrGridInvalid,
    /// A QMF analysis/synthesis filterbank received the wrong number of
    /// input samples for its rate, or the SBR decoder's own frame-size
    /// / mode-switch invariants were violated.
    SbrQmfInvalid,
    /// A Parametric Stereo payload was found on a low-power SBR decoder
    /// (Annex 8.A needs the complex QMF domain).
    SbrLowPowerPs,
    /// A Parametric Stereo (`EXTENSION_ID_PS`) payload was found, but
    /// HE-AAC v2 / Annex 8.A Parametric Stereo decoding is not
    /// implemented in this port (a later wave over the same QMF
    /// domain this SBR tool already assembles — see
    /// `symphonia-codec-aac/NOTICE`). Distinct from
    /// [`SbrError::SbrLowPowerPs`], which is the spec-mandated
    /// rejection of PS on a low-power SBR decoder.
    SbrPsUnsupported,
    /// The recomputed `bs_sbr_crc_bits` (`G10`, zero-init) disagreed
    /// with the transmitted value.
    SbrCrcMismatch,
}

pub(crate) type SbrResult<T> = core::result::Result<T, SbrError>;

impl SbrError {
    /// A static, greppable description for [`symphonia_core::errors::
    /// Error::DecodeError`].
    pub(crate) fn message(self) -> &'static str {
        match self {
            SbrError::SbrFreqBandInvalid => "aac (sbr): invalid frequency band parameters",
            SbrError::SbrHuffInvalid => "aac (sbr): invalid huffman codeword or truncated stream",
            SbrError::SbrGridInvalid => "aac (sbr): invalid time/frequency grid",
            SbrError::SbrQmfInvalid => "aac (sbr): invalid qmf filterbank input or mode switch",
            SbrError::SbrLowPowerPs => "aac (sbr): parametric stereo on a low-power sbr decoder",
            SbrError::SbrPsUnsupported => {
                "aac (sbr): parametric stereo (HE-AAC v2) is not yet supported"
            }
            SbrError::SbrCrcMismatch => "aac (sbr): bs_sbr_crc_bits mismatch",
        }
    }
}

impl From<SbrError> for CoreError {
    fn from(e: SbrError) -> Self {
        CoreError::DecodeError(e.message())
    }
}
