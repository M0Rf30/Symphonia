// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use extra_data::{
    VIDEO_EXTRA_DATA_ID_AV1_DECODER_CONFIG, VIDEO_EXTRA_DATA_ID_AVC_DECODER_CONFIG,
    VIDEO_EXTRA_DATA_ID_HEVC_DECODER_CONFIG, VIDEO_EXTRA_DATA_ID_VP9_DECODER_CONFIG,
};
use log::warn;

use symphonia_common::mpeg::video::{
    AVCDecoderConfigurationRecord, HEVCDecoderConfigurationRecord,
};
use symphonia_common::xiph::audio::flac::{MetadataBlockHeader, MetadataBlockType, StreamInfo};
use symphonia_common::xiph::audio::vorbis::unpack_xiph_laced_extradata;
use symphonia_core::audio::Channels;
use symphonia_core::audio::sample::SampleFormat;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_FLAC, CODEC_ID_VORBIS};
use symphonia_core::codecs::audio::{AudioCodecId, well_known::*};
use symphonia_core::codecs::subtitle::{SubtitleCodecId, SubtitleCodecParameters, well_known::*};
use symphonia_core::codecs::video::{
    VIDEO_EXTRA_DATA_ID_NULL, VideoCodecId, VideoCodecParameters, VideoExtraData, well_known::*,
};
use symphonia_core::codecs::{CodecId, CodecParameters, CodecProfile};
use symphonia_core::errors::{Error, Result, decode_error};
use symphonia_core::io::{BufReader, ReadBytes};

use crate::segment::TrackElement;

pub(crate) fn make_track_codec_params(track: TrackElement) -> Result<Option<CodecParameters>> {
    // Get the codec ID for the track.
    let codec_id = get_codec_id(&track);
    let (profile, level) = get_codec_profile_and_level(&track);

    match codec_id {
        Some(CodecId::Audio(id)) => make_audio_codec_params(id, profile, track),
        Some(CodecId::Video(id)) => make_video_codec_params(id, profile, level, track),
        Some(CodecId::Subtitle(id)) => make_subtitle_codec_params(id, track),
        _ => Ok(None),
    }
}

fn make_audio_codec_params(
    id: AudioCodecId,
    profile: Option<CodecProfile>,
    track: TrackElement,
) -> Result<Option<CodecParameters>> {
    // A nested audio track element in expected in the track element.
    let audio = match track.audio {
        Some(audio) => audio,
        None => {
            warn!("expected audio element in track element");
            return Ok(None);
        }
    };

    let mut codec_params = AudioCodecParameters::new();

    codec_params.for_codec(id);

    if let Some(profile) = profile {
        codec_params.with_profile(profile);
    }

    let channels = u16::try_from(audio.channels.get())
        .map_err(|_| Error::Unsupported("mkv: channel count too large"))?;

    // The sampling frequency is validated to be positive, but may be infinite.
    if !audio.sampling_frequency.is_finite() {
        return Err(Error::Unsupported("mkv: sampling frequency is not finite"));
    }

    codec_params.with_sample_rate(audio.sampling_frequency.round() as u32);
    codec_params.with_channels(Channels::Discrete(channels));

    let format = audio.bit_depth.and_then(|bits| match bits.get() {
        8 => Some(SampleFormat::S8),
        16 => Some(SampleFormat::S16),
        24 => Some(SampleFormat::S24),
        32 => Some(SampleFormat::S32),
        _ => None,
    });

    if let Some(format) = format {
        codec_params.with_sample_format(format);
    }

    if let Some(bits) = audio.bit_depth {
        codec_params.with_bits_per_sample(bits.get() as u32);
    }

    // The WAVE format structure describes the samples, not the track's bit depth. As the RIFF
    // reader does, only integer PCM states its (valid) bits per sample: A-law, mu-law and float
    // have an implicit width, and the compressed codecs have none.
    if track.codec_id == "A_MS/ACM" {
        codec_params.sample_format = None;
        codec_params.bits_per_sample = None;
        codec_params.bits_per_coded_sample = None;

        if let Some(acm) = track.codec_private.as_deref().and_then(AcmFormat::parse) {
            if acm.tag == AcmFormat::WAVE_FORMAT_PCM {
                let bits = u32::from(acm.valid_bits_per_sample);
                codec_params.with_bits_per_coded_sample(bits).with_bits_per_sample(bits);
            }
        }
    }

    // Matroska WavPack blocks have their `wvpk` headers stripped. The WavPack decoder relies on
    // the 2-byte version number that the Matroska spec mandates in CodecPrivate to recognise
    // this packet flavour, so make sure it is always present (default: 4.16, the version
    // written by libwavpack, for the muxers that omit it).
    let codec_private = match (id, track.codec_private) {
        (CODEC_ID_WAVPACK, Some(extra)) if extra.len() == 2 => Some(extra),
        (CODEC_ID_WAVPACK, _) => Some(Box::from([0x10u8, 0x04])),
        (_, codec_private) => codec_private,
    };

    if let Some(codec_private) = codec_private {
        let extra_data = match id {
            CODEC_ID_VORBIS => vorbis_extra_data_from_codec_private(&codec_private)?,
            CODEC_ID_FLAC => flac_extra_data_from_codec_private(&codec_private)?,
            _ => codec_private,
        };

        codec_params.with_extra_data(extra_data);
    }

    Ok(Some(CodecParameters::Audio(codec_params)))
}

