// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The fixed-point iLBC synthesis, ported from FFmpeg `libavcodec/ilbcdec.c` (itself derived from
//! the WebRTC fixed-point decoder, RFC 3951 section 4). All arithmetic reproduces the reference's
//! 16/32-bit integer behaviour (wrapping where the reference overflows) so that the output is
//! bit-exact.

use crate::frame::{Frame, Mode};
use crate::tables::{
    ALPHA, CB_FILTERS_REV, COS_DERIVATIVE_TBL, COS_TBL, FRG_QUANT_MOD, HP_OUT_COEFFS, ILBC_GAIN,
    ILBC_STATE, LSF_CODEBOOK, LSF_DIM_CODEBOOK, LSF_SIZE_CODEBOOK, LSF_WEIGHT_20MS,
    LSF_WEIGHT_30MS,
};

/// Order of the LPC filters.
const LPC_FILTERORDER: usize = 10;
/// Samples per sub-block.
const SUBL: usize = 40;
/// Samples of the codebook memory (`CB_MEML`).
const CB_MEML: usize = 147;
/// Length of the memory of the start state's codebook (`ST_MEM_L_TBL`).
const ST_MEM_L_TBL: usize = 85;
/// Samples of the start state, scalar part plus adaptive part (`STATE_LEN`).
const STATE_LEN: usize = 80;
/// Half-length of the codebook interpolation filter.
const CB_HALFFILTERLEN: usize = 4;
/// Length of the codebook interpolation filter.
const CB_FILTERLEN: usize = 8;
/// Longest frame, in samples.
pub const BLOCKL_MAX: usize = 240;
/// The most sub-blocks of a frame.
const NSUB_MAX: usize = 6;

/// Clips to the range of the 32-bit fixed-point filters of the reference.
fn clip_q12(o: i32) -> i32 {
    o.clamp(-134_217_728, 134_215_679)
}

/// `filter_mafq12`: `out[i] = sum_j b[j] * buf[base + i - j]`, Q12.
fn filter_ma(buf: &[i16], base: usize, out: &mut [i16], b: &[i16], length: usize) {
    for (i, o_i) in out.iter_mut().enumerate().take(length) {
        let mut o = 0i32;
        for (j, &bj) in b.iter().enumerate() {
            o = o.wrapping_add(i32::from(bj) * i32::from(buf[base + i - j]));
        }
        *o_i = ((clip_q12(o) + 2048) >> 12) as i16;
    }
}

/// `filter_arfq12`, in place on `buf[base..base + length]`; `buf[base - 10..base]` holds the
/// filter state.
fn filter_ar(buf: &mut [i16], base: usize, coefs: &[i16], length: usize) {
    for i in 0..length {
        let mut sum = 0i32;
        for j in (1..=LPC_FILTERORDER).rev() {
            sum = sum.wrapping_add(i32::from(coefs[j]) * i32::from(buf[base + i - j]));
        }
        let out = (i32::from(coefs[0]) * i32::from(buf[base + i])).wrapping_sub(sum);
        buf[base + i] = ((clip_q12(out) + 2048) >> 12) as i16;
    }
}

/// `index_conv`: expands the 7-bit stage-2/3 indices of sub-block 1 into the extended codebook.
fn index_conv(index: &mut [usize; 15]) {
    for k in 4..6 {
        if index[k] >= 44 && index[k] < 108 {
            index[k] += 64;
        }
        else if index[k] >= 108 && index[k] < 128 {
            index[k] += 128;
        }
    }
}

/// `lsf_dequantization`.
fn lsf_dequantization(lsfdeq: &mut [i16; 20], index: &[usize; 6], lpc_n: usize) {
    for set in 0..lpc_n {
        let mut pos = 0;
        let mut cb_pos = 0;
        for i in 0..3 {
            let dim = usize::from(LSF_DIM_CODEBOOK[i]);
            let src = cb_pos + index[3 * set + i] * dim;
            lsfdeq[set * LPC_FILTERORDER + pos..][..dim]
                .copy_from_slice(&LSF_CODEBOOK[src..][..dim]);
            pos += dim;
            cb_pos += usize::from(LSF_SIZE_CODEBOOK[i]) * dim;
        }
    }
}

