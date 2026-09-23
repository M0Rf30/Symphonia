// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The CELT decoder proper. Ported from libopus `celt/celt_decoder.c`
//! (`struct OpusCustomDecoder`, `celt_decoder_init`, `celt_decode_with_ec`).
//! Ported from libopus (BSD-3-Clause), see NOTICE. Owner (wave 1): "CeltSynthesis".
//!
//! This is the integration point wave 1's "CeltSynthesis" agent writes purely against the
//! signatures in [`crate::celt::bands`], [`crate::celt::rate`], [`crate::celt::quant_bands`],
//! [`crate::celt::cwrs`]/[`crate::celt::vq`] (owned by "CeltBitstream") plus this module's own
//! [`crate::celt::mdct`]/[`crate::celt::kiss_fft`]/[`crate::celt::pitch`]/[`crate::celt::lpc`]/
//! [`crate::celt::celt`] helpers.

use crate::celt::modes::CeltMode;
use crate::range::RangeDecoder;

/// C: `struct OpusCustomDecoder`, restricted to decoder-relevant fields (no `arch`/encoder
/// fields).
pub struct CeltDecoder {
    mode: &'static CeltMode,
    overlap: i32,
    channels: i32,
    stream_channels: i32,
    downsample: i32,
    start: i32,
    end: i32,
    signalling: bool,
    disable_inv: bool,

    // Cleared on `celt_decoder_ctl(CELT_RESET_STATE)` / reset, C: everything from
    // `DECODE_BUFFER_SIZE` onward (`decode_mem`, `lpc`, `old_e_bands`, `old_log_e`,
    // `old_log_e2`, `background_log_e`, PLC state, `postfilter_*`, `rng`, `error`).
    rng: u32,
    error: i32,
    postfilter_period: i32,
    postfilter_period_old: i32,
    postfilter_gain: f32,
    postfilter_gain_old: f32,
    postfilter_tapset: i32,
    postfilter_tapset_old: i32,
    // `decode_mem`/`lpc`/`old_e_bands` etc. are per-channel `Vec<f32>`s populated by wave 1;
    // omitted here (empty) since only the struct shape/API is wave 0's job.
}

impl CeltDecoder {
    /// C: `celt_decoder_init` (`opus_custom_decoder_init` restricted to the built-in 48 kHz
    /// mode, matching how `opus_decoder.c` always calls it).
    pub fn new(sample_rate: u32, channels: u8) -> Self {
        let downsample = match sample_rate {
            48000 => 1,
            24000 => 2,
            16000 => 3,
            12000 => 4,
            8000 => 6,
            _ => 1,
        };
        CeltDecoder {
            mode: &crate::celt::modes::MODE_48000_960,
            overlap: crate::celt::modes::MODE_48000_960.overlap,
            channels: channels as i32,
            stream_channels: channels as i32,
            downsample,
            start: 0,
            end: crate::celt::modes::MODE_48000_960.effective_ebands,
            signalling: true,
            disable_inv: channels == 1,
            rng: 0,
            error: 0,
            postfilter_period: 0,
            postfilter_period_old: 0,
            postfilter_gain: 0.0,
            postfilter_gain_old: 0.0,
            postfilter_tapset: 0,
            postfilter_tapset_old: 0,
        }
    }

    /// C: `celt_decoder_ctl(CELT_RESET_STATE)`.
    pub fn reset(&mut self) {
        todo!("wave 1 (celt/CeltSynthesis): CELT_RESET_STATE (clear decode_mem/lpc/old_e_bands/PLC state)")
    }

    /// C: `celt_decoder_ctl(CELT_SET_START_BAND(x))`. Hybrid mode uses `start = 17`.
    pub fn set_start_band(&mut self, start: i32) {
        self.start = start;
    }

    /// C: `celt_decoder_ctl(CELT_SET_END_BAND(x))`.
    pub fn set_end_band(&mut self, end: i32) {
        self.end = end;
    }

    /// C: `celt_decoder_ctl(CELT_SET_CHANNELS(x))` — the number of *coded* stream channels,
    /// which may differ from `self.channels` (the API-level output channel count).
    pub fn set_channels(&mut self, stream_channels: i32) {
        self.stream_channels = stream_channels;
    }

    /// C: `celt_decoder_ctl(CELT_SET_SIGNALLING(x))`. Opus always disables in-band signalling
    /// (`0`), since mode/bandwidth signalling is carried by the Opus TOC instead.
    pub fn set_signalling(&mut self, signalling: bool) {
        self.signalling = signalling;
    }

    /// C: `celt_decoder_ctl(OPUS_GET_AND_CLEAR_ERROR(&x))` (conceptually — libopus has no
    /// direct equivalent CTL for CELT; `st->error` is read after `celt_decode_with_ec` returns
    /// nonzero instead). Returns and clears the last decode error code, if any.
    pub fn get_and_clear_error(&mut self) -> i32 {
        let e = self.error;
        self.error = 0;
        e
    }

    /// C: `celt_decoder_ctl(CELT_GET_AND_CLEAR_ERROR(&x))`'s sibling `st->rng` accessor, used
    /// by `opus_decode_frame` to compute `st->rangeFinal` for Hybrid/CELT packets.
    pub fn final_range(&self) -> u32 {
        self.rng
    }

    /// C: `celt_decode_with_ec`. `data == None` requests PLC for `frame_size` samples (mirrors
    /// the C `data == NULL` convention, matching `crate::decoder::OpusDecoder::decode`).
    /// `rd` is `None` exactly when `data` is `None` (PLC never touches the range coder); when
    /// `Some`, it is the *same* range-coder instance SILK decoded from, for Hybrid packets
    /// (see `crate::decoder` module docs on hybrid range-coder sharing).
    pub fn decode_with_ec(
        &mut self,
        data: Option<&[u8]>,
        out: &mut [f32],
        frame_size: i32,
        rd: Option<&mut RangeDecoder<'_>>,
        accum: bool,
    ) -> Result<usize, i32> {
        let _ = (data, out, frame_size, rd, accum);
        todo!(
            "wave 1 (celt/CeltSynthesis): celt_decode_with_ec (band decode dispatch via \
             crate::celt::bands, MDCT synthesis, comb filter, PLC)"
        )
    }
}
