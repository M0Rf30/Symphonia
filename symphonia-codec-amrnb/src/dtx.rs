// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Receiver side discontinuous transmission (DTX): comfort noise generation.
//!
//! The comfort noise parameters are the SID frame layout of 3GPP TS 26.101 and the decoder
//! behaviour of TS 26.092 (comfort noise) and TS 26.093 (source controlled rate operation). The
//! algorithm follows the reference code of 3GPP TS 26.073 (`dtx_dec.c`, `b_cn_cod.c`), in floating
//! point: the reference quantities in Q-format are real numbers here (LSFs and LSPs in units of the
//! sampling rate and `[-1, 1]`, log energies in log2 units).
//!
//! The spectrum and level of the noise are interpolated from the previous to the latest SID
//! update over the period between the updates. Its excitation is ten pulses of equal magnitude
//! and random position and sign per subframe, the LSF vector gets a random variability taken from
//! the history of the speech spectrum.

use crate::celp::{LP_ORDER, SUBFRAME};
use crate::core::MIN_LSF_SPACING;
use crate::lsp::{lsf2lsp, lsp2lpc, set_min_dist_lsf};
use crate::tables::{DTX_LOG_EN_ADJUST, LSF_HIST_MEAN_SCALE, LSP_AVG_INIT};

/// Number of frames in the LSF and log energy histories.
const HIST_SIZE: usize = 8;
/// Initial value of the comfort noise generator shift register.
const PN_INITIAL_SEED: u32 = 0x7081_6958;
/// Frames of the encoder's hangover period (minus one) which the decoder skips counting.
const HANG_CONST: i32 = 7;
/// Number of frames without a SID update after which the comfort noise is muted.
const MAX_EMPTY_THRESH: i32 = 50;
/// Number of frames after which the decoder assumes the encoder has added a hangover period.
const ELAPSED_FRAMES_THRESH: i32 = 24 + 7 - 1;
/// Initial log energy: low level noise for better performance in DTX handover cases (Q11).
const INITIAL_LOG_EN: f32 = 3500.0 / 2048.0;
/// Log energy of the SID index 0, which is reserved for silence (`MIN_16` in Q11).
const SILENCE_LOG_EN: f32 = -16.0;
/// Number of pulses of the comfort noise excitation.
const CN_PULSES: usize = 10;
/// Soft and hard limits of the deviation of the LSFs from their mean (Q15 values 655 and 1310).
const LSF_DEVIATION_SOFT: f32 = 655.0 / 32768.0;
const LSF_DEVIATION_HARD: f32 = 1310.0 / 32768.0;
/// Bounds of the initial gain predictor memory in dB (Q10 values 0 and -14436).
const MA_PRED_MIN: f32 = -14436.0 / 1024.0;
/// Offset of the initial gain predictor memory in dB (Q10 value 9000).
const MA_PRED_OFFSET: f32 = 9000.0 / 1024.0;

/// State of the decoder's DTX state machine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum State {
    Speech,
    Dtx,
    DtxMute,
}

/// Received frame type, as far as it is relevant to the DTX state machine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Rx {
    /// A good speech frame.
    Speech,
    /// The first SID frame after speech; it has no comfort noise parameters.
    SidFirst,
    /// A SID frame with new comfort noise parameters.
    SidUpdate,
    /// A damaged SID frame.
    SidBad,
    /// No data was received.
    NoData,
}

/// Comfort noise parameters of a SID update frame.
pub(crate) struct SidData {
    /// LSP vector.
    pub lsp: [f64; LP_ORDER],
    /// Log energy index (6 bits).
    pub log_en_index: u16,
}

/// The parameters of one frame of comfort noise.
pub(crate) struct CnFrame {
    /// Interpolated LSFs, the spectrum of the speech decoder state.
    pub lsf: [f32; LP_ORDER],
    /// LPC coefficients for the post-filter.
    pub lpc: [f32; LP_ORDER],
    /// LPC coefficients (with LSF variability) of the synthesis filter.
    pub lpc_synth: [f32; LP_ORDER],
    /// Excitation of the four subframes.
    pub excitation: [[f32; SUBFRAME]; 4],
    /// Initial memory of the gain predictor in dB, when the SID frame reset it.
    pub ma_pred_init: Option<f32>,
}

