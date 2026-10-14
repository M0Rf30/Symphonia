// DFF (DSDIFF) Format Parser
//
// Implements "Direct Stream Digital Interchange File Format" v1.5 (Philips, 2004-04-27):
//
//   FRM8 { FVER, PROP { FS, CHNL, CMPR, ABSS, LSCO }, DSD | DST { FRTE, DSTF*, DSTC* },
//          DSTI, COMT, DIIN { EMID, MARK*, DIAR, DITI }, MANF, ID3 }
//
// All numeric fields are big-endian and every chunk size is 64-bit. Chunks are padded to an even
// length. The chunks are read in the order they appear in the file, in any order and also when
// the metadata chunks precede the sound data; unknown chunks are skipped (DSDIFF 2.4).
use crate::dff_info::{AbsTime, Be, DSDIFF_CHUNK_HEADER_LEN, DffMeta, chunks};
use crate::{CODEC_TYPE_DSD, CODEC_TYPE_DST, MAX_DSD_CHANNELS, read_up_to, read_up_to_into};
use log::warn;
use std::io::{ErrorKind, Seek, SeekFrom};
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
use symphonia_core::meta::{
    ChapterGroup, Metadata, MetadataLog, MetadataRevision, MetadataSideData,
};
use symphonia_core::support_format;
use symphonia_core::units::{Duration, TimeBase, Timestamp};
use symphonia_metadata::embedded::riff::read_riff_id3_chunk;

const DFF_FRM8_MAGIC: [u8; 4] = *b"FRM8";
const DFF_DSD_FORM: [u8; 4] = *b"DSD ";
const DFF_SND_FORM: [u8; 4] = *b"SND ";

/// The largest PROP chunk accepted. The chunk holds a handful of small chunks.
const MAX_PROP_LEN: u64 = 1 << 20;
/// The largest metadata chunk (COMT, DIIN, MANF) that is read into memory.
const MAX_META_LEN: u64 = 32 << 20;
/// The largest DST frame accepted. A frame decodes to at most `channels * rate / 75 / 8` bytes,
/// and a DST frame is never meaningfully larger than that.
const MAX_DST_FRAME_LEN: u64 = 16 << 20;
/// The number of bytes of uncompressed DSD data in a packet, for all channels together.
const DSD_PACKET_BYTES: u64 = 4096;

const DFF_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"DFF\0")),
    short_name: "dff",
    long_name: "DSDIFF",
};

/// The sound properties (PROP chunk).
struct SoundProperties {
    sample_rate: u32,
    channel_count: u16,
    channel_ids: Vec<[u8; 4]>,
    compression: Option<[u8; 4]>,
    abs_start: Option<AbsTime>,
    loudspeaker_config: Option<u16>,
    id3: Vec<MetadataRevision>,
    chapters: Option<ChapterGroup>,
}