/// `lsf_check_stability` with `dim = LPC_FILTERORDER`.
fn lsf_check_stability(lsf: &mut [i16; 20], nb_vectors: usize) {
    for _ in 0..2 {
        for m in 0..nb_vectors {
            for k in 0..LPC_FILTERORDER - 1 {
                let i = m * LPC_FILTERORDER + k;
                let a = i32::from(lsf[i]);
                let b = i32::from(lsf[i + 1]);

                if b - a < 319 {
                    if b < a {
                        lsf[i + 1] = (a + 160) as i16;
                        lsf[i] = (i32::from(lsf[i + 1]) - 160) as i16;
                    }
                    else {
                        lsf[i] = (a - 160) as i16;
                        lsf[i + 1] = (b + 160) as i16;
                    }
                }

                lsf[i] = lsf[i].clamp(82, 25723);
            }
        }
    }
}

/// `lsf_interpolate`.
fn lsf_interpolate(out: &mut [i16; 10], in1: &[i16], in2: &[i16], coef: i32) {
    let invcoef = 16384 - coef;
    for i in 0..LPC_FILTERORDER {
        out[i] = ((coef * i32::from(in1[i]) + invcoef * i32::from(in2[i]) + 8192) >> 14) as i16;
    }
}

/// `lsf2lsp`.
fn lsf2lsp(lsf: &[i16; 10]) -> [i16; 10] {
    let mut lsp = [0i16; 10];
    for i in 0..LPC_FILTERORDER {
        // 20861: 1 / (2 pi) in Q17. The upper 8 bits give the table index, the lower 8 the
        // difference that is approximated linearly.
        let freq = ((i32::from(lsf[i]) * 20861) >> 15) as i16;
        let k = ((freq >> 8) as usize).min(63);
        let diff = i32::from(freq & 0xff);
        let tmp = i32::from(COS_DERIVATIVE_TBL[k]) * diff;
        lsp[i] = (i32::from(COS_TBL[k]) + (tmp >> 12)) as i16;
    }
    lsp
}

/// `get_lsp_poly`: `lsp` is the even or odd LSP subsequence start.
fn get_lsp_poly(lsp: &[i16], f: &mut [i32; 6]) {
    f[0] = 16_777_216;
    f[1] = i32::from(lsp[0]) * -1024;

    let mut l = 2;
    let mut k = 2;
    for i in 2..=5 {
        f[l] = f[l - 2];

        for _ in (2..=i).rev() {
            let high = f[l - 1] >> 16;
            let low = (f[l - 1] - (high << 16)) >> 1;
            let c = i32::from(lsp[k]);

            let tmp = (high.wrapping_mul(c)).wrapping_mul(4).wrapping_add(((low * c) >> 15) * 4);

            f[l] = f[l].wrapping_add(f[l - 2]);
            f[l] = f[l].wrapping_sub(tmp);
            l -= 1;
        }

        f[l] = f[l].wrapping_sub(i32::from(lsp[k]) * (1 << 10));
        l += i;
        k += 2;
    }
}

/// `lsf2poly`: LSF to LPC polynomial in Q12.
fn lsf2poly(a: &mut [i16], lsf: &[i16; 10]) {
    let lsp = lsf2lsp(lsf);
    let mut f0 = [0i32; 6];
    let mut f1 = [0i32; 6];

    get_lsp_poly(&lsp[0..], &mut f0);
    get_lsp_poly(&lsp[1..], &mut f1);

    for i in (1..=5).rev() {
        f0[i] = f0[i].wrapping_add(f0[i - 1]);
        f1[i] = f1[i].wrapping_sub(f1[i - 1]);
    }

    a[0] = 4096;
    for i in (1..=5).rev() {
        let tmp = f0[6 - i].wrapping_add(f1[6 - i]).wrapping_add(4096);
        a[6 - i] = (tmp >> 13) as i16;

        let tmp = f0[6 - i].wrapping_sub(f1[6 - i]).wrapping_add(4096);
        a[5 + i] = (tmp >> 13) as i16;
    }
}

