// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fmt;
use std::io::{Seek, SeekFrom};

use symphonia_core::audio::AmbisonicBFormat;
use symphonia_core::audio::{ChannelLabel, Channels, Position};
use symphonia_core::codecs::audio::AudioCodecId;
use symphonia_core::codecs::audio::well_known::CODEC_ID_ATRAC3;
use symphonia_core::codecs::audio::well_known::CODEC_ID_G729;
use symphonia_core::codecs::audio::well_known::CODEC_ID_MSN_SIREN;
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_AC3, CODEC_ID_ADPCM_CT, CODEC_ID_ADPCM_G722, CODEC_ID_ADPCM_G726,
    CODEC_ID_ADPCM_IMA_DK3, CODEC_ID_ADPCM_IMA_DK4, CODEC_ID_ADPCM_IMA_OKI, CODEC_ID_ADPCM_IMA_WAV,
    CODEC_ID_ADPCM_IMA_XBOX, CODEC_ID_ADPCM_MS, CODEC_ID_ADPCM_YAMAHA, CODEC_ID_DCA, CODEC_ID_EAC3,
    CODEC_ID_GSM_MS, CODEC_ID_MP2, CODEC_ID_MP3, CODEC_ID_PCM_ALAW, CODEC_ID_PCM_F32LE,
    CODEC_ID_PCM_F64LE, CODEC_ID_PCM_MULAW, CODEC_ID_PCM_S16LE, CODEC_ID_PCM_S24LE,
    CODEC_ID_PCM_S32LE, CODEC_ID_PCM_U8, CODEC_ID_TRUESPEECH,
};
use symphonia_core::codecs::audio::well_known::{CODEC_ID_IAC, CODEC_ID_IMC};
use symphonia_core::codecs::audio::well_known::{CODEC_ID_WMAV1, CODEC_ID_WMAV2};
use symphonia_core::errors::{Error, Result, decode_error, unsupported_error};
use symphonia_core::formats::Track;
use symphonia_core::io::{MediaSource, MediaSourceStream, ReadBytes};
use symphonia_core::meta::{MetadataBuilder, MetadataLog, MetadataRevision};

use symphonia_metadata::embedded::riff;

use crate::common::{
    ByteOrder, ChunkParser, ChunksReader, FormatALaw, FormatAdpcm, FormatData, FormatEs,
    FormatExtensible, FormatIeeeFloat, FormatMpeg, FormatMuLaw, FormatPcm, Id3Chunk, NullChunks,
    PacketInfo, ParseChunk, ParseChunkTag,
};

use log::info;

use super::WAVE_METADATA_INFO;
use super::extras::{BufChunk, WaveExtras, is_extra_chunk};

pub struct WaveFormatChunk {
    /// The number of channels.
    pub num_channels: u16,
    /// The sample rate in Hz. For non-PCM formats, this value must be interpreted as per the
    /// format's specifications.
    pub sample_rate: u32,
    /// The required average data rate required in bytes/second. For non-PCM formats, this value
    /// must be interpreted as per the format's specifications.
    pub avg_bytes_per_sec: u32,
    /// The byte alignment of one audio frame. For PCM formats, this is equal to
    /// `(num_channels * format_data.bits_per_sample) / 8`. For non-PCM formats, this value must be
    /// interpreted as per the format's specifications.
    pub block_align: u16,
    /// Extra data associated with the format block conditional upon the format tag.
    pub format_data: FormatData,
}

impl WaveFormatChunk {
    fn read_pcm_fmt<B: ReadBytes>(
        reader: &mut B,
        block_align: u16,
        bits_per_sample: u16,
        num_channels: u16,
        len: u32,
    ) -> Result<FormatData> {
        // WaveFormat for a PCM format may be extended with an extra data length field followed by
        // the extension data itself. Use the chunk length to determine if the format chunk is
        // extended.
        match len {
            // Basic WavFormat struct, no extension.
            16 => (),
            // WaveFormatEx with extension data length field present, but no extension data.
            18 => {
                // Extension data length should be 0.
                let _extension_len = reader.read_be_u16()?;
            }
            // WaveFormatEx with extension data length field present, and extension data.
            40 => {
                // Extension data length should be either 0 or 22 (if valid data is present).
                let _extension_len = reader.read_u16()?;
                reader.ignore_bytes(22)?;
            }
            _ => return decode_error("wav: malformed fmt_pcm chunk"),
        }

        // Bits per sample for PCM is both the encoded sample width, and the actual sample width.
        // Strictly, this must either be 8 or 16 bits, but there is no reason why 24 and 32 bits
        // can't be supported. Since these files do exist, allow for 8/16/24/32-bit samples, but
        // error if not a multiple of 8 or greater than 32-bits.
        //
        // Select the appropriate codec using bits per sample. Samples are always interleaved and
        // little-endian encoded for the PCM format.
        let codec = match bits_per_sample {
            8 => CODEC_ID_PCM_U8,
            16 => CODEC_ID_PCM_S16LE,
            24 => CODEC_ID_PCM_S24LE,
            32 => CODEC_ID_PCM_S32LE,
            _ => {
                return decode_error(
                    "wav: bits per sample for fmt_pcm must be 8, 16, 24 or 32 bits",
                );
            }
        };

        // Block align is redundant for this format, but ideally it should be set correctly. Log if
        // it's incorrect.
        let expected_block_align = u32::from(num_channels) * u32::from(bits_per_sample / 8);

        if u32::from(block_align) != expected_block_align {
            info!("fmt_pcm expected block align of {}, got {}", expected_block_align, block_align);
        }

        let channels = map_wave_channel_count(num_channels)?;

        Ok(FormatData::Pcm(FormatPcm {
            bits_per_sample,
            valid_bits_per_sample: bits_per_sample,
            channels,
            codec,
        }))
    }

    /// Skips the bytes of the fmt chunk beyond the 16-byte base (the `WAVEFORMATEX` extension).
    fn skip_fmt_extension<B: ReadBytes>(reader: &mut B, len: u32) -> Result<()> {
        if len > 16 {
            reader.ignore_bytes(u64::from(len - 16))?;
        }
        Ok(())
    }

    fn read_adpcm_fmt<B: ReadBytes>(
        reader: &mut B,
        block_align: u16,
        bits_per_sample: u16,
        num_channels: u16,
        len: u32,
        codec: AudioCodecId,
    ) -> Result<FormatData> {
        let valid_bits = match codec {
            // IMA ADPCM may use 2 to 5 bits per sample.
            CODEC_ID_ADPCM_IMA_WAV => (2..=5).contains(&bits_per_sample),
            _ => bits_per_sample == 4,
        };

        if !valid_bits {
            return decode_error("wav: unsupported bits per sample for fmt_adpcm");
        }

        // WaveFormatEx with extension data length field present and with atleast frames per block data.
        if len < 20 {
            return decode_error("wav: malformed fmt_adpcm chunk");
        }

        let extra_size = reader.read_u16()? as u64;

        match codec {
            CODEC_ID_ADPCM_MS if extra_size < 32 => {
                return decode_error("wav: malformed fmt_adpcm chunk");
            }
            CODEC_ID_ADPCM_IMA_WAV if extra_size != 2 => {
                return decode_error("wav: malformed fmt_adpcm chunk");
            }
            _ => (),
        }

        // The extension data must be within the chunk.
        if extra_size > u64::from(len - 18) {
            return decode_error("wav: malformed fmt_adpcm chunk");
        }

        // The extension data holds the samples per block (IMA and MS ADPCM), and for MS ADPCM,
        // the coefficient table. Pass it on to the decoder verbatim.
        let extra_data = reader.read_boxed_slice_exact(extra_size as usize)?;

        let channels = map_wave_channel_count(num_channels)?;

        // Multichannel Microsoft ADPCM stores every channel as an independent mono block, and
        // `nBlockAlign` is the size of one of them. This follows FFmpeg's `wav_read_header`, which
        // multiplies the block align by the channel count for more than 2 channels. Mono and
        // stereo blocks are interleaved and `nBlockAlign` is the whole block.
        let block_align = if codec == CODEC_ID_ADPCM_MS && num_channels > 2 {
            let Some(block_align) = block_align.checked_mul(num_channels)
            else {
                return decode_error("wav: block align too large for fmt_adpcm");
            };
            block_align
        }
        else {
            block_align
        };

        Ok(FormatData::Adpcm(FormatAdpcm {
            block_align,
            bits_per_sample,
            channels,
            codec,
            extra_data: Some(extra_data),
        }))
    }

