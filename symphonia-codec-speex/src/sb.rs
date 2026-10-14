// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The sub-band decoder of the wide-band and ultra-wide-band modes.
//!
//! Ported from libspeex `sb_celp.c` and `modes_wb.c` (BSD-3-Clause), see NOTICE. A wide-band
//! stream is a narrow-band stream (the low band, 0-4 kHz) with a high band (4-8 kHz) coded on top
//! of it, and the two are recombined with a QMF filter bank. An ultra-wide-band stream is in turn
//! a wide-band stream with a further high band.

use crate::bits::Bits;
use crate::dsp::{
    QMF_ORDER, bw_lpc, compute_rms, iir_mem, lsp_interpolate, lsp_to_lpc, qmf_synth, speex_rand,
};
use crate::nb::{NbDecoder, SplitCb, Status};
use crate::stereo::StereoState;
use crate::tables;

/// Number of sub-frames per frame of the sub-band modes.
const SB_NB_SUBFRAMES: usize = 4;
/// Order of the high-band LPC filter.
const SB_LPC_SIZE: usize = 8;
const VERY_SMALL: f32 = 1e-15;

/// A high-band sub-mode (`SpeexSubmode` in `modes_wb.c`). The LSPs are always coded with
/// `lsp_unquant_high`.
struct Submode {
    /// The innovation codebook. `None` folds the low-band innovation into the high band instead.
    innovation: Option<SplitCb>,
    double_codebook: bool,
}

const NO_SUBMODE: Option<Submode> = None;

static WB_SUBMODES: [Option<Submode>; 8] = [
    None,
    Some(Submode { innovation: None, double_codebook: false }),
    Some(Submode {
        innovation: Some(SplitCb {
            subvect_size: 10,
            nb_subvect: 4,
            shape_cb: &tables::HEXC_10_32_TABLE,
            shape_bits: 5,
            have_sign: false,
        }),
        double_codebook: false,
    }),
    Some(Submode {
        innovation: Some(SplitCb {
            subvect_size: 8,
            nb_subvect: 5,
            shape_cb: &tables::HEXC_TABLE,
            shape_bits: 7,
            have_sign: true,
        }),
        double_codebook: false,
    }),
    Some(Submode {
        innovation: Some(SplitCb {
            subvect_size: 8,
            nb_subvect: 5,
            shape_cb: &tables::HEXC_TABLE,
            shape_bits: 7,
            have_sign: true,
        }),
        double_codebook: true,
    }),
    NO_SUBMODE,
    NO_SUBMODE,
    NO_SUBMODE,
];

static UWB_SUBMODES: [Option<Submode>; 8] = [
    None,
    Some(Submode { innovation: None, double_codebook: false }),
    NO_SUBMODE,
    NO_SUBMODE,
    NO_SUBMODE,
    NO_SUBMODE,
    NO_SUBMODE,
    NO_SUBMODE,
];

/// Reads the high-band LSPs (`lsp_unquant_high`).
fn lsp_unquant_high(lsp: &mut [f32; SB_LPC_SIZE], bits: &mut Bits<'_>) {
    for (i, l) in lsp.iter_mut().enumerate() {
        *l = (0.3125 * i as f64 + 0.75) as f32;
    }

    let id = bits.unpack(6) as usize;
    for i in 0..SB_LPC_SIZE {
        lsp[i] = (f64::from(lsp[i])
            + 0.0039062 * f64::from(tables::HIGH_LSP_CDBK[id * SB_LPC_SIZE + i]))
            as f32;
    }

    let id = bits.unpack(6) as usize;
    for i in 0..SB_LPC_SIZE {
        lsp[i] = (f64::from(lsp[i])
            + 0.0019531 * f64::from(tables::HIGH_LSP_CDBK2[id * SB_LPC_SIZE + i]))
            as f32;
    }
}

/// The layer below a sub-band decoder.
enum LowBand {
    Nb(Box<NbDecoder>),
    Sb(Box<SbDecoder>),
}

impl LowBand {
    fn decode(
        &mut self,
        bits: &mut Bits<'_>,
        out: &mut [f32],
        innov_save: Option<&mut [f32]>,
        stereo: Option<&mut StereoState>,
    ) -> Result<(), Status> {
        match self {
            LowBand::Nb(nb) => nb.decode(bits, out, innov_save, stereo),
            LowBand::Sb(sb) => sb.decode(bits, out, innov_save, stereo),
        }
    }

    fn decode_lost(&mut self, out: &mut [f32]) {
        match self {
            LowBand::Nb(nb) => nb.decode_lost(out),
            LowBand::Sb(sb) => sb.decode_lost(out),
        }
    }

    fn dtx_enabled(&self) -> bool {
        match self {
            LowBand::Nb(nb) => nb.dtx_enabled(),
            LowBand::Sb(sb) => sb.low.dtx_enabled(),
        }
    }

    fn pi_gain(&self) -> [f32; SB_NB_SUBFRAMES] {
        match self {
            LowBand::Nb(nb) => nb.pi_gain(),
            LowBand::Sb(sb) => sb.pi_gain,
        }
    }

