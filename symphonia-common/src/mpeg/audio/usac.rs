// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `UsacConfig()` of Unified Speech and Audio Coding (USAC, xHE-AAC): audio object type 42
//! of ISO/IEC 14496-3, with the syntax of ISO/IEC 23003-3 §5.2 (Tables 3 to 11).
//!
//! The configuration is carried by the audio specific config, after the generic
//! `audioObjectType`, `samplingFrequencyIndex` and `channelConfiguration` fields, which the
//! `UsacConfig()` repeats and supersedes.

use symphonia_core::audio::{Channels, Position};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::io::{FiniteBitStream, ReadBitsLtr};

use super::ChannelElement;

/// The maximum number of `UsacDecoderConfig()` elements (ffmpeg accepts 64 too).
const MAX_ELEMENTS: usize = 64;

/// The maximum number of output channels of a `UsacChannelConfig()`.
const MAX_OUT_CHANNELS: usize = 64;

/// The ratio between the output sample rate and the sample rate of the core of a USAC stream:
/// `sbrRatioIndex` of ISO/IEC 23003-3 Table 70 (`coreSbrFrameLengthIndex`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsacSbrRatio {
    /// No SBR: the output has the core sample rate.
    None,
    /// `sbrRatioIndex` 2: the output is 8/3 times the core sample rate (768 core samples to 2048).
    Ratio8To3,
    /// `sbrRatioIndex` 3: the output is twice the core sample rate (1024 core samples to 2048).
    Ratio2To1,
    /// `sbrRatioIndex` 4: the output is four times the core sample rate (1024 to 4096).
    Ratio4To1,
}

impl UsacSbrRatio {
    /// The number of output samples of a frame of `core_len` core samples.
    pub fn output_len(self, core_len: usize) -> usize {
        match self {
            UsacSbrRatio::None => core_len,
            UsacSbrRatio::Ratio8To3 => core_len * 8 / 3,
            UsacSbrRatio::Ratio2To1 => core_len * 2,
            UsacSbrRatio::Ratio4To1 => core_len * 4,
        }
    }
}

/// The type of a `UsacDecoderConfig()` element (`usacElementType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsacElementType {
    /// `ID_USAC_SCE`: a single channel element.
    Sce,
    /// `ID_USAC_CPE`: a channel pair element.
    Cpe,
    /// `ID_USAC_LFE`: a low frequency effects element.
    Lfe,
    /// `ID_USAC_EXT`: an extension element.
    Ext,
}

/// The `UsacCoreConfig()` of an element.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsacCoreConfig {
    /// `tw_mdct`: the frequency domain core uses the time warped MDCT.
    pub tw_mdct: bool,
    /// `noiseFilling`: the frequency domain core of the element has spectral noise filling.
    pub noise_filling: bool,
}

/// The default `SbrHeader()` of a `SbrConfig()` (`dflt_*`), with the values for the fields that
/// are not transmitted already substituted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsacSbrDefaultHeader {
    pub start_freq: u8,
    pub stop_freq: u8,
    pub freq_scale: u8,
    pub alter_scale: bool,
    pub noise_bands: u8,
    pub limiter_bands: u8,
    pub limiter_gains: u8,
    pub interpol_freq: bool,
    pub smoothing_mode: bool,
    /// `dflt_header_extra1`.
    pub header_extra1: bool,
    /// `dflt_header_extra2`.
    pub header_extra2: bool,
}

/// The `SbrConfig()` of an SCE or CPE element (ISO/IEC 23003-3 Table 6): the configuration of the
/// enhanced SBR (eSBR) of the element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsacSbrConfig {
    /// `harmonicSBR`: harmonic (phase vocoder based) transposition.
    pub harmonic_sbr: bool,
    /// `bs_interTes`: inter-subband-sample temporal envelope shaping.
    pub inter_tes: bool,
    /// `bs_pvc`: predictive vector coding of the envelope.
    pub pvc: bool,
    /// The default `SbrHeader()`.
    pub dflt: UsacSbrDefaultHeader,
}

