// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `sbr_data()` bitstream syntax of USAC (ISO/IEC 23003-3 7.3, Tables 20 to 26) for the
//! enhanced SBR (eSBR) subset that has the same signal processing as the SBR of HE-AAC: no
//! harmonic transposition, no predictive vector coding, no inter-subband-sample temporal
//! envelope shaping.
//!
//! The syntax differs from `sbr_extension_data()` of HE-AAC in that the header is split into an
//! info part and an optional header part, a frame has no `bs_data_extra`, the first delta
//! direction flags are implied in an independent frame, and there is no extended data.

use super::bits::{BitReader, bit_readers};
use super::element::{SbrChannel, SbrElement};
use super::envelope::{SbrEnvelopeData, SbrNoiseData};
use super::error::{SbrError as Error, SbrResult as Result};
use super::extension::SbrExtensionData;
use super::freq_bands::HiLoTables;
use super::grid::{GridSyntax, SbrDtdf, SbrGrid, SbrInvf};
use super::header::{
    DEFAULT_ALTER_SCALE, DEFAULT_FREQ_SCALE, DEFAULT_INTERPOL_FREQ, DEFAULT_LIMITER_BANDS,
    DEFAULT_LIMITER_GAINS, DEFAULT_NOISE_BANDS, DEFAULT_SMOOTHING_MODE, SbrHeader,
};

use symphonia_common::mpeg::audio::usac::UsacSbrConfig;

/// The parser of the `sbr_data()` of one USAC SCE or CPE element, which keeps the header that
/// is carried over from frame to frame.
pub(crate) struct UsacSbrParser {
    cfg: UsacSbrConfig,
    fs_sbr: u32,
    header: SbrHeader,
    /// A header was received, so that `bands` is valid.
    active: bool,
    bands: Option<HiLoTables>,
}

impl UsacSbrParser {
    pub fn new(cfg: UsacSbrConfig, fs_sbr: u32) -> Self {
        let d = &cfg.dflt;
        let header = SbrHeader {
            amp_res: false,
            start_freq: d.start_freq,
            stop_freq: d.stop_freq,
            xover_band: 0,
            reserved: 0,
            header_extra_1: d.header_extra1,
            header_extra_2: d.header_extra2,
            freq_scale: d.freq_scale,
            alter_scale: d.alter_scale,
            noise_bands: d.noise_bands,
            limiter_bands: d.limiter_bands,
            limiter_gains: d.limiter_gains,
            interpol_freq: d.interpol_freq,
            smoothing_mode: d.smoothing_mode,
            pre_proc: false,
        };

        UsacSbrParser { cfg, fs_sbr, header, active: false, bands: None }
    }

    /// Forget the header (after a decoder reset, the next frame is an independent one).
    pub fn reset(&mut self) {
        self.active = false;
        self.bands = None;
    }

