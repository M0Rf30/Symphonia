// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use log::{debug, error, info, warn};
use std::{convert::TryFrom, fmt, mem::size_of, str};
use symphonia_core::{
    audio::{AmbisonicBFormat, ChannelLabel, Channels, Position},
    codecs::audio::{AudioCodecId, well_known::*},
    errors::{Error, Result, decode_error, unsupported_error},
    io::{MediaSourceStream, ReadBytes, ScopedStream, read_base128_be_u64},
    meta::MetadataRevision,
    units::{Duration, Timestamp},
};
use symphonia_metadata::embedded::riff::read_riff_id3_chunk;

// CAF audio channel layouts.
const LAYOUT_TAG_USE_CHANNEL_DESCRIPTIONS: u32 = 0;
const LAYOUT_TAG_USE_CHANNEL_BITMAP: u32 = 1 << 16;
// CAF audio channel labels.
const CHANNEL_LABEL_LEFT: u32 = 1;
const CHANNEL_LABEL_RIGHT: u32 = 2;
const CHANNEL_LABEL_CENTER: u32 = 3;
const CHANNEL_LABEL_LFE_SCREEN: u32 = 4;
const CHANNEL_LABEL_LEFT_SURROUND: u32 = 5;
const CHANNEL_LABEL_RIGHT_SURROUND: u32 = 6;
const CHANNEL_LABEL_LEFT_CENTER: u32 = 7;
const CHANNEL_LABEL_RIGHT_CENTER: u32 = 8;
const CHANNEL_LABEL_CENTER_SURROUND: u32 = 9;
const CHANNEL_LABEL_LEFT_SURROUND_DIRECT: u32 = 10;
const CHANNEL_LABEL_RIGHT_SURROUND_DIRECT: u32 = 11;
const CHANNEL_LABEL_TOP_CENTER_SURROUND: u32 = 12;
const CHANNEL_LABEL_VERTICAL_HEIGHT_LEFT: u32 = 13;
const CHANNEL_LABEL_VERTICAL_HEIGHT_CENTER: u32 = 14;
const CHANNEL_LABEL_VERTICAL_HEIGHT_RIGHT: u32 = 15;
const CHANNEL_LABEL_TOP_BACK_LEFT: u32 = 16;
const CHANNEL_LABEL_TOP_BACK_CENTER: u32 = 17;
const CHANNEL_LABEL_TOP_BACK_RIGHT: u32 = 18;
const CHANNEL_LABEL_REAR_SURROUND_LEFT: u32 = 33;
const CHANNEL_LABEL_REAR_SURROUND_RIGHT: u32 = 34;
const CHANNEL_LABEL_LEFT_WIDE: u32 = 35;
const CHANNEL_LABEL_RIGHT_WIDE: u32 = 36;
const CHANNEL_LABEL_LFE2: u32 = 37;
const CHANNEL_LABEL_LEFT_TOTAL: u32 = 38;
const CHANNEL_LABEL_RIGHT_TOTAL: u32 = 39;
const CHANNEL_LABEL_MONO: u32 = 42;
const CHANNEL_LABEL_CENTER_SURROUND_DIRECT: u32 = 44;
const CHANNEL_LABEL_AMBISONIC_W: u32 = 200;
const CHANNEL_LABEL_AMBISONIC_X: u32 = 201;
const CHANNEL_LABEL_AMBISONIC_Y: u32 = 202;
const CHANNEL_LABEL_AMBISONIC_Z: u32 = 203;
const CHANNEL_LABEL_HEADPHONES_LEFT: u32 = 301;
const CHANNEL_LABEL_HEADPHONES_RIGHT: u32 = 302;
const CHANNEL_LABEL_DISCRETE_0: u32 = (1 << 16) | 0;
const CHANNEL_LABEL_DISCRETE_65535: u32 = (1 << 16) | 65535;
const CHANNEL_LABEL_MS_MID: u32 = 204;
const CHANNEL_LABEL_MS_SIDE: u32 = 205;
const CHANNEL_LABEL_XY_X: u32 = 206;
const CHANNEL_LABEL_XY_Y: u32 = 207;
const CHANNEL_LABEL_HOA_ACN_0: u32 = (2 << 16) | 0;
const CHANNEL_LABEL_HOA_ACN_65024: u32 = (2 << 16) | 65024;

#[derive(Debug)]
pub enum Chunk {
    AudioDescription(AudioDescription),
    AudioData(AudioData),
    ChannelLayout(ChannelLayout),
    PacketTable(PacketTable),
    MagicCookie(Box<[u8]>),
    /// The `info` chunk: an ordered list of key/value string pairs of free-form metadata (e.g.
    /// `artist`, `album`, `title`, `track`). See Apple's CAF specification, "Information Chunk".
    Info(Vec<(String, String)>),
    /// The `mark` chunk: the markers of the file. See "Marker Chunk".
    Markers(Vec<Marker>),
    /// The `regn` chunk: the regions of the file. See "Region Chunk".
    Regions(Vec<Region>),
    /// The `strg` chunk: the strings (labels of markers and regions) by ID. See "Strings Chunk".
    Strings(Vec<(u32, String)>),
    Free,
    /// An ID3v2 tag stored in a (non-standard) `ID3 `/`id3 ` chunk.
    Id3(MetadataRevision),
}

impl Chunk {
    /// Reads a chunk
    ///
    /// After calling this function the reader's position will be:
    ///   - at the start of the next chunk,
    ///   - or, at the end of the file,
    ///   - or, if the chunk is the audio data chunk and the size is unknown,
    ///     then at the start of the audio data.
    ///
    /// The first chunk read will be the AudioDescription chunk. Once it's been read, the caller
    /// should pass it in to subsequent read calls.
    pub fn read(
        reader: &mut MediaSourceStream<'_>,
        audio_description: &Option<AudioDescription>,
    ) -> Result<Option<Self>> {
        let chunk_type = reader.read_quad_bytes()?;
        let chunk_size = reader.read_be_i64()?;

        let result = match &chunk_type {
            b"desc" => Chunk::AudioDescription(AudioDescription::read(reader, chunk_size)?),
            b"data" => Chunk::AudioData(AudioData::read(reader, chunk_size)?),
            b"chan" => Chunk::ChannelLayout(ChannelLayout::read(reader, chunk_size)?),
            b"pakt" => {
                Chunk::PacketTable(PacketTable::read(reader, audio_description, chunk_size)?)
            }
            b"kuki" => {
                if let Ok(chunk_size) = usize::try_from(chunk_size) {
                    Chunk::MagicCookie(reader.read_boxed_slice_exact(chunk_size)?)
                }
                else {
                    return invalid_chunk_size_error("Magic Cookie", chunk_size);
                }
            }
            b"info" => Chunk::Info(read_info_chunk(reader, chunk_size)?),
            b"mark" | b"regn" | b"strg" => {
                let Some(data) = read_small_chunk(reader, chunk_size, &chunk_type)?
                else {
                    return Ok(Some(Chunk::Free));
                };

                match &chunk_type {
                    b"mark" => Chunk::Markers(parse_marker_chunk(&data)),
                    b"regn" => Chunk::Regions(parse_region_chunk(&data)),
                    _ => Chunk::Strings(parse_strings_chunk(&data)),
                }
            }
            b"free" => {
                if chunk_size < 0 {
                    return invalid_chunk_size_error("Free", chunk_size);
                }
                reader.ignore_bytes(chunk_size as u64)?;
                Chunk::Free
            }
            b"ID3 " | b"id3 " => {
                let Ok(chunk_len) = u64::try_from(chunk_size)
                else {
                    return invalid_chunk_size_error("ID3", chunk_size);
                };

                // Scope the parse to the chunk, and always leave the reader at the next chunk.
                let mut scoped = ScopedStream::new(&mut *reader, chunk_len);
                let mut side_data = Vec::new();
                let rev = read_riff_id3_chunk(&mut scoped, &mut side_data);
                scoped.ignore()?;

                match rev {
                    Ok(rev) => Chunk::Id3(rev),
                    Err(err) => {
                        warn!("failed to read ID3v2 chunk: {err}");
                        Chunk::Free
                    }
                }
            }
            other => {
                // Log unsupported chunk types but don't return an error
                info!("unsupported chunk type ('{}')", str::from_utf8(other).unwrap_or("????"));

                if chunk_size >= 0 {
                    reader.ignore_bytes(chunk_size as u64)?;
                    return Ok(None);
                }
                else {
                    return invalid_chunk_size_error("unsupported", chunk_size);
                }
            }
        };

        debug!("chunk: {result:?} - size: {chunk_size}");
        Ok(Some(result))
    }
}