    /// Sony ATRAC3 (`WAVE_FORMAT_SONY_ATRAC3`). The extension must be present as the decoder
    /// depends on the coding mode in it.
    fn read_atrac3_fmt<B: ReadBytes>(
        reader: &mut B,
        block_align: u16,
        num_channels: u16,
        len: u32,
    ) -> Result<FormatData> {
        if len < 18 {
            return decode_error("wav: malformed fmt_atrac3 chunk");
        }

        let extra_size = u64::from(reader.read_u16()?);

        if extra_size == 0 || extra_size > u64::from(len - 18) {
            return decode_error("wav: malformed fmt_atrac3 chunk");
        }

        let extra_data = reader.read_boxed_slice_exact(extra_size as usize)?;
        let channels = map_wave_channel_count(num_channels)?;

        Ok(FormatData::Adpcm(FormatAdpcm {
            block_align,
            bits_per_sample: 0,
            channels,
            codec: CODEC_ID_ATRAC3,
            extra_data: Some(extra_data),
        }))
    }

    fn read_ieee_fmt<B: ReadBytes>(
        reader: &mut B,
        bits_per_sample: u16,
        num_channels: u16,
        len: u32,
    ) -> Result<FormatData> {
        // WaveFormat for a IEEE format should not be extended, but it may still have an extra data
        // length parameter.
        match len {
            16 => (),
            18 => {
                let extra_size = reader.read_u16()?;
                if extra_size != 0 {
                    return decode_error("wav: extra data not expected for fmt_ieee chunk");
                }
            }
            40 => {
                // WAVEFORMATEXTENSIBLE is used for formats having more than two channels
                // or higher sample resolutions than allowed by WAVEFORMATEX but for now
                // we just ignore it
                let _ = reader.ignore_bytes(40 - 16);
            }
            _ => return decode_error("wav: malformed fmt_ieee chunk"),
        }

        // Officially, only 32-bit floats are supported, but Symphonia can handle 64-bit floats.
        //
        // Select the appropriate codec using bits per sample. Samples are always interleaved and
        // little-endian encoded for the IEEE Float format.
        let codec = match bits_per_sample {
            32 => CODEC_ID_PCM_F32LE,
            64 => CODEC_ID_PCM_F64LE,
            _ => return decode_error("wav: bits per sample for fmt_ieee must be 32 or 64 bits"),
        };

        let channels = map_wave_channel_count(num_channels)?;

        Ok(FormatData::IeeeFloat(FormatIeeeFloat { channels, codec }))
    }

    fn read_ext_fmt<B: ReadBytes>(
        reader: &mut B,
        block_align: u16,
        bits_per_sample: u16,
        num_channels: u16,
        len: u32,
    ) -> Result<FormatData> {
        // WaveFormat for the extensible format must be extended to 40 bytes in length.
        if len < 40 {
            return decode_error("wav: malformed fmt_ext chunk");
        }

        let extra_size = reader.read_u16()?;

        // The size of the extra data for the Extensible format is exactly 22 bytes, except for
        // ATRAC3 which appends its own extension (checked below).
        if extra_size < 22 || u32::from(extra_size) > len - 18 {
            return decode_error("wav: extra data size not 22 bytes for fmt_ext chunk");
        }

        // The number of valid bits per sample for uncompressed PCM audio, but if the data is
        // compressed, then this is the samples per block.
        let valid_bits_per_sample = reader.read_u16()?;

        // Bits per sample for extensible formats is the bits per sample as written in the stream.
        // This must be a multiple of 8, even for compressed data formats. However, compressed data
        // formats may also set this to 0.
        if bits_per_sample & 0x7 != 0 {
            return decode_error("wav: bits per coded sample for fmt_ext must be a multiple of 8");
        }

        // The channel mask.
        let channel_mask = reader.read_u32()?;

        let mut sub_format_guid = [0u8; 16];
        reader.read_buf_exact(&mut sub_format_guid)?;

        // These GUIDs identify the format of the data chunks. These definitions can be found in
        // ksmedia.h of the Microsoft Windows Platform SDK.
        #[rustfmt::skip]
        const KSDATAFORMAT_SUBTYPE_PCM: [u8; 16] = [
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00,
            0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
        ];
        // #[rustfmt::skip]
        // const KSDATAFORMAT_SUBTYPE_ADPCM: [u8; 16] = [
        //     0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00,
        //     0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
        // ];
        // The last 12 bytes of the base GUID that embeds a format tag in its first 4 bytes.
        #[rustfmt::skip]
        const ATRAC3_GUID_SUFFIX: [u8; 12] = [
            0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
        ];
        #[rustfmt::skip]
        const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: [u8; 16] = [
            0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00,
            0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
        ];
        #[rustfmt::skip]
        const KSDATAFORMAT_SUBTYPE_ALAW: [u8; 16] = [
            0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00,
            0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
        ];
        #[rustfmt::skip]
        const KSDATAFORMAT_SUBTYPE_MULAW: [u8; 16] = [
            0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00,
            0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
        ];
        #[rustfmt::skip]
        const KSDATAFORMAT_SUBTYPE_AMBISONIC_B_FORMAT_PCM: [u8; 16] = [
            0x01, 0x00, 0x00, 0x00, 0x21, 0x07, 0xd3, 0x11,
            0x86, 0x44, 0xc8, 0xc1, 0xca, 0x00, 0x00, 0x00,
        ];
        #[rustfmt::skip]
        const KSDATAFORMAT_SUBTYPE_AMBISONIC_B_FORMAT_IEEE_FLOAT: [u8; 16] = [
            0x03, 0x00, 0x00, 0x00, 0x21, 0x07, 0xd3, 0x11,
            0x86, 0x44, 0xc8, 0xc1, 0xca, 0x00, 0x00, 0x00,
        ];

        // ATRAC3 is the base GUID with format tag 0x0270, followed by the same 14-byte extension
        // as in the plain format (FFmpeg `ff_get_wav_header`).
        if sub_format_guid[..4] == [0x70, 0x02, 0x00, 0x00]
            && sub_format_guid[4..] == ATRAC3_GUID_SUFFIX
        {
            let extra_data = reader.read_boxed_slice_exact(usize::from(extra_size) - 22)?;
            if extra_data.is_empty() {
                return decode_error("wav: malformed fmt_ext atrac3 chunk");
            }
            let channels = match fix_wave_channel_mask(channel_mask, num_channels)
                .ok()
                .and_then(Position::from_wave_channel_mask)
            {
                Some(positions) => Channels::Positioned(positions),
                None => map_wave_channel_count(num_channels)?,
            };
            return Ok(FormatData::Adpcm(FormatAdpcm {
                block_align,
                bits_per_sample: 0,
                channels,
                codec: CODEC_ID_ATRAC3,
                extra_data: Some(extra_data),
            }));
        }

        if extra_size != 22 {
            return decode_error("wav: extra data size not 22 bytes for fmt_ext chunk");
        }

        // The compressed surround formats are stored as elementary streams that are framed from
        // the data chunk. They are identified by either their own GUID, or a format tag embedded
        // in the first four bytes of the standard base GUID.
        if let Some((codec, spdif)) = es_codec_for_guid(&sub_format_guid) {
            let channels = match fix_wave_channel_mask(channel_mask, num_channels)
                .ok()
                .and_then(Position::from_wave_channel_mask)
            {
                Some(positions) => Channels::Positioned(positions),
                None => map_wave_channel_count(num_channels)?,
            };
            return Ok(FormatData::Es(FormatEs { channels, codec, spdif }));
        }

        // Verify support based on the format GUID.
        let codec = match sub_format_guid {
            KSDATAFORMAT_SUBTYPE_PCM | KSDATAFORMAT_SUBTYPE_AMBISONIC_B_FORMAT_PCM => {
                // Only support up-to 32-bit integer samples.
                if bits_per_sample == 0 || bits_per_sample > 32 {
                    return decode_error(
                        "wav: bits per sample for fmt_ext PCM sub-type must be 8, 16, 24, or 32 bits",
                    );
                }

                // The number of valid bits per sample must be less-than the number of written bits
                // per sample.
                if valid_bits_per_sample > bits_per_sample {
                    return decode_error(
                        "wav: valid bits per sample for fmt_ext PCM sub-type must be <= bits per sample",
                    );
                }

                // Use bits per coded sample to select the codec to use. If bits per sample is less
                // than the bits per coded sample, the codec will expand the sample during decode.
                match bits_per_sample {
                    8 => CODEC_ID_PCM_U8,
                    16 => CODEC_ID_PCM_S16LE,
                    24 => CODEC_ID_PCM_S24LE,
                    32 => CODEC_ID_PCM_S32LE,
                    _ => unreachable!(),
                }
            }
            KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
            | KSDATAFORMAT_SUBTYPE_AMBISONIC_B_FORMAT_IEEE_FLOAT => {
                // IEEE floating formats do not support truncated sample widths.
                if valid_bits_per_sample != bits_per_sample {
                    return decode_error(
                        "wav: valid bits per sample for fmt_ext IEEE sub-type must equal bits per sample",
                    );
                }

                // Select the appropriate codec based on the bits per coded sample.
                match bits_per_sample {
                    32 => CODEC_ID_PCM_F32LE,
                    64 => CODEC_ID_PCM_F64LE,
                    _ => {
                        return decode_error(
                            "wav: bits per sample for fmt_ext IEEE sub-type must be 32 or 64 bits",
                        );
                    }
                }
            }
            KSDATAFORMAT_SUBTYPE_ALAW => {
                if bits_per_sample != 8 {
                    return decode_error(
                        "wav: bits per sample for fmt_ext a-law sub-type must be 8 bits",
                    );
                }
                CODEC_ID_PCM_ALAW
            }
            KSDATAFORMAT_SUBTYPE_MULAW => {
                if bits_per_sample != 8 {
                    return decode_error(
                        "wav: bits per sample for fmt_ext u-law sub-type must be 8 bits",
                    );
                }
                CODEC_ID_PCM_MULAW
            }
            _ => return unsupported_error("wav: unsupported fmt_ext sub-type"),
        };

        let channels = match sub_format_guid {
            KSDATAFORMAT_SUBTYPE_AMBISONIC_B_FORMAT_PCM
            | KSDATAFORMAT_SUBTYPE_AMBISONIC_B_FORMAT_IEEE_FLOAT => {
                // For Ambisonic B-format, use the number of channels to map to Ambisonic B-Format
                // channel components.
                map_amb_channel_count(num_channels)?
            }
            _ => {
                // For PCM audio, use the channel mask and number of channels to map to positioned
                // channels.

                // Fix the channel mask if it is invalid for the stated number of channels.
                let channel_mask = fix_wave_channel_mask(channel_mask, num_channels)?;

                // Try to map the channel mask to positioned channels.
                match Position::from_wave_channel_mask(channel_mask) {
                    Some(positions) => Channels::Positioned(positions),
                    _ => return unsupported_error("wav: too many channels in mask for fmt_ext"),
                }
            }
        };

        Ok(FormatData::Extensible(FormatExtensible {
            bits_per_sample,
            valid_bits_per_sample,
            channels,
            sub_format_guid,
            codec,
        }))
    }