/// `state_construct`: decodes the scalar-quantised start state and runs it through the all-pass
/// (time-reversed) synthesis filter.
fn state_construct(ifm: usize, idx: &[usize], synt_denum: &[i16], out: &mut [i16]) {
    let len = out.len();
    let mut numerator = [0i16; LPC_FILTERORDER + 1];
    // `sample_val` doubles as the AR filter output, at offset LPC_FILTERORDER.
    let mut sample_val = [0i16; 2 * 58 + LPC_FILTERORDER];
    let mut sample_ma = [0i16; 2 * 58 + LPC_FILTERORDER];

    for k in 0..=LPC_FILTERORDER {
        numerator[k] = synt_denum[LPC_FILTERORDER - k];
    }

    // Decode the maximum value, then the sample values.
    let max_val = i32::from(FRG_QUANT_MOD[ifm]);
    let (shift, round) = if ifm < 37 {
        // Q8 maximum, Q13 table: result in Q(-1).
        (22, 2_097_152)
    }
    else if ifm < 59 {
        (19, 262_144)
    }
    else {
        (17, 65_536)
    };

    for k in 0..len {
        let s = idx[len - 1 - k];
        sample_val[LPC_FILTERORDER + k] =
            ((max_val * i32::from(ILBC_STATE[s]) + round) >> shift) as i16;
    }

    // Circular convolution with the all-pass filter: MA part, then AR part.
    filter_ma(
        &sample_val,
        LPC_FILTERORDER,
        &mut sample_ma[LPC_FILTERORDER..],
        &numerator,
        len + LPC_FILTERORDER,
    );
    // The reference zeroes `sampleMa[len + 10 .. 2 * len]`; `sample_ma` is zero beyond what the
    // filter wrote.

    // The AR filter runs on `sample_val` (the reference aliases `sampleAr` to it) with a zero
    // state.
    sample_val[..LPC_FILTERORDER].fill(0);
    sample_val[LPC_FILTERORDER..LPC_FILTERORDER + 2 * len]
        .copy_from_slice(&sample_ma[LPC_FILTERORDER..LPC_FILTERORDER + 2 * len]);
    filter_ar(&mut sample_val, LPC_FILTERORDER, synt_denum, 2 * len);

    let ar = &sample_val[LPC_FILTERORDER..];
    for k in 0..len {
        out[k] = (i32::from(ar[len - 1 - k]) + i32::from(ar[2 * len - 1 - k])) as i16;
    }
}

/// `gain_dequantization`.
fn gain_dequantization(index: usize, max_in: i32, stage: usize) -> i32 {
    let scale = 1638.max(max_in.abs());
    i32::from(((scale * i32::from(ILBC_GAIN[stage][index]) + 8192) >> 14) as i16)
}

/// `create_augmented_vector`: `buf[bend..]` is the reference's `buffer` pointer.
fn create_augmented_vector(index: usize, buf: &[i16], bend: usize, cb_vec: &mut [i16; SUBL]) {
    let interp = 4.min(index);
    let ilow = index - interp;

    cb_vec[..index].copy_from_slice(&buf[bend - index..bend]);

    // vector_multiplication(&cbVec[ilow], buffer - index - interp, alpha, interp, 15)
    for i in 0..interp {
        cb_vec[ilow + i] =
            ((i32::from(buf[bend - index - interp + i]) * i32::from(ALPHA[i])) >> 15) as i16;
    }
    // vector_rmultiplication(tmp, buffer - interp, &alpha[interp - 1], interp, 15), then add.
    for i in 0..interp {
        let t =
            ((i32::from(buf[bend - interp + i]) * i32::from(ALPHA[interp - 1 - i])) >> 15) as i16;
        cb_vec[ilow + i] = (i32::from(cb_vec[ilow + i]) + i32::from(t)) as i16;
    }

    let n = (SUBL - index).min(index);
    cb_vec[index..index + n].copy_from_slice(&buf[bend - index..bend - index + n]);
}