#[derive(Debug)]
pub struct AudioDescription {
    pub sample_rate: f64,
    pub format_id: AudioDescriptionFormatId,
    pub bytes_per_packet: u32,
    pub frames_per_packet: u32,
    pub channels_per_frame: u32,
    pub bits_per_channel: u32,
}

impl AudioDescription {
    pub fn read(reader: &mut MediaSourceStream<'_>, chunk_size: i64) -> Result<Self> {
        if chunk_size != 32 {
            return invalid_chunk_size_error("Audio Description", chunk_size);
        }

        let sample_rate = reader.read_be_f64()?;
        if sample_rate == 0.0 {
            return decode_error("caf: sample rate must be not be zero");
        }

        let format_id = AudioDescriptionFormatId::read(reader)?;

        let bytes_per_packet = reader.read_be_u32()?;
        let frames_per_packet = reader.read_be_u32()?;

        let channels_per_frame = reader.read_be_u32()?;
        if channels_per_frame == 0 {
            return decode_error("caf: channels per frame must be not be zero");
        }

        let bits_per_channel = reader.read_be_u32()?;

        Ok(Self {
            sample_rate,
            format_id,
            bytes_per_packet,
            frames_per_packet,
            channels_per_frame,
            bits_per_channel,
        })
    }

    /// For linear PCM, the width in bits of the byte-aligned container each sample is stored in.
    ///
    /// Per the CAF specification, linear PCM always has one frame per packet, and the number of
    /// bytes per packet is the number of bytes per frame. Samples that are not byte-aligned (e.g.,
    /// 12-bit) or unpacked (e.g., 24-bit in 4 bytes) are high-aligned within their container.
    pub fn lpcm_container_bits(&self) -> Result<u32> {
        if self.frames_per_packet != 1
            || self.bytes_per_packet == 0
            || self.bytes_per_packet % self.channels_per_frame != 0
        {
            error!(
                "unsupported linear PCM packetization (bytes: {}, frames: {}, channels: {})",
                self.bytes_per_packet, self.frames_per_packet, self.channels_per_frame
            );
            return unsupported_error("caf: unsupported linear PCM packetization");
        }

        // Cannot overflow: bytes_per_packet / channels_per_frame is at most u32::MAX.
        Ok(8 * (self.bytes_per_packet / self.channels_per_frame))
    }

    pub fn codec_id(&self) -> Result<AudioCodecId> {
        use AudioDescriptionFormatId::*;

        let result = match &self.format_id {
            LinearPCM { floating_point, little_endian } => {
                let width = self.lpcm_container_bits()?;

                // The number of significant bits must fit in the container.
                if self.bits_per_channel == 0 || self.bits_per_channel > width {
                    error!(
                        "invalid linear PCM bits per channel ({}) for a {width}-bit sample",
                        self.bits_per_channel
                    );
                    return decode_error("caf: invalid bits per channel");
                }

                if *floating_point {
                    if self.bits_per_channel != width {
                        error!("unsupported PCM floating point format (bits: {width})");
                        return unsupported_error("caf: unsupported bits per channel");
                    }

                    match (width, *little_endian) {
                        (32, true) => CODEC_ID_PCM_F32LE,
                        (32, false) => CODEC_ID_PCM_F32BE,
                        (64, true) => CODEC_ID_PCM_F64LE,
                        (64, false) => CODEC_ID_PCM_F64BE,
                        (bits, _) => {
                            error!("unsupported PCM floating point format (bits: {bits})");
                            return unsupported_error("caf: unsupported bits per channel");
                        }
                    }
                }
                else {
                    // The codec is selected by the width of the container. The number of
                    // significant bits (<= the container width) is carried by the codec
                    // parameters.
                    match (width, *little_endian) {
                        // Endianness is meaningless for 8-bit samples, which are signed in CAF.
                        (8, _) => CODEC_ID_PCM_S8,
                        (16, true) => CODEC_ID_PCM_S16LE,
                        (16, false) => CODEC_ID_PCM_S16BE,
                        (24, true) => CODEC_ID_PCM_S24LE,
                        (24, false) => CODEC_ID_PCM_S24BE,
                        (32, true) => CODEC_ID_PCM_S32LE,
                        (32, false) => CODEC_ID_PCM_S32BE,
                        (bits, _) => {
                            error!("unsupported PCM integer format (bits: {bits})");
                            return unsupported_error("caf: unsupported bits per channel");
                        }
                    }
                }
            }
            AppleIMA4 => CODEC_ID_ADPCM_IMA_QT,
            MsAdpcm => CODEC_ID_ADPCM_MS,
            ImaAdpcmWav => CODEC_ID_ADPCM_IMA_WAV,
            MPEG4AAC => CODEC_ID_AAC,
            ULaw => CODEC_ID_PCM_MULAW,
            ALaw => CODEC_ID_PCM_ALAW,
            MPEGLayer1 => CODEC_ID_MP1,
            MPEGLayer2 => CODEC_ID_MP2,
            MPEGLayer3 => CODEC_ID_MP3,
            AppleLossless => CODEC_ID_ALAC,
            Flac => CODEC_ID_FLAC,
            Opus => CODEC_ID_OPUS,
            Ac3 => CODEC_ID_AC3,
            Eac3 => CODEC_ID_EAC3,
            unsupported => {
                error!("unsupported codec ({unsupported:?})");
                return unsupported_error("caf: unsupported codec");
            }
        };

        Ok(result)
    }

    pub fn is_variable_packet_format(&self) -> bool {
        self.bytes_per_packet == 0 || self.frames_per_packet == 0
    }
}

