// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The multistream Opus decoder (RFC 7845 section 5.2 / libopus `src/opus_multistream_decoder.c`,
//! `struct OpusMSDecoder`, `opus_multistream_decode_native`).
//! Ported from libopus (BSD-3-Clause), see NOTICE.
//!
//! # Wave 2 scope (not yet implemented here)
//!
//! A multistream packet contains `stream_count` self-delimited Opus packets back-to-back (the
//! last one NOT self-delimited, per `crate::packet::parse_impl`'s `self_delimited` flag), decoded
//! by `stream_count` independent [`crate::decoder::OpusDecoder`] instances (`coupled_count` of
//! them stereo, the rest mono), then the decoded channels are scattered into the output according
//! to [`crate::mapping::ChannelMapping::table`] (`255` entries produce silence). This module owns
//! that fan-out; it does not duplicate per-stream decoding logic.

use crate::decoder::{DecodeError, OpusDecoder, Result, SampleRate};
use crate::mapping::ChannelMapping;

/// C: `struct OpusMSDecoder` (conceptually; the C version packs decoder state inline in one
/// allocation, which a `Vec<OpusDecoder>` supersedes here).
pub struct MultistreamDecoder {
    mapping: ChannelMapping,
    layout_channels: u8,
    decoders: Vec<OpusDecoder>,
}

impl MultistreamDecoder {
    /// C: `opus_multistream_decoder_init`.
    pub fn try_new(sample_rate: SampleRate, layout_channels: u8, mapping: ChannelMapping) -> Result<Self> {
        if mapping.stream_count == 0 {
            return Err(DecodeError::InvalidChannelCount);
        }
        let mut decoders = Vec::with_capacity(mapping.stream_count as usize);
        for i in 0..mapping.stream_count {
            let ch = if i < mapping.coupled_count { 2 } else { 1 };
            decoders.push(OpusDecoder::try_new(sample_rate, ch)?);
        }
        Ok(MultistreamDecoder { mapping, layout_channels, decoders })
    }

    pub fn stream_count(&self) -> usize {
        self.decoders.len()
    }

    /// C: `opus_multistream_decode_native`. Splits `data` into per-stream self-delimited packets
    /// via `crate::packet::parse_impl(..., self_delimited = true)` (all but the last stream) /
    /// `false` (the last), decodes each with its own [`OpusDecoder`], and scatters the decoded
    /// channels into `out` per `self.mapping.table`. Wave 2 implements this.
    pub fn decode(&mut self, data: Option<&[u8]>, out: &mut [f32], frame_size: usize) -> Result<usize> {
        let _ = (data, out, frame_size, &self.mapping, self.layout_channels, &mut self.decoders);
        todo!("wave 2: per-stream self-delimited split, decode, channel-mapping scatter")
    }
}