/// DTX decoder state.
pub(crate) struct Dtx {
    since_last_sid: i32,
    true_sid_period_inv: f32,
    log_en: f32,
    old_log_en: f32,
    pn_seed: u32,
    lsp: [f64; LP_ORDER],
    lsp_old: [f64; LP_ORDER],
    lsf_hist: [[f32; LP_ORDER]; HIST_SIZE],
    lsf_hist_ptr: usize,
    lsf_hist_mean: [[f32; LP_ORDER]; HIST_SIZE],
    log_pg_mean: f32,
    log_en_hist: [f32; HIST_SIZE],
    log_en_hist_ptr: usize,
    log_en_adjust: f32,
    hangover_count: i32,
    dec_ana_elapsed: i32,
    sid_frame: bool,
    valid_data: bool,
    hangover_added: bool,
    /// The state of the previous frame.
    pub global_state: State,
    /// Whether comfort noise parameters were ever received.
    data_updated: bool,
}

/// Generates `bits` pseudo random bits (`pseudonoise`).
fn pseudonoise(shift_reg: &mut u32, bits: u32) -> usize {
    let mut noise = 0;
    for _ in 0..bits {
        let sn = (*shift_reg & 1) ^ ((*shift_reg >> 28) & 1);
        noise = (noise << 1) | (*shift_reg & 1) as usize;
        *shift_reg >>= 1;
        if sn != 0 {
            *shift_reg |= 0x4000_0000;
        }
    }
    noise
}

/// The product of `1 - k^2` of the reflection coefficients `k` of the LPC filter (`A_Refl`), which
/// is the normalized prediction error.
fn prediction_error(lpc: &[f32; LP_ORDER]) -> f64 {
    // 0.99997 in Q15.
    let mut err = 32767.0 / 32768.0;

    let mut a = [0.0f64; LP_ORDER];
    for (a, c) in a.iter_mut().zip(lpc) {
        *a = f64::from(*c);
    }

    for i in (0..LP_ORDER).rev() {
        let k = a[i];
        // An unstable filter has no valid reflection coefficients; the reference zeroes them all.
        if k.abs() >= 1.0 {
            return 32767.0 / 32768.0;
        }
        let d = 1.0 - k * k;
        err *= d;

        let mut b = [0.0f64; LP_ORDER];
        for j in 0..i {
            b[j] = (a[j] - k * a[i - 1 - j]) / d;
        }
        a = b;
    }
    err
}

impl Dtx {
    pub fn new() -> Self {
        let mut lsf = [0.0f32; LP_ORDER];
        for (l, v) in lsf.iter_mut().zip(LSP_AVG_INIT) {
            *l = f32::from(v) / 32768.0;
        }

        // LSPs of the decoder reset (`lsp_init_data`).
        let lsp_init = [
            30000.0, 26000.0, 21000.0, 15000.0, 8000.0, 0.0, -8000.0, -15000.0, -21000.0, -26000.0,
        ]
        .map(|v: f64| v / 32768.0);

        Dtx {
            since_last_sid: 0,
            true_sid_period_inv: 0.25,
            log_en: INITIAL_LOG_EN,
            old_log_en: INITIAL_LOG_EN,
            pn_seed: PN_INITIAL_SEED,
            lsp: lsp_init,
            lsp_old: lsp_init,
            lsf_hist: [lsf; HIST_SIZE],
            lsf_hist_ptr: 0,
            lsf_hist_mean: [[0.0; LP_ORDER]; HIST_SIZE],
            log_pg_mean: 0.0,
            log_en_hist: [INITIAL_LOG_EN; HIST_SIZE],
            log_en_hist_ptr: 0,
            log_en_adjust: 0.0,
            hangover_count: HANG_CONST,
            dec_ana_elapsed: 32767,
            sid_frame: false,
            valid_data: false,
            hangover_added: false,
            global_state: State::Dtx,
            data_updated: false,
        }
    }

