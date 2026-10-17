// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Post-processing of decoded spectra: scaling, stereo processing, windowing/IMDCT and the QMF
//! banks (ac4dec.c lines 4453-4810 and `decode_channel`).

use symphonia_core::errors::{Result, decode_error};

use crate::ctx::*;
use crate::spectral::get_sfb_offset;
use crate::tables::*;

/// Returns two distinct mutable elements of a slice.
fn two_mut<T>(v: &mut [T], a: usize, b: usize) -> Option<(&mut T, &mut T)> {
    if a < b {
        let (l, r) = v.split_at_mut(b);
        Some((l.get_mut(a)?, r.first_mut()?))
    }
    else if b < a {
        let (l, r) = v.split_at_mut(a);
        Some((r.first_mut()?, l.get_mut(b)?))
    }
    else {
        None
    }
}

/// `scalarproduct_float`.
#[inline]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        acc += x * y;
    }
    acc
}

/// ac4dec.c: compute_window
fn compute_window(
    frame_len_base_idx: i32,
    kbd_window: &[[[f32; 2048]; 5]; 8],
    w: &mut [f32; 2048],
    n: i32,
    n_prev: i32,
    _nfull: i32,
    dir: i32,
) -> Result<()> {
    let transf_lengths = transf_length_48khz(frame_len_base_idx.clamp(0, 7) as usize);

    let n_w = if n <= n_prev { n } else { n_prev };

    if n < 0 || n > 2048 || n_prev < 0 || n_prev > 2048 {
        return decode_error("ac4: invalid window length");
    }

    let idx = match transf_lengths.iter().position(|&l| i32::from(l) == n_w) {
        Some(i) => i,
        None => return decode_error("ac4: invalid window length"),
    };

    let n_skip = (n - n_w) / 2;
    if n_w + 2 * n_skip != n {
        return decode_error("ac4: invalid window length");
    }

    let kernel = &kbd_window[frame_len_base_idx.clamp(0, 7) as usize][idx];

    for i in 0..n {
        let v = if i < n_skip {
            dir as f32
        }
        else if i < n_w + n_skip {
            if dir == 0 {
                kernel[(i - n_skip) as usize]
            }
            else {
                kernel[(n_w - i + n_skip - 1) as usize]
            }
        }
        else if dir == 0 {
            1.0
        }
        else {
            0.0
        };
        w[i as usize] = v;
    }

    Ok(())
}

impl Ac4Core {
    /// ac4dec.c: spectral_reordering
    pub(crate) fn spectral_reordering(&mut self, ch: usize) {
        if ch >= self.substream.ssch.len() {
            return;
        }

        let num_groups = usize::from(self.substream.ssch[ch].scp.num_window_groups).min(16);

        self.substream.ssch[ch].spec_reord.fill(0.0);

        let mut k = 0usize;
        let mut win = 0usize;

        for g in 0..num_groups {
            let transf_length_g = self.get_transf_length(ch, g as i32, None);
            let sfb_offset = get_sfb_offset(transf_length_g);
            let max_sfb = self.get_max_sfb(ch, g as i32).max(0) as usize;

            let ssch = &mut self.substream.ssch[ch];
            let num_win = usize::from(ssch.scp.num_win_in_group[g]);

            for sfb in 0..max_sfb {
                let (lo, hi) = match (sfb_offset.get(sfb), sfb_offset.get(sfb + 1)) {
                    (Some(&lo), Some(&hi)) => (usize::from(lo), usize::from(hi)),
                    _ => return,
                };

                for w in 0..num_win {
                    let off = match ssch.win_offset.get(win + w) {
                        Some(&off) if off >= 0 => off as usize,
                        _ => return,
                    };

                    for l in lo..hi {
                        let (Some(&src), Some(dst)) =
                            (ssch.scaled_spec.get(k), ssch.spec_reord.get_mut(off + l))
                        else {
                            return;
                        };
                        *dst = src;
                        k += 1;
                    }
                }
            }

            win += num_win;
        }
    }

