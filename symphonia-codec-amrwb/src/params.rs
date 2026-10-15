// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Speech frame parameter extraction: bit un-sorting, pitch lag decoding and algebraic codebook
//! pulse decoding (3GPP TS 26.190 clauses 5.8 and 6.1, TS 26.201 clause 4; follows FFmpeg's
//! `libavcodec/amrwbdec.c` and `amr.h`).

use crate::tables::{BIT_ORDERINGS, FRAME_PARAMS, PULSES_NB_PER_MODE_TR};

/// Slot of the voice activity flag.
pub const VAD: usize = 0;
/// First slot of the seven ISF indices.
pub const ISP_ID: usize = 1;
/// First slot of the subframe parameters.
const SUBFRAME: usize = 8;
/// Number of slots per subframe.
const SUBFRAME_LEN: usize = 12;

/// Parameters of one subframe, as decoded from the bitstream.
#[derive(Clone, Copy, Default)]
pub struct SubFrame {
    /// Adaptive codebook (pitch) index.
    pub adap: u16,
    /// Long-term prediction filtering flag.
    pub ltp: u16,
    /// Vector-quantized adaptive and innovative gains.
    pub vq_gain: u16,
    /// High-band energy index (23.85 kbit/s only).
    pub hb_gain: u16,
    /// MSB parts of the algebraic codebook indices (higher modes only).
    pub pul_ih: [u16; 4],
    /// LSB parts of the algebraic codebook indices.
    pub pul_il: [u16; 4],
}

/// Parameters of one core speech frame.
#[derive(Clone, Copy, Default)]
pub struct Frame {
    /// Voice activity detection flag.
    pub vad: u16,
    /// Indices of the ISF sub-vectors.
    pub isp_id: [u16; 7],
    /// The four subframes.
    pub subframe: [SubFrame; 4],
}

/// Extracts the parameters of a speech frame of the given mode (0 to 8) from the speech bits
/// (the data following the table of contents byte). `data` must hold the whole speech frame.
pub fn unpack(mode: usize, data: &[u8]) -> Frame {
    let mut slots = [0u16; FRAME_PARAMS];

    // Bit-reordering table: [len, slot, bit positions...]..., 0.
    let mut table = BIT_ORDERINGS[mode].iter().copied();
    while let Some(len) = table.next().filter(|&len| len != 0) {
        let slot = table.next().unwrap_or(0) as usize;
        let mut field = 0u16;
        for _ in 0..len {
            let bit = usize::from(table.next().unwrap_or(0));
            // Bit `bit & 7` counted from the least significant bit of byte `bit >> 3`.
            let b = data.get(bit >> 3).map_or(0, |&byte| (byte >> (bit & 7)) & 1);
            field = (field << 1) | u16::from(b);
        }
        slots[slot] = field;
    }

    let mut frame = Frame { vad: slots[VAD], ..Default::default() };
    frame.isp_id.copy_from_slice(&slots[ISP_ID..ISP_ID + 7]);
    for (s, sf) in frame.subframe.iter_mut().enumerate() {
        let b = SUBFRAME + s * SUBFRAME_LEN;
        sf.adap = slots[b];
        sf.ltp = slots[b + 1];
        sf.vq_gain = slots[b + 2];
        sf.hb_gain = slots[b + 3];
        sf.pul_ih.copy_from_slice(&slots[b + 4..b + 8]);
        sf.pul_il.copy_from_slice(&slots[b + 8..b + 12]);
    }
    frame
}

/// Minimum pitch lag.
pub const P_DELAY_MIN: i32 = 34;
/// Maximum pitch lag.
pub const P_DELAY_MAX: i32 = 231;

