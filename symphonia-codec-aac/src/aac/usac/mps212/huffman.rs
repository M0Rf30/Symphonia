// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The entropy coding of the MPEG Surround parameter data (ISO/IEC 23003-1 5.2, `EcDataPairDec`).
//!
//! The structure follows the float MPEG Surround decoder of libxaac (`ixheaacd_mps_ecdatapairdec`
//! and `ixheaacd_huff_decode`, Apache-2.0).

use symphonia_core::errors::{Result, decode_error};
use symphonia_core::io::{BitReaderLtr, ReadBitsLtr};

use super::MAX_BANDS;
use super::tables::*;

/// The kind of parameter being decoded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ParamType {
    Cld,
    Icc,
    Ipd,
}

/// `DIFF_FREQ` and `DIFF_TIME`; the PCM/pilot index of the tables is not used without pilot
/// coding.
const DIFF_FREQ: usize = 0;
const DIFF_TIME: usize = 1;

/// Walk a Huffman tree. A positive entry is the next node; a non-positive entry `n` is a leaf of
/// the value `-(n + 1)`.
fn read_node(bs: &mut BitReaderLtr<'_>, tab: &[[i16; 2]]) -> Result<i32> {
    let mut node = 0usize;

    loop {
        let bit = bs.read_bit()? as usize;

        let Some(next) = tab.get(node).map(|n| i32::from(n[bit]))
        else {
            return decode_error("aac: usac mps212 invalid huffman tree");
        };

        if next <= 0 {
            return Ok(next);
        }

        node = next as usize;
    }
}

/// A leaf as a symbol value.
fn read_symbol(bs: &mut BitReaderLtr<'_>, tab: &[[i16; 2]]) -> Result<i32> {
    Ok(-(read_node(bs, tab)? + 1))
}

/// The largest number of values that share one PCM codeword of a `levels`-ary alphabet.
fn max_group_len(levels: u32) -> Result<usize> {
    Ok(match levels {
        3 => 5,
        7 => 6,
        11 => 2,
        13 | 19 => 4,
        25 => 3,
        51 => 4,
        4 | 8 | 15 | 16 | 26 | 31 => 1,
        _ => return decode_error("aac: usac mps212 invalid pcm alphabet"),
    })
}

/// Read `num_val` PCM values of `levels` levels, grouped into codewords. `put(index, value)`
/// receives the values in order.
fn pcm_decode(
    bs: &mut BitReaderLtr<'_>,
    num_val: usize,
    levels: u32,
    mut put: impl FnMut(usize, i32),
) -> Result<()> {
    let max_grp = max_group_len(levels)?;

    let mut i = 0;

    while i < num_val {
        let grp = max_grp.min(num_val - i);

        // ceil(grp * log2(levels)) bits.
        let range = u64::from(levels).pow(grp as u32);
        let bits = 64 - (range - 1).leading_zeros();

        let mut word = u64::from(bs.read_bits_leq32(bits)?);

        for j in 0..grp {
            let value = (word % u64::from(levels)) as i32;
            put(i + (grp - j - 1), value);
            word /= u64::from(levels);
        }

        i += max_grp;
    }

    Ok(())
}

/// Read a pair of 2-D symbols: `(data0, data1, escape)`.
fn read_2d(bs: &mut BitReaderLtr<'_>, tab: &[[i16; 2]]) -> Result<([i32; 2], bool)> {
    let node = read_node(bs, tab)?;

    if node == 0 {
        return Ok(([0, 1], true));
    }

    let sym = -(node + 1);
    Ok(([sym >> 4, sym & 0xf], false))
}

/// Restore the symmetry of a pair that was folded into a smaller alphabet.
fn sym_restore(bs: &mut BitReaderLtr<'_>, lav: i32, data: &mut [i32; 2], ipd: bool) -> Result<()> {
    let sum = data[0] + data[1];
    let diff = data[0] - data[1];

    if sum > lav {
        data[0] = -sum + (2 * lav + 1);
        data[1] = -diff;
    }
    else {
        data[0] = sum;
        data[1] = diff;
    }

    if !ipd && data[0] + data[1] != 0 && bs.read_bool()? {
        data[0] = -data[0];
        data[1] = -data[1];
    }

    if data[0] - data[1] != 0 && bs.read_bool()? {
        data.swap(0, 1);
    }

    Ok(())
}

