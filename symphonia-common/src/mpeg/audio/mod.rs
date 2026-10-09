// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::audio::{Channels, Position, layouts};
use symphonia_core::codecs::CodecProfile;
use symphonia_core::codecs::audio::well_known::profiles::*;
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::io::{BitReaderLtr, FiniteBitStream, ReadBitsLtr};

/// MPEG4 audio object types.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AudioObjectType {
    #[default]
    None,
    Main,
    Lc,
    Ssr,
    Ltp,
    Sbr,
    Scalable,
    TwinVq,
    Celp,
    Hvxc,
    Ttsi,
    MainSynth,
    WavetableSynth,
    GeneralMidi,
    Algorithmic,
    ErAacLc,
    ErAacLtp,
    ErAacScalable,
    ErTwinVq,
    ErBsac,
    ErAacLd,
    ErCelp,
    ErHvxc,
    ErHiln,
    ErParametric,
    Ssc,
    Ps,
    MpegSurround,
    Layer1,
    Layer2,
    Layer3,
    Dst,
    Als,
    Sls,
    SlsNonCore,
    ErAacEld,
    SmrSimple,
    SmrMain,
    Reserved,
    Unknown,
}

impl std::fmt::Display for AudioObjectType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", AUDIO_OBJECT_TYPE_NAMES[*self as usize])
    }
}

const AUDIO_OBJECT_TYPES: &[AudioObjectType] = &[
    AudioObjectType::None,
    AudioObjectType::Main,
    AudioObjectType::Lc,
    AudioObjectType::Ssr,
    AudioObjectType::Ltp,
    AudioObjectType::Sbr,
    AudioObjectType::Scalable,
    AudioObjectType::TwinVq,
    AudioObjectType::Celp,
    AudioObjectType::Hvxc,
    AudioObjectType::Reserved,
    AudioObjectType::Reserved,
    AudioObjectType::Ttsi,
    AudioObjectType::MainSynth,
    AudioObjectType::WavetableSynth,
    AudioObjectType::GeneralMidi,
    AudioObjectType::Algorithmic,
    AudioObjectType::ErAacLc,
    AudioObjectType::Reserved,
    AudioObjectType::ErAacLtp,
    AudioObjectType::ErAacScalable,
    AudioObjectType::ErTwinVq,
    AudioObjectType::ErBsac,
    AudioObjectType::ErAacLd,
    AudioObjectType::ErCelp,
    AudioObjectType::ErHvxc,
    AudioObjectType::ErHiln,
    AudioObjectType::ErParametric,
    AudioObjectType::Ssc,
    AudioObjectType::Ps,
    AudioObjectType::MpegSurround,
    AudioObjectType::Reserved, // Escape
    AudioObjectType::Layer1,
    AudioObjectType::Layer2,
    AudioObjectType::Layer3,
    AudioObjectType::Dst,
    AudioObjectType::Als,
    AudioObjectType::Sls,
    AudioObjectType::SlsNonCore,
    AudioObjectType::ErAacEld,
    AudioObjectType::SmrSimple,
    AudioObjectType::SmrMain,
];

const AUDIO_OBJECT_TYPE_NAMES: &[&str] = &[
    "None",
    "AAC Main",
    "AAC LC",
    "AAC SSR",
    "AAC LTP",
    "SBR",
    "AAC Scalable",
    "TwinVQ",
    "CELP",
    "HVXC",
    // "(Reserved10)",
    // "(Reserved11)",
    "TTSI",
    "Main synthetic",
    "Wavetable synthesis",
    "General MIDI",
    "Algorithmic Synthesis and Audio FX",
    "ER AAC LC",
    // "(Reserved18)",
    "ER AAC LTP",
    "ER AAC Scalable",
    "ER TwinVQ",
    "ER BSAC",
    "ER AAC LD",
    "ER CELP",
    "ER HVXC",
    "ER HILN",
    "ER Parametric",
    "SSC",
    "PS",
    "MPEG Surround",
    // "(Escape)",
    "Layer-1",
    "Layer-2",
    "Layer-3",
    "DST",
    "ALS",
    "SLS",
    "SLS non-core",
    "ER AAC ELD",
    "SMR Simple",
    "SMR Main",
    "(Reserved)",
    "(Unknown)",
];

/// Try to get the audio object type from the given audio object type index.
pub fn get_mpeg4_audio_object_type_by_index(index: u32) -> Option<AudioObjectType> {
    AUDIO_OBJECT_TYPES.get(index as usize).copied()
}

/// MPEG4 audio sample rate result.
pub enum Mpeg4AudioSampleRate {
    /// The sample rate is known.
    SampleRate(u32),
    /// The sample rate should be read manually.
    ///
    /// For the MPEG4 Audio Specific Config, the sample rate should be read as the next 24-bits
    /// after the sample rate index. For ADTS, this is an error.
    Escape,
    /// The sample rate index was invalid.
    Invalid,
}

/// Try to get the audio sample rate given the sample rate index.
pub fn get_mpeg4_audio_sample_rate_by_index(index: u32) -> Mpeg4AudioSampleRate {
    const MPEG4_AUDIO_SAMPLE_RATES: [u32; 13] =
        [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

    match index {
        0..=12 => Mpeg4AudioSampleRate::SampleRate(MPEG4_AUDIO_SAMPLE_RATES[index as usize]),
        15 => Mpeg4AudioSampleRate::Escape,
        _ => Mpeg4AudioSampleRate::Invalid,
    }
}

/// MPEG4 audio channel configuration result.
pub enum Mpeg4AudioChannels {
    /// The channel configuration is known.
    Channels(Channels),
    /// The channel configuration is defined in-band based on the audio object type (e.g., from the
    /// program configuration element).
    Escape,
    /// The channel configuration index was invalid.
    Invalid,
}

/// Try to get the set of channels given the configuration index.
pub fn get_mpeg4_audio_channels_by_config_index(index: u32) -> Mpeg4AudioChannels {
    let channels = match index {
        0 => return Mpeg4AudioChannels::Escape,
        1 => layouts::CHANNEL_LAYOUT_MONO,
        2 => layouts::CHANNEL_LAYOUT_STEREO,
        3 => layouts::CHANNEL_LAYOUT_AAC_3P0,
        4 => layouts::CHANNEL_LAYOUT_AAC_4P0,
        5 => layouts::CHANNEL_LAYOUT_AAC_5P0,
        6 => layouts::CHANNEL_LAYOUT_AAC_5P1,
        7 => layouts::CHANNEL_LAYOUT_AAC_7P1,
        11 => layouts::CHANNEL_LAYOUT_AAC_6P1,
        12 => layouts::CHANNEL_LAYOUT_7P1,
        _ => return Mpeg4AudioChannels::Invalid,
    };
    Mpeg4AudioChannels::Channels(channels)
}

/// One syntactic channel element (`SCE`/`CPE`/`LFE`) that is expected to appear, in this exact
/// order, in the program's audio element stream.
///
/// This maps the syntactic element order (dictated by `channelConfiguration`, or by an explicit
/// `program_config_element()` when `channelConfiguration == 0`) onto output channel positions,
/// since the syntactic order does not, in general, match the ascending-bit-position order that
/// [`Channels::Positioned`] uses for the output buffer's channel planes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelElement {
    /// A single channel element (`SCE` or `LFE`) for the given channel position.
    Single(Position),
    /// A channel pair element (`CPE`) for the given (left, right) channel positions.
    Pair(Position, Position),
}

