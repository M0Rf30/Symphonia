// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! mp3PRO Spectral Band Replication (SBR).
//!
//! The SBR data of an mp3PRO stream is carried in the ancillary data region of every MPEG-2/2.5
//! Layer III frame. [`SbrParser`] decodes it into per-channel parameter sets ([`SbrFrame`]), and
//! [`SbrDsp`] uses them to reconstruct the high band of the core PCM at twice the core rate.

mod bitstream;
mod decoder;
mod dsp;
mod freq;
mod huffman;

pub(crate) use bitstream::SbrParser;
pub(crate) use dsp::SbrDsp;

pub use decoder::Mp3ProDecoder;

/// Maximum number of envelopes in an SBR frame.
pub(crate) const MAX_ENVELOPES: usize = 4;
/// Maximum number of noise-floor envelopes in an SBR frame.
pub(crate) const MAX_NOISE_ENVELOPES: usize = 2;
/// Maximum number of high-resolution frequency bands (`k2 - k0 <= 48`).
pub(crate) const MAX_BANDS_HIGH: usize = 48;
/// Maximum number of low-resolution frequency bands.
pub(crate) const MAX_BANDS_LOW: usize = 24;
/// Maximum number of noise-floor bands.
pub(crate) const MAX_NOISE_BANDS: usize = 5;
/// Maximum number of limiter bands.
pub(crate) const MAX_LIMITER_BANDS: usize = 12;

/// The SBR header fields that affect decoding.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct SbrHeader {
    /// Envelope amplitude resolution: 0 = 1.5 dB, 1 = 3.0 dB.
    pub amp_res: u8,
    pub start_freq: u8,
    pub stop_freq: u8,
    pub xover_band: u8,
    pub freq_scale: u8,
    pub alter_scale: u8,
    pub noise_bands: u8,
    pub limiter_bands: u8,
    pub limiter_gains: u8,
    pub interpol_freq: u8,
    pub smoothing_mode: u8,
}

/// Frequency band tables derived from an [`SbrHeader`] (QMF band indices at the output rate).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct FreqTables {
    /// First SBR band (`f_hi[0]`).
    pub kx: usize,
    /// One past the last SBR band (`f_hi[n_high]`).
    pub k2: usize,
    /// High-resolution band borders, `n_high + 1` valid entries.
    pub f_hi: [u8; MAX_BANDS_HIGH + 1],
    pub n_high: usize,
    /// Low-resolution band borders, `n_low + 1` valid entries.
    pub f_lo: [u8; MAX_BANDS_LOW + 1],
    pub n_low: usize,
    /// Noise-floor band borders, `nq + 1` valid entries.
    pub f_noise: [u8; MAX_NOISE_BANDS + 1],
    pub nq: usize,
    /// Limiter band borders relative to `kx`, `nl + 1` valid entries.
    pub f_lim: [u8; MAX_LIMITER_BANDS + 1],
    pub nl: usize,
}

/// The dequantised SBR parameters of one channel for one frame, ready for the DSP stages.
#[derive(Copy, Clone, Debug)]
pub(crate) struct SbrFrame {
    /// Number of envelopes (1..=4).
    pub num_env: usize,
    /// Envelope borders in time slots, `num_env + 1` valid entries (after the start-border fix-up).
    pub borders: [u8; MAX_ENVELOPES + 1],
    /// Frequency resolution per envelope: true = `f_hi`, false = `f_lo`.
    pub freq_res: [bool; MAX_ENVELOPES],
    /// Transient envelope index (`l_A`), or -1 if none.
    pub l_a: i8,
    /// Number of noise-floor envelopes (1..=2).
    pub num_noise: usize,
    /// Noise-floor envelope borders in time slots, `num_noise + 1` valid entries.
    pub noise_borders: [u8; MAX_NOISE_ENVELOPES + 1],
    /// Linear envelope energies per band of the envelope's frequency table.
    pub env: [[f32; MAX_BANDS_HIGH]; MAX_ENVELOPES],
    /// Linear noise-to-signal ratios per noise band.
    pub noise: [[f32; MAX_NOISE_BANDS]; MAX_NOISE_ENVELOPES],
    /// Inverse-filtering mode of noise band 0 (the only one that reaches the DSP).
    pub invf: u8,
    /// The 3-bit `data_extra` field of the channel element (0 if absent).
    pub data_extra: u8,
}

impl Default for SbrFrame {
    fn default() -> Self {
        SbrFrame {
            num_env: 1,
            borders: [0, 9, 0, 0, 0],
            freq_res: [false; MAX_ENVELOPES],
            l_a: -1,
            num_noise: 1,
            noise_borders: [0, 9, 0],
            env: [[0.0; MAX_BANDS_HIGH]; MAX_ENVELOPES],
            noise: [[0.0; MAX_NOISE_BANDS]; MAX_NOISE_ENVELOPES],
            invf: 0,
            data_extra: 0,
        }
    }
}

/// The result of parsing the ancillary data of one frame.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ParseStatus {
    /// The header (start, stop, xover, freq_scale, alter_scale or noise_bands) changed: the
    /// DSP must be reconfigured with the new header and tables before processing this frame.
    pub header_reset: bool,
    /// Every channel is in the "SBR active" state and its [`SbrFrame`] must be applied. If false,
    /// the frame takes the core-only path (bands >= kx zeroed).
    pub apply: bool,
    /// SBR was switched off permanently (e.g., unsupported protocol). The core-only path is
    /// used for this and every following frame.
    pub disabled: bool,
}
