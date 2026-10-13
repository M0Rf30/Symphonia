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

//! Window function generators.

use std::f32::consts;
use std::f64::consts as consts64;

/// Window types.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowType {
    /// Simple sine window.
    Sine,
    /// Kaiser-Bessel derived window.
    KaiserBessel(f32),
}

/// Calculates window coefficients for the requested window type and size.
///
/// Set `half` flag to calculate only the first half of the window.
///
/// # Panics
///
/// Panics if `dst` is shorter than `size`.
pub fn generate_window(mode: WindowType, scale: f32, size: usize, half: bool, dst: &mut [f32]) {
    match mode {
        WindowType::Sine => {
            let param =
                if half { consts::PI / ((2 * size) as f32) } else { consts::PI / (size as f32) };
            for (n, d) in dst[..size].iter_mut().enumerate() {
                *d = (((n as f32) + 0.5) * param).sin() * scale;
            }
        }
        WindowType::KaiserBessel(alpha) => {
            let dlen = if half { size as f32 } else { (size as f32) * 0.5 };
            let alpha2 = f64::from((alpha * consts::PI / dlen) * (alpha * consts::PI / dlen));

            let mut kb: Vec<f64> = Vec::with_capacity(size);
            let mut sum = 0.0;
            for n in 0..size {
                let b = bessel_i0(((n * (size - n)) as f64) * alpha2);
                sum += b;
                kb.push(sum);
            }
            sum += 1.0;
            for (d, k) in dst[..size].iter_mut().zip(&kb) {
                *d = (k / sum).sqrt() as f32;
            }
        }
    };
}

/// Generates the rising half-window slope of the Vorbis power-sine window of `len` samples.
pub fn vorbis_power_sine_slope(len: usize) -> Vec<f32> {
    let denom = len as f64;

    (0..len)
        .map(|i| {
            let frac = consts64::FRAC_PI_2 * ((i as f64 + 0.5) / denom);
            (consts64::FRAC_PI_2 * frac.sin().powi(2)).sin() as f32
        })
        .collect()
}

/// Modified Bessel function of the first kind, order zero, `I0(z)`, evaluated with a fixed-length
/// power series in `x = (z / 2)^2`: `I0(z) = sum_k x^k / (k!)^2`.
pub fn bessel_i0(x: f64) -> f64 {
    let mut val: f64 = 1.0;
    for n in (1..64).rev() {
        val *= x / f64::from(n * n);
        val += 1.0;
    }
    val
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_bessel_i0() {
        assert!((bessel_i0(0.0) - 1.0).abs() < 1e-12);
        // I0(5) = 27.239871823604442
        // The argument is (z / 2)^2, so z = 5 is x = 6.25.
        assert!((bessel_i0(6.25) - 27.239_871_823_604_442).abs() < 1e-9);
    }
}
