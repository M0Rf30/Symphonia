// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The enhanced SBR (eSBR) of USAC.
//!
//! The SBR signal processing of HE-AAC is reused for the streams that have the same signal
//! flow: the 2:1, 8:3 and 4:1 SBR ratios, without harmonic transposition, predictive vector coding or
//! inter-subband-sample temporal envelope shaping. The other eSBR configurations are rejected.

use symphonia_core::errors::{Result, unsupported_error};
use symphonia_core::io::{BitReaderLtr, FiniteBitStream, ReadBitsLtr};

use symphonia_common::mpeg::audio::usac::{UsacSbrConfig, UsacSbrRatio};

use crate::aac::sbr::bits::BitReader;
use crate::aac::sbr::decoder::SbrDecoder;
use crate::aac::sbr::extension::SbrExtensionData;
use crate::aac::sbr::usac::UsacSbrParser;
use crate::aac::usac::mps212::Mps212Decoder;

/// The eSBR decoder of a channel pair or single channel.
pub struct EsbrDecoder {
    parser: UsacSbrParser,
    decoder: SbrDecoder,
    channels: usize,
    output_rate: u32,
    analysis_bands: usize,
    pending: Option<SbrExtensionData>,
    core: Vec<Vec<f64>>,
}

impl EsbrDecoder {
    /// `output_rate` is the sampling frequency of the decoded signal and `core_len` the number of
    /// core samples of a frame.
    pub fn new(
        cfg: UsacSbrConfig,
        ratio: UsacSbrRatio,
        channels: usize,
        output_rate: u32,
        core_len: usize,
    ) -> Result<Self> {
        let analysis_bands = match (ratio, core_len) {
            (UsacSbrRatio::Ratio2To1, 1024) => 32,
            (UsacSbrRatio::Ratio8To3, 768) => 24,
            (UsacSbrRatio::Ratio4To1, 1024) => 16,
            _ => return unsupported_error("aac: usac sbr ratio is not yet implemented"),
        };

        if cfg.harmonic_sbr || cfg.pvc || cfg.inter_tes {
            return unsupported_error(
                "aac: usac harmonic sbr, pvc and inter-tes are not yet implemented",
            );
        }

        let decoder = SbrDecoder::new_usac(output_rate, channels, analysis_bands)?;

        Ok(EsbrDecoder {
            parser: UsacSbrParser::new(cfg, output_rate, analysis_bands == 16),
            decoder,
            channels,
            output_rate,
            analysis_bands,
            pending: None,
            core: vec![vec![0.0; core_len]; channels],
        })
    }

    /// Forget the carried-over header and the filterbank history.
    pub fn reset(&mut self) {
        self.parser.reset();
        self.pending = None;

        if let Ok(decoder) =
            SbrDecoder::new_usac(self.output_rate, self.channels, self.analysis_bands)
        {
            self.decoder = decoder;
        }
    }

    /// Parse the `sbr_data()` of the element that starts at the current position of `bs`, which
    /// reads from `data`.
    pub fn parse(
        &mut self,
        bs: &mut BitReaderLtr<'_>,
        data: &[u8],
        independent: bool,
    ) -> Result<()> {
        let position = data.len() as u64 * 8 - bs.bits_left();

        let mut reader = BitReader::new(data);
        reader.ignore_bits(position as u32)?;

        let ext = self.parser.parse(&mut reader, self.channels == 2, independent)?;

        bs.ignore_bits((reader.bit_position() - position) as u32)?;
        self.pending = Some(ext);

        Ok(())
    }

    /// Render the frame of a single channel through the MPEG Surround upmix into two channels.
    pub fn process_upmix(
        &mut self,
        core: &[f32],
        mps: &mut Mps212Decoder,
        output: [&mut [f32]; 2],
        scale: f32,
    ) -> Result<()> {
        for (d, s) in self.core[0].iter_mut().zip(core) {
            *d = f64::from(*s);
        }

        let Some(ext) = self.pending.take()
        else {
            return unsupported_error("aac: usac sbr data is missing");
        };

        let refs: Vec<&[f64]> = self.core.iter().map(Vec::as_slice).collect();
        let rendered = self.decoder.process_frame_upmix(&ext, &refs, mps)?;

        for (dst, src) in output.into_iter().zip(&rendered) {
            for (d, s) in dst.iter_mut().zip(src) {
                *d = (*s as f32) * scale;
            }
        }

        Ok(())
    }

    /// Render the frame: `core` holds the core signal of each channel, `output` receives the
    /// full-rate signal.
    pub fn process(
        &mut self,
        core: &[&[f32]],
        output: &mut [&mut [f32]],
        scale: f32,
    ) -> Result<()> {
        // The SBR tool works on samples of the full scale of 16 bit PCM; `scale` converts the
        // core samples to the output scale.
        for (dst, src) in self.core.iter_mut().zip(core) {
            for (d, s) in dst.iter_mut().zip(*src) {
                *d = f64::from(*s);
            }
        }

        let Some(ext) = self.pending.take()
        else {
            return unsupported_error("aac: usac sbr data is missing");
        };
        let refs: Vec<&[f64]> = self.core.iter().map(Vec::as_slice).collect();
        let rendered = self.decoder.process_frame(&ext, &refs)?;

        for (dst, src) in output.iter_mut().zip(&rendered) {
            for (d, s) in dst.iter_mut().zip(src) {
                *d = (*s as f32) * scale;
            }
        }

        Ok(())
    }
}