    /// Records the quantized LSFs and the energy of the synthesis of a speech frame, which are the
    /// source of the comfort noise before the first SID update (`dtx_dec_activity_update`).
    pub fn activity_update(&mut self, lsf: &[f32; LP_ORDER], energy: f32) {
        self.lsf_hist_ptr = (self.lsf_hist_ptr + 1) % HIST_SIZE;
        self.lsf_hist[self.lsf_hist_ptr] = *lsf;

        // The reference computes log2 of twice the frame energy; the logarithm of the mean energy
        // is halved by the Q11 interpretation of the history.
        let energy = (2.0 * f64::from(energy)).clamp(1.0, f64::from(i32::MAX));
        let log_en = 0.5 * energy.log2() - (7497.0 + 1024.0) / 2048.0;

        self.log_en_hist_ptr = (self.log_en_hist_ptr + 1) % HIST_SIZE;
        self.log_en_hist[self.log_en_hist_ptr] = log_en as f32;
    }

    /// Decides what to do with a received frame and updates the state machine
    /// (`rx_dtx_handler`).
    pub fn rx_handler(&mut self, rx: Rx) -> State {
        let is_sid = matches!(rx, Rx::SidFirst | Rx::SidUpdate | Rx::SidBad);
        let in_dtx = matches!(self.global_state, State::Dtx | State::DtxMute);

        // DTX if SID frame or previously in DTX and no data.
        let new_state = if is_sid || (in_dtx && rx == Rx::NoData) {
            let mut state = State::Dtx;

            // Stay in mute for these input types.
            if self.global_state == State::DtxMute
                && matches!(rx, Rx::SidBad | Rx::SidFirst | Rx::NoData)
            {
                state = State::DtxMute;
            }

            // Evaluate if the noise parameters are too old. SID updates are handled separately to
            // avoid entering mute for late updates.
            self.since_last_sid = (self.since_last_sid + 1).min(i32::from(i16::MAX));
            if rx != Rx::SidUpdate && self.since_last_sid > MAX_EMPTY_THRESH {
                state = State::DtxMute;
            }
            state
        }
        else {
            self.since_last_sid = 0;
            State::Speech
        };

        // Reset the counter when receiving comfort noise data for the first time, to make the
        // counter robust against mismatch after handover.
        if !self.data_updated && rx == Rx::SidUpdate {
            self.dec_ana_elapsed = 0;
        }

        // Track when the encoder added a DTX hangover period.
        self.dec_ana_elapsed = (self.dec_ana_elapsed + 1).min(i32::from(i16::MAX));
        self.hangover_added = false;

        // A frame without data in speech may mean that the encoder sent speech.
        let encoder_speech = rx == Rx::Speech || (rx == Rx::NoData && new_state == State::Speech);

        if encoder_speech {
            self.hangover_count = HANG_CONST;
        }
        else if self.dec_ana_elapsed > ELAPSED_FRAMES_THRESH {
            self.hangover_added = true;
            self.dec_ana_elapsed = 0;
            self.hangover_count = 0;
        }
        else if self.hangover_count == 0 {
            self.dec_ana_elapsed = 0;
        }
        else {
            self.hangover_count -= 1;
        }

        if new_state != State::Speech {
            // The comfort noise data is not in the first SID, it is marked as bad but does a
            // backward analysis if a hangover period was added.
            self.sid_frame = false;
            self.valid_data = false;

            match rx {
                Rx::SidFirst => self.sid_frame = true,
                Rx::SidUpdate => {
                    self.sid_frame = true;
                    self.valid_data = true;
                }
                Rx::SidBad => {
                    self.sid_frame = true;
                    // Use the old data.
                    self.hangover_added = false;
                }
                Rx::Speech | Rx::NoData => (),
            }
        }

        new_state
    }