/// The 1-D trees of a parameter type, by diff type.
fn tree_1d(ty: ParamType, diff: usize) -> &'static [[i16; 2]] {
    match ty {
        ParamType::Cld => &CLD_1D[diff],
        ParamType::Icc => &ICC_1D[diff],
        ParamType::Ipd => &IPD_1D[diff],
    }
}

/// Decode `num_val` values with a 1-D tree. With `p0` the first value is coded with the
/// first-band tree instead.
fn dec_1d(
    bs: &mut BitReaderLtr<'_>,
    ty: ParamType,
    diff: usize,
    out: &mut [i32],
    p0: bool,
) -> Result<()> {
    let mut start = 0;

    if p0 {
        let tab: &[[i16; 2]] = match ty {
            ParamType::Cld => &PART0_CLD,
            ParamType::Icc => &PART0_ICC,
            ParamType::Ipd => &IPD_HP0,
        };
        out[0] = read_symbol(bs, tab)?;
        start = 1;
    }

    let tab = tree_1d(ty, diff);

    for o in &mut out[start..] {
        let mut od = read_symbol(bs, tab)?;

        // CLD and ICC carry a sign for non-zero values, IPD does not.
        if ty != ParamType::Ipd && od != 0 && bs.read_bool()? {
            od = -od;
        }

        *o = od;
    }

    Ok(())
}

/// Decode `num_val` values with a 2-D tree in steps of `stride` into `out[i]`, which holds the
/// pairs. `p0` receives the first-band values.
#[allow(clippy::too_many_arguments)]
fn dec_2d(
    bs: &mut BitReaderLtr<'_>,
    ty: ParamType,
    diff: usize,
    pair: usize,
    out: &mut [[i32; 2]],
    num_val: usize,
    stride: usize,
    p0: [Option<&mut i32>; 2],
) -> Result<()> {
    let lav_idx = read_symbol(bs, &LAV_IDX)?;

    let lav = match ty {
        ParamType::Cld => 2 * lav_idx + 3,
        ParamType::Icc => 2 * lav_idx + 1,
        ParamType::Ipd => 2 * (if lav_idx == 0 { 3 } else { lav_idx - 1 }) + 1,
    };

    for slot in p0.into_iter().flatten() {
        let tab: &[[i16; 2]] = match ty {
            ParamType::Cld => &PART0_CLD,
            ParamType::Icc => &PART0_ICC,
            ParamType::Ipd => &IPD_HP0,
        };
        *slot = read_symbol(bs, tab)?;
    }

    let tree: &[[i16; 2]] = match (ty, lav) {
        (ParamType::Cld, 3) => &CLD_2D_LAV3[diff][pair],
        (ParamType::Cld, 5) => &CLD_2D_LAV5[diff][pair],
        (ParamType::Cld, 7) => &CLD_2D_LAV7[diff][pair],
        (ParamType::Cld, 9) => &CLD_2D_LAV9[diff][pair],
        (ParamType::Icc, 1) => &ICC_2D_LAV1[diff][pair],
        (ParamType::Icc, 3) => &ICC_2D_LAV3[diff][pair],
        (ParamType::Icc, 5) => &ICC_2D_LAV5[diff][pair],
        (ParamType::Icc, 7) => &ICC_2D_LAV7[diff][pair],
        (ParamType::Ipd, 1) => &IPD_2D_LAV1[diff][pair],
        (ParamType::Ipd, 3) => &IPD_2D_LAV3[diff][pair],
        (ParamType::Ipd, 5) => &IPD_2D_LAV5[diff][pair],
        (ParamType::Ipd, 7) => &IPD_2D_LAV7[diff][pair],
        _ => return decode_error("aac: usac mps212 invalid lav"),
    };

    let mut escapes = [0usize; MAX_BANDS];
    let mut num_esc = 0;

    let mut i = 0;
    while i < num_val {
        let (mut pair_val, escape) = read_2d(bs, tree)?;

        if escape {
            escapes[num_esc] = i;
            num_esc += 1;
        }
        else {
            sym_restore(bs, lav, &mut pair_val, ty == ParamType::Ipd)?;
        }

        out[i] = pair_val;
        i += stride;
    }

    if num_esc > 0 {
        let mut esc = [[0i32; 2]; MAX_BANDS];

        pcm_decode(bs, 2 * num_esc, (2 * lav + 1) as u32, |idx, v| esc[idx / 2][idx % 2] = v)?;

        // The PCM values alternate between the two members of the pair.
        for (k, &pos) in escapes[..num_esc].iter().enumerate() {
            out[pos] = [esc[k][0] - lav, esc[k][1] - lav];
        }
    }

    Ok(())
}

