// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per sample-rate / bit-rate mode descriptions, from FFmpeg `libavcodec/twinvqdec.c` and
//! `libavcodec/twinvq.h` (`TwinVQModeTab`, `TwinVQFrameMode`).

use crate::tables::*;

/// Frame types. The discriminants index the per-frame-type arrays.
pub(crate) const FT_SHORT: usize = 0;
pub(crate) const FT_MEDIUM: usize = 1;
pub(crate) const FT_LONG: usize = 2;
/// Periodic peak component; only used as an index for the bitstream parameter tables.
pub(crate) const FT_PPC: usize = 3;

/// Parameters that depend on the frame type.
pub(crate) struct FrameMode {
    /// Number of sub-blocks in a frame.
    pub sub: usize,
    pub bark_tab: &'static [u16],
    /// Number of distinct bark scale envelope values.
    pub bark_env_size: usize,
    pub bark_cb: &'static [i16],
    /// Number of bark scale envelope codebook coefficients to read.
    pub bark_n_coef: usize,
    /// Number of bits of each bark scale envelope coefficient.
    pub bark_n_bit: u32,
    pub cb0: &'static [i16],
    pub cb1: &'static [i16],
    /// Number of spectrum coefficients per codebook vector.
    pub cb_len_read: usize,
}

/// Parameters that depend on the sample-rate / bit-rate combination.
pub(crate) struct ModeTab {
    /// Short, medium and long frame parameters.
    pub fmode: [FrameMode; 3],
    /// Frame size in samples.
    pub size: usize,
    /// Number of LSP coefficients.
    pub n_lsp: usize,
    pub lsp_codebook: &'static [f32],
    pub lsp_bit0: u32,
    pub lsp_bit1: u32,
    pub lsp_bit2: u32,
    /// Number of codebook entries for the LSP decoding.
    pub lsp_split: usize,
    pub ppc_shape_cb: &'static [i16],
    /// Number of bits of the PPC period value.
    pub ppc_period_bit: u32,
    /// Number of bits of the PPC shape codebook coefficients.
    pub ppc_shape_bit: u32,
    /// Size of the PPC shape codebook.
    pub ppc_shape_len: usize,
    /// Number of bits of the PPC gain.
    pub pgain_bit: u32,
    /// Constant for the peak period to peak width conversion.
    pub peak_per2wid: i32,
}

#[allow(clippy::too_many_arguments)]
const fn fm(
    sub: usize,
    bark_tab: &'static [u16],
    bark_env_size: usize,
    bark_cb: &'static [i16],
    bark_n_coef: usize,
    bark_n_bit: u32,
    cb0: &'static [i16],
    cb1: &'static [i16],
    cb_len_read: usize,
) -> FrameMode {
    FrameMode {
        sub,
        bark_tab,
        bark_env_size,
        bark_cb,
        bark_n_coef,
        bark_n_bit,
        cb0,
        cb1,
        cb_len_read,
    }
}

#[allow(clippy::too_many_arguments)]
const fn mode(
    fmode: [FrameMode; 3],
    size: usize,
    n_lsp: usize,
    lsp_codebook: &'static [f32],
    lsp_bits: [u32; 3],
    lsp_split: usize,
    ppc_shape_cb: &'static [i16],
    ppc_period_bit: u32,
    ppc_shape_bit: u32,
    ppc_shape_len: usize,
    pgain_bit: u32,
    peak_per2wid: i32,
) -> ModeTab {
    ModeTab {
        fmode,
        size,
        n_lsp,
        lsp_codebook,
        lsp_bit0: lsp_bits[0],
        lsp_bit1: lsp_bits[1],
        lsp_bit2: lsp_bits[2],
        lsp_split,
        ppc_shape_cb,
        ppc_period_bit,
        ppc_shape_bit,
        ppc_shape_len,
        pgain_bit,
        peak_per2wid,
    }
}

