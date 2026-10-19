// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! AMR-WB discontinuous transmission, receive side: the comfort noise (CN) parameter decoder and
//! the receive state machine.
//!
//! The algorithm is 3GPP TS 26.193 (source controlled rate operation) and TS 26.192 (comfort noise
//! aspects). It is a floating-point port of the decoder-side functions of the OpenCORE AMR-WB
//! decoder (`dtx_decoder_amr_wb.cpp`: `rx_amr_wb_dtx_handler`, `dtx_dec_amr_wb`,
//! `dtx_dec_amr_wb_activity_update`, `CN_dithering`; `qisf_ns.cpp`: `Disf_ns`;
//! `noise_gen_amrwb.cpp`), which itself follows TS 26.173. The text of the specifications was not
//! available while writing, so every statement below about the algorithm is taken from that
//! implementation.

use crate::lsp::set_min_dist_lsf;
use crate::tables::{
    DICO1_ISF_NOISE, DICO2_ISF_NOISE, DICO3_ISF_NOISE, DICO4_ISF_NOISE, DICO5_ISF_NOISE, ISF_INIT,
    ISF_MEAN_NOISE,
};

/// ISF order.
pub const ORDER: usize = 16;
/// Length of the CN excitation: one 12.8 kHz core frame.
pub const EXC_LEN: usize = 256;

/// Number of frames in the ISF and log energy histories.
const HIST_SIZE: usize = 8;
/// Number of frames without a SID update after which the CN is muted.
const MAX_EMPTY_THRESH: i32 = 50;
/// Number of frames after which a SID is assumed to follow a hangover period
/// (`DTX_ELAPSED_FRAMES_THRESH` = 24 + 7 - 1).
const ELAPSED_FRAMES_THRESH: i32 = 24 + 7 - 1;
/// Hangover frames (`DTX_HANG_CONST`).
const HANG_CONST: i32 = 7;
/// Initial value of the random number generators (`RANDOM_INITSEED`).
const RANDOM_INITSEED: i16 = 21845;
/// Raw (Q7) value the log energy history is initialized with (`log_en = 3500`). This is
/// preserved from the reference, even though the same constant is a Q9 number elsewhere.
const LOG_EN_HIST_INIT_Q7: f32 = 3500.0 / 128.0;
/// Initial log energy, `3500` in Q9.
const LOG_EN_INIT: f32 = 3500.0 / 512.0;
/// Minimum ISF spacing, 128 in Q15.
const ISF_GAP: f32 = 128.0 / 32768.0;
/// Minimum ISF spacing after dithering, 448 in Q15 (`ISF_DITH_GAP`).
const ISF_DITH_GAP: f32 = 448.0 / 32768.0;
/// Dithering factor of the first ISF (`ISF_FACTOR_LOW`).
const ISF_FACTOR_LOW: i32 = 256;
/// Increment of the dithering factor per ISF (`ISF_FACTOR_STEP`).
const ISF_FACTOR_STEP: i32 = 2;
/// Dithering factor of the log energy (`GAIN_FACTOR`).
const GAIN_FACTOR: i32 = 75;

/// Synthesis state: speech, comfort noise, or comfort noise that is being faded out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Speech,
    Dtx,
    DtxMute,
}

/// Received frame classification (`RX_*`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rx {
    SpeechGood,
    SpeechBad,
    SidFirst,
    SidUpdate,
    SidBad,
    NoData,
    SpeechLost,
}

/// Parameters of a good SID update frame (TS 26.192).
#[derive(Clone, Copy, Default, Debug)]
pub struct SidData {
    /// Indices of the five ISF sub-vector codebooks (6, 6, 6, 5 and 5 bits).
    pub isf_idx: [u16; 5],
    /// Log energy index (6 bits).
    pub log_en_idx: u16,
    /// Background noise non-stationarity flag (1 bit): enables comfort noise dithering.
    pub dither: bool,
}

