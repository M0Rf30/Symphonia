// DSF (DSD Stream File) Format Parser
//
// Implements the Sony "DSF File Format Specification" v1.01 (2012-01-24):
//
//   "DSD " header chunk (28 bytes) | "fmt " chunk (52 bytes) | "data" chunk | ID3v2 tag
//
// All numeric fields are little-endian. The sound data is stored in blocks of
// `block_size_per_channel` bytes per channel; the blocks of the channels alternate (channel 0
// block, channel 1 block, ...) and the last block of each channel is zero-padded.
use crate::{CODEC_TYPE_DSD, MAX_DSD_CHANNELS, read_up_to};
use log::warn;
use std::io::{Seek, SeekFrom};
use std::num::NonZero;
use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioCodecParameters, BitOrder, ChannelDataLayout};
use symphonia_core::common::FourCc;
use symphonia_core::errors::{
    Error, Result, SeekErrorKind, decode_error, seek_error, unsupported_error,
};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::io::*;
use symphonia_core::meta::{ChapterGroup, Metadata, MetadataLog, MetadataSideData};
use symphonia_core::support_format;
use symphonia_core::units::{Duration, TimeBase, Timestamp};
use symphonia_metadata::embedded::riff::read_riff_id3_chunk;

const DSF_MAGIC: [u8; 4] = *b"DSD ";
const DSF_FMT_MAGIC: [u8; 4] = *b"fmt ";
const DSF_DATA_MAGIC: [u8; 4] = *b"data";

/// The size of the "DSD " header chunk, including the chunk ID and size fields.
const DSF_HEADER_LEN: u64 = 28;
/// The minimum size of the "fmt " chunk, including the chunk ID and size fields.
const DSF_FMT_MIN_LEN: u64 = 52;
/// The size of the chunk ID and chunk size fields of the "data" chunk.
const DSF_DATA_HEADER_LEN: u64 = 12;
/// The largest per-channel block size accepted. The specification mandates 4096.
const DSF_MAX_BLOCK_SIZE: u32 = 1 << 20;

const DSF_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"DSF\0")),
    short_name: "dsf",
    long_name: "DSD Stream File",
};

struct DsfHeader {
    metadata_pointer: u64,
}
impl DsfHeader {
    fn read<B: ReadBytes>(reader: &mut B) -> Result<Self> {
        if reader.read_quad_bytes()? != DSF_MAGIC {
            return decode_error("dsf: bad magic");
        }
        let chunk_size = reader.read_u64()?;
        if chunk_size != DSF_HEADER_LEN {
            return decode_error("dsf: bad header size");
        }
        let _file_size = reader.read_u64()?;
        let metadata_pointer = reader.read_u64()?;
        Ok(DsfHeader { metadata_pointer })
    }
}

struct DsfFormatChunk {
    format_version: u32,
    format_id: u32,
    channel_type: u32,
    channel_count: u32,
    sample_rate: u32,
    bits_per_sample: u32,
    sample_count: u64,
    block_size_per_channel: u32,
}
impl DsfFormatChunk {
    fn read<B: ReadBytes>(reader: &mut B) -> Result<Self> {
        if reader.read_quad_bytes()? != DSF_FMT_MAGIC {
            return decode_error("dsf: bad fmt");
        }
        let size = reader.read_u64()?;
        if size < DSF_FMT_MIN_LEN {
            return decode_error("dsf: bad fmt size");
        }
        let fmt = DsfFormatChunk {
            format_version: reader.read_u32()?,
            format_id: reader.read_u32()?,
            channel_type: reader.read_u32()?,
            channel_count: reader.read_u32()?,
            sample_rate: reader.read_u32()?,
            bits_per_sample: reader.read_u32()?,
            sample_count: reader.read_u64()?,
            block_size_per_channel: reader.read_u32()?,
        };
        let _reserved = reader.read_u32()?;
        // The specification fixes the chunk size at 52 bytes. Skip any additional bytes a writer
        // may have appended so the data chunk is found at the position the size field claims.
        reader.ignore_bytes(size - DSF_FMT_MIN_LEN)?;
        Ok(fmt)
    }
    fn validate(&self) -> Result<()> {
        if self.format_version != 1 {
            return unsupported_error("dsf: bad version");
        }
        if self.format_id != 0 {
            return unsupported_error("dsf: bad format id");
        }
        if self.channel_count < 1 || self.channel_count > MAX_DSD_CHANNELS {
            return decode_error("dsf: bad channels");
        }
        if self.sample_rate == 0 {
            return decode_error("dsf: bad sample rate");
        }
        if self.block_size_per_channel == 0 || self.block_size_per_channel > DSF_MAX_BLOCK_SIZE {
            return decode_error("dsf: bad block size");
        }
        if self.bits_per_sample != 1 && self.bits_per_sample != 8 {
            return decode_error("dsf: bad bps");
        }
        Ok(())
    }
}

