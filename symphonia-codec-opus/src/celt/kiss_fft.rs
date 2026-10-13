// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Mixed-radix FFT, the core of the MDCT. Ported from libopus `celt/kiss_fft.c`
//! (`opus_fft_impl`, `kf_bfly2/3/4/5`, `opus_fft_c`/`opus_ifft_c`), restricted to the float
//! build (`FIXED_POINT` undefined) and to the single static 48 kHz mode, so `CUSTOM_MODES`-only
//! code (`opus_fft_alloc*`, `kf_factor`, `compute_bitrev_table`, `compute_twiddles`) is not
//! ported: the FFT plans are precomputed constants transcribed from
//! `celt/static_modes_float.h` (`fft_state48000_960_{0..3}`, `fft_twiddles48000_960`,
//! `fft_bitrev{480,240,120,60}`) by a throwaway script, not computed at runtime.
//! Ported from libopus (BSD-3-Clause), see NOTICE. Owner (wave 1): "CeltSynthesis".

// Tables and constants transcribed from libopus keep the reference decimal text.
#![allow(clippy::excessive_precision, clippy::approx_constant)]

/// A complex sample, C: `kiss_fft_cpx` (float build: `{f32, f32}`).
pub type Complex = symphonia_core::dsp::complex::Complex32;

/// C: `kiss_fft_state`. Owns a shared reference into the appropriate slice of the twiddle/
/// bit-reversal tables transcribed from `static_modes_float.h`; `shift` lets the four MDCT
/// short-block FFT sizes (480/240/120/60) all share the single 480-entry twiddle table
/// (matching `opus_fft_alloc_twiddles`'s `base`/`shift` mechanism), avoiding four separate
/// tables.
pub struct FftState {
    pub nfft: usize,
    scale: f32,
    /// C: `kiss_fft_state.shift`; `-1` means "not derived from a base" (still just used as the
    /// effective `max(shift, 0)` per `opus_fft_impl`).
    shift: i32,
    /// C: `kiss_fft_state.factors` (radix, remaining-length) pairs, terminated once the
    /// remaining length reaches 1.
    factors: [i16; 16],
    bitrev: &'static [u16],
    twiddles: &'static [Complex],
}