/// The `WAVEFORMATEX` or `WAVEFORMATEXTENSIBLE` structure that is the CodecPrivate of an
/// `A_MS/ACM` track (Matroska codec mappings; layout from Microsoft `mmreg.h`).
pub(crate) struct AcmFormat {
    /// The format tag. For `WAVEFORMATEXTENSIBLE`, the tag embedded in the sub-format GUID.
    tag: u16,
    block_align: u16,
    bits_per_sample: u16,
    valid_bits_per_sample: u16,
}

impl AcmFormat {
    const WAVE_FORMAT_PCM: u16 = 0x0001;
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
    const WAVE_FORMAT_ALAW: u16 = 0x0006;
    const WAVE_FORMAT_MULAW: u16 = 0x0007;
    const WAVE_FORMAT_EXTENSIBLE: u16 = 0xfffe;

    pub(crate) fn parse(data: &[u8]) -> Option<Self> {
        let u16_at = |pos: usize| data.get(pos..pos + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));

        // wFormatTag, nChannels, nSamplesPerSec, nAvgBytesPerSec, nBlockAlign, wBitsPerSample,
        // then cbSize and cbSize bytes of extra data (absent in a 16-byte PCMWAVEFORMAT).
        let mut tag = u16_at(0)?;
        let block_align = u16_at(12)?;
        let bits_per_sample = u16_at(14)?;
        let mut valid_bits_per_sample = bits_per_sample;

        if tag == Self::WAVE_FORMAT_EXTENSIBLE {
            // wValidBitsPerSample, dwChannelMask, then the SubFormat GUID. The GUIDs of the
            // KSDATAFORMAT_SUBTYPE_* formats start with the format tag (little-endian).
            if u16_at(16)? < 22 {
                return None;
            }
            valid_bits_per_sample = u16_at(18)?;
            tag = u16_at(24)?;

            if valid_bits_per_sample == 0 || valid_bits_per_sample > bits_per_sample {
                valid_bits_per_sample = bits_per_sample;
            }
        }