/// Map the DSF "channel type" (DSF specification, annotation 1) to a channel layout.
fn channels_for(channel_type: u32, channel_count: u32) -> Channels {
    let pos = match channel_type {
        1 => Position::FRONT_CENTER,
        2 => Position::FRONT_LEFT | Position::FRONT_RIGHT,
        3 => Position::FRONT_LEFT | Position::FRONT_RIGHT | Position::FRONT_CENTER,
        4 => {
            Position::FRONT_LEFT
                | Position::FRONT_RIGHT
                | Position::REAR_LEFT
                | Position::REAR_RIGHT
        }
        5 => Position::FRONT_LEFT | Position::FRONT_RIGHT | Position::FRONT_CENTER | Position::LFE1,
        6 => {
            Position::FRONT_LEFT
                | Position::FRONT_RIGHT
                | Position::FRONT_CENTER
                | Position::REAR_LEFT
                | Position::REAR_RIGHT
        }
        7 => {
            Position::FRONT_LEFT
                | Position::FRONT_RIGHT
                | Position::FRONT_CENTER
                | Position::LFE1
                | Position::REAR_LEFT
                | Position::REAR_RIGHT
        }
        _ => return Channels::Discrete(channel_count as u16),
    };
    if pos.bits().count_ones() != channel_count {
        return Channels::Discrete(channel_count as u16);
    }
    Channels::Positioned(pos)
}

/// The number of complete bytes of sound data per channel in `avail` bytes of block-interleaved
/// data.
///
/// The data of one block group is `channels` blocks of `block` bytes, one per channel. A trailing,
/// truncated block group yields only the bytes that every channel has.
fn bytes_per_channel_in(avail: u64, block: u64, channels: u64) -> u64 {
    let group = block * channels;
    let full = avail / group;
    let tail = (avail % group).saturating_sub((channels - 1) * block).min(block);
    full * block + tail
}

pub struct DsfReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    chapters: Option<ChapterGroup>,
    data_start_pos: u64,
    block_size_per_channel: u64,
    channel_count: u64,
    /// The index of the next DSD sample (per channel) to be returned.
    next_frame: u64,
    /// The total number of DSD samples (per channel) that can be read.
    num_frames: u64,
}

impl<'s> DsfReader<'s> {
    pub fn try_new(mut source: MediaSourceStream<'s>, _options: FormatOptions) -> Result<Self> {
        let header = DsfHeader::read(&mut source)?;
        let fmt = DsfFormatChunk::read(&mut source)?;
        fmt.validate()?;

        if source.read_quad_bytes()? != DSF_DATA_MAGIC {
            return decode_error("dsf: bad data");
        }
        let chunk_size = source.read_u64()?;
        if chunk_size < DSF_DATA_HEADER_LEN {
            return decode_error("dsf: data too small");
        }
        let data_start_pos = source.pos();
        let mut data_size = chunk_size - DSF_DATA_HEADER_LEN;

        // Never trust the size of the data chunk beyond the end of the file or the metadata chunk.
        let byte_len = source.byte_len();
        if let Some(len) = byte_len {
            data_size = data_size.min(len.saturating_sub(data_start_pos));
        }
        let metadata_pointer = Some(header.metadata_pointer)
            .filter(|&p| p >= data_start_pos && byte_len.is_none_or(|len| p < len));
        if let Some(pointer) = metadata_pointer.filter(|&p| p > 0) {
            data_size = data_size.min(pointer - data_start_pos);
        }

        let block = u64::from(fmt.block_size_per_channel);
        let channels = u64::from(fmt.channel_count);
        let available_frames = bytes_per_channel_in(data_size, block, channels).saturating_mul(8);
        // A sample count of zero is written by streaming writers that do not know the length.
        let num_frames = match fmt.sample_count {
            0 => available_frames,
            n => n.min(available_frames),
        };

        let mut metadata_log = MetadataLog::default();
        let mut chapters = None;
        if let Some(pointer) = metadata_pointer.filter(|&p| p > 0) {
            if source.is_seekable() {
                source.seek(SeekFrom::Start(pointer))?;
                let mut side_data = Vec::<MetadataSideData>::new();
                match read_riff_id3_chunk(&mut source, &mut side_data) {
                    Ok(rev) => metadata_log.push(rev),
                    Err(e) => warn!("dsf: failed to read ID3v2 metadata: {}", e),
                }
                for side in side_data {
                    if let MetadataSideData::Chapters(group) = side {
                        chapters = Some(group);
                    }
                }
                source.seek(SeekFrom::Start(data_start_pos))?;
            }
        }

        let bit_order =
            if fmt.bits_per_sample == 8 { BitOrder::MsbFirst } else { BitOrder::LsbFirst };
        let channels_layout = channels_for(fmt.channel_type, fmt.channel_count);
        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_TYPE_DSD)
            .with_sample_rate(fmt.sample_rate)
            .with_bits_per_sample(1)
            .with_channels(channels_layout)
            .with_channel_data_layout(ChannelDataLayout::Planar)
            .with_bit_order(bit_order);
        let Some(rate) = NonZero::new(fmt.sample_rate)
        else {
            return decode_error("dsf: bad sample rate");
        };
        let tb = TimeBase::new(NonZero::<u32>::MIN, rate);
        params.with_max_frames_per_packet(block * 8).with_frames_per_block(block * 8);
        let mut track = Track::new(0);
        track.time_base = Some(tb);
        track.num_frames = Some(num_frames);
        track.duration = Some(Duration::new(num_frames));
        track.with_codec_params(CodecParameters::Audio(params));

