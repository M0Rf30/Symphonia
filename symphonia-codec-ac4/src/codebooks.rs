// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Huffman codebooks, built once on first use (FFmpeg's `INIT_VLC_STATIC` tables, see `NOTICE`).

use std::sync::LazyLock;

use crate::tables::*;
use crate::vlc::Vlc;

/// All Huffman codebooks of the AC-4 bitstream.
pub(crate) struct Codebooks {
    pub(crate) channel_mode: Vlc,
    pub(crate) bitrate_indicator: Vlc,
    pub(crate) scale_factors: Vlc,
    pub(crate) snf: Vlc,
    /// `asf[cb - 1]` for spectral codebooks 1..=11.
    pub(crate) asf: Box<[Vlc; 11]>,
    pub(crate) aspx_int_class: Vlc,
    /// `aspx_signal[stereo_mode][quant_mode][hcb_type]` (`hcb_type`: F0, DF, DT).
    pub(crate) aspx_signal: Box<[[[Vlc; 3]; 2]; 2]>,
    /// `aspx_noise[stereo_mode][hcb_type]`.
    pub(crate) aspx_noise: Box<[[Vlc; 3]; 2]>,
    /// `acpl[data_type_class][quant_mode][hcb_type]`: class 0 = alpha, 1 = beta, 2 = beta3,
    /// 3 = gamma; `quant_mode` 0 = fine, 1 = coarse.
    pub(crate) acpl: Box<[[[Vlc; 3]; 2]; 4]>,
}

static CODEBOOKS: LazyLock<Codebooks> = LazyLock::new(build);

/// Returns the shared codebooks.
pub(crate) fn codebooks() -> &'static Codebooks {
    &CODEBOOKS
}