#[derive(Debug)]
pub struct AudioData {
    pub _edit_count: u32,
    pub start_pos: u64,
    pub data_len: Option<u64>,
}

impl AudioData {
    pub fn read(reader: &mut MediaSourceStream<'_>, chunk_size: i64) -> Result<Self> {
        let edit_count_offset = size_of::<u32>() as i64;

        if chunk_size != -1 && chunk_size < edit_count_offset {
            return invalid_chunk_size_error("Audio Data", chunk_size);
        }

        let edit_count = reader.read_be_u32()?;
        let start_pos = reader.pos();

        if chunk_size == -1 {
            return Ok(Self { _edit_count: edit_count, start_pos, data_len: None });
        }

        let data_len = (chunk_size - edit_count_offset) as u64;
        debug!("data_len: {data_len}");
        reader.ignore_bytes(data_len)?;
        Ok(Self { _edit_count: edit_count, start_pos, data_len: Some(data_len) })
    }
}

#[derive(Debug)]
pub enum AudioDescriptionFormatId {
    LinearPCM {
        floating_point: bool,
        little_endian: bool,
    },
    AppleIMA4,
    /// Microsoft ADPCM (`'ms\0\x02'`).
    MsAdpcm,
    /// IMA ADPCM in the WAVE block layout (`'ms\0\x11'`).
    ImaAdpcmWav,
    MPEG4AAC,
    MACE3,
    MACE6,
    ULaw,
    ALaw,
    MPEGLayer1,
    MPEGLayer2,
    MPEGLayer3,
    AppleLossless,
    Flac,
    Opus,
    /// Dolby Digital (`'ac-3'`).
    Ac3,
    /// Dolby Digital Plus (`'ec-3'`).
    Eac3,
}

impl AudioDescriptionFormatId {
    pub fn read(reader: &mut MediaSourceStream<'_>) -> Result<Self> {
        use AudioDescriptionFormatId::*;

        let format_id = reader.read_quad_bytes()?;
        let format_flags = reader.read_be_u32()?;

        let result = match &format_id {
            // Formats mentioned in the spec
            b"lpcm" => {
                let floating_point = format_flags & (1 << 0) != 0;
                let little_endian = format_flags & (1 << 1) != 0;
                return Ok(LinearPCM { floating_point, little_endian });
            }
            b"ima4" => AppleIMA4,
            b"aac " => {
                if format_flags != 2 {
                    warn!("undocumented AAC object type ({format_flags})");
                }
                return Ok(MPEG4AAC);
            }
            // The MPEG-4 AAC variants from CoreAudioBaseTypes.h (HE-AAC, HE-AAC v2, LD, ELD, ELD
            // with SBR, ELD v2). The audio object type is carried by the magic cookie's audio
            // specific config, so they all decode as AAC.
            b"aach" | b"aacp" | b"aacl" | b"aace" | b"aacf" | b"aacg" => return Ok(MPEG4AAC),
            b"ms\x00\x02" => MsAdpcm,
            b"ms\x00\x11" => ImaAdpcmWav,
            b"MAC3" => MACE3,
            b"MAC6" => MACE6,
            b"ulaw" => ULaw,
            b"alaw" => ALaw,
            b".mp1" => MPEGLayer1,
            b".mp2" => MPEGLayer2,
            b".mp3" => MPEGLayer3,
            b"alac" => AppleLossless,
            // Additional formats from CoreAudioBaseTypes.h
            b"flac" => Flac,
            b"opus" => Opus,
            b"ac-3" => Ac3,
            b"ec-3" => Eac3,
            other => {
                error!("unsupported format id ({other:?})");
                return unsupported_error("caf: unsupported format id");
            }
        };

        if format_flags != 0 {
            info!("non-zero format flags ({format_flags})");
        }

        Ok(result)
    }
}

#[derive(Debug)]
pub struct ChannelLayout {
    pub channel_layout: u32,
    pub channel_bitmap: u32,
    pub channel_descriptions: Vec<ChannelDescription>,
}

impl ChannelLayout {
    pub fn read(reader: &mut MediaSourceStream<'_>, chunk_size: i64) -> Result<Self> {
        if chunk_size < 12 {
            return invalid_chunk_size_error("Channel Layout", chunk_size);
        }

        let channel_layout = reader.read_be_u32()?;
        let channel_bitmap = reader.read_be_u32()?;
        let channel_description_count = reader.read_be_u32()?;

        // Each channel description is 20 bytes. The count must be consistent with the chunk size.
        if u64::from(channel_description_count) * 20 > (chunk_size as u64) - 12 {
            return decode_error("caf: channel description count exceeds chunk size");
        }

        let channel_descriptions: Vec<ChannelDescription> = (0..channel_description_count)
            .map(|_| ChannelDescription::read(reader))
            .collect::<Result<_>>()?;

        // Skip any trailing data in the chunk.
        let consumed = 12 + 20 * u64::from(channel_description_count);
        reader.ignore_bytes((chunk_size as u64) - consumed)?;

        Ok(Self { channel_layout, channel_bitmap, channel_descriptions })
    }

    /// Convert the layout into the set of channels it describes, in file order.
    ///
    /// `num_channels` is the channel count from the audio description. Returns `None` if the
    /// layout does not describe exactly `num_channels` channels, or if it cannot be represented.
    pub fn channels(&self, num_channels: u32) -> Option<Channels> {
        let channels = match self.channel_layout {
            // Use channel descriptions
            LAYOUT_TAG_USE_CHANNEL_DESCRIPTIONS => {
                let labels: Vec<u32> =
                    self.channel_descriptions.iter().map(|desc| desc.channel_label).collect();
                channels_from_labels(&labels)
            }
            // Use the channel bitmap
            LAYOUT_TAG_USE_CHANNEL_BITMAP => {
                // The CAF channel bitmap is identical to a WAVE channel mask.
                match Position::from_wave_channel_mask(self.channel_bitmap) {
                    Some(positions) => Channels::Positioned(positions),
                    None => {
                        warn!("unsupported channel bitmap: {}", self.channel_bitmap);
                        return None;
                    }
                }
            }
            // Layout tags: the high 16 bits are the ordering, the low 16 bits are the number of
            // channels.
            tag => {
                let Some(labels) = layout_tag_labels(tag >> 16)
                else {
                    debug!("unsupported channel layout: {tag}");
                    return None;
                };

                if (tag & 0xffff) as usize != labels.len() {
                    warn!("channel layout tag {tag} has an inconsistent channel count");
                    return None;
                }

                channels_from_labels(labels)
            }
        };

        if channels.count() != num_channels as usize {
            warn!(
                "channel layout describes {} channels, but the audio has {num_channels}",
                channels.count()
            );
            return None;
        }

        Some(channels)
    }
}