    fn exc_rms(&self) -> [f32; SB_NB_SUBFRAMES] {
        match self {
            LowBand::Nb(nb) => {
                let mut rms = [0f32; SB_NB_SUBFRAMES];
                nb.exc_rms(&mut rms);
                rms
            }
            LowBand::Sb(sb) => sb.exc_rms,
        }
    }
}

/// Decoder state of a sub-band stream (`SBDecState`).
pub struct SbDecoder {
    low: LowBand,
    /// Samples per frame of the low band, i.e. half of the output frame.
    frame_size: usize,
    subframe_size: usize,
    folding_gain: f32,
    submodes: &'static [Option<Submode>; 8],
    first: bool,
    g0_mem: [f32; QMF_ORDER],
    g1_mem: [f32; QMF_ORDER],
    exc_buf: Vec<f32>,
    old_qlsp: [f32; SB_LPC_SIZE],
    interp_qlpc: [f32; SB_LPC_SIZE],
    mem_sp: [f32; SB_LPC_SIZE],
    pi_gain: [f32; SB_NB_SUBFRAMES],
    exc_rms: [f32; SB_NB_SUBFRAMES],
    last_ener: f32,
    seed: u32,
    /// The innovation of the low band, which gets folded into the high band.
    low_innov: Vec<f32>,
    /// The high band, before recombination with the low band.
    high: Vec<f32>,
    xx1: Vec<f32>,
    xx2: Vec<f32>,
}

impl SbDecoder {
    /// Creates the decoder of the wide-band mode.
    pub fn new_wideband() -> Self {
        Self::new(LowBand::Nb(Box::new(NbDecoder::new(true))), 160, 40, 0.9, &WB_SUBMODES)
    }

    /// Creates the decoder of the ultra-wide-band mode.
    pub fn new_ultra_wideband() -> Self {
        let low = LowBand::Sb(Box::new(Self::new_wideband()));
        Self::new(low, 320, 80, 0.7, &UWB_SUBMODES)
    }

    fn new(
        low: LowBand,
        frame_size: usize,
        subframe_size: usize,
        folding_gain: f32,
        submodes: &'static [Option<Submode>; 8],
    ) -> Self {
        SbDecoder {
            low,
            frame_size,
            subframe_size,
            folding_gain,
            submodes,
            first: true,
            g0_mem: [0.0; QMF_ORDER],
            g1_mem: [0.0; QMF_ORDER],
            exc_buf: vec![0.0; subframe_size],
            old_qlsp: [0.0; SB_LPC_SIZE],
            interp_qlpc: [0.0; SB_LPC_SIZE],
            mem_sp: [0.0; SB_LPC_SIZE],
            pi_gain: [0.0; SB_NB_SUBFRAMES],
            exc_rms: [0.0; SB_NB_SUBFRAMES],
            last_ener: 0.0,
            seed: 1000,
            low_innov: vec![0.0; frame_size],
            high: vec![0.0; frame_size],
            xx1: vec![0.0; QMF_ORDER / 2 + frame_size],
            xx2: vec![0.0; QMF_ORDER / 2 + frame_size],
        }
    }

    /// Recombines the low band `out[..frame_size]` and the high band into `out`.
    fn synthesize(&mut self, out: &mut [f32]) {
        qmf_synth(
            &self.high,
            &tables::QMF_H0,
            out,
            &mut self.g0_mem,
            &mut self.g1_mem,
            &mut self.xx1,
            &mut self.xx2,
        );
    }

    /// Synthesises a lost frame, or the null sub-mode of a stream in discontinuous transmission:
    /// noise at the last high-band energy, which fades out while frames are lost
    /// (`sb_decode_lost`). The low band is already in `out[..frame_size]`.
    fn conceal(&mut self, dtx: bool, out: &mut [f32]) {
        if !dtx {
            let qlpc = self.interp_qlpc;
            bw_lpc(0.99, &qlpc, &mut self.interp_qlpc);
            self.last_ener *= 0.9f64 as f32;
        }

        self.first = true;
        for h in self.high.iter_mut() {
            *h = speex_rand(self.last_ener, &mut self.seed);
        }
        iir_mem(&mut self.high, &self.interp_qlpc, &mut self.mem_sp);
        self.synthesize(out);
    }

    /// Conceals a lost frame of `2 * frame_size` samples (`sb_decode` without a bit-stream).
    pub fn decode_lost(&mut self, out: &mut [f32]) {
        let fs = self.frame_size;
        let out = &mut out[..2 * fs];

        self.low.decode_lost(&mut out[..fs]);
        let dtx = self.low.dtx_enabled();
        self.conceal(dtx, out);
    }