impl UsacSbrConfig {
    /// Read a `SbrConfig()`.
    pub fn read<B: ReadBitsLtr>(bs: &mut B) -> Result<Self> {
        let harmonic_sbr = bs.read_bool()?;
        let inter_tes = bs.read_bool()?;
        let pvc = bs.read_bool()?;

        // SbrDfltHeader()
        let start_freq = bs.read_bits_leq32(4)? as u8;
        let stop_freq = bs.read_bits_leq32(4)? as u8;
        let header_extra1 = bs.read_bool()?;
        let header_extra2 = bs.read_bool()?;

        let (mut freq_scale, mut alter_scale, mut noise_bands) = (2, true, 2);

        if header_extra1 {
            freq_scale = bs.read_bits_leq32(2)? as u8;
            alter_scale = bs.read_bool()?;
            noise_bands = bs.read_bits_leq32(2)? as u8;
        }

        let (mut limiter_bands, mut limiter_gains, mut interpol_freq, mut smoothing_mode) =
            (2, 2, true, true);

        if header_extra2 {
            limiter_bands = bs.read_bits_leq32(2)? as u8;
            limiter_gains = bs.read_bits_leq32(2)? as u8;
            interpol_freq = bs.read_bool()?;
            smoothing_mode = bs.read_bool()?;
        }

        Ok(UsacSbrConfig {
            harmonic_sbr,
            inter_tes,
            pvc,
            dflt: UsacSbrDefaultHeader {
                start_freq,
                stop_freq,
                freq_scale,
                alter_scale,
                noise_bands,
                limiter_bands,
                limiter_gains,
                interpol_freq,
                smoothing_mode,
                header_extra1,
                header_extra2,
            },
        })
    }
}

/// The `Mps212Config()` of a CPE element with `stereoConfigIndex > 0` (ISO/IEC 23003-3
/// Table 11): the configuration of the MPEG Surround 2-1-2 tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsacMps212Config {
    /// `bsFreqRes`, 1 to 7: the number of parameter bands is 28, 20, 14, 10, 7, 5, 4.
    pub freq_res: u8,
    /// `bsFixedGainDMX`.
    pub fixed_gain_dmx: u8,
    /// `bsTempShapeConfig`.
    pub temp_shape_config: u8,
    /// `bsDecorrConfig`.
    pub decorr_config: u8,
    /// `bsHighRateMode`.
    pub high_rate_mode: bool,
    /// `bsPhaseCoding`.
    pub phase_coding: bool,
    /// `bsOttBandsPhase`: the transmitted value, or the default of ISO/IEC 23003-3 Table 109
    /// for the `freq_res`, raised to `residual_bands` if residual coding is used.
    pub ott_bands_phase: u8,
    /// `bsOttBandsPhasePresent`.
    pub ott_bands_phase_present: bool,
    /// `bsResidualCoding`: `stereoConfigIndex >= 2`.
    pub residual_coding: bool,
    /// `bsResidualBands`.
    pub residual_bands: u8,
    /// `bsPseudoLr`.
    pub pseudo_lr: bool,
    /// `bsEnvQuantMode`.
    pub env_quant_mode: bool,
}

impl UsacMps212Config {
    /// The number of parameter bands for a `bsFreqRes` of 1 to 7 (ISO/IEC 23003-1 Table 39).
    pub const NUM_BANDS: [u8; 8] = [0, 28, 20, 14, 10, 7, 5, 4];