/// The channel labels, in channel order, of a channel layout tag's ordering (the high 16 bits of
/// `mChannelLayoutTag`). See the CAF specification, "Channel Layout Tags".
///
/// Layouts that are aliases of another (e.g., `MPEG_1_0` and `Mono`) share the same ordering.
fn layout_tag_labels(ordering: u32) -> Option<&'static [u32]> {
    const L: u32 = CHANNEL_LABEL_LEFT;
    const R: u32 = CHANNEL_LABEL_RIGHT;
    const C: u32 = CHANNEL_LABEL_CENTER;
    const LFE: u32 = CHANNEL_LABEL_LFE_SCREEN;
    const LS: u32 = CHANNEL_LABEL_LEFT_SURROUND;
    const RS: u32 = CHANNEL_LABEL_RIGHT_SURROUND;
    const LC: u32 = CHANNEL_LABEL_LEFT_CENTER;
    const RC: u32 = CHANNEL_LABEL_RIGHT_CENTER;
    const CS: u32 = CHANNEL_LABEL_CENTER_SURROUND;
    const LSD: u32 = CHANNEL_LABEL_LEFT_SURROUND_DIRECT;
    const RSD: u32 = CHANNEL_LABEL_RIGHT_SURROUND_DIRECT;
    const VHL: u32 = CHANNEL_LABEL_VERTICAL_HEIGHT_LEFT;
    const VHC: u32 = CHANNEL_LABEL_VERTICAL_HEIGHT_CENTER;
    const VHR: u32 = CHANNEL_LABEL_VERTICAL_HEIGHT_RIGHT;
    const TBL: u32 = CHANNEL_LABEL_TOP_BACK_LEFT;
    const TBR: u32 = CHANNEL_LABEL_TOP_BACK_RIGHT;
    const RLS: u32 = CHANNEL_LABEL_REAR_SURROUND_LEFT;
    const RRS: u32 = CHANNEL_LABEL_REAR_SURROUND_RIGHT;
    const LW: u32 = CHANNEL_LABEL_LEFT_WIDE;
    const RW: u32 = CHANNEL_LABEL_RIGHT_WIDE;
    const LFE2: u32 = CHANNEL_LABEL_LFE2;
    const LT: u32 = CHANNEL_LABEL_LEFT_TOTAL;
    const RT: u32 = CHANNEL_LABEL_RIGHT_TOTAL;
    const CSD: u32 = CHANNEL_LABEL_CENTER_SURROUND_DIRECT;
    // CoreAudio labels not enumerated by the CAF specification (hearing impaired, visually
    // impaired narration, haptic). These have no speaker position.
    const HI: u32 = 40;
    const VI: u32 = 41;
    const HAPTIC: u32 = 45;

    let labels: &'static [u32] = match ordering {
        // Mono, MPEG_1_0, ITU_1_0, DVD_0.
        100 => &[L],
        // Stereo, StereoHeadphones, MPEG_2_0, ITU_2_0, DVD_1.
        101 | 102 => &[L, R],
        // MatrixStereo.
        103 => &[LT, RT],
        // MidSide.
        104 => &[CHANNEL_LABEL_MS_MID, CHANNEL_LABEL_MS_SIDE],
        // XY.
        105 => &[CHANNEL_LABEL_XY_X, CHANNEL_LABEL_XY_Y],
        // Binaural.
        106 => &[L, R],
        // Ambisonic B format: W X Y Z.
        107 => &[
            CHANNEL_LABEL_AMBISONIC_W,
            CHANNEL_LABEL_AMBISONIC_X,
            CHANNEL_LABEL_AMBISONIC_Y,
            CHANNEL_LABEL_AMBISONIC_Z,
        ],
        // Quadraphonic, AudioUnit_4, AAC_Quadraphonic: L R Ls Rs.
        108 => &[L, R, LS, RS],
        // Pentagonal, AudioUnit_5: L R Ls Rs C.
        109 => &[L, R, LS, RS, C],
        // Hexagonal, AudioUnit_6: L R Ls Rs C Cs.
        110 => &[L, R, LS, RS, C, CS],
        // Octagonal, AudioUnit_8: L R Ls Rs C Cs Lsd Rsd.
        111 => &[L, R, LS, RS, C, CS, LSD, RSD],
        // Cube: L R Ls Rs, top L, top R, top rear L, top rear R.
        112 => &[L, R, LS, RS, VHL, VHR, TBL, TBR],
        // MPEG_3_0_A, ITU_3_0, DVD_7.
        113 => &[L, R, C],
        // MPEG_3_0_B.
        114 => &[C, L, R],
        // MPEG_4_0_A, ITU_3_1, DVD_8, DVD_13.
        115 => &[L, R, C, CS],
        // MPEG_4_0_B, AAC_4_0.
        116 => &[C, L, R, CS],
        // MPEG_5_0_A, ITU_3_2, DVD_9, DVD_14.
        117 => &[L, R, C, LS, RS],
        // MPEG_5_0_B, DVD_19, AudioUnit_5_0.
        118 => &[L, R, LS, RS, C],
        // MPEG_5_0_C.
        119 => &[L, C, R, LS, RS],
        // MPEG_5_0_D, AAC_5_0.
        120 => &[C, L, R, LS, RS],
        // MPEG_5_1_A, ITU_3_2_1, DVD_12, DVD_17, AudioUnit_5_1.
        121 => &[L, R, C, LFE, LS, RS],
        // MPEG_5_1_B, DVD_20.
        122 => &[L, R, LS, RS, C, LFE],
        // MPEG_5_1_C.
        123 => &[L, C, R, LS, RS, LFE],
        // MPEG_5_1_D, AAC_5_1.
        124 => &[C, L, R, LS, RS, LFE],
        // MPEG_6_1_A, AudioUnit_6_1.
        125 => &[L, R, C, LFE, LS, RS, CS],
        // MPEG_7_1_A.
        126 => &[L, R, C, LFE, LS, RS, LC, RC],
        // MPEG_7_1_B, AAC_7_1.
        127 => &[C, LC, RC, L, R, LS, RS, LFE],
        // MPEG_7_1_C, ITU_3_4_1, AudioUnit_7_1.
        128 => &[L, R, C, LFE, LS, RS, RLS, RRS],
        // Emagic_Default_7_1.
        129 => &[L, R, LS, RS, C, LFE, LC, RC],
        // SMPTE_DTV: ITU 5.1 plus a matrix encoded stereo mix.
        130 => &[L, R, C, LFE, LS, RS, LT, RT],
        // ITU_2_1, DVD_2.
        131 => &[L, R, CS],
        // ITU_2_2, DVD_3.
        132 => &[L, R, LS, RS],
        // DVD_4.
        133 => &[L, R, LFE],
        // DVD_5.
        134 => &[L, R, LFE, CS],
        // DVD_6.
        135 => &[L, R, LFE, LS, RS],
        // DVD_10, DVD_15.
        136 => &[L, R, C, LFE],
        // DVD_11, DVD_16.
        137 => &[L, R, C, LFE, CS],
        // DVD_18.
        138 => &[L, R, LS, RS, LFE],
        // AudioUnit_6_0.
        139 => &[L, R, LS, RS, C, CS],
        // AudioUnit_7_0.
        140 => &[L, R, LS, RS, C, RLS, RRS],
        // AAC_6_0.
        141 => &[C, L, R, LS, RS, CS],
        // AAC_6_1.
        142 => &[C, L, R, LS, RS, CS, LFE],
        // AAC_7_0.
        143 => &[C, L, R, LS, RS, RLS, RRS],
        // AAC_Octagonal.
        144 => &[C, L, R, LS, RS, RLS, RRS, CS],
        // TMH_10_2_std.
        145 => &[L, R, C, VHC, LSD, RSD, LS, RS, VHL, VHR, LW, RW, CSD, CS, LFE, LFE2],
        // TMH_10_2_full: TMH_10_2_std plus Lc Rc HI VI Haptic.
        146 => &[
            L, R, C, VHC, LSD, RSD, LS, RS, VHL, VHR, LW, RW, CSD, CS, LFE, LFE2, LC, RC, HI, VI,
            HAPTIC,
        ],
        _ => return None,
    };

    Some(labels)
}

