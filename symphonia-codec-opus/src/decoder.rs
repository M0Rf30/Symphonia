// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The top-level hybrid Opus decoder. Ported from libopus `src/opus_decoder.c`
//! (`OpusDecoder`, `opus_decoder_init`, `opus_decode_native`, `opus_decode_frame`).
//! Ported from libopus (BSD-3-Clause), see NOTICE.
//!
//! # Wave 2 scope (not yet implemented here)
//!
//! `opus_decode_frame` (the real per-packet logic) must:
//! - Dispatch by [`crate::packet::OpusMode`] (SILK-only / Hybrid / CELT-only), decoded from the
//!   packet TOC (`crate::packet::Toc`).
//! - For SILK-only and Hybrid packets, create one [`crate::range::RangeDecoder`] over the frame
//!   payload and drive `crate::silk::SilkDecoder::decode` first; for Hybrid packets the *same*
//!   range decoder instance continues to be consumed by CELT afterwards (this sharing is the
//!   subtle, bug-prone part the user's previous attempt got wrong — the SILK and CELT stages
//!   are NOT independently entropy-coded in Hybrid mode).
//! - For CELT-only and Hybrid packets, drive `crate::celt::decoder::CeltDecoder::decode_with_ec`
//!   with the appropriate start band (0 for CELT-only, 17 for Hybrid, per libopus `start_band`).
//! - Handle "redundancy" (SILK frames with an embedded CELT redundant frame for smooth mode
//!   switching) and the `celt_to_silk`/silk-to-celt crossfade via `smooth_fade`.
//! - Handle PLC (`data == None`) by re-running the last mode's decoder in packet-loss mode.
//! - Track `st.rangeFinal` (the final range of the last-consumed range coder), needed for the
//!   RFC 8251 conformance harness's `enc_final_range` cross-check (see `tests/conformance.rs`).
//! - Downmix/upmix between `stream_channels` and the API's `channels`, and apply `decode_gain`.
//!
//! None of this is implemented in wave 0; see module docs in `crate::silk` and `crate::celt` for
//! the sub-decoder contracts this function will drive.

use crate::celt::decoder::CeltDecoder;
use crate::packet::Toc;
use crate::silk::SilkDecoder;

/// Supported API sample rates, mirroring libopus `opus_decoder_init`'s validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleRate {
    Hz8000,
    Hz12000,
    Hz16000,
    Hz24000,
    Hz48000,
}

impl SampleRate {
    pub fn as_hz(self) -> u32 {
        match self {
            SampleRate::Hz8000 => 8000,
            SampleRate::Hz12000 => 12000,
            SampleRate::Hz16000 => 16000,
            SampleRate::Hz24000 => 24000,
            SampleRate::Hz48000 => 48000,
        }
    }
}

/// Decode errors, distinct from [`crate::packet::PacketError`] (which is purely about framing).
#[derive(Debug)]
pub enum DecodeError {
    Packet(crate::packet::PacketError),
    Mapping(crate::mapping::MappingError),
    InvalidChannelCount,
    /// Placeholder for wave-0 stub bodies; never returned by finished code.
    Unimplemented(&'static str),
}

impl From<crate::packet::PacketError> for DecodeError {
    fn from(e: crate::packet::PacketError) -> Self {
        DecodeError::Packet(e)
    }
}

pub type Result<T, E = DecodeError> = std::result::Result<T, E>;

/// The top-level, single-stream Opus decoder. C: `struct OpusDecoder`.
///
/// Wave 2 implements `symphonia_core::codecs::audio::{AudioDecoder, RegisterableAudioDecoder}`
/// for this type; wave 0 only establishes the struct shape and construction/reset API so
/// `crate::multistream` and the conformance harness (`tests/conformance.rs`) have a stable
/// target to call once wave 2 lands.
pub struct OpusDecoder {
    channels: u8,
    sample_rate: SampleRate,
    silk: SilkDecoder,
    celt: CeltDecoder,

    // Fields below mirror `OPUS_DECODER_RESET_START` onward in the C struct: cleared on reset.
    stream_channels: u8,
    prev_mode: Mode,
    frame_size: usize,
    prev_redundancy: bool,
    last_packet_duration: usize,
    range_final: u32,
}

/// C: `MODE_SILK_ONLY` / `MODE_HYBRID` / `MODE_CELT_ONLY`, plus libopus's `mode == 0` "no
/// previous frame decoded yet" sentinel represented here as `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    None,
    Silk,
    Hybrid,
    Celt,
}

impl OpusDecoder {
    /// C: `opus_decoder_init` (combined with allocation, unlike the C two-step
    /// `opus_decoder_create`/`opus_decoder_init` split which exists only for fixed-size
    /// allocation reasons that don't apply to a Rust `Vec`/`Box`-based port).
    pub fn try_new(sample_rate: SampleRate, channels: u8) -> Result<Self> {
        if channels != 1 && channels != 2 {
            return Err(DecodeError::InvalidChannelCount);
        }
        Ok(OpusDecoder {
            channels,
            sample_rate,
            silk: SilkDecoder::new(),
            celt: CeltDecoder::new(sample_rate.as_hz(), channels),
            stream_channels: channels,
            prev_mode: Mode::None,
            frame_size: sample_rate.as_hz() as usize / 400,
            prev_redundancy: false,
            last_packet_duration: 0,
            range_final: 0,
        })
    }

    /// C: the `OPUS_RESET_STATE` CTL path (clears fields from `OPUS_DECODER_RESET_START` on).
    pub fn reset(&mut self) {
        self.silk.reset();
        self.celt.reset();
        self.stream_channels = self.channels;
        self.prev_mode = Mode::None;
        self.frame_size = self.sample_rate.as_hz() as usize / 400;
        self.prev_redundancy = false;
        self.last_packet_duration = 0;
    }

    /// C: `opus_decoder_get_nb_samples` / final range accessor used by
    /// `OPUS_GET_FINAL_RANGE`. Exposed for the RFC 8251 conformance harness.
    pub fn final_range(&self) -> u32 {
        self.range_final
    }

    /// C: `opus_decode_native` -> `opus_decode_frame`. Wave 2 implements this; wave 0 leaves it
    /// as `todo!()` so the crate still compiles and the conformance harness can reference a
    /// stable signature (see `tests/conformance.rs::decode_vector`).
    ///
    /// `data == None` requests PLC for `frame_size` samples (C: `data == NULL` path).
    pub fn decode(&mut self, data: Option<&[u8]>, out: &mut [f32], frame_size: usize) -> Result<usize> {
        let _ = Toc::new(data.map(|d| d[0]).unwrap_or(0));
        let _ = (out, frame_size);
        todo!("wave 2: opus_decode_frame (SILK/Hybrid/CELT dispatch, PLC, redundancy)")
    }
}