impl SidData {
    /// Parses the 35 bit SID frame payload (after the table of contents byte) that holds the ISF
    /// indices, the log energy index and the dithering flag. The following bit is the SID type
    /// indicator (see [`SidData::is_update`]). Returns `None` if `data` is too short.
    pub fn parse(data: &[u8]) -> Option<SidData> {
        let mut pos = 0usize;
        let mut take = |n: usize| -> Option<u16> {
            let mut v = 0u16;
            for _ in 0..n {
                let byte = *data.get(pos >> 3)?;
                v = (v << 1) | u16::from((byte >> (7 - (pos & 7))) & 1);
                pos += 1;
            }
            Some(v)
        };

        let isf_idx = [take(6)?, take(6)?, take(6)?, take(5)?, take(5)?];
        let log_en_idx = take(6)?;
        let dither = take(1)? != 0;

        Some(SidData { isf_idx, log_en_idx, dither })
    }

    /// Reads the SID type indicator (STI, bit 35): `true` for a SID_UPDATE, `false` for a
    /// SID_FIRST frame.
    pub fn is_update(data: &[u8]) -> Option<bool> {
        Some(*data.get(4)? & 0x10 != 0)
    }
}

/// Comfort noise for one core frame.
pub struct CnFrame {
    /// ISF vector (Q15 divided by 32768; the last element is not doubled).
    pub isf: [f32; ORDER],
    /// Excitation at 12.8 kHz, in the 16-bit sample domain.
    pub exc: [f32; EXC_LEN],
}

/// `noise_gen_amrwb`: 16-bit linear congruential generator.
fn noise_gen(seed: &mut i16) -> i16 {
    *seed = (i32::from(*seed)).wrapping_mul(31821).wrapping_add(13849) as i16;
    *seed
}

/// `mult_int16_r`: Q15 multiplication with rounding.
fn mult_r(a: i32, b: i32) -> i32 {
    (a * b + 0x4000) >> 15
}

/// Decoder-side DTX state.
#[derive(Clone)]
pub struct Dtx {
    /// Frames since the last SID update (`since_last_sid`).
    since_last_sid: i32,
    /// Inverse of the SID update period, the CN interpolation rate (`true_sid_period_inv`).
    true_sid_period_inv: f32,
    /// `log2(E) + 2` of the most recent SID, or of the background noise (`log_en`, Q9 / 512).
    log_en: f32,
    /// The same for the previous SID.
    old_log_en: f32,
    cng_seed: i16,
    dither_seed: i16,
    hist_ptr: usize,
    /// Most recent SID ISF vector.
    isf: [f32; ORDER],
    /// Previous SID ISF vector.
    isf_old: [f32; ORDER],
    isf_hist: [[f32; ORDER]; HIST_SIZE],
    /// `log2` of the mean energy of the excitation of the last speech frames.
    log_en_hist: [f32; HIST_SIZE],
    hangover_count: i32,
    elapsed_count: i32,
    sid_frame: bool,
    valid_data: bool,
    hangover_added: bool,
    /// State of the previous frame (`dtxGlobalState`).
    pub global_state: State,
    data_updated: bool,
    dither_enabled: bool,
}

impl Dtx {
    pub fn new() -> Self {
        let mut isf_init = [0.0f32; ORDER];
        for (d, &s) in isf_init.iter_mut().zip(ISF_INIT.iter()) {
            *d = f32::from(s) * (1.0 / 32768.0);
        }

        Dtx {
            since_last_sid: 0,
            true_sid_period_inv: 0.25,
            log_en: LOG_EN_INIT,
            old_log_en: LOG_EN_INIT,
            cng_seed: RANDOM_INITSEED,
            dither_seed: RANDOM_INITSEED,
            hist_ptr: 0,
            isf: isf_init,
            isf_old: isf_init,
            isf_hist: [isf_init; HIST_SIZE],
            log_en_hist: [LOG_EN_HIST_INIT_Q7; HIST_SIZE],
            hangover_count: HANG_CONST,
            elapsed_count: 32767,
            sid_frame: false,
            valid_data: false,
            hangover_added: false,
            global_state: State::Speech,
            data_updated: false,
            dither_enabled: false,
        }
    }