    /// ac4dec.c: scale_spec
    pub(crate) fn scale_spec(&mut self, ch: usize) {
        let n = self.frame_len_base.clamp(0, 2048) as usize;
        let Some(ssch) = self.substream.ssch.get_mut(ch)
        else {
            return;
        };

        ssch.scaled_spec.fill(0.0);

        for k in 0..n {
            let x = ssch.quant_spec[k];
            let sfb = usize::try_from(ssch.offset2sfb[k]).unwrap_or(0).min(127);
            let g = usize::from(ssch.offset2g[k]).min(15);
            let lut = self.quant_lut.get(usize::from(x.unsigned_abs())).copied().unwrap_or(0.0);

            ssch.scaled_spec[k] = ssch.sf_gain[g][sfb] * lut.copysign(f32::from(x));
        }
    }

    /// ac4dec.c: two_channel_processing
    pub(crate) fn two_channel_processing(&mut self, ch0: usize, ch1: usize) {
        let n = self.frame_len_base.clamp(0, 2048) as usize;

        if ch0 >= self.substream.ssch.len() || ch1 >= self.substream.ssch.len() || ch0 == ch1 {
            return;
        }

        let num_groups = usize::from(self.substream.ssch[ch0].scp.num_window_groups).min(16);

        let mut max_sfb = [0i32; 16];
        for g in 0..num_groups {
            max_sfb[g] = self.get_max_sfb(ch0, g as i32);
        }
        let mut max_sfb_prev = self.get_max_sfb(ch0, 0);

        let ss = &mut *self.substream;

        ss.alpha_q = [[0.0; 128]; 16];

        {
            let ssch0 = &ss.ssch[ch0];

            for g in 0..num_groups {
                let max_sfb_g = max_sfb[g];

                for sfb in 0..max_sfb_g.clamp(0, 128) as usize {
                    let mut m = [[0.0f32; 2]; 2];

                    if ssch0.sap_mode == 0 || (ssch0.sap_mode == 1 && ssch0.ms_used[g][sfb] == 0) {
                        m[0][0] = 1.0;
                        m[1][1] = 1.0;
                        m[0][1] = 0.0;
                        m[1][0] = 0.0;
                    }
                    else if ssch0.sap_mode == 2
                        || (ssch0.sap_mode == 1 && ssch0.ms_used[g][sfb] == 1)
                    {
                        m[0][0] = 1.0;
                        m[0][1] = 1.0;
                        m[1][0] = 1.0;
                        m[1][1] = -1.0;
                    }
                    else {
                        // sap_mode == 3
                        if ssch0.sap_coeff_used[g][sfb] != 0 {
                            // setup alpha_q[g][sfb]
                            if sfb & 1 != 0 {
                                ss.alpha_q[g][sfb] = ss.alpha_q[g][sfb - 1];
                            }
                            else {
                                let delta = ssch0.dpcm_alpha_q[g][sfb].wrapping_sub(60) as f32;

                                let code_delta = if g == 0 || max_sfb_g != max_sfb_prev {
                                    0
                                }
                                else {
                                    ssch0.delta_code_time
                                };

                                if code_delta != 0 {
                                    ss.alpha_q[g][sfb] = ss.alpha_q[g - 1][sfb] + delta;
                                }
                                else if sfb == 0 {
                                    ss.alpha_q[g][sfb] = delta;
                                }
                                else {
                                    ss.alpha_q[g][sfb] = ss.alpha_q[g][sfb - 2] + delta;
                                }
                            }
                            // inverse quantize alpha_q[g][sfb]
                            let sap_gain = ss.alpha_q[g][sfb] * 0.1f32;
                            m[0][0] = 1.0 + sap_gain;
                            m[0][1] = 1.0;
                            m[1][0] = 1.0 - sap_gain;
                            m[1][1] = -1.0;
                        }
                        else {
                            m[0][0] = 1.0;
                            m[0][1] = 0.0;
                            m[1][0] = 0.0;
                            m[1][1] = 1.0;
                        }
                    }

                    ss.matrix_stereo[g][sfb] = m;
                }

                max_sfb_prev = max_sfb_g;
            }
        }

        let Some((ssch0, ssch1)) = two_mut(&mut ss.ssch, ch0, ch1)
        else {
            return;
        };

        for k in 0..n {
            let sfb = usize::try_from(ssch0.offset2sfb[k]).unwrap_or(0).min(127);
            let g = usize::from(ssch0.offset2g[k]).min(15);
            let mat = &ss.matrix_stereo[g][sfb];
            let a = mat[0][0];
            let b = mat[0][1];
            let c = mat[1][0];
            let d = mat[1][1];
            let i0 = ssch0.scaled_spec[k];
            let i1 = ssch1.scaled_spec[k];

            let o0 = i0 * a + i1 * b;
            let o1 = i0 * c + i1 * d;

            ssch0.scaled_spec[k] = o0;
            ssch1.scaled_spec[k] = o1;
        }
    }