pub(crate) static FFT_TWIDDLES_480: [Complex; 480] = [
    Complex { re: 1.0000000f32, im: -0.0000000f32 },
    Complex { re: 0.99991433f32, im: -0.013089596f32 },
    Complex { re: 0.99965732f32, im: -0.026176948f32 },
    Complex { re: 0.99922904f32, im: -0.039259816f32 },
    Complex { re: 0.99862953f32, im: -0.052335956f32 },
    Complex { re: 0.99785892f32, im: -0.065403129f32 },
    Complex { re: 0.99691733f32, im: -0.078459096f32 },
    Complex { re: 0.99580493f32, im: -0.091501619f32 },
    Complex { re: 0.99452190f32, im: -0.10452846f32 },
    Complex { re: 0.99306846f32, im: -0.11753740f32 },
    Complex { re: 0.99144486f32, im: -0.13052619f32 },
    Complex { re: 0.98965139f32, im: -0.14349262f32 },
    Complex { re: 0.98768834f32, im: -0.15643447f32 },
    Complex { re: 0.98555606f32, im: -0.16934950f32 },
    Complex { re: 0.98325491f32, im: -0.18223553f32 },
    Complex { re: 0.98078528f32, im: -0.19509032f32 },
    Complex { re: 0.97814760f32, im: -0.20791169f32 },
    Complex { re: 0.97534232f32, im: -0.22069744f32 },
    Complex { re: 0.97236992f32, im: -0.23344536f32 },
    Complex { re: 0.96923091f32, im: -0.24615329f32 },
    Complex { re: 0.96592583f32, im: -0.25881905f32 },
    Complex { re: 0.96245524f32, im: -0.27144045f32 },
    Complex { re: 0.95881973f32, im: -0.28401534f32 },
    Complex { re: 0.95501994f32, im: -0.29654157f32 },
    Complex { re: 0.95105652f32, im: -0.30901699f32 },
    Complex { re: 0.94693013f32, im: -0.32143947f32 },
    Complex { re: 0.94264149f32, im: -0.33380686f32 },
    Complex { re: 0.93819134f32, im: -0.34611706f32 },
    Complex { re: 0.93358043f32, im: -0.35836795f32 },
    Complex { re: 0.92880955f32, im: -0.37055744f32 },
    Complex { re: 0.92387953f32, im: -0.38268343f32 },
    Complex { re: 0.91879121f32, im: -0.39474386f32 },
    Complex { re: 0.91354546f32, im: -0.40673664f32 },
    Complex { re: 0.90814317f32, im: -0.41865974f32 },
    Complex { re: 0.90258528f32, im: -0.43051110f32 },
    Complex { re: 0.89687274f32, im: -0.44228869f32 },
    Complex { re: 0.89100652f32, im: -0.45399050f32 },
    Complex { re: 0.88498764f32, im: -0.46561452f32 },
    Complex { re: 0.87881711f32, im: -0.47715876f32 },
    Complex { re: 0.87249601f32, im: -0.48862124f32 },
    Complex { re: 0.86602540f32, im: -0.50000000f32 },
    Complex { re: 0.85940641f32, im: -0.51129309f32 },
    Complex { re: 0.85264016f32, im: -0.52249856f32 },
    Complex { re: 0.84572782f32, im: -0.53361452f32 },
    Complex { re: 0.83867057f32, im: -0.54463904f32 },
    Complex { re: 0.83146961f32, im: -0.55557023f32 },
    Complex { re: 0.82412619f32, im: -0.56640624f32 },
    Complex { re: 0.81664156f32, im: -0.57714519f32 },
    Complex { re: 0.80901699f32, im: -0.58778525f32 },
    Complex { re: 0.80125381f32, im: -0.59832460f32 },
    Complex { re: 0.79335334f32, im: -0.60876143f32 },
    Complex { re: 0.78531693f32, im: -0.61909395f32 },
    Complex { re: 0.77714596f32, im: -0.62932039f32 },
    Complex { re: 0.76884183f32, im: -0.63943900f32 },
    Complex { re: 0.76040597f32, im: -0.64944805f32 },
    Complex { re: 0.75183981f32, im: -0.65934582f32 },
    Complex { re: 0.74314483f32, im: -0.66913061f32 },
    Complex { re: 0.73432251f32, im: -0.67880075f32 },
    Complex { re: 0.72537437f32, im: -0.68835458f32 },
    Complex { re: 0.71630194f32, im: -0.69779046f32 },
    Complex { re: 0.70710678f32, im: -0.70710678f32 },
    Complex { re: 0.69779046f32, im: -0.71630194f32 },
    Complex { re: 0.68835458f32, im: -0.72537437f32 },
    Complex { re: 0.67880075f32, im: -0.73432251f32 },
    Complex { re: 0.66913061f32, im: -0.74314483f32 },
    Complex { re: 0.65934582f32, im: -0.75183981f32 },
    Complex { re: 0.64944805f32, im: -0.76040597f32 },
    Complex { re: 0.63943900f32, im: -0.76884183f32 },
    Complex { re: 0.62932039f32, im: -0.77714596f32 },
    Complex { re: 0.61909395f32, im: -0.78531693f32 },
    Complex { re: 0.60876143f32, im: -0.79335334f32 },
    Complex { re: 0.59832460f32, im: -0.80125381f32 },
    Complex { re: 0.58778525f32, im: -0.80901699f32 },
    Complex { re: 0.57714519f32, im: -0.81664156f32 },
    Complex { re: 0.56640624f32, im: -0.82412619f32 },
    Complex { re: 0.55557023f32, im: -0.83146961f32 },
    Complex { re: 0.54463904f32, im: -0.83867057f32 },
    Complex { re: 0.53361452f32, im: -0.84572782f32 },
    Complex { re: 0.52249856f32, im: -0.85264016f32 },
    Complex { re: 0.51129309f32, im: -0.85940641f32 },
    Complex { re: 0.50000000f32, im: -0.86602540f32 },
    Complex { re: 0.48862124f32, im: -0.87249601f32 },
    Complex { re: 0.47715876f32, im: -0.87881711f32 },
    Complex { re: 0.46561452f32, im: -0.88498764f32 },
    Complex { re: 0.45399050f32, im: -0.89100652f32 },
    Complex { re: 0.44228869f32, im: -0.89687274f32 },
    Complex { re: 0.43051110f32, im: -0.90258528f32 },
    Complex { re: 0.41865974f32, im: -0.90814317f32 },
    Complex { re: 0.40673664f32, im: -0.91354546f32 },
    Complex { re: 0.39474386f32, im: -0.91879121f32 },
    Complex { re: 0.38268343f32, im: -0.92387953f32 },
    Complex { re: 0.37055744f32, im: -0.92880955f32 },
    Complex { re: 0.35836795f32, im: -0.93358043f32 },
    Complex { re: 0.34611706f32, im: -0.93819134f32 },
    Complex { re: 0.33380686f32, im: -0.94264149f32 },
    Complex { re: 0.32143947f32, im: -0.94693013f32 },
    Complex { re: 0.30901699f32, im: -0.95105652f32 },
    Complex { re: 0.29654157f32, im: -0.95501994f32 },
    Complex { re: 0.28401534f32, im: -0.95881973f32 },
    Complex { re: 0.27144045f32, im: -0.96245524f32 },
    Complex { re: 0.25881905f32, im: -0.96592583f32 },
    Complex { re: 0.24615329f32, im: -0.96923091f32 },
    Complex { re: 0.23344536f32, im: -0.97236992f32 },
    Complex { re: 0.22069744f32, im: -0.97534232f32 },
    Complex { re: 0.20791169f32, im: -0.97814760f32 },
    Complex { re: 0.19509032f32, im: -0.98078528f32 },
    Complex { re: 0.18223553f32, im: -0.98325491f32 },
    Complex { re: 0.16934950f32, im: -0.98555606f32 },
    Complex { re: 0.15643447f32, im: -0.98768834f32 },
    Complex { re: 0.14349262f32, im: -0.98965139f32 },
    Complex { re: 0.13052619f32, im: -0.99144486f32 },
    Complex { re: 0.11753740f32, im: -0.99306846f32 },
    Complex { re: 0.10452846f32, im: -0.99452190f32 },
    Complex { re: 0.091501619f32, im: -0.99580493f32 },
    Complex { re: 0.078459096f32, im: -0.99691733f32 },
    Complex { re: 0.065403129f32, im: -0.99785892f32 },
    Complex { re: 0.052335956f32, im: -0.99862953f32 },
    Complex { re: 0.039259816f32, im: -0.99922904f32 },
    Complex { re: 0.026176948f32, im: -0.99965732f32 },
    Complex { re: 0.013089596f32, im: -0.99991433f32 },
    Complex { re: 6.1230318e-17f32, im: -1.0000000f32 },
    Complex { re: -0.013089596f32, im: -0.99991433f32 },
    Complex { re: -0.026176948f32, im: -0.99965732f32 },
    Complex { re: -0.039259816f32, im: -0.99922904f32 },
    Complex { re: -0.052335956f32, im: -0.99862953f32 },
    Complex { re: -0.065403129f32, im: -0.99785892f32 },
    Complex { re: -0.078459096f32, im: -0.99691733f32 },
    Complex { re: -0.091501619f32, im: -0.99580493f32 },
    Complex { re: -0.10452846f32, im: -0.99452190f32 },
    Complex { re: -0.11753740f32, im: -0.99306846f32 },
    Complex { re: -0.13052619f32, im: -0.99144486f32 },
    Complex { re: -0.14349262f32, im: -0.98965139f32 },
    Complex { re: -0.15643447f32, im: -0.98768834f32 },
    Complex { re: -0.16934950f32, im: -0.98555606f32 },
    Complex { re: -0.18223553f32, im: -0.98325491f32 },
    Complex { re: -0.19509032f32, im: -0.98078528f32 },
    Complex { re: -0.20791169f32, im: -0.97814760f32 },
    Complex { re: -0.22069744f32, im: -0.97534232f32 },
    Complex { re: -0.23344536f32, im: -0.97236992f32 },
    Complex { re: -0.24615329f32, im: -0.96923091f32 },
    Complex { re: -0.25881905f32, im: -0.96592583f32 },
    Complex { re: -0.27144045f32, im: -0.96245524f32 },
    Complex { re: -0.28401534f32, im: -0.95881973f32 },
    Complex { re: -0.29654157f32, im: -0.95501994f32 },
    Complex { re: -0.30901699f32, im: -0.95105652f32 },
    Complex { re: -0.32143947f32, im: -0.94693013f32 },
    Complex { re: -0.33380686f32, im: -0.94264149f32 },
    Complex { re: -0.34611706f32, im: -0.93819134f32 },
    Complex { re: -0.35836795f32, im: -0.93358043f32 },
    Complex { re: -0.37055744f32, im: -0.92880955f32 },
    Complex { re: -0.38268343f32, im: -0.92387953f32 },
    Complex { re: -0.39474386f32, im: -0.91879121f32 },
    Complex { re: -0.40673664f32, im: -0.91354546f32 },
    Complex { re: -0.41865974f32, im: -0.90814317f32 },
    Complex { re: -0.43051110f32, im: -0.90258528f32 },
    Complex { re: -0.44228869f32, im: -0.89687274f32 },
    Complex { re: -0.45399050f32, im: -0.89100652f32 },
    Complex { re: -0.46561452f32, im: -0.88498764f32 },
    Complex { re: -0.47715876f32, im: -0.87881711f32 },
    Complex { re: -0.48862124f32, im: -0.87249601f32 },
    Complex { re: -0.50000000f32, im: -0.86602540f32 },
    Complex { re: -0.51129309f32, im: -0.85940641f32 },
    Complex { re: -0.52249856f32, im: -0.85264016f32 },
    Complex { re: -0.53361452f32, im: -0.84572782f32 },
    Complex { re: -0.54463904f32, im: -0.83867057f32 },
    Complex { re: -0.55557023f32, im: -0.83146961f32 },
    Complex { re: -0.56640624f32, im: -0.82412619f32 },
    Complex { re: -0.57714519f32, im: -0.81664156f32 },
    Complex { re: -0.58778525f32, im: -0.80901699f32 },
    Complex { re: -0.59832460f32, im: -0.80125381f32 },
    Complex { re: -0.60876143f32, im: -0.79335334f32 },
    Complex { re: -0.61909395f32, im: -0.78531693f32 },
    Complex { re: -0.62932039f32, im: -0.77714596f32 },
    Complex { re: -0.63943900f32, im: -0.76884183f32 },
    Complex { re: -0.64944805f32, im: -0.76040597f32 },
    Complex { re: -0.65934582f32, im: -0.75183981f32 },
    Complex { re: -0.66913061f32, im: -0.74314483f32 },
    Complex { re: -0.67880075f32, im: -0.73432251f32 },
    Complex { re: -0.68835458f32, im: -0.72537437f32 },
    Complex { re: -0.69779046f32, im: -0.71630194f32 },
    Complex { re: -0.70710678f32, im: -0.70710678f32 },
    Complex { re: -0.71630194f32, im: -0.69779046f32 },
    Complex { re: -0.72537437f32, im: -0.68835458f32 },
    Complex { re: -0.73432251f32, im: -0.67880075f32 },
    Complex { re: -0.74314483f32, im: -0.66913061f32 },
    Complex { re: -0.75183981f32, im: -0.65934582f32 },
    Complex { re: -0.76040597f32, im: -0.64944805f32 },
    Complex { re: -0.76884183f32, im: -0.63943900f32 },
    Complex { re: -0.77714596f32, im: -0.62932039f32 },
    Complex { re: -0.78531693f32, im: -0.61909395f32 },
    Complex { re: -0.79335334f32, im: -0.60876143f32 },
    Complex { re: -0.80125381f32, im: -0.59832460f32 },
    Complex { re: -0.80901699f32, im: -0.58778525f32 },
    Complex { re: -0.81664156f32, im: -0.57714519f32 },
    Complex { re: -0.82412619f32, im: -0.56640624f32 },
    Complex { re: -0.83146961f32, im: -0.55557023f32 },
    Complex { re: -0.83867057f32, im: -0.54463904f32 },
    Complex { re: -0.84572782f32, im: -0.53361452f32 },
    Complex { re: -0.85264016f32, im: -0.52249856f32 },
    Complex { re: -0.85940641f32, im: -0.51129309f32 },
    Complex { re: -0.86602540f32, im: -0.50000000f32 },
    Complex { re: -0.87249601f32, im: -0.48862124f32 },
    Complex { re: -0.87881711f32, im: -0.47715876f32 },
    Complex { re: -0.88498764f32, im: -0.46561452f32 },
    Complex { re: -0.89100652f32, im: -0.45399050f32 },
    Complex { re: -0.89687274f32, im: -0.44228869f32 },
    Complex { re: -0.90258528f32, im: -0.43051110f32 },
    Complex { re: -0.90814317f32, im: -0.41865974f32 },
    Complex { re: -0.91354546f32, im: -0.40673664f32 },
    Complex { re: -0.91879121f32, im: -0.39474386f32 },
    Complex { re: -0.92387953f32, im: -0.38268343f32 },
    Complex { re: -0.92880955f32, im: -0.37055744f32 },
    Complex { re: -0.93358043f32, im: -0.35836795f32 },
    Complex { re: -0.93819134f32, im: -0.34611706f32 },
    Complex { re: -0.94264149f32, im: -0.33380686f32 },
    Complex { re: -0.94693013f32, im: -0.32143947f32 },
    Complex { re: -0.95105652f32, im: -0.30901699f32 },
    Complex { re: -0.95501994f32, im: -0.29654157f32 },
    Complex { re: -0.95881973f32, im: -0.28401534f32 },
    Complex { re: -0.96245524f32, im: -0.27144045f32 },
    Complex { re: -0.96592583f32, im: -0.25881905f32 },
    Complex { re: -0.96923091f32, im: -0.24615329f32 },
    Complex { re: -0.97236992f32, im: -0.23344536f32 },
    Complex { re: -0.97534232f32, im: -0.22069744f32 },
    Complex { re: -0.97814760f32, im: -0.20791169f32 },
    Complex { re: -0.98078528f32, im: -0.19509032f32 },
    Complex { re: -0.98325491f32, im: -0.18223553f32 },
    Complex { re: -0.98555606f32, im: -0.16934950f32 },
    Complex { re: -0.98768834f32, im: -0.15643447f32 },
    Complex { re: -0.98965139f32, im: -0.14349262f32 },
    Complex { re: -0.99144486f32, im: -0.13052619f32 },
    Complex { re: -0.99306846f32, im: -0.11753740f32 },
    Complex { re: -0.99452190f32, im: -0.10452846f32 },
    Complex { re: -0.99580493f32, im: -0.091501619f32 },
    Complex { re: -0.99691733f32, im: -0.078459096f32 },
    Complex { re: -0.99785892f32, im: -0.065403129f32 },
    Complex { re: -0.99862953f32, im: -0.052335956f32 },
    Complex { re: -0.99922904f32, im: -0.039259816f32 },
    Complex { re: -0.99965732f32, im: -0.026176948f32 },
    Complex { re: -0.99991433f32, im: -0.013089596f32 },
    Complex { re: -1.0000000f32, im: -1.2246064e-16f32 },
    Complex { re: -0.99991433f32, im: 0.013089596f32 },
    Complex { re: -0.99965732f32, im: 0.026176948f32 },
    Complex { re: -0.99922904f32, im: 0.039259816f32 },
    Complex { re: -0.99862953f32, im: 0.052335956f32 },
    Complex { re: -0.99785892f32, im: 0.065403129f32 },
    Complex { re: -0.99691733f32, im: 0.078459096f32 },
    Complex { re: -0.99580493f32, im: 0.091501619f32 },
    Complex { re: -0.99452190f32, im: 0.10452846f32 },
    Complex { re: -0.99306846f32, im: 0.11753740f32 },
    Complex { re: -0.99144486f32, im: 0.13052619f32 },
    Complex { re: -0.98965139f32, im: 0.14349262f32 },
    Complex { re: -0.98768834f32, im: 0.15643447f32 },
    Complex { re: -0.98555606f32, im: 0.16934950f32 },
    Complex { re: -0.98325491f32, im: 0.18223553f32 },
    Complex { re: -0.98078528f32, im: 0.19509032f32 },
    Complex { re: -0.97814760f32, im: 0.20791169f32 },
    Complex { re: -0.97534232f32, im: 0.22069744f32 },
    Complex { re: -0.97236992f32, im: 0.23344536f32 },
    Complex { re: -0.96923091f32, im: 0.24615329f32 },
    Complex { re: -0.96592583f32, im: 0.25881905f32 },
    Complex { re: -0.96245524f32, im: 0.27144045f32 },
    Complex { re: -0.95881973f32, im: 0.28401534f32 },
    Complex { re: -0.95501994f32, im: 0.29654157f32 },
    Complex { re: -0.95105652f32, im: 0.30901699f32 },
    Complex { re: -0.94693013f32, im: 0.32143947f32 },
    Complex { re: -0.94264149f32, im: 0.33380686f32 },
    Complex { re: -0.93819134f32, im: 0.34611706f32 },
    Complex { re: -0.93358043f32, im: 0.35836795f32 },
    Complex { re: -0.92880955f32, im: 0.37055744f32 },
    Complex { re: -0.92387953f32, im: 0.38268343f32 },
    Complex { re: -0.91879121f32, im: 0.39474386f32 },
    Complex { re: -0.91354546f32, im: 0.40673664f32 },
    Complex { re: -0.90814317f32, im: 0.41865974f32 },
    Complex { re: -0.90258528f32, im: 0.43051110f32 },
    Complex { re: -0.89687274f32, im: 0.44228869f32 },
    Complex { re: -0.89100652f32, im: 0.45399050f32 },
    Complex { re: -0.88498764f32, im: 0.46561452f32 },
    Complex { re: -0.87881711f32, im: 0.47715876f32 },
    Complex { re: -0.87249601f32, im: 0.48862124f32 },
    Complex { re: -0.86602540f32, im: 0.50000000f32 },
    Complex { re: -0.85940641f32, im: 0.51129309f32 },
    Complex { re: -0.85264016f32, im: 0.52249856f32 },
    Complex { re: -0.84572782f32, im: 0.53361452f32 },
    Complex { re: -0.83867057f32, im: 0.54463904f32 },
    Complex { re: -0.83146961f32, im: 0.55557023f32 },
    Complex { re: -0.82412619f32, im: 0.56640624f32 },
    Complex { re: -0.81664156f32, im: 0.57714519f32 },
    Complex { re: -0.80901699f32, im: 0.58778525f32 },
    Complex { re: -0.80125381f32, im: 0.59832460f32 },
    Complex { re: -0.79335334f32, im: 0.60876143f32 },
    Complex { re: -0.78531693f32, im: 0.61909395f32 },
    Complex { re: -0.77714596f32, im: 0.62932039f32 },
    Complex { re: -0.76884183f32, im: 0.63943900f32 },
    Complex { re: -0.76040597f32, im: 0.64944805f32 },
    Complex { re: -0.75183981f32, im: 0.65934582f32 },
    Complex { re: -0.74314483f32, im: 0.66913061f32 },
    Complex { re: -0.73432251f32, im: 0.67880075f32 },
    Complex { re: -0.72537437f32, im: 0.68835458f32 },
    Complex { re: -0.71630194f32, im: 0.69779046f32 },
    Complex { re: -0.70710678f32, im: 0.70710678f32 },
    Complex { re: -0.69779046f32, im: 0.71630194f32 },
    Complex { re: -0.68835458f32, im: 0.72537437f32 },
    Complex { re: -0.67880075f32, im: 0.73432251f32 },
    Complex { re: -0.66913061f32, im: 0.74314483f32 },
    Complex { re: -0.65934582f32, im: 0.75183981f32 },
    Complex { re: -0.64944805f32, im: 0.76040597f32 },
    Complex { re: -0.63943900f32, im: 0.76884183f32 },
    Complex { re: -0.62932039f32, im: 0.77714596f32 },
    Complex { re: -0.61909395f32, im: 0.78531693f32 },
    Complex { re: -0.60876143f32, im: 0.79335334f32 },
    Complex { re: -0.59832460f32, im: 0.80125381f32 },
    Complex { re: -0.58778525f32, im: 0.80901699f32 },
    Complex { re: -0.57714519f32, im: 0.81664156f32 },
    Complex { re: -0.56640624f32, im: 0.82412619f32 },
    Complex { re: -0.55557023f32, im: 0.83146961f32 },
    Complex { re: -0.54463904f32, im: 0.83867057f32 },
    Complex { re: -0.53361452f32, im: 0.84572782f32 },
    Complex { re: -0.52249856f32, im: 0.85264016f32 },
    Complex { re: -0.51129309f32, im: 0.85940641f32 },
    Complex { re: -0.50000000f32, im: 0.86602540f32 },
    Complex { re: -0.48862124f32, im: 0.87249601f32 },
    Complex { re: -0.47715876f32, im: 0.87881711f32 },
    Complex { re: -0.46561452f32, im: 0.88498764f32 },
    Complex { re: -0.45399050f32, im: 0.89100652f32 },
    Complex { re: -0.44228869f32, im: 0.89687274f32 },
    Complex { re: -0.43051110f32, im: 0.90258528f32 },
    Complex { re: -0.41865974f32, im: 0.90814317f32 },
    Complex { re: -0.40673664f32, im: 0.91354546f32 },
    Complex { re: -0.39474386f32, im: 0.91879121f32 },
    Complex { re: -0.38268343f32, im: 0.92387953f32 },
    Complex { re: -0.37055744f32, im: 0.92880955f32 },
    Complex { re: -0.35836795f32, im: 0.93358043f32 },
    Complex { re: -0.34611706f32, im: 0.93819134f32 },
    Complex { re: -0.33380686f32, im: 0.94264149f32 },
    Complex { re: -0.32143947f32, im: 0.94693013f32 },
    Complex { re: -0.30901699f32, im: 0.95105652f32 },
    Complex { re: -0.29654157f32, im: 0.95501994f32 },
    Complex { re: -0.28401534f32, im: 0.95881973f32 },
    Complex { re: -0.27144045f32, im: 0.96245524f32 },
    Complex { re: -0.25881905f32, im: 0.96592583f32 },
    Complex { re: -0.24615329f32, im: 0.96923091f32 },
    Complex { re: -0.23344536f32, im: 0.97236992f32 },
    Complex { re: -0.22069744f32, im: 0.97534232f32 },
    Complex { re: -0.20791169f32, im: 0.97814760f32 },
    Complex { re: -0.19509032f32, im: 0.98078528f32 },
    Complex { re: -0.18223553f32, im: 0.98325491f32 },
    Complex { re: -0.16934950f32, im: 0.98555606f32 },
    Complex { re: -0.15643447f32, im: 0.98768834f32 },
    Complex { re: -0.14349262f32, im: 0.98965139f32 },
    Complex { re: -0.13052619f32, im: 0.99144486f32 },
    Complex { re: -0.11753740f32, im: 0.99306846f32 },
    Complex { re: -0.10452846f32, im: 0.99452190f32 },
    Complex { re: -0.091501619f32, im: 0.99580493f32 },
    Complex { re: -0.078459096f32, im: 0.99691733f32 },
    Complex { re: -0.065403129f32, im: 0.99785892f32 },
    Complex { re: -0.052335956f32, im: 0.99862953f32 },
    Complex { re: -0.039259816f32, im: 0.99922904f32 },
    Complex { re: -0.026176948f32, im: 0.99965732f32 },
    Complex { re: -0.013089596f32, im: 0.99991433f32 },
    Complex { re: -1.8369095e-16f32, im: 1.0000000f32 },
    Complex { re: 0.013089596f32, im: 0.99991433f32 },
    Complex { re: 0.026176948f32, im: 0.99965732f32 },
    Complex { re: 0.039259816f32, im: 0.99922904f32 },
    Complex { re: 0.052335956f32, im: 0.99862953f32 },
    Complex { re: 0.065403129f32, im: 0.99785892f32 },
    Complex { re: 0.078459096f32, im: 0.99691733f32 },
    Complex { re: 0.091501619f32, im: 0.99580493f32 },
    Complex { re: 0.10452846f32, im: 0.99452190f32 },
    Complex { re: 0.11753740f32, im: 0.99306846f32 },
    Complex { re: 0.13052619f32, im: 0.99144486f32 },
    Complex { re: 0.14349262f32, im: 0.98965139f32 },
    Complex { re: 0.15643447f32, im: 0.98768834f32 },
    Complex { re: 0.16934950f32, im: 0.98555606f32 },
    Complex { re: 0.18223553f32, im: 0.98325491f32 },
    Complex { re: 0.19509032f32, im: 0.98078528f32 },
    Complex { re: 0.20791169f32, im: 0.97814760f32 },
    Complex { re: 0.22069744f32, im: 0.97534232f32 },
    Complex { re: 0.23344536f32, im: 0.97236992f32 },
    Complex { re: 0.24615329f32, im: 0.96923091f32 },
    Complex { re: 0.25881905f32, im: 0.96592583f32 },
    Complex { re: 0.27144045f32, im: 0.96245524f32 },
    Complex { re: 0.28401534f32, im: 0.95881973f32 },
    Complex { re: 0.29654157f32, im: 0.95501994f32 },
    Complex { re: 0.30901699f32, im: 0.95105652f32 },
    Complex { re: 0.32143947f32, im: 0.94693013f32 },
    Complex { re: 0.33380686f32, im: 0.94264149f32 },
    Complex { re: 0.34611706f32, im: 0.93819134f32 },
    Complex { re: 0.35836795f32, im: 0.93358043f32 },
    Complex { re: 0.37055744f32, im: 0.92880955f32 },
    Complex { re: 0.38268343f32, im: 0.92387953f32 },
    Complex { re: 0.39474386f32, im: 0.91879121f32 },
    Complex { re: 0.40673664f32, im: 0.91354546f32 },
    Complex { re: 0.41865974f32, im: 0.90814317f32 },
    Complex { re: 0.43051110f32, im: 0.90258528f32 },
    Complex { re: 0.44228869f32, im: 0.89687274f32 },
    Complex { re: 0.45399050f32, im: 0.89100652f32 },
    Complex { re: 0.46561452f32, im: 0.88498764f32 },
    Complex { re: 0.47715876f32, im: 0.87881711f32 },
    Complex { re: 0.48862124f32, im: 0.87249601f32 },
    Complex { re: 0.50000000f32, im: 0.86602540f32 },
    Complex { re: 0.51129309f32, im: 0.85940641f32 },
    Complex { re: 0.52249856f32, im: 0.85264016f32 },
    Complex { re: 0.53361452f32, im: 0.84572782f32 },
    Complex { re: 0.54463904f32, im: 0.83867057f32 },
    Complex { re: 0.55557023f32, im: 0.83146961f32 },
    Complex { re: 0.56640624f32, im: 0.82412619f32 },
    Complex { re: 0.57714519f32, im: 0.81664156f32 },
    Complex { re: 0.58778525f32, im: 0.80901699f32 },
    Complex { re: 0.59832460f32, im: 0.80125381f32 },
    Complex { re: 0.60876143f32, im: 0.79335334f32 },
    Complex { re: 0.61909395f32, im: 0.78531693f32 },
    Complex { re: 0.62932039f32, im: 0.77714596f32 },
    Complex { re: 0.63943900f32, im: 0.76884183f32 },
    Complex { re: 0.64944805f32, im: 0.76040597f32 },
    Complex { re: 0.65934582f32, im: 0.75183981f32 },
    Complex { re: 0.66913061f32, im: 0.74314483f32 },
    Complex { re: 0.67880075f32, im: 0.73432251f32 },
    Complex { re: 0.68835458f32, im: 0.72537437f32 },
    Complex { re: 0.69779046f32, im: 0.71630194f32 },
    Complex { re: 0.70710678f32, im: 0.70710678f32 },
    Complex { re: 0.71630194f32, im: 0.69779046f32 },
    Complex { re: 0.72537437f32, im: 0.68835458f32 },
    Complex { re: 0.73432251f32, im: 0.67880075f32 },
    Complex { re: 0.74314483f32, im: 0.66913061f32 },
    Complex { re: 0.75183981f32, im: 0.65934582f32 },
    Complex { re: 0.76040597f32, im: 0.64944805f32 },
    Complex { re: 0.76884183f32, im: 0.63943900f32 },
    Complex { re: 0.77714596f32, im: 0.62932039f32 },
    Complex { re: 0.78531693f32, im: 0.61909395f32 },
    Complex { re: 0.79335334f32, im: 0.60876143f32 },
    Complex { re: 0.80125381f32, im: 0.59832460f32 },
    Complex { re: 0.80901699f32, im: 0.58778525f32 },
    Complex { re: 0.81664156f32, im: 0.57714519f32 },
    Complex { re: 0.82412619f32, im: 0.56640624f32 },
    Complex { re: 0.83146961f32, im: 0.55557023f32 },
    Complex { re: 0.83867057f32, im: 0.54463904f32 },
    Complex { re: 0.84572782f32, im: 0.53361452f32 },
    Complex { re: 0.85264016f32, im: 0.52249856f32 },
    Complex { re: 0.85940641f32, im: 0.51129309f32 },
    Complex { re: 0.86602540f32, im: 0.50000000f32 },
    Complex { re: 0.87249601f32, im: 0.48862124f32 },
    Complex { re: 0.87881711f32, im: 0.47715876f32 },
    Complex { re: 0.88498764f32, im: 0.46561452f32 },
    Complex { re: 0.89100652f32, im: 0.45399050f32 },
    Complex { re: 0.89687274f32, im: 0.44228869f32 },
    Complex { re: 0.90258528f32, im: 0.43051110f32 },
    Complex { re: 0.90814317f32, im: 0.41865974f32 },
    Complex { re: 0.91354546f32, im: 0.40673664f32 },
    Complex { re: 0.91879121f32, im: 0.39474386f32 },
    Complex { re: 0.92387953f32, im: 0.38268343f32 },
    Complex { re: 0.92880955f32, im: 0.37055744f32 },
    Complex { re: 0.93358043f32, im: 0.35836795f32 },
    Complex { re: 0.93819134f32, im: 0.34611706f32 },
    Complex { re: 0.94264149f32, im: 0.33380686f32 },
    Complex { re: 0.94693013f32, im: 0.32143947f32 },
    Complex { re: 0.95105652f32, im: 0.30901699f32 },
    Complex { re: 0.95501994f32, im: 0.29654157f32 },
    Complex { re: 0.95881973f32, im: 0.28401534f32 },
    Complex { re: 0.96245524f32, im: 0.27144045f32 },
    Complex { re: 0.96592583f32, im: 0.25881905f32 },
    Complex { re: 0.96923091f32, im: 0.24615329f32 },
    Complex { re: 0.97236992f32, im: 0.23344536f32 },
    Complex { re: 0.97534232f32, im: 0.22069744f32 },
    Complex { re: 0.97814760f32, im: 0.20791169f32 },
    Complex { re: 0.98078528f32, im: 0.19509032f32 },
    Complex { re: 0.98325491f32, im: 0.18223553f32 },
    Complex { re: 0.98555606f32, im: 0.16934950f32 },
    Complex { re: 0.98768834f32, im: 0.15643447f32 },
    Complex { re: 0.98965139f32, im: 0.14349262f32 },
    Complex { re: 0.99144486f32, im: 0.13052619f32 },
    Complex { re: 0.99306846f32, im: 0.11753740f32 },
    Complex { re: 0.99452190f32, im: 0.10452846f32 },
    Complex { re: 0.99580493f32, im: 0.091501619f32 },
    Complex { re: 0.99691733f32, im: 0.078459096f32 },
    Complex { re: 0.99785892f32, im: 0.065403129f32 },
    Complex { re: 0.99862953f32, im: 0.052335956f32 },
    Complex { re: 0.99922904f32, im: 0.039259816f32 },
    Complex { re: 0.99965732f32, im: 0.026176948f32 },
    Complex { re: 0.99991433f32, im: 0.013089596f32 },
];

