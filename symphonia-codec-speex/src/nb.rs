// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The narrowband CELP decoder.
//!
//! Ported from libspeex `nb_celp.c`, `ltp.c`, `cb_search.c`, `quant_lsp.c` and `modes.c`
//! (BSD-3-Clause), see NOTICE. This is the floating-point decoder of the reference and it keeps
//! its buffer layout and operation order, including the quirks that matter for exactness.

use crate::bits::Bits;
use crate::dsp::{
    bw_lpc, compute_rms, highpass, iir_mem, lsp_interpolate, lsp_to_lpc, multicomb, sanitize,
    speex_rand,
};
use crate::stereo::StereoState;
use crate::tables;

/// Why a frame could not be decoded.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// The packet has no more frames (end of data, or a terminator): `-1` in libspeex.
    End,
    /// The bit-stream is invalid: `-2` in libspeex.
    Corrupt,
}

pub const NB_FRAME_SIZE: usize = 160;
const NB_SUBFRAME_SIZE: usize = 40;
const NB_NB_SUBFRAMES: usize = 4;
const NB_ORDER: usize = 10;
const VERY_SMALL: f32 = 1e-15;
const NB_PITCH_START: i32 = 17;
const NB_PITCH_END: i32 = 144;
const NB_DEC_BUFFER: usize = NB_FRAME_SIZE + 2 * NB_PITCH_END as usize + NB_SUBFRAME_SIZE + 12;
/// Offset of the current frame's excitation within the excitation buffer.
const EXC: usize = 2 * NB_PITCH_END as usize + NB_SUBFRAME_SIZE + 6;

/// Bits of each wide-band sub-mode, skipped when a wide-band layer is found by a narrow-band
/// decoder (`wb_skip_table`).
const WB_SKIP_TABLE: [i32; 8] = [0, 36, 112, 192, 352, 0, 0, 0];

/// Attenuation of the pitch contribution and of the noise by the number of consecutive lost
/// frames (`attenuation`, floating-point table).
const LOST_ATTENUATION: [f32; 10] =
    [1.0, 0.961, 0.852, 0.698, 0.527, 0.368, 0.237, 0.141, 0.077, 0.039];

/// Median of three values (`median3`).
fn median3(a: f32, b: f32, c: f32) -> f32 {
    if a < b {
        if b < c {
            b
        }
        else if a < c {
            c
        }
        else {
            a
        }
    }
    else if c < b {
        b
    }
    else if c < a {
        c
    }
    else {
        a
    }
}

/// Parameters of a split (vector-quantised) innovation codebook.
pub struct SplitCb {
    pub subvect_size: usize,
    pub nb_subvect: usize,
    pub shape_cb: &'static [i8],
    pub shape_bits: u32,
    pub have_sign: bool,
}

impl SplitCb {
    /// Adds the decoded innovation to `exc` (`split_cb_shape_sign_unquant`).
    pub fn unquant(&self, exc: &mut [f32], bits: &mut Bits<'_>) {
        let mut ind = [0usize; 8];
        let mut signs = [false; 8];

        for i in 0..self.nb_subvect {
            signs[i] = self.have_sign && bits.unpack(1) != 0;
            ind[i] = bits.unpack(self.shape_bits) as usize;
        }

        for i in 0..self.nb_subvect {
            let s: f32 = if signs[i] { -1.0 } else { 1.0 };
            let cb = &self.shape_cb[ind[i] * self.subvect_size..][..self.subvect_size];
            let out = &mut exc[self.subvect_size * i..][..self.subvect_size];
            for (o, &c) in out.iter_mut().zip(cb) {
                *o = (f64::from(*o) + f64::from(s) * 0.03125 * f64::from(c)) as f32;
            }
        }
    }
}

enum Lsp {
    Nb,
    Lbr,
}

