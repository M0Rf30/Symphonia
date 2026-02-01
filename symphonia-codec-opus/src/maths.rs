// Symphonia
// Copyright (c) 2019-2022 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Mathematical utility functions for CELT decoder.

use std::mem::size_of;

/// Integer logarithm trait for CELT.
pub trait ILog {
    fn celt_ilog2(&self) -> Self;
}

impl ILog for usize {
    fn celt_ilog2(&self) -> Self {
        size_of::<usize>() * 8 - self.leading_zeros() as usize
    }
}

impl ILog for i32 {
    fn celt_ilog2(&self) -> Self {
        (size_of::<Self>() * 8 - self.leading_zeros() as usize) as i32
    }
}