    /// Generates the parameters of a comfort noise frame (`dtx_dec`).
    ///
    /// `sid` has the parameters of a SID update frame; `mode` is the speech mode.
    pub fn decode(&mut self, mode: u8, new_state: State, sid: Option<&SidData>) -> CnFrame {
        let mode_adjust = DTX_LOG_EN_ADJUST[usize::from(mode).min(DTX_LOG_EN_ADJUST.len() - 1)];
        let mut ma_pred_init = None;

        if self.hangover_added && self.sid_frame {
            // SID first or update after a DTX hangover period: derive the noise from the history
            // of the decoded speech.
            self.log_en_adjust = mode_adjust;

            // Replace the oldest entries with copies of the newest.
            let next = (self.lsf_hist_ptr + 1) % HIST_SIZE;
            self.lsf_hist[next] = self.lsf_hist[self.lsf_hist_ptr];
            let next = (self.log_en_hist_ptr + 1) % HIST_SIZE;
            self.log_en_hist[next] = self.log_en_hist[self.log_en_hist_ptr];

            // Average energy and LSFs.
            self.log_en = self.log_en_hist.iter().map(|e| e / HIST_SIZE as f32).sum();
            let mut lsf = [0.0f32; LP_ORDER];
            for row in &self.lsf_hist {
                for (l, v) in lsf.iter_mut().zip(row) {
                    *l += v / HIST_SIZE as f32;
                }
            }
            lsf2lsp(&mut self.lsp, &lsf);

            // Make the log energy independent of the speech mode; it is added again before
            // synthesis.
            self.log_en -= self.log_en_adjust;

            // Compute the LSF variability vectors: the deviation of each stored vector from the
            // mean, attenuated (especially for upper LSFs) and limited.
            self.lsf_hist_mean = self.lsf_hist;
            for i in 0..LP_ORDER {
                let mean: f32 = self.lsf_hist_mean.iter().map(|r| r[i] / HIST_SIZE as f32).sum();
                for row in self.lsf_hist_mean.iter_mut() {
                    let v = (row[i] - mean) * LSF_HIST_MEAN_SCALE[i];
                    let mut m = v.abs();
                    if m > LSF_DEVIATION_SOFT {
                        m = LSF_DEVIATION_SOFT + (m - LSF_DEVIATION_SOFT) / 4.0;
                    }
                    m = m.min(LSF_DEVIATION_HARD);
                    row[i] = if v < 0.0 { -m } else { m };
                }
            }
        }

        if self.sid_frame {
            // Set the old SID parameters, always shift, even without new valid data.
            self.lsp_old = self.lsp;
            self.old_log_en = self.log_en;

            if let (true, Some(sid)) = (self.valid_data, sid) {
                // Interpolation factor: the division only works for values below 32, so limit the
                // interpolation to 32 frames.
                let length = self.since_last_sid.min(32);
                self.since_last_sid = 0;
                self.true_sid_period_inv = if length >= 2 { 1.0 / length as f32 } else { 0.5 };

                self.lsp = sid.lsp;

                // Index 0 is reserved for silence, otherwise the index is in steps of 1/4 with an
                // offset of -2.5.
                self.log_en = if sid.log_en_index == 0 {
                    SILENCE_LOG_EN
                }
                else {
                    f32::from(sid.log_en_index) / 4.0 - 2.5
                };

                // No interpolation at startup after a decoder reset or when the SID update was
                // received right after speech.
                if !self.data_updated || self.global_state == State::Speech {
                    self.lsp_old = self.lsp;
                    self.old_log_en = self.log_en;
                }
            }

            // Initialize the gain predictor memory for the next speech frame.
            ma_pred_init = Some((self.log_en - MA_PRED_OFFSET).clamp(MA_PRED_MIN, 0.0));
        }

        // Recompute the level adjustment factor: 0.9 * old + 0.1 * mode adjustment.
        self.log_en_adjust = 0.9 * self.log_en_adjust + 0.1 * mode_adjust;

        // Interpolate the SID info from the old to the new.
        let int_fac = ((1 + self.since_last_sid) as f32 * self.true_sid_period_inv).min(1.0);

        let log_en_int = int_fac * self.log_en + (1.0 - int_fac) * self.old_log_en;
        let mut lsp_int = [0.0f64; LP_ORDER];
        for i in 0..LP_ORDER {
            lsp_int[i] =
                f64::from(int_fac) * self.lsp[i] + f64::from(1.0 - int_fac) * self.lsp_old[i];
        }

        // The amount of LSF variability depends on the prediction gain of the speech.
        let lsf_variab_factor = (1.0 - (self.log_pg_mean - 0.6) * 0.3).clamp(0.0, 1.0);
        let lsf_variab_index = pseudonoise(&mut self.pn_seed, 3);

        let mut lsf_int = [0.0f32; LP_ORDER];
        for (l, p) in lsf_int.iter_mut().zip(&lsp_int) {
            *l = (p.acos() / (2.0 * std::f64::consts::PI)) as f32;
        }

        let mut lsf_variab = lsf_int;
        for i in 0..LP_ORDER {
            lsf_variab[i] += lsf_variab_factor * self.lsf_hist_mean[lsf_variab_index][i];
        }

        // Make sure that the LSFs are ordered.
        set_min_dist_lsf(&mut lsf_int, MIN_LSF_SPACING);
        set_min_dist_lsf(&mut lsf_variab, MIN_LSF_SPACING);

        let mut lsp_variab = [0.0f64; LP_ORDER];
        lsf2lsp(&mut lsp_int, &lsf_int);
        lsf2lsp(&mut lsp_variab, &lsf_variab);

        // The plain filter is used for the level normalization and the post-filter, the filter
        // with variability is used for the synthesis. This keeps the level in the high frequencies
        // from jumping up and down.
        let mut lpc = [0.0f32; LP_ORDER];
        let mut lpc_synth = [0.0f32; LP_ORDER];
        lsp2lpc(&lsp_int, &mut lpc);
        lsp2lpc(&lsp_variab, &mut lpc_synth);

        // The logarithm of the prediction gain, in log2 units of the amplitude.
        let log_pg = (-0.5 * prediction_error(&lpc).log2()) as f32;
        self.log_pg_mean = 0.9 * self.log_pg_mean + 0.1 * log_pg;

        // The level of the pulses: add 4 (the unit of the pulse amplitude), subtract the
        // prediction gain and adjust the level to the speech mode.
        let level_log = log_en_int + 4.0 - log_pg + self.log_en_adjust;
        let level = f64::from(level_log).exp2().floor().clamp(0.0, 32767.0) as i32;
        // The pulses have amplitude 4096 (Q15 0.125): `mult(level, +-4096)`.
        let amp = (level * 4096) >> 15;
        let amp_neg = -((level * 4096 + 32767) >> 15);

        let mut excitation = [[0.0f32; SUBFRAME]; 4];
        for ex in excitation.iter_mut() {
            for k in 0..CN_PULSES {
                let pos = 10 * pseudonoise(&mut self.pn_seed, 2) + k;
                let sign = pseudonoise(&mut self.pn_seed, 1);
                ex[pos] = if sign > 0 { amp as f32 } else { amp_neg as f32 };
            }
        }

        if new_state == State::DtxMute {
            // Mute the comfort noise as it has been a long time since the last SID update.
            let length = self.since_last_sid.min(32);
            // Safety guard against division by zero.
            let length = if length <= 0 { 8 } else { length };
            self.true_sid_period_inv = 1.0 / length as f32;

            self.since_last_sid = 0;
            self.lsp_old = self.lsp;
            self.old_log_en = self.log_en;
            // Subtract 1/8 in Q11, that is -6/8 dB.
            self.log_en -= 256.0 / 2048.0;
        }

        // Reset the interpolation length timer if data has been updated.
        if self.sid_frame && (self.valid_data || self.hangover_added) {
            self.since_last_sid = 0;
            self.data_updated = true;
        }

        CnFrame { lsf: lsf_int, lpc, lpc_synth, excitation, ma_pred_init }
    }
}