enum Ltp {
    Forced,
    ThreeTap { gain_cdbk: &'static [i8], gain_bits: u32, pitch_bits: u32 },
}

enum Innovation {
    Noise,
    Split(SplitCb),
}

/// A narrow-band sub-mode (`SpeexSubmode`).
struct Submode {
    /// The pitch is coded once per frame (open loop) rather than per sub-frame.
    lbr_pitch: bool,
    forced_pitch_gain: bool,
    have_subframe_gain: u8,
    double_codebook: bool,
    lsp: Lsp,
    ltp: Ltp,
    innovation: Innovation,
    comb_gain: f32,
}

static SUBMODES: [Option<Submode>; 9] = [
    None,
    // 2.15 kbps "vocoder-like" mode for comfort noise.
    Some(Submode {
        lbr_pitch: true,
        forced_pitch_gain: true,
        have_subframe_gain: 0,
        double_codebook: false,
        lsp: Lsp::Lbr,
        ltp: Ltp::Forced,
        innovation: Innovation::Noise,
        comb_gain: -1.0,
    }),
    // 5.95 kbps very low bit-rate mode.
    Some(Submode {
        lbr_pitch: true,
        forced_pitch_gain: false,
        have_subframe_gain: 0,
        double_codebook: false,
        lsp: Lsp::Lbr,
        ltp: Ltp::ThreeTap { gain_cdbk: &tables::GAIN_CDBK_LBR, gain_bits: 5, pitch_bits: 0 },
        innovation: Innovation::Split(SplitCb {
            subvect_size: 10,
            nb_subvect: 4,
            shape_cb: &tables::EXC_10_16_TABLE,
            shape_bits: 4,
            have_sign: false,
        }),
        comb_gain: 0.6,
    }),
    // 8 kbps low bit-rate mode.
    Some(Submode {
        lbr_pitch: false,
        forced_pitch_gain: false,
        have_subframe_gain: 1,
        double_codebook: false,
        lsp: Lsp::Lbr,
        ltp: Ltp::ThreeTap { gain_cdbk: &tables::GAIN_CDBK_LBR, gain_bits: 5, pitch_bits: 7 },
        innovation: Innovation::Split(SplitCb {
            subvect_size: 10,
            nb_subvect: 4,
            shape_cb: &tables::EXC_10_32_TABLE,
            shape_bits: 5,
            have_sign: false,
        }),
        comb_gain: 0.55,
    }),
    // 11 kbps medium bit-rate mode.
    Some(Submode {
        lbr_pitch: false,
        forced_pitch_gain: false,
        have_subframe_gain: 1,
        double_codebook: false,
        lsp: Lsp::Lbr,
        ltp: Ltp::ThreeTap { gain_cdbk: &tables::GAIN_CDBK_LBR, gain_bits: 5, pitch_bits: 7 },
        innovation: Innovation::Split(SplitCb {
            subvect_size: 8,
            nb_subvect: 5,
            shape_cb: &tables::EXC_8_128_TABLE,
            shape_bits: 7,
            have_sign: false,
        }),
        comb_gain: 0.45,
    }),
    // 15 kbps high bit-rate mode.
    Some(Submode {
        lbr_pitch: false,
        forced_pitch_gain: false,
        have_subframe_gain: 3,
        double_codebook: false,
        lsp: Lsp::Nb,
        ltp: Ltp::ThreeTap { gain_cdbk: &tables::GAIN_CDBK_NB, gain_bits: 7, pitch_bits: 7 },
        innovation: Innovation::Split(SplitCb {
            subvect_size: 5,
            nb_subvect: 8,
            shape_cb: &tables::EXC_5_64_TABLE,
            shape_bits: 6,
            have_sign: false,
        }),
        comb_gain: 0.25,
    }),
    // 18.2 kbps high bit-rate mode.
    Some(Submode {
        lbr_pitch: false,
        forced_pitch_gain: false,
        have_subframe_gain: 3,
        double_codebook: false,
        lsp: Lsp::Nb,
        ltp: Ltp::ThreeTap { gain_cdbk: &tables::GAIN_CDBK_NB, gain_bits: 7, pitch_bits: 7 },
        innovation: Innovation::Split(SplitCb {
            subvect_size: 5,
            nb_subvect: 8,
            shape_cb: &tables::EXC_5_256_TABLE,
            shape_bits: 8,
            have_sign: false,
        }),
        comb_gain: 0.15,
    }),
    // 24.6 kbps high bit-rate mode.
    Some(Submode {
        lbr_pitch: false,
        forced_pitch_gain: false,
        have_subframe_gain: 3,
        double_codebook: true,
        lsp: Lsp::Nb,
        ltp: Ltp::ThreeTap { gain_cdbk: &tables::GAIN_CDBK_NB, gain_bits: 7, pitch_bits: 7 },
        innovation: Innovation::Split(SplitCb {
            subvect_size: 5,
            nb_subvect: 8,
            shape_cb: &tables::EXC_5_64_TABLE,
            shape_bits: 6,
            have_sign: false,
        }),
        comb_gain: 0.05,
    }),
    // 3.95 kbps very low bit-rate mode.
    Some(Submode {
        lbr_pitch: true,
        forced_pitch_gain: true,
        have_subframe_gain: 0,
        double_codebook: false,
        lsp: Lsp::Lbr,
        ltp: Ltp::Forced,
        innovation: Innovation::Split(SplitCb {
            subvect_size: 20,
            nb_subvect: 2,
            shape_cb: &tables::EXC_20_32_TABLE,
            shape_bits: 5,
            have_sign: false,
        }),
        comb_gain: 0.5,
    }),
];

impl Lsp {
    /// Reads the quantised LSPs (`lsp_unquant_nb` and `lsp_unquant_lbr`).
    fn unquant(&self, lsp: &mut [f32; NB_ORDER], bits: &mut Bits<'_>) {
        // `lsp[i] += c * k`, evaluated in double precision as in the reference.
        fn add(lsp: &mut f32, k: f64, c: i8) {
            *lsp = (f64::from(*lsp) + k * f64::from(c)) as f32;
        }

        for (i, l) in lsp.iter_mut().enumerate() {
            *l = (0.25 * i as f64 + 0.25) as f32;
        }

        let id = bits.unpack(6) as usize;
        for i in 0..10 {
            add(&mut lsp[i], 0.0039062, tables::CDBK_NB[id * 10 + i]);
        }

        let id = bits.unpack(6) as usize;
        for i in 0..5 {
            add(&mut lsp[i], 0.0019531, tables::CDBK_NB_LOW1[id * 5 + i]);
        }

        if let Lsp::Nb = self {
            let id = bits.unpack(6) as usize;
            for i in 0..5 {
                add(&mut lsp[i], 0.00097656, tables::CDBK_NB_LOW2[id * 5 + i]);
            }

            let id = bits.unpack(6) as usize;
            for i in 0..5 {
                add(&mut lsp[i + 5], 0.0019531, tables::CDBK_NB_HIGH1[id * 5 + i]);
            }

            let id = bits.unpack(6) as usize;
            for i in 0..5 {
                add(&mut lsp[i + 5], 0.00097656, tables::CDBK_NB_HIGH2[id * 5 + i]);
            }
        }
        else {
            let id = bits.unpack(6) as usize;
            for i in 0..5 {
                add(&mut lsp[i + 5], 0.0019531, tables::CDBK_NB_HIGH1[id * 5 + i]);
            }
        }
    }
}

/// `gain_3tap_to_1tap`: the sum of the magnitudes of the three pitch taps, the outer taps
/// weighted by one half when negative.
fn gain_3tap_to_1tap(g: &[f32; 3]) -> f32 {
    let outer = |v: f32| if v > 0.0 { f64::from(v) } else { -0.5 * f64::from(v) };
    (f64::from(g[1].abs()) + outer(g[0]) + outer(g[2])) as f32
}

/// Decoder state of a narrow-band (or the narrow-band layer of a wide-band) stream
/// (`DecState`).
pub struct NbDecoder {
    first: bool,
    is_wideband: bool,
    exc_buf: [f32; NB_DEC_BUFFER],
    old_qlsp: [f32; NB_ORDER],
    interp_qlpc: [f32; NB_ORDER],
    mem_sp: [f32; NB_ORDER],
    mem_hp: [f32; 2],
    pi_gain: [f32; NB_NB_SUBFRAMES],
    seed: u32,
    voc_m1: f32,
    voc_m2: f32,
    voc_mean: f32,
    voc_offset: i32,
    dtx_enabled: bool,
    /// Number of consecutive frames that were lost before the current one.
    count_lost: usize,
    /// Pitch and pitch gain of the last correctly decoded frame.
    last_pitch: i32,
    last_pitch_gain: f32,
    /// Pitch gains of the last three frames and the slot to overwrite next.
    pitch_gain_buf: [f32; 3],
    pitch_gain_buf_idx: usize,
}

impl NbDecoder {
    pub fn new(is_wideband: bool) -> Self {
        NbDecoder {
            first: true,
            is_wideband,
            exc_buf: [0.0; NB_DEC_BUFFER],
            old_qlsp: [0.0; NB_ORDER],
            interp_qlpc: [0.0; NB_ORDER],
            mem_sp: [0.0; NB_ORDER],
            mem_hp: [0.0; 2],
            pi_gain: [0.0; NB_NB_SUBFRAMES],
            seed: 1000,
            voc_m1: 0.0,
            voc_m2: 0.0,
            voc_mean: 0.0,
            voc_offset: 0,
            dtx_enabled: false,
            count_lost: 0,
            last_pitch: 40,
            last_pitch_gain: 0.0,
            pitch_gain_buf: [0.0; 3],
            pitch_gain_buf_idx: 0,
        }
    }