/// Convert an ordered list of CAF channel labels into the set of channels.
///
/// Labels with a speaker position are `Positioned`. The CAF specification overloads some labels
/// depending on the layout, which is resolved here:
///
///  * `LeftSurround`/`RightSurround` are the "back" speakers of a WAVE layout, unless the layout
///    also has `RearSurroundLeft`/`RearSurroundRight`. Then they are the side speakers.
///  * `LeftTotal`/`RightTotal` (matrix encoded stereo) are the front left/right, unless the layout
///    also has `Left`/`Right`.
///  * `CenterSurroundDirect` is the rear center, unless the layout also has `CenterSurround`.
///
/// Labels without a speaker position are exposed as discrete channels identified by the channel's
/// index. If all channels are positioned, and in ascending order of position, the result is a
/// `Positioned` set; otherwise a `Custom` list preserving the file's channel order.
fn channels_from_labels(labels: &[u32]) -> Channels {
    let has = |label| labels.contains(&label);

    let has_rear_surround =
        has(CHANNEL_LABEL_REAR_SURROUND_LEFT) || has(CHANNEL_LABEL_REAR_SURROUND_RIGHT);
    let has_left_right = has(CHANNEL_LABEL_LEFT) || has(CHANNEL_LABEL_RIGHT);
    let has_center_surround = has(CHANNEL_LABEL_CENTER_SURROUND);

    let mut used = Position::empty();
    let mut out = Vec::with_capacity(labels.len());

    for (index, &label) in labels.iter().enumerate() {
        let position = match label {
            // Standard positioned WAVE channels.
            CHANNEL_LABEL_LEFT | CHANNEL_LABEL_HEADPHONES_LEFT => Some(Position::FRONT_LEFT),
            CHANNEL_LABEL_RIGHT | CHANNEL_LABEL_HEADPHONES_RIGHT => Some(Position::FRONT_RIGHT),
            CHANNEL_LABEL_CENTER => Some(Position::FRONT_CENTER),
            CHANNEL_LABEL_LFE_SCREEN => Some(Position::LFE1),
            CHANNEL_LABEL_LEFT_SURROUND if has_rear_surround => Some(Position::SIDE_LEFT),
            CHANNEL_LABEL_RIGHT_SURROUND if has_rear_surround => Some(Position::SIDE_RIGHT),
            CHANNEL_LABEL_LEFT_SURROUND | CHANNEL_LABEL_REAR_SURROUND_LEFT => {
                Some(Position::REAR_LEFT)
            }
            CHANNEL_LABEL_RIGHT_SURROUND | CHANNEL_LABEL_REAR_SURROUND_RIGHT => {
                Some(Position::REAR_RIGHT)
            }
            CHANNEL_LABEL_LEFT_CENTER => Some(Position::FRONT_LEFT_CENTER),
            CHANNEL_LABEL_RIGHT_CENTER => Some(Position::FRONT_RIGHT_CENTER),
            CHANNEL_LABEL_CENTER_SURROUND => Some(Position::REAR_CENTER),
            CHANNEL_LABEL_CENTER_SURROUND_DIRECT if !has_center_surround => {
                Some(Position::REAR_CENTER)
            }
            CHANNEL_LABEL_LEFT_SURROUND_DIRECT => Some(Position::SIDE_LEFT),
            CHANNEL_LABEL_RIGHT_SURROUND_DIRECT => Some(Position::SIDE_RIGHT),
            CHANNEL_LABEL_TOP_CENTER_SURROUND => Some(Position::TOP_CENTER),
            CHANNEL_LABEL_VERTICAL_HEIGHT_LEFT => Some(Position::TOP_FRONT_LEFT),
            CHANNEL_LABEL_VERTICAL_HEIGHT_CENTER => Some(Position::TOP_FRONT_CENTER),
            CHANNEL_LABEL_VERTICAL_HEIGHT_RIGHT => Some(Position::TOP_FRONT_RIGHT),
            CHANNEL_LABEL_TOP_BACK_LEFT => Some(Position::TOP_REAR_LEFT),
            CHANNEL_LABEL_TOP_BACK_CENTER => Some(Position::TOP_REAR_CENTER),
            CHANNEL_LABEL_TOP_BACK_RIGHT => Some(Position::TOP_REAR_RIGHT),
            // Non-standard positioned channels.
            CHANNEL_LABEL_LEFT_WIDE => Some(Position::FRONT_LEFT_WIDE),
            CHANNEL_LABEL_RIGHT_WIDE => Some(Position::FRONT_RIGHT_WIDE),
            CHANNEL_LABEL_LFE2 => Some(Position::LFE2),
            // Matrix encoded stereo.
            CHANNEL_LABEL_LEFT_TOTAL if !has_left_right => Some(Position::FRONT_LEFT),
            CHANNEL_LABEL_RIGHT_TOTAL if !has_left_right => Some(Position::FRONT_RIGHT),
            // A single mono channel.
            CHANNEL_LABEL_MONO if labels.len() == 1 => Some(Position::FRONT_LEFT),
            _ => None,
        };

        // A speaker position can only be used by one channel.
        if let Some(position) = position.filter(|position| !used.intersects(*position)) {
            used |= position;
            out.push(ChannelLabel::Positioned(position));
            continue;
        }

        out.push(match label {
            // First-order Ambisonic channels.
            CHANNEL_LABEL_AMBISONIC_W => AmbisonicBFormat::W.into(),
            CHANNEL_LABEL_AMBISONIC_X => AmbisonicBFormat::X.into(),
            CHANNEL_LABEL_AMBISONIC_Y => AmbisonicBFormat::Y.into(),
            CHANNEL_LABEL_AMBISONIC_Z => AmbisonicBFormat::Z.into(),
            // Higher-order Ambisonic channels.
            acn @ CHANNEL_LABEL_HOA_ACN_0..=CHANNEL_LABEL_HOA_ACN_65024 => {
                ChannelLabel::Ambisonic((acn - CHANNEL_LABEL_HOA_ACN_0) as u16)
            }
            // Discrete channels with a label-specified index.
            index @ CHANNEL_LABEL_DISCRETE_0..=CHANNEL_LABEL_DISCRETE_65535 => {
                ChannelLabel::Discrete((index - CHANNEL_LABEL_DISCRETE_0) as u16)
            }
            // Any other label (unknown, unused, coordinates, mid/side, X/Y, narration, ...) has no
            // speaker position. The channel is discrete, identified by its index in the file.
            _ => ChannelLabel::Discrete(index.min(usize::from(u16::MAX)) as u16),
        });
    }

    // If every channel is a distinct position and the file order is the ascending order of
    // position bits, then the channels are exactly what a position mask describes.
    let mut mask = Position::empty();
    let mut ascending = true;

    for label in &out {
        match label {
            ChannelLabel::Positioned(position) if position.bits() > mask.bits() => {
                mask |= *position;
            }
            _ => {
                ascending = false;
                break;
            }
        }
    }

    if ascending && !out.is_empty() {
        Channels::Positioned(mask)
    }
    else {
        Channels::Custom(out.into_boxed_slice())
    }
}

