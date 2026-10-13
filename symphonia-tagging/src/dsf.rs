// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PLACEHOLDER: replaced by the container implementation.

use std::path::Path;

use symphonia_core::meta::{Tag, Visual};

use crate::{Error, Result, WriteOptions};

pub(crate) fn write(path: &Path, _tags: &[Tag], _visuals: &[Visual], _opts: &WriteOptions) -> Result<()> {
    Err(Error::Unsupported(path.display().to_string()))
}