pub(crate) static FFT_BITREV_480: [u16; 480] = [
    0, 96, 192, 288, 384, 32, 128, 224, 320, 416, 64, 160, 256, 352, 448, 8, 104, 200, 296, 392,
    40, 136, 232, 328, 424, 72, 168, 264, 360, 456, 16, 112, 208, 304, 400, 48, 144, 240, 336, 432,
    80, 176, 272, 368, 464, 24, 120, 216, 312, 408, 56, 152, 248, 344, 440, 88, 184, 280, 376, 472,
    4, 100, 196, 292, 388, 36, 132, 228, 324, 420, 68, 164, 260, 356, 452, 12, 108, 204, 300, 396,
    44, 140, 236, 332, 428, 76, 172, 268, 364, 460, 20, 116, 212, 308, 404, 52, 148, 244, 340, 436,
    84, 180, 276, 372, 468, 28, 124, 220, 316, 412, 60, 156, 252, 348, 444, 92, 188, 284, 380, 476,
    1, 97, 193, 289, 385, 33, 129, 225, 321, 417, 65, 161, 257, 353, 449, 9, 105, 201, 297, 393,
    41, 137, 233, 329, 425, 73, 169, 265, 361, 457, 17, 113, 209, 305, 401, 49, 145, 241, 337, 433,
    81, 177, 273, 369, 465, 25, 121, 217, 313, 409, 57, 153, 249, 345, 441, 89, 185, 281, 377, 473,
    5, 101, 197, 293, 389, 37, 133, 229, 325, 421, 69, 165, 261, 357, 453, 13, 109, 205, 301, 397,
    45, 141, 237, 333, 429, 77, 173, 269, 365, 461, 21, 117, 213, 309, 405, 53, 149, 245, 341, 437,
    85, 181, 277, 373, 469, 29, 125, 221, 317, 413, 61, 157, 253, 349, 445, 93, 189, 285, 381, 477,
    2, 98, 194, 290, 386, 34, 130, 226, 322, 418, 66, 162, 258, 354, 450, 10, 106, 202, 298, 394,
    42, 138, 234, 330, 426, 74, 170, 266, 362, 458, 18, 114, 210, 306, 402, 50, 146, 242, 338, 434,
    82, 178, 274, 370, 466, 26, 122, 218, 314, 410, 58, 154, 250, 346, 442, 90, 186, 282, 378, 474,
    6, 102, 198, 294, 390, 38, 134, 230, 326, 422, 70, 166, 262, 358, 454, 14, 110, 206, 302, 398,
    46, 142, 238, 334, 430, 78, 174, 270, 366, 462, 22, 118, 214, 310, 406, 54, 150, 246, 342, 438,
    86, 182, 278, 374, 470, 30, 126, 222, 318, 414, 62, 158, 254, 350, 446, 94, 190, 286, 382, 478,
    3, 99, 195, 291, 387, 35, 131, 227, 323, 419, 67, 163, 259, 355, 451, 11, 107, 203, 299, 395,
    43, 139, 235, 331, 427, 75, 171, 267, 363, 459, 19, 115, 211, 307, 403, 51, 147, 243, 339, 435,
    83, 179, 275, 371, 467, 27, 123, 219, 315, 411, 59, 155, 251, 347, 443, 91, 187, 283, 379, 475,
    7, 103, 199, 295, 391, 39, 135, 231, 327, 423, 71, 167, 263, 359, 455, 15, 111, 207, 303, 399,
    47, 143, 239, 335, 431, 79, 175, 271, 367, 463, 23, 119, 215, 311, 407, 55, 151, 247, 343, 439,
    87, 183, 279, 375, 471, 31, 127, 223, 319, 415, 63, 159, 255, 351, 447, 95, 191, 287, 383, 479,
];