    /// Whether the last frame announced discontinuous transmission.
    pub fn dtx_enabled(&self) -> bool {
        self.dtx_enabled
    }

    /// The gain of the synthesis filter at `w = pi` of each sub-frame of the last frame.
    pub fn pi_gain(&self) -> [f32; NB_NB_SUBFRAMES] {
        self.pi_gain
    }

    /// The RMS of the excitation of each sub-frame of the last frame.
    pub fn exc_rms(&self, rms: &mut [f32]) {
        for (i, r) in rms.iter_mut().enumerate().take(NB_NB_SUBFRAMES) {
            *r = compute_rms(&self.exc_buf[EXC + i * NB_SUBFRAME_SIZE..][..NB_SUBFRAME_SIZE]);
        }
    }

    /// Synthesises a frame of comfort noise at the level of the previous excitation: the null
    /// mode, and a lost frame while discontinuous transmission is on.
    fn comfort_noise(&mut self, out: &mut [f32]) {
        let mut lpc = [0f32; NB_ORDER];
        bw_lpc(0.93, &self.interp_qlpc, &mut lpc);

        let exc = &mut self.exc_buf[EXC..EXC + NB_FRAME_SIZE];
        let innov_gain = compute_rms(exc);
        for e in exc.iter_mut() {
            *e = speex_rand(innov_gain, &mut self.seed);
        }

        self.first = true;

        out.copy_from_slice(exc);
        iir_mem(out, &lpc, &mut self.mem_sp);
        self.count_lost = 0;
    }