    /// Read a `Mps212Config(stereoConfigIndex)`.
    pub fn read<B: ReadBitsLtr>(bs: &mut B, stereo_config_index: u8) -> Result<Self> {
        let freq_res = bs.read_bits_leq32(3)? as u8;

        if freq_res == 0 {
            return decode_error("usac: reserved bsFreqRes");
        }

        let num_bands = Self::NUM_BANDS[usize::from(freq_res)];

        let fixed_gain_dmx = bs.read_bits_leq32(3)? as u8;
        let temp_shape_config = bs.read_bits_leq32(2)? as u8;
        let decorr_config = bs.read_bits_leq32(2)? as u8;
        let high_rate_mode = bs.read_bool()?;
        let phase_coding = bs.read_bool()?;

        // ISO/IEC 23003-3 Table 109: the default value of bsOttBandsPhase.
        let mut ott_bands_phase = [0, 10, 10, 7, 5, 3, 2, 2][usize::from(freq_res)];

        let ott_bands_phase_present = bs.read_bool()?;

        if ott_bands_phase_present {
            ott_bands_phase = bs.read_bits_leq32(5)? as u8;

            if ott_bands_phase > num_bands {
                return decode_error("usac: invalid bsOttBandsPhase");
            }
        }

        let residual_coding = stereo_config_index >= 2;
        let mut residual_bands = 0;
        let mut pseudo_lr = false;

        if residual_coding {
            residual_bands = bs.read_bits_leq32(5)? as u8;

            if residual_bands > num_bands {
                return decode_error("usac: invalid bsResidualBands");
            }

            ott_bands_phase = ott_bands_phase.max(residual_bands);
            pseudo_lr = bs.read_bool()?;
        }

        let env_quant_mode = if temp_shape_config == 2 { bs.read_bool()? } else { false };

        Ok(UsacMps212Config {
            freq_res,
            fixed_gain_dmx,
            temp_shape_config,
            decorr_config,
            high_rate_mode,
            phase_coding,
            ott_bands_phase,
            ott_bands_phase_present,
            residual_coding,
            residual_bands,
            pseudo_lr,
            env_quant_mode,
        })
    }
}

/// The type of an extension element (`usacExtElementType`, ISO/IEC 23003-3 Table 14).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsacExtType {
    /// `ID_EXT_ELE_FILL`: fill data.
    Fill,
    /// `ID_EXT_ELE_MPEGS`: MPEG Surround payload.
    Mpegs,
    /// `ID_EXT_ELE_SAOC`: SAOC payload.
    Saoc,
    /// `ID_EXT_ELE_AUDIOPREROLL`: the audio pre-roll (config and frames to decode first).
    AudioPreroll,
    /// `ID_EXT_ELE_UNI_DRC`: `uniDrc()` gain data (ISO/IEC 23003-4).
    UniDrc,
    /// Any other value (reserved, or application specific).
    Other(u32),
}

impl UsacExtType {
    fn from_value(value: u32) -> Self {
        match value {
            0 => UsacExtType::Fill,
            1 => UsacExtType::Mpegs,
            2 => UsacExtType::Saoc,
            3 => UsacExtType::AudioPreroll,
            4 => UsacExtType::UniDrc,
            other => UsacExtType::Other(other),
        }
    }
}

/// The `UsacExtElementConfig()` of an extension element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsacExtElementConfig {
    /// `usacExtElementType`.
    pub ext_type: UsacExtType,
    /// `usacExtElementDefaultLength`, if `usacExtElementDefaultLengthPresent`.
    pub default_length: Option<u32>,
    /// `usacExtElementPayloadFrag`: the payload may be fragmented over several frames.
    pub payload_frag: bool,
    /// The bytes of `usacExtElementConfigData()`, `usacExtElementConfigLength` of them. Its
    /// contents are not interpreted, except where a decoder uses them.
    pub config_data: Box<[u8]>,
}

/// The configuration of an element of the `UsacDecoderConfig()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsacElementConfig {
    /// A single channel element.
    Sce { core: UsacCoreConfig, sbr: Option<UsacSbrConfig> },
    /// A channel pair element. `stereo_config_index` is 0 for a stereo core (with the stereo
    /// tools of the frequency domain: M/S and complex prediction), and 1 to 3 if the element uses
    /// MPEG Surround 2-1-2 (`mps212` is then present).
    ///
    /// * 1: one core channel, upmixed to a stereo pair by the MPS tool.
    /// * 2: two core channels: a downmix and a residual signal for the lower bands.
    /// * 3: as 2, with the residual signal being a prediction residual (`bsPseudoLr`).
    Cpe {
        core: UsacCoreConfig,
        sbr: Option<UsacSbrConfig>,
        stereo_config_index: u8,
        mps212: Option<UsacMps212Config>,
    },
    /// A low frequency effects element. It has no configuration: it is an SCE with `tw_mdct` and
    /// `noiseFilling` off.
    Lfe,
    /// An extension element.
    Ext(UsacExtElementConfig),
}