pub(crate) static FFT_BITREV_240: [u16; 240] = [
    0, 48, 96, 144, 192, 16, 64, 112, 160, 208, 32, 80, 128, 176, 224, 4, 52, 100, 148, 196, 20,
    68, 116, 164, 212, 36, 84, 132, 180, 228, 8, 56, 104, 152, 200, 24, 72, 120, 168, 216, 40, 88,
    136, 184, 232, 12, 60, 108, 156, 204, 28, 76, 124, 172, 220, 44, 92, 140, 188, 236, 1, 49, 97,
    145, 193, 17, 65, 113, 161, 209, 33, 81, 129, 177, 225, 5, 53, 101, 149, 197, 21, 69, 117, 165,
    213, 37, 85, 133, 181, 229, 9, 57, 105, 153, 201, 25, 73, 121, 169, 217, 41, 89, 137, 185, 233,
    13, 61, 109, 157, 205, 29, 77, 125, 173, 221, 45, 93, 141, 189, 237, 2, 50, 98, 146, 194, 18,
    66, 114, 162, 210, 34, 82, 130, 178, 226, 6, 54, 102, 150, 198, 22, 70, 118, 166, 214, 38, 86,
    134, 182, 230, 10, 58, 106, 154, 202, 26, 74, 122, 170, 218, 42, 90, 138, 186, 234, 14, 62,
    110, 158, 206, 30, 78, 126, 174, 222, 46, 94, 142, 190, 238, 3, 51, 99, 147, 195, 19, 67, 115,
    163, 211, 35, 83, 131, 179, 227, 7, 55, 103, 151, 199, 23, 71, 119, 167, 215, 39, 87, 135, 183,
    231, 11, 59, 107, 155, 203, 27, 75, 123, 171, 219, 43, 91, 139, 187, 235, 15, 63, 111, 159,
    207, 31, 79, 127, 175, 223, 47, 95, 143, 191, 239,
];