/// The syntactic element order for the predefined `channelConfiguration` values 1-7, 11 and 12 of
/// ISO/IEC 14496-3 Table 1.19. Returns `None` for the "escape" value (0, use
/// `program_config_element()`) or any reserved/invalid index.
fn default_channel_elements(index: u32) -> Option<Vec<ChannelElement>> {
    use ChannelElement::{Pair, Single};

    let elements = match index {
        1 => vec![Single(Position::FRONT_CENTER)],
        2 => vec![Pair(Position::FRONT_LEFT, Position::FRONT_RIGHT)],
        3 => {
            vec![Single(Position::FRONT_CENTER), Pair(Position::FRONT_LEFT, Position::FRONT_RIGHT)]
        }
        4 => vec![
            Single(Position::FRONT_CENTER),
            Pair(Position::FRONT_LEFT, Position::FRONT_RIGHT),
            Single(Position::REAR_CENTER),
        ],
        5 => vec![
            Single(Position::FRONT_CENTER),
            Pair(Position::FRONT_LEFT, Position::FRONT_RIGHT),
            Pair(Position::REAR_LEFT, Position::REAR_RIGHT),
        ],
        6 => vec![
            Single(Position::FRONT_CENTER),
            Pair(Position::FRONT_LEFT, Position::FRONT_RIGHT),
            Pair(Position::REAR_LEFT, Position::REAR_RIGHT),
            Single(Position::LFE1),
        ],
        7 => vec![
            Single(Position::FRONT_CENTER),
            Pair(Position::FRONT_LEFT, Position::FRONT_RIGHT),
            Pair(Position::FRONT_LEFT_CENTER, Position::FRONT_RIGHT_CENTER),
            Pair(Position::REAR_LEFT, Position::REAR_RIGHT),
            Single(Position::LFE1),
        ],
        // 6.1: front C, front L/R, back L/R, back C, LFE.
        11 => vec![
            Single(Position::FRONT_CENTER),
            Pair(Position::FRONT_LEFT, Position::FRONT_RIGHT),
            Pair(Position::REAR_LEFT, Position::REAR_RIGHT),
            Single(Position::REAR_CENTER),
            Single(Position::LFE1),
        ],
        // 7.1: front C, front L/R, side L/R, back L/R, LFE.
        12 => vec![
            Single(Position::FRONT_CENTER),
            Pair(Position::FRONT_LEFT, Position::FRONT_RIGHT),
            Pair(Position::SIDE_LEFT, Position::SIDE_RIGHT),
            Pair(Position::REAR_LEFT, Position::REAR_RIGHT),
            Single(Position::LFE1),
        ],
        _ => return None,
    };
    Some(elements)
}

/// Reverse lookup: given a channel set that exactly matches one of the predefined
/// `channelConfiguration` values 1-7 (ISO/IEC 14496-3 Table 1.19), return its syntactic element
/// order. Returns `None` if `channels` does not exactly match one of these predefined layouts
/// (e.g. a `program_config_element()`-derived, discrete, or otherwise custom channel set).
///
/// This is useful for codecs that only receive a resolved [`Channels`] value (e.g. from an ADTS
/// header, which has no independent [`AudioSpecificConfig`]) but still need the syntactic element
/// order to place `raw_data_block()` elements into the correct output channel.
pub fn channel_elements_for_config(channels: &Channels) -> Option<Vec<ChannelElement>> {
    (1..=7u32).find_map(|index| {
        let candidate = match get_mpeg4_audio_channels_by_config_index(index) {
            Mpeg4AudioChannels::Channels(c) => c,
            _ => return None,
        };
        if candidate == *channels { default_channel_elements(index) } else { None }
    })
}

/// MPEG4 Audio Specific Configuration.
#[non_exhaustive]
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AudioSpecificConfig {
    pub object_type: AudioObjectType,
    pub sample_rate: u32,
    pub channels: Option<Channels>,
    /// The syntactic element order (`SCE`/`CPE`/`LFE`) implied by `channels`, when known. This is
    /// `Some` for both the predefined `channelConfiguration` values 1-7 and an explicit
    /// `program_config_element()` (`channelConfiguration == 0`).
    pub channel_elements: Option<Vec<ChannelElement>>,
    pub samples: usize,
    /// The SBR output sampling frequency (and, for some object types, the extension channel
    /// configuration), if the stream explicitly signals SBR (hierarchically, or backwards
    /// compatibly via the `sync_extension`).
    pub sbr_ps_info: Option<(u32, Option<Channels>)>,
    pub sbr_present: bool,
    pub ps_present: bool,
    /// True if the config signals the use of the error resilience tools of ER AAC LD and ER AAC
    /// ELD: Huffman codeword reordering, reversible variable length coding, or virtual codebooks
    /// (the `aacSectionDataResilienceFlag`, `aacScalefactorDataResilienceFlag`, and
    /// `aacSpectralDataResilienceFlag`).
    pub er_resilience: bool,
    /// The SBR config of an ER AAC ELD stream with low delay SBR (`ldSbrPresentFlag`).
    pub eld_sbr: Option<EldSbrConfig>,
}

/// The SBR config of an `ELDSpecificConfig()` (ISO/IEC 14496-3 §4.4.1.2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EldSbrConfig {
    /// The `ldSbrSamplingRate` flag. If true, the SBR output has twice the sampling rate of the
    /// core codec (dual rate). Otherwise, it is the same (downsampled SBR).
    pub dual_rate: bool,
    /// The `ldSbrCrcFlag` flag: the SBR payloads are protected by a CRC.
    pub crc: bool,
    /// The `sbr_header()` of each SBR element (SCE or CPE) of the stream, in order.
    pub headers: Vec<SbrHeaderConfig>,
}

/// The fields of an `sbr_header()` (ISO/IEC 14496-3 §4.6.18.2.1), with the default values for
/// the fields that are not transmitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SbrHeaderConfig {
    pub amp_res: bool,
    pub start_freq: u8,
    pub stop_freq: u8,
    pub xover_band: u8,
    pub freq_scale: u8,
    pub alter_scale: bool,
    pub noise_bands: u8,
    pub limiter_bands: u8,
    pub limiter_gains: u8,
    pub interpol_freq: bool,
    pub smoothing_mode: bool,
}

