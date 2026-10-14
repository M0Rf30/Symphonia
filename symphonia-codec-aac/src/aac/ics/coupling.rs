// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The coupling channel element (`ID_CCE`): ISO/IEC 14496-3 subclause 4.6.8.3 (ISO/IEC 13818-7
//! subclause 12, Table 22 and the `decode_coupling_channel()` pseudo code).
//!
//! A coupling channel carries a spectrum that is scaled by a gain and added to the channels of
//! other (target) channel elements. Dependently switched coupling (`ind_sw_cce_flag == 0`) adds
//! the scaled spectrum, before (`cc_domain == 0`) or after (`cc_domain == 1`) the TNS filters of
//! the target. Independently switched coupling (`ind_sw_cce_flag == 1`) decodes the coupling
//! channel to the time domain and adds the scaled signal to the output of the target.

use symphonia_core::errors::Result;
use symphonia_core::io::ReadBitsLtr;

use symphonia_common::mpeg::audio::AudioObjectType;

use crate::aac::codebooks;
use crate::aac::common::*;
use crate::aac::dsp;

use super::{Ics, ZERO_HCB};

/// The maximum number of channel elements coupled to by one coupling channel.
const MAX_TARGETS: usize = 8;
/// The maximum number of gain lists: a gain list for each channel of each target.
const MAX_GAIN_LISTS: usize = 2 * MAX_TARGETS;
/// The maximum number of gains of a list: one for each scalefactor band of each window group.
const MAX_GAINS: usize = 128;

/// `cc_scale_table` (Table 61 of ISO/IEC 13818-7).
const CC_SCALE: [f32; 4] = [1.090_507_7, 1.189_207_1, std::f32::consts::SQRT_2, 2.0];

/// Where the coupling channel is added to its targets.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CouplingPoint {
    /// Spectrum of the target, before its TNS filters.
    BeforeTns,
    /// Spectrum of the target, after its TNS filters.
    AfterTns,
    /// Time domain output of the target.
    AfterImdct,
}

#[derive(Copy, Clone, Default)]
struct Target {
    is_cpe: bool,
    tag: u8,
    /// `(cc_l << 1) | cc_r` for a CPE (3: a gain list for each channel, 2: left channel only,
    /// 1: right channel only, 0: one gain list shared by both channels), and 2 for a SCE.
    select: u8,
}

pub struct CouplingChannel {
    /// Whether the element was in the current `raw_data_block()`.
    pub present: bool,
    /// The window shape of the last block of the coupling channel that was transformed.
    last_window_shape: bool,
    point: CouplingPoint,
    targets: [Target; MAX_TARGETS],
    num_targets: usize,
    num_gain_lists: usize,
    ics: Ics,
    /// The gains of each list, indexed by the scalefactor band index (`g * max_sfb + sfb`), or the
    /// first gain only for independently switched coupling.
    gains: Box<[[f32; MAX_GAINS]; MAX_GAIN_LISTS]>,
    /// The output of the coupling channel, for coupling after the IMDCT.
    time: Box<[f32; 1024]>,
}

impl CouplingChannel {
    pub fn new(sbinfo: GASubbandInfo, aot: AudioObjectType) -> Self {
        Self {
            present: false,
            last_window_shape: false,
            point: CouplingPoint::BeforeTns,
            targets: [Target::default(); MAX_TARGETS],
            num_targets: 0,
            num_gain_lists: 0,
            ics: Ics::new(sbinfo, aot),
            gains: Box::new([[0.0; MAX_GAINS]; MAX_GAIN_LISTS]),
            time: Box::new([0.0; 1024]),
        }
    }

    pub fn reset(&mut self) {
        self.present = false;
        self.ics.reset();
        self.time.fill(0.0);
        self.last_window_shape = false;
    }

