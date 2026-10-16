// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Channel unit state and bitstream parsing.
//!
//! This follows FFmpeg's `libavcodec/atrac3plus.c` function by function.

use symphonia_core::errors::{Result, decode_error};

use crate::bits::{BitReader, Vlc};
use crate::codebooks::Codebooks;
use crate::tables::*;

/// Number of PQF subbands.
pub(crate) const SUBBANDS: usize = 16;
/// Number of samples per subband.
pub(crate) const SUBBAND_SAMPLES: usize = 128;
/// Number of samples per frame and channel.
pub(crate) const FRAME_SAMPLES: usize = SUBBANDS * SUBBAND_SAMPLES;
/// Power compensation level that disables the compensation.
pub(crate) const POWER_COMP_OFF: u8 = 15;
/// Maximum number of GHA sine waves per unit.
pub(crate) const MAX_WAVES: usize = 48;

/// Channel unit types, as coded in the bitstream.
pub(crate) const CH_UNIT_MONO: u32 = 0;
pub(crate) const CH_UNIT_STEREO: u32 = 1;
pub(crate) const CH_UNIT_EXTENSION: u32 = 2;
pub(crate) const CH_UNIT_TERMINATOR: u32 = 3;

/// Gain control parameters for one subband.
#[derive(Clone, Copy, Default)]
pub(crate) struct GainInfo {
    pub num_points: usize,
    /// Level at the corresponding control point.
    pub lev_code: [i32; 7],
    /// Location of the gain control points.
    pub loc_code: [i32; 7],
}

/// Amplitude envelope of a group of sine waves.
#[derive(Clone, Copy, Default)]
pub(crate) struct WaveEnvelope {
    pub has_start_point: bool,
    pub has_stop_point: bool,
    /// Start position expressed in n*4 samples.
    pub start_pos: i32,
    /// Stop position expressed in n*4 samples.
    pub stop_pos: i32,
}

/// Parameters of a group of sine waves.
#[derive(Clone, Copy, Default)]
pub(crate) struct WavesData {
    /// Pending envelope from the previous frame.
    pub pend_env: WaveEnvelope,
    /// Group envelope from the current frame.
    pub curr_env: WaveEnvelope,
    pub num_wavs: usize,
    /// Start index into the global tones table for this subband.
    pub start_index: usize,
}

/// Parameters of a single sine wave.
#[derive(Clone, Copy, Default)]
pub(crate) struct WaveParam {
    pub freq_index: i32,
    pub amp_sf: i32,
    pub phase_index: i32,
}

/// Per-unit sine wave parameters.
#[derive(Clone)]
pub(crate) struct WaveSynthParams {
    pub tones_present: bool,
    pub num_tone_bands: usize,
    pub tone_sharing: [u8; SUBBANDS],
    pub tone_master: [u8; SUBBANDS],
    pub invert_phase: [u8; SUBBANDS],
    pub tones_index: usize,
    pub waves: [WaveParam; MAX_WAVES],
}

impl Default for WaveSynthParams {
    fn default() -> Self {
        WaveSynthParams {
            tones_present: false,
            num_tone_bands: 0,
            tone_sharing: [0; SUBBANDS],
            tone_master: [0; SUBBANDS],
            invert_phase: [0; SUBBANDS],
            tones_index: 0,
            waves: [WaveParam::default(); MAX_WAVES],
        }
    }
}

/// Sound channel parameters.
#[derive(Clone)]
pub(crate) struct ChanParams {
    pub ch_num: usize,
    pub num_coded_vals: usize,
    pub fill_mode: u32,
    pub split_point: usize,
    pub table_type: usize,
    pub qu_wordlen: [i32; 32],
    pub qu_sf_idx: [i32; 32],
    pub qu_tab_idx: [i32; 32],
    /// Decoded (quantized) MDCT spectrum.
    pub spectrum: [i16; FRAME_SAMPLES],
    pub power_levs: [u8; 5],
    /// IMDCT window shape history for two frames, `0` = sine, `1` = steep.
    pub wnd_shape: [[u8; SUBBANDS]; 2],
    /// Gain control data history for two frames.
    pub gain_data: [[GainInfo; SUBBANDS]; 2],
    /// Tones data history for two frames.
    pub tones_info: [[WavesData; SUBBANDS]; 2],
}

impl ChanParams {
    fn new(ch_num: usize) -> Self {
        ChanParams {
            ch_num,
            num_coded_vals: 0,
            fill_mode: 0,
            split_point: 0,
            table_type: 0,
            qu_wordlen: [0; 32],
            qu_sf_idx: [0; 32],
            qu_tab_idx: [0; 32],
            spectrum: [0; FRAME_SAMPLES],
            power_levs: [POWER_COMP_OFF; 5],
            wnd_shape: [[0; SUBBANDS]; 2],
            gain_data: [[GainInfo::default(); SUBBANDS]; 2],
            tones_info: [[WavesData::default(); SUBBANDS]; 2],
        }
    }
}

/// Per-channel inverse PQF history.
pub(crate) struct IpqfState {
    pub buf1: [[f32; 8]; 24],
    pub buf2: [[f32; 8]; 24],
    pub pos: usize,
}

impl IpqfState {
    fn new() -> Self {
        IpqfState { buf1: [[0.0; 8]; 24], buf2: [[0.0; 8]; 24], pos: 0 }
    }
}

/// Channel unit: one coded channel (mono) or two jointly coded channels (stereo).
///
/// State that the bitstream overlaps between frames is kept twice. Index `cur` holds the frame
/// being decoded and `cur ^ 1` the previous frame; `cur` is flipped after each frame.
pub(crate) struct ChanUnit {
    pub unit_type: u32,
    pub num_quant_units: usize,
    pub num_subbands: usize,
    pub used_quant_units: usize,
    pub num_coded_subbands: usize,
    pub mute_flag: bool,
    pub use_full_table: bool,
    pub swap_channels: [u8; SUBBANDS],
    pub negate_coeffs: [u8; SUBBANDS],
    pub channels: [ChanParams; 2],
    pub wave_synth: [WaveSynthParams; 2],
    pub cur: usize,
    pub ipqf: [IpqfState; 2],
    /// Overlap buffers.
    pub prev_buf: [[f32; FRAME_SAMPLES]; 2],
}

impl ChanUnit {
    pub(crate) fn new() -> Box<ChanUnit> {
        Box::new(ChanUnit {
            unit_type: CH_UNIT_MONO,
            num_quant_units: 0,
            num_subbands: 0,
            used_quant_units: 0,
            num_coded_subbands: 0,
            mute_flag: false,
            use_full_table: false,
            swap_channels: [0; SUBBANDS],
            negate_coeffs: [0; SUBBANDS],
            channels: [ChanParams::new(0), ChanParams::new(1)],
            wave_synth: [WaveSynthParams::default(), WaveSynthParams::default()],
            cur: 0,
            ipqf: [IpqfState::new(), IpqfState::new()],
            prev_buf: [[0.0; FRAME_SAMPLES]; 2],
        })
    }
}

#[inline]
fn sign_extend(val: u32, bits: u32) -> i32 {
    ((val << (32 - bits)) as i32) >> (32 - bits)
}

/// Floor of the base-2 logarithm, `0` for `0` (FFmpeg's `av_log2`).
#[inline]
fn log2(v: u32) -> u32 {
    31 - (v | 1).leading_zeros()
}