impl SbrHeaderConfig {
    /// Read an `sbr_header()`.
    fn read<B: ReadBitsLtr>(bs: &mut B) -> Result<SbrHeaderConfig> {
        let amp_res = bs.read_bool()?;
        let start_freq = bs.read_bits_leq32(4)? as u8;
        let stop_freq = bs.read_bits_leq32(4)? as u8;
        let xover_band = bs.read_bits_leq32(3)? as u8;
        bs.ignore_bits(2)?; // bs_reserved
        let header_extra_1 = bs.read_bool()?;
        let header_extra_2 = bs.read_bool()?;

        let (mut freq_scale, mut alter_scale, mut noise_bands) = (2, true, 2);

        if header_extra_1 {
            freq_scale = bs.read_bits_leq32(2)? as u8;
            alter_scale = bs.read_bool()?;
            noise_bands = bs.read_bits_leq32(2)? as u8;
        }

        let (mut limiter_bands, mut limiter_gains, mut interpol_freq, mut smoothing_mode) =
            (2, 2, true, true);

        if header_extra_2 {
            limiter_bands = bs.read_bits_leq32(2)? as u8;
            limiter_gains = bs.read_bits_leq32(2)? as u8;
            interpol_freq = bs.read_bool()?;
            smoothing_mode = bs.read_bool()?;
        }

        Ok(SbrHeaderConfig {
            amp_res,
            start_freq,
            stop_freq,
            xover_band,
            freq_scale,
            alter_scale,
            noise_bands,
            limiter_bands,
            limiter_gains,
            interpol_freq,
            smoothing_mode,
        })
    }
}

/// The number of `sbr_header()`s in the `ELDSpecificConfig()`: one for each SCE and CPE of the
/// channel configuration (the numbers of ISO/IEC 14496-3 Table 4.? for the predefined channel
/// configurations).
fn eld_num_sbr_headers(elements: &[ChannelElement]) -> usize {
    elements
        .iter()
        .filter(|element| {
            !matches!(
                element,
                ChannelElement::Single(pos) if *pos == Position::LFE1 || *pos == Position::LFE2
            )
        })
        .count()
}

impl AudioSpecificConfig {
    /// The sampling frequency of the decoded output.
    ///
    /// For plain streams this is the core sampling frequency. If the stream signals SBR, it is the
    /// SBR output sampling frequency: twice the core rate for dual-rate SBR, or equal to the core
    /// rate for the downsampled SBR mode.
    pub fn output_sample_rate(&self) -> u32 {
        if !self.sbr_present {
            return self.sample_rate;
        }

        match self.sbr_ps_info {
            Some((rate, _)) if rate == self.sample_rate || rate == self.sample_rate * 2 => rate,
            _ => self.sample_rate.saturating_mul(2),
        }
    }

    /// The channels of the decoded output: identical to `channels`, unless parametric stereo
    /// expands a mono core stream to stereo.
    pub fn output_channels(&self) -> Option<Channels> {
        match &self.channels {
            Some(channels) if self.ps_present && channels.count() == 1 => {
                Some(layouts::CHANNEL_LAYOUT_STEREO)
            }
            channels => channels.clone(),
        }
    }

    /// Read the audio specific configuration from the provided buffer. ISO14496-3-2009
    pub fn read(buf: &[u8]) -> Result<AudioSpecificConfig> {
        Self::read_from(&mut BitReaderLtr::new(buf))
    }

    /// Read the audio specific configuration from the current position of a bit stream, leaving
    /// the bit stream positioned after it. ISO14496-3-2009
    ///
    /// If the audio specific configuration is not the last element of the bit stream, as in a
    /// LATM stream mux config, the bits following it must not be mistaken for the trailing
    /// `syncExtensionType` of an explicitly signalled extension (which, however, requires at
    /// least 16 bits and an 11-bit sync word to match).
    pub fn read_from<B: ReadBitsLtr + FiniteBitStream>(bs: &mut B) -> Result<AudioSpecificConfig> {
        let mut asc = Self::read_core_from(bs)?;
        let _ = asc.read_sync_extension(bs)?;
        Ok(asc)
    }