    /// ac4dec.c: stereo_processing
    pub(crate) fn stereo_processing(&mut self) {
        if self.substream.mdct_stereo_proc[0] != 0 {
            self.two_channel_processing(0, 1);
        }
    }

    /// ac4dec.c: m5channel_processing
    pub(crate) fn m5channel_processing(&mut self) {
        let proc = self.substream.mdct_stereo_proc;
        let coding_config = self.substream.coding_config;

        match self.substream.codec_mode {
            CM_SIMPLE | CM_ASPX => {
                if coding_config == 0 {
                    if proc[0] != 0 {
                        self.two_channel_processing(0, 1);
                    }
                    if proc[1] != 0 {
                        self.two_channel_processing(2, 3);
                    }
                }
            }
            CM_ASPX_ACPL_1 | CM_ASPX_ACPL_2 if coding_config == 0 && proc[0] != 0 => {
                self.two_channel_processing(0, 1);
            }
            _ => (),
        }
    }

    /// ac4dec.c: qmf_analysis
    pub(crate) fn qmf_analysis(&mut self, ch: usize) {
        let num_ts = self.num_qmf_timeslots.clamp(0, 32) as usize;
        let Some(ssch) = self.substream.ssch.get_mut(ch)
        else {
            return;
        };

        let mut u = [0.0f32; 128];
        let mut z = [0.0f32; 640];

        for ts in 0..num_ts {
            /* shift time-domain input samples by 64 */
            ssch.qmf_filt.copy_within(0..640 - 64, 64);

            /* feed new audio samples */
            for sb in (0..64).rev() {
                ssch.qmf_filt[sb] = ssch.pcm[ts * 64 + 63 - sb] / 32768.0;
            }

            /* multiply input samples by window coefficients */
            for i in 0..640 {
                z[i] = ssch.qmf_filt[i] * QWIN[i];
            }

            /* sum the samples to create vector u */
            for n in 0..128 {
                u[n] = z[n];
                for k in 1..5 {
                    u[n] += z[n + k * 128];
                }
            }

            /* compute 64 new subband samples */
            for sb in 0..64 {
                ssch.q[0][ts][sb] = dot(&u, &self.cos_atab[sb]);
                ssch.q[1][ts][sb] = dot(&u, &self.sin_atab[sb]);
            }
        }
    }

    /// ac4dec.c: qmf_synthesis
    pub(crate) fn qmf_synthesis(&mut self, ch: usize, pcm: &mut [f32]) {
        let num_ts = (self.num_qmf_timeslots.clamp(0, 32) as usize).min(pcm.len() / 64);
        let Some(ssch) = self.substream.ssch.get_mut(ch)
        else {
            return;
        };

        let mut g = [0.0f32; 640];
        let mut w = [0.0f32; 640];

        for ts in 0..num_ts {
            /* shift samples by 128 */
            ssch.qsyn_filt.copy_within(0..1280 - 128, 128);

            for n in 0..128 {
                ssch.qsyn_filt[n] =
                    dot(&ssch.q[0][ts], &self.cos_stab[n]) - dot(&ssch.q[1][ts], &self.sin_stab[n]);
            }

            for n in 0..5 {
                for sb in 0..64 {
                    g[128 * n + sb] = ssch.qsyn_filt[256 * n + sb];
                    g[128 * n + 64 + sb] = ssch.qsyn_filt[256 * n + 192 + sb];
                }
            }

            /* multiply by window coefficients */
            for i in 0..640 {
                w[i] = g[i] * QWIN[i];
            }

            /* compute 64 new time-domain output samples */
            for sb in 0..64 {
                let mut temp = 0.0f32;

                for n in 0..10 {
                    temp += w[64 * n + sb];
                }
                pcm[ts * 64 + sb] = temp;
            }
        }
    }