    fn read_alaw_pcm_fmt<B: ReadBytes>(
        reader: &mut B,
        num_channels: u16,
        len: u32,
    ) -> Result<FormatData> {
        if len != 18 {
            return decode_error("wav: malformed fmt_alaw chunk");
        }

        let extra_size = reader.read_u16()?;

        if extra_size > 0 {
            reader.ignore_bytes(u64::from(extra_size))?;
        }

        let channels = map_wave_channel_count(num_channels)?;
        Ok(FormatData::ALaw(FormatALaw { codec: CODEC_ID_PCM_ALAW, channels }))
    }

    fn read_mulaw_pcm_fmt<B: ReadBytes>(
        reader: &mut B,
        num_channels: u16,
        len: u32,
    ) -> Result<FormatData> {
        if len != 18 {
            return decode_error("wav: malformed fmt_mulaw chunk");
        }

        let extra_size = reader.read_u16()?;

        if extra_size > 0 {
            reader.ignore_bytes(u64::from(extra_size))?;
        }

        let channels = map_wave_channel_count(num_channels)?;
        Ok(FormatData::MuLaw(FormatMuLaw { codec: CODEC_ID_PCM_MULAW, channels }))
    }

    fn read_mpeg_fmt<B: ReadBytes>(
        reader: &mut B,
        num_channels: u16,
        len: u32,
        codec: AudioCodecId,
    ) -> Result<FormatData> {
        // The fmt chunk carries a WAVEFORMATEX header followed by an MPEGLAYER3WAVEFORMAT
        // extension (cbSize, then wID/fdwFlags/nBlockSize/nFramesPerBlock/nCodecDelay). None of the
        // extension is needed: MPEG audio frames are self-describing and are framed from the data
        // chunk (see `wave::mpeg`). Consume any bytes beyond the 16-byte base fmt so the next chunk
        // reads from the right position.
        if len > 16 {
            reader.ignore_bytes(u64::from(len - 16))?;
        }

        let channels = map_wave_channel_count(num_channels)?;
        Ok(FormatData::Mpeg(FormatMpeg { codec, channels }))
    }

    fn read_es_fmt<B: ReadBytes>(
        reader: &mut B,
        num_channels: u16,
        len: u32,
        codec: AudioCodecId,
        spdif: bool,
    ) -> Result<FormatData> {
        // Any extension is not needed: the stream is self-describing.
        Self::skip_fmt_extension(reader, len)?;

        let channels = map_wave_channel_count(num_channels)?;
        Ok(FormatData::Es(FormatEs { channels, codec, spdif }))
    }

    pub(crate) fn packet_info(&self) -> Result<PacketInfo> {
        match &self.format_data {
            FormatData::Pcm(pcm) => pcm.make_packet_info(),
            FormatData::Adpcm(adpcm) => adpcm.make_packet_info(),
            FormatData::IeeeFloat(ieee) => ieee.make_packet_info(),
            FormatData::ALaw(alaw) => alaw.make_packet_info(),
            FormatData::MuLaw(mulaw) => mulaw.make_packet_info(),
            FormatData::Extensible(ext) => ext.make_packet_info(),
            FormatData::Mpeg(mpeg) => mpeg.make_packet_info(),
            FormatData::Es(es) => es.make_packet_info(),
        }
    }
}