/// `get_codebook`: `mem[mo]` is the reference's `mem` pointer, with `mem[-4..]` addressable.
fn get_codebook(
    cbvec: &mut [i16; SUBL],
    mem: &mut [i16],
    mo: usize,
    index: usize,
    lmem: usize,
    cbveclen: usize,
) {
    let mut base_size = lmem - cbveclen + 1;
    if cbveclen == SUBL {
        base_size += cbveclen / 2;
    }

    cbvec.fill(0);

    if index < lmem - cbveclen + 1 {
        // First non-interpolated vectors.
        let k = index + cbveclen;
        cbvec[..cbveclen].copy_from_slice(&mem[mo + lmem - k..mo + lmem - k + cbveclen]);
    }
    else if index < base_size {
        let k = 2 * (index - (lmem - cbveclen + 1)) + cbveclen;
        let lag = k / 2;
        create_augmented_vector(lag, mem, mo + lmem, cbvec);
    }
    else if index - base_size < lmem - cbveclen + 1 {
        // Filtered vectors; stuff zeros outside the memory buffer.
        let mem_ind_test = lmem - (index - base_size + cbveclen);

        mem[mo - CB_HALFFILTERLEN..mo].fill(0);
        mem[mo + lmem..mo + lmem + CB_HALFFILTERLEN].fill(0);

        filter_ma(mem, mo + mem_ind_test + 4, cbvec, &CB_FILTERS_REV, cbveclen);
    }
    else {
        // Interpolated vectors.
        let mem_ind_test = lmem - cbveclen - CB_FILTERLEN;
        let mut tempbuff2 = [0i16; SUBL + 5];
        mem[mo + lmem..mo + lmem + CB_HALFFILTERLEN].fill(0);

        filter_ma(mem, mo + mem_ind_test + 7, &mut tempbuff2, &CB_FILTERS_REV, cbveclen + 5);

        let lag = (cbveclen << 1) + index - 20 - base_size - lmem - 1;
        create_augmented_vector(lag, &tempbuff2, SUBL + 5, cbvec);
    }
}

/// `construct_vector`: decodes one sub-block vector from three codebook stages.
fn construct_vector(
    decvector: &mut [i16],
    index: &[usize],
    gain_index: &[usize],
    mem: &mut [i16],
    mo: usize,
    lmem: usize,
) {
    let veclen = decvector.len();
    let g0 = gain_dequantization(gain_index[0], 16384, 0);
    let g1 = gain_dequantization(gain_index[1], g0, 1);
    let g2 = gain_dequantization(gain_index[2], g1, 2);

    let mut cb0 = [0i16; SUBL];
    let mut cb1 = [0i16; SUBL];
    let mut cb2 = [0i16; SUBL];
    get_codebook(&mut cb0, mem, mo, index[0], lmem, veclen);
    get_codebook(&mut cb1, mem, mo, index[1], lmem, veclen);
    get_codebook(&mut cb2, mem, mo, index[2], lmem, veclen);

    for j in 0..veclen {
        let a32 = (g0 * i32::from(cb0[j]))
            .wrapping_add(g1 * i32::from(cb1[j]))
            .wrapping_add(g2 * i32::from(cb2[j]));
        decvector[j] = (a32.wrapping_add(8192) >> 14) as i16;
    }
}

/// The decoder state that persists across frames (`ILBCContext`).
pub struct Core {
    mode: Mode,
    lsfdeqold: [i16; LPC_FILTERORDER],
    synt_mem: [i16; LPC_FILTERORDER],
    syntdenum: [i16; NSUB_MAX * (LPC_FILTERORDER + 1)],
    decresidual: [i16; BLOCKL_MAX],
    /// Time-reversed decoded vectors (the reference reuses `enh_buf`).
    reverse: [i16; BLOCKL_MAX],
    /// Codebook memory with 4 samples of filter headroom (the reference reuses `prevResidual`).
    mem: [i16; BLOCKL_MAX],
    /// Synthesis buffer: 10 samples of filter state followed by the frame.
    plc: [i16; BLOCKL_MAX + LPC_FILTERORDER],
    hpimemx: [i16; 2],
    hpimemy: [i16; 4],
}