impl UsacElementConfig {
    /// The type of the element.
    pub fn element_type(&self) -> UsacElementType {
        match self {
            UsacElementConfig::Sce { .. } => UsacElementType::Sce,
            UsacElementConfig::Cpe { .. } => UsacElementType::Cpe,
            UsacElementConfig::Lfe => UsacElementType::Lfe,
            UsacElementConfig::Ext(_) => UsacElementType::Ext,
        }
    }
}

/// An element of the `UsacConfigExtension()` (`usacConfigExtType`, ISO/IEC 23003-3 Table 15).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsacConfigExtension {
    /// The `usacConfigExtType`: 0 fill, 1 downmix, 2 loudness info set, 3 audio scene info,
    /// 7 stream id, others reserved.
    pub ext_type: u32,
    /// The `usacConfigExtLength` bytes of the extension payload.
    pub data: Box<[u8]>,
}

impl UsacConfigExtension {
    /// `ID_CONFIG_EXT_FILL`.
    pub const FILL: u32 = 0;
    /// `ID_CONFIG_EXT_LOUDNESS_INFO`.
    pub const LOUDNESS_INFO: u32 = 2;
    /// `ID_CONFIG_EXT_STREAM_ID`.
    pub const STREAM_ID: u32 = 7;
}

/// The `UsacConfig()` (ISO/IEC 23003-3 Table 3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsacConfig {
    /// `usacSamplingFrequency`: the sampling frequency of the decoded output (with eSBR, that is
    /// `sbr_ratio` times the sampling frequency of the core).
    pub sampling_frequency: u32,
    /// `coreSbrFrameLengthIndex`, 0 to 4.
    pub core_sbr_frame_length_index: u8,
    /// `coreCoderFrameLength`: the number of samples of a frame of the core coder, 768 or 1024.
    pub core_frame_length: usize,
    /// The ratio of the output and core sample rates.
    pub sbr_ratio: UsacSbrRatio,
    /// `channelConfigurationIndex`; 0 if the channels are given by `UsacChannelConfig()`.
    pub channel_configuration_index: u8,
    /// The output channels.
    pub channels: Channels,
    /// The syntactic elements (`SCE`, `CPE` and `LFE`) that decode to output channels, in
    /// bitstream order (the extension elements are not listed), and their output channel
    /// positions.
    pub channel_elements: Vec<ChannelElement>,
    /// The `UsacDecoderConfig()` elements, in bitstream order.
    pub elements: Vec<UsacElementConfig>,
    /// The `UsacConfigExtension()` elements.
    pub config_extensions: Vec<UsacConfigExtension>,
}

impl UsacConfig {
    /// The sampling frequency of the core coder (before SBR).
    pub fn core_sampling_frequency(&self) -> u32 {
        match self.sbr_ratio {
            UsacSbrRatio::None => self.sampling_frequency,
            UsacSbrRatio::Ratio8To3 => (u64::from(self.sampling_frequency) * 3 / 8) as u32,
            UsacSbrRatio::Ratio2To1 => self.sampling_frequency / 2,
            UsacSbrRatio::Ratio4To1 => self.sampling_frequency / 4,
        }
    }

    /// The number of output samples of a frame.
    pub fn output_frame_length(&self) -> usize {
        self.sbr_ratio.output_len(self.core_frame_length)
    }