    /// Parse the `sbr_data()` of a single channel element (`pair == false`) or of a channel
    /// pair element.
    pub fn parse(
        &mut self,
        reader: &mut BitReader<'_>,
        pair: bool,
        independent: bool,
    ) -> Result<SbrExtensionData> {
        let start = reader.bit_position();

        // sbr_info / sbr_header presence (Table 21).
        let (info_present, header_present) = if independent {
            (true, true)
        }
        else if read_flag(reader)? {
            (true, read_flag(reader)?)
        }
        else {
            (false, false)
        };

        let prev = self.header;

        if info_present {
            self.header.amp_res = read_flag(reader)?;
            self.header.xover_band = read(reader, 4)? as u8;
            // bs_sbr_preprocessing
            self.header.pre_proc = read_flag(reader)?;
            if self.cfg.pvc && read(reader, 2)? != 0 {
                return Err(Error::SbrFreqBandInvalid);
            }
        }

        if header_present {
            if read_flag(reader)? {
                // The default header of the SbrConfig().
                let d = &self.cfg.dflt;
                self.header.start_freq = d.start_freq;
                self.header.stop_freq = d.stop_freq;
                self.header.header_extra_1 = d.header_extra1;
                self.header.header_extra_2 = d.header_extra2;
                self.header.freq_scale = d.freq_scale;
                self.header.alter_scale = d.alter_scale;
                self.header.noise_bands = d.noise_bands;
                self.header.limiter_bands = d.limiter_bands;
                self.header.limiter_gains = d.limiter_gains;
                self.header.interpol_freq = d.interpol_freq;
                self.header.smoothing_mode = d.smoothing_mode;
            }
            else {
                self.header.start_freq = read(reader, 4)? as u8;
                self.header.stop_freq = read(reader, 4)? as u8;
                self.header.header_extra_1 = read_flag(reader)?;
                self.header.header_extra_2 = read_flag(reader)?;

                if self.header.header_extra_1 {
                    self.header.freq_scale = read(reader, 2)? as u8;
                    self.header.alter_scale = read_flag(reader)?;
                    self.header.noise_bands = read(reader, 2)? as u8;
                }
                else {
                    self.header.freq_scale = DEFAULT_FREQ_SCALE;
                    self.header.alter_scale = DEFAULT_ALTER_SCALE;
                    self.header.noise_bands = DEFAULT_NOISE_BANDS;
                }

                if self.header.header_extra_2 {
                    self.header.limiter_bands = read(reader, 2)? as u8;
                    self.header.limiter_gains = read(reader, 2)? as u8;
                    self.header.interpol_freq = read_flag(reader)?;
                    self.header.smoothing_mode = read_flag(reader)?;
                }
                else {
                    self.header.limiter_bands = DEFAULT_LIMITER_BANDS;
                    self.header.limiter_gains = DEFAULT_LIMITER_GAINS;
                    self.header.interpol_freq = DEFAULT_INTERPOL_FREQ;
                    self.header.smoothing_mode = DEFAULT_SMOOTHING_MODE;
                }
            }
        }

        if !self.active && !(info_present && header_present) {
            // A stream cannot start without a complete header.
            return Err(Error::SbrFreqBandInvalid);
        }

        if !self.active || self.header.band_geometry_changed(&prev) {
            self.bands = Some(self.header.derive_bands(self.fs_sbr)?);
        }
        self.active = true;

        let Some(bands) = self.bands.as_ref()
        else {
            return Err(Error::SbrFreqBandInvalid);
        };

        let element = if pair {
            parse_pair(reader, bands, self.header.amp_res, independent)?
        }
        else {
            parse_single(reader, bands, self.header.amp_res, independent)?
        };

        Ok(SbrExtensionData {
            crc: None,
            crc_region: None,
            header_present,
            header: self.header,
            element,
            num_sbr_bits: reader.bit_position() - start,
        })
    }
}

/// `sbr_single_channel_element()` (Table 22).
fn parse_single(
    reader: &mut BitReader<'_>,
    bands: &HiLoTables,
    amp_res: bool,
    independent: bool,
) -> Result<SbrElement> {
    let n_q = bands.n_q();

    let grid = SbrGrid::parse_with(reader, GridSyntax::Standard)?;
    let dtdf = SbrDtdf::parse_usac(reader, grid.num_env, grid.num_noise, independent)?;
    let invf = SbrInvf::parse(reader, n_q)?;
    let eff_amp = grid.effective_amp_res(amp_res);

    let envelope = SbrEnvelopeData::parse(reader, &grid, &dtdf, bands, false, false, eff_amp)?;
    let noise = SbrNoiseData::parse(reader, &grid, &dtdf, n_q, false, false, eff_amp)?;
    let add_harmonic = parse_harmonics(reader, bands)?;

    Ok(SbrElement {
        coupling: false,
        channels: vec![SbrChannel { grid, dtdf, invf, envelope, noise, add_harmonic }],
        extension: None,
    })
}