/// Decode a Huffman symbol as a signed value.
#[inline]
fn vlc(gb: &mut BitReader<'_>, tab: &Vlc) -> Result<i32> {
    Ok(tab.decode(gb)? as i32)
}

/// Decode the number of coded quantization units.
fn num_coded_units(
    gb: &mut BitReader<'_>,
    chan: &mut ChanParams,
    num_quant_units: usize,
) -> Result<()> {
    chan.fill_mode = gb.get(2);
    if chan.fill_mode == 0 {
        chan.num_coded_vals = num_quant_units;
    }
    else {
        chan.num_coded_vals = gb.get(5) as usize;
        if chan.num_coded_vals > num_quant_units {
            return decode_error("atrac3plus: invalid number of transmitted units");
        }
        if chan.fill_mode == 3 {
            chan.split_point = gb.get(2) as usize + (chan.ch_num << 1) + 1;
        }
    }
    Ok(())
}

/// Add weighting coefficients to the decoded word-length information.
fn add_wordlen_weights(
    chan: &mut ChanParams,
    num_quant_units: usize,
    wtab_idx: usize,
) -> Result<()> {
    let weights = &WL_WEIGHTS[chan.ch_num * 3 + wtab_idx - 1];
    for (wl, &w) in chan.qu_wordlen[..num_quant_units].iter_mut().zip(weights) {
        *wl += i32::from(w);
        if !(0..=7).contains(wl) {
            return decode_error("atrac3plus: word length index out of range");
        }
    }
    Ok(())
}

/// Subtract weighting coefficients from decoded scale factors.
fn subtract_sf_weights(
    chan: &mut ChanParams,
    used_quant_units: usize,
    wtab_idx: usize,
) -> Result<()> {
    let weights = &SF_WEIGHTS[wtab_idx - 1];
    for (sf, &w) in chan.qu_sf_idx[..used_quant_units].iter_mut().zip(weights) {
        *sf -= i32::from(w);
        if !(0..=63).contains(sf) {
            return decode_error("atrac3plus: scale factor index out of range");
        }
    }
    Ok(())
}

/// Unpack a vector quantization shape.
fn unpack_vq_shape(start_val: i32, shape: &[i8; 9], dst: &mut [i32; 32], num_values: usize) {
    if num_values > 0 {
        dst[0] = start_val;
        dst[1] = start_val;
        dst[2] = start_val;
        for i in 3..num_values {
            dst[i] = start_val - i32::from(shape[usize::from(QU_NUM_TO_SEG[i]) - 1]);
        }
    }
}

/// Read a scale factor VQ shape: a 6-bit start value and a 6-bit shape index.
fn unpack_sf_vq_shape(gb: &mut BitReader<'_>, dst: &mut [i32; 32], num_values: usize) {
    let start_val = gb.get(6) as i32;
    let shape = &SF_SHAPES[gb.get(6) as usize];
    unpack_vq_shape(start_val, shape, dst, num_values);
}

/// Decode the word length for each quantization unit of a channel.
fn decode_channel_wordlen(
    gb: &mut BitReader<'_>,
    cb: &Codebooks,
    unit: &mut ChanUnit,
    ch_num: usize,
) -> Result<()> {
    let nqu = unit.num_quant_units;
    let ref_wl = unit.channels[0].qu_wordlen;
    let chan = &mut unit.channels[ch_num];
    let mut weight_idx = 0;

    chan.fill_mode = 0;

    match gb.get(2) {
        // Coded using a constant number of bits.
        0 => {
            for wl in chan.qu_wordlen[..nqu].iter_mut() {
                *wl = gb.get(3) as i32;
            }
        }
        1 => {
            if ch_num != 0 {
                num_coded_units(gb, chan, nqu)?;
                if chan.num_coded_vals != 0 {
                    let tab = &cb.wl[gb.get(2) as usize];
                    for i in 0..chan.num_coded_vals {
                        let delta = vlc(gb, tab)?;
                        chan.qu_wordlen[i] = (ref_wl[i] + delta) & 7;
                    }
                }
            }
            else {
                weight_idx = gb.get(2) as usize;
                num_coded_units(gb, chan, nqu)?;
                if chan.num_coded_vals != 0 {
                    let pos = gb.get(5) as usize;
                    if pos > chan.num_coded_vals {
                        return decode_error("atrac3plus: word length mode 1: invalid position");
                    }
                    let delta_bits = gb.get(2);
                    let min_val = gb.get(3) as i32;

                    for i in 0..pos {
                        chan.qu_wordlen[i] = gb.get(3) as i32;
                    }
                    for i in pos..chan.num_coded_vals {
                        chan.qu_wordlen[i] = (min_val + gb.get(delta_bits) as i32) & 7;
                    }
                }
            }
        }
        2 => {
            num_coded_units(gb, chan, nqu)?;

            if ch_num != 0 && chan.num_coded_vals != 0 {
                let tab = &cb.wl[gb.get(2) as usize];
                let delta = vlc(gb, tab)?;
                chan.qu_wordlen[0] = (ref_wl[0] + delta) & 7;

                for i in 1..chan.num_coded_vals {
                    let diff = ref_wl[i] - ref_wl[i - 1];
                    let delta = vlc(gb, tab)?;
                    chan.qu_wordlen[i] = (chan.qu_wordlen[i - 1] + diff + delta) & 7;
                }
            }
            else if chan.num_coded_vals != 0 {
                let flag = gb.get1();
                let tab = &cb.wl[gb.get(1) as usize];

                let start_val = gb.get(3) as i32;
                let shape = &WL_SHAPES[start_val as usize][gb.get(4) as usize];
                unpack_vq_shape(start_val, shape, &mut chan.qu_wordlen, chan.num_coded_vals);

                if !flag {
                    for i in 0..chan.num_coded_vals {
                        let delta = vlc(gb, tab)?;
                        chan.qu_wordlen[i] = (chan.qu_wordlen[i] + delta) & 7;
                    }
                }
                else {
                    let even = chan.num_coded_vals & !1;
                    for i in (0..even).step_by(2) {
                        if !gb.get1() {
                            let delta = vlc(gb, tab)?;
                            chan.qu_wordlen[i] = (chan.qu_wordlen[i] + delta) & 7;
                            let delta = vlc(gb, tab)?;
                            chan.qu_wordlen[i + 1] = (chan.qu_wordlen[i + 1] + delta) & 7;
                        }
                    }
                    if chan.num_coded_vals & 1 != 0 {
                        let delta = vlc(gb, tab)?;
                        chan.qu_wordlen[even] = (chan.qu_wordlen[even] + delta) & 7;
                    }
                }
            }
        }
        _ => {
            weight_idx = gb.get(2) as usize;
            num_coded_units(gb, chan, nqu)?;

            if chan.num_coded_vals != 0 {
                let tab = &cb.wl[gb.get(2) as usize];

                // The first coefficient is coded directly.
                chan.qu_wordlen[0] = gb.get(3) as i32;

                for i in 1..chan.num_coded_vals {
                    let delta = vlc(gb, tab)?;
                    chan.qu_wordlen[i] = (chan.qu_wordlen[i - 1] + delta) & 7;
                }
            }
        }
    }

    if chan.fill_mode == 2 {
        for i in chan.num_coded_vals..nqu {
            chan.qu_wordlen[i] = if ch_num != 0 { i32::from(gb.get1()) } else { 1 };
        }
    }
    else if chan.fill_mode == 3 {
        let pos = if ch_num != 0 {
            chan.num_coded_vals + chan.split_point
        }
        else {
            nqu.saturating_sub(chan.split_point)
        };
        let pos = pos.min(chan.qu_wordlen.len());
        for i in chan.num_coded_vals..pos {
            chan.qu_wordlen[i] = 1;
        }
    }

    if weight_idx != 0 {
        return add_wordlen_weights(chan, nqu, weight_idx);
    }

    Ok(())
}