    /// Conceals a lost frame (`nb_decode` without a bit-stream): comfort noise if the stream
    /// is in discontinuous transmission, otherwise the pitch contribution of the previous
    /// frames, fading out with the number of lost frames, plus noise (`nb_decode_lost`).
    pub fn decode_lost(&mut self, out: &mut [f32]) {
        let out = &mut out[..NB_FRAME_SIZE];

        if self.dtx_enabled {
            self.exc_buf.copy_within(NB_FRAME_SIZE.., 0);
            self.comfort_noise(out);
            return;
        }

        let fact = LOST_ATTENUATION.get(self.count_lost).copied().unwrap_or(0.0);

        let gain_med =
            median3(self.pitch_gain_buf[0], self.pitch_gain_buf[1], self.pitch_gain_buf[2]);
        if gain_med < self.last_pitch_gain {
            self.last_pitch_gain = gain_med;
        }

        let mut pitch_gain = self.last_pitch_gain;
        if f64::from(pitch_gain) > 0.85 {
            pitch_gain = 0.85f64 as f32;
        }
        pitch_gain = fact * pitch_gain + VERY_SMALL;

        let innov_gain = compute_rms(&self.exc_buf[EXC..EXC + NB_FRAME_SIZE]);
        let noise_gain = innov_gain * (fact * (1.0 - pitch_gain * pitch_gain));

        // Shift the excitation by one frame.
        self.exc_buf.copy_within(NB_FRAME_SIZE.., 0);

        // The pitch is that of the last good frame, with a small random walk (the C cast
        // truncates towards zero).
        let walk = speex_rand((1 + self.count_lost) as f32, &mut self.seed) as i32;
        let pitch = (self.last_pitch + walk).clamp(NB_PITCH_START, NB_PITCH_END) as usize;

        for i in 0..NB_FRAME_SIZE {
            let past = self.exc_buf[EXC + i - pitch];
            self.exc_buf[EXC + i] =
                pitch_gain * (past + VERY_SMALL) + speex_rand(noise_gain, &mut self.seed);
        }

        let qlpc = self.interp_qlpc;
        bw_lpc(0.98, &qlpc, &mut self.interp_qlpc);
        out.copy_from_slice(&self.exc_buf[EXC - NB_SUBFRAME_SIZE..][..NB_FRAME_SIZE]);
        iir_mem(out, &self.interp_qlpc, &mut self.mem_sp);
        // The reference always uses the narrow-band filter here.
        highpass(out, 1, &mut self.mem_hp);

        self.first = false;
        self.count_lost += 1;
        self.pitch_gain_buf[self.pitch_gain_buf_idx] = pitch_gain;
        self.pitch_gain_buf_idx += 1;
        if self.pitch_gain_buf_idx > 2 {
            self.pitch_gain_buf_idx = 0;
        }
    }