/// The state of one parameter type that `EcDataPairDec` reads.
pub(super) struct PairRequest<'a> {
    pub ty: ParamType,
    /// The previous values, per data band.
    pub history: &'a [i32; MAX_BANDS],
    pub data_bands: usize,
    pub pair: bool,
    pub coarse: bool,
    pub diff_time_back: bool,
}

/// `(quantizer levels, offset, attach one LSB)` of a parameter type.
fn quantizer(ty: ParamType, coarse: bool) -> (u32, i32, bool) {
    match (ty, coarse) {
        (ParamType::Cld, true) => (15, 7, false),
        (ParamType::Cld, false) => (31, 15, false),
        (ParamType::Icc, true) => (4, 0, false),
        (ParamType::Icc, false) => (8, 0, false),
        (ParamType::Ipd, true) => (8, 0, false),
        (ParamType::Ipd, false) => (16, 0, true),
    }
}

/// Decode one parameter set, or a pair of sets, into `out[0]` (and `out[1]` for a pair).
pub(super) fn ec_data_pair_dec(
    bs: &mut BitReaderLtr<'_>,
    req: &PairRequest<'_>,
    out: &mut [[i32; MAX_BANDS]; 2],
) -> Result<()> {
    let n = req.data_bands;
    let ty = req.ty;
    let (levels, offset, attach_lsb) = quantizer(ty, req.coarse);

    if n == 0 || n > MAX_BANDS {
        return decode_error("aac: usac mps212 invalid band count");
    }

    let pcm = bs.read_bool()?;

    if pcm {
        let pair = req.pair;
        let total = if pair { 2 * n } else { n };

        pcm_decode(bs, total, levels, |idx, v| {
            if pair {
                out[idx % 2][idx / 2] = v - offset;
            }
            else {
                out[0][idx] = v - offset;
            }
        })?;

        return Ok(());
    }

    let mut diff_type = [DIFF_FREQ, DIFF_FREQ];

    if req.pair || req.diff_time_back {
        diff_type[0] = bs.read_bit()? as usize;
    }

    if req.pair && (diff_type[0] == DIFF_FREQ || req.diff_time_back) {
        diff_type[1] = bs.read_bit()? as usize;
    }

    // cdg_scheme: HUFF_1D or HUFF_2D with FREQ_PAIR or TIME_PAIR.
    let two_d = bs.read_bool()?;
    let mut time_pair = false;

    if two_d && req.pair {
        time_pair = bs.read_bool()?;
    }

    let mut diff = [[0i32; MAX_BANDS]; 2];
    let channels = if req.pair { 2 } else { 1 };

    if !two_d {
        for c in 0..channels {
            let p0 = diff_type[c] == DIFF_FREQ;
            dec_1d(bs, ty, diff_type[c], &mut diff[c][..n], p0)?;
        }
    }
    else if !time_pair {
        // FREQ_PAIR: the sets are decoded one after the other, each in pairs of bands.
        for c in 0..channels {
            let mut num = n;
            let mut base = 0;
            let mut first = None;

            if diff_type[c] == DIFF_FREQ {
                first = Some(0);
                num -= 1;
                base = 1;
            }

            let rest = num % 2;
            num -= rest;

            let mut pairs = [[0i32; 2]; MAX_BANDS];
            let mut p0_val = 0;

            {
                let p0: [Option<&mut i32>; 2] = if first.is_some() {
                    if c == 0 { [Some(&mut p0_val), None] } else { [None, Some(&mut p0_val)] }
                }
                else {
                    [None, None]
                };

                dec_2d(bs, ty, diff_type[c], 0, &mut pairs, num, 2, p0)?;
            }

            if first.is_some() {
                diff[c][0] = p0_val;
            }

            if rest == 1 {
                let mut v = [0i32; 1];
                dec_1d(bs, ty, diff_type[c], &mut v, false)?;
                diff[c][base + num] = v[0];
            }

            let mut i = 0;
            while i + 1 < num {
                diff[c][base + i] = pairs[i][0];
                diff[c][base + i + 1] = pairs[i][1];
                i += 2;
            }
        }
    }
    else {
        // TIME_PAIR: one pair per band, covering both sets.
        let mut num = n;
        let mut base = 0;
        let mut p0 = [0i32; 2];
        let has_p0 = diff_type[0] == DIFF_FREQ || diff_type[1] == DIFF_FREQ;

        if has_p0 {
            num -= 1;
            base = 1;
        }

        let dt = if diff_type[0] == DIFF_TIME || diff_type[1] == DIFF_TIME {
            DIFF_TIME
        }
        else {
            DIFF_FREQ
        };

        let mut pairs = [[0i32; 2]; MAX_BANDS];

        {
            let [a, b] = &mut p0;
            let p0r: [Option<&mut i32>; 2] = if has_p0 { [Some(a), Some(b)] } else { [None, None] };
            dec_2d(bs, ty, dt, 1, &mut pairs, num, 1, p0r)?;
        }

        if has_p0 {
            diff[0][0] = p0[0];
            diff[1][0] = p0[1];
        }

        for i in 0..num {
            diff[0][base + i] = pairs[i][0];
            diff[1][base + i] = pairs[i][1];
        }
    }

    // Undo the differential coding.
    let mut data = [[0i32; MAX_BANDS]; 2];
    let mut direction_backwards = true;

    if (diff_type[0] == DIFF_TIME || diff_type[1] == DIFF_TIME) && req.pair {
        if diff_type[0] == DIFF_TIME && !req.diff_time_back {
            direction_backwards = false;
        }
        else if diff_type[1] == DIFF_TIME {
            direction_backwards = true;
        }
        else {
            direction_backwards = bs.read_bool()?;
        }
    }

    let mixed_time_pair = two_d && time_pair && diff_type[0] != diff_type[1];

    let freq_decode = |d: &[i32; MAX_BANDS], o: &mut [i32; MAX_BANDS]| {
        o[0] = d[0];
        for i in 1..n {
            o[i] = o[i - 1] + d[i];
        }
    };

    if direction_backwards {
        if diff_type[0] == DIFF_FREQ {
            freq_decode(&diff[0], &mut data[0]);
        }
        else {
            let mut msb = [0i32; MAX_BANDS];
            for i in 0..n {
                msb[i] = req.history[i] + offset;
                if attach_lsb {
                    msb[i] >>= 1;
                }
            }

            time_decode(&msb, &diff[0], &mut data[0], mixed_time_pair, n, 1);
        }

        if req.pair {
            if diff_type[1] == DIFF_FREQ {
                freq_decode(&diff[1], &mut data[1]);
            }
            else {
                let prev = data[0];
                time_decode(&prev, &diff[1], &mut data[1], mixed_time_pair, n, 1);
            }
        }
    }
    else {
        freq_decode(&diff[1], &mut data[1]);

        if diff_type[0] == DIFF_FREQ {
            freq_decode(&diff[0], &mut data[0]);
        }
        else {
            let next = data[1];
            time_decode(&next, &diff[0], &mut data[0], mixed_time_pair, n, -1);
        }
    }

    // Attach the least significant bit and remove the offset.
    for c in 0..channels {
        for i in 0..n {
            let msb = data[c][i];

            out[c][i] = if attach_lsb {
                let lsb = bs.read_bit()? as i32;
                ((msb << 1) | lsb) - offset
            }
            else {
                msb - offset
            };
        }
    }

    Ok(())
}

/// Time-differential decoding: `out = reference + sign * diff` (`sign` is 1 for a backward and
/// -1 for a forward reference). A mixed time pair keeps the first value as coded.
fn time_decode(
    reference: &[i32; MAX_BANDS],
    diff: &[i32; MAX_BANDS],
    out: &mut [i32; MAX_BANDS],
    mixed: bool,
    n: usize,
    sign: i32,
) {
    let start = usize::from(mixed);

    if mixed {
        out[0] = diff[0];
    }

    for i in start..n {
        out[i] = reference[i] + sign * diff[i];
    }
}