impl ParseChunk for WaveFormatChunk {
    fn parse<B: ReadBytes>(reader: &mut B, _tag: [u8; 4], len: u32) -> Result<WaveFormatChunk> {
        // WaveFormat has a minimal length of 16 bytes. This may be extended with format specific
        // data later.
        if len < 16 {
            return decode_error("wav: malformed fmt chunk");
        }

        let format = reader.read_u16()?;
        let num_channels = reader.read_u16()?;
        let mut sample_rate = reader.read_u32()?;
        let avg_bytes_per_sec = reader.read_u32()?;
        let block_align = reader.read_u16()?;
        let bits_per_sample = reader.read_u16()?;

        // The definition of these format identifiers can be found in mmreg.h of the Microsoft
        // Windows Platform SDK.
        const WAVE_FORMAT_PCM: u16 = 0x0001;
        const WAVE_FORMAT_ADPCM: u16 = 0x0002;
        const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
        const WAVE_FORMAT_ALAW: u16 = 0x0006;
        const WAVE_FORMAT_MULAW: u16 = 0x0007;
        const WAVE_FORMAT_ADPCM_IMA: u16 = 0x0011;
        const WAVE_FORMAT_DSPGROUP_TRUESPEECH: u16 = 0x0022;
        const WAVE_FORMAT_GSM610: u16 = 0x0031;
        const WAVE_FORMAT_WMAV1: u16 = 0x0160;
        const WAVE_FORMAT_WMAV2: u16 = 0x0161;
        const WAVE_FORMAT_MSNAUDIO: u16 = 0x0032;
        const WAVE_FORMAT_GSM_MS_ALT: u16 = 0x1500;
        const WAVE_FORMAT_G729A: u16 = 0x0083;
        const WAVE_FORMAT_G729_ALT: u16 = 0x2222;
        const WAVE_FORMAT_G729_ALT2: u16 = 0x729a;
        const WAVE_FORMAT_G723_ADPCM: u16 = 0x0014;
        const WAVE_FORMAT_ANTEX_G721: u16 = 0x0040;
        const WAVE_FORMAT_OKI_ADPCM: u16 = 0x0010;
        const WAVE_FORMAT_DIALOGIC_OKI_ADPCM: u16 = 0x0017;
        const WAVE_FORMAT_DK4_ADPCM: u16 = 0x0061;
        const WAVE_FORMAT_DK3_ADPCM: u16 = 0x0062;
        const WAVE_FORMAT_XBOX_ADPCM: u16 = 0x0069;
        const WAVE_FORMAT_CREATIVE_ADPCM: u16 = 0x0200;
        const WAVE_FORMAT_G726: u16 = 0x0045;
        const WAVE_FORMAT_G726_ADPCM: u16 = 0x0064;
        const WAVE_FORMAT_G722: u16 = 0x028f;
        const WAVE_FORMAT_MSN_SIREN: u16 = 0x028e;
        const WAVE_FORMAT_YAMAHA_ADPCM: u16 = 0x0020;
        const WAVE_FORMAT_SONY_ATRAC3: u16 = 0x0270;
        const WAVE_FORMAT_IMC: u16 = 0x0401;
        const WAVE_FORMAT_IAC: u16 = 0x0402;
        const WAVE_FORMAT_MPEG: u16 = 0x0050;
        const WAVE_FORMAT_MPEGLAYER3: u16 = 0x0055;
        const WAVE_FORMAT_DOLBY_AC3_SPDIF: u16 = 0x0092;
        const WAVE_FORMAT_DVM: u16 = 0x2000;
        const WAVE_FORMAT_DTS2: u16 = 0x2001;
        const WAVE_FORMAT_EXTENSIBLE: u16 = 0xfffe;

        let format_data = match format {
            // The PCM Wave Format
            WAVE_FORMAT_PCM => {
                Self::read_pcm_fmt(reader, block_align, bits_per_sample, num_channels, len)
            }
            // The Microsoft ADPCM Format
            WAVE_FORMAT_ADPCM => Self::read_adpcm_fmt(
                reader,
                block_align,
                bits_per_sample,
                num_channels,
                len,
                CODEC_ID_ADPCM_MS,
            ),
            // The IEEE Float Wave Format
            WAVE_FORMAT_IEEE_FLOAT => {
                Self::read_ieee_fmt(reader, bits_per_sample, num_channels, len)
            }
            // The Extensible Wave Format
            WAVE_FORMAT_EXTENSIBLE => {
                Self::read_ext_fmt(reader, block_align, bits_per_sample, num_channels, len)
            }
            // The Alaw Wave Format.
            WAVE_FORMAT_ALAW => Self::read_alaw_pcm_fmt(reader, num_channels, len),
            // The MuLaw Wave Format.
            WAVE_FORMAT_MULAW => Self::read_mulaw_pcm_fmt(reader, num_channels, len),
            // The IMA ADPCM Format
            WAVE_FORMAT_ADPCM_IMA => Self::read_adpcm_fmt(
                reader,
                block_align,
                bits_per_sample,
                num_channels,
                len,
                CODEC_ID_ADPCM_IMA_WAV,
            ),
            // Windows Media Audio 1 and 2. The WMA decoder needs the whole `WAVEFORMATEX`: the
            // average byte rate and the encoder flags of the extension are not available
            // elsewhere. FFmpeg: `wmav1`/`wmav2`. WMA Pro (0x0162), Lossless (0x0163) and Voice
            // (0x000a) are other codecs and are not decoded.
            WAVE_FORMAT_WMAV1 | WAVE_FORMAT_WMAV2 => {
                let extension = if len >= 18 {
                    let extra_size = u64::from(reader.read_u16()?);
                    if extra_size > u64::from(len - 18) {
                        return decode_error("wav: malformed fmt_wma chunk");
                    }
                    reader.read_boxed_slice_exact(extra_size as usize)?
                }
                else {
                    Box::default()
                };
                let mut extra_data = Vec::with_capacity(18 + extension.len());
                extra_data.extend_from_slice(&format.to_le_bytes());
                extra_data.extend_from_slice(&num_channels.to_le_bytes());
                extra_data.extend_from_slice(&sample_rate.to_le_bytes());
                extra_data.extend_from_slice(&avg_bytes_per_sec.to_le_bytes());
                extra_data.extend_from_slice(&block_align.to_le_bytes());
                extra_data.extend_from_slice(&bits_per_sample.to_le_bytes());
                extra_data.extend_from_slice(&(extension.len() as u16).to_le_bytes());
                extra_data.extend_from_slice(&extension);
                let channels = map_wave_channel_count(num_channels)?;
                Ok(FormatData::Adpcm(FormatAdpcm {
                    block_align,
                    bits_per_sample,
                    channels,
                    codec: if format == WAVE_FORMAT_WMAV1 {
                        CODEC_ID_WMAV1
                    }
                    else {
                        CODEC_ID_WMAV2
                    },
                    extra_data: Some(extra_data.into_boxed_slice()),
                }))
            }
            // DSP Group TrueSpeech: 8 kHz mono, 240 samples per 32-byte frame. `WAVEFORMATEX`
            // extension (revision, samples per block, reserved) is passed on verbatim.
            WAVE_FORMAT_DSPGROUP_TRUESPEECH => {
                let extra_data = if len >= 18 {
                    let extra_size = u64::from(reader.read_u16()?);
                    if extra_size > u64::from(len - 18) {
                        return decode_error("wav: malformed fmt_truespeech chunk");
                    }
                    Some(reader.read_boxed_slice_exact(extra_size as usize)?)
                }
                else {
                    None
                };
                let channels = map_wave_channel_count(num_channels)?;
                Ok(FormatData::Adpcm(FormatAdpcm {
                    block_align,
                    bits_per_sample,
                    channels,
                    codec: CODEC_ID_TRUESPEECH,
                    extra_data,
                }))
            }
            // MSN Siren (Windows Live Messenger): mono, 16 kHz, 320 samples per 40-byte frame. The
            // `WAVEFORMATEX` extension only repeats the samples per frame. FFmpeg: `MSNSIREN`.
            WAVE_FORMAT_MSN_SIREN => {
                Self::skip_fmt_extension(reader, len)?;
                let channels = map_wave_channel_count(num_channels)?;
                Ok(FormatData::Adpcm(FormatAdpcm {
                    block_align,
                    bits_per_sample,
                    channels,
                    codec: CODEC_ID_MSN_SIREN,
                    extra_data: None,
                }))
            }
            // ITU-T G.729 / G.729A: 8 kHz mono, 80 samples per 10-byte frame (an 8-byte block is a
            // G.729 Annex D frame). The `WAVEFORMATEX` extension, if any, is not needed. 0x2222
            // and 0x729a are the other tags that FFmpeg (`ff_codec_wav_tags`) maps to it.
            WAVE_FORMAT_G729A | WAVE_FORMAT_G729_ALT | WAVE_FORMAT_G729_ALT2 => {
                Self::skip_fmt_extension(reader, len)?;
                let channels = map_wave_channel_count(num_channels)?;
                Ok(FormatData::Adpcm(FormatAdpcm {
                    block_align,
                    bits_per_sample,
                    channels,
                    codec: CODEC_ID_G729,
                    extra_data: None,
                }))
            }
            // Microsoft GSM 6.10 (WAV49): mono, 320 samples per 65-byte block. The `WAVEFORMATEX`
            // extension only repeats the samples per block. 0x0032 (MSN Audio) and 0x1500 are
            // the other tags that FFmpeg (`ff_codec_wav_tags`) maps to it.
            WAVE_FORMAT_GSM610 | WAVE_FORMAT_MSNAUDIO | WAVE_FORMAT_GSM_MS_ALT => {
                Self::skip_fmt_extension(reader, len)?;
                let channels = map_wave_channel_count(num_channels)?;
                Ok(FormatData::Adpcm(FormatAdpcm {
                    block_align,
                    bits_per_sample,
                    channels,
                    codec: CODEC_ID_GSM_MS,
                    extra_data: None,
                }))
            }
            // G.726 ADPCM (ITU-T), 2 to 5 bits per sample. 0x0014 and 0x0064 are the other tags
            // that are used for it.
            WAVE_FORMAT_G726
            | WAVE_FORMAT_G723_ADPCM
            | WAVE_FORMAT_G726_ADPCM
            | WAVE_FORMAT_ANTEX_G721 => {
                if !(2..=5).contains(&bits_per_sample) {
                    return decode_error("wav: bits per sample for g726 must be 2 to 5 bits");
                }
                Self::skip_fmt_extension(reader, len)?;
                let channels = map_wave_channel_count(num_channels)?;
                Ok(FormatData::Adpcm(FormatAdpcm {
                    block_align,
                    bits_per_sample,
                    channels,
                    codec: CODEC_ID_ADPCM_G726,
                    extra_data: None,
                }))
            }
            // The 4-bit ADPCM codecs without a codec specific extension, as FFmpeg maps them
            // (`ff_codec_wav_tags`): Creative, Dialogic OKI, Duck DK3 and DK4, and Xbox IMA. The
            // block layout alone decides the samples per block. FFmpeg's decoders for these never
            // look at the bits per sample field, so it isn't checked here either.
            WAVE_FORMAT_CREATIVE_ADPCM
            | WAVE_FORMAT_OKI_ADPCM
            | WAVE_FORMAT_DIALOGIC_OKI_ADPCM
            | WAVE_FORMAT_DK4_ADPCM
            | WAVE_FORMAT_DK3_ADPCM
            | WAVE_FORMAT_XBOX_ADPCM => {
                Self::skip_fmt_extension(reader, len)?;
                let codec = match format {
                    WAVE_FORMAT_CREATIVE_ADPCM => CODEC_ID_ADPCM_CT,
                    WAVE_FORMAT_DK4_ADPCM => CODEC_ID_ADPCM_IMA_DK4,
                    WAVE_FORMAT_DK3_ADPCM => CODEC_ID_ADPCM_IMA_DK3,
                    WAVE_FORMAT_XBOX_ADPCM => CODEC_ID_ADPCM_IMA_XBOX,
                    _ => CODEC_ID_ADPCM_IMA_OKI,
                };
                let channels = map_wave_channel_count(num_channels)?;
                Ok(FormatData::Adpcm(FormatAdpcm {
                    block_align,
                    bits_per_sample: 4,
                    channels,
                    codec,
                    extra_data: None,
                }))
            }
            // Intel Music Coder (0x0401, mono) and Indeo Audio Coder (0x0402, mono or stereo).
            // Blocks of 64 bytes per channel hold 256 samples; the `WAVEFORMATEX` extension is
            // not needed by the decoder.
            WAVE_FORMAT_IMC | WAVE_FORMAT_IAC => {
                Self::skip_fmt_extension(reader, len)?;
                let channels = map_wave_channel_count(num_channels)?;
                Ok(FormatData::Adpcm(FormatAdpcm {
                    block_align,
                    bits_per_sample,
                    channels,
                    codec: if format == WAVE_FORMAT_IMC { CODEC_ID_IMC } else { CODEC_ID_IAC },
                    extra_data: None,
                }))
            }
            // Sony ATRAC3. The 14-byte `WAVEFORMATEX` extension (version, samples per block,
            // coding mode, ...) is passed on verbatim; the decoder validates the block align.
            WAVE_FORMAT_SONY_ATRAC3 => {
                Self::read_atrac3_fmt(reader, block_align, num_channels, len)
            }
            // Yamaha ADPCM, 4 bits per sample.
            WAVE_FORMAT_YAMAHA_ADPCM => {
                if bits_per_sample != 4 {
                    return decode_error("wav: bits per sample for yamaha adpcm must be 4 bits");
                }
                Self::skip_fmt_extension(reader, len)?;
                let channels = map_wave_channel_count(num_channels)?;
                Ok(FormatData::Adpcm(FormatAdpcm {
                    block_align,
                    bits_per_sample,
                    channels,
                    codec: CODEC_ID_ADPCM_YAMAHA,
                    extra_data: None,
                }))
            }
            // G.722 (ITU-T) wideband ADPCM. The audio is always sampled at 16 kHz, regardless of
            // the rate in the header.
            WAVE_FORMAT_G722 => {
                Self::skip_fmt_extension(reader, len)?;
                sample_rate = 16000;
                let channels = map_wave_channel_count(num_channels)?;
                Ok(FormatData::Adpcm(FormatAdpcm {
                    block_align,
                    bits_per_sample: 8,
                    channels,
                    codec: CODEC_ID_ADPCM_G722,
                    extra_data: None,
                }))
            }
            // The MPEG-1 Audio Layer III (MP3) Format.
            WAVE_FORMAT_MPEGLAYER3 => Self::read_mpeg_fmt(reader, num_channels, len, CODEC_ID_MP3),
            // The MPEG-1 Audio Layer I/II Format.
            WAVE_FORMAT_MPEG => Self::read_mpeg_fmt(reader, num_channels, len, CODEC_ID_MP2),
            // Dolby AC-3 (`WAVE_FORMAT_DVM`) as raw frames.
            WAVE_FORMAT_DVM => Self::read_es_fmt(reader, num_channels, len, CODEC_ID_AC3, false),
            // Dolby AC-3 in IEC 61937 bursts.
            WAVE_FORMAT_DOLBY_AC3_SPDIF => {
                Self::read_es_fmt(reader, num_channels, len, CODEC_ID_AC3, true)
            }
            // DTS as raw frames.
            WAVE_FORMAT_DTS2 => Self::read_es_fmt(reader, num_channels, len, CODEC_ID_DCA, false),
            // Unsupported format.
            _ => return unsupported_error("wav: unsupported wave format"),
        }?;

        Ok(WaveFormatChunk {
            num_channels,
            sample_rate,
            avg_bytes_per_sec,
            block_align,
            format_data,
        })
    }
}

