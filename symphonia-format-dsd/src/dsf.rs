// DSF (DSD Stream File) Format Parser
// Based on DSF specification v1.01

use std::io::{Seek, SeekFrom};

use symphonia_core::audio::layouts;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::common::FourCc;
use symphonia_core::errors::{decode_error, seek_error, unsupported_error};
use symphonia_core::errors::{Result, SeekErrorKind};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::io::ScopedStream;
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::support_format;
use symphonia_core::units::{Duration, Timestamp};

use log::{debug, info, warn};

use crate::CODEC_ID_DSD;

const DSF_MAGIC: [u8; 4] = *b"DSD ";
const DSF_FMT_MAGIC: [u8; 4] = *b"fmt ";
const DSF_DATA_MAGIC: [u8; 4] = *b"data";

const DSF_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"DSF ")),
    short_name: "dsf",
    long_name: "DSD Stream File",
};

#[derive(Debug)]
struct DsfHeader {
    file_size: u64,
    metadata_pointer: u64,
}

impl DsfHeader {
    fn read(reader: &mut MediaSourceStream<'_>) -> Result<Self> {
        let magic = reader.read_quad_bytes()?;
        if magic != DSF_MAGIC {
            return unsupported_error("dsf: invalid magic number");
        }

        let chunk_size = reader.read_u64()?;
        if chunk_size != 28 {
            return decode_error("dsf: invalid header chunk size");
        }

        let file_size = reader.read_u64()?;
        let metadata_pointer = reader.read_u64()?;

        Ok(DsfHeader { file_size, metadata_pointer })
    }
}

fn read_dsf_metadata(reader: &mut MediaSourceStream<'_>, metadata_pointer: u64, metadata_log: &mut MetadataLog) {
    if metadata_pointer == 0 {
        debug!("DSF: No metadata pointer");
        return;
    }

    let current_pos = reader.pos();

    if let Err(e) = reader.seek(SeekFrom::Start(metadata_pointer)) {
        warn!("DSF: Failed to seek to metadata at {}: {}", metadata_pointer, e);
        let _ = reader.seek(SeekFrom::Start(current_pos));
        return;
    }

    let mut side_data = Vec::new();
    let metadata_result = symphonia_metadata::embedded::dsd::read_dsf_id3_metadata(reader, &mut side_data);

    if let Err(e) = reader.seek(SeekFrom::Start(current_pos)) {
        warn!("DSF: Failed to restore position after reading metadata: {}", e);
    }

    match metadata_result {
        Ok(revision) => {
            debug!("DSF: Successfully read ID3v2 metadata");
            metadata_log.push(revision);
        }
        Err(e) => {
            warn!("DSF: Failed to read ID3v2 metadata: {}", e);
        }
    }
}

#[derive(Debug)]
struct DsfFormatChunk {
    format_version: u32,
    format_id: u32,
    _channel_type: u32,
    channel_num: u32,
    sampling_frequency: u32,
    bits_per_sample: u32,
    sample_count: u64,
    block_size_per_channel: u32,
}

impl DsfFormatChunk {
    fn read(reader: &mut MediaSourceStream<'_>) -> Result<Self> {
        let chunk_id = reader.read_quad_bytes()?;
        if chunk_id != DSF_FMT_MAGIC {
            return decode_error("dsf: invalid format chunk ID");
        }

        let chunk_size = reader.read_u64()?;
        if chunk_size != 52 {
            return decode_error("dsf: invalid format chunk size");
        }

        let format_version = reader.read_u32()?;
        let format_id = reader.read_u32()?;
        let _channel_type = reader.read_u32()?;
        let channel_num = reader.read_u32()?;
        let sampling_frequency = reader.read_u32()?;
        let bits_per_sample = reader.read_u32()?;
        let sample_count = reader.read_u64()?;
        let block_size_per_channel = reader.read_u32()?;

        reader.read_u32()?;

        debug!(
            "DSF Format: version={}, channels={}, rate={}, bps={}, samples={}",
            format_version, channel_num, sampling_frequency, bits_per_sample, sample_count
        );

        Ok(DsfFormatChunk {
            format_version,
            format_id,
            _channel_type,
            channel_num,
            sampling_frequency,
            bits_per_sample,
            sample_count,
            block_size_per_channel,
        })
    }

    fn validate(&self) -> Result<()> {
        if self.format_version != 1 {
            return unsupported_error("dsf: unsupported format version");
        }

        if self.format_id != 0 {
            return unsupported_error("dsf: only DSD Raw format supported");
        }

        if self.bits_per_sample != 1 && self.bits_per_sample != 8 {
            return decode_error("dsf: invalid bits per sample");
        }

        if self.channel_num == 0 || self.channel_num > 6 {
            return unsupported_error("dsf: unsupported channel count");
        }

        if self.block_size_per_channel == 0 {
            return decode_error("dsf: invalid block size (zero)");
        }

        let total_block_size = self.block_size_per_channel as u64 * self.channel_num as u64;
        if total_block_size > u32::MAX as u64 {
            return decode_error("dsf: block size too large");
        }

        Ok(())
    }
}

#[derive(Debug)]
struct DsfDataChunk {
    data_size: u64,
}