    /// Read the audio specific configuration from the current position of a bit stream, except
    /// for the optional trailing `syncExtensionType` extension (see [`Self::read_sync_extension`]).
    pub fn read_core_from<B: ReadBitsLtr + FiniteBitStream>(
        bs: &mut B,
    ) -> Result<AudioSpecificConfig> {
        let mut asc = AudioSpecificConfig {
            object_type: Self::read_audio_object_type(bs)?,
            sample_rate: Self::read_sampling_frequency(bs)?,
            ..Default::default()
        };

        if asc.sample_rate == 0 {
            return decode_error("common (mp4a): a sample rate of 0 is invalid");
        }

        let (channels, channel_elements) = Self::read_channel_config(bs)?;
        asc.channels = channels;
        asc.channel_elements = channel_elements;

        if (asc.object_type == AudioObjectType::Sbr) || (asc.object_type == AudioObjectType::Ps) {
            asc.sbr_present = true;
            if asc.object_type == AudioObjectType::Ps {
                asc.ps_present = true;
            }
            let ext_srate = Self::read_sampling_frequency(bs)?;
            asc.object_type = Self::read_audio_object_type(bs)?;

            let ext_chans = if asc.object_type == AudioObjectType::ErBsac {
                Self::read_channel_config(bs)?.0
            }
            else {
                None
            };

            asc.sbr_ps_info = Some((ext_srate, ext_chans));
        }

        match asc.object_type {
            AudioObjectType::Main
            | AudioObjectType::Lc
            | AudioObjectType::Ssr
            | AudioObjectType::Scalable
            | AudioObjectType::TwinVq
            | AudioObjectType::ErAacLc
            | AudioObjectType::ErAacLtp
            | AudioObjectType::ErAacScalable
            | AudioObjectType::ErTwinVq
            | AudioObjectType::ErBsac
            | AudioObjectType::ErAacLd => {
                // GASpecificConfig
                let short_frame = bs.read_bool()?;

                // The frame length of AAC LD is 512 or 480 samples, rather than 1024 or 960.
                asc.samples = match (asc.object_type, short_frame) {
                    (AudioObjectType::ErAacLd, true) => 480,
                    (AudioObjectType::ErAacLd, false) => 512,
                    (_, true) => 960,
                    (_, false) => 1024,
                };

                let depends_on_core = bs.read_bool()?;

                if depends_on_core {
                    let _delay = bs.read_bits_leq32(14)?;
                }

                let extension_flag = bs.read_bool()?;

                if asc.channels.is_none() {
                    // `channelConfiguration == 0`: the channel layout is given explicitly by a
                    // `program_config_element()` at this exact point in `GASpecificConfig()`
                    // (ISO/IEC 14496-3 §1.6.2.1, Table 1.15).
                    let (channels, elements) = Self::read_program_config_element(bs)?;
                    asc.channels = Some(channels);
                    asc.channel_elements = Some(elements);
                }

                if (asc.object_type == AudioObjectType::Scalable)
                    || (asc.object_type == AudioObjectType::ErAacScalable)
                {
                    let _layer = bs.read_bits_leq32(3)?;
                }

                if extension_flag {
                    if asc.object_type == AudioObjectType::ErBsac {
                        let _num_subframes = bs.read_bits_leq32(5)? as usize;
                        let _layer_length = bs.read_bits_leq32(11)?;
                    }

                    if (asc.object_type == AudioObjectType::ErAacLc)
                        || (asc.object_type == AudioObjectType::ErAacLtp)
                        || (asc.object_type == AudioObjectType::ErAacScalable)
                        || (asc.object_type == AudioObjectType::ErAacLd)
                    {
                        let section_data_resilience = bs.read_bool()?;
                        let scalefactors_resilience = bs.read_bool()?;
                        let spectral_data_resilience = bs.read_bool()?;

                        asc.er_resilience = section_data_resilience
                            || scalefactors_resilience
                            || spectral_data_resilience;
                    }

                    let extension_flag3 = bs.read_bool()?;

                    if extension_flag3 {
                        return unsupported_error("common (mp4a): version3 extensions");
                    }
                }
            }
            AudioObjectType::Celp => {
                return unsupported_error("common (mp4a): CELP config");
            }
            AudioObjectType::Hvxc => {
                return unsupported_error("common (mp4a): HVXC config");
            }
            AudioObjectType::Ttsi => {
                return unsupported_error("common (mp4a): TTS config");
            }
            AudioObjectType::MainSynth
            | AudioObjectType::WavetableSynth
            | AudioObjectType::GeneralMidi
            | AudioObjectType::Algorithmic => {
                return unsupported_error("common (mp4a): structured audio config");
            }
            AudioObjectType::ErCelp => {
                return unsupported_error("common (mp4a): ER CELP config");
            }
            AudioObjectType::ErHvxc => {
                return unsupported_error("common (mp4a): ER HVXC config");
            }
            AudioObjectType::ErHiln | AudioObjectType::ErParametric => {
                return unsupported_error("common (mp4a): parametric config");
            }
            AudioObjectType::Ssc => {
                return unsupported_error("common (mp4a): SSC config");
            }
            AudioObjectType::MpegSurround => {
                // bs.ignore_bits(1)?; // sacPayloadEmbedding
                return unsupported_error("common (mp4a): MPEG Surround config");
            }
            AudioObjectType::Layer1 | AudioObjectType::Layer2 | AudioObjectType::Layer3 => {
                return unsupported_error("common (mp4a): MPEG Layer 1/2/3 config");
            }
            AudioObjectType::Dst => {
                return unsupported_error("common (mp4a): DST config");
            }
            AudioObjectType::Als => {
                // bs.ignore_bits(5)?; // fillBits
                return unsupported_error("common (mp4a): ALS config");
            }
            AudioObjectType::Sls | AudioObjectType::SlsNonCore => {
                return unsupported_error("common (mp4a): SLS config");
            }
            AudioObjectType::ErAacEld => {
                // ELDSpecificConfig
                let short_frame = bs.read_bool()?;

                asc.samples = if short_frame { 480 } else { 512 };

                let section_data_resilience = bs.read_bool()?;
                let scalefactors_resilience = bs.read_bool()?;
                let spectral_data_resilience = bs.read_bool()?;

                asc.er_resilience =
                    section_data_resilience || scalefactors_resilience || spectral_data_resilience;

                // ldSbrPresentFlag
                if bs.read_bool()? {
                    let dual_rate = bs.read_bool()?;
                    let crc = bs.read_bool()?;

                    let Some(elements) = asc.channel_elements.as_deref()
                    else {
                        return unsupported_error(
                            "common (mp4a): ELD with SBR requires a predefined channel configuration",
                        );
                    };

                    let num_headers = eld_num_sbr_headers(elements);

                    let mut headers = Vec::with_capacity(num_headers);

                    for _ in 0..num_headers {
                        headers.push(SbrHeaderConfig::read(bs)?);
                    }

                    asc.sbr_present = true;
                    asc.sbr_ps_info =
                        Some((if dual_rate { asc.sample_rate * 2 } else { asc.sample_rate }, None));
                    asc.eld_sbr = Some(EldSbrConfig { dual_rate, crc, headers });
                }

                // ELDEXT: skip the extensions.
                loop {
                    let ext_type = bs.read_bits_leq32(4)?;

                    // ELDEXT_TERM
                    if ext_type == 0 {
                        break;
                    }

                    let mut len = bs.read_bits_leq32(4)?;

                    if len == 15 {
                        let len_add = bs.read_bits_leq32(8)?;
                        len += len_add;

                        if len_add == 255 {
                            len += bs.read_bits_leq32(16)?;
                        }
                    }

                    bs.ignore_bits(len * 8)?;
                }
            }
            AudioObjectType::SmrSimple | AudioObjectType::SmrMain => {
                return unsupported_error("common (mp4a): symbolic music config");
            }
            _ => {}
        };

        match asc.object_type {
            AudioObjectType::ErAacLc
            | AudioObjectType::ErAacLtp
            | AudioObjectType::ErAacScalable
            | AudioObjectType::ErTwinVq
            | AudioObjectType::ErBsac
            | AudioObjectType::ErAacLd
            | AudioObjectType::ErCelp
            | AudioObjectType::ErHvxc
            | AudioObjectType::ErHiln
            | AudioObjectType::ErParametric
            | AudioObjectType::ErAacEld => {
                let ep_config = bs.read_bits_leq32(2)?;

                if (ep_config == 2) || (ep_config == 3) {
                    return unsupported_error("common (mp4a): error protection config");
                }
                // if ep_config == 3 {
                //     let direct_mapping = bs.read_bit()?;
                //     validate!(direct_mapping);
                // }
            }
            _ => {}
        };

        Ok(asc)
    }

    /// Read the optional trailing `syncExtensionType` extension of an audio specific config, which
    /// is how "explicit backwards compatible" HE-AAC v1/v2 signals SBR/PS on top of a plain
    /// (non-hierarchical) outer `audioObjectType`, from the bit stream.
    ///
    /// The bit stream is read past the extension by up to 11 bits (the sync word that is not
    /// there). Returns the number of bits that were part of the extension (0 if there was none),
    /// so that a caller that reads an audio specific config that is not the last element of a
    /// bit stream can skip only those.
    pub fn read_sync_extension<B: ReadBitsLtr + FiniteBitStream>(
        &mut self,
        bs: &mut B,
    ) -> Result<u64> {
        let asc = self;
        let start_left = bs.bits_left();
        let mut unmatched = 0;

        // §1.6.6: this trailing `syncExtensionType` check is unconditional on `bits_left()
        // >= 16` -- it is how "explicit backwards compatible" HE-AAC v1/v2 signals SBR/PS on
        // top of a plain (non-hierarchical) outer `audioObjectType` (e.g. `Lc`), so it must
        // not be gated on `asc.sbr_ps_info.is_some()` (which is only set by the *hierarchical*
        // `audioObjectType == 5/29` branch above). A real Fraunhofer HE-AAC v2 fixture
        // (`SBRtestStereoAot29Sig1.mp4`) has outer `object_type == Lc`, `channels == 1`, and
        // conveys both SBR and PS only through this trailing extension.
        if bs.bits_left() >= 16 {
            let sync = bs.read_bits_leq32(11)?;

            if sync != 0x2B7 {
                unmatched = 11;
            }
            else {
                let ext_otype = Self::read_audio_object_type(bs)?;
                if ext_otype == AudioObjectType::Sbr {
                    asc.sbr_present = bs.read_bool()?;
                    if asc.sbr_present {
                        let ext_srate = Self::read_sampling_frequency(bs)?;
                        // Backwards-compatible explicit signalling also conveys the SBR output
                        // sampling frequency.
                        if asc.sbr_ps_info.is_none() {
                            asc.sbr_ps_info = Some((ext_srate, None));
                        }
                        if bs.bits_left() >= 12 {
                            let sync = bs.read_bits_leq32(11)?;
                            if sync == 0x548 {
                                asc.ps_present = bs.read_bool()?;
                            }
                            else {
                                unmatched = 11;
                            }
                        }
                    }
                }
                if ext_otype == AudioObjectType::Ps {
                    asc.sbr_present = bs.read_bool()?;
                    if asc.sbr_present {
                        let _ext_srate = Self::read_sampling_frequency(bs)?;
                    }
                    let _ext_channels = bs.read_bits_leq32(4)?;
                }
            }
        }

        Ok(start_left - bs.bits_left() - unmatched)
    }