pub(crate) static FFT_BITREV_120: [u16; 120] = [
    0, 24, 48, 72, 96, 8, 32, 56, 80, 104, 16, 40, 64, 88, 112, 4, 28, 52, 76, 100, 12, 36, 60, 84,
    108, 20, 44, 68, 92, 116, 1, 25, 49, 73, 97, 9, 33, 57, 81, 105, 17, 41, 65, 89, 113, 5, 29,
    53, 77, 101, 13, 37, 61, 85, 109, 21, 45, 69, 93, 117, 2, 26, 50, 74, 98, 10, 34, 58, 82, 106,
    18, 42, 66, 90, 114, 6, 30, 54, 78, 102, 14, 38, 62, 86, 110, 22, 46, 70, 94, 118, 3, 27, 51,
    75, 99, 11, 35, 59, 83, 107, 19, 43, 67, 91, 115, 7, 31, 55, 79, 103, 15, 39, 63, 87, 111, 23,
    47, 71, 95, 119,
];

pub(crate) static FFT_BITREV_60: [u16; 60] = [
    0, 12, 24, 36, 48, 4, 16, 28, 40, 52, 8, 20, 32, 44, 56, 1, 13, 25, 37, 49, 5, 17, 29, 41, 53,
    9, 21, 33, 45, 57, 2, 14, 26, 38, 50, 6, 18, 30, 42, 54, 10, 22, 34, 46, 58, 3, 15, 27, 39, 51,
    7, 19, 31, 43, 55, 11, 23, 35, 47, 59,
];