/// Decode the scale factor indexes for each quantization unit of a channel.
fn decode_channel_sf_idx(
    gb: &mut BitReader<'_>,
    cb: &Codebooks,
    unit: &mut ChanUnit,
    ch_num: usize,
) -> Result<()> {
    let used = unit.used_quant_units;
    let ref_sf = unit.channels[0].qu_sf_idx;
    let chan = &mut unit.channels[ch_num];
    let mut weight_idx = 0;

    match gb.get(2) {
        // Coded using a constant number of bits.
        0 => {
            for sf in chan.qu_sf_idx[..used].iter_mut() {
                *sf = gb.get(6) as i32;
            }
        }
        1 => {
            if ch_num != 0 {
                let tab = &cb.sf[gb.get(2) as usize];
                for i in 0..used {
                    let delta = vlc(gb, tab)?;
                    chan.qu_sf_idx[i] = (ref_sf[i] + delta) & 0x3f;
                }
            }
            else {
                weight_idx = gb.get(2) as usize;
                if weight_idx == 3 {
                    unpack_sf_vq_shape(gb, &mut chan.qu_sf_idx, used);

                    let num_long_vals = gb.get(5) as usize;
                    let delta_bits = gb.get(2);
                    let min_val = gb.get(4) as i32 - 7;

                    for i in 0..num_long_vals {
                        chan.qu_sf_idx[i] = (chan.qu_sf_idx[i] + gb.get(4) as i32 - 7) & 0x3f;
                    }

                    // All others are: min_val + delta.
                    for i in num_long_vals..used {
                        chan.qu_sf_idx[i] =
                            (chan.qu_sf_idx[i] + min_val + gb.get(delta_bits) as i32) & 0x3f;
                    }
                }
                else {
                    let num_long_vals = gb.get(5) as usize;
                    let delta_bits = gb.get(3);
                    let min_val = gb.get(6) as i32;
                    if num_long_vals > used || delta_bits == 7 {
                        return decode_error("atrac3plus: scale factor mode 1: invalid parameters");
                    }

                    // Read full-precision scale factor indexes.
                    for i in 0..num_long_vals {
                        chan.qu_sf_idx[i] = gb.get(6) as i32;
                    }

                    // All others are: min_val + delta.
                    for i in num_long_vals..used {
                        chan.qu_sf_idx[i] = (min_val + gb.get(delta_bits) as i32) & 0x3f;
                    }
                }
            }
        }
        2 => {
            if ch_num != 0 {
                let tab = &cb.sf[gb.get(2) as usize];

                let delta = vlc(gb, tab)?;
                chan.qu_sf_idx[0] = (ref_sf[0] + delta) & 0x3f;

                for i in 1..used {
                    let diff = ref_sf[i] - ref_sf[i - 1];
                    let delta = vlc(gb, tab)?;
                    chan.qu_sf_idx[i] = (chan.qu_sf_idx[i - 1] + diff + delta) & 0x3f;
                }
            }
            else {
                let tab = &cb.sf[gb.get(2) as usize + 4];

                unpack_sf_vq_shape(gb, &mut chan.qu_sf_idx, used);

                for i in 0..used {
                    let delta = tab.decode(gb)?;
                    chan.qu_sf_idx[i] = (chan.qu_sf_idx[i] + sign_extend(delta, 4)) & 0x3f;
                }
            }
        }
        _ => {
            if ch_num != 0 {
                // Copy coefficients from the reference channel.
                chan.qu_sf_idx[..used].copy_from_slice(&ref_sf[..used]);
            }
            else {
                weight_idx = gb.get(2) as usize;
                let vlc_sel = gb.get(2) as usize;

                if weight_idx == 3 {
                    let tab = &cb.sf[vlc_sel + 4];

                    unpack_sf_vq_shape(gb, &mut chan.qu_sf_idx, used);

                    let mut diff = (gb.get(4) as i32 + 56) & 0x3f;
                    chan.qu_sf_idx[0] = (chan.qu_sf_idx[0] + diff) & 0x3f;

                    for i in 1..used {
                        let delta = tab.decode(gb)?;
                        diff = (diff + sign_extend(delta, 4)) & 0x3f;
                        chan.qu_sf_idx[i] = (diff + chan.qu_sf_idx[i]) & 0x3f;
                    }
                }
                else {
                    let tab = &cb.sf[vlc_sel];

                    // The first coefficient is coded directly.
                    chan.qu_sf_idx[0] = gb.get(6) as i32;

                    for i in 1..used {
                        let delta = vlc(gb, tab)?;
                        chan.qu_sf_idx[i] = (chan.qu_sf_idx[i - 1] + delta) & 0x3f;
                    }
                }
            }
        }
    }

    if weight_idx != 0 && weight_idx < 3 {
        return subtract_sf_weights(chan, used, weight_idx);
    }

    Ok(())
}

/// Decode the word length information for each channel.
fn decode_quant_wordlen(
    gb: &mut BitReader<'_>,
    cb: &Codebooks,
    unit: &mut ChanUnit,
    num_channels: usize,
) -> Result<()> {
    for ch_num in 0..num_channels {
        unit.channels[ch_num].qu_wordlen = [0; 32];
        decode_channel_wordlen(gb, cb, unit, ch_num)?;
    }

    // Scan for the last non-zero coefficient in both channels and set the number of quant units
    // having a coded spectrum.
    let mut used = unit.num_quant_units;
    while used > 0 {
        let i = used - 1;
        if unit.channels[0].qu_wordlen[i] != 0
            || (num_channels == 2 && unit.channels[1].qu_wordlen[i] != 0)
        {
            break;
        }
        used -= 1;
    }
    unit.used_quant_units = used;

    Ok(())
}

/// Decode the scale factor indexes for each channel.
fn decode_scale_factors(
    gb: &mut BitReader<'_>,
    cb: &Codebooks,
    unit: &mut ChanUnit,
    num_channels: usize,
) -> Result<()> {
    if unit.used_quant_units == 0 {
        return Ok(());
    }

    for ch_num in 0..num_channels {
        unit.channels[ch_num].qu_sf_idx = [0; 32];
        decode_channel_sf_idx(gb, cb, unit, ch_num)?;
    }

    Ok(())
}

/// Decode the number of code table values.
fn get_num_ct_values(gb: &mut BitReader<'_>, used_quant_units: usize) -> Result<usize> {
    if gb.get1() {
        let n = gb.get(5) as usize;
        if n > used_quant_units {
            return decode_error("atrac3plus: invalid number of code table indexes");
        }
        Ok(n)
    }
    else {
        Ok(used_quant_units)
    }
}