        Some(AcmFormat { tag, block_align, bits_per_sample, valid_bits_per_sample })
    }

    /// Gets the codec of the format, if it is supported.
    pub(crate) fn codec(&self) -> Option<AudioCodecId> {
        let codec = match (self.tag, self.bits_per_sample) {
            // 8-bit WAVE PCM is unsigned.
            (Self::WAVE_FORMAT_PCM, 8) => CODEC_ID_PCM_U8,
            (Self::WAVE_FORMAT_PCM, 16) => CODEC_ID_PCM_S16LE,
            (Self::WAVE_FORMAT_PCM, 24) => CODEC_ID_PCM_S24LE,
            (Self::WAVE_FORMAT_PCM, 32) => CODEC_ID_PCM_S32LE,
            (Self::WAVE_FORMAT_IEEE_FLOAT, 32) => CODEC_ID_PCM_F32LE,
            (Self::WAVE_FORMAT_IEEE_FLOAT, 64) => CODEC_ID_PCM_F64LE,
            (Self::WAVE_FORMAT_ALAW, 8) => CODEC_ID_PCM_ALAW,
            (Self::WAVE_FORMAT_MULAW, 8) => CODEC_ID_PCM_MULAW,
            // Self-framing codecs (WAVE_FORMAT_MPEG, _MPEGLAYER3, Dolby AC-3 SPDIF, DTS).
            (0x0022, _) => CODEC_ID_TRUESPEECH,
            // Microsoft GSM 6.10, MSN Audio and the other tag FFmpeg maps to it.
            (0x0031 | 0x0032 | 0x1500, _) => CODEC_ID_GSM_MS,
            // Intel Music Coder and Indeo Audio Coder.
            (0x0401, _) => CODEC_ID_IMC,
            (0x0402, _) => CODEC_ID_IAC,
            (0x0050, _) => CODEC_ID_MP2,
            (0x0055, _) => CODEC_ID_MP3,
            (0x2000, _) => CODEC_ID_AC3,
            (0x2001, _) => CODEC_ID_DCA,
            _ => return None,
        };
        Some(codec)
    }

    /// Gets the number of bytes in a frame, if the format is PCM.
    pub(crate) fn pcm_frame_bytes(&self) -> Option<u32> {
        let is_pcm = matches!(
            self.tag,
            Self::WAVE_FORMAT_PCM
                | Self::WAVE_FORMAT_IEEE_FLOAT
                | Self::WAVE_FORMAT_ALAW
                | Self::WAVE_FORMAT_MULAW
        );
        (is_pcm && self.codec().is_some()).then_some(u32::from(self.block_align))
    }
}

fn make_video_codec_params(
    id: VideoCodecId,
    profile: Option<CodecProfile>,
    level: Option<u32>,
    track: TrackElement,
) -> Result<Option<CodecParameters>> {
    // A nested video track element in expected in the track element.
    let video = match track.video {
        Some(video) => video,
        None => {
            warn!("expected video element in track element");
            return Ok(None);
        }
    };

    let mut codec_params = VideoCodecParameters {
        codec: id,
        width: Some(
            u16::try_from(video.pixel_width.get())
                .map_err(|_| Error::Unsupported("mkv: video width too large"))?,
        ),
        height: Some(
            u16::try_from(video.pixel_height.get())
                .map_err(|_| Error::Unsupported("mkv: video height too large"))?,
        ),
        ..Default::default()
    };

    if let Some(profile) = profile {
        codec_params.with_profile(profile);
    }

    if let Some(level) = level {
        codec_params.with_level(level);
    }

    if let Some(codec_private) = track.codec_private {
        let extra_data_id = match id {
            CODEC_ID_H264 => VIDEO_EXTRA_DATA_ID_AVC_DECODER_CONFIG,
            CODEC_ID_HEVC => VIDEO_EXTRA_DATA_ID_HEVC_DECODER_CONFIG,
            CODEC_ID_VP9 => VIDEO_EXTRA_DATA_ID_VP9_DECODER_CONFIG,
            CODEC_ID_AV1 => VIDEO_EXTRA_DATA_ID_AV1_DECODER_CONFIG,
            _ => VIDEO_EXTRA_DATA_ID_NULL,
        };
        codec_params.add_extra_data(VideoExtraData { id: extra_data_id, data: codec_private });
    }

    for block in track.block_addition_mappings {
        if let Some(extra_data) = block.extra_data {
            codec_params.add_extra_data(extra_data);
        }
    }

    Ok(Some(CodecParameters::Video(codec_params)))
}

fn make_subtitle_codec_params(
    id: SubtitleCodecId,
    track: TrackElement,
) -> Result<Option<CodecParameters>> {
    let mut codec_params = SubtitleCodecParameters::new();

    codec_params.for_codec(id);

    if let Some(codec_private) = track.codec_private {
        codec_params.with_extra_data(codec_private);
    }

    Ok(Some(CodecParameters::Subtitle(codec_params)))
}

// Extra data parsing
fn vorbis_extra_data_from_codec_private(extra: &[u8]) -> Result<Box<[u8]>> {
    // Private Data for this codec is Xiph laced: the identification, comment, and setup headers.
    // The layout expected by the Vorbis codec is the identification header followed by the setup
    // header.
    let (ident_header, setup_header) = unpack_xiph_laced_extradata(extra)?;

    Ok([ident_header, setup_header].concat().into_boxed_slice())
}