// C: `fft_state48000_960_0` (nfft=480, the full-size short-MDCT FFT, LM=0 short blocks).
pub static FFT_STATE_0: FftState = FftState {
    nfft: 480,
    scale: 0.002083333f32,
    shift: -1,
    factors: [5, 96, 3, 32, 4, 8, 2, 4, 4, 1, 0, 0, 0, 0, 0, 0],
    bitrev: &FFT_BITREV_480,
    twiddles: &FFT_TWIDDLES_480,
};

/// C: `fft_state48000_960_1` (nfft=240).
pub static FFT_STATE_1: FftState = FftState {
    nfft: 240,
    scale: 0.004166667f32,
    shift: 1,
    factors: [5, 48, 3, 16, 4, 4, 4, 1, 0, 0, 0, 0, 0, 0, 0, 0],
    bitrev: &FFT_BITREV_240,
    twiddles: &FFT_TWIDDLES_480,
};

/// C: `fft_state48000_960_2` (nfft=120).
pub static FFT_STATE_2: FftState = FftState {
    nfft: 120,
    scale: 0.008333333f32,
    shift: 2,
    factors: [5, 24, 3, 8, 2, 4, 4, 1, 0, 0, 0, 0, 0, 0, 0, 0],
    bitrev: &FFT_BITREV_120,
    twiddles: &FFT_TWIDDLES_480,
};