    /// Read a `UsacConfig()`.
    pub fn read<B: ReadBitsLtr + FiniteBitStream>(bs: &mut B) -> Result<Self> {
        // usacSamplingFrequencyIndex (ISO/IEC 23003-3 Table 67).
        const SAMPLE_RATES: [u32; 32] = [
            96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000,
            7350, 0, 0, 57600, 51200, 40000, 38400, 34150, 28800, 25600, 20000, 19200, 17075,
            14400, 12800, 9600, 0, 0, 0, 0,
        ];

        let rate_index = bs.read_bits_leq32(5)? as usize;

        let sampling_frequency =
            if rate_index == 0x1f { bs.read_bits_leq32(24)? } else { SAMPLE_RATES[rate_index] };

        if sampling_frequency == 0 {
            return decode_error("usac: invalid sampling frequency");
        }

        // coreSbrFrameLengthIndex (ISO/IEC 23003-3 Table 70).
        let core_sbr_frame_length_index = bs.read_bits_leq32(3)? as u8;

        let (core_frame_length, sbr_ratio) = match core_sbr_frame_length_index {
            0 => (768, UsacSbrRatio::None),
            1 => (1024, UsacSbrRatio::None),
            2 => (768, UsacSbrRatio::Ratio8To3),
            3 => (1024, UsacSbrRatio::Ratio2To1),
            4 => (1024, UsacSbrRatio::Ratio4To1),
            _ => return decode_error("usac: reserved coreSbrFrameLengthIndex"),
        };

        let channel_configuration_index = bs.read_bits_leq32(5)? as u8;

        let mut explicit_positions = Vec::new();

        if channel_configuration_index == 0 {
            // UsacChannelConfig()
            let num_out_channels = read_escaped_value(bs, 5, 8, 16)? as usize;

            if num_out_channels > MAX_OUT_CHANNELS {
                return decode_error("usac: too many output channels");
            }

            for _ in 0..num_out_channels {
                explicit_positions.push(bs.read_bits_leq32(5)? as u8);
            }
        }

        // UsacDecoderConfig()
        let num_elements = read_escaped_value(bs, 4, 8, 16)? as usize + 1;

        if num_elements > MAX_ELEMENTS {
            return decode_error("usac: too many elements");
        }

        let mut elements = Vec::with_capacity(num_elements);

        for _ in 0..num_elements {
            let element = match bs.read_bits_leq32(2)? {
                0 => {
                    let core = read_core_config(bs)?;
                    let sbr = read_sbr_config_if(bs, sbr_ratio)?;
                    UsacElementConfig::Sce { core, sbr }
                }
                1 => {
                    let core = read_core_config(bs)?;
                    let mut sbr = None;
                    let mut stereo_config_index = 0;

                    if sbr_ratio != UsacSbrRatio::None {
                        sbr = Some(UsacSbrConfig::read(bs)?);
                        stereo_config_index = bs.read_bits_leq32(2)? as u8;
                    }

                    let mps212 = if stereo_config_index > 0 {
                        Some(UsacMps212Config::read(bs, stereo_config_index)?)
                    }
                    else {
                        None
                    };

                    UsacElementConfig::Cpe { core, sbr, stereo_config_index, mps212 }
                }
                2 => UsacElementConfig::Lfe,
                _ => UsacElementConfig::Ext(read_ext_element_config(bs)?),
            };

            elements.push(element);
        }

        // The channel elements: a CPE yields two output channels (a channel pair, or the MPS
        // upmix of one core channel).
        let (channels, channel_elements) =
            resolve_channels(channel_configuration_index, &explicit_positions, &elements)?;

        let mut config_extensions = Vec::new();

        // usacConfigExtensionPresent
        if bs.read_bool()? {
            let num_extensions = read_escaped_value(bs, 2, 4, 8)? + 1;

            for _ in 0..num_extensions {
                let ext_type = read_escaped_value(bs, 4, 8, 16)?;
                let len = read_escaped_value(bs, 4, 8, 16)? as usize;

                config_extensions
                    .push(UsacConfigExtension { ext_type, data: read_bytes(bs, len)? });
            }
        }

        Ok(UsacConfig {
            sampling_frequency,
            core_sbr_frame_length_index,
            core_frame_length,
            sbr_ratio,
            channel_configuration_index,
            channels,
            channel_elements,
            elements,
            config_extensions,
        })
    }
}