/// `sbr_channel_pair_element()` (Table 23).
fn parse_pair(
    reader: &mut BitReader<'_>,
    bands: &HiLoTables,
    amp_res: bool,
    independent: bool,
) -> Result<SbrElement> {
    let n_q = bands.n_q();
    let coupling = read_flag(reader)?;

    let channels = if coupling {
        let grid = SbrGrid::parse_with(reader, GridSyntax::Standard)?;
        let dtdf0 = SbrDtdf::parse_usac(reader, grid.num_env, grid.num_noise, independent)?;
        let dtdf1 = SbrDtdf::parse_usac(reader, grid.num_env, grid.num_noise, independent)?;
        let invf0 = SbrInvf::parse(reader, n_q)?;
        let eff_amp = grid.effective_amp_res(amp_res);

        let env0 = SbrEnvelopeData::parse(reader, &grid, &dtdf0, bands, true, false, eff_amp)?;
        let noise0 = SbrNoiseData::parse(reader, &grid, &dtdf0, n_q, true, false, eff_amp)?;
        let env1 = SbrEnvelopeData::parse(reader, &grid, &dtdf1, bands, true, true, eff_amp)?;
        let noise1 = SbrNoiseData::parse(reader, &grid, &dtdf1, n_q, true, true, eff_amp)?;

        let h0 = parse_harmonics(reader, bands)?;
        let h1 = parse_harmonics(reader, bands)?;

        vec![
            SbrChannel {
                grid: grid.clone(),
                dtdf: dtdf0,
                invf: invf0,
                envelope: env0,
                noise: noise0,
                add_harmonic: h0,
            },
            SbrChannel {
                grid,
                dtdf: dtdf1,
                invf: SbrInvf { invf_mode: Vec::new() },
                envelope: env1,
                noise: noise1,
                add_harmonic: h1,
            },
        ]
    }
    else {
        let grid0 = SbrGrid::parse_with(reader, GridSyntax::Standard)?;
        let grid1 = SbrGrid::parse_with(reader, GridSyntax::Standard)?;
        let dtdf0 = SbrDtdf::parse_usac(reader, grid0.num_env, grid0.num_noise, independent)?;
        let dtdf1 = SbrDtdf::parse_usac(reader, grid1.num_env, grid1.num_noise, independent)?;
        let invf0 = SbrInvf::parse(reader, n_q)?;
        let invf1 = SbrInvf::parse(reader, n_q)?;

        let eff0 = grid0.effective_amp_res(amp_res);
        let eff1 = grid1.effective_amp_res(amp_res);

        let env0 = SbrEnvelopeData::parse(reader, &grid0, &dtdf0, bands, false, false, eff0)?;
        let env1 = SbrEnvelopeData::parse(reader, &grid1, &dtdf1, bands, false, true, eff1)?;
        let noise0 = SbrNoiseData::parse(reader, &grid0, &dtdf0, n_q, false, false, eff0)?;
        let noise1 = SbrNoiseData::parse(reader, &grid1, &dtdf1, n_q, false, true, eff1)?;

        let h0 = parse_harmonics(reader, bands)?;
        let h1 = parse_harmonics(reader, bands)?;

        vec![
            SbrChannel {
                grid: grid0,
                dtdf: dtdf0,
                invf: invf0,
                envelope: env0,
                noise: noise0,
                add_harmonic: h0,
            },
            SbrChannel {
                grid: grid1,
                dtdf: dtdf1,
                invf: invf1,
                envelope: env1,
                noise: noise1,
                add_harmonic: h1,
            },
        ]
    };

    Ok(SbrElement { coupling, channels, extension: None })
}

/// `bs_add_harmonic_flag` and, if set, the `bs_add_harmonic[]` of the high resolution bands.
fn parse_harmonics(reader: &mut BitReader<'_>, bands: &HiLoTables) -> Result<Vec<bool>> {
    if !read_flag(reader)? {
        return Ok(Vec::new());
    }

    (0..bands.n_high()).map(|_| read_flag(reader)).collect()
}

bit_readers!(Error::SbrHuffInvalid, read, read_flag);