    /// Records the ISF vector and the excitation energy of a decoded speech frame
    /// (`dtx_dec_amr_wb_activity_update`). `isf` is the decoded ISF vector with the last element
    /// not doubled, `exc_energy` the sum of the squares of the 256 excitation samples.
    pub fn activity_update(&mut self, isf: &[f32; ORDER], exc_energy: f64) {
        self.hist_ptr = (self.hist_ptr + 1) % HIST_SIZE;
        self.isf_hist[self.hist_ptr] = *isf;

        // `amrwb_log_2` yields the integer and 15 bit fractional parts of the logarithm; the
        // result is kept with 7 fractional bits. Zero maps to zero, and 256 samples are
        // averaged (-8).
        let log2 = if exc_energy >= 1.0 { exc_energy.log2() } else { 0.0 };
        self.log_en_hist[self.hist_ptr] = ((log2 * 128.0).floor() / 128.0) as f32 - 8.0;
        self.global_state = State::Speech;
    }

    /// Advances the receive state machine for a frame and returns the new synthesis state
    /// (`rx_amr_wb_dtx_handler`).
    pub fn rx_handler(&mut self, rx: Rx) -> State {
        let in_dtx = matches!(self.global_state, State::Dtx | State::DtxMute);

        let new_state = if matches!(rx, Rx::SidFirst | Rx::SidUpdate | Rx::SidBad)
            || (in_dtx && matches!(rx, Rx::NoData | Rx::SpeechBad | Rx::SpeechLost))
        {
            let mut s = State::Dtx;

            // Stay in mute for these frame types.
            if self.global_state == State::DtxMute
                && matches!(rx, Rx::SidBad | Rx::SidFirst | Rx::SpeechLost | Rx::NoData)
            {
                s = State::DtxMute;
            }

            self.since_last_sid += 1;

            // The CN parameters are too old.
            if self.since_last_sid > MAX_EMPTY_THRESH {
                s = State::DtxMute;
            }
            s
        }
        else {
            self.since_last_sid = 0;
            State::Speech
        };

        // Reset the counter at the first SID update, in case the counters are not in sync.
        if !self.data_updated && rx == Rx::SidUpdate {
            self.elapsed_count = 0;
        }

        // Track whether the encoder added a hangover period.
        self.elapsed_count = (self.elapsed_count + 1).min(32767);
        self.hangover_added = false;

        let enc_dtx = matches!(rx, Rx::SidFirst | Rx::SidUpdate | Rx::SidBad | Rx::NoData);

        if !enc_dtx {
            self.hangover_count = HANG_CONST;
        }
        else if self.elapsed_count > ELAPSED_FRAMES_THRESH {
            self.hangover_added = true;
            self.elapsed_count = 0;
            self.hangover_count = 0;
        }
        else if self.hangover_count == 0 {
            self.elapsed_count = 0;
        }
        else {
            self.hangover_count -= 1;
        }

        if new_state != State::Speech {
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
                _ => (),
            }
        }