/// Decodes the adaptive codebook index of the 12.65 kbit/s and higher modes into an integer and
/// a fractional (1/4 resolution) lag. In the first and third subframes the index is absolute;
/// otherwise it is relative to `base_lag_int` (TS 26.190 clause 6.1.1).
pub fn decode_pitch_lag_high(
    pitch_index: i32,
    base_lag_int: &mut i32,
    subframe: usize,
) -> (i32, i32) {
    if subframe == 0 || subframe == 2 {
        let (lag_int, lag_frac);
        if pitch_index < 376 {
            lag_int = (pitch_index + 137) >> 2;
            lag_frac = pitch_index - (lag_int << 2) + 136;
        }
        else if pitch_index < 440 {
            lag_int = (pitch_index + 257 - 376) >> 1;
            // The actual resolution is 1/2 but it is expressed as 1/4.
            lag_frac = (pitch_index - (lag_int << 1) + 256 - 376) * 2;
        }
        else {
            lag_int = pitch_index - 280;
            lag_frac = 0;
        }
        // The specification states that the base lag is the nearest integer to the lag minus 8,
        // but the reference code (and FFmpeg) always uses the floor.
        *base_lag_int =
            (lag_int - 8 - i32::from(lag_frac < 0)).clamp(P_DELAY_MIN, P_DELAY_MAX - 15);
        (lag_int, lag_frac)
    }
    else {
        let lag_int = (pitch_index + 1) >> 2;
        let lag_frac = pitch_index - (lag_int << 2);
        (lag_int + *base_lag_int, lag_frac)
    }
}

/// Decodes the adaptive codebook index of the 6.60 and 8.85 kbit/s modes (1/2 resolution
/// expressed as 1/4). In 6.60 kbit/s all subframes but the first are relative.
pub fn decode_pitch_lag_low(
    pitch_index: i32,
    base_lag_int: &mut i32,
    subframe: usize,
    mode_6k60: bool,
) -> (i32, i32) {
    if subframe == 0 || (subframe == 2 && !mode_6k60) {
        let (lag_int, lag_frac);
        if pitch_index < 116 {
            lag_int = (pitch_index + 69) >> 1;
            lag_frac = (pitch_index - (lag_int << 1) + 68) * 2;
        }
        else {
            lag_int = pitch_index - 24;
            lag_frac = 0;
        }
        *base_lag_int =
            (lag_int - 8 - i32::from(lag_frac < 0)).clamp(P_DELAY_MIN, P_DELAY_MAX - 15);
        (lag_int, lag_frac)
    }
    else {
        let lag_int = (pitch_index + 1) >> 1;
        let lag_frac = (pitch_index - (lag_int << 1)) * 2;
        (lag_int + *base_lag_int, lag_frac)
    }
}

/// Gets `len` bits of `x` starting at bit `lsb` (zero extended).
#[inline]
fn bit_str(x: u32, lsb: u32, len: u32) -> u32 {
    (x >> lsb) & ((1u32 << len) - 1)
}

/// Gets the bit of `x` at position `p`.
#[inline]
fn bit_pos(x: u32, p: u32) -> u32 {
    (x >> p) & 1
}

// The decode_[i]p_track functions decode exactly `i` pulse positions and amplitudes in a track
// using the pulse indexing of TS 26.190 clause 5.8.2. Results are written to `out`; a negative
// number denotes an amplitude of -1 (the position is 1-based, so the amplitude is the sign).
// `m` is the log2 of the number of potential positions and `off` the offset of the positions.

fn decode_1p_track(out: &mut [i32], code: u32, m: u32, off: u32) {
    let pos = (bit_str(code, 0, m) + off) as i32;
    out[0] = if bit_pos(code, m) != 0 { -pos } else { pos };
}

fn decode_2p_track(out: &mut [i32], code: u32, m: u32, off: u32) {
    let pos0 = (bit_str(code, m, m) + off) as i32;
    let pos1 = (bit_str(code, 0, m) + off) as i32;

    out[0] = if bit_pos(code, 2 * m) != 0 { -pos0 } else { pos0 };
    out[1] = if bit_pos(code, 2 * m) != 0 { -pos1 } else { pos1 };
    out[1] = if pos0 > pos1 { -out[1] } else { out[1] };
}

fn decode_3p_track(out: &mut [i32], code: u32, m: u32, off: u32) {
    let half_2p = bit_pos(code, 2 * m - 1) << (m - 1);

    decode_2p_track(out, bit_str(code, 0, 2 * m - 1), m - 1, off + half_2p);
    decode_1p_track(&mut out[2..], bit_str(code, 2 * m, m + 1), m, off);
}

