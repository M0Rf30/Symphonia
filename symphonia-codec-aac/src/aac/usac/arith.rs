// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The context-adaptive binary arithmetic decoder of the USAC frequency-domain coder.
//!
//! This implements `arith_decode()` and the `ari_*` context machinery of ISO/IEC 23003-3
//! (MPEG-D USAC) clauses 7.5.2.1 through 7.5.2.3 (the `ac_spectral_data()` tool, shared by
//! `fd_channel_stream()` and the TCX branch of `lpd_channel_stream()`). The context state is
//! therefore owned per channel so that consecutive windows (and, in LPD, TCX frames) share it.

use symphonia_core::errors::{Result, decode_error};
use symphonia_core::io::{BitReaderLtr, FiniteBitStream, ReadBitsLtr};

use super::tables::{AC_HASH_M, AC_LOOKUP_M, AC_LSB_CDFS, AC_MSB_CDFS};

/// The escape symbol of the most-significant 2-tuple model (ISO/IEC 23003-3 7.5.2.3).
const VALUE_ESCAPE: usize = 16;

/// The maximum number of spectral lines the context state can span: the 1024-line long window
/// uses 512 pairs. Two extra slots (`pairs`, `pairs + 1`) carry the `ari_rescaling` tail that
/// seeds the next context resize.
const MAX_PAIRS: usize = 512;
const CONTEXT_LEN: usize = MAX_PAIRS + 2;

/// Per-channel state of the arithmetic coder, kept alive across frames.
///
/// `prev` stores, in pairs, the magnitudes of the previous frame's decoded 2-tuples
/// (`ari_cf_m` model "previous frame" input, plus the two rescaling tail values). `prev_len`
/// is the number of spectral lines of that previous frame, used to rescale the context when the
/// transform size changes (FD long/short and TCX 256/512/1024 transitions).
pub struct ArithState {
    prev: [u8; CONTEXT_LEN],
    prev_len: usize,
}

impl ArithState {
    pub fn new() -> Self {
        ArithState { prev: [0; CONTEXT_LEN], prev_len: 0 }
    }

    /// Forget the context model (called on a decoder reset / seek).
    pub fn reset(&mut self) {
        self.prev = [0; CONTEXT_LEN];
        self.prev_len = 0;
    }

