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

/// The syntactic element order for the predefined `channelConfiguration` values 1-7 of
/// ISO/IEC 14496-3 Table 1.19. Returns `None` for the "escape" value (0, use
/// `program_config_element()`) or any reserved/invalid index.
fn default_channel_elements(index: u32) -> Option<Vec<ChannelElement>> {
    use ChannelElement::{Pair, Single};

    let elements = match index {
        1 => vec![Single(Position::FRONT_CENTER)],
        2 => vec![Pair(Position::FRONT_LEFT, Position::FRONT_RIGHT)],
        3 => vec![Single(Position::FRONT_CENTER), Pair(Position::FRONT_LEFT, Position::FRONT_RIGHT)],
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
    pub sbr_ps_info: Option<(u32, Option<Channels>)>,
    pub sbr_present: bool,
    pub ps_present: bool,
}

impl AudioSpecificConfig {
    /// Read the audio specific configuration from the provided buffer. ISO14496-3-2009
    pub fn read(buf: &[u8]) -> Result<AudioSpecificConfig> {
        let mut bs = BitReaderLtr::new(buf);

        let mut asc = AudioSpecificConfig {
            object_type: Self::read_audio_object_type(&mut bs)?,
            sample_rate: Self::read_sampling_frequency(&mut bs)?,
            ..Default::default()
        };

        if asc.sample_rate == 0 {
            return decode_error("common (mp4a): a sample rate of 0 is invalid");
        }

        let (channels, channel_elements) = Self::read_channel_config(&mut bs)?;
        asc.channels = channels;
        asc.channel_elements = channel_elements;

        if (asc.object_type == AudioObjectType::Sbr) || (asc.object_type == AudioObjectType::Ps) {
            asc.sbr_present = true;
            if asc.object_type == AudioObjectType::Ps {
                asc.ps_present = true;
            }
            let ext_srate = Self::read_sampling_frequency(&mut bs)?;
            asc.object_type = Self::read_audio_object_type(&mut bs)?;

            let ext_chans = if asc.object_type == AudioObjectType::ErBsac {
                Self::read_channel_config(&mut bs)?.0
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

                asc.samples = if short_frame { 960 } else { 1024 };

                let depends_on_core = bs.read_bool()?;

                if depends_on_core {
                    let _delay = bs.read_bits_leq32(14)?;
                }

                let extension_flag = bs.read_bool()?;

                if asc.channels.is_none() {
                    // `channelConfiguration == 0`: the channel layout is given explicitly by a
                    // `program_config_element()` at this exact point in `GASpecificConfig()`
                    // (ISO/IEC 14496-3 §1.6.2.1, Table 1.15).
                    let (channels, elements) = Self::read_program_config_element(&mut bs)?;
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
                        let _section_data_resilience = bs.read_bool()?;
                        let _scalefactors_resilience = bs.read_bool()?;
                        let _spectral_data_resilience = bs.read_bool()?;
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
                return unsupported_error("common (mp4a): ELD config");
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

        if asc.sbr_ps_info.is_some() && (bs.bits_left() >= 16) {
            let sync = bs.read_bits_leq32(11)?;

            if sync == 0x2B7 {
                let ext_otype = Self::read_audio_object_type(&mut bs)?;
                if ext_otype == AudioObjectType::Sbr {
                    asc.sbr_present = bs.read_bool()?;
                    if asc.sbr_present {
                        let _ext_srate = Self::read_sampling_frequency(&mut bs)?;
                        if bs.bits_left() >= 12 {
                            let sync = bs.read_bits_leq32(11)?;
                            if sync == 0x548 {
                                asc.ps_present = bs.read_bool()?;
                            }
                        }
                    }
                }
                if ext_otype == AudioObjectType::Ps {
                    asc.sbr_present = bs.read_bool()?;
                    if asc.sbr_present {
                        let _ext_srate = Self::read_sampling_frequency(&mut bs)?;
                    }
                    let _ext_channels = bs.read_bits_leq32(4)?;
                }
            }
        }

        Ok(asc)
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

    /// Parse `program_config_element()` (ISO/IEC 14496-3 §4.4.1.1, Table 4.2 / Table 8.2).
    ///
    /// Assigns each declared front/side/back/LFE element a channel position using the
    /// conventional ordering used throughout the industry for the common layouts described
    /// informatively in ISO/IEC 14496-3 subclause 8.5.3: the first front `SCE` is the
    /// front-center channel; front `CPE`s are assigned outward from the front-left/right pair to
    /// the front-left/right-of-center "wide" pair; side `CPE`s are the side-left/right pair; the
    /// first back `CPE` is the rear-left/right pair, optionally followed by a rear-center `SCE`;
    /// LFE `SCE`s are LFE1, then LFE2. Layouts that don't fit this convention (e.g. more than one
    /// front SCE, or a side SCE) are rejected as unsupported rather than silently mis-assigned.
    ///
    /// Mixdown coefficients, comment fields, and element instance tags (used to associate
    /// elements with mixdowns/CCEs) are parsed for correct bitstream alignment but otherwise
    /// discarded; the element order alone determines how `raw_data_block()` syntactic elements
    /// map onto output channels (see [`ChannelElement`]).
    fn read_program_config_element<B: ReadBitsLtr>(
        bs: &mut B,
    ) -> Result<(Channels, Vec<ChannelElement>)> {
        let _element_instance_tag = bs.read_bits_leq32(4)?;
        let _object_type = bs.read_bits_leq32(2)?;
        let _sampling_frequency_index = bs.read_bits_leq32(4)?;

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

        let mut elements = Vec::new();
        let mut mask = Position::empty();

        let mut front_sce_seen = false;
        let mut front_cpe_seen = 0u32;

        for &is_cpe in &front_is_cpe {
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
        for &is_cpe in &side_is_cpe {
            if !is_cpe || side_cpe_seen {
                return unsupported_error("common (mp4a): PCE side channel layout too complex");
            }
            side_cpe_seen = true;
            elements.push(ChannelElement::Pair(Position::SIDE_LEFT, Position::SIDE_RIGHT));
            mask |= Position::SIDE_LEFT | Position::SIDE_RIGHT;
        }

        let mut back_cpe_seen = false;
        let mut back_sce_seen = false;
        for &is_cpe in &back_is_cpe {
            if is_cpe {
                if back_cpe_seen {
                    return unsupported_error(
                        "common (mp4a): PCE back channel layout too complex",
                    );
                }
                back_cpe_seen = true;
                elements.push(ChannelElement::Pair(Position::REAR_LEFT, Position::REAR_RIGHT));
                mask |= Position::REAR_LEFT | Position::REAR_RIGHT;
            }
            else {
                if back_sce_seen {
                    return unsupported_error(
                        "common (mp4a): PCE back channel layout too complex",
                    );
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
        _ => None,
    }
}