impl Core {
    pub fn new(mode: Mode) -> Core {
        Core {
            mode,
            lsfdeqold: [0; LPC_FILTERORDER],
            synt_mem: [0; LPC_FILTERORDER],
            syntdenum: [0; NSUB_MAX * (LPC_FILTERORDER + 1)],
            decresidual: [0; BLOCKL_MAX],
            reverse: [0; BLOCKL_MAX],
            mem: [0; BLOCKL_MAX],
            plc: [0; BLOCKL_MAX + LPC_FILTERORDER],
            hpimemx: [0; 2],
            hpimemy: [0; 4],
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// `lsp_interpolate` (the weighting filter, which the decoder does not use, is omitted).
    fn lsp_interpolate(&mut self, lsfdeq: &[i16; 20]) {
        let (l1, l2) = lsfdeq.split_at(LPC_FILTERORDER);
        let mut tmp = [0i16; LPC_FILTERORDER];

        if self.mode == Mode::Ms30 {
            lsf_interpolate(&mut tmp, &self.lsfdeqold, l1, i32::from(LSF_WEIGHT_30MS[0]));
            lsf2poly(&mut self.syntdenum[..LPC_FILTERORDER + 1], &tmp);

            for i in 1..6 {
                lsf_interpolate(&mut tmp, l1, l2, i32::from(LSF_WEIGHT_30MS[i]));
                lsf2poly(&mut self.syntdenum[i * (LPC_FILTERORDER + 1)..], &tmp);
            }
            self.lsfdeqold.copy_from_slice(l2);
        }
        else {
            for i in 0..self.mode.nsub() {
                lsf_interpolate(&mut tmp, &self.lsfdeqold, l1, i32::from(LSF_WEIGHT_20MS[i]));
                lsf2poly(&mut self.syntdenum[i * (LPC_FILTERORDER + 1)..], &tmp);
            }
            self.lsfdeqold.copy_from_slice(l1);
        }
    }

    /// `decode_residual`: start state, then the forward and backward predicted sub-blocks.
    fn decode_residual(&mut self, f: &Frame) {
        let ssl = self.mode.state_short_len();
        let nsub = self.mode.nsub();
        let diff = STATE_LEN - ssl;
        // `mem` in the reference starts 4 samples into its buffer.
        let mo = CB_HALFFILTERLEN;

        let start_pos =
            if f.state_first { (f.start - 1) * SUBL } else { (f.start - 1) * SUBL + diff };

        // Scalar part of the start state.
        state_construct(
            f.ifm,
            &f.idx,
            &self.syntdenum[(f.start - 1) * (LPC_FILTERORDER + 1)..],
            &mut self.decresidual[start_pos..start_pos + ssl],
        );

        if f.state_first {
            // Adaptive part at the end.
            self.mem[mo..mo + CB_MEML - ssl].fill(0);
            self.mem[mo + CB_MEML - ssl..mo + CB_MEML]
                .copy_from_slice(&self.decresidual[start_pos..start_pos + ssl]);

            construct_vector(
                &mut self.decresidual[start_pos + ssl..start_pos + ssl + diff],
                &f.cb_index,
                &f.gain_index,
                &mut self.mem,
                mo + CB_MEML - ST_MEM_L_TBL,
                ST_MEM_L_TBL,
            );
        }
        else {
            // Adaptive part at the beginning, predicted backwards in time.
            for j in 0..ssl {
                self.mem[mo + CB_MEML - 1 - j] = self.decresidual[start_pos + j];
            }
            self.mem[mo..mo + CB_MEML - ssl].fill(0);

            construct_vector(
                &mut self.reverse[..diff],
                &f.cb_index,
                &f.gain_index,
                &mut self.mem,
                mo + CB_MEML - ST_MEM_L_TBL,
                ST_MEM_L_TBL,
            );

            for j in 0..diff {
                self.decresidual[start_pos - 1 - j] = self.reverse[j];
            }
        }

        let mut subcount = 1;

        // Forward prediction of sub-blocks.
        let nfor = nsub - f.start - 1;
        if nfor > 0 {
            self.mem[mo..mo + CB_MEML - STATE_LEN].fill(0);
            self.mem[mo + CB_MEML - STATE_LEN..mo + CB_MEML]
                .copy_from_slice(&self.decresidual[(f.start - 1) * SUBL..][..STATE_LEN]);

            for subframe in 0..nfor {
                let dst = (f.start + 1 + subframe) * SUBL;
                construct_vector(
                    &mut self.decresidual[dst..dst + SUBL],
                    &f.cb_index[subcount * 3..],
                    &f.gain_index[subcount * 3..],
                    &mut self.mem,
                    mo,
                    CB_MEML,
                );

                self.mem.copy_within(mo + SUBL..mo + CB_MEML, mo);
                self.mem[mo + CB_MEML - SUBL..mo + CB_MEML]
                    .copy_from_slice(&self.decresidual[dst..dst + SUBL]);

                subcount += 1;
            }
        }

        // Backward prediction of sub-blocks.
        let nback = f.start - 1;
        if nback > 0 {
            let meml_gotten = (SUBL * (nsub + 1 - f.start)).min(CB_MEML);

            for j in 0..meml_gotten {
                self.mem[mo + CB_MEML - 1 - j] = self.decresidual[(f.start - 1) * SUBL + j];
            }
            self.mem[mo..mo + CB_MEML - meml_gotten].fill(0);

            for subframe in 0..nback {
                construct_vector(
                    &mut self.reverse[subframe * SUBL..(subframe + 1) * SUBL],
                    &f.cb_index[subcount * 3..],
                    &f.gain_index[subcount * 3..],
                    &mut self.mem,
                    mo,
                    CB_MEML,
                );

                self.mem.copy_within(mo + SUBL..mo + CB_MEML, mo);
                self.mem[mo + CB_MEML - SUBL..mo + CB_MEML]
                    .copy_from_slice(&self.reverse[subframe * SUBL..(subframe + 1) * SUBL]);

                subcount += 1;
            }

            for j in 0..SUBL * nback {
                self.decresidual[SUBL * nback - 1 - j] = self.reverse[j];
            }
        }
    }

    /// `hp_output`: the 2nd-order high-pass post-filter, in place.
    fn hp_output(&mut self, signal: &mut [i16]) {
        let ba = &HP_OUT_COEFFS;
        let (x, y) = (&mut self.hpimemx, &mut self.hpimemy);

        for s in signal.iter_mut() {
            let mut tmp = i32::from(y[1]) * i32::from(ba[3]);
            tmp += i32::from(y[3]) * i32::from(ba[4]);
            tmp >>= 15;
            tmp += i32::from(y[0]) * i32::from(ba[3]);
            tmp += i32::from(y[2]) * i32::from(ba[4]);
            tmp = tmp.wrapping_mul(2);

            tmp = tmp.wrapping_add(i32::from(*s) * i32::from(ba[0]));
            tmp = tmp.wrapping_add(i32::from(x[0]) * i32::from(ba[1]));
            tmp = tmp.wrapping_add(i32::from(x[1]) * i32::from(ba[2]));

            x[1] = x[0];
            x[0] = *s;

            // Back to Q0, times two.
            *s = (tmp.wrapping_add(1024).clamp(-(1 << 26), (1 << 26) - 1) >> 11) as i16;

            y[2] = y[0];
            y[3] = y[1];

            // Up-shift by 3 with saturation.
            tmp = if tmp > 268_435_455 {
                i32::MAX
            }
            else if tmp < -268_435_456 {
                i32::MIN
            }
            else {
                tmp * 8
            };

            y[0] = (tmp >> 16) as i16;
            y[1] = ((tmp - (i32::from(y[0]) << 16)) >> 1) as i16;
        }
    }

    /// Decodes one frame of `mode.bytes()` bytes into `mode.samples()` samples.
    ///
    /// A frame flagged as empty, or with an out-of-range start sub-block, is concealed like the
    /// reference does: the previous residual and synthesis filters are replayed.
    pub fn decode_frame(&mut self, data: &[u8], out: &mut [i16]) {
        let mode = self.mode;
        let bs = mode.samples();
        let frame = Frame::unpack(mode, data);

        if !frame.flag && (1..=5).contains(&frame.start) {
            let mut f = frame;
            index_conv(&mut f.cb_index);

            let mut lsfdeq = [0i16; 20];
            lsf_dequantization(&mut lsfdeq, &f.lsf, mode.lpc_n());
            lsf_check_stability(&mut lsfdeq, mode.lpc_n());
            self.lsp_interpolate(&lsfdeq);
            self.decode_residual(&f);
        }

        // Synthesis (the reference's enhancer is not implemented, see the crate documentation).
        let (state, data) = self.plc.split_at_mut(LPC_FILTERORDER);
        state.copy_from_slice(&self.synt_mem);
        data[..bs].copy_from_slice(&self.decresidual[..bs]);

        for i in 0..mode.nsub() {
            filter_ar(
                &mut self.plc,
                LPC_FILTERORDER + i * SUBL,
                &self.syntdenum[i * (LPC_FILTERORDER + 1)..][..LPC_FILTERORDER + 1],
                SUBL,
            );
        }

        self.synt_mem.copy_from_slice(&self.plc[bs..bs + LPC_FILTERORDER]);

        out[..bs].copy_from_slice(&self.plc[LPC_FILTERORDER..LPC_FILTERORDER + bs]);
        self.hp_output(&mut out[..bs]);
    }
}