    /// Decode `coded_len` spectral coefficients into `out` using the 2-tuple arithmetic model.
    ///
    /// * `reset` - the `arith_reset_flag`: true for the first window of an independent frame
    ///   (or an explicitly signalled reset), which discards the previous-frame context.
    /// * `coded_len` - the number of transmitted lines (`swb_offset[max_sfb]` for FD, `tcx_lg`
    ///   for TCX), always even. Lines `coded_len..n` of `out` are zeroed.
    /// * `n` - the window transform length (`ccfl` for a long window, `ccfl / 8` for a short
    ///   window), always even.
    ///
    /// The reader is advanced past the arithmetic payload, leaving the bit position at the
    /// start of the per-coefficient sign bits (the 14-bit `ari_termination` rollback of
    /// ISO/IEC 23003-3 7.5.2.3 is applied internally). Signs are consumed from the same reader
    /// and applied to the returned magnitudes.
    pub fn decode_spectrum(
        &mut self,
        bs: &mut BitReaderLtr<'_>,
        reset: bool,
        coded_len: usize,
        n: usize,
        out: &mut [i32],
    ) -> Result<()> {
        debug_assert!(coded_len <= n);
        debug_assert!(n % 2 == 0 && coded_len % 2 == 0);

        let pairs = n / 2;

        if reset {
            self.prev = [0; CONTEXT_LEN];
        }
        else if self.prev_len != n {
            if self.prev_len == 0 {
                return decode_error("aac: usac arith context without previous frame");
            }
            resize_context(&mut self.prev, self.prev_len / 2, pairs);
        }
        self.prev_len = n;

        if coded_len == 0 {
            // No payload: the context model is still advanced (all pairs set to the "empty"
            // magnitude, ISO/IEC 23003-3 7.5.2.3 `arith_map_context` with `lg == 0`).
            self.prev[..pairs].fill(1);
            out[..n].fill(0);
            return Ok(());
        }

        // Decode into a cloned reader so the original can be rolled back by the 14-bit
        // `ari_termination` padding without losing the ability to measure how far the payload
        // reached.
        let mut dec = bs.clone();
        let bits_before = dec.bits_left();

        let mut state = RangeState { low: 0, high: 0xffff, value: dec.read_bits_leq32(16)? as u16 };

        // The three previous pairs' magnitudes of the *current* frame (the `histories` of the
        // context state); reset per window by construction.
        let mut histories = [0u8; 3];

        // `ari_hash_m` state increment: a 16-bit value that wraps (ISO/IEC 23003-3 7.5.2.2).
        let mut state_increment = (self.prev[0] as u16) << 12;

        let mut decoded = 0usize;
        while decoded < coded_len / 2 {
            // Compose the context state from the accumulated increment, the previous frame's
            // following pair and the most recent current-frame pair.
            let mut context =
                u32::from(state_increment >> 8) + (u32::from(self.prev[decoded + 1]) << 8);
            context = (context << 4) + u32::from(histories[0]);
            state_increment = context as u16;

            if decoded > 3 && (histories[0] as u16 + histories[1] as u16 + histories[2] as u16) < 5
            {
                context += 0x1_0000;
            }

            let mut level = 0usize;
            let mut escapes = 0usize;
            let symbol = loop {
                let pk = probability_index(context + ((escapes as u32) << 17));
                let symbol = decode_symbol(&mut dec, &mut state, &AC_MSB_CDFS[pk])?;
                if symbol < VALUE_ESCAPE {
                    break symbol;
                }
                level += 1;
                if level > 23 {
                    return decode_error("aac: usac arith escape overflow");
                }
                escapes = (escapes + 1).min(7);
            };

            // `ARITH_STOP` (ISO/IEC 23003-3 7.5.2.3): a zero tuple after one or more escapes
            // terminates the payload.
            if symbol == 0 && escapes != 0 {
                break;
            }

            let (mut a, mut b) =
                if symbol == 0 { (0, 0) } else { ((symbol & 3) as i32, (symbol >> 2) as i32) };

            // Least-significant bits of the escaped magnitudes.
            for _ in 0..level {
                let model = if a == 0 {
                    AC_LSB_CDFS[1]
                }
                else if b == 0 {
                    AC_LSB_CDFS[0]
                }
                else {
                    AC_LSB_CDFS[2]
                };
                let low = decode_symbol(&mut dec, &mut state, &model)?;
                a = (a << 1) | (low as i32 & 1);
                b = (b << 1) | (low as i32 >> 1);
            }

            out[decoded * 2] = a;
            out[decoded * 2 + 1] = b;

            let amplitude = (a + b + 1).min(15) as u8;
            histories = [amplitude, histories[0], histories[1]];
            self.prev[decoded] = amplitude;

            decoded += 1;
        }

        // Roll the original reader back 14 bits so the sign bits (read next) start at the
        // correct position (`ari_termination`, ISO/IEC 23003-3 7.5.2.3).
        if dec.bits_left() < super::PAD_BITS as u64 - 14 {
            return decode_error("aac: usac arithmetic data is truncated");
        }

        let consumed = bits_before - dec.bits_left();
        bs.ignore_bits(consumed.saturating_sub(14) as u32)?;

        // Undecoded pairs keep the "empty" magnitude for the next frame's context.
        self.prev[decoded..pairs].fill(1);
        out[decoded * 2..n].fill(0);

        // Signs: one bit per non-zero coefficient, zero meaning negative.
        for coefficient in &mut out[..coded_len] {
            if *coefficient != 0 && !bs.read_bool()? {
                *coefficient = -*coefficient;
            }
        }

        Ok(())
    }
}

