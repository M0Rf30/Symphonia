// Symphonia
// Copyright (c) 2019-2024 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::errors::Result;
use symphonia_core::io::ReadBytes;
use symphonia_core::meta::{MetadataBuilder, MetadataRevision, MetadataSideData};

pub fn read_dsf_id3_metadata<B: ReadBytes>(
    reader: &mut B,
    side_data: &mut Vec<MetadataSideData>,
) -> Result<MetadataRevision> {
    let mut builder = MetadataBuilder::new(crate::id3v2::ID3V2_METADATA_INFO);
    crate::id3v2::read_id3v2(reader, &mut builder, side_data)?;
    Ok(builder.build())
}