fn build() -> Codebooks {
    let asf = Box::new([
        Vlc::new(&ASF_CODEBOOK_1_BITS, &ASF_CODEBOOK_1_CODES),
        Vlc::new(&ASF_CODEBOOK_2_BITS, &ASF_CODEBOOK_2_CODES),
        Vlc::new(&ASF_CODEBOOK_3_BITS, &ASF_CODEBOOK_3_CODES),
        Vlc::new(&ASF_CODEBOOK_4_BITS, &ASF_CODEBOOK_4_CODES),
        Vlc::new(&ASF_CODEBOOK_5_BITS, &ASF_CODEBOOK_5_CODES),
        Vlc::new(&ASF_CODEBOOK_6_BITS, &ASF_CODEBOOK_6_CODES),
        Vlc::new(&ASF_CODEBOOK_7_BITS, &ASF_CODEBOOK_7_CODES),
        Vlc::new(&ASF_CODEBOOK_8_BITS, &ASF_CODEBOOK_8_CODES),
        Vlc::new(&ASF_CODEBOOK_9_BITS, &ASF_CODEBOOK_9_CODES),
        Vlc::new(&ASF_CODEBOOK_10_BITS, &ASF_CODEBOOK_10_CODES),
        Vlc::new(&ASF_CODEBOOK_11_BITS, &ASF_CODEBOOK_11_CODES),
    ]);

    let aspx_signal = Box::new([
        [
            [
                Vlc::new(&ASPX_HCB_ENV_LEVEL_15_F0_BITS, &ASPX_HCB_ENV_LEVEL_15_F0_CODES),
                Vlc::new(&ASPX_HCB_ENV_LEVEL_15_DF_BITS, &ASPX_HCB_ENV_LEVEL_15_DF_CODES),
                Vlc::new(&ASPX_HCB_ENV_LEVEL_15_DT_BITS, &ASPX_HCB_ENV_LEVEL_15_DT_CODES),
            ],
            [
                Vlc::new(&ASPX_HCB_ENV_LEVEL_30_F0_BITS, &ASPX_HCB_ENV_LEVEL_30_F0_CODES),
                Vlc::new(&ASPX_HCB_ENV_LEVEL_30_DF_BITS, &ASPX_HCB_ENV_LEVEL_30_DF_CODES),
                Vlc::new(&ASPX_HCB_ENV_LEVEL_30_DT_BITS, &ASPX_HCB_ENV_LEVEL_30_DT_CODES),
            ],
        ],
        [
            [
                Vlc::new(&ASPX_HCB_ENV_BALANCE_15_F0_BITS, &ASPX_HCB_ENV_BALANCE_15_F0_CODES),
                Vlc::new(&ASPX_HCB_ENV_BALANCE_15_DF_BITS, &ASPX_HCB_ENV_BALANCE_15_DF_CODES),
                Vlc::new(&ASPX_HCB_ENV_BALANCE_15_DT_BITS, &ASPX_HCB_ENV_BALANCE_15_DT_CODES),
            ],
            [
                Vlc::new(&ASPX_HCB_ENV_BALANCE_30_F0_BITS, &ASPX_HCB_ENV_BALANCE_30_F0_CODES),
                Vlc::new(&ASPX_HCB_ENV_BALANCE_30_DF_BITS, &ASPX_HCB_ENV_BALANCE_30_DF_CODES),
                Vlc::new(&ASPX_HCB_ENV_BALANCE_30_DT_BITS, &ASPX_HCB_ENV_BALANCE_30_DT_CODES),
            ],
        ],
    ]);

    let aspx_noise = Box::new([
        [
            Vlc::new(&ASPX_HCB_NOISE_LEVEL_F0_BITS, &ASPX_HCB_NOISE_LEVEL_F0_CODES),
            Vlc::new(&ASPX_HCB_NOISE_LEVEL_DF_BITS, &ASPX_HCB_NOISE_LEVEL_DF_CODES),
            Vlc::new(&ASPX_HCB_NOISE_LEVEL_DT_BITS, &ASPX_HCB_NOISE_LEVEL_DT_CODES),
        ],
        [
            Vlc::new(&ASPX_HCB_NOISE_BALANCE_F0_BITS, &ASPX_HCB_NOISE_BALANCE_F0_CODES),
            Vlc::new(&ASPX_HCB_NOISE_BALANCE_DF_BITS, &ASPX_HCB_NOISE_BALANCE_DF_CODES),
            Vlc::new(&ASPX_HCB_NOISE_BALANCE_DT_BITS, &ASPX_HCB_NOISE_BALANCE_DT_CODES),
        ],
    ]);

    let acpl = Box::new([
        [
            [
                Vlc::new(&ACPL_HCB_ALPHA_FINE_F0_BITS, &ACPL_HCB_ALPHA_FINE_F0_CODES),
                Vlc::new(&ACPL_HCB_ALPHA_FINE_DF_BITS, &ACPL_HCB_ALPHA_FINE_DF_CODES),
                Vlc::new(&ACPL_HCB_ALPHA_FINE_DT_BITS, &ACPL_HCB_ALPHA_FINE_DT_CODES),
            ],
            [
                Vlc::new(&ACPL_HCB_ALPHA_COARSE_F0_BITS, &ACPL_HCB_ALPHA_COARSE_F0_CODES),
                Vlc::new(&ACPL_HCB_ALPHA_COARSE_DF_BITS, &ACPL_HCB_ALPHA_COARSE_DF_CODES),
                Vlc::new(&ACPL_HCB_ALPHA_COARSE_DT_BITS, &ACPL_HCB_ALPHA_COARSE_DT_CODES),
            ],
        ],
        [
            [
                Vlc::new(&ACPL_HCB_BETA_FINE_F0_BITS, &ACPL_HCB_BETA_FINE_F0_CODES),
                Vlc::new(&ACPL_HCB_BETA_FINE_DF_BITS, &ACPL_HCB_BETA_FINE_DF_CODES),
                Vlc::new(&ACPL_HCB_BETA_FINE_DT_BITS, &ACPL_HCB_BETA_FINE_DT_CODES),
            ],
            [
                Vlc::new(&ACPL_HCB_BETA_COARSE_F0_BITS, &ACPL_HCB_BETA_COARSE_F0_CODES),
                Vlc::new(&ACPL_HCB_BETA_COARSE_DF_BITS, &ACPL_HCB_BETA_COARSE_DF_CODES),
                Vlc::new(&ACPL_HCB_BETA_COARSE_DT_BITS, &ACPL_HCB_BETA_COARSE_DT_CODES),
            ],
        ],
        [
            [
                Vlc::new(&ACPL_HCB_BETA3_FINE_F0_BITS, &ACPL_HCB_BETA3_FINE_F0_CODES),
                Vlc::new(&ACPL_HCB_BETA3_FINE_DF_BITS, &ACPL_HCB_BETA3_FINE_DF_CODES),
                Vlc::new(&ACPL_HCB_BETA3_FINE_DT_BITS, &ACPL_HCB_BETA3_FINE_DT_CODES),
            ],
            [
                Vlc::new(&ACPL_HCB_BETA3_COARSE_F0_BITS, &ACPL_HCB_BETA3_COARSE_F0_CODES),
                Vlc::new(&ACPL_HCB_BETA3_COARSE_DF_BITS, &ACPL_HCB_BETA3_COARSE_DF_CODES),
                Vlc::new(&ACPL_HCB_BETA3_COARSE_DT_BITS, &ACPL_HCB_BETA3_COARSE_DT_CODES),
            ],
        ],
        [
            [
                Vlc::new(&ACPL_HCB_GAMMA_FINE_F0_BITS, &ACPL_HCB_GAMMA_FINE_F0_CODES),
                Vlc::new(&ACPL_HCB_GAMMA_FINE_DF_BITS, &ACPL_HCB_GAMMA_FINE_DF_CODES),
                Vlc::new(&ACPL_HCB_GAMMA_FINE_DT_BITS, &ACPL_HCB_GAMMA_FINE_DT_CODES),
            ],
            [
                Vlc::new(&ACPL_HCB_GAMMA_COARSE_F0_BITS, &ACPL_HCB_GAMMA_COARSE_F0_CODES),
                Vlc::new(&ACPL_HCB_GAMMA_COARSE_DF_BITS, &ACPL_HCB_GAMMA_COARSE_DF_CODES),
                Vlc::new(&ACPL_HCB_GAMMA_COARSE_DT_BITS, &ACPL_HCB_GAMMA_COARSE_DT_CODES),
            ],
        ],
    ]);

    Codebooks {
        acpl,
        channel_mode: Vlc::new(&CHANNEL_MODE_BITS, &CHANNEL_MODE_CODES),
        bitrate_indicator: Vlc::new(&BITRATE_INDICATOR_BITS, &BITRATE_INDICATOR_CODES),
        scale_factors: Vlc::new(&SCALE_FACTORS_BITS, &SCALE_FACTORS_CODES),
        snf: Vlc::new(&SNF_BITS, &SNF_CODES),
        asf,
        aspx_int_class: Vlc::new(&ASPX_INT_CLASS_BITS, &ASPX_INT_CLASS_CODES),
        aspx_signal,
        aspx_noise,
    }
}