        new_state
    }

    /// Generates the comfort noise parameters and excitation for a frame (`dtx_dec_amr_wb`).
    /// `sid` is the decoded payload of a SID update frame, and is only used if the preceding call
    /// to [`Dtx::rx_handler`] was for one.
    pub fn decode(&mut self, new_state: State, sid: Option<&SidData>) -> CnFrame {
        if self.hangover_added && self.sid_frame {
            // A SID after a hangover period, estimated by the encoder from the speech that
            // preceded it: average the history of the decoded speech instead (backward analysis),
            // where the last frame counts twice.
            let next = (self.hist_ptr + 1) % HIST_SIZE;
            self.isf_hist[next] = self.isf_hist[self.hist_ptr];
            self.log_en_hist[next] = self.log_en_hist[self.hist_ptr];

            // The mean log energy, plus the 2.0 offset of the "log2(E) + 2" representation.
            let sum: f32 = self.log_en_hist.iter().sum();
            self.log_en = (sum / HIST_SIZE as f32 + 2.0).max(0.0);

            for j in 0..ORDER {
                self.isf[j] = self.isf_hist.iter().map(|v| v[j]).sum::<f32>() / HIST_SIZE as f32;
            }
        }

        if self.sid_frame {
            // Always shift the parameters, even if there is no new data.
            self.isf_old = self.isf;
            self.old_log_en = self.log_en;

            if let (true, Some(sid)) = (self.valid_data, sid) {
                // The interpolation is only defined for up to 32 frames.
                let len = self.since_last_sid.min(32);
                self.true_sid_period_inv = if len >= 2 { 1.0 / len as f32 } else { 0.5 };

                self.isf = decode_isf(&sid.isf_idx);

                // log2(E) lies in -2..22; the "-2" is applied after the power of two.
                self.log_en = f32::from(sid.log_en_idx) / 2.625;
                self.dither_enabled = sid.dither;

                // No interpolation at startup, or when the first SID update directly follows
                // speech.
                if !self.data_updated || self.global_state == State::Speech {
                    self.isf_old = self.isf;
                    self.old_log_en = self.log_en;
                }
            }
        }

        if self.sid_frame && self.valid_data {
            self.since_last_sid = 0;
        }

        // Interpolate the SID information (up to the most recent one).
        let k = (self.since_last_sid as f32 * self.true_sid_period_inv).min(1.0);
        let mut log_en = k * self.log_en + (1.0 - k) * self.old_log_en;
        let mut isf = [0.0f32; ORDER];
        for (i, v) in isf.iter_mut().enumerate() {
            *v = k * self.isf[i] + (1.0 - k) * self.isf_old[i];
        }

        if self.dither_enabled {
            self.dither(&mut isf, &mut log_en);
        }

        // `log_en` is log2(E) + 2, so the excitation RMS is 2^(log2(E) / 2) = 2^(log_en / 2 - 1).
        let level = (f64::from(log_en) * 0.5 - 1.0).exp2();

        // White noise at a normalized power.
        let mut exc = [0.0f32; EXC_LEN];
        let mut energy = 0.0f64;
        for e in exc.iter_mut() {
            let v = noise_gen(&mut self.cng_seed) >> 4;
            *e = f32::from(v);
            energy += f64::from(v) * f64::from(v);
        }

        // gain = level / sqrt(energy) * sqrt(EXC_LEN)
        if energy > 0.0 {
            let gain = (level / energy.sqrt() * (EXC_LEN as f64).sqrt()) as f32;
            for e in exc.iter_mut() {
                *e *= gain;
            }
        }

        if new_state == State::DtxMute {
            // It has been a long time since the last SID update: fade the noise out.
            let len = self.since_last_sid.min(32);
            self.true_sid_period_inv = if len >= 1 { 1.0 / len as f32 } else { 1.0 };
            self.since_last_sid = 0;
            self.old_log_en = self.log_en;

            // Subtract 1/8 (energy -3/8 dB) in the Q9 log domain.
            self.log_en = (self.log_en - 64.0 / 512.0).max(0.0);
        }

        // The interpolation timer restarts when the data was updated.
        if self.sid_frame && (self.valid_data || self.hangover_added) {
            self.since_last_sid = 0;
            self.data_updated = true;
        }

        CnFrame { isf, exc }
    }

    /// Comfort noise dithering of the energy and the ISF vector (`CN_dithering`).
    fn dither(&mut self, isf: &mut [f32; ORDER], log_en: &mut f32) {
        let rand = |seed: &mut i16| -> i32 {
            let a = noise_gen(seed) >> 1;
            let b = noise_gen(seed) >> 1;
            // The sum is an `int16` in the reference.
            i32::from(a.wrapping_add(b))
        };

        let r = rand(&mut self.dither_seed);
        *log_en = (*log_en + (r * GAIN_FACTOR * 2) as f32 / (1 << 24) as f32).max(0.0);

        let mut factor = ISF_FACTOR_LOW;
        let r = rand(&mut self.dither_seed);
        isf[0] = (isf[0] + mult_r(r, factor) as f32 / 32768.0).max(ISF_GAP);

        for i in 1..ORDER - 1 {
            factor += ISF_FACTOR_STEP;
            let r = rand(&mut self.dither_seed);
            let v = isf[i] + mult_r(r, factor) as f32 / 32768.0;
            // Keep the spacing.
            isf[i] = if v - isf[i - 1] < ISF_DITH_GAP { isf[i - 1] + ISF_DITH_GAP } else { v };
        }

        // The second to last ISF must not exceed 16384 (0.5).
        isf[ORDER - 2] = isf[ORDER - 2].min(0.5);
    }
}