fn flac_extra_data_from_codec_private(codec_private: &[u8]) -> Result<Box<[u8]>> {
    let mut reader = BufReader::new(codec_private);

    let marker = reader.read_quad_bytes()?;
    if marker != *b"fLaC" {
        return decode_error("mkv (flac): missing flac stream marker");
    }

    // The codec private data contains all header/matadata blocks. Extract the stream information
    // block (typically the first).
    loop {
        let header = MetadataBlockHeader::read(&mut reader)?;

        match header.block_type {
            MetadataBlockType::StreamInfo => {
                // Ensure the stream information block is the expected size.
                if !StreamInfo::is_valid_size(u64::from(header.block_len)) {
                    return decode_error("isomp4 (flac): invalid stream info block length");
                }

                break Ok(reader.read_boxed_slice_exact(header.block_len as usize)?);
            }
            _ => reader.ignore_bytes(u64::from(header.block_len))?,
        }
    }
}

fn get_codec_id(track: &TrackElement) -> Option<CodecId> {
    let bit_depth = track.audio.as_ref().and_then(|a| a.bit_depth);

    let codec_id = match track.codec_id.as_str() {
        // Audio Codecs
        "A_MPEG/L1" => CodecId::Audio(CODEC_ID_MP1),
        "A_MPEG/L2" => CodecId::Audio(CODEC_ID_MP2),
        "A_MPEG/L3" => CodecId::Audio(CODEC_ID_MP3),
        "A_FLAC" => CodecId::Audio(CODEC_ID_FLAC),
        "A_OPUS" => CodecId::Audio(CODEC_ID_OPUS),
        "A_VORBIS" => CodecId::Audio(CODEC_ID_VORBIS),
        "A_AAC/MPEG2/MAIN" | "A_AAC/MPEG2/LC" | "A_AAC/MPEG2/LC/SBR" | "A_AAC/MPEG2/SSR"
        | "A_AAC/MPEG4/MAIN" | "A_AAC/MPEG4/LC" | "A_AAC/MPEG4/LC/SBR" | "A_AAC/MPEG4/SSR"
        | "A_AAC/MPEG4/LTP" | "A_AAC" => CodecId::Audio(CODEC_ID_AAC),
        "A_MPC" => CodecId::Audio(CODEC_ID_MUSEPACK),
        "A_AC3" | "A_AC3/BSID9" | "A_AC3/BSID10" => CodecId::Audio(CODEC_ID_AC3),
        "A_EAC3" => CodecId::Audio(CODEC_ID_EAC3),
        // The TrueHD decoder handles both TrueHD and MLP access units.
        "A_TRUEHD" | "A_MLP" => CodecId::Audio(CODEC_ID_TRUEHD),
        "A_ALAC" => CodecId::Audio(CODEC_ID_ALAC),
        "A_DTS" | "A_DTS/EXPRESS" | "A_DTS/LOSSLESS" => CodecId::Audio(CODEC_ID_DCA),
        "A_TTA1" => CodecId::Audio(CODEC_ID_TTA),
        "A_WAVPACK4" => CodecId::Audio(CODEC_ID_WAVPACK),
        "A_ATRAC/AT1" => CodecId::Audio(CODEC_ID_ATRAC1),
        "A_REAL/ATRC" => CodecId::Audio(CODEC_ID_ATRAC3),
        "A_REAL/14_4" => CodecId::Audio(CODEC_ID_RA10),
        "A_REAL/28_8" => CodecId::Audio(CODEC_ID_RA20),
        "A_REAL/COOK" => CodecId::Audio(CODEC_ID_COOK),
        "A_REAL/SIPR" => CodecId::Audio(CODEC_ID_SIPR),
        "A_REAL/RALF" => CodecId::Audio(CODEC_ID_RALF),
        "A_PCM/INT/BIG" => match bit_depth?.get() {
            16 => CodecId::Audio(CODEC_ID_PCM_S16BE),
            24 => CodecId::Audio(CODEC_ID_PCM_S24BE),
            32 => CodecId::Audio(CODEC_ID_PCM_S32BE),
            _ => return None,
        },
        "A_PCM/INT/LIT" => match bit_depth?.get() {
            16 => CodecId::Audio(CODEC_ID_PCM_S16LE),
            24 => CodecId::Audio(CODEC_ID_PCM_S24LE),
            32 => CodecId::Audio(CODEC_ID_PCM_S32LE),
            _ => return None,
        },
        "A_PCM/FLOAT/IEEE" => match bit_depth?.get() {
            32 => CodecId::Audio(CODEC_ID_PCM_F32LE),
            64 => CodecId::Audio(CODEC_ID_PCM_F64LE),
            _ => return None,
        },
        "A_MS/ACM" => CodecId::Audio(AcmFormat::parse(track.codec_private.as_deref()?)?.codec()?),
        // A_QUICKTIME
        // A_QUICKTIME/QDMC
        // A_QUICKTIME/QDM2

        // Video Codecs
        "V_MJPEG" => CodecId::Video(CODEC_ID_MJPEG),
        "V_MPEG4/MS/V3" => CodecId::Video(CODEC_ID_MSMPEG4V3),
        "V_MPEG1" => CodecId::Video(CODEC_ID_MPEG1),
        "V_MPEG2" => CodecId::Video(CODEC_ID_MPEG2),
        "V_MPEG4/ISO/SP" | "V_MPEG4/ISO/ASP" => CodecId::Video(CODEC_ID_MPEG4),
        "V_MPEG4/ISO/AVC" | "V_MPEG4/ISO/AP" => CodecId::Video(CODEC_ID_H264),
        "V_MPEGH/ISO/HEVC" => CodecId::Video(CODEC_ID_HEVC),
        "V_REAL/RV10" => CodecId::Video(CODEC_ID_RV10),
        "V_REAL/RV20" => CodecId::Video(CODEC_ID_RV20),
        "V_REAL/RV30" => CodecId::Video(CODEC_ID_RV30),
        "V_REAL/RV40" => CodecId::Video(CODEC_ID_RV40),
        "V_THEORA" => CodecId::Video(CODEC_ID_THEORA),
        "V_VP8" => CodecId::Video(CODEC_ID_VP8),
        "V_VP9" => CodecId::Video(CODEC_ID_VP9),
        "V_AV1" => CodecId::Video(CODEC_ID_AV1),
        "V_AVS2" => CodecId::Video(CODEC_ID_AVS2),
        "V_AVS3" => CodecId::Video(CODEC_ID_AVS3),
        // V_MS/VFW/FOURCC WVC1 (VC-1)
        // V_UNCOMPRESSED
        // V_QUICKTIME
        // V_PRORES
        // V_FFV1

        // Subtitle Codecs
        "S_TEXT/UTF8" => CodecId::Subtitle(CODEC_ID_TEXT_UTF8),
        "S_TEXT/SSA" => CodecId::Subtitle(CODEC_ID_SSA),
        "S_TEXT/ASS" => CodecId::Subtitle(CODEC_ID_ASS),
        "S_TEXT/WEBVTT" => CodecId::Subtitle(CODEC_ID_WEBVTT),
        "S_IMAGE/BMP" => CodecId::Subtitle(CODEC_ID_BMP),
        "S_VOBSUB" => CodecId::Subtitle(CODEC_ID_VOBSUB),
        "S_DVBSUB" => CodecId::Subtitle(CODEC_ID_DVBSUB),
        "S_HDMV/PGS" => CodecId::Subtitle(CODEC_ID_HDMV_PGS),
        "S_HDMV/TEXTST" => CodecId::Subtitle(CODEC_ID_HDMV_TEXTST),
        "S_KATE" => CodecId::Subtitle(CODEC_ID_KATE),
        // S_ARIBSUB

        // Other Codecs
        _ => {
            log::info!("unknown codec: {}", track.codec_id);
            return None;
        }
    };

    Some(codec_id)
}

