// DFF (DSDIFF) Format Parser
// Based on DSDIFF specification v1.5

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

use log::{debug, warn};

use crate::dff_info::DffInfoParser;
use crate::CODEC_ID_DSD;

const DFF_FRM8_MAGIC: [u8; 4] = *b"FRM8";
const DFF_DSD_FORM: [u8; 4] = *b"DSD ";
const DFF_FVER_ID: [u8; 4] = *b"FVER";
const DFF_PROP_ID: [u8; 4] = *b"PROP";
const DFF_SND_FORM: [u8; 4] = *b"SND ";
const DFF_CMPR_DSD: [u8; 4] = *b"DSD ";

const DFF_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"DFF ")),
    short_name: "dff",
    long_name: "DSDIFF",
};

#[derive(Debug)]
struct DffHeader {
    _file_size: u64,
}

impl DffHeader {
    fn read(reader: &mut MediaSourceStream<'_>) -> Result<Self> {
        let magic = reader.read_quad_bytes()?;
        if magic != DFF_FRM8_MAGIC {
            return unsupported_error("dff: invalid FRM8 magic");
        }

        let _file_size = reader.read_be_u64()?;

        let form_type = reader.read_quad_bytes()?;
        if form_type != DFF_DSD_FORM {
            return unsupported_error("dff: not a DSD form");
        }

        Ok(DffHeader { _file_size })
    }
}

#[derive(Debug)]
struct DffFormatVersion {
    major: u8,
}

impl DffFormatVersion {
    fn read(reader: &mut MediaSourceStream<'_>) -> Result<Self> {
        let chunk_id = reader.read_quad_bytes()?;
        if chunk_id != DFF_FVER_ID {
            return decode_error("dff: expected FVER chunk");
        }

        let chunk_size = reader.read_be_u64()?;
        if chunk_size != 4 {
            return decode_error("dff: invalid FVER chunk size");
        }

        let major = reader.read_u8()?;
        let _minor = reader.read_u8()?;
        let _revision = reader.read_u8()?;
        let _build = reader.read_u8()?;

        debug!("DFF version: {}.{}.{}.{}", major, _minor, _revision, _build);

        Ok(DffFormatVersion { major })
    }
}

fn validate_channel_ids(channels: &[[u8; 4]], count: u16) -> Result<()> {
    const VALID_CHANNEL_IDS: &[&[u8; 4]] = &[
        b"SLFT", b"SRGT", b"MLFT", b"MRGT", b"C   ", b"LFE ",
        b"LS  ", b"RS  ", b"SL  ", b"SR  ",
    ];

    if channels.len() != count as usize {
        return decode_error("dff: channel ID count mismatch");
    }

    for ch_id in channels {
        if !VALID_CHANNEL_IDS.contains(&ch_id) {
            warn!("DFF: Unknown channel ID: {}", String::from_utf8_lossy(ch_id));
        }
    }

    match count {
        2 => {
            if channels[0] != *b"SLFT" || channels[1] != *b"SRGT" {
                warn!(
                    "DFF: Non-standard stereo channel order: {:?}, {:?}",
                    String::from_utf8_lossy(&channels[0]),
                    String::from_utf8_lossy(&channels[1])
                );
            }
        }
        6 => {
            let expected = [*b"MLFT", *b"MRGT", *b"C   ", *b"LFE ", *b"LS  ", *b"RS  "];
            if channels != expected {
                warn!("DFF: Non-standard 5.1 channel order");
            }
        }
        _ => {}
    }

    Ok(())
}

#[derive(Debug)]
struct DffSoundProperties {
    sample_rate: u32,
    channel_count: u16,
    _channels: Vec<[u8; 4]>,
    compression: [u8; 4],
}