/// C: `fft_state48000_960_3` (nfft=60).
pub static FFT_STATE_3: FftState = FftState {
    nfft: 60,
    scale: 0.016666667f32,
    shift: 3,
    factors: [5, 12, 3, 4, 4, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    bitrev: &FFT_BITREV_60,
    twiddles: &FFT_TWIDDLES_480,
};

/// C: `kf_bfly2` (float build only has the `m==4` "radix-2 always follows a radix-4" case;
/// the `CUSTOM_MODES`-only `m==1` degenerate case is never compiled for the static mode).
fn kf_bfly2(fout: &mut [Complex], m: usize, n: usize) {
    debug_assert_eq!(m, 4);
    const TW: f32 = 0.707_106_78_f32;
    let mut idx = 0usize;
    for _ in 0..n {
        // Fout2 = Fout + 4.
        let f0 = fout[idx];
        let f1 = fout[idx + 1];
        let f2 = fout[idx + 2];
        let f3 = fout[idx + 3];
        let g0 = fout[idx + 4];
        let g1 = fout[idx + 5];
        let g2 = fout[idx + 6];
        let g3 = fout[idx + 7];

        let t0 = g0;
        fout[idx + 4] = f0 - t0;
        fout[idx] = f0 + t0;

        let t1 = Complex { re: (g1.re + g1.im) * TW, im: (g1.im - g1.re) * TW };
        fout[idx + 5] = f1 - t1;
        fout[idx + 1] = f1 + t1;

        let t2 = Complex { re: g2.im, im: -g2.re };
        fout[idx + 6] = f2 - t2;
        fout[idx + 2] = f2 + t2;

        let t3 = Complex { re: (g3.im - g3.re) * TW, im: -(g3.im + g3.re) * TW };
        fout[idx + 7] = f3 - t3;
        fout[idx + 3] = f3 + t3;

        idx += 8;
    }
}

/// C: `kf_bfly4`.
fn kf_bfly4(
    fout: &mut [Complex],
    fstride: usize,
    twiddles: &[Complex],
    m: usize,
    n: usize,
    mm: usize,
) {
    if m == 1 {
        // Degenerate case where all the twiddles are 1.
        let mut idx = 0usize;
        for _ in 0..n {
            let scratch0 = fout[idx] - fout[idx + 2];
            fout[idx] += fout[idx + 2];
            let scratch1 = fout[idx + 1] + fout[idx + 3];
            fout[idx + 2] = fout[idx] - scratch1;
            fout[idx] += scratch1;
            let scratch1b = fout[idx + 1] - fout[idx + 3];

            fout[idx + 1] =
                Complex { re: scratch0.re + scratch1b.im, im: scratch0.im - scratch1b.re };
            fout[idx + 3] =
                Complex { re: scratch0.re - scratch1b.im, im: scratch0.im + scratch1b.re };
            idx += 4;
        }
    }
    else {
        // The last stage (n == 1) is called with mm == 1, as in libopus.
        debug_assert!(n == 1 || mm == 4 * m);
        for chunk in fout.chunks_exact_mut(4 * m).take(n) {
            let (a, rest) = chunk.split_at_mut(m);
            let (b, rest) = rest.split_at_mut(m);
            let (c, d) = rest.split_at_mut(m);
            let tw1 = twiddles.iter().step_by(fstride);
            let tw2 = twiddles.iter().step_by(fstride * 2);
            let tw3 = twiddles.iter().step_by(fstride * 3);
            for (((((f0, f1), f2), f3), (t1, t2)), t3) in a
                .iter_mut()
                .zip(b.iter_mut())
                .zip(c.iter_mut())
                .zip(d.iter_mut())
                .zip(tw1.zip(tw2))
                .zip(tw3)
            {
                let scratch0 = *f1 * *t1;
                let scratch1 = *f2 * *t2;
                let scratch2 = *f3 * *t3;

                let scratch5 = *f0 - scratch1;
                *f0 += scratch1;
                let scratch3 = scratch0 + scratch2;
                let scratch4 = scratch0 - scratch2;
                *f2 = *f0 - scratch3;
                *f0 += scratch3;

                *f1 = Complex { re: scratch5.re + scratch4.im, im: scratch5.im - scratch4.re };
                *f3 = Complex { re: scratch5.re - scratch4.im, im: scratch5.im + scratch4.re };
            }
        }
    }
}

/// C: `kf_bfly3`.
fn kf_bfly3(
    fout: &mut [Complex],
    fstride: usize,
    twiddles: &[Complex],
    m: usize,
    n: usize,
    mm: usize,
) {
    // C: `epi3 = st->twiddles[fstride*m]` (float build).
    let epi3 = twiddles[fstride * m];
    // The last stage (n == 1) is called with mm == 1, as in libopus.
    debug_assert!(n == 1 || mm == 3 * m);
    for chunk in fout.chunks_exact_mut(3 * m).take(n) {
        let (a, rest) = chunk.split_at_mut(m);
        let (b, c) = rest.split_at_mut(m);
        let tw1 = twiddles.iter().step_by(fstride);
        let tw2 = twiddles.iter().step_by(fstride * 2);
        for (((f0, f1), f2), (t1, t2)) in
            a.iter_mut().zip(b.iter_mut()).zip(c.iter_mut()).zip(tw1.zip(tw2))
        {
            let scratch1 = *f1 * *t1;
            let scratch2 = *f2 * *t2;

            let scratch3 = scratch1 + scratch2;
            let scratch0 = scratch1 - scratch2;

            let fm_r = f0.re - 0.5 * scratch3.re;
            let fm_i = f0.im - 0.5 * scratch3.im;

            let scratch0 = Complex { re: scratch0.re * epi3.im, im: scratch0.im * epi3.im };

            *f0 += scratch3;

            *f2 = Complex { re: fm_r + scratch0.im, im: fm_i - scratch0.re };
            *f1 = Complex { re: fm_r - scratch0.im, im: fm_i + scratch0.re };
        }
    }
}

/// C: `kf_bfly5`.
fn kf_bfly5(
    fout: &mut [Complex],
    fstride: usize,
    twiddles: &[Complex],
    m: usize,
    n: usize,
    mm: usize,
) {
    let ya = twiddles[fstride * m];
    let yb = twiddles[fstride * 2 * m];
    // The last stage (n == 1) is called with mm == 1, as in libopus.
    debug_assert!(n == 1 || mm == 5 * m);
    for chunk in fout.chunks_exact_mut(5 * m).take(n) {
        let (a, rest) = chunk.split_at_mut(m);
        let (b, rest) = rest.split_at_mut(m);
        let (c, rest) = rest.split_at_mut(m);
        let (d, e) = rest.split_at_mut(m);
        for u in 0..m {
            let scratch0 = a[u];
            let scratch1 = b[u] * twiddles[u * fstride];
            let scratch2 = c[u] * twiddles[2 * u * fstride];
            let scratch3 = d[u] * twiddles[3 * u * fstride];
            let scratch4 = e[u] * twiddles[4 * u * fstride];

            let scratch7 = scratch1 + scratch4;
            let scratch10 = scratch1 - scratch4;
            let scratch8 = scratch2 + scratch3;
            let scratch9 = scratch2 - scratch3;

            a[u] = Complex {
                re: scratch0.re + scratch7.re + scratch8.re,
                im: scratch0.im + scratch7.im + scratch8.im,
            };

            let scratch5 = Complex {
                re: scratch0.re + scratch7.re * ya.re + scratch8.re * yb.re,
                im: scratch0.im + scratch7.im * ya.re + scratch8.im * yb.re,
            };
            let scratch6 = Complex {
                re: scratch10.im * ya.im + scratch9.im * yb.im,
                im: -(scratch10.re * ya.im + scratch9.re * yb.im),
            };

            b[u] = scratch5 - scratch6;
            e[u] = scratch5 + scratch6;

            let scratch11 = Complex {
                re: scratch0.re + scratch7.re * yb.re + scratch8.re * ya.re,
                im: scratch0.im + scratch7.im * yb.re + scratch8.im * ya.re,
            };
            let scratch12 = Complex {
                re: scratch9.im * ya.im - scratch10.im * yb.im,
                im: scratch10.re * yb.im - scratch9.re * ya.im,
            };

            c[u] = scratch11 + scratch12;
            d[u] = scratch11 - scratch12;
        }
    }
}

impl FftState {
    /// C: `opus_fft_impl` (the butterfly network only; no bit-reversal or scaling — those are
    /// [`FftState::forward`]/[`FftState::inverse`]'s job, matching `opus_fft_c`/`opus_ifft_c`).
    /// `clt_mdct_backward_c` calls this directly on data it has already bit-reversed itself.
    pub fn fft_impl(&self, fout: &mut [Complex]) {
        let shift = if self.shift > 0 { self.shift as usize } else { 0 };

        let mut fstride = [0usize; 9];
        fstride[0] = 1;
        let mut l = 0usize;
        let mut m;
        loop {
            let p = self.factors[2 * l] as usize;
            m = self.factors[2 * l + 1] as usize;
            fstride[l + 1] = fstride[l] * p;
            l += 1;
            if m == 1 {
                break;
            }
        }
        let mut m = self.factors[2 * l - 1] as usize;
        for i in (0..l).rev() {
            let m2 = if i != 0 { self.factors[2 * i - 1] as usize } else { 1 };
            match self.factors[2 * i] {
                2 => kf_bfly2(fout, m, fstride[i]),
                4 => kf_bfly4(fout, fstride[i] << shift, self.twiddles, m, fstride[i], m2),
                3 => kf_bfly3(fout, fstride[i] << shift, self.twiddles, m, fstride[i], m2),
                5 => kf_bfly5(fout, fstride[i] << shift, self.twiddles, m, fstride[i], m2),
                _ => unreachable!("unsupported FFT radix"),
            }
            m = m2;
        }
    }

    /// C: `opus_fft_c` (bit-reversed input, forward transform, output scaled by `1/nfft`).
    /// Used only by round-trip unit tests; CELT decode only needs [`FftState::fft_impl`]
    /// (via MDCT) and [`FftState::inverse`] is not called by decode either — kept for symmetry
    /// and testability.
    pub fn forward(&self, input: &[Complex], output: &mut [Complex]) {
        assert_eq!(input.len(), self.nfft);
        assert_eq!(output.len(), self.nfft);
        for (x, &rev) in input.iter().zip(self.bitrev.iter()) {
            let rev = rev as usize;
            output[rev] = Complex { re: self.scale * x.re, im: self.scale * x.im };
        }
        self.fft_impl(output);
    }

    /// C: `opus_ifft_c`.
    pub fn inverse(&self, input: &[Complex], output: &mut [Complex]) {
        assert_eq!(input.len(), self.nfft);
        assert_eq!(output.len(), self.nfft);
        for (&x, &rev) in input.iter().zip(self.bitrev.iter()) {
            output[rev as usize] = x;
        }
        for x in output.iter_mut() {
            x.im = -x.im;
        }
        self.fft_impl(output);
        for x in output.iter_mut() {
            x.im = -x.im;
        }
    }
}

impl FftState {
    /// Largest `nfft` among the four static FFT states (`FFT_STATE_0`, nfft=480); sizes the
    /// fixed stack scratch buffer of the MDCT so the decode hot path never allocates.
    pub(crate) const MAX_NFFT: usize = 480;

    /// Crate-visible accessor for `bitrev[i]`, needed by `mdct.rs`'s pre-rotation step (which
    /// bit-reverses directly into its own output buffer rather than calling
    /// [`FftState::forward`]/[`FftState::inverse`]).
    pub(crate) fn bitrev_at(&self, i: usize) -> u16 {
        self.bitrev[i]
    }

    /// Crate-visible accessor for `scale` (`1/nfft`), needed by `mdct.rs`'s forward transform.
    pub(crate) fn scale(&self) -> f32 {
        self.scale
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Naive O(n^2) DFT used as an independent oracle. Matches the sign convention of
    /// `opus_fft_c`/kiss_fft's forward transform: `X[k] = (1/N) * sum_n x[n] * exp(-2*pi*i*k*n/N)`.
    fn naive_dft(x: &[Complex], scale: f32) -> Vec<Complex> {
        let n = x.len();
        let mut out = vec![Complex::default(); n];
        #[allow(clippy::needless_range_loop)]
        // index shared across several buffers, mirrors the C loop
        for k in 0..n {
            let mut r = 0f64;
            let mut i = 0f64;
            for (t, xt) in x.iter().enumerate() {
                let phase = -2.0 * std::f64::consts::PI * (k * t) as f64 / n as f64;
                let (s, c) = phase.sin_cos();
                r += xt.re as f64 * c - xt.im as f64 * s;
                i += xt.re as f64 * s + xt.im as f64 * c;
            }
            out[k] = Complex { re: (r as f32) * scale, im: (i as f32) * scale };
        }
        out
    }

    fn check_forward(state: &FftState, seed: u32) {
        let n = state.nfft;
        let mut x = vec![Complex::default(); n];
        let mut s = seed;
        for c in x.iter_mut() {
            // Simple LCG so the test is deterministic without pulling in `rand`.
            s = s.wrapping_mul(1103515245).wrapping_add(12345);
            let a = (s >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
            s = s.wrapping_mul(1103515245).wrapping_add(12345);
            let b = (s >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
            *c = Complex { re: a, im: b };
        }
        let mut out = vec![Complex::default(); n];
        state.forward(&x, &mut out);
        let expected = naive_dft(&x, state.scale());
        for k in 0..n {
            assert!(
                (out[k].re - expected[k].re).abs() < 1e-3
                    && (out[k].im - expected[k].im).abs() < 1e-3,
                "nfft={n} bin {k}: got {:?}, expected {:?}",
                out[k],
                expected[k]
            );
        }
    }

    #[test]
    fn forward_matches_naive_dft_all_sizes() {
        check_forward(&FFT_STATE_0, 1);
        check_forward(&FFT_STATE_1, 2);
        check_forward(&FFT_STATE_2, 3);
        check_forward(&FFT_STATE_3, 4);
    }

    #[test]
    fn inverse_is_forward_round_trip() {
        for state in [&FFT_STATE_0, &FFT_STATE_1, &FFT_STATE_2, &FFT_STATE_3] {
            let n = state.nfft;
            let mut x = vec![Complex::default(); n];
            let mut s = 42u32;
            for c in x.iter_mut() {
                s = s.wrapping_mul(1103515245).wrapping_add(12345);
                let a = (s >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                s = s.wrapping_mul(1103515245).wrapping_add(12345);
                let b = (s >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                *c = Complex { re: a, im: b };
            }
            let mut fwd = vec![Complex::default(); n];
            state.forward(&x, &mut fwd);
            let mut back = vec![Complex::default(); n];
            state.inverse(&fwd, &mut back);
            // forward scales by 1/n, inverse doesn't rescale, so `inverse(forward(x)) == x`.
            for k in 0..n {
                assert!(
                    (back[k].re - x[k].re).abs() < 1e-3 && (back[k].im - x[k].im).abs() < 1e-3,
                    "nfft={n} bin {k}: round-trip {:?} != original {:?}",
                    back[k],
                    x[k]
                );
            }
        }
    }
}