    /// Read the `coupling_channel_element()` after its `element_instance_tag`.
    pub fn decode<B: ReadBitsLtr>(
        &mut self,
        bs: &mut B,
        lcg: &mut Lcg,
        aot: AudioObjectType,
    ) -> Result<()> {
        let ind_sw_cce_flag = bs.read_bool()?;
        let num_coupled = bs.read_bits_leq32(3)? as usize;

        self.num_targets = num_coupled + 1;
        self.num_gain_lists = 0;

        for c in 0..self.num_targets {
            self.num_gain_lists += 1;

            let is_cpe = bs.read_bool()?;
            let tag = bs.read_bits_leq32(4)? as u8;
            let mut select = 2;

            if is_cpe {
                select = bs.read_bits_leq32(2)? as u8;

                if select == 3 {
                    self.num_gain_lists += 1;
                }
            }

            self.targets[c] = Target { is_cpe, tag, select };
        }

        let cc_domain = bs.read_bool()?;

        // An independently switched coupling channel is always coupled in the time domain.
        self.point = if ind_sw_cce_flag {
            CouplingPoint::AfterImdct
        }
        else if cc_domain {
            CouplingPoint::AfterTns
        }
        else {
            CouplingPoint::BeforeTns
        };

        let gain_element_sign = bs.read_bool()?;
        let cc_scale = CC_SCALE[bs.read_bits_leq32(2)? as usize];

        self.ics.decode(bs, lcg, aot, false)?;

        let max_sfb = self.ics.info.max_sfb;
        validate!(self.ics.info.window_groups * max_sfb <= MAX_GAINS);

        let scf_cb = &codebooks::SCALEFACTORS;

        // The first gain list is not transmitted: the coupling channel is added as is.
        for gain in self.gains[0].iter_mut() {
            *gain = 1.0;
        }

        for list in 1..self.num_gain_lists {
            // An independently switched coupling channel only has common gain elements.
            let cge = ind_sw_cce_flag || bs.read_bool()?;

            if cge {
                // `gain_element_sign` does not apply to a common gain element.
                let gain = i32::from(bs.read_codebook(scf_cb)?.0) - 60;
                let value = cc_scale.powi(-gain);

                self.gains[list][..MAX_GAINS].fill(value);
            }
            else {
                let mut acc = 0i32;
                let mut value = 1.0f32;

                for g in 0..self.ics.info.window_groups {
                    for sfb in 0..max_sfb {
                        let idx = g * max_sfb + sfb;

                        if self.ics.sfb_cb[g][sfb] == ZERO_HCB {
                            continue;
                        }

                        let delta = i32::from(bs.read_codebook(scf_cb)?.0) - 60;

                        if delta != 0 {
                            acc += delta;

                            // With `gain_element_sign` the least significant bit is the sign
                            // (1 for out of phase), the remaining bits the exponent.
                            value = if gain_element_sign {
                                let sign = 1.0 - 2.0 * (acc & 1) as f32;
                                sign * cc_scale.powi(-(acc >> 1))
                            }
                            else {
                                cc_scale.powi(-acc)
                            };
                        }

                        self.gains[list][idx] = value;
                    }
                }
            }
        }

        Ok(())
    }

    /// Prepare the coupling channel for the targets of the block: the coupling channels that are
    /// coupled after the IMDCT are decoded to the time domain.
    pub fn prepare(&mut self, dsp: &mut dsp::Dsp, rate_idx: usize) {
        // The TNS filters of the coupling channel are applied for all coupling points.
        self.ics.apply_tns(rate_idx);

        if self.point == CouplingPoint::AfterImdct {
            // FFmpeg windows the overlap of the previous transformed block, and the first half
            // of the block, both with the shape of the block that precedes it in the stream. The
            // blocks of a dependently switched coupling channel are not transformed, so the
            // block that was transformed last may have had another shape: apply the shape of the
            // preceding block to its overlap.
            if self.ics.info.long_win {
                dsp.rewindow_long_overlap(
                    &mut self.ics.delay,
                    self.last_window_shape,
                    self.ics.info.prev_window_shape,
                );
            }

            self.ics.imdct(dsp, &mut self.time[..]);
            self.last_window_shape = self.ics.info.window_shape;
        }
    }

    /// The gain list of a channel of a target element: `None` if the channel is not a target.
    fn gain_list(&self, is_cpe: bool, tag: u8, channel: usize) -> Option<usize> {
        let mut list = 0;

        for target in &self.targets[..self.num_targets] {
            let hit = target.is_cpe == is_cpe && target.tag == tag;

            if hit {
                if channel == 0 && target.select != 1 {
                    return Some(list);
                }

                if channel == 1 && target.select != 2 {
                    return Some(if target.select == 3 { list + 1 } else { list });
                }
            }

            list += if target.select == 3 { 2 } else { 1 };
        }

        None
    }

    /// Add the scaled spectrum of the coupling channel to a channel of a target element, if the
    /// channel is a target of this coupling channel, and the coupling is at `point`.
    pub fn couple_spectrum(
        &self,
        point: CouplingPoint,
        is_cpe: bool,
        tag: u8,
        channel: usize,
        dest: &mut Ics,
    ) {
        if !self.present || self.point != point {
            return;
        }

        let Some(list) = self.gain_list(is_cpe, tag, channel)
        else {
            return;
        };

        let info = &self.ics.info;
        let bands = self.ics.get_bands();
        let gains = &self.gains[list];

        for g in 0..info.window_groups {
            let first_win = info.get_group_start(g);
            let last_win = info.get_group_start(g + 1);

            for sfb in 0..info.max_sfb {
                if self.ics.sfb_cb[g][sfb] == ZERO_HCB {
                    continue;
                }

                let gain = gains[g * info.max_sfb + sfb];

                for w in first_win..last_win {
                    let range = w * 128 + bands[sfb]..w * 128 + bands[sfb + 1];

                    for (d, s) in dest.coeffs[range.clone()].iter_mut().zip(&self.ics.coeffs[range])
                    {
                        *d += gain * *s;
                    }
                }
            }
        }
    }

    /// Add the scaled output of the coupling channel to the output of a channel of a target
    /// element, if the channel is a target and the coupling is after the IMDCT.
    pub fn couple_time(&self, is_cpe: bool, tag: u8, channel: usize, dst: &mut [f32]) {
        if !self.present || self.point != CouplingPoint::AfterImdct {
            return;
        }

        if let Some(list) = self.gain_list(is_cpe, tag, channel) {
            let gain = self.gains[list][0];

            for (d, s) in dst.iter_mut().zip(self.time.iter()) {
                *d += gain * *s;
            }
        }
    }
}