    /// Decodes one frame of `NB_FRAME_SIZE` samples to `out`. `innov_save` receives the
    /// innovation, which the wide-band layer folds into the high band. In-band stereo requests
    /// are applied to `stereo`, if the stream has two channels.
    pub fn decode(
        &mut self,
        bits: &mut Bits<'_>,
        out: &mut [f32],
        mut innov_save: Option<&mut [f32]>,
        mut stereo: Option<&mut StereoState>,
    ) -> Result<(), Status> {
        let out = &mut out[..NB_FRAME_SIZE];

        // Search for the next narrow-band block, handling in-band requests and skipping
        // wide-band blocks.
        let submode_id = loop {
            if bits.remaining() < 5 {
                return Err(Status::End);
            }
            let mut wideband = bits.unpack(1);
            if wideband != 0 {
                let submode = bits.unpack(3) as usize;
                bits.advance(WB_SKIP_TABLE[submode] - 4);

                if bits.remaining() < 5 {
                    return Err(Status::End);
                }
                wideband = bits.unpack(1);
                if wideband != 0 {
                    let submode = bits.unpack(3) as usize;
                    bits.advance(WB_SKIP_TABLE[submode] - 4);
                    wideband = bits.unpack(1);
                    if wideband != 0 {
                        return Err(Status::Corrupt);
                    }
                }
            }
            if bits.remaining() < 4 {
                return Err(Status::End);
            }
            let m = bits.unpack(4);
            match m {
                15 => return Err(Status::End),
                14 => inband_request(bits, stereo.as_deref_mut()),
                13 => {
                    // A user request, which this decoder ignores: skip it.
                    let req_size = bits.unpack(4);
                    bits.advance(5 + 8 * req_size as i32);
                }
                9..=12 => return Err(Status::Corrupt),
                _ => break m as usize,
            }
        };

        // Shift the excitation by one frame.
        self.exc_buf.copy_within(NB_FRAME_SIZE.., 0);

        // A null mode carries no data: synthesise comfort noise.
        let Some(submode) = &SUBMODES[submode_id]
        else {
            self.comfort_noise(out);
            return Ok(());
        };

        let mut qlsp = [0f32; NB_ORDER];
        submode.lsp.unquant(&mut qlsp, bits);

        // Damp the filter memory if a frame was lost and the LSPs changed too much.
        if self.count_lost > 0 {
            let mut lsp_dist = 0f32;
            for (o, q) in self.old_qlsp.iter().zip(&qlsp) {
                lsp_dist += (o - q).abs();
            }
            let fact = (0.6 * (-0.2 * f64::from(lsp_dist)).exp()) as f32;
            for m in self.mem_sp.iter_mut() {
                *m *= fact;
            }
        }

        // Handle the first frame and the lost-packet case.
        if self.first || self.count_lost > 0 {
            self.old_qlsp = qlsp;
        }

        // Open-loop pitch for the low bit-rate pitch coding.
        let mut ol_pitch = 0;
        if submode.lbr_pitch {
            ol_pitch = NB_PITCH_START + bits.unpack(7) as i32;
        }

        let mut ol_pitch_coef = 0f32;
        if submode.forced_pitch_gain {
            let quant = bits.unpack(4);
            ol_pitch_coef = (0.066667 * f64::from(quant)) as f32;
        }

        // Global excitation gain.
        let qe = bits.unpack(5);
        let ol_gain = (f64::from(qe) / 3.5).exp() as f32;

        if submode_id == 1 {
            self.dtx_enabled = bits.unpack(4) == 15;
        }
        if submode_id > 1 {
            self.dtx_enabled = false;
        }

        let mut best_pitch = 40;
        let mut best_pitch_gain = 0f32;
        let mut pitch_average = 0f32;

        for sub in 0..NB_NB_SUBFRAMES {
            let offset = NB_SUBFRAME_SIZE * sub;
            let exc_pos = EXC + offset;

            // The adaptive codebook contribution, derived from the past excitation.
            self.exc_buf[exc_pos..exc_pos + NB_SUBFRAME_SIZE].fill(0.0);

            let mut exc32 = [0f32; NB_SUBFRAME_SIZE];
            let mut pitch_gain = [0f32; 3];
            let pitch;

            match &submode.ltp {
                Ltp::Forced => {
                    // `forced_pitch_unquant`; the pitch is the open-loop one, with a fixed gain.
                    let coef =
                        if f64::from(ol_pitch_coef) > 0.99 { 0.99f32 } else { ol_pitch_coef };
                    let start = ol_pitch as usize;
                    for i in 0..NB_SUBFRAME_SIZE {
                        exc32[i] = self.exc_buf[exc_pos + i - start] * coef;
                        self.exc_buf[exc_pos + i] = exc32[i];
                    }
                    pitch = ol_pitch;
                    pitch_gain = [0.0, coef, 0.0];
                }
                Ltp::ThreeTap { gain_cdbk, gain_bits, pitch_bits } => {
                    // `pitch_unquant_3tap`.
                    let start = if submode.lbr_pitch { ol_pitch } else { NB_PITCH_START };
                    pitch = bits.unpack(*pitch_bits) as i32 + start;
                    let gain_index = bits.unpack(*gain_bits) as usize;
                    for (i, g) in pitch_gain.iter_mut().enumerate() {
                        *g = (0.015625 * f64::from(gain_cdbk[gain_index * 4 + i]) + 0.5) as f32;
                    }

                    // After a lost frame, do not let the pitch gain exceed that of the last
                    // good frame (halved after more than three lost frames).
                    if self.count_lost > 0 && pitch as usize > offset {
                        let limit = if self.count_lost < 4 {
                            self.last_pitch_gain
                        }
                        else {
                            0.5 * self.last_pitch_gain
                        };
                        let limit = if f64::from(limit) > 0.95 { 0.95f64 as f32 } else { limit };
                        let gain_sum = gain_3tap_to_1tap(&pitch_gain);
                        if gain_sum > limit {
                            let fact = limit / gain_sum;
                            for g in pitch_gain.iter_mut() {
                                *g *= fact;
                            }
                        }
                    }

                    for i in 0..3 {
                        let pp = (pitch + 1 - i as i32) as usize;
                        let tmp1 = NB_SUBFRAME_SIZE.min(pp);
                        let g = pitch_gain[2 - i];
                        for j in 0..tmp1 {
                            exc32[j] += g * self.exc_buf[exc_pos + j - pp];
                        }
                        let tmp3 = NB_SUBFRAME_SIZE.min(pp + pitch as usize);
                        for j in tmp1..tmp3 {
                            exc32[j] += g * self.exc_buf[exc_pos + j - pp - pitch as usize];
                        }
                    }
                }
            }

            // Guard against crafted streams making the excitation blow up.
            sanitize(&mut exc32, -32000.0, 32000.0);

            let tmp = gain_3tap_to_1tap(&pitch_gain);
            pitch_average += tmp;

            let bp = best_pitch;
            let bg = f64::from(best_pitch_gain);
            let t = f64::from(tmp);
            if (tmp > best_pitch_gain
                && (2 * bp - pitch).abs() >= 3
                && (3 * bp - pitch).abs() >= 4
                && (4 * bp - pitch).abs() >= 5)
                || (t > 0.6 * bg
                    && ((bp - 2 * pitch).abs() < 3
                        || (bp - 3 * pitch).abs() < 4
                        || (bp - 4 * pitch).abs() < 5))
                || (0.67 * t > bg
                    && ((2 * bp - pitch).abs() < 3
                        || (3 * bp - pitch).abs() < 4
                        || (4 * bp - pitch).abs() < 5))
            {
                best_pitch = pitch;
                if tmp > best_pitch_gain {
                    best_pitch_gain = tmp;
                }
            }

            // The innovation (fixed codebook contribution).
            let mut innov = [0f32; NB_SUBFRAME_SIZE];

            let ener = match submode.have_subframe_gain {
                3 => tables::EXC_GAIN_QUANT_SCAL3[bits.unpack(3) as usize] * ol_gain,
                1 => tables::EXC_GAIN_QUANT_SCAL1[bits.unpack(1) as usize] * ol_gain,
                _ => ol_gain,
            };

            match &submode.innovation {
                Innovation::Noise => {
                    for v in innov.iter_mut() {
                        *v = speex_rand(1.0, &mut self.seed);
                    }
                }
                Innovation::Split(cb) => cb.unquant(&mut innov, bits),
            }
            for v in innov.iter_mut() {
                *v *= ener;
            }

            if submode.double_codebook {
                let mut innov2 = [0f32; NB_SUBFRAME_SIZE];
                if let Innovation::Split(cb) = &submode.innovation {
                    cb.unquant(&mut innov2, bits);
                }
                let ener2 = 0.454545f32 * ener;
                for (v, &v2) in innov.iter_mut().zip(&innov2) {
                    *v += v2 * ener2;
                }
            }

            for i in 0..NB_SUBFRAME_SIZE {
                self.exc_buf[exc_pos + i] = exc32[i] + innov[i];
            }

            if let Some(save) = innov_save.as_deref_mut() {
                save[offset..offset + NB_SUBFRAME_SIZE].copy_from_slice(&innov);
            }

            // Vocoder mode.
            if submode_id == 1 {
                let mut g = 1.5f32 * (ol_pitch_coef - 0.2f32);
                g = g.clamp(0.0, 1.0);

                let exc = &mut self.exc_buf[exc_pos..exc_pos + NB_SUBFRAME_SIZE];
                exc.fill(0.0);
                while self.voc_offset < NB_SUBFRAME_SIZE as i32 {
                    if self.voc_offset >= 0 {
                        exc[self.voc_offset as usize] =
                            (f64::from(2 * ol_pitch).sqrt() as f32) * (g * ol_gain);
                    }
                    self.voc_offset += ol_pitch;
                }
                self.voc_offset -= NB_SUBFRAME_SIZE as i32;

                for i in 0..NB_SUBFRAME_SIZE {
                    let exci = exc[i];
                    exc[i] = (0.7f32 * exc[i] + 0.3f32 * self.voc_m1)
                        + ((1.0 - 0.85f32 * g) * innov[i] - (0.15f32 * g) * self.voc_m2);
                    self.voc_m1 = exci;
                    self.voc_m2 = innov[i];
                    self.voc_mean = 0.8f32 * self.voc_mean + 0.2f32 * exc[i];
                    exc[i] -= self.voc_mean;
                }
            }
        }

        // The perceptual enhancer (comb filter) on the excitation.
        if submode.comb_gain > 0.0 && self.count_lost == 0 {
            let (first, second) = out.split_at_mut(2 * NB_SUBFRAME_SIZE);
            multicomb(
                &self.exc_buf,
                EXC - NB_SUBFRAME_SIZE,
                first,
                2 * NB_SUBFRAME_SIZE,
                best_pitch,
                40,
                submode.comb_gain,
            );
            multicomb(
                &self.exc_buf,
                EXC + NB_SUBFRAME_SIZE,
                second,
                2 * NB_SUBFRAME_SIZE,
                best_pitch,
                40,
                submode.comb_gain,
            );
        }
        else {
            out.copy_from_slice(&self.exc_buf[EXC - NB_SUBFRAME_SIZE..][..NB_FRAME_SIZE]);
        }

        // If the last frame was lost, rescale the excitation to the energy that was encoded.
        if self.count_lost > 0 {
            let exc_ener = compute_rms(&self.exc_buf[EXC..EXC + NB_FRAME_SIZE]);
            let gain = (ol_gain / (exc_ener + 1.0)).min(2.0);
            for i in 0..NB_FRAME_SIZE {
                self.exc_buf[EXC + i] *= gain;
                out[i] = self.exc_buf[EXC + i - NB_SUBFRAME_SIZE];
            }
        }

        // Synthesis filter, sub-frame by sub-frame.
        let mut interp_qlsp = [0f32; NB_ORDER];
        let mut ak = [0f32; NB_ORDER];
        for sub in 0..NB_NB_SUBFRAMES {
            let sp = &mut out[NB_SUBFRAME_SIZE * sub..][..NB_SUBFRAME_SIZE];

            lsp_interpolate(&self.old_qlsp, &qlsp, &mut interp_qlsp, sub, NB_NB_SUBFRAMES, 0.002);
            lsp_to_lpc(&interp_qlsp, &mut ak);

            // Gain of the analysis filter at w = pi.
            let mut pi_g = 1.0f32;
            for i in (0..NB_ORDER).step_by(2) {
                pi_g += ak[i + 1] - ak[i];
            }
            self.pi_gain[sub] = pi_g;

            iir_mem(sp, &self.interp_qlpc, &mut self.mem_sp);

            self.interp_qlpc = ak;
        }

        highpass(out, if self.is_wideband { 3 } else { 1 }, &mut self.mem_hp);

        self.old_qlsp = qlsp;
        self.first = false;
        self.count_lost = 0;
        self.last_pitch = best_pitch;
        self.last_pitch_gain = (0.25 * f64::from(pitch_average)) as f32;
        self.pitch_gain_buf[self.pitch_gain_buf_idx] = self.last_pitch_gain;
        self.pitch_gain_buf_idx += 1;
        if self.pitch_gain_buf_idx > 2 {
            self.pitch_gain_buf_idx = 0;
        }

        Ok(())
    }
}

/// Handles an in-band request (`speex_inband_handler`). Only the stereo request is acted upon,
/// the others are skipped.
fn inband_request(bits: &mut Bits<'_>, stereo: Option<&mut StereoState>) {
    const STEREO_REQUEST: u32 = 9;

    let id = bits.unpack(4);

    if let (STEREO_REQUEST, Some(stereo)) = (id, stereo) {
        stereo.read_request(bits);
        return;
    }

    let skip = match id {
        0..=1 => 1,
        2..=7 => 4,
        8..=9 => 8,
        10..=11 => 16,
        12..=13 => 32,
        _ => 64,
    };
    bits.advance(skip);
}
