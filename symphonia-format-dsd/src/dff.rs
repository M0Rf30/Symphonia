// DFF (DSDIFF) Format Parser
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
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::support_format;
use symphonia_core::units::{TimeBase, Timestamp, Duration};
use log::warn;
use crate::dff_info::DffInfoParser;
use crate::CODEC_TYPE_DSD;

const DFF_FRM8_MAGIC: [u8; 4] = *b"FRM8";
const DFF_DSD_FORM: [u8; 4] = *b"DSD ";
const DFF_FVER_ID: [u8; 4] = *b"FVER";
const DFF_PROP_ID: [u8; 4] = *b"PROP";
const DFF_SND_FORM: [u8; 4] = *b"SND ";

const DFF_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"DFF\0")), short_name: "dff", long_name: "DSDIFF",
};

struct DffHeader;
impl DffHeader {
    fn read<B: ReadBytes>(reader: &mut B) -> Result<Self> {
        if reader.read_quad_bytes()? != DFF_FRM8_MAGIC { return decode_error("dff: bad magic"); }
        let _file_size = reader.read_be_u64()?;
        if reader.read_quad_bytes()? != DFF_DSD_FORM { return decode_error("dff: not DSD"); }
        Ok(DffHeader)
    }
}

struct DffFormatVersion { major: u32 }
impl DffFormatVersion {
    fn read<B: ReadBytes>(reader: &mut B) -> Result<Self> {
        if reader.read_quad_bytes()? != DFF_FVER_ID { return decode_error("dff: expected FVER"); }
        let _chunk_size = reader.read_be_u64()?;
        Ok(DffFormatVersion { major: reader.read_be_u32()? >> 24 })
    }
}

struct DffSoundProperties { channel_count: u16, sample_rate: u32, compression: [u8; 4], channels: Channels }
impl DffSoundProperties {
    fn read<B: ReadBytes>(reader: &mut B) -> Result<Self> {
        if reader.read_quad_bytes()? != DFF_PROP_ID { return decode_error("dff: expected PROP"); }
        let prop_size = reader.read_be_u64()?;
        if reader.read_quad_bytes()? != DFF_SND_FORM { return decode_error("dff: not sound"); }
        let mut consumed: u64 = 4;
        let mut sample_rate = None;
        let mut channel_count = None;
        let mut compression = *b"DSD ";
        let mut channel_ids: Vec<[u8; 4]> = Vec::new();
        while consumed < prop_size {
            let id = reader.read_quad_bytes()?;
            let size = reader.read_be_u64()?;
            match &id {
                b"FS  " => { sample_rate = Some(reader.read_be_u32()?); if size > 4 { reader.ignore_bytes(size - 4)?; } }
                b"CHNL" => {
                    let count = reader.read_be_u16()?;
                    for _ in 0..count { channel_ids.push(reader.read_quad_bytes()?); }
                    let read_bytes = 2 + 4 * count as u64;
                    if size > read_bytes { reader.ignore_bytes(size - read_bytes)?; }
                    channel_count = Some(count);
                }
                b"CMPR" => { compression = reader.read_quad_bytes()?; if size > 4 { reader.ignore_bytes(size - 4)?; } }
                _ => { reader.ignore_bytes(size)?; }
            }
            let pad = size & 1;
            if pad == 1 { reader.ignore_bytes(1)?; }
            consumed += 12 + size + pad;
        }
        let sample_rate = match sample_rate { Some(s) => s, None => return decode_error("dff: missing sample rate") };
        let channel_count = match channel_count { Some(c) => c, None => return decode_error("dff: missing channel count") };
        let channels = Self::map_channel_ids(&channel_ids, channel_count);
        Ok(DffSoundProperties { channel_count, sample_rate, compression, channels })
    }
    fn map_channel_ids(ids: &[[u8; 4]], count: u16) -> Channels {
        let mut mask = Position::empty();
        let mut last_bit: i32 = -1;
        let mut all_known = true;
        for id in ids {
            let pos = match id {
                b"SLFT" | b"MLFT" => Position::FRONT_LEFT,
                b"SRGT" | b"MRGT" => Position::FRONT_RIGHT,
                b"C   " => Position::FRONT_CENTER,
                b"LFE " => Position::LFE1,
                b"LS  " => Position::REAR_LEFT,
                b"RS  " => Position::REAR_RIGHT,
                b"Cs  " => Position::REAR_CENTER,
                _ => { all_known = false; break; }
            };
            let bit = pos.bits().trailing_zeros() as i32;
            if bit <= last_bit { all_known = false; break; }
            last_bit = bit;
            mask |= pos;
        }
        if all_known && mask.bits().count_ones() == count as u32 { Channels::Positioned(mask) } else { Channels::Discrete(count) }
    }
    fn validate(&self) -> Result<()> {
        if self.compression != *b"DSD " {
            return unsupported_error(
                "dff: only uncompressed DSD is supported; DST (Direct Stream Transfer) compression is not implemented"
            );
        }
        if self.channel_count < 1 || self.channel_count > 6 { return decode_error("dff: bad channels"); }
        Ok(())
    }
}

pub struct DffReader<'s> {
    reader: MediaSourceStream<'s>, media_info: MediaInfo, tracks: Vec<Track>,
    metadata: MetadataLog, data_start_pos: u64, data_end_pos: u64, current_pos: u64,
    channel_count: u16,
}