/// Read an escaped value of ISO/IEC 23003-3 §5.2: `n1` bits, with an escape to an addend of `n2`
/// bits, which itself escapes to an addend of `n3` bits.
pub fn read_escaped_value<B: ReadBitsLtr>(bs: &mut B, n1: u32, n2: u32, n3: u32) -> Result<u32> {
    let mut value = bs.read_bits_leq32(n1)?;

    if value < (1 << n1) - 1 {
        return Ok(value);
    }

    let value2 = bs.read_bits_leq32(n2)?;
    value += value2;

    if n3 > 0 && value2 == (1 << n2) - 1 {
        value += bs.read_bits_leq32(n3)?;
    }

    Ok(value)
}

/// Read `len` whole bytes.
fn read_bytes<B: ReadBitsLtr + FiniteBitStream>(bs: &mut B, len: usize) -> Result<Box<[u8]>> {
    // Do not allocate for a length that is clearly beyond the data.
    if bs.bits_left() < len.saturating_mul(8) as u64 {
        return decode_error("usac: truncated configuration");
    }

    let mut data = Vec::with_capacity(len);

    for _ in 0..len {
        data.push(bs.read_bits_leq32(8)? as u8);
    }

    Ok(data.into_boxed_slice())
}

/// Read a `UsacCoreConfig()`.
fn read_core_config<B: ReadBitsLtr>(bs: &mut B) -> Result<UsacCoreConfig> {
    let tw_mdct = bs.read_bool()?;
    let noise_filling = bs.read_bool()?;
    Ok(UsacCoreConfig { tw_mdct, noise_filling })
}

/// Read the `SbrConfig()` of an element if the stream uses SBR.
fn read_sbr_config_if<B: ReadBitsLtr>(
    bs: &mut B,
    ratio: UsacSbrRatio,
) -> Result<Option<UsacSbrConfig>> {
    if ratio == UsacSbrRatio::None { Ok(None) } else { UsacSbrConfig::read(bs).map(Some) }
}

/// Read a `UsacExtElementConfig()`.
fn read_ext_element_config<B: ReadBitsLtr + FiniteBitStream>(
    bs: &mut B,
) -> Result<UsacExtElementConfig> {
    let ext_type = UsacExtType::from_value(read_escaped_value(bs, 4, 8, 16)?);
    let config_length = read_escaped_value(bs, 4, 8, 16)? as usize;

    let default_length =
        if bs.read_bool()? { Some(read_escaped_value(bs, 8, 16, 0)? + 1) } else { None };

    let payload_frag = bs.read_bool()?;
    let config_data = read_bytes(bs, config_length)?;

    Ok(UsacExtElementConfig { ext_type, default_length, payload_frag, config_data })
}

/// Map a `bsOutputChannelPos` (ISO/IEC 23003-3 Table 74) to a channel position. Positions that
/// the channel set of the decoder cannot represent return `None`.
fn output_channel_position(pos: u8) -> Option<Position> {
    Some(match pos {
        0 => Position::FRONT_LEFT,
        1 => Position::FRONT_RIGHT,
        2 => Position::FRONT_CENTER,
        3 => Position::LFE1,
        4 => Position::SIDE_LEFT,
        5 => Position::SIDE_RIGHT,
        6 => Position::FRONT_LEFT_CENTER,
        7 => Position::FRONT_RIGHT_CENTER,
        8 => Position::REAR_LEFT,
        9 => Position::REAR_RIGHT,
        10 => Position::REAR_CENTER,
        11 => Position::SURROUND_DIRECT_LEFT,
        12 => Position::SURROUND_DIRECT_RIGHT,
        13 => Position::SIDE_LEFT,
        14 => Position::SIDE_RIGHT,
        15 => Position::FRONT_LEFT_WIDE,
        16 => Position::FRONT_RIGHT_WIDE,
        17 => Position::TOP_FRONT_LEFT,
        18 => Position::TOP_FRONT_RIGHT,
        19 => Position::TOP_FRONT_CENTER,
        20 => Position::TOP_REAR_LEFT,
        21 => Position::TOP_REAR_RIGHT,
        22 => Position::TOP_REAR_CENTER,
        23 => Position::TOP_SIDE_LEFT,
        24 => Position::TOP_SIDE_RIGHT,
        25 => Position::TOP_CENTER,
        26 => Position::LFE2,
        27 => Position::BOTTOM_FRONT_LEFT,
        28 => Position::BOTTOM_FRONT_RIGHT,
        29 => Position::BOTTOM_FRONT_CENTER,
        _ => return None,
    })
}