    fn read_audio_object_type<B: ReadBitsLtr>(bs: &mut B) -> Result<AudioObjectType> {
        let index = match bs.read_bits_leq32(5)? {
            index if index < 31 => index as usize,
            31 => (bs.read_bits_leq32(6)? + 32) as usize,
            _ => unreachable!(),
        };

        let aot = AUDIO_OBJECT_TYPES.get(index).copied().unwrap_or(AudioObjectType::Unknown);

        Ok(aot)
    }

    fn read_sampling_frequency<B: ReadBitsLtr>(bs: &mut B) -> Result<u32> {
        let index = bs.read_bits_leq32(4)?;
        let rate = match get_mpeg4_audio_sample_rate_by_index(index) {
            Mpeg4AudioSampleRate::SampleRate(rate) => rate,
            Mpeg4AudioSampleRate::Escape => bs.read_bits_leq32(24)?,
            Mpeg4AudioSampleRate::Invalid => {
                return decode_error("common (mp4a): invalid sample rate");
            }
        };
        Ok(rate)
    }

    fn read_channel_config<B: ReadBitsLtr>(
        bs: &mut B,
    ) -> Result<(Option<Channels>, Option<Vec<ChannelElement>>)> {
        let index = bs.read_bits_leq32(4)?;
        let channels = match get_mpeg4_audio_channels_by_config_index(index) {
            Mpeg4AudioChannels::Channels(channels) => Some(channels),
            Mpeg4AudioChannels::Escape => None,
            Mpeg4AudioChannels::Invalid => {
                return decode_error("common (mp4a): invalid channel configuration");
            }
        };
        let elements = default_channel_elements(index);
        Ok((channels, elements))
    }

    /// Parse `program_config_element()` and map its elements onto channel positions. See
    /// [`ProgramConfig`].
    fn read_program_config_element<B: ReadBitsLtr>(
        bs: &mut B,
    ) -> Result<(Channels, Vec<ChannelElement>)> {
        ProgramConfig::read(bs)?.layout()
    }
}

/// The syntax of a `program_config_element()` (ISO/IEC 14496-3 §4.4.1.1): the part of it that
/// determines the syntactic elements of the program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramConfig {
    /// The `profile` (2 bits): the audio object type minus 1 (0 is AAC Main, 1 is AAC LC, 2 is AAC
    /// SSR, 3 is AAC LTP).
    pub object_type: u32,
    /// The index of the sampling frequency.
    pub sampling_frequency_index: u32,
    /// For each front element, in order, true if it is a channel pair element.
    pub front_is_cpe: Vec<bool>,
    /// For each side element, in order, true if it is a channel pair element.
    pub side_is_cpe: Vec<bool>,
    /// For each back element, in order, true if it is a channel pair element.
    pub back_is_cpe: Vec<bool>,
    /// The number of LFE elements.
    pub num_lfe: u32,
    /// The number of associated data elements.
    pub num_assoc_data: u32,
    /// The number of valid coupling channel elements.
    pub num_valid_cc: u32,
}

impl ProgramConfig {
    /// Read a `program_config_element()` (ISO/IEC 14496-3 §4.4.1.1, Table 4.2 / Table 8.2) from the
    /// current position of a bit stream. The byte alignment before the comment field is relative
    /// to the start of the bit stream.
    ///
    /// Mixdown coefficients, comment fields, and element instance tags (used to associate
    /// elements with mixdowns/CCEs) are parsed for correct bitstream alignment but otherwise
    /// discarded; the element order alone determines how `raw_data_block()` syntactic elements
    /// map onto output channels (see [`ChannelElement`]).
    pub fn read<B: ReadBitsLtr>(bs: &mut B) -> Result<ProgramConfig> {
        let _element_instance_tag = bs.read_bits_leq32(4)?;
        let object_type = bs.read_bits_leq32(2)?;
        let sampling_frequency_index = bs.read_bits_leq32(4)?;

        let num_front = bs.read_bits_leq32(4)?;
        let num_side = bs.read_bits_leq32(4)?;
        let num_back = bs.read_bits_leq32(4)?;
        let num_lfe = bs.read_bits_leq32(2)?;
        let num_assoc_data = bs.read_bits_leq32(3)?;
        let num_valid_cc = bs.read_bits_leq32(4)?;

        if bs.read_bool()? {
            let _mono_mixdown_element_number = bs.read_bits_leq32(4)?;
        }
        if bs.read_bool()? {
            let _stereo_mixdown_element_number = bs.read_bits_leq32(4)?;
        }
        if bs.read_bool()? {
            let _matrix_mixdown_idx = bs.read_bits_leq32(2)?;
            let _pseudo_surround_enable = bs.read_bool()?;
        }

        let mut front_is_cpe = Vec::with_capacity(num_front as usize);
        for _ in 0..num_front {
            front_is_cpe.push(bs.read_bool()?);
            let _tag = bs.read_bits_leq32(4)?;
        }
        let mut side_is_cpe = Vec::with_capacity(num_side as usize);
        for _ in 0..num_side {
            side_is_cpe.push(bs.read_bool()?);
            let _tag = bs.read_bits_leq32(4)?;
        }
        let mut back_is_cpe = Vec::with_capacity(num_back as usize);
        for _ in 0..num_back {
            back_is_cpe.push(bs.read_bool()?);
            let _tag = bs.read_bits_leq32(4)?;
        }
        for _ in 0..num_lfe {
            let _tag = bs.read_bits_leq32(4)?;
        }
        for _ in 0..num_assoc_data {
            let _tag = bs.read_bits_leq32(4)?;
        }
        for _ in 0..num_valid_cc {
            let _is_ind_sw = bs.read_bool()?;
            let _tag = bs.read_bits_leq32(4)?;
        }

        bs.realign();

        let comment_field_bytes = bs.read_bits_leq32(8)?;
        for _ in 0..comment_field_bytes {
            bs.ignore_bits(8)?;
        }

        Ok(ProgramConfig {
            object_type,
            sampling_frequency_index,
            front_is_cpe,
            side_is_cpe,
            back_is_cpe,
            num_lfe,
            num_assoc_data,
            num_valid_cc,
        })
    }