        let mut media_info = MediaInfo::new();
        media_info.time_base = Some(tb);
        media_info.duration = Some(Duration::new(num_frames));

        Ok(DsfReader {
            reader: source,
            media_info,
            tracks: vec![track],
            metadata: metadata_log,
            chapters,
            data_start_pos,
            block_size_per_channel: block,
            channel_count: channels,
            next_frame: 0,
            num_frames,
        })
    }
}

impl Scoreable for DsfReader<'_> {
    fn score(mut mss: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        Ok(match mss.read_quad_bytes() {
            Ok(b) if b == DSF_MAGIC => Score::Supported(255),
            _ => Score::Unsupported,
        })
    }
}

impl ProbeableFormat<'_> for DsfReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(DsfReader::try_new(mss, opts)?))
    }
    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(DSF_FORMAT_INFO, &["dsf"], &["audio/dsd"], &[b"DSD "])]
    }
}

impl FormatReader for DsfReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &DSF_FORMAT_INFO
    }
    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }
    fn next_packet(&mut self) -> Result<Option<Packet>> {
        let block = self.block_size_per_channel;
        let channels = self.channel_count;
        if self.next_frame >= self.num_frames {
            return Ok(None);
        }
        let mut frames = (self.num_frames - self.next_frame).min(block * 8);
        let mut bytes_per_channel = frames.div_ceil(8);

        // A full block group is read as is. For the final, partially filled group only the bytes
        // up to the last needed byte of the last channel are read; the rest is padding.
        let to_read = (channels - 1) * block + bytes_per_channel;
        let mut buf = read_up_to(&mut self.reader, to_read as usize)?;
        if (buf.len() as u64) < to_read {
            // The source ended early (it is shorter than it was when it was opened). Deliver the
            // bytes every channel has, then end the stream.
            warn!("dsf: unexpected end of data");
            bytes_per_channel =
                (buf.len() as u64).saturating_sub((channels - 1) * block).min(bytes_per_channel);
            self.num_frames = self.next_frame + bytes_per_channel * 8;
            if bytes_per_channel == 0 {
                return Ok(None);
            }
            frames = frames.min(bytes_per_channel * 8);
        }

        // Keep the planar layout (the data of each channel is contiguous) and drop the padding.
        if bytes_per_channel < block {
            let stride = block as usize;
            let bpc = bytes_per_channel as usize;
            let mut trimmed = Vec::with_capacity(bpc * channels as usize);
            for plane in 0..channels as usize {
                trimmed.extend_from_slice(&buf[plane * stride..plane * stride + bpc]);
            }
            buf = trimmed.into_boxed_slice();
        }

        let pts = self.next_frame;
        self.next_frame += frames;
        Ok(Some(
            PacketBuilder::new()
                .track_id(0)
                .pts(Timestamp::new(pts as i64))
                .dur(Duration::new(frames))
                .data(buf)
                .build(),
        ))
    }
    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }
    fn chapters(&self) -> Option<&ChapterGroup> {
        self.chapters.as_ref()
    }
    fn tracks(&self) -> &[Track] {
        &self.tracks
    }
    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        if !self.reader.is_seekable() {
            return seek_error(SeekErrorKind::Unseekable);
        }
        let required_ts = match to {
            SeekTo::Timestamp { ts, .. } => ts,
            SeekTo::Time { time, .. } => {
                let tb =
                    self.tracks[0].time_base.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;
                tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?
            }
        };
        if required_ts.is_negative() {
            return seek_error(SeekErrorKind::OutOfRange);
        }
        let required_frame = required_ts.get() as u64;
        if required_frame >= self.num_frames {
            return seek_error(SeekErrorKind::OutOfRange);
        }
        // Seek to the start of the block group holding the required sample.
        let frames_per_block = self.block_size_per_channel * 8;
        let block_idx = required_frame / frames_per_block;
        let seek_pos =
            self.data_start_pos + block_idx * self.block_size_per_channel * self.channel_count;
        self.reader.seek(SeekFrom::Start(seek_pos))?;
        self.next_frame = block_idx * frames_per_block;
        Ok(SeekedTo { track_id: 0, required_ts, actual_ts: Timestamp::new(self.next_frame as i64) })
    }
    fn into_inner<'a>(self: Box<Self>) -> MediaSourceStream<'a>
    where
        Self: 'a,
    {
        self.reader
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    /// Build a stereo DSF file with the given block size, sample count, and raw payload.
    fn dsf_bytes(channels: u32, block: u32, sample_count: u64, payload: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"DSD ");
        b.extend_from_slice(&28u64.to_le_bytes());
        b.extend_from_slice(&0u64.to_le_bytes());
        b.extend_from_slice(&0u64.to_le_bytes());
        b.extend_from_slice(b"fmt ");
        b.extend_from_slice(&52u64.to_le_bytes());
        for v in [1u32, 0, 2, channels, 2_822_400, 1] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&sample_count.to_le_bytes());
        b.extend_from_slice(&block.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(12 + payload.len() as u64).to_le_bytes());
        b.extend_from_slice(payload);
        b
    }

    fn open(data: Vec<u8>) -> Result<DsfReader<'static>> {
        let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
        DsfReader::try_new(mss, FormatOptions::default())
    }

    #[test]
    fn verify_truncated_block_group_delivers_common_prefix() {
        // Two channels, 16-byte blocks. One full group (32 bytes) and then channel 0 of the next
        // group is complete while channel 1 only has 5 bytes. The sample count claims more.
        let payload: Vec<u8> = (0..16 + 16 + 16 + 5).map(|i| i as u8).collect::<Vec<_>>();
        let mut reader =
            open(dsf_bytes(2, 16, 4096, &payload)).expect("truncated data is accepted");
        assert_eq!(reader.tracks()[0].num_frames, Some((16 + 5) * 8));

        let first = reader.next_packet().unwrap().unwrap();
        assert_eq!(first.data.len(), 32);
        assert_eq!(first.dur.get(), 16 * 8);

        let second = reader.next_packet().unwrap().unwrap();
        assert_eq!(second.pts.get(), 16 * 8);
        assert_eq!(second.dur.get(), 5 * 8);
        // Planar: 5 bytes of channel 0 followed by 5 bytes of channel 1.
        assert_eq!(&second.data[..5], &payload[32..37]);
        assert_eq!(&second.data[5..], &payload[48..53]);
        assert!(reader.next_packet().unwrap().is_none());
    }

    #[test]
    fn verify_data_chunk_without_complete_channel_prefix_is_empty() {
        let mut reader = open(dsf_bytes(2, 4096, 1024, &[0u8; 5])).expect("short data is accepted");
        assert_eq!(reader.tracks()[0].num_frames, Some(0));
        assert!(reader.next_packet().unwrap().is_none());
    }

    #[test]
    fn verify_oversized_block_size_is_rejected() {
        assert!(open(dsf_bytes(2, u32::MAX, 1024, &[0u8; 64])).is_err());
    }

    #[test]
    fn verify_fmt_chunk_larger_than_52_is_skipped() {
        let mut data = dsf_bytes(1, 8, 64, &[0xAA; 8]);
        // Grow the fmt chunk by 4 bytes (size field at offset 32, extra bytes before "data").
        data[32..40].copy_from_slice(&56u64.to_le_bytes());
        let at = 28 + 52;
        data.splice(at..at, [0u8; 4]);
        let mut reader = open(data).expect("oversized fmt chunk");
        assert_eq!(reader.next_packet().unwrap().unwrap().data.len(), 8);
    }

    #[test]
    fn verify_zero_sample_count_uses_data_size() {
        let reader = open(dsf_bytes(1, 8, 0, &[0u8; 16])).expect("zero sample count");
        assert_eq!(reader.tracks()[0].num_frames, Some(16 * 8));
    }
}