#[derive(Debug)]
pub struct ChannelDescription {
    pub channel_label: u32,
    #[allow(dead_code)]
    pub channel_flags: u32,
    #[allow(dead_code)]
    pub coordinates: [f32; 3],
}

impl ChannelDescription {
    pub fn read(reader: &mut MediaSourceStream<'_>) -> Result<Self> {
        Ok(Self {
            channel_label: reader.read_be_u32()?,
            channel_flags: reader.read_be_u32()?,
            coordinates: [reader.read_be_f32()?, reader.read_be_f32()?, reader.read_be_f32()?],
        })
    }
}

pub struct PacketTable {
    pub valid_frames: i64,
    pub priming_frames: i32,
    pub remainder_frames: i32,
    pub packets: Vec<CafPacket>,
}

impl PacketTable {
    pub fn read(
        reader: &mut MediaSourceStream<'_>,
        desc: &Option<AudioDescription>,
        chunk_size: i64,
    ) -> Result<Self> {
        /// The maximum number of packet table entries to preallocate before reading from the
        /// source. Since the source could be malicious and choose a very large number, this
        /// prevents exhausting system memory.
        pub const MAX_TABLE_INITIAL_CAPACITY: usize = 32 * 1024;

        if chunk_size < 24 {
            return invalid_chunk_size_error("Packet Table", chunk_size);
        }

        let desc = desc.as_ref().ok_or_else(|| {
            error!("missing audio description");
            Error::DecodeError("caf: missing audio description")
        })?;

        let total_packets = reader.read_be_i64()?;
        if total_packets < 0 {
            error!("invalid number of packets in the packet table ({total_packets})");
            return decode_error("caf: invalid number of packets in the packet table");
        }

        let valid_frames = reader.read_be_i64()?;
        if valid_frames < 0 {
            error!("invalid number of frames in the packet table ({valid_frames})");
            return decode_error("caf: invalid number of frames in the packet table");
        }

        let priming_frames = reader.read_be_i32()?;
        let remainder_frames = reader.read_be_i32()?;

        let mut packets =
            Vec::with_capacity((total_packets as usize).min(MAX_TABLE_INITIAL_CAPACITY));

        let mut current_frame =
            Timestamp::from(-i64::from(if priming_frames > 0 { priming_frames } else { 0 }));
        let mut packet_offset = 0u64;

        // The packet sizes are untrusted, and may overflow the data offset.
        const OFFSET_OVERFLOW: Error = Error::DecodeError("caf: packet table data size overflow");

        match (desc.bytes_per_packet, desc.frames_per_packet) {
            // Variable bytes per packet, variable number of frames
            (0, 0) => {
                for _ in 0..total_packets {
                    let size = read_variable_length_integer(reader)?;
                    let frames = Duration::from(read_variable_length_integer(reader)?);
                    packets.push(CafPacket {
                        size,
                        frames,
                        start_frame: current_frame,
                        data_offset: packet_offset,
                    });
                    current_frame = current_frame
                        .checked_add(frames)
                        .ok_or(Error::Unsupported("track too long"))?;
                    packet_offset = packet_offset.checked_add(size).ok_or(OFFSET_OVERFLOW)?;
                }
            }
            // Variable bytes per packet, constant number of frames
            (0, frames_per_packet) => {
                for _ in 0..total_packets {
                    let size = read_variable_length_integer(reader)?;
                    let frames = Duration::from(frames_per_packet);
                    packets.push(CafPacket {
                        size,
                        frames,
                        start_frame: current_frame,
                        data_offset: packet_offset,
                    });
                    current_frame = current_frame
                        .checked_add(frames)
                        .ok_or(Error::Unsupported("track too long"))?;
                    packet_offset = packet_offset.checked_add(size).ok_or(OFFSET_OVERFLOW)?;
                }
            }
            // Constant bytes per packet, variable number of frames
            (bytes_per_packet, 0) => {
                for _ in 0..total_packets {
                    let size = bytes_per_packet as u64;
                    let frames = Duration::from(read_variable_length_integer(reader)?);
                    packets.push(CafPacket {
                        size,
                        frames,
                        start_frame: current_frame,
                        data_offset: packet_offset,
                    });
                    current_frame = current_frame
                        .checked_add(frames)
                        .ok_or(Error::Unsupported("track too long"))?;
                    packet_offset = packet_offset.checked_add(size).ok_or(OFFSET_OVERFLOW)?;
                }
            }
            // Constant bit rate format
            (_, _) => {
                if total_packets > 0 {
                    error!(
                        "unexpected packet table for constant bit rate ({total_packets} packets)"
                    );
                    return decode_error(
                        "caf: unexpected packet table for constant bit rate format",
                    );
                }
            }
        }

        Ok(Self { valid_frames, priming_frames, remainder_frames, packets })
    }
}

impl fmt::Debug for PacketTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PacketTable")?;
        write!(
            f,
            "{{ valid_frames: {}, priming_frames: {}, remainder_frames: {}, packet count: {}}}",
            self.valid_frames,
            self.priming_frames,
            self.remainder_frames,
            self.packets.len()
        )
    }
}

#[derive(Debug)]
pub struct CafPacket {
    // The packet's offset in bytes from the start of the data
    pub data_offset: u64,
    // The index of the first frame in the packet
    pub start_frame: Timestamp,
    // The number of frames in the packet
    // For files with a constant frames per packet this value will match frames_per_packet
    pub frames: Duration,
    // The size in bytes of the packet
    // For constant bit-rate files this value will match bytes_per_packet
    pub size: u64,
}

fn invalid_chunk_size_error<T>(chunk_type: &str, chunk_size: i64) -> Result<T> {
    error!("invalid {chunk_type} chunk size ({chunk_size})");
    decode_error("caf: invalid chunk size")
}

/// The maximum size of an `info` chunk that will be read into memory. Larger chunks are skipped.
const MAX_INFO_CHUNK_SIZE: usize = 4 * 1024 * 1024;

/// Reads the `info` chunk: a `UInt32` count of entries followed by that many key/value pairs of
/// NUL-terminated UTF-8 strings. See Apple's CAF specification, "Information Chunk".
///
/// The entry count is not trusted. Entries are read until the chunk data is exhausted, or the
/// stated count is reached. A malformed entry (e.g., a missing terminator) ends the list: the
/// entries read up to that point are returned since tags are optional.
/// A marker (`CAFMarker`).
#[derive(Clone, Debug)]
pub struct Marker {
    /// The position of the marker in frames from the start of the audio data.
    pub frame_position: f64,
    /// The ID of the string that describes the marker (`mMarkerID`).
    pub id: u32,
}