fn decode_4p_track(out: &mut [i32], code: u32, m: u32, off: u32) {
    let b_offset = 1u32 << (m - 1);

    match bit_str(code, 4 * m - 2, 2) {
        // 0 pulses in A, 4 pulses in B or vice versa.
        0 => {
            let half_4p = bit_pos(code, 4 * m - 3) << (m - 1);
            let subhalf_2p = bit_pos(code, 2 * m - 3) << (m - 2);

            decode_2p_track(out, bit_str(code, 0, 2 * m - 3), m - 2, off + half_4p + subhalf_2p);
            decode_2p_track(
                &mut out[2..],
                bit_str(code, 2 * m - 2, 2 * m - 1),
                m - 1,
                off + half_4p,
            );
        }
        // 1 pulse in A, 3 pulses in B.
        1 => {
            decode_1p_track(out, bit_str(code, 3 * m - 2, m), m - 1, off);
            decode_3p_track(&mut out[1..], bit_str(code, 0, 3 * m - 2), m - 1, off + b_offset);
        }
        // 2 pulses in each half.
        2 => {
            decode_2p_track(out, bit_str(code, 2 * m - 1, 2 * m - 1), m - 1, off);
            decode_2p_track(&mut out[2..], bit_str(code, 0, 2 * m - 1), m - 1, off + b_offset);
        }
        // 3 pulses in A, 1 pulse in B.
        _ => {
            decode_3p_track(out, bit_str(code, m, 3 * m - 2), m - 1, off);
            decode_1p_track(&mut out[3..], bit_str(code, 0, m), m - 1, off + b_offset);
        }
    }
}

fn decode_5p_track(out: &mut [i32], code: u32, m: u32, off: u32) {
    let half_3p = bit_pos(code, 5 * m - 1) << (m - 1);

    decode_3p_track(out, bit_str(code, 2 * m + 1, 3 * m - 2), m - 1, off + half_3p);
    decode_2p_track(&mut out[3..], bit_str(code, 0, 2 * m + 1), m, off);
}

fn decode_6p_track(out: &mut [i32], code: u32, m: u32, off: u32) {
    let b_offset = 1u32 << (m - 1);
    // Which half has more pulses in cases 0 to 2.
    let half_more = bit_pos(code, 6 * m - 5) << (m - 1);
    let half_other = b_offset - half_more;

    match bit_str(code, 6 * m - 4, 2) {
        // 0 pulses in A, 6 pulses in B or vice versa.
        0 => {
            decode_1p_track(out, bit_str(code, 0, m), m - 1, off + half_more);
            decode_5p_track(&mut out[1..], bit_str(code, m, 5 * m - 5), m - 1, off + half_more);
        }
        // 1 pulse in A, 5 pulses in B or vice versa.
        1 => {
            decode_1p_track(out, bit_str(code, 0, m), m - 1, off + half_other);
            decode_5p_track(&mut out[1..], bit_str(code, m, 5 * m - 5), m - 1, off + half_more);
        }
        // 2 pulses in A, 4 pulses in B or vice versa.
        2 => {
            decode_2p_track(out, bit_str(code, 0, 2 * m - 1), m - 1, off + half_other);
            decode_4p_track(
                &mut out[2..],
                bit_str(code, 2 * m - 1, 4 * m - 4),
                m - 1,
                off + half_more,
            );
        }
        // 3 pulses in A, 3 pulses in B.
        _ => {
            decode_3p_track(out, bit_str(code, 3 * m - 2, 3 * m - 2), m - 1, off);
            decode_3p_track(&mut out[3..], bit_str(code, 0, 3 * m - 2), m - 1, off + b_offset);
        }
    }
}

