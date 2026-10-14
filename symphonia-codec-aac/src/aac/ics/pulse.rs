// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// Previous Author: Kostya Shishkov <kostya.shiskov@gmail.com>
//
// This source file includes code originally written for the NihAV
// project. With the author's permission, it has been relicensed for,
// and ported to the Symphonia project.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::errors::Result;
use symphonia_core::io::ReadBitsLtr;

use super::pow43;

#[derive(Clone, Copy)]
#[allow(dead_code)]
pub struct Pulse {
    number_pulse: usize,
    pulse_start_sfb: usize,
    pulse_offset: [u8; 4],
    pulse_amp: [u8; 4],
}

impl Pulse {
    pub fn read<B: ReadBitsLtr>(bs: &mut B) -> Result<Option<Self>> {
        let pulse_data_present = bs.read_bool()?;

        if !pulse_data_present {
            return Ok(None);
        }

        let number_pulse = bs.read_bits_leq32(2)? as usize + 1;
        let pulse_start_sfb = bs.read_bits_leq32(6)? as usize;

        let mut pulse_offset: [u8; 4] = [0; 4];
        let mut pulse_amp: [u8; 4] = [0; 4];

        for i in 0..number_pulse {
            pulse_offset[i] = bs.read_bits_leq32(5)? as u8;
            pulse_amp[i] = bs.read_bits_leq32(4)? as u8;
        }

        Ok(Some(Self { number_pulse, pulse_start_sfb, pulse_offset, pulse_amp }))
    }

    /// Add the pulses to the dequantized spectrum `coeffs` of a long window, ISO/IEC 14496-3
    /// section 4.6.3.
    ///
    /// The pulses are added to the *quantized* values (`x_quant[k] += sign(x_quant[k]) * amp`, with
    /// a non-positive value counting as negative), which are then dequantized again with the
    /// scale factor of the band, so this must run before any stereo processing (the pulses are part
    /// of the coded channel).
    ///
    /// Only the bands of `sfb_cb`/`scales` below `max_sfb` carry data. A pulse in a band that is
    /// zero, noise substituted or intensity coded has no quantized value to modify and is ignored.
    pub fn apply(
        &self,
        bands: &[usize],
        max_sfb: usize,
        sfb_cb: &[u8],
        scales: &[f32],
        coeffs: &mut [f32; 1024],
    ) {
        let num_bands = max_sfb.min(bands.len() - 1);

        if self.pulse_start_sfb >= num_bands {
            return;
        }

        let mut k = bands[self.pulse_start_sfb];
        let mut band = self.pulse_start_sfb;

        for pno in 0..self.number_pulse {
            k += self.pulse_offset[pno] as usize;

            if k >= 1024 {
                return;
            }

            while bands[band + 1] <= k {
                band += 1;
            }

            if band >= num_bands || !(1..=11).contains(&sfb_cb[band]) {
                continue;
            }

            let scale = scales[band];

            // Recover the quantized value: |coeff| = |q|^(4/3) * scale.
            let quant = (coeffs[k].abs() / scale).powf(0.75).round();
            let quant = if coeffs[k] > 0.0 { quant } else { -quant };

            let amp = f32::from(self.pulse_amp[pno]);
            let quant = if quant > 0.0 { quant + amp } else { quant - amp };

            coeffs[k] = pow43(quant) * scale;
        }
    }
}
