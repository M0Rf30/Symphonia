// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Parametric Stereo (Annex 8.A / HE-AAC v2) — `ps_data()` bitstream
//! parsing, the hybrid analysis/synthesis filterbank, de-correlation
//! and stereo mixing that turn one SBR-decoded mono element into a
//! stereo pair.
//!
//! The bitstream/DSP modules in this submodule tree are ported from
//! the MIT-licensed `oxideav-aac` 0.1.7 crate
//! (<https://github.com/OxideAV/oxideav-aac>, Copyright (c) 2026
//! Karpelès Lab Inc.), a clean-room implementation of the spec. See
//! `symphonia-codec-aac/NOTICE` for the full license text; per-file/
//! per-item provenance is carried over from upstream as doc comments
//! citing the relevant spec section.
//!
//! [`decoder::PsDecoder`] is the entry point [`super::decoder::
//! SbrDecoder`] drives once a single-channel element's SBR extension
//! carries an `EXTENSION_ID_PS` payload (see that module's
//! `PsState`).

pub(super) mod data;
pub(super) mod decoder;
mod decorr;
mod huffman;
pub(super) mod hybrid;
mod map;
mod stereo;