impl SoundProperties {
    /// Parse the body of a PROP chunk.
    fn parse(body: &[u8], meta: &mut DffMeta) -> Result<Option<Self>> {
        let mut r = Be::new(body);
        if r.id() != Some(DFF_SND_FORM) {
            // Property types other than "SND " are reserved for future use (DSDIFF 3.2).
            return Ok(None);
        }
        let mut props = SoundProperties {
            sample_rate: 0,
            channel_count: 0,
            channel_ids: Vec::new(),
            compression: None,
            abs_start: None,
            loudspeaker_config: None,
            id3: Vec::new(),
            chapters: None,
        };
        let mut have_rate = false;
        let mut have_channels = false;
        for (id, data) in chunks(r.rest()) {
            let mut c = Be::new(data);
            match &id {
                b"FS  " => {
                    let Some(rate) = c.u32()
                    else {
                        return decode_error("dff: FS chunk too small");
                    };
                    props.sample_rate = rate;
                    have_rate = true;
                }
                b"CHNL" => {
                    let Some(count) = c.u16()
                    else {
                        return decode_error("dff: CHNL chunk too small");
                    };
                    if c.remaining() < 4 * usize::from(count) {
                        return decode_error("dff: CHNL chunk shorter than its channel count");
                    }
                    props.channel_ids = (0..count).filter_map(|_| c.id()).collect();
                    props.channel_count = count;
                    have_channels = true;
                }
                b"CMPR" => {
                    let Some(ty) = c.id()
                    else {
                        return decode_error("dff: CMPR chunk too small");
                    };
                    props.compression = Some(ty);
                }
                b"ABSS" => props.abs_start = AbsTime::parse(data),
                b"LSCO" => props.loudspeaker_config = c.u16(),
                id if is_id3_chunk(id) => {
                    let mut side_data = Vec::<MetadataSideData>::new();
                    match read_riff_id3_chunk(&mut BufReader::new(data), &mut side_data) {
                        Ok(rev) => props.id3.push(rev),
                        Err(e) => warn!("dff: failed to read ID3v2 metadata: {}", e),
                    }
                    for side in side_data {
                        if let MetadataSideData::Chapters(group) = side {
                            props.chapters = Some(group);
                        }
                    }
                }
                _ => (),
            }
        }
        if !have_rate {
            return decode_error("dff: missing sample rate");
        }
        if !have_channels {
            return decode_error("dff: missing channel count");
        }
        if let Some(abss) = props.abs_start {
            meta.add_abss(abss);
        }
        if let Some(lsco) = props.loudspeaker_config {
            meta.add_lsco(lsco);
        }
        Ok(Some(props))
    }

    fn validate(&self) -> Result<()> {
        if self.channel_count < 1 || u32::from(self.channel_count) > MAX_DSD_CHANNELS {
            return decode_error("dff: bad channels");
        }
        if self.sample_rate == 0 {
            return decode_error("dff: bad sample rate");
        }
        match self.compression {
            None | Some(DFF_DSD_FORM) | Some(DFF_DST_ID) => Ok(()),
            Some(_) => unsupported_error("dff: unsupported compression type"),
        }
    }

    /// Map the channel IDs (DSDIFF 3.2.2) to a channel layout.
    ///
    /// The layout is only positioned if the IDs are in the order of the positions' bits, which is
    /// the data order a `Channels::Positioned` layout implies. If the IDs do not identify the
    /// speakers (the context-specific `C000` to `C999`) the loudspeaker configuration chunk is
    /// used, if it matches the channel count.
    fn channels(&self) -> Channels {
        let count = usize::from(self.channel_count);
        if let Some(mask) = Self::positions_from_ids(&self.channel_ids, count) {
            return Channels::Positioned(mask);
        }
        let from_lsco = match (self.loudspeaker_config, count) {
            (Some(0), 2) => Some(Position::FRONT_LEFT | Position::FRONT_RIGHT),
            (Some(3), 5) => Some(
                Position::FRONT_LEFT
                    | Position::FRONT_RIGHT
                    | Position::FRONT_CENTER
                    | Position::REAR_LEFT
                    | Position::REAR_RIGHT,
            ),
            (Some(4), 6) => Some(
                Position::FRONT_LEFT
                    | Position::FRONT_RIGHT
                    | Position::FRONT_CENTER
                    | Position::LFE1
                    | Position::REAR_LEFT
                    | Position::REAR_RIGHT,
            ),
            _ => None,
        };
        // The context-specific IDs say nothing about the speakers, so the loudspeaker
        // configuration decides. Named IDs that are out of order are never overridden.
        let all_context = self.channel_ids.iter().all(is_context_channel);
        match from_lsco {
            Some(mask) if all_context => Channels::Positioned(mask),
            _ => Channels::Discrete(self.channel_count),
        }
    }