/// Decodes the algebraic codebook indices of a subframe into the fixed (innovation) vector
/// (TS 26.190 clause 6.1.2). `mode` is 0 to 8.
pub fn decode_fixed_vector(fixed_vector: &mut [f32; 64], sub: &SubFrame, mode: usize) {
    // For each track, the 1-based pulse positions multiplied by the pulse amplitude (+1/-1).
    let mut sig_pos = [[0i32; 6]; 4];
    let spacing = if mode == 0 { 2 } else { 4 };
    let lo = |i: usize| u32::from(sub.pul_il[i]);
    let hi = |i: usize| u32::from(sub.pul_ih[i]);

    match mode {
        0 => {
            for i in 0..2 {
                decode_1p_track(&mut sig_pos[i], lo(i), 5, 1);
            }
        }
        1 => {
            for i in 0..4 {
                decode_1p_track(&mut sig_pos[i], lo(i), 4, 1);
            }
        }
        2 => {
            for i in 0..4 {
                decode_2p_track(&mut sig_pos[i], lo(i), 4, 1);
            }
        }
        3 => {
            for i in 0..2 {
                decode_3p_track(&mut sig_pos[i], lo(i), 4, 1);
            }
            for i in 2..4 {
                decode_2p_track(&mut sig_pos[i], lo(i), 4, 1);
            }
        }
        4 => {
            for i in 0..4 {
                decode_3p_track(&mut sig_pos[i], lo(i), 4, 1);
            }
        }
        5 => {
            for i in 0..4 {
                decode_4p_track(&mut sig_pos[i], lo(i) + (hi(i) << 14), 4, 1);
            }
        }
        6 => {
            for i in 0..2 {
                decode_5p_track(&mut sig_pos[i], lo(i) + (hi(i) << 10), 4, 1);
            }
            for i in 2..4 {
                decode_4p_track(&mut sig_pos[i], lo(i) + (hi(i) << 14), 4, 1);
            }
        }
        _ => {
            for i in 0..4 {
                decode_6p_track(&mut sig_pos[i], lo(i) + (hi(i) << 11), 4, 1);
            }
        }
    }

    fixed_vector.fill(0.0);

    for (i, track) in sig_pos.iter().enumerate() {
        for &sp in &track[..usize::from(PULSES_NB_PER_MODE_TR[mode][i])] {
            // Positions are 1-based and bounded by the field widths: at most 16 per track (32 in
            // 6.60 kbit/s, which has a spacing of 2), so the index is at most 63.
            let pos = (sp.unsigned_abs() as usize - 1) * spacing + i;
            fixed_vector[pos] += if sp < 0 { -1.0 } else { 1.0 };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::CF_SIZES_WB;

    /// All bit positions of every table are within the speech frame, and every bit of the frame
    /// is used exactly once.
    #[test]
    fn verify_bit_orderings_cover_frame() {
        for mode in 0..9 {
            let bits = usize::from(CF_SIZES_WB[mode]);
            let mut seen = vec![0u8; bits];
            let mut table = BIT_ORDERINGS[mode].iter().copied();
            while let Some(len) = table.next().filter(|&l| l != 0) {
                let slot = table.next().unwrap() as usize;
                assert!(slot < FRAME_PARAMS);
                for _ in 0..len {
                    // The positions number the bits of a byte from the least significant bit,
                    // while the frame bits are numbered from the most significant bit.
                    let pos = usize::from(table.next().unwrap());
                    seen[(pos & !7) + 7 - (pos & 7)] += 1;
                }
            }
            assert!(table.next().is_none());
            assert!(seen.iter().all(|&n| n == 1), "mode {mode}: bits not covered exactly once");
        }
    }

    /// Whatever the bits, pulse positions stay inside the subframe.
    #[test]
    fn verify_fixed_vector_bounds() {
        let mut state = 0x9e37_79b9u32;
        let mut v = [0.0f32; 64];
        for mode in 0..9 {
            for _ in 0..20000 {
                let mut next = || {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    state
                };
                let mut sub = SubFrame::default();
                for i in 0..4 {
                    sub.pul_il[i] = (next() & 0x3fff) as u16;
                    sub.pul_ih[i] = (next() & 0x7ff) as u16;
                }
                decode_fixed_vector(&mut v, &sub, mode);
                assert!(v.iter().all(|x| x.is_finite()));
            }
        }
    }
}