impl DsfDataChunk {
    fn read(reader: &mut MediaSourceStream<'_>) -> Result<Self> {
        let chunk_id = reader.read_quad_bytes()?;
        if chunk_id != DSF_DATA_MAGIC {
            return decode_error("dsf: invalid data chunk ID");
        }

        let chunk_size = reader.read_u64()?;
        let data_size = chunk_size - 12;

        Ok(DsfDataChunk { data_size })
    }
}

pub struct DsfReader<'s> {
    reader: MediaSourceStream<'s>,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    data_start_pos: u64,
    data_end_pos: u64,
    block_size: u64,
    current_block: u64,
    total_blocks: u64,
}

impl Scoreable for DsfReader<'_> {
    fn score(_src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        Ok(Score::Supported(255))
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

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        debug_assert!(
            self.current_block <= self.total_blocks,
            "current_block exceeds total_blocks"
        );

        if self.reader.pos() >= self.data_end_pos {
            return Ok(None);
        }

        if self.current_block >= self.total_blocks {
            return Ok(None);
        }

        let to_read = self.block_size.min(self.data_end_pos - self.reader.pos());

        let buf = self.reader.read_boxed_slice_exact(to_read as usize)?;

        let ts = Timestamp::new((self.current_block * self.block_size) as i64);
        let dur = Duration::from(to_read);
        self.current_block += 1;

        Ok(Some(Packet::new(0, ts, dur, buf)))
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        let required_ts = match to {
            SeekTo::TimeStamp { ts, .. } => ts,
            SeekTo::Time { time, .. } => {
                let track = &self.tracks[0];
                let tb = track.time_base.unwrap();
                match tb.calc_timestamp(time) {
                    Some(ts) => ts,
                    None => return seek_error(SeekErrorKind::OutOfRange),
                }
            }
        };

        let required_block = required_ts.get() as u64 / self.block_size;

        if required_block >= self.total_blocks {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        let seek_pos = self.data_start_pos + (required_block * self.block_size);

        self.reader.seek(SeekFrom::Start(seek_pos))?;
        self.current_block = required_block;

        let actual_ts = Timestamp::new((required_block * self.block_size) as i64);

        Ok(SeekedTo { track_id: 0, required_ts, actual_ts })
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.reader
    }
}

impl<'s> DsfReader<'s> {
    pub fn try_new(mut source: MediaSourceStream<'s>, _options: FormatOptions) -> Result<Self> {
        let header = DsfHeader::read(&mut source)?;

        info!("DSF file size: {} bytes", header.file_size);

        let format = DsfFormatChunk::read(&mut source)?;
        format.validate()?;

        let data = DsfDataChunk::read(&mut source)?;

        let data_start_pos = source.pos();
        let data_end_pos = data_start_pos + data.data_size;

        let block_size = format.block_size_per_channel * format.channel_num;
        let total_blocks = if block_size > 0 { data.data_size / block_size as u64 } else { 0 };

        debug!(
            "DSF data: start={}, end={}, block_size={}, total_blocks={}",
            data_start_pos, data_end_pos, block_size, total_blocks
        );

        let mut codec_params = AudioCodecParameters::new();

        let channels = match format.channel_num {
            1 => layouts::CHANNEL_LAYOUT_MONO,
            2 => layouts::CHANNEL_LAYOUT_STEREO,
            3 => layouts::CHANNEL_LAYOUT_2P1,
            6 => layouts::CHANNEL_LAYOUT_5P1,
            n => {
                use symphonia_core::audio::{Channels, Position};
                let mut pos = Position::empty();
                if n >= 1 { pos |= Position::FRONT_LEFT; }
                if n >= 2 { pos |= Position::FRONT_RIGHT; }
                if n >= 3 { pos |= Position::FRONT_CENTER; }
                if n >= 4 { pos |= Position::LFE1; }
                if n >= 5 { pos |= Position::REAR_LEFT; }
                if n >= 6 { pos |= Position::REAR_RIGHT; }
                Channels::Positioned(pos)
            }
        };

        codec_params
            .for_codec(CODEC_ID_DSD)
            .with_sample_rate(format.sampling_frequency)
            .with_bits_per_sample(format.bits_per_sample)
            .with_channels(channels)
            .with_channel_data_layout(symphonia_core::codecs::audio::ChannelDataLayout::Planar)
            .with_bit_order(symphonia_core::codecs::audio::BitOrder::LsbFirst)
            .with_max_frames_per_packet(format.block_size_per_channel as u64)
            .with_frames_per_block(format.block_size_per_channel as u64);

        let mut track = Track::new(0);
        track.with_codec_params(CodecParameters::Audio(codec_params));

        if format.sample_count > 0 {
            if let Some(tb) = TimeBase::try_from_recip(format.sampling_frequency) {
                track.with_time_base(tb);
            }
            track.with_num_frames(format.sample_count);
        }

        let mut metadata_log = MetadataLog::default();
        read_dsf_metadata(&mut source, header.metadata_pointer, &mut metadata_log);

        Ok(DsfReader {
            reader: source,
            tracks: vec![track],
            metadata: metadata_log,
            data_start_pos,
            data_end_pos,
            block_size: block_size as u64,
            current_block: 0,
            total_blocks,
        })
    }
}