/// The arithmetic coder interval state (`low`/`high`/`value` of `arith_decode`).
#[derive(Clone, Copy)]
struct RangeState {
    low: u16,
    high: u16,
    value: u16,
}

/// Decode one symbol from `cdf` (cumulative frequencies, most-significant first, terminated by
/// zero), ISO/IEC 23003-3 7.5.2.1.
#[allow(clippy::collapsible_if)] // the renormalisation loop mirrors the spec flowchart
fn decode_symbol(bs: &mut BitReaderLtr<'_>, state: &mut RangeState, cdf: &[u16]) -> Result<usize> {
    let range = u32::from(state.high) - u32::from(state.low) + 1;
    let scaled = ((u32::from(state.value) - u32::from(state.low) + 1) << 14) - 1;

    let mut symbol = 0usize;
    while symbol + 1 < cdf.len() && u32::from(cdf[symbol]) * range > scaled {
        symbol += 1;
    }

    let mut high = u32::from(state.high);
    if symbol != 0 {
        high = u32::from(state.low) + ((range * u32::from(cdf[symbol - 1])) >> 14) - 1;
    }
    let mut low = u32::from(state.low) + ((range * u32::from(cdf[symbol])) >> 14);
    let mut value = u32::from(state.value);

    loop {
        if high & 0x8000 != 0 {
            if low & 0x8000 == 0 {
                if low & 0x4000 != 0 && high & 0x4000 == 0 {
                    low -= 0x4000;
                    high -= 0x4000;
                    value -= 0x4000;
                }
                else {
                    break;
                }
            }
        }
        low = (low << 1) & 0xffff;
        high = ((high << 1) | 1) & 0xffff;
        value = ((value << 1) | u32::from(bs.read_bool()?)) & 0xffff;
    }

    state.low = low as u16;
    state.high = high as u16;
    state.value = value as u16;

    Ok(symbol)
}

/// Map a context state onto a probability-table index via `ari_lookup_m`
/// (ISO/IEC 23003-3 7.5.2.2). The hash table is sorted by context code, so a binary search
/// finds the first entry whose context code is not less than `key`; a miss falls back to the
/// adjacent `ari_lookup_m` entry.
fn probability_index(context: u32) -> usize {
    let key = context;
    let mut i_min = -1isize;
    let mut i_max = AC_HASH_M.len() as isize - 1;

    while i_max - i_min > 1 {
        let i = i_min + (i_max - i_min) / 2;
        let j = AC_HASH_M[i as usize];
        if key < (j >> 8) {
            i_max = i;
        }
        else if key > (j >> 8) {
            i_min = i;
        }
        else {
            return (j & 0xff) as usize;
        }
    }

    AC_LOOKUP_M[i_max as usize] as usize
}

/// Rescale the previous-frame context when the transform length changes between frames
/// (ISO/IEC 23003-3 7.5.2.2 `arith_map_context`). The factor is one of 2, 4 or 8, chosen from
/// the length ratio, and the `input`/`input + 1` tail pair is carried over to the new size.
fn resize_context(context: &mut [u8], input: usize, output: usize) {
    let factor = if input < output {
        if input < output / 4 {
            8
        }
        else if input == output / 4 {
            4
        }
        else {
            2
        }
    }
    else if output < input / 4 {
        8
    }
    else if output == input / 4 {
        4
    }
    else {
        2
    };

    if input < output {
        context[output] = context[input];
        context[output + 1] = context[input + 1];
        let mut out = output;
        for source in (0..input).rev() {
            for _ in 0..factor {
                out -= 1;
                context[out] = context[source];
            }
        }
    }
    else {
        for out in 0..output {
            context[out] = context[out * factor];
        }
        context[output] = context[input];
        context[output + 1] = context[input + 1];
    }
}
