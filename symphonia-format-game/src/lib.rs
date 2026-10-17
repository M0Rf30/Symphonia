// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]

//! Demuxers of the audio formats of games consoles and their middleware.
//!
//! * [`AdpReader`]: Nintendo GameCube DTK (`.adp`).
//! * [`AdxReader`]: CRI ADX.
//! * [`AfcReader`]: Nintendo GameCube AFC.
//! * [`AstReader`]: Nintendo AST (`STRM`).
//! * [`BrstmReader`]: Nintendo BRSTM, BFSTM and BCSTM.
//! * [`HcaReader`]: CRI HCA.
//! * [`VagReader`]: Sony PlayStation VAG and its variants.

mod adp;
mod adx;
mod afc;
mod ast;
mod brstm;
mod common;
mod hca;
mod vag;

pub use adp::AdpReader;
pub use adx::AdxReader;
pub use afc::AfcReader;
pub use ast::AstReader;
pub use brstm::BrstmReader;
pub use hca::HcaReader;
pub use vag::VagReader;