static MODE_08_08: ModeTab = mode(
    [
        fm(8, &BARK_TAB_S08_64, 10, &FCB08S, 1, 5, &CB0808S0, &CB0808S1, 18),
        fm(2, &BARK_TAB_M08_256, 20, &FCB08M, 2, 5, &CB0808M0, &CB0808M1, 16),
        fm(1, &BARK_TAB_L08_512, 30, &FCB08L, 3, 6, &CB0808L0, &CB0808L1, 17),
    ],
    512,
    12,
    &METASOUND_LSP8,
    [1, 5, 3],
    3,
    &SHAPE08,
    8,
    28,
    20,
    6,
    40,
);

static MODE_11_08: ModeTab = mode(
    [
        fm(8, &BARK_TAB_S11_64, 10, &FCB11S, 1, 5, &CB1108S0, &CB1108S1, 29),
        fm(2, &BARK_TAB_M11_256, 20, &FCB11M, 2, 5, &CB1108M0, &CB1108M1, 24),
        fm(1, &BARK_TAB_L11_512, 30, &FCB11L, 3, 6, &CB1108L0, &CB1108L1, 27),
    ],
    512,
    16,
    &METASOUND_LSP11,
    [1, 6, 4],
    3,
    &SHAPE11,
    9,
    36,
    30,
    7,
    90,
);

static MODE_11_10: ModeTab = mode(
    [
        fm(8, &BARK_TAB_S11_64, 10, &FCB11S, 1, 5, &CB1110S0, &CB1110S1, 21),
        fm(2, &BARK_TAB_M11_256, 20, &FCB11M, 2, 5, &CB1110M0, &CB1110M1, 18),
        fm(1, &BARK_TAB_L11_512, 30, &FCB11L, 3, 6, &CB1110L0, &CB1110L1, 20),
    ],
    512,
    16,
    &METASOUND_LSP11,
    [1, 6, 4],
    3,
    &SHAPE11,
    9,
    36,
    30,
    7,
    90,
);

static MODE_16_16: ModeTab = mode(
    [
        fm(8, &BARK_TAB_S16_128, 10, &FCB16S, 1, 5, &CB1616S0, &CB1616S1, 16),
        fm(2, &BARK_TAB_M16_512, 20, &FCB16M, 2, 5, &CB1616M0, &CB1616M1, 15),
        fm(1, &BARK_TAB_L16_1024, 30, &FCB16L, 3, 6, &CB1616L0, &CB1616L1, 16),
    ],
    1024,
    16,
    &METASOUND_LSP16,
    [1, 6, 4],
    3,
    &SHAPE16,
    9,
    56,
    60,
    7,
    180,
);

static MODE_22_20: ModeTab = mode(
    [
        fm(8, &BARK_TAB_S22_128, 10, &FCB22S_1, 1, 6, &CB2220S0, &CB2220S1, 18),
        fm(2, &BARK_TAB_M22_512, 20, &FCB22M_1, 2, 6, &CB2220M0, &CB2220M1, 17),
        fm(1, &BARK_TAB_L22_1024, 32, &FCB22L_1, 4, 6, &CB2220L0, &CB2220L1, 18),
    ],
    1024,
    16,
    &METASOUND_LSP22,
    [1, 6, 4],
    3,
    &SHAPE22_1,
    9,
    56,
    36,
    7,
    144,
);

static MODE_22_24: ModeTab = mode(
    [
        fm(8, &BARK_TAB_S22_128, 10, &FCB22S_1, 1, 6, &CB2224S0, &CB2224S1, 15),
        fm(2, &BARK_TAB_M22_512, 20, &FCB22M_1, 2, 6, &CB2224M0, &CB2224M1, 14),
        fm(1, &BARK_TAB_L22_1024, 32, &FCB22L_1, 4, 6, &CB2224L0, &CB2224L1, 15),
    ],
    1024,
    16,
    &METASOUND_LSP22,
    [1, 6, 4],
    3,
    &SHAPE22_1,
    9,
    56,
    36,
    7,
    144,
);