impl fmt::Display for WaveFormatChunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "WaveFormatChunk {{")?;
        writeln!(f, "\tnum_channels: {},", self.num_channels)?;
        writeln!(f, "\tsample_rate: {} Hz,", self.sample_rate)?;
        writeln!(f, "\tavg_bytes_per_sec: {},", self.avg_bytes_per_sec)?;
        writeln!(f, "\tblock_align: {},", self.block_align)?;

        match self.format_data {
            FormatData::Pcm(ref pcm) => {
                writeln!(f, "\tformat_data: Pcm {{")?;
                writeln!(f, "\t\tbits_per_sample: {},", pcm.bits_per_sample)?;
                writeln!(f, "\t\tvalid_bits_per_sample: {},", pcm.valid_bits_per_sample)?;
                writeln!(f, "\t\tchannels: {},", pcm.channels)?;
                writeln!(f, "\t\tcodec: {},", pcm.codec)?;
            }
            FormatData::Adpcm(ref adpcm) => {
                writeln!(f, "\tformat_data: Adpcm {{")?;
                writeln!(f, "\t\tblock_align: {},", adpcm.block_align)?;
                writeln!(f, "\t\tbits_per_sample: {},", adpcm.bits_per_sample)?;
                writeln!(f, "\t\tchannels: {},", adpcm.channels)?;
                writeln!(f, "\t\tcodec: {},", adpcm.codec)?;
            }
            FormatData::IeeeFloat(ref ieee) => {
                writeln!(f, "\tformat_data: IeeeFloat {{")?;
                writeln!(f, "\t\tchannels: {},", ieee.channels)?;
                writeln!(f, "\t\tcodec: {},", ieee.codec)?;
            }
            FormatData::Extensible(ref ext) => {
                writeln!(f, "\tformat_data: Extensible {{")?;
                writeln!(f, "\t\tbits_per_sample: {},", ext.bits_per_sample)?;
                writeln!(f, "\t\tvalid_bits_per_sample: {},", ext.valid_bits_per_sample)?;
                writeln!(f, "\t\tchannels: {},", ext.channels)?;
                writeln!(f, "\t\tsub_format_guid: {:?},", ext.sub_format_guid)?;
                writeln!(f, "\t\tcodec: {},", ext.codec)?;
            }
            FormatData::ALaw(ref alaw) => {
                writeln!(f, "\tformat_data: ALaw {{")?;
                writeln!(f, "\t\tchannels: {},", alaw.channels)?;
                writeln!(f, "\t\tcodec: {},", alaw.codec)?;
            }
            FormatData::MuLaw(ref mulaw) => {
                writeln!(f, "\tformat_data: MuLaw {{")?;
                writeln!(f, "\t\tchannels: {},", mulaw.channels)?;
                writeln!(f, "\t\tcodec: {},", mulaw.codec)?;
            }
            FormatData::Mpeg(ref mpeg) => {
                writeln!(f, "\tformat_data: Mpeg {{")?;
                writeln!(f, "\t\tchannels: {},", mpeg.channels)?;
                writeln!(f, "\t\tcodec: {},", mpeg.codec)?;
            }
            FormatData::Es(ref es) => {
                writeln!(f, "\tformat_data: Es {{")?;
                writeln!(f, "\t\tchannels: {},", es.channels)?;
                writeln!(f, "\t\tcodec: {},", es.codec)?;
                writeln!(f, "\t\tspdif: {},", es.spdif)?;
            }
        };

        writeln!(f, "\t}}")?;
        writeln!(f, "}}")
    }
}

pub struct FactChunk {
    pub num_frames: u32,
}

impl ParseChunk for FactChunk {
    fn parse<B: ReadBytes>(reader: &mut B, _tag: [u8; 4], len: u32) -> Result<Self> {
        // A Fact chunk is exactly 4 bytes long, though there is some mystery as to whether there
        // can be more fields in the chunk.
        if len != 4 {
            return decode_error("wav: malformed fact chunk");
        }

        Ok(FactChunk { num_frames: reader.read_u32()? })
    }
}

impl fmt::Display for FactChunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "FactChunk {{")?;
        writeln!(f, "\tnum_frames: {},", self.num_frames)?;
        writeln!(f, "}}")
    }
}

pub struct ListChunk {
    pub form: [u8; 4],
    pub len: u32,
}

impl ListChunk {
    pub fn skip<B: ReadBytes>(&self, reader: &mut B) -> Result<()> {
        ChunksReader::<NullChunks>::new(Some(self.len), ByteOrder::LittleEndian).finish(reader)
    }
}

impl ParseChunk for ListChunk {
    fn parse<B: ReadBytes>(reader: &mut B, _tag: [u8; 4], len: u32) -> Result<Self> {
        // A List chunk must contain atleast the list/form identifier. However, an empty list
        // (len == 4) is permissible.
        if len < 4 {
            return decode_error("wav: malformed list chunk");
        }

        Ok(ListChunk { form: reader.read_quad_bytes()?, len: len - 4 })
    }
}

impl fmt::Display for ListChunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "ListChunk {{")?;
        writeln!(f, "\tform: {},", String::from_utf8_lossy(&self.form))?;
        writeln!(f, "\tlen: {},", self.len)?;
        writeln!(f, "}}")
    }
}

