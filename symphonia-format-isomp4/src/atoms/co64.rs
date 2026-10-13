// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::atoms::{Atom, AtomHeader, AtomIterator, ReadAtom, Result};

/// Chunk offset atom (64-bit version).
#[allow(dead_code)]
#[derive(Debug)]
pub struct Co64Atom {
    pub chunk_offsets: Vec<u64>,
}

impl Atom for Co64Atom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, _header: &AtomHeader) -> Result<Self> {
        let (_, _) = it.read_extended_header()?;

        let chunk_offsets = it.read_table(|it| it.read_u64())?;

        Ok(Co64Atom { chunk_offsets })
    }
}