    /// ac4dec.c: spectral_synthesis
    pub(crate) fn spectral_synthesis(&mut self, ch: usize) {
        // The window layout has been validated (`channel_windows_valid`) before this is called;
        // a layout that still fails here leaves the channel untouched.
        let _ = self.spectral_synthesis_impl(ch);
    }

    fn spectral_synthesis_impl(&mut self, ch: usize) -> Result<()> {
        let nfull = self.frame_len_base;
        let midx = self.frame_len_base_idx;

        if ch >= self.substream.ssch.len() || nfull < 0 || nfull > 2048 {
            return decode_error("ac4: invalid channel");
        }

        let mut inp = [0.0f32; 2048];
        let mut x = [0.0f32; 4096];

        let num_groups = usize::from(self.substream.ssch[ch].scp.num_window_groups).min(16);
        let mut win = 0usize;

        for g in 0..num_groups {
            let n = self.get_transf_length(ch, g as i32, None);
            let ssch = &mut self.substream.ssch[ch];

            if ssch.n_prev == 0 {
                ssch.n_prev = nfull;
            }

            if n <= 0 || n > nfull || ssch.n_prev < 0 || ssch.n_prev > nfull {
                return decode_error("ac4: invalid transform length");
            }

            compute_window(midx, &self.kbd_window, &mut self.winl, n, ssch.n_prev, nfull, 0)?;
            compute_window(midx, &self.kbd_window, &mut self.winr, ssch.n_prev, n, nfull, 1)?;

            let nu = n as usize;
            let half = nu >> 1;
            let n_prev = ssch.n_prev as usize;
            let nskip = ((nfull - n) / 2) as usize;
            let nskip_prev = ((nfull - ssch.n_prev) / 2) as usize;
            let num_win = usize::from(ssch.scp.num_win_in_group[g]);

            for w in 0..num_win {
                let off = match ssch.win_offset.get(win + w) {
                    Some(&off) if off >= 0 && off as usize + nu <= ssch.pcm.len() => off as usize,
                    _ => return decode_error("ac4: invalid window offset"),
                };

                inp[..nu].copy_from_slice(&ssch.spec_reord[off..off + nu]);

                self.imdct.imdct(nu, &inp[..nu], &mut x[half..half + nu])?;

                for i in 0..half {
                    x[i] = -x[nu - i - 1];
                    x[nu * 2 - i - 1] = x[nu + i];
                }

                for i in 0..nu / 4 {
                    x[2 * i] *= self.winl[2 * i];
                    x[2 * i + 1] *= self.winl[2 * i + 1];
                    x[nu / 2 + 2 * i] *= self.winl[nu / 2 + 2 * i];
                    x[nu / 2 + 2 * i + 1] *= self.winl[nu / 2 + 2 * i + 1];
                }

                /* window second half of previous block */
                for i in 0..n_prev {
                    ssch.overlap[nskip_prev + i] *= self.winr[i];
                }

                /* overlap/add using first N samples from x[n] */
                for i in 0..nu {
                    ssch.overlap[nskip + i] += x[i];
                }

                /* output pcm */
                for i in 0..nu {
                    ssch.pcm[off + i] = ssch.overlap[i];
                }
                /* move samples in overlap[] not stored in pcm[] */
                for i in 0..nskip {
                    ssch.overlap[i] = ssch.overlap[nu + i];
                }

                /* store second N samples from x[n] for next overlap/add */
                ssch.overlap[nskip..nskip + nu].copy_from_slice(&x[nu..nu + nu]);
            }

            ssch.n_prev = n;

            win += num_win;
        }

        Ok(())
    }

    /// ac4dec.c: decode_channel
    pub(crate) fn decode_channel(&mut self, ch: usize, out: &mut [f32]) {
        self.qmf_synthesis(ch, out);
    }
}
