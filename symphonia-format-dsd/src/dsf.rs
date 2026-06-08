// DSF (DSD Stream File) Format Parser
use std::io::{Seek, SeekFrom};
use std::num::NonZero;
use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::audio::{AudioCodecParameters, BitOrder, ChannelDataLayout};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::common::FourCc;
use symphonia_core::errors::{decode_error, seek_error, unsupported_error, Result, SeekErrorKind};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog, MetadataSideData};
use symphonia_core::support_format;
use symphonia_core::units::{TimeBase, Timestamp, Duration};
use symphonia_metadata::embedded::riff::read_riff_id3_chunk;
use log::warn;
use crate::CODEC_TYPE_DSD;

const DSF_MAGIC: [u8; 4] = *b"DSD ";
const DSF_FMT_MAGIC: [u8; 4] = *b"fmt ";
const DSF_DATA_MAGIC: [u8; 4] = *b"data";

const DSF_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"DSF\0")), short_name: "dsf", long_name: "DSD Stream File",
};

struct DsfHeader { metadata_pointer: u64 }
impl DsfHeader {
    fn read<B: ReadBytes>(reader: &mut B) -> Result<Self> {
        if reader.read_quad_bytes()? != DSF_MAGIC { return decode_error("dsf: bad magic"); }
        let chunk_size = reader.read_u64()?;
        if chunk_size != 28 { return decode_error("dsf: bad header size"); }
        let _file_size = reader.read_u64()?;
        let metadata_pointer = reader.read_u64()?;
        Ok(DsfHeader { metadata_pointer })
    }
}

struct DsfFormatChunk { format_version: u32, format_id: u32, channel_type: u32, channel_count: u32, sample_rate: u32, bits_per_sample: u32, sample_count: u64, block_size_per_channel: u32, _reserved: u32 }
impl DsfFormatChunk {
    fn read<B: ReadBytes>(reader: &mut B) -> Result<Self> {
        if reader.read_quad_bytes()? != DSF_FMT_MAGIC { return decode_error("dsf: bad fmt"); }
        if reader.read_u64()? != 52 { return decode_error("dsf: bad fmt size"); }
        Ok(DsfFormatChunk {
            format_version: reader.read_u32()?, format_id: reader.read_u32()?,
            channel_type: reader.read_u32()?, channel_count: reader.read_u32()?,
            sample_rate: reader.read_u32()?, bits_per_sample: reader.read_u32()?,
            sample_count: reader.read_u64()?, block_size_per_channel: reader.read_u32()?,
            _reserved: reader.read_u32()?,
        })
    }
    fn validate(&self) -> Result<()> {
        if self.format_version != 1 { return unsupported_error("dsf: bad version"); }
        if self.format_id != 0 { return unsupported_error("dsf: bad format id"); }
        if self.channel_count < 1 || self.channel_count > 6 { return decode_error("dsf: bad channels"); }
        if self.bits_per_sample != 1 && self.bits_per_sample != 8 { return decode_error("dsf: bad bps"); }
        Ok(())
    }
}

struct DsfDataChunk { data_size: u64 }
impl DsfDataChunk {
    fn read<B: ReadBytes>(reader: &mut B) -> Result<Self> {
        if reader.read_quad_bytes()? != DSF_DATA_MAGIC { return decode_error("dsf: bad data"); }
        let data_size = reader.read_u64()?;
        if data_size < 12 { return decode_error("dsf: data too small"); }
        Ok(DsfDataChunk { data_size: data_size - 12 })
    }
}

fn channels_for(channel_type: u32, channel_count: u32) -> Channels {
    let pos = match channel_type {
        1 => Position::FRONT_CENTER,
        2 => Position::FRONT_LEFT | Position::FRONT_RIGHT,
        3 => Position::FRONT_LEFT | Position::FRONT_RIGHT | Position::FRONT_CENTER,
        4 => Position::FRONT_LEFT | Position::FRONT_RIGHT | Position::REAR_LEFT | Position::REAR_RIGHT,
        5 => Position::FRONT_LEFT | Position::FRONT_RIGHT | Position::FRONT_CENTER | Position::LFE1,
        6 => Position::FRONT_LEFT | Position::FRONT_RIGHT | Position::FRONT_CENTER | Position::REAR_LEFT | Position::REAR_RIGHT,
        7 => Position::FRONT_LEFT | Position::FRONT_RIGHT | Position::FRONT_CENTER | Position::LFE1 | Position::REAR_LEFT | Position::REAR_RIGHT,
        _ => return Channels::Discrete(channel_count as u16),
    };
    if pos.bits().count_ones() != channel_count { return Channels::Discrete(channel_count as u16); }
    Channels::Positioned(pos)
}

pub struct DsfReader<'s> {
    reader: MediaSourceStream<'s>, media_info: MediaInfo, tracks: Vec<Track>,
    metadata: MetadataLog, data_start_pos: u64, data_end_pos: u64,
    block_size_per_channel: u32, channel_count: u32, current_pos: u64, sample_count: u64,
}