pub struct InfoChunk {
    pub tag: [u8; 4],
    pub buf: Box<[u8]>,
}

impl ParseChunk for InfoChunk {
    fn parse<B: ReadBytes>(reader: &mut B, tag: [u8; 4], len: u32) -> Result<InfoChunk> {
        // INFO strings are short. Don't allocate for an excessively long one, skip it instead.
        const MAX_INFO_LEN: u32 = 1024 * 1024;

        if len > MAX_INFO_LEN {
            reader.ignore_bytes(u64::from(len))?;
            return Ok(InfoChunk { tag, buf: Box::new([]) });
        }

        let buf = reader.read_boxed_slice_exact(len as usize)?;
        Ok(InfoChunk { tag, buf })
    }
}

pub struct DataChunk {
    pub len: Option<u32>,
}

impl ParseChunk for DataChunk {
    fn parse<B: ReadBytes>(_: &mut B, _: [u8; 4], len: u32) -> Result<DataChunk> {
        // If the length is u32::MAX, that usually indicates the file is streaming and the length
        // is not known. For RF64 files, this also indicates the actual size is in the ds64 chunk.
        Ok(DataChunk { len: Some(len).filter(|&len| len != u32::MAX) })
    }
}

/// DS64 chunk contains 64-bit sizes for RF64 files.
/// This chunk must appear before the data chunk in RF64 files.
/// Reference: EBU Tech 3306 - MBWF / RF64: An extended File Format for Audio.
pub struct Ds64Chunk {
    /// 64-bit RIFF chunk size (total file size minus 8).
    pub riff_size: u64,
    /// 64-bit data chunk size.
    pub data_size: u64,
    /// 64-bit sample/frame count (replaces fact chunk value).
    pub sample_count: u64,
}

impl ParseChunk for Ds64Chunk {
    fn parse<B: ReadBytes>(reader: &mut B, _tag: [u8; 4], len: u32) -> Result<Self> {
        // ds64 fixed header: riffSize64 (8) + dataSize64 (8) + sampleCount64 (8) + tableLength (4)
        const DS64_MIN_CHUNK_SIZE: u32 = 28;
        // Each table entry: chunkId (4) + chunkSize64 (8)
        const DS64_TABLE_ENTRY_SIZE: u64 = 12;

        if len < DS64_MIN_CHUNK_SIZE {
            return decode_error("wav: malformed ds64 chunk");
        }

        let riff_size = reader.read_u64()?;
        let data_size = reader.read_u64()?;
        let sample_count = reader.read_u64()?;
        let table_length = reader.read_u32()?;

        // Validate that the table entries fit within the declared chunk length.
        let table_bytes = u64::from(table_length) * DS64_TABLE_ENTRY_SIZE;
        let available_bytes = u64::from(len) - u64::from(DS64_MIN_CHUNK_SIZE);

        if table_bytes > available_bytes {
            return decode_error("wav: ds64 table exceeds chunk size");
        }

        // Skip over the table entries if present.
        if table_length > 0 {
            reader.ignore_bytes(table_bytes)?;
        }

        Ok(Ds64Chunk { riff_size, data_size, sample_count })
    }
}

impl fmt::Display for Ds64Chunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Ds64Chunk {{")?;
        writeln!(f, "\triff_size: {},", self.riff_size)?;
        writeln!(f, "\tdata_size: {},", self.data_size)?;
        writeln!(f, "\tsample_count: {},", self.sample_count)?;
        writeln!(f, "}}")
    }
}

pub enum RiffWaveChunks {
    Format(ChunkParser<WaveFormatChunk>),
    List(ChunkParser<ListChunk>),
    Fact(ChunkParser<FactChunk>),
    Data(ChunkParser<DataChunk>),
    Ds64(ChunkParser<Ds64Chunk>),
    Id3(ChunkParser<Id3Chunk>),
    Extra(ChunkParser<BufChunk>),
}

macro_rules! parser {
    ($class:expr, $result:ty, $tag:expr, $len:expr) => {
        Some($class(ChunkParser::<$result>::new($tag, $len)))
    };
}

impl ParseChunkTag for RiffWaveChunks {
    fn parse_tag(tag: [u8; 4], len: u32) -> Option<Self> {
        match &tag {
            b"fmt " => parser!(RiffWaveChunks::Format, WaveFormatChunk, tag, len),
            b"LIST" => parser!(RiffWaveChunks::List, ListChunk, tag, len),
            b"fact" => parser!(RiffWaveChunks::Fact, FactChunk, tag, len),
            b"data" => parser!(RiffWaveChunks::Data, DataChunk, tag, len),
            b"ds64" => parser!(RiffWaveChunks::Ds64, Ds64Chunk, tag, len),
            b"id3 " | b"ID3 " => parser!(RiffWaveChunks::Id3, Id3Chunk, tag, len),
            _ if is_extra_chunk(&tag, len) => parser!(RiffWaveChunks::Extra, BufChunk, tag, len),
            _ => None,
        }
    }

    fn overrides_parent_len(tag: [u8; 4]) -> bool {
        &tag == b"data"
    }
}

pub enum RiffInfoListChunks {
    Info(ChunkParser<InfoChunk>),
}

impl ParseChunkTag for RiffInfoListChunks {
    fn parse_tag(tag: [u8; 4], len: u32) -> Option<Self> {
        // Right now it is assumed all list chunks are INFO chunks, but that's not really
        // guaranteed.
        //
        // TODO: Actually validate that the chunk is an info chunk.
        parser!(RiffInfoListChunks::Info, InfoChunk, tag, len)
    }
}

pub fn append_fact_params(track: &mut Track, fact: &FactChunk) {
    track.with_num_frames(u64::from(fact.num_frames));
}

pub fn read_info_chunk(source: &mut MediaSourceStream<'_>, len: u32) -> Result<MetadataRevision> {
    let mut builder = MetadataBuilder::new(WAVE_METADATA_INFO);

    let mut list = ChunksReader::<RiffInfoListChunks>::new(Some(len), ByteOrder::LittleEndian);

    while let Some(RiffInfoListChunks::Info(info)) = list.next(source)? {
        let info = info.parse(source)?;
        // Ignore errors while parsing one chunk.
        let _ = riff::parse_riff_info_chunk(info.tag, &info.buf, &mut builder);
    }

    list.finish(source)?;

    Ok(builder.build())
}

/// Chunks that may follow the data chunk and are of interest. All other chunks are skipped.
pub enum RiffTrailingChunks {
    List(ChunkParser<ListChunk>),
    Id3(ChunkParser<Id3Chunk>),
    Extra(ChunkParser<BufChunk>),
}

impl ParseChunkTag for RiffTrailingChunks {
    fn parse_tag(tag: [u8; 4], len: u32) -> Option<Self> {
        match &tag {
            b"LIST" => parser!(RiffTrailingChunks::List, ListChunk, tag, len),
            b"id3 " | b"ID3 " => parser!(RiffTrailingChunks::Id3, Id3Chunk, tag, len),
            _ if is_extra_chunk(&tag, len) => {
                parser!(RiffTrailingChunks::Extra, BufChunk, tag, len)
            }
            _ => None,
        }
    }
}

/// Reads the metadata chunks that follow the data chunk, which many taggers write there, and
/// appends them to `metadata`. The data chunk is at `[data_start_pos, data_end_pos)`.
///
/// This requires seeking. If the source is not seekable, nothing is read. Any malformed chunk
/// following the data chunk ends the search without an error since the audio is still playable.
/// The position of the stream is restored to `data_start_pos` before returning.
pub fn read_trailing_metadata(
    source: &mut MediaSourceStream<'_>,
    data_start_pos: u64,
    data_end_pos: u64,
    metadata: &mut MetadataLog,
    extras: &mut WaveExtras,
) -> Result<()> {
    /// The maximum number of chunks that will be inspected.
    const MAX_CHUNKS: usize = 32;
    /// The maximum length of an INFO list that will be read.
    const MAX_INFO_LIST_LEN: u32 = 1024 * 1024;

    if !source.is_seekable() {
        return Ok(());
    }

    // The data chunk is padded to an even length.
    let next_chunk_pos = data_end_pos.saturating_add((data_end_pos - data_start_pos) & 0x1);

    // Nothing follows the data chunk.
    if source.byte_len().is_some_and(|byte_len| next_chunk_pos.saturating_add(8) > byte_len) {
        return Ok(());
    }

    if source.seek(SeekFrom::Start(next_chunk_pos)).is_ok() {
        // The RIFF chunk length is not used to bound the search since it is frequently not
        // updated when a tagger appends a chunk.
        let mut chunks = ChunksReader::<RiffTrailingChunks>::new(None, ByteOrder::LittleEndian);

        for _ in 0..MAX_CHUNKS {
            let chunk = match chunks.next(source) {
                Ok(Some(chunk)) => chunk,
                _ => break,
            };

            let revision = match chunk {
                RiffTrailingChunks::List(list) => {
                    let Ok(list) = list.parse(source)
                    else {
                        break;
                    };

                    // Only INFO and adtl lists are supported.
                    if &list.form == b"adtl" && list.len <= MAX_INFO_LIST_LEN {
                        if extras.read_adtl_list(source, list.len).is_err() {
                            break;
                        }
                        continue;
                    }

                    if &list.form != b"INFO" {
                        if list.skip(source).is_err() {
                            break;
                        }
                        continue;
                    }

                    if list.len > MAX_INFO_LIST_LEN {
                        break;
                    }

                    read_info_chunk(source, list.len).ok()
                }
                RiffTrailingChunks::Extra(chunk) => {
                    let Ok(chunk) = chunk.parse(source)
                    else {
                        break;
                    };
                    extras.add_chunk(chunk, metadata);
                    continue;
                }
                RiffTrailingChunks::Id3(id3) => id3.parse(source).ok().map(|id3| id3.metadata),
            };

            match revision {
                Some(revision) => metadata.push(revision),
                None => break,
            }
        }
    }

    // Return to the start of the audio data.
    source.seek(SeekFrom::Start(data_start_pos))?;

    Ok(())
}