/// How the code table index of a quantization unit is coded.
enum CtCoding<'a> {
    Direct(u32),
    Vlc(&'a Vlc),
    VlcDelta(&'a Vlc, &'a Vlc),
    VlcDiff(&'a Vlc),
}

/// Decode the code table indexes for each quant unit of a channel.
fn decode_channel_code_tab(
    gb: &mut BitReader<'_>,
    cb: &Codebooks,
    unit: &mut ChanUnit,
    ch_num: usize,
) -> Result<()> {
    let full = unit.use_full_table;
    let used = unit.used_quant_units;
    let mask = if full { 7 } else { 3 };
    let ref_wl = unit.channels[0].qu_wordlen;
    let ref_tab = unit.channels[0].qu_tab_idx;
    let chan = &mut unit.channels[ch_num];

    chan.table_type = gb.get(1) as usize;

    let coding = match gb.get(2) {
        // Directly coded.
        0 => CtCoding::Direct(u32::from(full) + 2),
        // Entropy-coded.
        1 => CtCoding::Vlc(if full { &cb.ct[1] } else { &cb.ct[0] }),
        // Entropy-coded delta.
        2 => {
            if full {
                CtCoding::VlcDelta(&cb.ct[1], &cb.ct[2])
            }
            else {
                CtCoding::VlcDelta(&cb.ct[0], &cb.ct[0])
            }
        }
        // Entropy-coded difference to the master channel.
        _ => {
            if ch_num == 0 {
                return Ok(());
            }
            CtCoding::VlcDiff(if full { &cb.ct[3] } else { &cb.ct[0] })
        }
    };

    let num_vals = get_num_ct_values(gb, used)?;
    let mut pred = 0;

    for i in 0..num_vals {
        if chan.qu_wordlen[i] != 0 {
            chan.qu_tab_idx[i] = match coding {
                CtCoding::Direct(num_bits) => gb.get(num_bits) as i32,
                CtCoding::Vlc(tab) => vlc(gb, tab)?,
                CtCoding::VlcDelta(first, delta) => {
                    let v = if i == 0 { vlc(gb, first)? } else { (pred + vlc(gb, delta)?) & mask };
                    pred = v;
                    v
                }
                CtCoding::VlcDiff(tab) => (ref_tab[i] + vlc(gb, tab)?) & mask,
            };
        }
        else if ch_num != 0 && ref_wl[i] != 0 {
            // Clone master flag.
            chan.qu_tab_idx[i] = i32::from(gb.get1());
        }
    }

    Ok(())
}

/// Decode the code table indexes for each channel.
fn decode_code_table_indexes(
    gb: &mut BitReader<'_>,
    cb: &Codebooks,
    unit: &mut ChanUnit,
    num_channels: usize,
) -> Result<()> {
    if unit.used_quant_units == 0 {
        return Ok(());
    }

    unit.use_full_table = gb.get1();

    for ch_num in 0..num_channels {
        unit.channels[ch_num].qu_tab_idx = [0; 32];
        decode_channel_code_tab(gb, cb, unit, ch_num)?;
    }

    Ok(())
}

/// Decode the Huffman-coded spectral lines of one quant unit.
///
/// This is a generalized version for all known coding modes.
fn decode_qu_spectra(
    gb: &mut BitReader<'_>,
    tab: (u8, u8, u8, u8),
    vlc_tab: &Vlc,
    out: &mut [i16],
) -> Result<()> {
    let (group_size, num_coeffs, bits, is_signed) = tab;
    let (group_size, num_coeffs, bits) =
        (usize::from(group_size), usize::from(num_coeffs), u32::from(bits));
    let mut pos = 0;

    while pos < out.len() {
        if group_size == 1 || gb.get1() {
            for _ in 0..group_size {
                let mut val = vlc_tab.decode(gb)?;

                for _ in 0..num_coeffs {
                    let mut cf = (val & ((1 << bits) - 1)) as i32;
                    if is_signed != 0 {
                        cf = sign_extend(cf as u32, bits);
                    }
                    else if cf != 0 && gb.get1() {
                        cf = -cf;
                    }

                    if let Some(o) = out.get_mut(pos) {
                        *o = cf as i16;
                    }
                    pos += 1;
                    val >>= bits;
                }
            }
        }
        else {
            // Group skipped.
            pos += group_size * num_coeffs;
        }
    }

    Ok(())
}

/// Decode the Huffman-coded spectrum for all channels.
fn decode_spectrum(
    gb: &mut BitReader<'_>,
    cb: &Codebooks,
    unit: &mut ChanUnit,
    num_channels: usize,
) -> Result<()> {
    let used = unit.used_quant_units;
    let full = unit.use_full_table;
    let num_coded_subbands = unit.num_coded_subbands;
    let master_wl = unit.channels[0].qu_wordlen;

    for ch_num in 0..num_channels {
        // The master spectrum is needed to clone from while decoding the slave channel.
        let (head, tail) = unit.channels.split_at_mut(1);
        let (master, chan) =
            if ch_num == 0 { (None, &mut head[0]) } else { (Some(&head[0]), &mut tail[0]) };

        chan.spectrum.fill(0);

        // Set the power compensation level to disabled.
        chan.power_levs = [POWER_COMP_OFF; 5];

        for qu in 0..used {
            let start = usize::from(QU_TO_SPEC_POS[qu]);
            let end = usize::from(QU_TO_SPEC_POS[qu + 1]);

            let wordlen = chan.qu_wordlen[qu];
            let mut codetab = chan.qu_tab_idx[qu] as usize;
            if wordlen != 0 {
                if !full {
                    codetab = match CT_RESTRICTED_TO_FULL[chan.table_type]
                        .get(wordlen as usize - 1)
                        .and_then(|t| t.get(codetab))
                    {
                        Some(&c) => usize::from(c),
                        None => return decode_error("atrac3plus: invalid code table index"),
                    };
                }

                let tab_index = (chan.table_type * 8 + codetab) * 7 + wordlen as usize - 1;
                let (Some(&tab), Some(vlc_tab)) =
                    (SPECTRA_TABS.get(tab_index), cb.spectrum(tab_index))
                else {
                    return decode_error("atrac3plus: invalid spectrum table");
                };

                decode_qu_spectra(gb, tab, vlc_tab, &mut chan.spectrum[start..end])?;
            }
            else if let Some(master) = master {
                if master_wl[qu] != 0 && codetab == 0 {
                    // Copy coefficients from the master.
                    chan.spectrum[start..end].copy_from_slice(&master.spectrum[start..end]);
                    chan.qu_wordlen[qu] = master_wl[qu];
                }
            }
        }

        // Power compensation levels are only present in the bitstream if there are more than 2
        // quant units. The lowest two units correspond to the frequencies 0...351 Hz, which
        // shouldn't be affected by the power compensation.
        if used > 2 {
            let n = usize::from(SUBBAND_TO_NUM_POWGRPS[num_coded_subbands.saturating_sub(1)]);
            for lev in chan.power_levs[..n].iter_mut() {
                *lev = gb.get(4) as u8;
            }
        }
    }

    Ok(())
}

/// Retrieve a number of flag bits from the bitstream.
///
/// The data can be shortened in the following two common cases: if all bits are zero then only one
/// signal bit `0` is stored, if all bits are ones then two signal bits `1, 0` are stored.
/// Otherwise all bits are stored directly, prefixed by the two signal bits `1, 1`.
///
/// Returns whether at least one flag is non-zero.
fn get_subband_flags(gb: &mut BitReader<'_>, out: &mut [u8]) -> bool {
    out.fill(0);

    let result = gb.get1();
    if result {
        if gb.get1() {
            for o in out.iter_mut() {
                *o = u8::from(gb.get1());
            }
        }
        else {
            out.fill(1);
        }
    }
    result
}

/// Decode the number of gain control points.
fn decode_gainc_npoints(
    gb: &mut BitReader<'_>,
    cb: &Codebooks,
    chans: &mut [ChanParams; 2],
    cur: usize,
    ch_num: usize,
    coded_subbands: usize,
) -> Result<()> {
    let ref_np: [usize; SUBBANDS] = std::array::from_fn(|i| chans[0].gain_data[cur][i].num_points);
    let gd = &mut chans[ch_num].gain_data[cur];

    match gb.get(2) {
        // Fixed-length coding.
        0 => {
            for g in gd[..coded_subbands].iter_mut() {
                g.num_points = gb.get(3) as usize;
            }
        }
        // Variable-length coding.
        1 => {
            for g in gd[..coded_subbands].iter_mut() {
                g.num_points = cb.gain[0].decode(gb)? as usize;
            }
        }
        2 => {
            if ch_num != 0 {
                // VLC modulo delta to the master channel.
                for i in 0..coded_subbands {
                    let delta = cb.gain[1].decode(gb)? as usize;
                    gd[i].num_points = (ref_np[i] + delta) & 7;
                }
            }
            else {
                // VLC modulo delta to the previous.
                gd[0].num_points = cb.gain[0].decode(gb)? as usize;

                for i in 1..coded_subbands {
                    let delta = cb.gain[1].decode(gb)? as usize;
                    gd[i].num_points = (gd[i - 1].num_points + delta) & 7;
                }
            }
        }
        _ => {
            if ch_num != 0 {
                // Copy the data from the master channel.
                for i in 0..coded_subbands {
                    gd[i].num_points = ref_np[i];
                }
            }
            else {
                // Shorter delta to min.
                let delta_bits = gb.get(2);
                let min_val = gb.get(3) as usize;

                for g in gd[..coded_subbands].iter_mut() {
                    g.num_points = min_val + gb.get(delta_bits) as usize;
                    if g.num_points > 7 {
                        return decode_error("atrac3plus: invalid number of gain points");
                    }
                }
            }
        }
    }

    if gd[..coded_subbands].iter().any(|g| g.num_points > 7) {
        return decode_error("atrac3plus: invalid number of gain points");
    }

    Ok(())
}

/// Coding mode 3 (slave) for gain compensation levels.
fn gainc_level_mode3s(dst: &mut GainInfo, reference: &GainInfo) {
    for i in 0..dst.num_points {
        dst.lev_code[i] = if i >= reference.num_points { 7 } else { reference.lev_code[i] };
    }
}

/// Coding mode 1 (master) for gain compensation levels.
fn gainc_level_mode1m(gb: &mut BitReader<'_>, cb: &Codebooks, dst: &mut GainInfo) -> Result<()> {
    if dst.num_points > 0 {
        dst.lev_code[0] = vlc(gb, &cb.gain[2])?;
    }

    for i in 1..dst.num_points {
        let delta = vlc(gb, &cb.gain[3])?;
        dst.lev_code[i] = (dst.lev_code[i - 1] + delta) & 0xf;
    }

    Ok(())
}

/// Decode the level code for each gain control point.
fn decode_gainc_levels(
    gb: &mut BitReader<'_>,
    cb: &Codebooks,
    chans: &mut [ChanParams; 2],
    cur: usize,
    ch_num: usize,
    coded_subbands: usize,
) -> Result<()> {
    let ref_gd = chans[0].gain_data[cur];
    let gd = &mut chans[ch_num].gain_data[cur];

    match gb.get(2) {
        // Fixed-length coding.
        0 => {
            for g in gd[..coded_subbands].iter_mut() {
                for i in 0..g.num_points {
                    g.lev_code[i] = gb.get(4) as i32;
                }
            }
        }
        1 => {
            if ch_num != 0 {
                // VLC modulo delta to the master channel.
                for sb in 0..coded_subbands {
                    for i in 0..gd[sb].num_points {
                        let delta = vlc(gb, &cb.gain[5])?;
                        let pred =
                            if i >= ref_gd[sb].num_points { 7 } else { ref_gd[sb].lev_code[i] };
                        gd[sb].lev_code[i] = (pred + delta) & 0xf;
                    }
                }
            }
            else {
                // VLC modulo delta to the previous.
                for g in gd[..coded_subbands].iter_mut() {
                    gainc_level_mode1m(gb, cb, g)?;
                }
            }
        }
        2 => {
            if ch_num != 0 {
                // VLC modulo delta to the previous or clone master.
                for sb in 0..coded_subbands {
                    if gd[sb].num_points > 0 {
                        if gb.get1() {
                            gainc_level_mode1m(gb, cb, &mut gd[sb])?;
                        }
                        else {
                            gainc_level_mode3s(&mut gd[sb], &ref_gd[sb]);
                        }
                    }
                }
            }
            else {
                // VLC modulo delta to the level codes of the previous subband.
                if gd[0].num_points > 0 {
                    gainc_level_mode1m(gb, cb, &mut gd[0])?;
                }

                for sb in 1..coded_subbands {
                    for i in 0..gd[sb].num_points {
                        let delta = vlc(gb, &cb.gain[4])?;
                        let pred =
                            if i >= gd[sb - 1].num_points { 7 } else { gd[sb - 1].lev_code[i] };
                        gd[sb].lev_code[i] = (pred + delta) & 0xf;
                    }
                }
            }
        }
        _ => {
            if ch_num != 0 {
                // Clone the master.
                for sb in 0..coded_subbands {
                    gainc_level_mode3s(&mut gd[sb], &ref_gd[sb]);
                }
            }
            else {
                // Shorter delta to min.
                let delta_bits = gb.get(2);
                let min_val = gb.get(4) as i32;

                for g in gd[..coded_subbands].iter_mut() {
                    for i in 0..g.num_points {
                        g.lev_code[i] = min_val + gb.get(delta_bits) as i32;
                        if g.lev_code[i] > 15 {
                            return decode_error("atrac3plus: invalid gain level");
                        }
                    }
                }
            }
        }
    }

    if gd[..coded_subbands].iter().any(|g| g.lev_code[..g.num_points].iter().any(|&l| l > 15)) {
        return decode_error("atrac3plus: invalid gain level");
    }

    Ok(())
}

/// Coding mode 0 for gain compensation locations.
fn gainc_loc_mode0(gb: &mut BitReader<'_>, dst: &mut GainInfo, pos: usize) {
    if pos == 0 || dst.loc_code[pos - 1] < 15 {
        dst.loc_code[pos] = gb.get(5) as i32;
    }
    else if dst.loc_code[pos - 1] >= 30 {
        dst.loc_code[pos] = 31;
    }
    else {
        let delta_bits = log2((30 - dst.loc_code[pos - 1]) as u32) + 1;
        dst.loc_code[pos] = dst.loc_code[pos - 1] + gb.get(delta_bits) as i32 + 1;
    }
}

/// Coding mode 1 for gain compensation locations.
fn gainc_loc_mode1(gb: &mut BitReader<'_>, cb: &Codebooks, dst: &mut GainInfo) -> Result<()> {
    if dst.num_points > 0 {
        // The first coefficient is stored directly.
        dst.loc_code[0] = gb.get(5) as i32;

        for i in 1..dst.num_points {
            // Switch the VLC according to the curve direction (ascending/descending).
            let tab =
                if dst.lev_code[i] <= dst.lev_code[i - 1] { &cb.gain[7] } else { &cb.gain[9] };
            dst.loc_code[i] = dst.loc_code[i - 1] + vlc(gb, tab)?;
        }
    }
    Ok(())
}

/// Decode the location code for each gain control point.
fn decode_gainc_loc_codes(
    gb: &mut BitReader<'_>,
    cb: &Codebooks,
    chans: &mut [ChanParams; 2],
    cur: usize,
    ch_num: usize,
    coded_subbands: usize,
) -> Result<()> {
    let ref_gd = chans[0].gain_data[cur];
    let gd = &mut chans[ch_num].gain_data[cur];

    match gb.get(2) {
        // Sequence of numbers in ascending order.
        0 => {
            for g in gd[..coded_subbands].iter_mut() {
                for i in 0..g.num_points {
                    gainc_loc_mode0(gb, g, i);
                }
            }
        }
        1 => {
            if ch_num != 0 {
                for sb in 0..coded_subbands {
                    if gd[sb].num_points == 0 {
                        continue;
                    }
                    let dst = &mut gd[sb];
                    let r = &ref_gd[sb];

                    // The first value is a VLC-coded modulo delta to the master.
                    let delta = vlc(gb, &cb.gain[10])?;
                    let pred = if r.num_points > 0 { r.loc_code[0] } else { 0 };
                    dst.loc_code[0] = (pred + delta) & 0x1f;

                    for i in 1..dst.num_points {
                        let more_than_ref = i >= r.num_points;
                        if dst.lev_code[i] > dst.lev_code[i - 1] {
                            // Ascending curve.
                            if more_than_ref {
                                let delta = vlc(gb, &cb.gain[9])?;
                                dst.loc_code[i] = dst.loc_code[i - 1] + delta;
                            }
                            else if gb.get1() {
                                // Direct coding.
                                gainc_loc_mode0(gb, dst, i);
                            }
                            else {
                                // Clone the master.
                                dst.loc_code[i] = r.loc_code[i];
                            }
                        }
                        else {
                            // Descending curve.
                            let tab = if more_than_ref { &cb.gain[7] } else { &cb.gain[10] };
                            let delta = vlc(gb, tab)?;
                            if more_than_ref {
                                dst.loc_code[i] = dst.loc_code[i - 1] + delta;
                            }
                            else {
                                dst.loc_code[i] = (r.loc_code[i] + delta) & 0x1f;
                            }
                        }
                    }
                }
            }
            else {
                // VLC delta to the previous.
                for g in gd[..coded_subbands].iter_mut() {
                    gainc_loc_mode1(gb, cb, g)?;
                }
            }
        }
        2 => {
            if ch_num != 0 {
                for sb in 0..coded_subbands {
                    if gd[sb].num_points == 0 {
                        continue;
                    }
                    let dst = &mut gd[sb];
                    let r = &ref_gd[sb];
                    if dst.num_points > r.num_points || gb.get1() {
                        gainc_loc_mode1(gb, cb, dst)?;
                    }
                    else {
                        // Clone the master for the whole subband.
                        for i in 0..dst.num_points {
                            dst.loc_code[i] = r.loc_code[i];
                        }
                    }
                }
            }
            else {
                // The data for the first subband is coded directly.
                for i in 0..gd[0].num_points {
                    gainc_loc_mode0(gb, &mut gd[0], i);
                }

                for sb in 1..coded_subbands {
                    if gd[sb].num_points == 0 {
                        continue;
                    }
                    let prev = gd[sb - 1];
                    let dst = &mut gd[sb];

                    // The first value is a VLC-coded modulo delta to the corresponding value of
                    // the previous subband, if any, or zero.
                    let delta = vlc(gb, &cb.gain[6])?;
                    let pred = if prev.num_points > 0 { prev.loc_code[0] } else { 0 };
                    dst.loc_code[0] = (pred + delta) & 0x1f;

                    for i in 1..dst.num_points {
                        let more_than_ref = i >= prev.num_points;
                        // Select the VLC according to the curve direction and the presence of a
                        // prediction.
                        let tab = &cb.gain[usize::from(dst.lev_code[i] > dst.lev_code[i - 1]) * 2
                            + usize::from(more_than_ref)
                            + 6];
                        let delta = vlc(gb, tab)?;
                        if more_than_ref {
                            dst.loc_code[i] = dst.loc_code[i - 1] + delta;
                        }
                        else {
                            dst.loc_code[i] = (prev.loc_code[i] + delta) & 0x1f;
                        }
                    }
                }
            }
        }
        _ => {
            if ch_num != 0 {
                // Clone the master or direct coding.
                for sb in 0..coded_subbands {
                    for i in 0..gd[sb].num_points {
                        if i >= ref_gd[sb].num_points {
                            gainc_loc_mode0(gb, &mut gd[sb], i);
                        }
                        else {
                            gd[sb].loc_code[i] = ref_gd[sb].loc_code[i];
                        }
                    }
                }
            }
            else {
                // Shorter delta to min.
                let delta_bits = gb.get(2) + 1;
                let min_val = gb.get(5) as i32;

                for g in gd[..coded_subbands].iter_mut() {
                    for i in 0..g.num_points {
                        g.loc_code[i] = min_val + i as i32 + gb.get(delta_bits) as i32;
                    }
                }
            }
        }
    }

    // Validate the decoded information.
    for g in gd[..coded_subbands].iter() {
        for i in 0..g.num_points {
            if !(0..=31).contains(&g.loc_code[i]) || (i > 0 && g.loc_code[i] <= g.loc_code[i - 1]) {
                return decode_error("atrac3plus: invalid gain location");
            }
        }
    }

    Ok(())
}

/// Decode the gain control data for all channels.
fn decode_gainc_data(
    gb: &mut BitReader<'_>,
    cb: &Codebooks,
    unit: &mut ChanUnit,
    num_channels: usize,
) -> Result<()> {
    let cur = unit.cur;

    for ch_num in 0..num_channels {
        unit.channels[ch_num].gain_data[cur] = [GainInfo::default(); SUBBANDS];

        // Is gain control data present?
        if gb.get1() {
            let coded_subbands = gb.get(4) as usize + 1;
            // Is high band gain data replication on?
            let num_gain_subbands = if gb.get1() { gb.get(4) as usize + 1 } else { coded_subbands };

            let chans = &mut unit.channels;
            decode_gainc_npoints(gb, cb, chans, cur, ch_num, coded_subbands)?;
            decode_gainc_levels(gb, cb, chans, cur, ch_num, coded_subbands)?;
            decode_gainc_loc_codes(gb, cb, chans, cur, ch_num, coded_subbands)?;

            // Propagate the gain data if requested.
            let gd = &mut chans[ch_num].gain_data[cur];
            for sb in coded_subbands..num_gain_subbands {
                gd[sb] = gd[sb - 1];
            }
        }
    }

    Ok(())
}

/// Decode the envelope for all tones of a channel.
fn decode_tones_envelope(
    gb: &mut BitReader<'_>,
    unit: &mut ChanUnit,
    ch_num: usize,
    band_has_tones: &[bool; SUBBANDS],
) {
    let cur = unit.cur;
    let num_tone_bands = unit.wave_synth[cur].num_tone_bands;
    let ref_env: [WaveEnvelope; SUBBANDS] =
        std::array::from_fn(|i| unit.channels[0].tones_info[cur][i].pend_env);
    let dst = &mut unit.channels[ch_num].tones_info[cur];

    if ch_num == 0 || !gb.get1() {
        // Mode 0: fixed-length coding.
        for sb in 0..num_tone_bands {
            if !band_has_tones[sb] {
                continue;
            }
            let env = &mut dst[sb].pend_env;
            env.has_start_point = gb.get1();
            env.start_pos = if env.has_start_point { gb.get(5) as i32 } else { -1 };
            env.has_stop_point = gb.get1();
            env.stop_pos = if env.has_stop_point { gb.get(5) as i32 } else { 32 };
        }
    }
    else {
        // Mode 1 (slave only): copy the master.
        for sb in 0..num_tone_bands {
            if band_has_tones[sb] {
                dst[sb].pend_env = ref_env[sb];
            }
        }
    }
}

/// Decode the number of tones for each subband of a channel.
fn decode_band_numwavs(
    gb: &mut BitReader<'_>,
    cb: &Codebooks,
    unit: &mut ChanUnit,
    ch_num: usize,
    band_has_tones: &[bool; SUBBANDS],
) -> Result<()> {
    let cur = unit.cur;
    let num_tone_bands = unit.wave_synth[cur].num_tone_bands;
    let ref_num: [usize; SUBBANDS] =
        std::array::from_fn(|i| unit.channels[0].tones_info[cur][i].num_wavs);
    let dst = &mut unit.channels[ch_num].tones_info[cur];

    match gb.get(ch_num as u32 + 1) {
        // Fixed-length coding.
        0 => {
            for sb in 0..num_tone_bands {
                if band_has_tones[sb] {
                    dst[sb].num_wavs = gb.get(4) as usize;
                }
            }
        }
        // Variable-length coding.
        1 => {
            for sb in 0..num_tone_bands {
                if band_has_tones[sb] {
                    dst[sb].num_wavs = cb.tone[1].decode(gb)? as usize;
                }
            }
        }
        // VLC modulo delta to the master (slave only).
        2 => {
            for sb in 0..num_tone_bands {
                if band_has_tones[sb] {
                    let delta = sign_extend(cb.tone[2].decode(gb)?, 3);
                    dst[sb].num_wavs = ((ref_num[sb] as i32 + delta) & 0xf) as usize;
                }
            }
        }
        // Copy the master (slave only).
        _ => {
            for sb in 0..num_tone_bands {
                if band_has_tones[sb] {
                    dst[sb].num_wavs = ref_num[sb];
                }
            }
        }
    }

    // Initialize the start tone index for each subband.
    let ws = &mut unit.wave_synth[cur];
    for sb in 0..num_tone_bands {
        if band_has_tones[sb] {
            if ws.tones_index + dst[sb].num_wavs > MAX_WAVES {
                return decode_error("atrac3plus: too many tones");
            }
            dst[sb].start_index = ws.tones_index;
            ws.tones_index += dst[sb].num_wavs;
        }
    }

    Ok(())
}

/// Decode the frequency information for each subband of a channel.
fn decode_tones_frequency(
    gb: &mut BitReader<'_>,
    cb: &Codebooks,
    unit: &mut ChanUnit,
    ch_num: usize,
    band_has_tones: &[bool; SUBBANDS],
) -> Result<()> {
    let cur = unit.cur;
    let ref_info = unit.channels[0].tones_info[cur];
    let dst = &unit.channels[ch_num].tones_info[cur];
    let waves = &mut unit.wave_synth[cur].waves;
    let num_tone_bands = unit.wave_synth[cur].num_tone_bands;

    if ch_num == 0 || !gb.get1() {
        // Mode 0: fixed-length coding.
        for sb in 0..num_tone_bands {
            if !band_has_tones[sb] || dst[sb].num_wavs == 0 {
                continue;
            }
            let n = dst[sb].num_wavs;
            let iwav = &mut waves[dst[sb].start_index..dst[sb].start_index + n];
            let direction = if n > 1 { gb.get1() } else { false };
            if direction {
                // Packed numbers in descending order.
                iwav[n - 1].freq_index = gb.get(10) as i32;
                for i in (0..n - 1).rev() {
                    let nbits = log2(iwav[i + 1].freq_index as u32) + 1;
                    iwav[i].freq_index = gb.get(nbits) as i32;
                }
            }
            else {
                // Packed numbers in ascending order.
                for i in 0..n {
                    if i == 0 || iwav[i - 1].freq_index < 512 {
                        iwav[i].freq_index = gb.get(10) as i32;
                    }
                    else {
                        let nbits = log2((1023 - iwav[i - 1].freq_index) as u32) + 1;
                        iwav[i].freq_index = gb.get(nbits) as i32 + 1024 - (1 << nbits);
                    }
                }
            }
        }
    }
    else {
        // Mode 1: VLC modulo delta to the master (slave only).
        for sb in 0..num_tone_bands {
            if !band_has_tones[sb] || dst[sb].num_wavs == 0 {
                continue;
            }
            let ref_start = ref_info[sb].start_index;
            let ref_num = ref_info[sb].num_wavs;
            let out_start = dst[sb].start_index;
            for i in 0..dst[sb].num_wavs {
                let delta = sign_extend(cb.tone[6].decode(gb)?, 8);
                let pred = if i < ref_num {
                    waves[ref_start + i].freq_index
                }
                else if ref_num != 0 {
                    waves[ref_start + ref_num - 1].freq_index
                }
                else {
                    0
                };
                waves[out_start + i].freq_index = (pred + delta) & 0x3ff;
            }
        }
    }

    Ok(())
}

/// Decode the amplitude information for each subband of a channel.
fn decode_tones_amplitude(
    gb: &mut BitReader<'_>,
    cb: &Codebooks,
    unit: &mut ChanUnit,
    ch_num: usize,
    band_has_tones: &[bool; SUBBANDS],
) -> Result<()> {
    let cur = unit.cur;
    let ref_info = unit.channels[0].tones_info[cur];
    let dst = &unit.channels[ch_num].tones_info[cur];
    let waves = &mut unit.wave_synth[cur].waves;
    let num_tone_bands = unit.wave_synth[cur].num_tone_bands;
    let mut refwaves = [-1i32; MAX_WAVES];

    if ch_num != 0 {
        for sb in 0..num_tone_bands {
            if !band_has_tones[sb] || dst[sb].num_wavs == 0 {
                continue;
            }
            let (ds, rs) = (dst[sb].start_index, ref_info[sb].start_index);
            for j in 0..dst[sb].num_wavs {
                let mut fi = 0;
                let mut maxdiff = 1024;
                for i in 0..ref_info[sb].num_wavs {
                    let diff = (waves[ds + j].freq_index - waves[rs + i].freq_index).abs();
                    if diff < maxdiff {
                        maxdiff = diff;
                        fi = i;
                    }
                }

                refwaves[ds + j] = if maxdiff < 8 {
                    (fi + rs) as i32
                }
                else if j < ref_info[sb].num_wavs {
                    (j + rs) as i32
                }
                else {
                    -1
                };
            }
        }
    }

    let mode = gb.get(ch_num as u32 + 1);

    // The reference amplitude of a wave, or `default` if it has no reference.
    let ref_amp = |waves: &[WaveParam; MAX_WAVES], idx: usize, default: i32| -> i32 {
        match refwaves[idx] {
            r if r >= 0 => waves[r as usize].amp_sf,
            _ => default,
        }
    };

    // Only the amplitude mode 1 (one scale factor per wave) is supported by the parser.
    for sb in 0..num_tone_bands {
        if !band_has_tones[sb] {
            continue;
        }
        let start = dst[sb].start_index;
        let num = dst[sb].num_wavs;

        // Modes 0-2 skip bands without waves; mode 3 has nothing to do for them.
        if num == 0 {
            continue;
        }

        match mode {
            // Fixed-length coding.
            0 => {
                for w in waves[start..start + num].iter_mut() {
                    w.amp_sf = gb.get(6) as i32;
                }
            }
            // Min + VLC delta.
            1 => {
                for w in waves[start..start + num].iter_mut() {
                    w.amp_sf = vlc(gb, &cb.tone[3])? + 20;
                }
            }
            // VLC modulo delta to the master (slave only).
            2 => {
                for i in 0..num {
                    let delta = sign_extend(cb.tone[5].decode(gb)?, 5);
                    let pred = ref_amp(waves, start + i, 34);
                    waves[start + i].amp_sf = (pred + delta) & 0x3f;
                }
            }
            // Clone the master (slave only).
            _ => {
                for i in 0..num {
                    waves[start + i].amp_sf = ref_amp(waves, start + i, 32);
                }
            }
        }
    }

    if waves.iter().any(|w| w.amp_sf > 63) {
        return decode_error("atrac3plus: invalid wave amplitude");
    }

    Ok(())
}

/// Decode the phase information for each subband of a channel.
fn decode_tones_phase(
    gb: &mut BitReader<'_>,
    unit: &mut ChanUnit,
    ch_num: usize,
    band_has_tones: &[bool; SUBBANDS],
) {
    let cur = unit.cur;
    let dst = &unit.channels[ch_num].tones_info[cur];
    let waves = &mut unit.wave_synth[cur].waves;

    for sb in 0..unit.wave_synth[cur].num_tone_bands {
        if !band_has_tones[sb] {
            continue;
        }
        for w in waves[dst[sb].start_index..dst[sb].start_index + dst[sb].num_wavs].iter_mut() {
            w.phase_index = gb.get(5) as i32;
        }
    }
}

/// Decode the tones info for all channels.
fn decode_tones_info(
    gb: &mut BitReader<'_>,
    cb: &Codebooks,
    unit: &mut ChanUnit,
    num_channels: usize,
) -> Result<()> {
    let cur = unit.cur;

    for ch_num in 0..num_channels {
        unit.channels[ch_num].tones_info[cur] = [WavesData::default(); SUBBANDS];
    }

    unit.wave_synth[cur].tones_present = gb.get1();
    if !unit.wave_synth[cur].tones_present {
        return Ok(());
    }

    unit.wave_synth[cur].waves = [WaveParam::default(); MAX_WAVES];

    // Amplitude mode: 1 = low range, 0 = high range (not supported).
    if !gb.get1() {
        return decode_error("atrac3plus: GHA amplitude mode 0 is not supported");
    }

    let num_tone_bands = cb.tone[0].decode(gb)? as usize + 1;
    if num_tone_bands > SUBBANDS {
        return decode_error("atrac3plus: invalid number of tone bands");
    }
    let ws = &mut unit.wave_synth[cur];
    ws.num_tone_bands = num_tone_bands;

    if num_channels == 2 {
        get_subband_flags(gb, &mut ws.tone_sharing[..num_tone_bands]);
        get_subband_flags(gb, &mut ws.tone_master[..num_tone_bands]);
        get_subband_flags(gb, &mut ws.invert_phase[..num_tone_bands]);
    }

    ws.tones_index = 0;

    for ch_num in 0..num_channels {
        let mut band_has_tones = [false; SUBBANDS];
        for (i, b) in band_has_tones[..num_tone_bands].iter_mut().enumerate() {
            *b = ch_num == 0 || unit.wave_synth[cur].tone_sharing[i] == 0;
        }

        decode_tones_envelope(gb, unit, ch_num, &band_has_tones);
        decode_band_numwavs(gb, cb, unit, ch_num, &band_has_tones)?;
        decode_tones_frequency(gb, cb, unit, ch_num, &band_has_tones)?;
        decode_tones_amplitude(gb, cb, unit, ch_num, &band_has_tones)?;
        decode_tones_phase(gb, unit, ch_num, &band_has_tones);
    }

    if num_channels == 2 {
        let (head, tail) = unit.channels.split_at_mut(1);
        let (t0, t1) = (&mut head[0].tones_info[cur], &mut tail[0].tones_info[cur]);
        let ws = &unit.wave_synth[cur];
        for i in 0..num_tone_bands {
            if ws.tone_sharing[i] != 0 {
                t1[i] = t0[i];
            }
            if ws.tone_master[i] != 0 {
                std::mem::swap(&mut t0[i], &mut t1[i]);
            }
        }
    }

    Ok(())
}

/// Decode the bitstream data of a channel unit.
pub(crate) fn decode_channel_unit(
    gb: &mut BitReader<'_>,
    cb: &Codebooks,
    unit: &mut ChanUnit,
    num_channels: usize,
) -> Result<()> {
    // Parse the sound header.
    unit.num_quant_units = gb.get(5) as usize + 1;
    if unit.num_quant_units > 28 && unit.num_quant_units < 32 {
        return decode_error("atrac3plus: invalid number of quantization units");
    }

    unit.mute_flag = gb.get1();

    // Decode the various sound parameters.
    decode_quant_wordlen(gb, cb, unit, num_channels)?;

    unit.num_subbands = usize::from(QU_TO_SUBBAND[unit.num_quant_units - 1]) + 1;
    unit.num_coded_subbands = if unit.used_quant_units != 0 {
        usize::from(QU_TO_SUBBAND[unit.used_quant_units - 1]) + 1
    }
    else {
        0
    };

    decode_scale_factors(gb, cb, unit, num_channels)?;
    decode_code_table_indexes(gb, cb, unit, num_channels)?;
    decode_spectrum(gb, cb, unit, num_channels)?;

    if num_channels == 2 {
        let n = unit.num_coded_subbands;
        get_subband_flags(gb, &mut unit.swap_channels[..n]);
        get_subband_flags(gb, &mut unit.negate_coeffs[..n]);
    }

    // Decode the window shape flags for all channels.
    let (cur, num_subbands) = (unit.cur, unit.num_subbands);
    for chan in unit.channels[..num_channels].iter_mut() {
        get_subband_flags(gb, &mut chan.wnd_shape[cur][..num_subbands]);
    }

    decode_gainc_data(gb, cb, unit, num_channels)?;
    decode_tones_info(gb, cb, unit, num_channels)?;

    // Decode the global noise info. It is parsed but not used by the reference decoder.
    if gb.get1() {
        gb.skip(8);
    }

    Ok(())
}