/// A region (`CAFRegion`).
#[derive(Clone, Debug)]
pub struct Region {
    pub markers: Vec<Marker>,
}

/// The size of a `CAFMarker`: type, frame position, marker ID, SMPTE time (4 bytes, and a 4 byte
/// sub-frame offset), and channel.
const MARKER_SIZE: usize = 28;

/// Reads the body of a chunk that is small enough to be read entirely. Returns `None` if the chunk
/// was too large, and was skipped.
fn read_small_chunk(
    reader: &mut MediaSourceStream<'_>,
    chunk_size: i64,
    chunk_type: &[u8; 4],
) -> Result<Option<Box<[u8]>>> {
    let name = str::from_utf8(chunk_type).unwrap_or("????");

    let Ok(chunk_size) = usize::try_from(chunk_size)
    else {
        return invalid_chunk_size_error(name, chunk_size);
    };

    if chunk_size > MAX_INFO_CHUNK_SIZE {
        warn!("skipping excessively large '{name}' chunk ({chunk_size} bytes)");
        reader.ignore_bytes(chunk_size as u64)?;
        return Ok(None);
    }

    Ok(Some(reader.read_boxed_slice_exact(chunk_size)?))
}

fn be_u32(data: &[u8], pos: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(pos..pos.checked_add(4)?)?.try_into().ok()?))
}

fn be_i64(data: &[u8], pos: usize) -> Option<i64> {
    Some(i64::from_be_bytes(data.get(pos..pos.checked_add(8)?)?.try_into().ok()?))
}

/// Parse the `CAFMarker` at `pos`.
fn parse_marker(data: &[u8], pos: usize) -> Option<Marker> {
    data.get(pos..pos.checked_add(MARKER_SIZE)?)?;

    Some(Marker {
        frame_position: f64::from_bits(be_i64(data, pos + 4)? as u64),
        id: be_u32(data, pos + 12)?,
    })
}

/// Parse a Marker chunk data section. Markers beyond the end of the chunk are dropped.
fn parse_marker_chunk(data: &[u8]) -> Vec<Marker> {
    let mut markers = Vec::new();

    // The SMPTE time type is at offset 0, and is not used.
    let Some(count) = be_u32(data, 4)
    else {
        return markers;
    };

    // The count is untrusted, but the number of markers is bounded by the chunk size.
    for i in 0..(count as usize).min(data.len() / MARKER_SIZE) {
        match parse_marker(data, 8 + i * MARKER_SIZE) {
            Some(marker) => markers.push(marker),
            None => break,
        }
    }

    markers
}

/// Parse a Region chunk data section. Regions beyond the end of the chunk are dropped.
fn parse_region_chunk(data: &[u8]) -> Vec<Region> {
    let mut regions = Vec::new();

    // The SMPTE time type is at offset 0, and is not used.
    let Some(count) = be_u32(data, 4)
    else {
        return regions;
    };

    let mut pos = 8;

    // A region is at least 12 bytes.
    for _ in 0..(count as usize).min(data.len() / 12) {
        // The region ID (`mRegionID`) and flags (`mFlags`) precede the number of markers.
        let Some(num_markers) = be_u32(data, pos + 8)
        else {
            break;
        };

        pos += 12;

        let mut markers = Vec::new();

        for _ in 0..(num_markers as usize).min(data.len() / MARKER_SIZE) {
            match parse_marker(data, pos) {
                Some(marker) => markers.push(marker),
                None => return regions,
            }
            pos += MARKER_SIZE;
        }

        regions.push(Region { markers });
    }

    regions
}

/// Parse a Strings chunk data section: a number of entries, an (ID, byte offset) table, and the
/// NUL-terminated UTF-8 strings the offsets (from the end of the table) refer to.
fn parse_strings_chunk(data: &[u8]) -> Vec<(u32, String)> {
    let mut strings = Vec::new();

    let Some(count) = be_u32(data, 0)
    else {
        return strings;
    };

    // An entry is a 4 byte ID and 8 byte offset.
    let count = (count as usize).min(data.len() / 12);
    let base = 4 + count * 12;
    let mut total = 0;

    for i in 0..count {
        let (Some(id), Some(offset)) = (be_u32(data, 4 + i * 12), be_i64(data, 8 + i * 12))
        else {
            break;
        };

        let Some(start) = usize::try_from(offset).ok().and_then(|offset| base.checked_add(offset))
        else {
            continue;
        };

        let mut pos = start;

        if start <= data.len() {
            if let Ok(string) = read_c_string(data, &mut pos) {
                // Offsets may overlap. Bound the memory used by the chunk size.
                total += string.len();

                if total > data.len() {
                    break;
                }

                strings.push((id, string));
            }
        }
    }

    strings
}

fn read_info_chunk(
    reader: &mut MediaSourceStream<'_>,
    chunk_size: i64,
) -> Result<Vec<(String, String)>> {
    let Ok(chunk_size) = usize::try_from(chunk_size)
    else {
        return invalid_chunk_size_error("Information", chunk_size);
    };

    if chunk_size < 4 {
        return invalid_chunk_size_error("Information", chunk_size as i64);
    }

    if chunk_size > MAX_INFO_CHUNK_SIZE {
        warn!("skipping excessively large information chunk ({chunk_size} bytes)");
        reader.ignore_bytes(chunk_size as u64)?;
        return Ok(Vec::new());
    }

    let data = reader.read_boxed_slice_exact(chunk_size)?;

    let num_entries = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);

    // Every entry is at least two bytes (a key and a value terminator). Therefore, the number of
    // entries that can be in the chunk is bounded by the chunk size, regardless of the stated count.
    let max_entries = (data.len() - 4) / 2;

    if num_entries as usize > max_entries {
        warn!("information chunk entry count ({num_entries}) exceeds the chunk size");
    }

    let mut pos = 4;
    let mut entries = Vec::with_capacity((num_entries as usize).min(max_entries));

    for _ in 0..num_entries {
        let Ok(key) = read_c_string(&data, &mut pos)
        else {
            break;
        };
        let Ok(value) = read_c_string(&data, &mut pos)
        else {
            break;
        };
        entries.push((key, value));
    }

    Ok(entries)
}

/// Reads a single NUL-terminated UTF-8 string from `data`, starting at `*pos`, advancing `*pos`
/// past the terminating NUL. Invalid UTF-8 is replaced lossily rather than failing the whole
/// chunk, since a single malformed entry shouldn't prevent reading the rest of the tags.
fn read_c_string(data: &[u8], pos: &mut usize) -> Result<String> {
    let start = *pos;

    while *pos < data.len() && data[*pos] != 0 {
        *pos += 1;
    }

    if *pos >= data.len() {
        return decode_error("caf: info chunk entry missing NUL terminator");
    }

    let s = String::from_utf8_lossy(&data[start..*pos]).into_owned();
    *pos += 1;

    Ok(s)
}

