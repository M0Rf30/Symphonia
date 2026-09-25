// Symphonia Musepack demuxer+decoder
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Huffman code tables for SV7 and SV8. See `crate::bits` for the decode primitives that
//! consume these tables, and `NOTICE` for licensing/attribution.

pub mod sv7_tables;
pub mod sv8_tables;