static MODE_22_32: ModeTab = mode(
    [
        fm(4, &BARK_TAB_S22_128, 10, &FCB22S_2, 1, 6, &CB2232S0, &CB2232S1, 11),
        fm(2, &BARK_TAB_M22_256, 20, &FCB22M_2, 2, 6, &CB2232M0, &CB2232M1, 11),
        fm(1, &BARK_TAB_L22_512, 32, &FCB22L_2, 4, 6, &CB2232L0, &CB2232L1, 12),
    ],
    512,
    16,
    &LSP22_2,
    [1, 6, 4],
    4,
    &SHAPE22_2,
    9,
    56,
    36,
    7,
    72,
);

static MODE_44_40: ModeTab = mode(
    [
        fm(16, &BARK_TAB_S44_128, 10, &FCB44S, 1, 6, &CB4440S0, &CB4440S1, 18),
        fm(4, &BARK_TAB_M44_512, 20, &FCB44M, 2, 6, &CB4440M0, &CB4440M1, 17),
        fm(1, &BARK_TAB_L44_2048, 40, &FCB44L, 4, 6, &CB4440L0, &CB4440L1, 17),
    ],
    2048,
    20,
    &METASOUND_LSP44,
    [1, 6, 4],
    4,
    &SHAPE44,
    9,
    84,
    54,
    7,
    432,
);

static MODE_44_48: ModeTab = mode(
    [
        fm(16, &BARK_TAB_S44_128, 10, &FCB44S, 1, 6, &CB4448S0, &CB4448S1, 15),
        fm(4, &BARK_TAB_M44_512, 20, &FCB44M, 2, 6, &CB4448M0, &CB4448M1, 14),
        fm(1, &BARK_TAB_L44_2048, 40, &FCB44L, 4, 6, &CB4448L0, &CB4448L1, 14),
    ],
    2048,
    20,
    &METASOUND_LSP44,
    [1, 6, 4],
    4,
    &SHAPE44,
    9,
    84,
    54,
    7,
    432,
);

/// Every supported mode.
#[cfg(test)]
pub(crate) static ALL_MODES: [&ModeTab; 9] = [
    &MODE_08_08,
    &MODE_11_08,
    &MODE_11_10,
    &MODE_16_16,
    &MODE_22_20,
    &MODE_22_24,
    &MODE_22_32,
    &MODE_44_40,
    &MODE_44_48,
];

/// Selects the mode for a sample rate flag (in kHz, 8..=44) and a bit rate per channel (kbit/s).
pub(crate) fn select_mode(isampf: u32, ibps: u32) -> Option<&'static ModeTab> {
    // FFmpeg libavcodec/twinvqdec.c, twinvq_decode_init().
    Some(match (isampf, ibps) {
        (8, 8) => &MODE_08_08,
        (11, 8) => &MODE_11_08,
        (11, 10) => &MODE_11_10,
        (16, 16) => &MODE_16_16,
        (22, 20) => &MODE_22_20,
        (22, 24) => &MODE_22_24,
        (22, 32) => &MODE_22_32,
        (44, 40) => &MODE_44_40,
        (44, 48) => &MODE_44_48,
        _ => return None,
    })
}

/// A `very_broken_op` rounding table: `(vector length, flattened table)`.
pub(crate) fn rounding_table(idx: usize) -> Option<(usize, &'static [u8])> {
    // FFmpeg `tabs[]`, indexed by `b / 5`.
    Some(match idx {
        1 | 2 | 4 | 8 => (5, &TAB8),
        3 | 6 | 12 => (15, &TAB12),
        5 | 10 => (25, &TAB10),
        7 => (35, &TAB7),
        9 => (45, &TAB9),
        11 => (55, &TAB11),
        _ => return None,
    })
}