    fn positions_from_ids(ids: &[[u8; 4]], count: usize) -> Option<Position> {
        if ids.len() != count {
            return None;
        }
        let mut mask = Position::empty();
        let mut last_bit: i32 = -1;
        for id in ids {
            let pos = match id {
                b"SLFT" | b"MLFT" => Position::FRONT_LEFT,
                b"SRGT" | b"MRGT" => Position::FRONT_RIGHT,
                b"C   " => Position::FRONT_CENTER,
                b"LFE " => Position::LFE1,
                b"LS  " => Position::REAR_LEFT,
                b"RS  " => Position::REAR_RIGHT,
                b"Cs  " => Position::REAR_CENTER,
                _ => return None,
            };
            let bit = pos.bits().trailing_zeros() as i32;
            if bit <= last_bit {
                return None;
            }
            last_bit = bit;
            mask |= pos;
        }
        (mask.bits().count_ones() as usize == count).then_some(mask)
    }
}

const DFF_DST_ID: [u8; 4] = *b"DST ";

/// Whether `id` is a context-specific channel ID, `C000` to `C999`.
fn is_context_channel(id: &[u8; 4]) -> bool {
    id[0] == b'C' && id[1..].iter().all(u8::is_ascii_digit)
}

fn is_id3_chunk(id: &[u8; 4]) -> bool {
    id == b"ID3 " || id == b"id3 "
}

/// Reads an ID3v2 tag from an `ID3 ` chunk, consuming exactly `chunk_size` bytes. Errors are
/// logged and the chunk is skipped.
fn read_id3_chunk(
    source: &mut MediaSourceStream<'_>,
    chunk_size: u64,
    out: &mut Vec<MetadataRevision>,
    chapters: &mut Option<ChapterGroup>,
) {
    let mut scoped = ScopedStream::new(&mut *source, chunk_size);
    let mut side_data = Vec::<MetadataSideData>::new();
    match read_riff_id3_chunk(&mut scoped, &mut side_data) {
        Ok(rev) => out.push(rev),
        Err(e) => warn!("dff: failed to read ID3v2 metadata: {}", e),
    }
    for side in side_data {
        if let MetadataSideData::Chapters(group) = side {
            *chapters = Some(group);
        }
    }
    // Always leave the stream at the end of the chunk.
    if let Err(e) = scoped.ignore() {
        warn!("dff: failed to skip ID3 chunk remainder: {}", e);
    }
}