impl<'s> DsfReader<'s> {
    pub fn try_new(mut source: MediaSourceStream<'s>, _options: FormatOptions) -> Result<Self> {
        let header = DsfHeader::read(&mut source)?;
        let fmt = DsfFormatChunk::read(&mut source)?;
        fmt.validate()?;
        let mut metadata_log = MetadataLog::default();
        let data = DsfDataChunk::read(&mut source)?;
        let data_start_pos = source.pos();
        let data_end_pos = data_start_pos + data.data_size;

        if header.metadata_pointer != 0 && header.metadata_pointer >= data_end_pos && source.is_seekable() {
            source.seek(SeekFrom::Start(header.metadata_pointer))?;
            let mut side_data = Vec::<MetadataSideData>::new();
            match read_riff_id3_chunk(&mut source, &mut side_data) {
                Ok(rev) => metadata_log.push(rev),
                Err(e) => warn!("dsf: failed to read ID3v2 metadata: {}", e),
            }
            source.seek(SeekFrom::Start(data_start_pos))?;
        }

        let bit_order = if fmt.bits_per_sample == 8 { BitOrder::MsbFirst } else { BitOrder::LsbFirst };
        let channels = channels_for(fmt.channel_type, fmt.channel_count);
        let mut params = AudioCodecParameters::new();
        params.for_codec(CODEC_TYPE_DSD).with_sample_rate(fmt.sample_rate)
            .with_bits_per_sample(1).with_channels(channels)
            .with_channel_data_layout(ChannelDataLayout::Planar)
            .with_bit_order(bit_order);
        let tb = TimeBase::new(NonZero::new(1).unwrap(), NonZero::new(fmt.sample_rate).unwrap());
        params.with_max_frames_per_packet(4096 * 8).with_frames_per_block(4096 * 8);
        let mut track = Track::new(0);
        track.time_base = Some(tb);
        track.num_frames = Some(fmt.sample_count);
        track.with_codec_params(CodecParameters::Audio(params));

        Ok(DsfReader { reader: source, media_info: MediaInfo::new(), tracks: vec![track],
            metadata: metadata_log, data_start_pos, data_end_pos,
            block_size_per_channel: fmt.block_size_per_channel, channel_count: fmt.channel_count,
            current_pos: data_start_pos, sample_count: fmt.sample_count })
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
    fn try_probe_new(mss: MediaSourceStream<'_>, opts: FormatOptions) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(DsfReader::try_new(mss, opts)?))
    }
    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(DSF_FORMAT_INFO, &["dsf"], &["audio/dsd"], &[b"DSD "])]
    }
}

impl FormatReader for DsfReader<'_> {
    fn format_info(&self) -> &FormatInfo { &DSF_FORMAT_INFO }
    fn media_info(&self) -> &MediaInfo { &self.media_info }
    fn next_packet(&mut self) -> Result<Option<Packet>> {
        let channels = self.channel_count as u64;
        let frame_pos = ((self.current_pos - self.data_start_pos) * 8) / channels;
        if self.current_pos >= self.data_end_pos || frame_pos >= self.sample_count { return Ok(None); }
        let to_read = (self.block_size_per_channel as u64 * channels).min(self.data_end_pos - self.current_pos);
        let mut frames = (to_read * 8) / channels;
        frames = frames.min(self.sample_count - frame_pos);
        let buf = self.reader.read_boxed_slice_exact(to_read as usize)?;
        self.current_pos += to_read;
        Ok(Some(PacketBuilder::new().track_id(0).pts(Timestamp::new(frame_pos as i64))
            .dur(Duration::new(frames)).data(buf).build()))
    }
    fn metadata(&mut self) -> Metadata<'_> { self.metadata.metadata() }
    fn tracks(&self) -> &[Track] { &self.tracks }
    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        let required_frame = match to {
            SeekTo::Timestamp { ts, .. } => ts.get() as u64,
            SeekTo::Time { time, .. } => {
                let tb = self.tracks[0].time_base
                    .ok_or(symphonia_core::errors::Error::SeekError(SeekErrorKind::Unseekable))?;
                tb.calc_timestamp(time).unwrap_or(Timestamp::ZERO).get() as u64
            }
        };
        let frames_per_block = self.block_size_per_channel as u64 * 8;
        let block_idx = required_frame / frames_per_block;
        let seek_pos = self.data_start_pos + block_idx * (self.block_size_per_channel as u64 * self.channel_count as u64);
        if seek_pos >= self.data_end_pos { return seek_error(SeekErrorKind::OutOfRange); }
        self.reader.seek(SeekFrom::Start(seek_pos))?;
        self.current_pos = seek_pos;
        let actual_frame = block_idx * frames_per_block;
        Ok(SeekedTo { track_id: 0, required_ts: Timestamp::new(actual_frame as i64), actual_ts: Timestamp::new(actual_frame as i64) })
    }
    fn into_inner<'a>(self: Box<Self>) -> MediaSourceStream<'a> where Self: 'a { self.reader }
}