/// The last 12 bytes of the GUID of the Wave64 "riff" chunk.
pub const W64_RIFF_GUID_TAIL: [u8; 12] =
    [0x2e, 0x91, 0xcf, 0x11, 0xa5, 0xd6, 0x28, 0xdb, 0x04, 0xc1, 0x00, 0x00];

/// The last 12 bytes of the GUIDs of the Wave64 "wave", "fmt ", "data", etc. chunks.
const W64_CHUNK_GUID_TAIL: [u8; 12] =
    [0xf3, 0xac, 0xd3, 0x11, 0x8c, 0xd1, 0x00, 0xc0, 0x4f, 0x8e, 0xdb, 0x8a];

/// The size, in bytes, of a Wave64 chunk header (a 16-byte GUID and a 64-bit size).
const W64_CHUNK_HEADER_LEN: u64 = 24;

/// Reads the remainder of the Wave64 file header, after the first four bytes of the "riff" GUID
/// that identify the file.
pub fn read_w64_header<B: ReadBytes>(reader: &mut B) -> Result<()> {
    let mut guid_tail = [0u8; 12];
    reader.read_buf_exact(&mut guid_tail)?;

    if guid_tail != W64_RIFF_GUID_TAIL {
        return unsupported_error("wav: invalid w64 riff guid");
    }

    // The size of the file, including this header.
    let _file_len = reader.read_u64()?;

    // The form is identified by the GUID of the "wave" chunk.
    let mut form = [0u8; 16];
    reader.read_buf_exact(&mut form)?;

    if form[..4] != *b"wave" || form[4..] != W64_CHUNK_GUID_TAIL {
        return unsupported_error("wav: w64 form is not wave");
    }

    Ok(())
}

/// `W64ChunksReader` reads the chunks of a Sony Wave64 file, and maps them to the same chunk
/// parsers as those of a RIFF/WAVE file.
///
/// Wave64 is structured like RIFF, but chunks are identified by 16-byte GUIDs, chunk sizes are
/// 64-bit and include the chunk header, and chunks are padded to a multiple of 8 bytes. Only the
/// `fmt ` and `data` chunks are used.
pub struct W64ChunksReader {
    /// The number of padding bytes after the previous chunk.
    pad: u64,
}

impl W64ChunksReader {
    pub fn new() -> Self {
        W64ChunksReader { pad: 0 }
    }

    /// Reads chunk headers until a chunk of interest is found. Returns the chunk parser and the
    /// length of the chunk's payload. The length of the payload of a `data` chunk may exceed the
    /// maximum 32-bit length, in which case the length given to the chunk parser is `u32::MAX`.
    pub fn next<B: ReadBytes>(&mut self, reader: &mut B) -> Result<Option<(RiffWaveChunks, u64)>> {
        loop {
            if self.pad > 0 {
                reader.ignore_bytes(self.pad)?;
                self.pad = 0;
            }

            let mut guid = [0u8; 16];
            reader.read_buf_exact(&mut guid)?;

            let chunk_len = reader.read_u64()?;

            // The chunk length includes the chunk header.
            let Some(payload_len) = chunk_len.checked_sub(W64_CHUNK_HEADER_LEN)
            else {
                return decode_error("wav: invalid w64 chunk length");
            };

            self.pad = payload_len.wrapping_neg() & 0x7;

            let tag = [guid[0], guid[1], guid[2], guid[3]];

            let chunk = if guid[4..] == W64_CHUNK_GUID_TAIL {
                match &tag {
                    b"data" => {
                        let len = u32::try_from(payload_len).unwrap_or(u32::MAX);
                        RiffWaveChunks::parse_tag(tag, len)
                    }
                    b"fmt " => u32::try_from(payload_len)
                        .ok()
                        .and_then(|len| RiffWaveChunks::parse_tag(tag, len)),
                    _ => None,
                }
            }
            else {
                None
            };

            match chunk {
                Some(chunk) => return Ok(Some((chunk, payload_len))),
                None => reader.ignore_bytes(payload_len)?,
            }
        }
    }
}

/// Corrects a WAVE channel mask that doesn't is not valid for the stated number of channels.
/// Maps the sub-format GUID of a `WAVE_FORMAT_EXTENSIBLE` fmt chunk to a compressed surround
/// codec and whether the payload is IEC 61937 wrapped.
pub(super) fn es_codec_for_guid(guid: &[u8; 16]) -> Option<(AudioCodecId, bool)> {
    // MEDIASUBTYPE_DOLBY_AC3 and KSDATAFORMAT_SUBTYPE_DOLBY_DIGITAL_PLUS, as listed by ffmpeg.
    #[rustfmt::skip]
    const GUID_AC3: [u8; 16] = [
        0x2c, 0x80, 0x6d, 0xe0, 0x46, 0xdb, 0xcf, 0x11,
        0xb4, 0xd1, 0x00, 0x80, 0x5f, 0x6c, 0xbb, 0xea,
    ];
    #[rustfmt::skip]
    const GUID_EAC3: [u8; 16] = [
        0xaf, 0x87, 0xfb, 0xa7, 0x02, 0x2d, 0xfb, 0x42,
        0xa4, 0xd4, 0x05, 0xcd, 0x93, 0x84, 0x3b, 0xdd,
    ];

    if *guid == GUID_AC3 {
        return Some((CODEC_ID_AC3, false));
    }
    if *guid == GUID_EAC3 {
        return Some((CODEC_ID_EAC3, false));
    }

    // Base GUID: {tag-0000-0010-8000-00aa00389b71}.
    let base = [0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71];
    if guid[4..] != base {
        return None;
    }

    match u32::from_le_bytes([guid[0], guid[1], guid[2], guid[3]]) {
        0x2000 => Some((CODEC_ID_AC3, false)),
        0x2001 => Some((CODEC_ID_DCA, false)),
        0x0092 => Some((CODEC_ID_AC3, true)),
        _ => None,
    }
}

fn fix_wave_channel_mask(mut channel_mask: u32, num_channels: u16) -> Result<u32> {
    let channel_diff = i32::from(num_channels) - channel_mask.count_ones() as i32;

    if channel_diff != 0 {
        info!("channel mask not set correctly, channel positions may be incorrect!");
    }

    // Check that the number of ones in the channel mask match the number of channels.
    if channel_diff > 0 {
        // Too few ones in mask so add extra ones above the most significant one
        let shift = 32 - channel_mask.leading_zeros();

        // There must be enough room above the most significant one for the missing channels.
        if shift as i32 + channel_diff > 32 {
            return unsupported_error("wav: too many channels in mask for fmt_ext");
        }

        channel_mask |= (((1u64 << channel_diff) - 1) << shift) as u32;
    }
    else {
        // Too many ones in mask so remove the most significant extra ones
        while channel_mask.count_ones() != u32::from(num_channels) {
            let highest_one = 31 - (!channel_mask).leading_ones();
            channel_mask &= !(1 << highest_one);
        }
    }

    Ok(channel_mask)
}

#[test]
fn test_fix_channel_mask() {
    // Too few
    assert_eq!(fix_wave_channel_mask(0, 9).unwrap(), 0b111111111);
    assert_eq!(fix_wave_channel_mask(0b101000, 5).unwrap(), 0b111101000);

    // Too many
    assert_eq!(fix_wave_channel_mask(0b1111111, 0).unwrap(), 0);
    assert_eq!(fix_wave_channel_mask(0b101110111010, 5).unwrap(), 0b10111010);
    assert_eq!(fix_wave_channel_mask(0xFFFFFFFF, 8).unwrap(), 0b11111111);

    // Not enough free bits above the mask to add the missing channels.
    assert!(fix_wave_channel_mask(0, 40).is_err());
    assert!(fix_wave_channel_mask(0x8000_0000, 2).is_err());
    assert!(fix_wave_channel_mask(0x4000_0000, 3).is_err());
    assert_eq!(fix_wave_channel_mask(0x4000_0000, 2).unwrap(), 0xC000_0000);
}