impl DffSoundProperties {
    fn read(reader: &mut MediaSourceStream<'_>) -> Result<Self> {
        let chunk_id = reader.read_quad_bytes()?;
        if chunk_id != DFF_PROP_ID {
            return decode_error("dff: expected PROP chunk");
        }

        let chunk_size = reader.read_be_u64()?;
        let prop_end = reader.pos() + chunk_size;

        let form_type = reader.read_quad_bytes()?;
        if form_type != DFF_SND_FORM {
            return unsupported_error("dff: expected SND property form");
        }

        let mut sample_rate = None;
        let mut channel_count = None;
        let mut channels = None;
        let mut compression = DFF_CMPR_DSD;

        while reader.pos() < prop_end {
            let id = reader.read_quad_bytes()?;
            let size = reader.read_be_u64()?;

            match &id {
                b"FS  " => {
                    sample_rate = Some(reader.read_be_u32()?);
                }
                b"CHNL" => {
                    let count = reader.read_be_u16()?;
                    channel_count = Some(count);

                    let mut ch_ids = Vec::new();
                    for _ in 0..count {
                        let ch_id = reader.read_quad_bytes()?;
                        ch_ids.push(ch_id);
                    }
                    channels = Some(ch_ids);
                }
                b"CMPR" => {
                    compression = reader.read_quad_bytes()?;
                    if size > 4 {
                        reader.ignore_bytes(size - 4)?;
                    }
                }
                _ => {
                    warn!("DFF: Skipping unknown PROP chunk: {}", String::from_utf8_lossy(&id));
                    reader.ignore_bytes(size)?;
                }
            }

            if size % 2 == 1 {
                reader.ignore_bytes(1)?;
            }
        }

        let sample_rate = match sample_rate {
            Some(sr) => sr,
            None => return decode_error("dff: missing sample rate in PROP chunk"),
        };

        let channel_count = match channel_count {
            Some(cc) => cc,
            None => return decode_error("dff: missing channel count in PROP chunk"),
        };

        let _channels = match channels {
            Some(ch) => ch,
            None => return decode_error("dff: missing channel IDs in PROP chunk"),
        };

        validate_channel_ids(&_channels, channel_count)?;

        debug!(
            "DFF properties: rate={}, channels={}, compression={:?}",
            sample_rate, channel_count, compression
        );

        Ok(DffSoundProperties { sample_rate, channel_count, _channels, compression })
    }

    fn validate(&self) -> Result<()> {
        if self.compression != DFF_CMPR_DSD {
            return unsupported_error(
                "dff: only uncompressed DSD is supported. DST (Direct Stream Transfer) compression \
                 is not currently implemented. Consider converting the file using tools like: \
                 (1) foobar2000 with DSD transcoder plugin, (2) Saracon audio converter, \
                 (3) ffmpeg with DST support"
            );
        }

        if self.channel_count == 0 || self.channel_count > 6 {
            return unsupported_error("dff: unsupported channel count");
        }

        Ok(())
    }
}

pub struct DffReader<'s> {
    reader: MediaSourceStream<'s>,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    data_start_pos: u64,
    data_end_pos: u64,
    current_pos: u64,
}

impl Scoreable for DffReader<'_> {
    fn score(_src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        Ok(Score::Supported(255))
    }
}

impl ProbeableFormat<'_> for DffReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(DffReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(DFF_FORMAT_INFO, &["dff"], &["audio/dsd"], &[b"FRM8"])]
    }
}

impl FormatReader for DffReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &DFF_FORMAT_INFO
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        if self.current_pos >= self.data_end_pos {
            return Ok(None);
        }

        let block_size: u64 = 4096;
        let remaining = self.data_end_pos - self.current_pos;
        let to_read = block_size.min(remaining);

        let buf = self.reader.read_boxed_slice_exact(to_read as usize)?;

        let ts = Timestamp::new(((self.current_pos - self.data_start_pos) * 8) as i64);
        let dur = Duration::from(to_read * 8);
        self.current_pos += to_read;

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

        let required_byte = required_ts.get() as u64 / 8;
        let seek_pos = self.data_start_pos + required_byte;

        if seek_pos >= self.data_end_pos {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        self.reader.seek(SeekFrom::Start(seek_pos))?;
        self.current_pos = seek_pos;

        let actual_ts = Timestamp::new(((seek_pos - self.data_start_pos) * 8) as i64);

        Ok(SeekedTo { track_id: 0, required_ts, actual_ts })
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.reader
    }
}

