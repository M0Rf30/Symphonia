// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The SILK sub-decoder. Ported from libopus `silk/*.c` (fixed-point / `SILK_FIXED`, which is
//! what libopus uses even in float builds — `silk_float` is only used by the SILK *encoder*'s
//! float variant, never the decoder). This module is kept strictly integer for bit-exactness
//! with the reference. Ported from libopus (BSD-3-Clause), see NOTICE.
//!
//! # Ownership (wave 1)
//!
//! This entire `silk/**` subtree is owned by a single wave-1 agent ("Silk"). The planned file
//! list below mirrors `silk/*.c` 1:1 so each Rust file can be diffed against exactly one C
//! translation unit:
//!
//! - `dec_api.rs` — `silk/dec_API.c`: `silk_InitDecoder`/`silk_ResetDecoder`/`silk_Decode`
//!   (the entry points re-exported as [`SilkDecoder::new`]/[`SilkDecoder::reset`]/
//!   [`SilkDecoder::decode`] below).
//! - `decode_frame.rs` — `silk/decode_frame.c`: `silk_decode_frame` (one 20/10ms sub-frame set).
//! - `decode_indices.rs` — `silk/decode_indices.c`: side-info symbol decoding.
//! - `decode_pulses.rs` — `silk/decode_pulses.c` (+ `shell_coder.c`): excitation pulse decoding.
//! - `decode_core.rs` — `silk/decode_core.c`: LTP + LPC synthesis core.
//! - `decode_parameters.rs` — `silk/decode_parameters.c`: gains/LSF/pitch-lag/LTP-coef decode.
//! - `gain_quant.rs` — `silk/gain_quant.c`.
//! - `nlsf_decode.rs` — `silk/NLSF_decode.c`.
//! - `nlsf_stabilize.rs` — `silk/NLSF_stabilize.c`.
//! - `nlsf2a.rs` — `silk/NLSF2A.c`.
//! - `lpc_inv_pred_gain.rs` — `silk/LPC_inv_pred_gain.c`.
//! - `bwexpander.rs` — `silk/bwexpander.c` (+ `bwexpander_32.c`).
//! - `decode_pitch.rs` — `silk/decode_pitch.c`.
//! - `plc.rs` — `silk/PLC.c`: packet-loss concealment (`FLAG_PACKET_LOST`).
//! - `cng.rs` — `silk/CNG.c`: comfort noise generation.
//! - `stereo_decode_pred.rs` — `silk/stereo_decode_pred.c`.
//! - `stereo_ms_to_lr.rs` — `silk/stereo_MS_to_LR.c`.
//! - `resampler.rs` (+ `resampler_private_*.rs`) — `silk/resampler.c` and friends: internal
//!   (8/12/16 kHz) rate to API output rate conversion.
//! - `tables_*.rs` — `silk/tables_*.c`: static codebooks (pulse/gain/LSF/pitch/NLSF-CB1-2).
//! - `structs.rs` — `silk/structs.h`: `silk_decoder_state`, `silk_decoder_control`,
//!   `silk_nsq_state`, `silk_CNG_struct`, `silk_PLC_struct`, one Rust struct each.
//!
//! Everything in this list is `todo!()` / absent in wave 0; only the public API surface below
//! (used by `crate::decoder`) is defined here so downstream wave-2 code has a stable target.

use crate::range::RangeDecoder;

/// C: `FLAG_DECODE_NORMAL` / `FLAG_PACKET_LOST` / `FLAG_DECODE_LBRR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeFlag {
    Normal,
    PacketLost,
    Lbrr,
}

/// C: `silk_DecControlStruct`. `internalSampleRate` and `prevPitchLag` are output (`O:`) fields
/// in the C struct; kept `pub` here since Rust has no in/out parameter distinction.
#[derive(Debug, Clone, Copy, Default)]
pub struct DecControl {
    pub n_channels_api: i32,
    pub n_channels_internal: i32,
    pub api_sample_rate: i32,
    /// O: internal sampling rate actually used (8000/12000/16000).
    pub internal_sample_rate: i32,
    pub payload_size_ms: i32,
    /// O: pitch lag of the previous frame (0 if unvoiced), in samples at 48 kHz.
    pub prev_pitch_lag: i32,
}

/// Decode errors specific to SILK (distinct from packet-framing errors).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SilkError {
    /// Mirrors libopus's few `SILK_*` internal error returns surfaced through `silk_Decode`.
    Internal,
}

pub type Result<T, E = SilkError> = std::result::Result<T, E>;

/// The SILK decoder state. C: the anonymous decoder object sized by
/// `silk_Get_Decoder_Size`/initialized by `silk_InitDecoder` (a `silk_decoder` wrapping one
/// `silk_decoder_state` per channel, per `silk/structs.h`).
pub struct SilkDecoder {
    // Populated by wave 1 with per-channel `silk_decoder_state` (see `structs.rs` in the file
    // list above); left empty in wave 0 so the type is constructible and movable.
    _private: (),
}

impl SilkDecoder {
    /// C: `silk_InitDecoder`.
    pub fn new() -> Self {
        SilkDecoder { _private: () }
    }

    /// C: `silk_ResetDecoder` (called on `OPUS_RESET_STATE` and internally on some mode
    /// transitions).
    pub fn reset(&mut self) {
        todo!("wave 1 (silk): silk_ResetDecoder")
    }

    /// C: `silk_Decode`. Decodes one SILK frame (or LBRR frame, or runs PLC) from `rd` into
    /// `out`, writing the number of samples produced to the return value.
    ///
    /// `new_packet` mirrors the C `newPacketFlag` (`1` on the first `silk_Decode` call for a
    /// given transport packet, resetting internal per-packet frame counters).
    ///
    /// Per libopus 1.5.2, `silk_Decode`'s `samplesOut` buffer is `opus_int16`, NOT float — the
    /// caller (`crate::decoder`, wave 2) is responsible for converting to `f32` alongside CELT's
    /// float output and/or hybrid mixing. `out` is therefore `&mut [i16]` here to match exactly.
    pub fn decode(
        &mut self,
        ctl: &mut DecControl,
        lost_flag: DecodeFlag,
        new_packet: bool,
        rd: &mut RangeDecoder<'_>,
        out: &mut [i16],
        n_samples_out: &mut usize,
    ) -> Result<()> {
        let _ = (ctl, lost_flag, new_packet, rd, out, n_samples_out);
        todo!("wave 1 (silk): silk_Decode (decode_frame/decode_indices/decode_pulses/decode_core)")
    }
}

impl Default for SilkDecoder {
    fn default() -> Self {
        Self::new()
    }
}