/// Read a chunk header. `None` is returned at the end of the stream.
fn read_chunk_header<B: ReadBytes>(reader: &mut B) -> Result<Option<([u8; 4], u64)>> {
    let id = match reader.read_quad_bytes() {
        Ok(id) => id,
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    match reader.read_be_u64() {
        Ok(size) => Ok(Some((id, size))),
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Position the stream at `target`, which is not behind the current position unless the stream is
/// seekable. Returns false if that is not possible (including a position beyond the stream's end).
fn skip_to(source: &mut MediaSourceStream<'_>, target: u64) -> bool {
    let pos = source.pos();
    if target == pos {
        return true;
    }
    if source.is_seekable() {
        source.seek(SeekFrom::Start(target)).is_ok()
    }
    else if target > pos {
        source.ignore_bytes(target - pos).is_ok()
    }
    else {
        false
    }
}

/// Where the sound data of the file is.
struct SoundDataChunk {
    /// The position of the first byte of the chunk's data.
    start: u64,
    /// The size of the chunk's data, as declared by the file.
    size: u64,
    /// The sound data is DST compressed.
    dst: Option<DstFrameInfo>,
}

/// The DST frame information (FRTE chunk, DSDIFF 3.4.1) and the position of the first frame.
#[derive(Copy, Clone)]
struct DstFrameInfo {
    num_frames: u32,
    frame_rate: u16,
    /// The position of the first chunk after the FRTE chunk.
    first_chunk: u64,
}

/// The state of a DST sound data chunk being read.
struct DstState {
    /// The position of the end of the DST chunk's data.
    end: u64,
    first_chunk: u64,
    samples_per_frame: u64,
    /// The index of the frame the next packet holds.
    next_frame: u64,
    /// A chunk header that was read ahead, and its position.
    pending: Option<([u8; 4], u64, u64)>,
    /// The positions of the headers of the `DSTF` chunks of the frames `0..index.len()`.
    index: Vec<u64>,
    /// The position where the search for further frames resumes, which is the position of the
    /// first chunk after the last frame in `index`.
    scan: u64,
}

enum Body {
    Dsd { start: u64, end: u64, pos: u64 },
    Dst(DstState),
}

pub struct DffReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    chapters: Option<ChapterGroup>,
    channel_count: u64,
    body: Body,
}

impl<'s> DffReader<'s> {
    pub fn try_new(mut source: MediaSourceStream<'s>, _options: FormatOptions) -> Result<Self> {
        if source.read_quad_bytes()? != DFF_FRM8_MAGIC {
            return decode_error("dff: bad magic");
        }
        let _form_size = source.read_be_u64()?;
        if source.read_quad_bytes()? != DFF_DSD_FORM {
            return decode_error("dff: not DSD");
        }

        let byte_len = source.byte_len();
        let mut meta = DffMeta::default();
        let mut id3_revisions = Vec::new();
        let mut id3_chapters = None;
        let mut props = None;
        let mut data: Option<SoundDataChunk> = None;
        let mut seen_version = false;

        // The sound data may be followed by further chunks (DSTI, COMT, DIIN, MANF, ID3). They are
        // only reachable if the stream is seekable.
        while let Some((id, size)) = read_chunk_header(&mut source)? {
            let body_pos = source.pos();
            let next = body_pos.saturating_add(size).saturating_add(size & 1);
            // A chunk that is larger than the stream can only be the last one.
            let body_len = match byte_len {
                Some(len) => size.min(len.saturating_sub(body_pos)),
                None => size,
            };

            match &id {
                b"FVER" => {
                    if seen_version || size < 4 {
                        warn!("dff: ignoring bad FVER chunk");
                    }
                    else {
                        seen_version = true;
                        // The first byte is the main version number (DSDIFF 3.1).
                        if source.read_u8()? != 1 {
                            return unsupported_error("dff: bad version");
                        }
                    }
                }
                b"PROP" if props.is_none() => {
                    if size > MAX_PROP_LEN {
                        return decode_error("dff: PROP chunk too large");
                    }
                    let body = source.read_boxed_slice_exact(body_len as usize)?;
                    props = SoundProperties::parse(&body, &mut meta)?;
                }
                b"DSD " | b"DST " if data.is_none() => {
                    let dst = if &id == b"DST " {
                        Some(Self::read_frame_info(&mut source, body_pos, body_len)?)
                    }
                    else {
                        None
                    };
                    data = Some(SoundDataChunk { start: body_pos, size: body_len, dst });
                    if !source.is_seekable() {
                        // The chunks after the sound data cannot be reached.
                        break;
                    }
                }
                b"COMT" | b"DIIN" | b"MANF" => {
                    if size > MAX_META_LEN {
                        warn!("dff: skipping oversized {} chunk", String::from_utf8_lossy(&id));
                    }
                    else {
                        let body = source.read_boxed_slice_exact(body_len as usize)?;
                        match &id {
                            b"COMT" => meta.parse_comt(&body),
                            b"DIIN" => meta.parse_diin(&body),
                            _ => meta.parse_manf(&body),
                        }
                    }
                }
                id if is_id3_chunk(id) => {
                    read_id3_chunk(&mut source, body_len, &mut id3_revisions, &mut id3_chapters);
                }
                _ => (),
            }

            if !skip_to(&mut source, next) {
                break;
            }
        }

        let Some(props) = props
        else {
            return decode_error("dff: missing PROP chunk");
        };
        props.validate()?;
        let Some(data) = data
        else {
            return decode_error(match props.compression {
                Some(DFF_DST_ID) => "dff: missing DST sound data chunk",
                _ => "dff: no data",
            });
        };

        let channels = u64::from(props.channel_count);
        let Some(rate) = NonZero::new(props.sample_rate)
        else {
            return decode_error("dff: bad sample rate");
        };
        let tb = TimeBase::new(NonZero::<u32>::MIN, rate);

        let mut params = AudioCodecParameters::new();
        params
            .with_sample_rate(props.sample_rate)
            .with_bits_per_sample(1)
            .with_channels(props.channels())
            .with_channel_data_layout(ChannelDataLayout::Interleaved)
            .with_bit_order(BitOrder::MsbFirst);

        let (num_frames, body) = match data.dst {
            None => {
                if props.compression == Some(DFF_DST_ID) {
                    warn!("dff: the compression type is DST but the sound data is not");
                }
                let end = data.start + data.size;
                let samples_per_channel = data.size / channels * 8;
                params
                    .for_codec(CODEC_TYPE_DSD)
                    .with_max_frames_per_packet(DSD_PACKET_BYTES * 8)
                    .with_frames_per_block(DSD_PACKET_BYTES * 8);
                (samples_per_channel, Body::Dsd { start: data.start, end, pos: data.start })
            }
            Some(info) => {
                let frame_rate = u32::from(info.frame_rate);
                if frame_rate == 0
                    || props.sample_rate % frame_rate != 0
                    || (props.sample_rate / frame_rate) % 8 != 0
                {
                    return decode_error("dff: bad DST frame rate");
                }
                let samples_per_frame = u64::from(props.sample_rate / frame_rate);
                params
                    .for_codec(CODEC_TYPE_DST)
                    .with_max_frames_per_packet(samples_per_frame)
                    .with_frames_per_block(samples_per_frame);
                let state = DstState {
                    end: data.start + data.size,
                    first_chunk: info.first_chunk,
                    samples_per_frame,
                    next_frame: 0,
                    pending: None,
                    index: Vec::new(),
                    scan: info.first_chunk,
                };
                (u64::from(info.num_frames) * samples_per_frame, Body::Dst(state))
            }
        };

        let chapters = meta
            .chapters(props.sample_rate, props.abs_start.unwrap_or_default(), tb)
            .or(props.chapters)
            .or(id3_chapters);
        let mut metadata = MetadataLog::default();
        if let Some(rev) = meta.revision() {
            metadata.push(rev);
        }
        for rev in props.id3.into_iter().chain(id3_revisions) {
            metadata.push(rev);
        }

        // Rewind to the start of the sound data if the trailing chunks were read.
        let start = match &body {
            Body::Dsd { start, .. } => *start,
            Body::Dst(d) => d.first_chunk,
        };
        if source.is_seekable() {
            source.seek(SeekFrom::Start(start))?;
        }

        let mut track = Track::new(0);
        track.time_base = Some(tb);
        track.num_frames = Some(num_frames);
        track.duration = Some(Duration::new(num_frames));
        track.with_codec_params(CodecParameters::Audio(params));

        let mut media_info = MediaInfo::new();
        media_info.time_base = Some(tb);
        media_info.duration = Some(Duration::new(num_frames));

        Ok(DffReader {
            reader: source,
            media_info,
            tracks: vec![track],
            metadata,
            chapters,
            channel_count: channels,
            body,
        })
    }

    /// Read the first chunk of a DST sound data chunk, the DST frame information (FRTE). The
    /// stream is left at the first chunk after it.
    fn read_frame_info(
        source: &mut MediaSourceStream<'_>,
        body_pos: u64,
        body_len: u64,
    ) -> Result<DstFrameInfo> {
        let Some((id, size)) = read_chunk_header(source)?
        else {
            return decode_error("dff: DST chunk without frame information");
        };
        if &id != b"FRTE" || size < 6 || size + DSDIFF_CHUNK_HEADER_LEN as u64 > body_len {
            return decode_error("dff: DST chunk without frame information");
        }
        let num_frames = source.read_be_u32()?;
        let frame_rate = source.read_be_u16()?;
        let first_chunk = body_pos + DSDIFF_CHUNK_HEADER_LEN as u64 + size + (size & 1);
        source.ignore_bytes(size - 6 + (size & 1))?;
        Ok(DstFrameInfo { num_frames, frame_rate, first_chunk })
    }

    fn next_dsd_packet(&mut self) -> Result<Option<Packet>> {
        let channels = self.channel_count;
        let Body::Dsd { start, end, pos } = &mut self.body
        else {
            unreachable!()
        };
        let aligned = (DSD_PACKET_BYTES / channels).max(1) * channels;
        let mut to_read = aligned.min(*end - *pos);
        to_read -= to_read % channels;
        if to_read == 0 {
            return Ok(None);
        }
        let buf = read_up_to(&mut self.reader, to_read as usize)?;
        // The stream may be shorter than the chunk claims. Deliver the whole samples that are
        // present, then end the stream.
        let got = buf.len() as u64 / channels * channels;
        if got < to_read {
            warn!("dff: unexpected end of data");
            *end = *pos + got;
        }
        if got == 0 {
            return Ok(None);
        }
        let buf = if got == buf.len() as u64 { buf } else { buf[..got as usize].into() };
        let pts = (*pos - *start) / channels * 8;
        *pos += got;
        Ok(Some(
            PacketBuilder::new()
                .track_id(0)
                .pts(Timestamp::new(pts as i64))
                .dur(Duration::new(got / channels * 8))
                .data(buf)
                .build(),
        ))
    }

    fn next_dst_header(&mut self) -> Result<Option<([u8; 4], u64, u64)>> {
        let Body::Dst(dst) = &mut self.body
        else {
            unreachable!()
        };
        if let Some(pending) = dst.pending.take() {
            return Ok(Some(pending));
        }
        let pos = self.reader.pos();
        if pos.saturating_add(DSDIFF_CHUNK_HEADER_LEN as u64) > dst.end {
            return Ok(None);
        }
        Ok(read_chunk_header(&mut self.reader)?.map(|(id, size)| (id, size, pos)))
    }

    fn next_dst_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            let Some((id, size, header_pos)) = self.next_dst_header()?
            else {
                return Ok(None);
            };
            let Body::Dst(dst) = &mut self.body
            else {
                unreachable!()
            };
            let body_pos = header_pos + DSDIFF_CHUNK_HEADER_LEN as u64;
            let padded = size.saturating_add(size & 1);

            if &id != b"DSTF" {
                // DSTC chunks that are not preceded by a frame, and unknown chunks.
                if body_pos.saturating_add(padded) > dst.end
                    || self.reader.ignore_bytes(padded).is_err()
                {
                    return Ok(None);
                }
                continue;
            }
            if size > MAX_DST_FRAME_LEN || body_pos.saturating_add(size) > dst.end {
                warn!("dff: truncated or oversized DST frame");
                return Ok(None);
            }

            // Reserve the header: flags and CRC (see the crate documentation).
            let mut buf = vec![0u8; 5 + size as usize];
            if read_up_to_into(&mut self.reader, &mut buf[5..])? as u64 != size {
                warn!("dff: unexpected end of data");
                return Ok(None);
            }
            if size & 1 == 1 {
                let _ = self.reader.ignore_bytes(1);
            }

            // A frame may be followed by the CRC of its decoded data.
            let after = self.reader.pos();
            if let Some((next_id, next_size)) = read_chunk_header(&mut self.reader)? {
                if &next_id == b"DSTC" && after.saturating_add(next_size + 12) <= dst.end {
                    let crc = read_up_to(&mut self.reader, next_size.min(4) as usize)?;
                    let rest = next_size - crc.len() as u64 + (next_size & 1);
                    let _ = self.reader.ignore_bytes(rest);
                    if let Ok(crc) = <[u8; 4]>::try_from(&crc[..]) {
                        buf[0] = 1;
                        buf[1..5].copy_from_slice(&crc);
                    }
                }
                else {
                    dst.pending = Some((next_id, next_size, after));
                }
            }

            let frame = dst.next_frame;
            if frame == dst.index.len() as u64 {
                dst.index.push(header_pos);
                dst.scan = body_pos + padded;
            }
            dst.next_frame += 1;
            return Ok(Some(
                PacketBuilder::new()
                    .track_id(0)
                    .pts(Timestamp::new((frame * dst.samples_per_frame) as i64))
                    .dur(Duration::new(dst.samples_per_frame))
                    .data(buf.into_boxed_slice())
                    .build(),
            ));
        }
    }

    /// Seek to the DST frame `frame`, indexing the frames on the way if needed. Returns false if
    /// the file has no such frame.
    fn seek_dst_frame(&mut self, frame: u64) -> Result<bool> {
        let Body::Dst(dst) = &mut self.body
        else {
            unreachable!()
        };
        let mut pos = dst.scan;
        while frame >= dst.index.len() as u64 {
            if pos.saturating_add(DSDIFF_CHUNK_HEADER_LEN as u64) > dst.end {
                return Ok(false);
            }
            self.reader.seek(SeekFrom::Start(pos))?;
            let Some((id, size)) = read_chunk_header(&mut self.reader)?
            else {
                return Ok(false);
            };
            let next = (pos + DSDIFF_CHUNK_HEADER_LEN as u64).saturating_add(size + (size & 1));
            if &id == b"DSTF" {
                dst.index.push(pos);
                dst.scan = next;
            }
            pos = next;
        }
        self.reader.seek(SeekFrom::Start(dst.index[frame as usize]))?;
        dst.pending = None;
        dst.next_frame = frame;
        Ok(true)
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
    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }
    fn next_packet(&mut self) -> Result<Option<Packet>> {
        match self.body {
            Body::Dsd { .. } => self.next_dsd_packet(),
            Body::Dst(_) => self.next_dst_packet(),
        }
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
                tb.calc_timestamp(time).unwrap_or(Timestamp::ZERO)
            }
        };
        if required_ts.is_negative() {
            return seek_error(SeekErrorKind::OutOfRange);
        }
        let required = required_ts.get() as u64;
        let channels = self.channel_count;

        let actual_ts = match &mut self.body {
            Body::Dsd { start, end, pos } => {
                // The timestamp is in DSD samples per channel. Each byte holds 8 samples of one
                // channel, and the channels are byte-interleaved, so a group of `channels` bytes
                // must not be split by the seek.
                let seek_pos = (required / 8)
                    .checked_mul(channels)
                    .and_then(|offset| start.checked_add(offset));
                let seek_pos = match seek_pos {
                    Some(seek_pos) if seek_pos + channels <= *end => seek_pos,
                    _ => return seek_error(SeekErrorKind::OutOfRange),
                };
                self.reader.seek(SeekFrom::Start(seek_pos))?;
                *pos = seek_pos;
                (seek_pos - *start) / channels * 8
            }
            Body::Dst(dst) => {
                let frame = required / dst.samples_per_frame;
                if !self.seek_dst_frame(frame)? {
                    return seek_error(SeekErrorKind::OutOfRange);
                }
                match &self.body {
                    Body::Dst(dst) => frame * dst.samples_per_frame,
                    Body::Dsd { .. } => unreachable!(),
                }
            }
        };
        Ok(SeekedTo { track_id: 0, required_ts, actual_ts: Timestamp::new(actual_ts as i64) })
    }
    fn into_inner<'a>(self: Box<Self>) -> MediaSourceStream<'a>
    where
        Self: 'a,
    {
        self.reader
    }
}