    /// Decodes one frame of `2 * frame_size` samples to `out` (`sb_decode`).
    pub fn decode(
        &mut self,
        bits: &mut Bits<'_>,
        out: &mut [f32],
        mut innov_save: Option<&mut [f32]>,
        stereo: Option<&mut StereoState>,
    ) -> Result<(), Status> {
        let fs = self.frame_size;
        let out = &mut out[..2 * fs];

        // Decode the low band.
        let ret = self.low.decode(bits, &mut out[..fs], Some(&mut self.low_innov), stereo);
        let dtx = self.low.dtx_enabled();
        ret?;

        // Check the "wideband bit".
        let wideband = if bits.remaining() > 0 { bits.peek() } else { 0 };
        let submode_id = if wideband != 0 {
            bits.unpack(1);
            bits.unpack(3) as usize
        }
        else {
            // A narrow-band frame: the "null submode".
            0
        };

        let submodes = self.submodes;
        let Some(submode) = &submodes[submode_id]
        else {
            if submode_id != 0 {
                return Err(Status::Corrupt);
            }

            // Null submode: no high band was transmitted.
            if dtx {
                // Discontinuous transmission: comfort noise at the last energy.
                self.conceal(true, out);
                return Ok(());
            }

            self.first = true;
            self.high.fill(VERY_SMALL);
            iir_mem(&mut self.high, &self.interp_qlpc, &mut self.mem_sp);
            self.synthesize(out);
            return Ok(());
        };

        let low_pi_gain = self.low.pi_gain();
        let low_exc_rms = self.low.exc_rms();

        let mut qlsp = [0f32; SB_LPC_SIZE];
        lsp_unquant_high(&mut qlsp, bits);

        if self.first {
            self.old_qlsp = qlsp;
        }

        let ss = self.subframe_size;
        let mut exc = [0f32; 80];
        let mut innov2 = [0f32; 80];
        let mut interp_qlsp = [0f32; SB_LPC_SIZE];
        let mut ak = [0f32; SB_LPC_SIZE];
        let mut exc_ener_sum = 0f32;

        for sub in 0..SB_NB_SUBFRAMES {
            let offset = ss * sub;
            let exc = &mut exc[..ss];

            if let Some(save) = innov_save.as_deref_mut() {
                save[2 * offset..2 * (offset + ss)].fill(0.0);
            }

            lsp_interpolate(&self.old_qlsp, &qlsp, &mut interp_qlsp, sub, SB_NB_SUBFRAMES, 0.05);
            lsp_to_lpc(&interp_qlsp, &mut ak);

            // The response ratio between the low and high filter in the middle of the band
            // (4000 Hz).
            self.pi_gain[sub] = 1.0;
            let mut rh = 1.0f32;
            for i in (0..SB_LPC_SIZE).step_by(2) {
                rh += ak[i + 1] - ak[i];
                self.pi_gain[sub] += ak[i] + ak[i + 1];
            }
            let rl = low_pi_gain[sub];
            let filter_ratio = ((f64::from(rl) + 0.01) / (f64::from(rh) + 0.01)) as f32;

            exc.fill(0.0);
            match &submode.innovation {
                None => {
                    // Fold the low-band innovation into the high band.
                    let quant = bits.unpack(5);
                    let mut g = f64::from(0.125f32 * (quant as i32 - 10) as f32).exp() as f32;
                    g /= filter_ratio;

                    let low = &self.low_innov[offset..offset + ss];
                    for i in (0..ss).step_by(2) {
                        exc[i] = (self.folding_gain * low[i]) * g;
                        exc[i + 1] = -((self.folding_gain * low[i + 1]) * g);
                    }
                }
                Some(cb) => {
                    let qgc = bits.unpack(4) as usize;
                    let el = low_exc_rms[sub];
                    let mut gc = (0.87360 * f64::from(tables::GC_QUANT_BOUND[qgc])) as f32;
                    if ss == 80 {
                        #[allow(clippy::approx_constant)]
                        let sqrt2 = 1.4142f32;
                        gc *= sqrt2;
                    }

                    let scale = (gc * el) / filter_ratio;
                    cb.unquant(exc, bits);
                    for e in exc.iter_mut() {
                        *e *= scale;
                    }

                    if submode.double_codebook {
                        let innov2 = &mut innov2[..ss];
                        innov2.fill(0.0);
                        cb.unquant(innov2, bits);
                        let scale2 = 0.4f32 * scale;
                        for (e, &v) in exc.iter_mut().zip(innov2.iter()) {
                            *e += v * scale2;
                        }
                    }
                }
            }

            if let Some(save) = innov_save.as_deref_mut() {
                for (i, &e) in exc.iter().enumerate() {
                    save[2 * (offset + i)] = e;
                }
            }

            // The reference synthesises the excitation of the *previous* sub-frame with the
            // filter of the previous sub-frame, and this is reproduced exactly.
            let sp = &mut self.high[offset..offset + ss];
            sp.copy_from_slice(&self.exc_buf);
            iir_mem(sp, &self.interp_qlpc, &mut self.mem_sp);

            self.exc_buf.copy_from_slice(exc);
            self.interp_qlpc = ak;

            self.exc_rms[sub] = compute_rms(&self.exc_buf);
            exc_ener_sum += (self.exc_rms[sub] * self.exc_rms[sub]) / SB_NB_SUBFRAMES as f32;
        }

        self.last_ener = f64::from(exc_ener_sum).sqrt() as f32;

        self.synthesize(out);
        self.old_qlsp = qlsp;
        self.first = false;

        Ok(())
    }
}
