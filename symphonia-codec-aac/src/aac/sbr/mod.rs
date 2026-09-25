// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Spectral Band Replication (SBR) — the HE-AAC v1 (`ISO/IEC 14496-3`
//! §4.6.18) high-frequency reconstruction tool.
//!
//! The bitstream parsing, band-table derivation, HF generation /
//! adjustment, and 32/64-band QMF filterbanks in this module tree are
//! ported from the MIT-licensed `oxideav-aac` 0.1.7 crate
//! (<https://github.com/OxideAV/oxideav-aac>, Copyright (c) 2026
//! Karpelès Lab Inc.), a clean-room implementation of the spec. See
//! `symphonia-codec-aac/NOTICE` for the full license text and
//! per-file/per-item provenance is carried over from upstream as doc
//! comments citing the relevant spec section.
//!
//! [`decoder::SbrDecoder`] is the entry point: construct one per SBR
//! channel element (SCE or CPE) once the element's `id_aac` and the
//! SBR internal sample rate (`2 ×` the AAC core rate) are known, then
//! feed it [`extension::SbrExtensionData`] (parsed with
//! [`extension::SbrExtensionData::parse`]) plus the core decoder's
//! time-domain PCM for each frame.
//!
//! ## Parametric Stereo (HE-AAC v2) — not implemented
//!
//! Annex 8.A Parametric Stereo (PS) reuses this same QMF/hybrid-filter
//! domain to add a second (right) channel to a single mono SBR element,
//! but decoding it is out of scope for this port. [`decoder::SbrDecoder`]
//! detects a `bs_extension_id == EXTENSION_ID_PS` payload
//! ([`element::EXTENSION_ID_PS`]) and returns
//! [`error::SbrError::SbrPsUnsupported`] instead of silently decoding
//! mono-only output. A later wave can reintroduce upstream's `ps:
//! Option<PsState>` decoder field (holding a `PsDecoder` plus a second
//! synthesis filterbank for the right channel) at the same call sites in
//! [`decoder`] — search that module for `EXTENSION_ID_PS`.
//!
//! ## Low-power (real-valued) SBR mode
//!
//! [`decoder::SbrDecoder::set_low_power`] and the real-valued QMF
//! variants ([`qmf::RealAnalysisQmf`] etc.) and aliasing-reduction tools
//! ([`lp`]) are ported and unit-tested, but nothing in this crate calls
//! `set_low_power(true)` yet — every real-world HE-AAC v1 stream this
//! port has been validated against uses the standard complex-QMF path.

mod bits;
mod crc;
mod dequant;
mod element;
mod env_adjust;
mod envelope;
mod error;
mod freq_bands;
mod grid;
mod header;
mod huffman;
mod hf_gen;
mod limiter;
mod qmf;
mod noise_table;
mod lp;
mod reconstruct;
mod time_grid;

pub(crate) mod decoder;
pub(crate) mod extension;

/// Minimal local mirror of `oxideav_aac::raw_data_block::IdSynEle` — the
/// AAC core `id_syn_ele` kind an SBR payload attaches to
/// (`raw_data_block()`, ISO/IEC 14496-3 Table 4.3). Only [`IdSynEle::
/// Sce`] / [`IdSynEle::Cpe`] are ever valid for
/// [`extension::SbrExtensionData::parse`] (a `sbr_extension_data()`
/// payload only attaches to a single-channel or channel-pair element);
/// the other `id_syn_ele` values are included for parity with the
/// integration call site in `aac::mod`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdSynEle {
    /// `ID_SCE` — single channel element.
    Sce,
    /// `ID_CPE` — channel pair element.
    Cpe,
    /// `ID_CCE` — coupling channel element.
    Cce,
    /// `ID_LFE` — low frequency effects element.
    Lfe,
    /// `ID_DSE` — data stream element.
    Dse,
    /// `ID_PCE` — program config element.
    Pce,
    /// `ID_FIL` — fill element.
    Fil,
}