    /// Map the syntactic elements of the program onto channel positions.
    ///
    /// Assigns each declared front/side/back/LFE element a channel position using the
    /// conventional ordering used throughout the industry for the common layouts described
    /// informatively in ISO/IEC 14496-3 subclause 8.5.3: the first front `SCE` is the
    /// front-center channel; front `CPE`s are assigned outward from the front-left/right pair to
    /// the front-left/right-of-center "wide" pair; side `CPE`s are the side-left/right pair; the
    /// first back `CPE` is the rear-left/right pair, optionally followed by a rear-center `SCE`;
    /// LFE `SCE`s are LFE1, then LFE2. Layouts that don't fit this convention (e.g. more than one
    /// front SCE, or a side SCE) are rejected as unsupported rather than silently mis-assigned.
    pub fn layout(&self) -> Result<(Channels, Vec<ChannelElement>)> {
        let ProgramConfig { front_is_cpe, side_is_cpe, back_is_cpe, num_lfe, .. } = self;
        let num_lfe = *num_lfe;

        let mut elements = Vec::new();
        let mut mask = Position::empty();

        let mut front_sce_seen = false;
        let mut front_cpe_seen = 0u32;

        for &is_cpe in front_is_cpe {
            if is_cpe {
                let (l, r) = match front_cpe_seen {
                    0 => (Position::FRONT_LEFT, Position::FRONT_RIGHT),
                    1 => (Position::FRONT_LEFT_CENTER, Position::FRONT_RIGHT_CENTER),
                    _ => {
                        return unsupported_error(
                            "common (mp4a): PCE front channel layout too complex",
                        );
                    }
                };
                front_cpe_seen += 1;
                elements.push(ChannelElement::Pair(l, r));
                mask |= l | r;
            }
            else {
                if front_sce_seen {
                    return unsupported_error(
                        "common (mp4a): PCE front channel layout too complex",
                    );
                }
                front_sce_seen = true;
                elements.push(ChannelElement::Single(Position::FRONT_CENTER));
                mask |= Position::FRONT_CENTER;
            }
        }

        let mut side_cpe_seen = false;
        for &is_cpe in side_is_cpe {
            if !is_cpe || side_cpe_seen {
                return unsupported_error("common (mp4a): PCE side channel layout too complex");
            }
            side_cpe_seen = true;
            elements.push(ChannelElement::Pair(Position::SIDE_LEFT, Position::SIDE_RIGHT));
            mask |= Position::SIDE_LEFT | Position::SIDE_RIGHT;
        }

        // A second back `CPE` is only meaningful alongside a first one, in which case the pair
        // that is listed first is the side surround pair (e.g. Fraunhofer FDK writes a 7.1 stream
        // as front `SCE` + `CPE`, back `CPE` + `CPE`, `LFE`) and the pair that is listed last is
        // the rear surround pair. This is only done when the PCE does not declare a side pair of
        // its own.
        let back_cpe_count = back_is_cpe.iter().filter(|&&is_cpe| is_cpe).count();
        let mut back_cpe_seen = 0usize;
        let mut back_sce_seen = false;
        for &is_cpe in back_is_cpe {
            if is_cpe {
                let (l, r) = match (back_cpe_count, back_cpe_seen) {
                    (1, 0) => (Position::REAR_LEFT, Position::REAR_RIGHT),
                    (2, 0) if !side_cpe_seen => (Position::SIDE_LEFT, Position::SIDE_RIGHT),
                    (2, 1) if !side_cpe_seen => (Position::REAR_LEFT, Position::REAR_RIGHT),
                    _ => {
                        return unsupported_error(
                            "common (mp4a): PCE back channel layout too complex",
                        );
                    }
                };
                back_cpe_seen += 1;
                elements.push(ChannelElement::Pair(l, r));
                mask |= l | r;
            }
            else {
                if back_sce_seen {
                    return unsupported_error("common (mp4a): PCE back channel layout too complex");
                }
                back_sce_seen = true;
                elements.push(ChannelElement::Single(Position::REAR_CENTER));
                mask |= Position::REAR_CENTER;
            }
        }

        for i in 0..num_lfe {
            let pos = match i {
                0 => Position::LFE1,
                1 => Position::LFE2,
                _ => return unsupported_error("common (mp4a): PCE has too many LFE channels"),
            };
            elements.push(ChannelElement::Single(pos));
            mask |= pos;
        }

        if mask.is_empty() {
            return decode_error("common (mp4a): PCE declares no channels");
        }

        Ok((Channels::Positioned(mask), elements))
    }
}

/// The maximum number of frames [`aac_seek_start_frame`] starts decoding before the target.
pub const AAC_SEEK_MAX_PREROLL_FRAMES: u64 = 56;

/// Get the index of the AAC frame (packet) to start decoding from, after a decoder reset, to
/// reproduce a continuous decode from frame `target` onwards. The frame indices are relative to
/// the start of the stream (the last SBR reset).
///
/// * AAC-LC needs the previous frame for the MDCT overlap-add.
/// * With SBR (HE-AAC), the noise and sinusoid phase indices run through a 512 entry table with
///   a period of at most 16 frames, which the decoder restarts at the position given by the
///   leading border of the first frame it decodes. This is only correct for a frame that is a
///   multiple of 16 frames from the start of the stream. The envelope, gain, and filterbank
///   state take about 8 frames to settle, and the delta coded envelopes only resynchronise at
///   the next frame coded independently of its predecessor, which encoders insert regularly
///   (but, e.g., Nero's less often than FDK's). So, decode from the multiple of 16 frames that
///   is at least 41 frames before the target, up to 56 frames in total. This converges to the
///   continuous decode exactly for the HE-AAC v1 and v2 streams of FDK and Nero tested.
pub fn aac_seek_start_frame(target: u64, sbr: bool) -> u64 {
    aac_seek_start_frame_with_overlap(target, sbr, 1)
}

/// The number of previous frames whose spectra the output of a frame of an object type depends on
/// (the overlap of its filterbank): one for AAC-LC and AAC-LD, and three for AAC-ELD, whose
/// window spans four frames.
pub fn aac_overlap_frames(object_type: AudioObjectType) -> u64 {
    match object_type {
        AudioObjectType::ErAacEld => 3,
        _ => 1,
    }
}

/// As [`aac_seek_start_frame`], for a stream whose filterbank overlaps `overlap` previous frames
/// (see [`aac_overlap_frames`]).
pub fn aac_seek_start_frame_with_overlap(target: u64, sbr: bool, overlap: u64) -> u64 {
    aac_seek_start_frame_with_period(target, sbr, overlap, AAC_SBR_PHASE_PERIOD)
}

/// The number of frames of the period of the noise and sinusoid phase of SBR in HE-AAC.
const AAC_SBR_PHASE_PERIOD: u64 = 16;

/// The number of frames before the target that the decoding of SBR starts from, at the most
/// (the settling of the envelope, gain and filterbank state).
const AAC_SBR_PREROLL_FRAMES: u64 = 41;

/// The period, in frames, of the noise and sinusoid phase of the SBR of a stream: the number of
/// frames after which the phase indices are back at their start.
///
/// The noise table has 512 entries, and the phase advances by the number of SBR bands (`M`) for
/// each of the 32 QMF slots of a frame of HE-AAC, which makes a period of 16 frames. In the low
/// delay SBR of AAC-ELD the phase advances for each of the 16 (frame length of 512) or 15 (480)
/// slots, which makes 32 frames for the 512 samples (at most, depending on `M`), but, with an odd
/// number of slots, 512 frames for the 480 samples.
pub fn aac_sbr_phase_period(asc: &AudioSpecificConfig) -> u64 {
    match (asc.object_type, asc.samples) {
        (AudioObjectType::ErAacEld, 512) => 32,
        (AudioObjectType::ErAacEld, 480) => 512,
        _ => AAC_SBR_PHASE_PERIOD,
    }
}