impl<'s> DffReader<'s> {
    pub fn try_new(mut source: MediaSourceStream<'s>, _options: FormatOptions) -> Result<Self> {
        let _header = DffHeader::read(&mut source)?;
        if DffFormatVersion::read(&mut source)?.major != 1 { return unsupported_error("dff: bad version"); }
        let props = DffSoundProperties::read(&mut source)?;
        props.validate()?;

        let mut info_parser = DffInfoParser::new();
        let mut data_start_pos = None;
        let mut data_size = None;
        while let Ok(chunk_id) = source.read_quad_bytes() {
            let chunk_size = source.read_be_u64()?;
            if &chunk_id == b"DSD " { data_start_pos = Some(source.pos()); data_size = Some(chunk_size); break; }
            source.ignore_bytes(chunk_size)?;
            if chunk_size % 2 == 1 { source.ignore_bytes(1)?; }
        }
        let data_start_pos = match data_start_pos { Some(p) => p, None => return Err(decode_error::<u64>("dff: no data").unwrap_err()), };
        let data_size = match data_size { Some(s) => s, None => return Err(decode_error::<u64>("dff: no data size").unwrap_err()), };
        let data_end_pos = data_start_pos + data_size;
        if source.is_seekable() {
            let pad = data_size & 1;
            let _ = source.seek(SeekFrom::Start(data_end_pos + pad));
            while let Ok(chunk_id) = source.read_quad_bytes() {
                let chunk_size = source.read_be_u64()?;
                match &chunk_id {
                    b"DIIN" => { if let Err(e) = info_parser.parse_diin(&mut source, chunk_size) { warn!("DFF DIIN: {}", e); } }
                    b"COMT" => { if let Err(e) = info_parser.parse_comt(&mut source, chunk_size) { warn!("DFF COMT: {}", e); } }
                    _ => { source.ignore_bytes(chunk_size)?; }
                }
                if chunk_size % 2 == 1 { source.ignore_bytes(1)?; }
            }
            source.seek(SeekFrom::Start(data_start_pos))?;
        }
        let channels = props.channels.clone();
        let mut params = AudioCodecParameters::new();
        params.for_codec(CODEC_TYPE_DSD).with_sample_rate(props.sample_rate)
            .with_bits_per_sample(1).with_channels(channels)
            .with_channel_data_layout(ChannelDataLayout::Interleaved)
            .with_bit_order(BitOrder::MsbFirst);
        let total_bytes = data_size;
        let samples_per_channel = (total_bytes * 8) / props.channel_count as u64;
        let tb = TimeBase::new(NonZero::new(1).unwrap(), NonZero::new(props.sample_rate).unwrap());
        params.with_max_frames_per_packet(4096 * 8).with_frames_per_block(4096 * 8);
        let mut track = Track::new(0);
        track.time_base = Some(tb); track.num_frames = Some(samples_per_channel);
        track.with_codec_params(CodecParameters::Audio(params));
        let mut metadata_log = MetadataLog::default();
        metadata_log.push(info_parser.into_metadata().build());
        Ok(DffReader { reader: source, media_info: MediaInfo::new(), tracks: vec![track],
            metadata: metadata_log, data_start_pos, data_end_pos, current_pos: data_start_pos,
            channel_count: props.channel_count })
    }
}

impl Scoreable for DffReader<'_> {
    fn score(mut mss: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        match mss.read_quad_bytes() {
            Ok(b) if b == DFF_FRM8_MAGIC => Ok(Score::Supported(255)),
            _ => Ok(Score::Unsupported),
        }
    }
}

impl ProbeableFormat<'_> for DffReader<'_> {
    fn try_probe_new(mss: MediaSourceStream<'_>, opts: FormatOptions) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(DffReader::try_new(mss, opts)?))
    }
    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(DFF_FORMAT_INFO, &["dff"], &["audio/dsd"], &[b"FRM8"])]
    }
}

impl FormatReader for DffReader<'_> {
    fn format_info(&self) -> &FormatInfo { &DFF_FORMAT_INFO }
    fn media_info(&self) -> &MediaInfo { &self.media_info }
    fn next_packet(&mut self) -> Result<Option<Packet>> {
        let channels = self.channel_count as u64;
        if self.current_pos >= self.data_end_pos { return Ok(None); }
        let aligned = (4096 / channels) * channels;
        let remaining = self.data_end_pos - self.current_pos;
        let mut to_read = aligned.min(remaining);
        to_read -= to_read % channels;
        if to_read == 0 { return Ok(None); }
        let frames = (to_read * 8) / channels;
        let pts = ((self.current_pos - self.data_start_pos) * 8) / channels;
        let buf = self.reader.read_boxed_slice_exact(to_read as usize)?;
        self.current_pos += to_read;
        Ok(Some(PacketBuilder::new().track_id(0).pts(Timestamp::new(pts as i64))
            .dur(Duration::new(frames)).data(buf).build()))
    }
    fn metadata(&mut self) -> Metadata<'_> { self.metadata.metadata() }
    fn tracks(&self) -> &[Track] { &self.tracks }
    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        let required_byte = match to {
            SeekTo::Timestamp { ts, .. } => ts.get() as u64 / 8,
            SeekTo::Time { time, .. } => {
                let tb = self.tracks[0].time_base
                    .ok_or(symphonia_core::errors::Error::SeekError(SeekErrorKind::Unseekable))?;
                tb.calc_timestamp(time).unwrap_or(Timestamp::ZERO).get() as u64 / 8
            }
        };
        let seek_pos = self.data_start_pos + required_byte;
        if seek_pos >= self.data_end_pos { return seek_error(SeekErrorKind::OutOfRange); }
        self.reader.seek(SeekFrom::Start(seek_pos))?;
        self.current_pos = seek_pos;
        let actual_ts = (seek_pos - self.data_start_pos) * 8;
        Ok(SeekedTo { track_id: 0, required_ts: Timestamp::new(actual_ts as i64), actual_ts: Timestamp::new(actual_ts as i64) })
    }
    fn into_inner<'a>(self: Box<Self>) -> MediaSourceStream<'a> where Self: 'a { self.reader }
}