fn get_codec_profile_and_level(track: &TrackElement) -> (Option<CodecProfile>, Option<u32>) {
    use symphonia_core::codecs::audio::well_known::profiles::{
        CODEC_PROFILE_AAC_HE, CODEC_PROFILE_AAC_LC, CODEC_PROFILE_AAC_LTP, CODEC_PROFILE_AAC_MAIN,
        CODEC_PROFILE_AAC_SSR,
    };
    use symphonia_core::codecs::video::well_known::profiles::{
        CODEC_PROFILE_MPEG4_ADVANCED_SIMPLE, CODEC_PROFILE_MPEG4_SIMPLE,
    };

    match track.codec_id.as_str() {
        // Audio Codecs
        "A_AAC" | "A_AAC/MPEG2/LC" | "A_AAC/MPEG4/LC" => (Some(CODEC_PROFILE_AAC_LC), None),
        "A_AAC/MPEG2/MAIN" | "A_AAC/MPEG4/MAIN" => (Some(CODEC_PROFILE_AAC_MAIN), None),
        "A_AAC/MPEG2/LC/SBR" | "A_AAC/MPEG4/LC/SBR" => (Some(CODEC_PROFILE_AAC_HE), None),
        "A_AAC/MPEG2/SSR" | "A_AAC/MPEG4/SSR" => (Some(CODEC_PROFILE_AAC_SSR), None),
        "A_AAC/MPEG4/LTP" => (Some(CODEC_PROFILE_AAC_LTP), None),

        // Video Codecs
        "V_MPEG4/ISO/SP" => (Some(CODEC_PROFILE_MPEG4_SIMPLE), None),
        "V_MPEG4/ISO/ASP" => (Some(CODEC_PROFILE_MPEG4_ADVANCED_SIMPLE), None),
        "V_MPEG4/ISO/AVC" | "V_MPEG4/ISO/AP" => {
            // Parse AVCDecoderConfigurationRecord extra data to acquire the profile and level.
            track
                .codec_private
                .as_ref()
                .and_then(|buf| AVCDecoderConfigurationRecord::read(buf).ok())
                .map(|cfg| (Some(cfg.profile), Some(cfg.level)))
                .unwrap_or_else(|| (None, None))
        }
        "V_MPEGH/ISO/HEVC" => {
            // Parse HevcDecoderConfigurationRecord extra data to acquire the profile and level.
            track
                .codec_private
                .as_ref()
                .and_then(|buf| HEVCDecoderConfigurationRecord::read(buf).ok())
                .map(|cfg| (Some(cfg.profile), Some(cfg.level)))
                .unwrap_or_else(|| (None, None))
        }

        // Other Codecs
        _ => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use symphonia_core::errors::Error;

    use super::make_track_codec_params;
    use crate::segment::{AudioElement, MatroskaTicks, TrackElement};

    fn audio_track(channels: u64, sampling_frequency: f64) -> TrackElement {
        TrackElement {
            number: NonZeroU64::MIN,
            uid: NonZeroU64::MIN,
            lang: "eng".into(),
            lang_bcp47: None,
            codec_id: "A_AC3".into(),
            codec_private: None,
            codec_delay: MatroskaTicks::default(),
            block_addition_mappings: Vec::new(),
            audio: Some(AudioElement {
                sampling_frequency,
                output_sampling_frequency: None,
                channels: NonZeroU64::new(channels).expect("non-zero channel count"),
                bit_depth: None,
            }),
            video: None,
            default_duration: None,
            default_decoded_field_duration: None,
            track_timestamp_scale: 1.0,
            seek_pre_roll: MatroskaTicks::default(),
            flags: Default::default(),
            content_encodings: Vec::new(),
        }
    }

    #[test]
    fn verify_surround_codec_ids() {
        use symphonia_core::codecs::CodecParameters;
        use symphonia_core::codecs::audio::well_known::{
            CODEC_ID_AC3, CODEC_ID_DCA, CODEC_ID_EAC3, CODEC_ID_TRUEHD,
        };

        let cases = [
            ("A_AC3", CODEC_ID_AC3),
            ("A_AC3/BSID9", CODEC_ID_AC3),
            ("A_AC3/BSID10", CODEC_ID_AC3),
            ("A_EAC3", CODEC_ID_EAC3),
            ("A_DTS", CODEC_ID_DCA),
            ("A_DTS/EXPRESS", CODEC_ID_DCA),
            ("A_DTS/LOSSLESS", CODEC_ID_DCA),
            ("A_TRUEHD", CODEC_ID_TRUEHD),
            ("A_MLP", CODEC_ID_TRUEHD),
        ];

        for (name, codec) in cases {
            let mut track = audio_track(6, 48000.0);
            track.codec_id = name.into();
            let Ok(Some(CodecParameters::Audio(params))) = make_track_codec_params(track)
            else {
                panic!("no audio params for {name}");
            };
            assert_eq!(params.codec, codec, "{name}");
            assert_eq!(params.sample_rate, Some(48000), "{name}");
            assert_eq!(params.channels.map(|c| c.count()), Some(6), "{name}");
        }
    }

    #[test]
    fn verify_channel_count_overflow_is_rejected() {
        assert!(matches!(
            make_track_codec_params(audio_track(65537, 48000.0)),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(make_track_codec_params(audio_track(2, 48000.0)), Ok(Some(_))));
    }

    #[test]
    fn verify_infinite_sampling_frequency_is_rejected() {
        assert!(matches!(
            make_track_codec_params(audio_track(2, f64::INFINITY)),
            Err(Error::Unsupported(_))
        ));
    }

    /// A WAVEFORMATEX with the given tag, block alignment, bits per sample and extra data.
    fn wave_format(tag: u16, block_align: u16, bits: u16, extra: &[u8]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&tag.to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes()); // Channels.
        data.extend_from_slice(&48000u32.to_le_bytes()); // Sample rate.
        data.extend_from_slice(&0u32.to_le_bytes()); // Average bytes per second.
        data.extend_from_slice(&block_align.to_le_bytes());
        data.extend_from_slice(&bits.to_le_bytes());
        data.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        data.extend_from_slice(extra);
        data
    }

    #[test]
    fn verify_acm_wave_formats() {
        use symphonia_core::codecs::audio::well_known::*;

        use super::AcmFormat;

        let parse = |data: &[u8]| AcmFormat::parse(data).and_then(|f| f.codec().zip(Some(f)));

        // 8-bit WAVE PCM is unsigned; a 16-byte PCMWAVEFORMAT has no cbSize.
        let Some((codec, fmt)) = parse(&wave_format(1, 2, 8, &[])[..16])
        else {
            panic!("8-bit pcm")
        };
        assert_eq!(codec, CODEC_ID_PCM_U8);
        assert_eq!(fmt.pcm_frame_bytes(), Some(2));

        // WAVEFORMATEXTENSIBLE: 20 valid bits in 24, KSDATAFORMAT_SUBTYPE_PCM.
        let mut ext = Vec::new();
        ext.extend_from_slice(&20u16.to_le_bytes());
        ext.extend_from_slice(&3u32.to_le_bytes());
        ext.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71]);
        let Some((codec, fmt)) = parse(&wave_format(0xfffe, 6, 24, &ext))
        else {
            panic!("extensible pcm")
        };
        assert_eq!(codec, CODEC_ID_PCM_S24LE);
        assert_eq!(fmt.valid_bits_per_sample, 20);
        assert_eq!(fmt.pcm_frame_bytes(), Some(6));

        // MP3 is not PCM: its blocks have no fixed frame size.
        let Some((codec, fmt)) = parse(&wave_format(0x55, 1, 0, &[0; 12]))
        else {
            panic!("mp3")
        };
        assert_eq!(codec, CODEC_ID_MP3);
        assert_eq!(fmt.pcm_frame_bytes(), None);

        // DSP Group TrueSpeech.
        let Some((codec, _)) = parse(&wave_format(0x22, 32, 1, &[0; 32]))
        else {
            panic!("truespeech")
        };
        assert_eq!(codec, CODEC_ID_TRUESPEECH);

        // Microsoft GSM 6.10 (65-byte blocks of 320 samples).
        let Some((codec, _)) = parse(&wave_format(0x31, 65, 0, &[0x40, 0x01]))
        else {
            panic!("gsm_ms")
        };
        assert_eq!(codec, CODEC_ID_GSM_MS);

        // Intel Music Coder and Indeo Audio Coder.
        let Some((codec, _)) = parse(&wave_format(0x401, 64, 0, &[]))
        else {
            panic!("imc")
        };
        assert_eq!(codec, CODEC_ID_IMC);
        let Some((codec, _)) = parse(&wave_format(0x402, 128, 0, &[0; 2]))
        else {
            panic!("iac")
        };
        assert_eq!(codec, CODEC_ID_IAC);

        // ADPCM is not mapped, and a truncated EXTENSIBLE is rejected.
        assert!(parse(&wave_format(2, 1024, 4, &[0; 32])).is_none());
        assert!(parse(&wave_format(0xfffe, 6, 24, &ext[..10])).is_none());
    }
}