/// The number of frames [`aac_seek_start_frame_with_period`] starts decoding before the target, at
/// most, for streams with SBR.
pub fn aac_seek_max_preroll_frames(phase_period: u64) -> u64 {
    AAC_SBR_PREROLL_FRAMES + phase_period - 1
}

/// As [`aac_seek_start_frame_with_overlap`], for a stream whose SBR phase repeats every
/// `phase_period` frames (see [`aac_sbr_phase_period`]).
pub fn aac_seek_start_frame_with_period(
    target: u64,
    sbr: bool,
    overlap: u64,
    phase_period: u64,
) -> u64 {
    if sbr {
        let phase_period = phase_period.max(1);
        (target.saturating_sub(AAC_SBR_PREROLL_FRAMES) / phase_period) * phase_period
    }
    else {
        target.saturating_sub(overlap)
    }
}

pub fn get_audio_codec_profile(asc: &AudioSpecificConfig) -> Option<CodecProfile> {
    match asc.object_type {
        AudioObjectType::Main => Some(CODEC_PROFILE_AAC_MAIN),
        AudioObjectType::Ssr => Some(CODEC_PROFILE_AAC_SSR),
        AudioObjectType::Ltp => Some(CODEC_PROFILE_AAC_LTP),
        AudioObjectType::Lc => {
            if asc.ps_present {
                Some(CODEC_PROFILE_AAC_HE_V2)
            }
            else if asc.sbr_present {
                Some(CODEC_PROFILE_AAC_HE)
            }
            else {
                Some(CODEC_PROFILE_AAC_LC)
            }
        }
        AudioObjectType::ErAacLd => Some(CODEC_PROFILE_AAC_LD),
        AudioObjectType::ErAacEld => Some(CODEC_PROFILE_AAC_ELD),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FDK_7P1_PCE: [u8; 11] =
        [0x11, 0x80, 0x04, 0xc8, 0x09, 0x00, 0x01, 0x08, 0xc8, 0x00, 0x00];

    fn elements_7p1() -> Vec<ChannelElement> {
        vec![
            ChannelElement::Single(Position::FRONT_CENTER),
            ChannelElement::Pair(Position::FRONT_LEFT, Position::FRONT_RIGHT),
            ChannelElement::Pair(Position::SIDE_LEFT, Position::SIDE_RIGHT),
            ChannelElement::Pair(Position::REAR_LEFT, Position::REAR_RIGHT),
            ChannelElement::Single(Position::LFE1),
        ]
    }

    #[test]
    fn pce_with_two_back_pairs_is_7p1() {
        // Fraunhofer FDK writes 7.1 as a PCE with front SCE + CPE, two back CPEs, and an LFE.
        let asc = AudioSpecificConfig::read(&FDK_7P1_PCE).expect("valid asc");
        assert_eq!(asc.channels, Some(layouts::CHANNEL_LAYOUT_7P1));
        assert_eq!(asc.channel_elements, Some(elements_7p1()));
    }

    #[test]
    fn channel_configuration_12_is_7p1() {
        // AAC-LC, 48 kHz, channelConfiguration 12.
        let asc = AudioSpecificConfig::read(&[0x11, 0xe0]).expect("valid asc");
        assert_eq!(asc.channels, Some(layouts::CHANNEL_LAYOUT_7P1));
        assert_eq!(asc.channel_elements, Some(elements_7p1()));
    }

    #[test]
    fn channel_configuration_11_is_6p1() {
        // AAC-LC, 48 kHz, channelConfiguration 11.
        let asc = AudioSpecificConfig::read(&[0x11, 0xd8]).expect("valid asc");
        assert_eq!(asc.channels, Some(layouts::CHANNEL_LAYOUT_AAC_6P1));
        assert_eq!(asc.channel_elements.map(|e| e.len()), Some(5));
    }

    #[test]
    fn aac_seek_start_frames() {
        assert_eq!(aac_seek_start_frame(0, false), 0);
        assert_eq!(aac_seek_start_frame(100, false), 99);
        assert_eq!(aac_seek_start_frame_with_overlap(100, false, 3), 97);
        assert_eq!(aac_seek_start_frame_with_overlap(2, false, 3), 0);
        assert_eq!(aac_overlap_frames(AudioObjectType::ErAacEld), 3);
        assert_eq!(aac_overlap_frames(AudioObjectType::ErAacLd), 1);
        assert_eq!(aac_overlap_frames(AudioObjectType::Lc), 1);
        assert_eq!(aac_seek_start_frame(5, true), 0);
        assert_eq!(aac_seek_start_frame(79, true), 32);
        assert_eq!(aac_seek_start_frame(100, true), 48);
        for target in 0..1000 {
            let start = aac_seek_start_frame(target, true);
            assert!(start <= target && target - start <= AAC_SEEK_MAX_PREROLL_FRAMES);
        }
    }

    #[test]
    fn seek_start_frame_with_the_sbr_phase_period_of_low_delay_sbr() {
        let asc = |object_type, samples| AudioSpecificConfig {
            object_type,
            samples,
            ..Default::default()
        };

        assert_eq!(aac_sbr_phase_period(&asc(AudioObjectType::Lc, 1024)), 16);
        assert_eq!(aac_sbr_phase_period(&asc(AudioObjectType::ErAacEld, 512)), 32);
        assert_eq!(aac_sbr_phase_period(&asc(AudioObjectType::ErAacEld, 480)), 512);
        assert_eq!(aac_seek_max_preroll_frames(16), AAC_SEEK_MAX_PREROLL_FRAMES);

        for period in [16, 32, 512] {
            for target in [0, 1, 40, 41, 100, 1000, 5000] {
                let start = aac_seek_start_frame_with_period(target, true, 3, period);
                assert_eq!(start % period, 0);
                assert!(start <= target);
                assert!(target - start <= aac_seek_max_preroll_frames(period));
                // At least 41 frames before the target, unless at the stream start.
                assert!(start == 0 || target - start >= 41);
            }
        }

        // Without SBR, the period does not matter.
        assert_eq!(aac_seek_start_frame_with_period(100, false, 3, 512), 97);
    }

    #[test]
    fn he_aac_output_format() {
        // HE-AAC v1: 22.05 kHz stereo core, explicit SBR at 44.1 kHz.
        let asc = AudioSpecificConfig::read(&[0x13, 0x90, 0x56, 0xe5, 0xa0]).expect("valid asc");
        assert_eq!(asc.sample_rate, 22_050);
        assert_eq!(asc.output_sample_rate(), 44_100);
        assert_eq!(asc.output_channels(), Some(layouts::CHANNEL_LAYOUT_STEREO));

        // HE-AAC v2: 22.05 kHz mono core, SBR + PS.
        let asc = AudioSpecificConfig::read(&[0x13, 0x88, 0x56, 0xe5, 0xa5, 0x48, 0x80])
            .expect("valid asc");
        assert_eq!(asc.sample_rate, 22_050);
        assert_eq!(asc.output_sample_rate(), 44_100);
        assert_eq!(asc.output_channels(), Some(layouts::CHANNEL_LAYOUT_STEREO));

        // Downsampled SBR: 44.1 kHz core with a 44.1 kHz SBR output rate.
        let asc = AudioSpecificConfig::read(&[0x12, 0x10, 0x56, 0xe5, 0xa0]).expect("valid asc");
        assert_eq!(asc.output_sample_rate(), 44_100);

        // Plain AAC-LC.
        let asc = AudioSpecificConfig::read(&[0x12, 0x10]).expect("valid asc");
        assert_eq!(asc.output_sample_rate(), 44_100);
    }

    #[test]
    fn aac_ld_config() {
        // AAC LD, 44.1 kHz, mono, frame length 512: the config of a stream encoded by FDK.
        let asc = AudioSpecificConfig::read(&[0xba, 0x09, 0x00]).expect("valid asc");
        assert_eq!(asc.object_type, AudioObjectType::ErAacLd);
        assert_eq!(asc.sample_rate, 44_100);
        assert_eq!(asc.channels, Some(layouts::CHANNEL_LAYOUT_MONO));
        assert_eq!(asc.samples, 512);
        assert!(!asc.er_resilience);
        assert!(!asc.sbr_present);
        assert_eq!(get_audio_codec_profile(&asc), Some(CODEC_PROFILE_AAC_LD));

        // The same with a frame length of 480.
        let asc = AudioSpecificConfig::read(&[0xba, 0x0d, 0x00]).expect("valid asc");
        assert_eq!(asc.object_type, AudioObjectType::ErAacLd);
        assert_eq!(asc.samples, 480);

        // Stereo, 48 kHz, frame length 480.
        let asc = AudioSpecificConfig::read(&[0xb9, 0x95, 0x00]).expect("valid asc");
        assert_eq!(asc.sample_rate, 48_000);
        assert_eq!(asc.channels, Some(layouts::CHANNEL_LAYOUT_STEREO));
        assert_eq!(asc.samples, 480);
    }

    #[test]
    fn aac_ld_resilience_flags() {
        // AAC LD, 44.1 kHz, mono, frame length 512, with the spectral data resilience flag set
        // (the config of FDK has all of the flags clear).
        let asc = AudioSpecificConfig::read(&[0xba, 0x09, 0x20]).expect("valid asc");
        assert_eq!(asc.object_type, AudioObjectType::ErAacLd);
        assert!(asc.er_resilience);
    }

    #[test]
    fn aac_eld_config() {
        // AAC ELD, 44.1 kHz, mono, frame length 512, without SBR: the config of a stream encoded
        // by FDK.
        let asc = AudioSpecificConfig::read(&[0xf8, 0xe8, 0x20, 0x00]).expect("valid asc");
        assert_eq!(asc.object_type, AudioObjectType::ErAacEld);
        assert_eq!(asc.sample_rate, 44_100);
        assert_eq!(asc.output_sample_rate(), 44_100);
        assert_eq!(asc.channels, Some(layouts::CHANNEL_LAYOUT_MONO));
        assert_eq!(asc.samples, 512);
        assert!(!asc.er_resilience);
        assert!(!asc.sbr_present);
        assert_eq!(asc.eld_sbr, None);
        assert_eq!(get_audio_codec_profile(&asc), Some(CODEC_PROFILE_AAC_ELD));

        // Frame length 480, stereo, 48 kHz.
        let asc = AudioSpecificConfig::read(&[0xf8, 0xe6, 0x50, 0x00]).expect("valid asc");
        assert_eq!(asc.sample_rate, 48_000);
        assert_eq!(asc.channels, Some(layouts::CHANNEL_LAYOUT_STEREO));
        assert_eq!(asc.samples, 480);
    }

    #[test]
    fn aac_eld_extensions_are_skipped() {
        // An ELD config (44.1 kHz, mono, 512) with one extension of type 2 and 3 bytes, followed
        // by the terminator and epConfig 0.
        //
        // 11111 000111 0100 0001 | 0 000 0 | 0010 0011 aabb cc | 0000 | 00
        let asc =
            AudioSpecificConfig::read(&[0xf8, 0xe8, 0x20, 0x00, 0x23, 0xaa, 0xbb, 0xcc, 0x00])
                .expect("valid asc");
        assert_eq!(asc.object_type, AudioObjectType::ErAacEld);
        assert_eq!(asc.samples, 512);
        assert!(!asc.er_resilience);
    }

    #[test]
    fn aac_eld_sbr_config() {
        // ELD at 24 kHz with SBR at 48 kHz (dual rate), mono, with CRC off.
        let mut bits: Vec<(u32, u32)> = vec![
            (31, 5),
            (7, 6), // AOT 39
            (6, 4), // 24 kHz
            (1, 4), // mono
            (0, 1), // frameLengthFlag
            (0, 3), // resilience flags
            (1, 1), // ldSbrPresentFlag
            (1, 1), // ldSbrSamplingRate
            (0, 1), // ldSbrCrcFlag
            (1, 1), // bs_amp_res
            (5, 4), // bs_start_freq
            (9, 4), // bs_stop_freq
            (1, 3), // bs_xover_band
            (0, 2), // bs_reserved
            (1, 1), // bs_header_extra_1
            (0, 1), // bs_header_extra_2
            (3, 2), // bs_freq_scale
            (0, 1), // bs_alter_scale
            (1, 2), // bs_noise_bands
            (0, 4), // ELDEXT_TERM
            (0, 2), // epConfig
        ];

        let mut asc_bytes = vec![];
        let mut acc = 0u32;
        let mut n = 0;

        for (value, len) in bits.drain(..) {
            for i in (0..len).rev() {
                acc = (acc << 1) | ((value >> i) & 1);
                n += 1;

                if n == 8 {
                    asc_bytes.push(acc as u8);
                    acc = 0;
                    n = 0;
                }
            }
        }

        if n > 0 {
            asc_bytes.push((acc << (8 - n)) as u8);
        }

        let asc = AudioSpecificConfig::read(&asc_bytes).expect("valid asc");

        assert_eq!(asc.object_type, AudioObjectType::ErAacEld);
        assert_eq!(asc.sample_rate, 24_000);
        assert!(asc.sbr_present);
        assert_eq!(asc.output_sample_rate(), 48_000);

        let sbr = asc.eld_sbr.expect("ld sbr config");
        assert!(sbr.dual_rate);
        assert!(!sbr.crc);
        assert_eq!(sbr.headers.len(), 1);

        let header = sbr.headers[0];
        assert!(header.amp_res);
        assert_eq!((header.start_freq, header.stop_freq, header.xover_band), (5, 9, 1));
        assert_eq!((header.freq_scale, header.alter_scale, header.noise_bands), (3, false, 1));
        // The fields that were not transmitted have their defaults.
        assert_eq!((header.limiter_bands, header.limiter_gains), (2, 2));
        assert!(header.interpol_freq && header.smoothing_mode);
    }
}
