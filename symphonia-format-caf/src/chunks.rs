// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use log::{debug, error, info, warn};
use std::{convert::TryFrom, fmt, mem::size_of, str};
use symphonia_core::{
    audio::{AmbisonicBFormat, ChannelLabel, Channels, Position, layouts},
    codecs::audio::{AudioCodecId, well_known::*},
    errors::{Error, Result, decode_error, unsupported_error},
    io::{MediaSourceStream, ReadBytes},
    units::{Duration, Timestamp},
};

// CAF audio channel layouts.
const LAYOUT_TAG_USE_CHANNEL_DESCRIPTIONS: u32 = 0;
const LAYOUT_TAG_USE_CHANNEL_BITMAP: u32 = 1 << 16;
// Layout tags from the CAF spec that match the first N channels of a standard layout
const LAYOUT_TAG_MONO: u32 = (100 << 16) | 1;
const LAYOUT_TAG_STEREO: u32 = (101 << 16) | 2;
const LAYOUT_TAG_STEREO_HEADPHONES: u32 = (102 << 16) | 2;
const LAYOUT_TAG_MPEG_3_0_A: u32 = (113 << 16) | 3; // L R C
const LAYOUT_TAG_MPEG_5_1_A: u32 = (121 << 16) | 6; // L R C LFE Ls Rs
const LAYOUT_TAG_MPEG_7_1_A: u32 = (126 << 16) | 8; // L R C LFE Ls Rs Lc Rc
const LAYOUT_TAG_DVD_10: u32 = (136 << 16) | 4; // L R C LFE

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
const CHANNEL_LABEL_LEFT_WIDE: u32 = 35;
const CHANNEL_LABEL_RIGHT_WIDE: u32 = 36;
const CHANNEL_LABEL_LFE2: u32 = 37;
const CHANNEL_LABEL_AMBISONIC_W: u32 = 200;
const CHANNEL_LABEL_AMBISONIC_X: u32 = 201;
const CHANNEL_LABEL_AMBISONIC_Y: u32 = 202;
const CHANNEL_LABEL_AMBISONIC_Z: u32 = 203;
const CHANNEL_LABEL_DISCRETE_0: u32 = (1 << 16) | 0;
const CHANNEL_LABEL_DISCRETE_65535: u32 = (1 << 16) | 65535;
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
    Free,
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
            b"free" => {
                if chunk_size < 0 {
                    return invalid_chunk_size_error("Free", chunk_size);
                }
                reader.ignore_bytes(chunk_size as u64)?;
                Chunk::Free
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

    pub fn codec_id(&self) -> Result<AudioCodecId> {
        use AudioDescriptionFormatId::*;

        let result = match &self.format_id {
            LinearPCM { floating_point, little_endian } => {
                if *floating_point {
                    match (self.bits_per_channel, *little_endian) {
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
                    match (self.bits_per_channel, *little_endian) {
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
            MPEG4AAC => CODEC_ID_AAC,
            ULaw => CODEC_ID_PCM_MULAW,
            ALaw => CODEC_ID_PCM_ALAW,
            MPEGLayer1 => CODEC_ID_MP1,
            MPEGLayer2 => CODEC_ID_MP2,
            MPEGLayer3 => CODEC_ID_MP3,
            AppleLossless => CODEC_ID_ALAC,
            Flac => CODEC_ID_FLAC,
            Opus => CODEC_ID_OPUS,
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
    LinearPCM { floating_point: bool, little_endian: bool },
    AppleIMA4,
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

    pub fn channels(&self) -> Option<Channels> {
        let channels = match self.channel_layout {
            // Use channel descriptions
            LAYOUT_TAG_USE_CHANNEL_DESCRIPTIONS => {
                let mut labels = Vec::new();

                for channel in self.channel_descriptions.iter() {
                    let label = match channel.channel_label {
                        // Standard positioned WAVE channels.
                        CHANNEL_LABEL_LEFT => Position::FRONT_LEFT.into(),
                        CHANNEL_LABEL_RIGHT => Position::FRONT_RIGHT.into(),
                        CHANNEL_LABEL_CENTER => Position::FRONT_CENTER.into(),
                        CHANNEL_LABEL_LFE_SCREEN => Position::LFE1.into(),
                        CHANNEL_LABEL_LEFT_SURROUND => Position::REAR_LEFT.into(),
                        CHANNEL_LABEL_RIGHT_SURROUND => Position::REAR_RIGHT.into(),
                        CHANNEL_LABEL_LEFT_CENTER => Position::FRONT_LEFT_CENTER.into(),
                        CHANNEL_LABEL_RIGHT_CENTER => Position::FRONT_RIGHT_CENTER.into(),
                        CHANNEL_LABEL_CENTER_SURROUND => Position::REAR_CENTER.into(),
                        CHANNEL_LABEL_LEFT_SURROUND_DIRECT => Position::SIDE_LEFT.into(),
                        CHANNEL_LABEL_RIGHT_SURROUND_DIRECT => Position::SIDE_RIGHT.into(),
                        CHANNEL_LABEL_TOP_CENTER_SURROUND => Position::TOP_CENTER.into(),
                        CHANNEL_LABEL_VERTICAL_HEIGHT_LEFT => Position::TOP_FRONT_LEFT.into(),
                        CHANNEL_LABEL_VERTICAL_HEIGHT_CENTER => Position::TOP_FRONT_CENTER.into(),
                        CHANNEL_LABEL_VERTICAL_HEIGHT_RIGHT => Position::TOP_FRONT_RIGHT.into(),
                        CHANNEL_LABEL_TOP_BACK_LEFT => Position::TOP_REAR_LEFT.into(),
                        CHANNEL_LABEL_TOP_BACK_CENTER => Position::TOP_REAR_CENTER.into(),
                        CHANNEL_LABEL_TOP_BACK_RIGHT => Position::TOP_REAR_RIGHT.into(),
                        // Non-standard positioned channels.
                        CHANNEL_LABEL_LEFT_WIDE => Position::FRONT_LEFT_WIDE.into(),
                        CHANNEL_LABEL_RIGHT_WIDE => Position::FRONT_RIGHT_WIDE.into(),
                        CHANNEL_LABEL_LFE2 => Position::LFE2.into(),
                        // First-order Ambisonic channels.
                        CHANNEL_LABEL_AMBISONIC_W => AmbisonicBFormat::W.into(),
                        CHANNEL_LABEL_AMBISONIC_X => AmbisonicBFormat::X.into(),
                        CHANNEL_LABEL_AMBISONIC_Y => AmbisonicBFormat::Y.into(),
                        CHANNEL_LABEL_AMBISONIC_Z => AmbisonicBFormat::Z.into(),
                        // Discrete channels.
                        index @ CHANNEL_LABEL_DISCRETE_0..=CHANNEL_LABEL_DISCRETE_65535 => {
                            ChannelLabel::Discrete((index - CHANNEL_LABEL_DISCRETE_0) as u16)
                        }
                        // Higher-order Ambisonic channels.
                        acn @ CHANNEL_LABEL_HOA_ACN_0..=CHANNEL_LABEL_HOA_ACN_65024 => {
                            ChannelLabel::Ambisonic((acn - CHANNEL_LABEL_HOA_ACN_0) as u16)
                        }
                        unsupported => {
                            warn!("unsupported channel label: {unsupported}");
                            return None;
                        }
                    };

                    labels.push(label);
                }

                Channels::Custom(labels.into_boxed_slice())
            }
            // Use the channel bitmap
            LAYOUT_TAG_USE_CHANNEL_BITMAP => {
                // The CAF channel bitmap is identical to a WAVE channel mask.
                let positions = match Position::from_wave_channel_mask(self.channel_bitmap) {
                    Some(positions) => positions,
                    None => {
                        warn!("unsupported channel bitmap: {}", self.channel_bitmap);
                        return None;
                    }
                };

                Channels::Positioned(positions)
            }
            // Layout tags which have channel roles that match the standard channel layout
            LAYOUT_TAG_MONO => layouts::CHANNEL_LAYOUT_MONO,
            LAYOUT_TAG_STEREO | LAYOUT_TAG_STEREO_HEADPHONES => layouts::CHANNEL_LAYOUT_STEREO,
            LAYOUT_TAG_MPEG_3_0_A => layouts::CHANNEL_LAYOUT_MPEG_3P0_A,
            LAYOUT_TAG_MPEG_5_1_A => layouts::CHANNEL_LAYOUT_MPEG_5P1_A,
            LAYOUT_TAG_MPEG_7_1_A => layouts::CHANNEL_LAYOUT_MPEG_7P1_A,
            LAYOUT_TAG_DVD_10 => layouts::CHANNEL_LAYOUT_3P1,
            unsupported => {
                debug!("unsupported channel layout: {unsupported}");
                return None;
            }
        };

        Some(channels)
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
            Error::DecodeError("caf: missing audio descripton")
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
    let mut result = 0;

    for _ in 0..9 {
        let byte = reader.read_byte()?;

        result |= (byte & 0x7f) as u64;

        if byte & 0x80 == 0 {
            return Ok(result);
        }

        result <<= 7;
    }

    decode_error("caf: unterminated variable-length integer")
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
}