/// Decodes the ISF vector of a SID frame from the five codebook indices (`Disf_ns`).
fn decode_isf(idx: &[u16; 5]) -> [f32; ORDER] {
    const S: f32 = 1.0 / 32768.0;
    let ix = |i: usize, max: usize| usize::from(idx[i]).min(max - 1);

    let mut isf = [0.0f32; ORDER];

    for (i, &v) in DICO1_ISF_NOISE[ix(0, 64)].iter().enumerate() {
        isf[i] = f32::from(v) * S;
    }
    for (i, &v) in DICO2_ISF_NOISE[ix(1, 64)].iter().enumerate() {
        isf[2 + i] = f32::from(v) * S;
    }
    for (i, &v) in DICO3_ISF_NOISE[ix(2, 64)].iter().enumerate() {
        isf[5 + i] = f32::from(v) * S;
    }
    for (i, &v) in DICO4_ISF_NOISE[ix(3, 32)].iter().enumerate() {
        isf[8 + i] = f32::from(v) * S;
    }
    for (i, &v) in DICO5_ISF_NOISE[ix(4, 32)].iter().enumerate() {
        isf[12 + i] = f32::from(v) * S;
    }
    for (v, &m) in isf.iter_mut().zip(ISF_MEAN_NOISE.iter()) {
        *v += f32::from(m) * S;
    }

    set_min_dist_lsf(&mut isf[..ORDER - 1], f64::from(ISF_GAP));
    isf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sid_parse() {
        // indices 1, 2, 3, 4, 5, log_en 33, dither 1, STI 1:
        // 000001 000010 000011 00100 00101 100001 1 1 + 4 mode bits.
        let bits = "00000100001000001100100001011000011100000";
        let mut bytes = [0u8; 5];
        for (i, c) in bits.chars().enumerate() {
            if c == '1' {
                bytes[i / 8] |= 0x80 >> (i % 8);
            }
        }
        let sid = SidData::parse(&bytes).unwrap();
        assert_eq!(sid.isf_idx, [1, 2, 3, 4, 5]);
        assert_eq!(sid.log_en_idx, 33);
        assert!(sid.dither);
        assert_eq!(SidData::is_update(&bytes), Some(true));
        assert!(SidData::parse(&bytes[..4]).is_none());
    }

    #[test]
    fn noise_generator_sequence() {
        // The product is truncated to 16 bits, as the reference `int16` cast.
        let mut seed = RANDOM_INITSEED;
        let v = noise_gen(&mut seed);
        assert_eq!(i32::from(v), ((21845i64 * 31821 + 13849) as i16).into());
        assert_eq!(seed, v);
    }
}