/// The channel elements of the predefined `channelConfigurationIndex` values (ISO/IEC 23003-3
/// Table 69, which refers to the ChannelConfiguration of ISO/IEC 23091-3, with the element order
/// of ISO/IEC 14496-3 Table 1.19). Only the configurations whose channels the output channel
/// sets can represent are supported.
fn predefined_channel_elements(index: u8) -> Option<Vec<ChannelElement>> {
    match index {
        1..=7 | 11 | 12 => super::default_channel_elements(u32::from(index)),
        _ => None,
    }
}

/// Resolve the output channels and the syntactic channel elements of the configuration.
fn resolve_channels(
    channel_configuration_index: u8,
    explicit_positions: &[u8],
    elements: &[UsacElementConfig],
) -> Result<(Channels, Vec<ChannelElement>)> {
    let element_channels = |element: &UsacElementConfig| match element {
        UsacElementConfig::Sce { .. } | UsacElementConfig::Lfe => 1,
        UsacElementConfig::Cpe { .. } => 2,
        UsacElementConfig::Ext(_) => 0,
    };

    let num_channels: usize = elements.iter().map(element_channels).sum();

    let channel_elements = if channel_configuration_index == 0 {
        // The positions are assigned to the channels of the elements in order.
        if explicit_positions.len() != num_channels {
            return decode_error("usac: the number of output channels does not match the elements");
        }

        let mut positions = explicit_positions.iter().map(|&p| output_channel_position(p));
        let mut next_position = || match positions.next() {
            Some(Some(position)) => Ok(position),
            _ => unsupported_error("usac: unsupported output channel position"),
        };

        let mut channel_elements = Vec::new();

        for element in elements {
            match element {
                UsacElementConfig::Sce { .. } | UsacElementConfig::Lfe => {
                    channel_elements.push(ChannelElement::Single(next_position()?));
                }
                UsacElementConfig::Cpe { .. } => {
                    let left = next_position()?;
                    let right = next_position()?;
                    channel_elements.push(ChannelElement::Pair(left, right));
                }
                UsacElementConfig::Ext(_) => (),
            }
        }

        channel_elements
    }
    else {
        let Some(channel_elements) = predefined_channel_elements(channel_configuration_index)
        else {
            return unsupported_error("usac: unsupported channel configuration index");
        };

        // The elements of the configuration must be those of the channel configuration.
        let matches = channel_elements.len()
            == elements.iter().filter(|e| e.element_type() != UsacElementType::Ext).count()
            && channel_elements
                .iter()
                .zip(elements.iter().filter(|e| e.element_type() != UsacElementType::Ext))
                .all(|(channel_element, element)| {
                    matches!(
                        (channel_element, element),
                        (ChannelElement::Pair(..), UsacElementConfig::Cpe { .. })
                            | (ChannelElement::Single(_), UsacElementConfig::Sce { .. })
                            | (ChannelElement::Single(_), UsacElementConfig::Lfe)
                    )
                });

        if !matches {
            return decode_error("usac: the elements do not match the channel configuration");
        }

        channel_elements
    };

    let mut positions = Position::empty();

    for element in &channel_elements {
        let added = match *element {
            ChannelElement::Single(position) => position,
            ChannelElement::Pair(left, right) => left | right,
        };

        if positions.intersects(added) {
            return unsupported_error("usac: duplicate output channel position");
        }

        positions |= added;
    }

    if positions.is_empty() {
        return decode_error("usac: no output channels");
    }

    Ok((Channels::Positioned(positions), channel_elements))
}
