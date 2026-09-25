// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Local error type for the ported SBR + Parametric Stereo tools.
//!
//! The upstream `oxideav-aac` 0.1.7 sources (see `symphonia-codec-aac/
//! NOTICE`) return a crate-wide `Error` enum with one variant per failure
//! kind; only seven of its variants are reachable from the decode-side
//! SBR/PS modules (`Error::Sbr*` plus PS's single `Error::PsDataInvalid`,
//! see the grep in the port notes). This module defines exactly those
//! seven variants, under the **same names**, so every ported
//! `Error::SbrXxx` / `Error::PsDataInvalid` call site is source-identical
//! to upstream — only the `use crate::{Error, Result}` line at the top of
//! each file changed (to `use super::error::{SbrError as Error,
//! SbrResult as Result}`, or `use super::super::error::{...}` from the
//! nested `ps` submodule).

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
    /// A Parametric Stereo `ps_data()` element (§8.4.2 Table 8.9) failed
    /// to parse: an unmatched Annex 8.B Huffman codeword, a truncated
    /// payload, a reserved `iid_mode`/`icc_mode`, or a resolved IID/ICC
    /// index outside its Table 8.24/8.27 range.
    PsDataInvalid,
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
            SbrError::PsDataInvalid => "aac (sbr): parametric stereo ps_data() parse failed",
            SbrError::SbrCrcMismatch => "aac (sbr): bs_sbr_crc_bits mismatch",
        }
    }
}

impl From<SbrError> for CoreError {
    fn from(e: SbrError) -> Self {
        CoreError::DecodeError(e.message())
    }
}