impl<'s> DffReader<'s> {
    pub fn try_new(mut source: MediaSourceStream<'s>, _options: FormatOptions) -> Result<Self> {
        let _header = DffHeader::read(&mut source)?;

        let version = DffFormatVersion::read(&mut source)?;

        if version.major != 1 {
            return unsupported_error("dff: unsupported format version");
        }

        let props = DffSoundProperties::read(&mut source)?;
        props.validate()?;

        let mut info_parser = DffInfoParser::new();

        let mut data_start_pos = None;
        let mut data_size = None;

        while let Ok(chunk_id) = source.read_quad_bytes() {
            let chunk_size = source.read_be_u64()?;

            if &chunk_id == b"DSD " {
                data_start_pos = Some(source.pos());
                data_size = Some(chunk_size);
                break;
            }
            else if &chunk_id == b"DIIN" || &chunk_id == b"DITI" || &chunk_id == b"DIAR"
                    || &chunk_id == b"DISP" || &chunk_id == b"DIGE" || &chunk_id == b"COMT" {
                debug!("DFF: Parsing INFO chunk: {}", String::from_utf8_lossy(&chunk_id));
                if let Err(e) = info_parser.parse_chunk(&chunk_id, &mut source, chunk_size) {
                    warn!("DFF: Failed to parse INFO chunk: {}", e);
                }

                if chunk_size % 2 == 1 {
                    source.ignore_bytes(1)?;
                }
            }
            else {
                debug!("DFF: Skipping chunk: {}", String::from_utf8_lossy(&chunk_id));
                source.ignore_bytes(chunk_size)?;

                if chunk_size % 2 == 1 {
                    source.ignore_bytes(1)?;
                }
            }
        }

        let data_start_pos = match data_start_pos {
            Some(pos) => pos,
            None => return decode_error("dff: no DSD audio data chunk found"),
        };

        let data_size = match data_size {
            Some(size) => size,
            None => return decode_error("dff: no DSD audio data size"),
        };

        let data_end_pos = data_start_pos + data_size;

        debug!("DFF data: start={}, end={}, size={}", data_start_pos, data_end_pos, data_size);

        let mut codec_params = AudioCodecParameters::new();

        let channels = match props.channel_count {
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
            .with_sample_rate(props.sample_rate)
            .with_bits_per_sample(1)
            .with_channels(channels)
            .with_channel_data_layout(symphonia_core::codecs::audio::ChannelDataLayout::Interleaved)
            .with_bit_order(symphonia_core::codecs::audio::BitOrder::MsbFirst);

        let total_bytes = data_size;
        let samples_per_channel = (total_bytes * 8) / props.channel_count as u64;

        let block_size: u64 = 4096;
        codec_params
            .with_max_frames_per_packet(block_size * 8)
            .with_frames_per_block(block_size * 8);

        let mut track = Track::new(0);
        track.with_codec_params(CodecParameters::Audio(codec_params));

        if let Some(tb) = TimeBase::try_from_recip(props.sample_rate) {
            track.with_time_base(tb);
        }
        track.with_num_frames(samples_per_channel);

        let mut metadata_log = MetadataLog::default();
        let info_revision = info_parser.into_revision();
        if !info_revision.media.tags.is_empty() || !info_revision.media.visuals.is_empty() {
            metadata_log.push(info_revision);
        }

        Ok(DffReader {
            reader: source,
            tracks: vec![track],
            metadata: metadata_log,
            data_start_pos,
            data_end_pos,
            current_pos: data_start_pos,
        })
    }
}