#[test]
fn test_fmt_pcm_block_align_does_not_overflow() {
    use symphonia_core::io::BufReader;

    // 32-bit samples with a channel count whose block align exceeds u16::MAX must yield an
    // error (too many channels), not an arithmetic overflow.
    let mut reader = BufReader::new(&[]);
    assert!(WaveFormatChunk::read_pcm_fmt(&mut reader, 0, 32, 20000, 16).is_err());
}

/// Map a WAVE channel count to a set of channels.
fn map_wave_channel_count(count: u16) -> Result<Channels> {
    // There must be atleast one channel.
    (1..)
        .contains(&count)
        .then(|| Position::from_count(u32::from(count)))
        .flatten()
        .map(Channels::Positioned)
        .ok_or(Error::DecodeError("riff: invalid channel count"))
}

#[test]
fn test_map_wave_channel_count() {
    assert!(map_wave_channel_count(0).is_err());

    for i in 1..29 {
        assert!(map_wave_channel_count(i).is_ok());
    }

    for i in 29..u16::MAX {
        assert!(map_wave_channel_count(i).is_err());
    }
}

/// Map a channel count to a set of Ambisonic B-format components.
fn map_amb_channel_count(count: u16) -> Result<Channels> {
    let components: &[AmbisonicBFormat] = match count {
        // W
        1 => &[AmbisonicBFormat::W],
        // WY
        2 => &[AmbisonicBFormat::W, AmbisonicBFormat::Y],
        // WXY
        3 => &[AmbisonicBFormat::W, AmbisonicBFormat::X, AmbisonicBFormat::Y],
        // WXYZ
        4 => &[AmbisonicBFormat::W, AmbisonicBFormat::X, AmbisonicBFormat::Y, AmbisonicBFormat::Z],
        // WXY,UV
        5 => &[
            // 1st order.
            AmbisonicBFormat::W,
            AmbisonicBFormat::X,
            AmbisonicBFormat::Y,
            // 2nd order.
            AmbisonicBFormat::U,
            AmbisonicBFormat::V,
        ],
        // WXYZ,UV
        6 => &[
            // 1st order.
            AmbisonicBFormat::W,
            AmbisonicBFormat::X,
            AmbisonicBFormat::Y,
            AmbisonicBFormat::Z,
            // 2nd order.
            AmbisonicBFormat::U,
            AmbisonicBFormat::V,
        ],
        // WXY,UV,PQ
        7 => &[
            // 1st order.
            AmbisonicBFormat::W,
            AmbisonicBFormat::X,
            AmbisonicBFormat::Y,
            // 2nd order.
            AmbisonicBFormat::U,
            AmbisonicBFormat::V,
            // 3rd order.
            AmbisonicBFormat::P,
            AmbisonicBFormat::Q,
        ],
        // WXYZ,UV,PQ
        8 => &[
            // 1st order.
            AmbisonicBFormat::W,
            AmbisonicBFormat::X,
            AmbisonicBFormat::Y,
            AmbisonicBFormat::Z,
            // 2nd order.
            AmbisonicBFormat::U,
            AmbisonicBFormat::V,
            // 3rd order.
            AmbisonicBFormat::P,
            AmbisonicBFormat::Q,
        ],
        // WXYZ,RSTUV
        9 => &[
            // 1st order.
            AmbisonicBFormat::W,
            AmbisonicBFormat::X,
            AmbisonicBFormat::Y,
            AmbisonicBFormat::Z,
            // 2nd order.
            AmbisonicBFormat::R,
            AmbisonicBFormat::S,
            AmbisonicBFormat::T,
            AmbisonicBFormat::U,
            AmbisonicBFormat::V,
        ],
        // WXYZ,RSTUV,PQ
        11 => &[
            // 1st order.
            AmbisonicBFormat::W,
            AmbisonicBFormat::X,
            AmbisonicBFormat::Y,
            AmbisonicBFormat::Z,
            // 2nd order.
            AmbisonicBFormat::R,
            AmbisonicBFormat::S,
            AmbisonicBFormat::T,
            AmbisonicBFormat::U,
            AmbisonicBFormat::V,
            // 3rd order.
            AmbisonicBFormat::P,
            AmbisonicBFormat::Q,
        ],
        // WXYZ,RSTUV,KLMNOPQ
        16 => &[
            // 1st order.
            AmbisonicBFormat::W,
            AmbisonicBFormat::X,
            AmbisonicBFormat::Y,
            AmbisonicBFormat::Z,
            // 2nd order.
            AmbisonicBFormat::R,
            AmbisonicBFormat::S,
            AmbisonicBFormat::T,
            AmbisonicBFormat::U,
            AmbisonicBFormat::V,
            // 3rd order.
            AmbisonicBFormat::K,
            AmbisonicBFormat::L,
            AmbisonicBFormat::M,
            AmbisonicBFormat::N,
            AmbisonicBFormat::O,
            AmbisonicBFormat::P,
            AmbisonicBFormat::Q,
        ],
        _ => return decode_error("wav: invalid ambisonic channel count"),
    };

    let labels = components
        .iter()
        .map(|&c| ChannelLabel::AmbisonicBFormat(c))
        .collect::<Vec<ChannelLabel>>()
        .into_boxed_slice();

    Ok(Channels::Custom(labels))
}

#[cfg(test)]
mod ds64_tests {
    use super::*;
    use std::io::Cursor;
    use symphonia_core::io::ReadOnlySource;

    #[test]
    fn test_ds64_chunk_parse() {
        // ds64 chunk: riffSize=0x123456789ABCDEF0, dataSize=0x0FEDCBA987654321,
        // sampleCount=0x0000000100000000, tableLen=0
        let data: [u8; 28] = [
            // riffSize64 (little-endian)
            0xF0, 0xDE, 0xBC, 0x9A, 0x78, 0x56, 0x34, 0x12, // dataSize64 (little-endian)
            0x21, 0x43, 0x65, 0x87, 0xA9, 0xCB, 0xED, 0x0F,
            // sampleCount64 (little-endian)
            0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // tableLength
            0x00, 0x00, 0x00, 0x00,
        ];

        let source = ReadOnlySource::new(Cursor::new(data));
        let mut reader = MediaSourceStream::new(Box::new(source), Default::default());

        let ds64 = Ds64Chunk::parse(&mut reader, *b"ds64", 28).unwrap();

        assert_eq!(ds64.riff_size, 0x123456789ABCDEF0);
        assert_eq!(ds64.data_size, 0x0FEDCBA987654321);
        assert_eq!(ds64.sample_count, 0x0000000100000000);
    }

    #[test]
    fn test_ds64_chunk_with_table() {
        // ds64 chunk with 1 table entry
        let data: [u8; 40] = [
            // riffSize64
            0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, // dataSize64
            0x00, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, // sampleCount64
            0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // tableLength = 1
            0x01, 0x00, 0x00, 0x00, // table entry: "level" + size
            b'l', b'e', b'v', b'l', 0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00,
        ];

        let source = ReadOnlySource::new(Cursor::new(data));
        let mut reader = MediaSourceStream::new(Box::new(source), Default::default());

        let ds64 = Ds64Chunk::parse(&mut reader, *b"ds64", 40).unwrap();

        assert_eq!(ds64.riff_size, 0x0000001000000000);
        assert_eq!(ds64.data_size, 0x0000000800000000);
        assert_eq!(ds64.sample_count, 0x1000);
    }

    #[test]
    fn test_ds64_chunk_too_short() {
        let data: [u8; 20] = [0u8; 20];

        let source = ReadOnlySource::new(Cursor::new(data));
        let mut reader = MediaSourceStream::new(Box::new(source), Default::default());

        let result = Ds64Chunk::parse(&mut reader, *b"ds64", 20);
        assert!(result.is_err());
    }

    #[test]
    fn test_ds64_table_exceeds_chunk_size() {
        // ds64 with table_length=100 but chunk len=28 (no room for table entries)
        let data: [u8; 28] = [
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // riffSize64
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // dataSize64
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // sampleCount64
            0x64, 0x00, 0x00, 0x00, // tableLength = 100
        ];

        let source = ReadOnlySource::new(Cursor::new(data));
        let mut reader = MediaSourceStream::new(Box::new(source), Default::default());

        let result = Ds64Chunk::parse(&mut reader, *b"ds64", 28);
        assert!(result.is_err());
    }
}