fn read_variable_length_integer(reader: &mut MediaSourceStream<'_>) -> Result<u64> {
    match read_base128_be_u64(9, || reader.read_byte())? {
        Some((value, _)) => Ok(value),
        None => decode_error("caf: unterminated variable-length integer"),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn variable_length_integer_test(bytes: &[u8], expected: u64) -> Result<()> {
        let cursor = Cursor::new(Vec::from(bytes));
        let mut source = MediaSourceStream::new(Box::new(cursor), Default::default());

        assert_eq!(read_variable_length_integer(&mut source)?, expected);

        Ok(())
    }

    #[test]
    fn variable_length_integers() -> Result<()> {
        variable_length_integer_test(&[0x01], 1)?;
        variable_length_integer_test(&[0x11], 17)?;
        variable_length_integer_test(&[0x7f], 127)?;
        variable_length_integer_test(&[0x81, 0x00], 128)?;
        variable_length_integer_test(&[0x81, 0x02], 130)?;
        variable_length_integer_test(&[0x82, 0x01], 257)?;
        variable_length_integer_test(&[0xff, 0x7f], 16383)?;
        variable_length_integer_test(&[0x81, 0x80, 0x00], 16384)?;
        Ok(())
    }

    #[test]
    fn unterminated_variable_length_integer() {
        let cursor = Cursor::new(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        let mut source = MediaSourceStream::new(Box::new(cursor), Default::default());

        assert!(read_variable_length_integer(&mut source).is_err());
    }

    fn open_stream(data: Vec<u8>) -> MediaSourceStream<'static> {
        MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default())
    }

    #[test]
    fn info_chunk_with_huge_entry_count_does_not_preallocate() {
        // A chunk of 4 + 4 bytes that claims 0xFFFFFFFF entries. Preallocating the stated number of
        // entries would exhaust memory (an allocation of 200+ GB).
        let mut data = Vec::new();
        data.extend_from_slice(&0xffff_ffffu32.to_be_bytes());
        data.extend_from_slice(b"k\0v\0");

        let mut stream = open_stream(data);
        let entries = read_info_chunk(&mut stream, 8).unwrap();

        // The entries that are actually present are read.
        assert_eq!(entries, vec![("k".to_string(), "v".to_string())]);
        // The whole chunk was consumed.
        assert_eq!(stream.pos(), 8);
    }

    #[test]
    fn info_chunk_stops_at_malformed_entry() {
        let mut data = Vec::new();
        data.extend_from_slice(&3u32.to_be_bytes());
        data.extend_from_slice(b"a\0b\0c\0d\0e");

        let mut stream = open_stream(data);
        let entries = read_info_chunk(&mut stream, 13).unwrap();

        assert_eq!(
            entries,
            vec![("a".to_string(), "b".to_string()), ("c".to_string(), "d".to_string())]
        );
    }

    #[test]
    fn info_chunk_too_small_is_an_error() {
        let mut stream = open_stream(vec![0; 3]);
        assert!(read_info_chunk(&mut stream, 3).is_err());
    }

    #[test]
    fn channel_layout_description_count_is_bounded_by_chunk_size() {
        let mut data = Vec::new();
        data.extend_from_slice(&0u32.to_be_bytes()); // Layout tag: use channel descriptions.
        data.extend_from_slice(&0u32.to_be_bytes()); // Bitmap.
        data.extend_from_slice(&0x7fff_ffffu32.to_be_bytes()); // Number of descriptions.

        let mut stream = open_stream(data);
        assert!(ChannelLayout::read(&mut stream, 12).is_err());
    }

    #[test]
    fn packet_table_data_offset_overflow_is_an_error() {
        let desc = AudioDescription {
            sample_rate: 44100.0,
            format_id: AudioDescriptionFormatId::MPEG4AAC,
            bytes_per_packet: 0,
            frames_per_packet: 1024,
            channels_per_frame: 2,
            bits_per_channel: 0,
        };

        // Three packets of size 2^63 - 1 (the largest variable-length integer is 9 bytes: 8 bytes
        // of 0xff, then 0x7f). The sum of the sizes overflows 64 bits.
        let huge = [0xffu8, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f];

        let mut data = Vec::new();
        data.extend_from_slice(&3i64.to_be_bytes()); // Packets.
        data.extend_from_slice(&3072i64.to_be_bytes()); // Valid frames.
        data.extend_from_slice(&0i32.to_be_bytes()); // Priming frames.
        data.extend_from_slice(&0i32.to_be_bytes()); // Remainder frames.
        data.extend_from_slice(&huge);
        data.extend_from_slice(&huge);
        data.extend_from_slice(&huge);

        let len = data.len() as i64;
        let mut stream = open_stream(data);
        assert!(PacketTable::read(&mut stream, &Some(desc), len).is_err());
    }

    fn marker_bytes(frame: f64, id: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&frame.to_be_bytes());
        out.extend_from_slice(&id.to_be_bytes());
        out.extend_from_slice(&[0xff; 8]);
        out.extend_from_slice(&0u32.to_be_bytes());
        out
    }

    #[test]
    fn markers_regions_and_strings() {
        let mut mark = vec![0u8; 4];
        mark.extend_from_slice(&2u32.to_be_bytes());
        mark.extend(marker_bytes(100.0, 1));
        mark.extend(marker_bytes(2500.0, 2));
        let markers = parse_marker_chunk(&mark);
        assert_eq!(markers.len(), 2);
        assert_eq!((markers[1].frame_position, markers[1].id), (2500.0, 2));

        let mut regn = vec![0u8; 4];
        regn.extend_from_slice(&1u32.to_be_bytes());
        regn.extend_from_slice(&7u32.to_be_bytes());
        regn.extend_from_slice(&0u32.to_be_bytes());
        regn.extend_from_slice(&2u32.to_be_bytes());
        regn.extend(marker_bytes(10.0, 3));
        regn.extend(marker_bytes(20.0, 3));
        let regions = parse_region_chunk(&regn);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].markers.len(), 2);

        // Two strings: "ab" at offset 0, and "c" at offset 3.
        let mut strg = 2u32.to_be_bytes().to_vec();
        strg.extend_from_slice(&1u32.to_be_bytes());
        strg.extend_from_slice(&0i64.to_be_bytes());
        strg.extend_from_slice(&2u32.to_be_bytes());
        strg.extend_from_slice(&3i64.to_be_bytes());
        strg.extend_from_slice(b"ab\0c\0");
        assert_eq!(parse_strings_chunk(&strg), [(1, "ab".to_string()), (2, "c".to_string())]);
    }

    #[test]
    fn marker_and_string_counts_are_bounded_by_the_chunk() {
        // Claims 2^32 - 1 entries.
        let data = [0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff];
        assert!(parse_marker_chunk(&data).is_empty());
        assert!(parse_region_chunk(&data).is_empty());
        assert!(parse_strings_chunk(&data[4..]).is_empty());

        // Truncated marker.
        let mut mark = vec![0u8; 4];
        mark.extend_from_slice(&1u32.to_be_bytes());
        mark.extend_from_slice(&[0u8; 20]);
        assert!(parse_marker_chunk(&mark).is_empty());
    }
}
