// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Parsing of the ASF Header Object and of the objects it contains.
//!
//! Section numbers refer to the Microsoft Advanced Systems Format (ASF) Specification, revision
//! 01.20.06. FFmpeg's `libavformat/asfdec_f.c` is the behavioural reference.

use std::io::SeekFrom;

use log::{debug, warn};

use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::io::{MediaSource, MediaSourceStream, ReadBytes};
use symphonia_core::meta::{MetadataBuilder, MetadataRevision};

use crate::meta;

/// A GUID in its on-disk (mixed-endian) byte order.
pub type Guid = [u8; 16];

/// Builds the on-disk byte representation of a GUID from its canonical `D1-D2-D3-D4` fields.
const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> Guid {
    let a = d1.to_le_bytes();
    let b = d2.to_le_bytes();
    let c = d3.to_le_bytes();
    [
        a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d4[0], d4[1], d4[2], d4[3], d4[4], d4[5],
        d4[6], d4[7],
    ]
}

/// ASF_Header_Object (spec 3.1).
pub const HEADER_OBJECT: Guid =
    guid(0x75b2_2630, 0x668e, 0x11cf, [0xa6, 0xd9, 0x00, 0xaa, 0x00, 0x62, 0xce, 0x6c]);
/// ASF_Data_Object (spec 5.1).
const DATA_OBJECT: Guid =
    guid(0x75b2_2636, 0x668e, 0x11cf, [0xa6, 0xd9, 0x00, 0xaa, 0x00, 0x62, 0xce, 0x6c]);
/// ASF_Simple_Index_Object (spec 6.1).
const SIMPLE_INDEX_OBJECT: Guid =
    guid(0x33000890, 0xe5b1, 0x11cf, [0x89, 0xf4, 0x00, 0xa0, 0xc9, 0x03, 0x49, 0xcb]);
/// ASF_File_Properties_Object (spec 3.2).
const FILE_PROPERTIES_OBJECT: Guid =
    guid(0x8cab_dca1, 0xa947, 0x11cf, [0x8e, 0xe4, 0x00, 0xc0, 0x0c, 0x20, 0x53, 0x65]);
/// ASF_Stream_Properties_Object (spec 3.3).
const STREAM_PROPERTIES_OBJECT: Guid =
    guid(0xb7dc_0791, 0xa9b7, 0x11cf, [0x8e, 0xe6, 0x00, 0xc0, 0x0c, 0x20, 0x53, 0x65]);
/// ASF_Header_Extension_Object (spec 3.4).
const HEADER_EXTENSION_OBJECT: Guid =
    guid(0x5fbf_03b5, 0xa92e, 0x11cf, [0x8e, 0xe3, 0x00, 0xc0, 0x0c, 0x20, 0x53, 0x65]);
/// ASF_Content_Description_Object (spec 3.10).
const CONTENT_DESCRIPTION_OBJECT: Guid =
    guid(0x75b2_2633, 0x668e, 0x11cf, [0xa6, 0xd9, 0x00, 0xaa, 0x00, 0x62, 0xce, 0x6c]);
/// ASF_Extended_Content_Description_Object (spec 3.11).
const EXTENDED_CONTENT_DESCRIPTION_OBJECT: Guid =
    guid(0xd2d0_a440, 0xe307, 0x11d2, [0x97, 0xf0, 0x00, 0xa0, 0xc9, 0x5e, 0xa8, 0x50]);
/// ASF_Content_Encryption_Object (spec 3.13).
const CONTENT_ENCRYPTION_OBJECT: Guid =
    guid(0x2211_b3fb, 0xbd23, 0x11d2, [0xb4, 0xb7, 0x00, 0xa0, 0xc9, 0x55, 0xfc, 0x6e]);
/// ASF_Extended_Content_Encryption_Object (spec 3.14).
const EXTENDED_CONTENT_ENCRYPTION_OBJECT: Guid =
    guid(0x298a_e614, 0x2622, 0x4c17, [0xb9, 0x35, 0xda, 0xe0, 0x7e, 0xe9, 0x28, 0x9c]);
/// ASF_Metadata_Object (spec 4.7, in the Header Extension Object).
const METADATA_OBJECT: Guid =
    guid(0xc5f8_cbea, 0x5baf, 0x4877, [0x84, 0x67, 0xaa, 0x8c, 0x44, 0xfa, 0x4c, 0xca]);
/// ASF_Metadata_Library_Object (spec 4.8, in the Header Extension Object).
const METADATA_LIBRARY_OBJECT: Guid =
    guid(0x4423_1c94, 0x9498, 0x49d1, [0xa1, 0x41, 0x1d, 0x13, 0x4e, 0x45, 0x70, 0x54]);
/// ASF_Audio_Media (spec 10.1).
const AUDIO_MEDIA: Guid =
    guid(0xf869_9e40, 0x5b4d, 0x11cf, [0xa8, 0xfd, 0x00, 0x80, 0x5f, 0x5c, 0x44, 0x2b]);
/// ASF_Audio_Spread (spec 10.4), the error correction type of scrambled audio streams.
const AUDIO_SPREAD: Guid =
    guid(0xbfc3_cd50, 0x618f, 0x11cf, [0x8b, 0xb2, 0x00, 0xaa, 0x00, 0xb4, 0xe2, 0x20]);

/// The size of an object header: a GUID and a 64-bit size.
const OBJECT_HEADER_LEN: u64 = 24;
/// The size of the Data Object fields following the object header (spec 5.1): File ID (16),
/// Total Data Packets (8), and Reserved (2).
const DATA_OBJECT_FIELDS_LEN: u64 = 26;
/// The largest object body that will be buffered in memory. Larger objects are skipped.
const MAX_OBJECT_BODY_LEN: u64 = 16 * 1024 * 1024;
/// The largest number of objects that will be visited in the Header Object.
const MAX_HEADER_OBJECTS: usize = 1 << 14;
/// The largest number of streams that will be tracked.
const MAX_STREAMS: usize = 128;
/// The largest data packet size accepted, in bytes. Matches FFmpeg's limit.
pub const MAX_PACKET_SIZE: u32 = 1 << 29;

/// A bounds-checked little-endian reader over an in-memory object body.
pub(crate) struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Cursor { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(len);
        match end.and_then(|end| self.buf.get(self.pos..end).map(|slice| (end, slice))) {
            Some((end, slice)) => {
                self.pos = end;
                Ok(slice)
            }
            None => decode_error("asf: object is truncated"),
        }
    }

    pub fn skip(&mut self, len: usize) -> Result<()> {
        self.take(len).map(|_| ())
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }
}

/// The File Properties Object (spec 3.2).
#[derive(Copy, Clone, Debug)]
pub struct FileProperties {
    /// The play duration in 100 ns units, including the preroll.
    pub play_duration: u64,
    /// The preroll in milliseconds.
    pub preroll_ms: u64,
    /// The flags: bit 0 is Broadcast, bit 1 is Seekable.
    pub flags: u32,
    /// The minimum data packet size in bytes.
    pub min_packet_size: u32,
    /// The maximum data packet size in bytes.
    pub max_packet_size: u32,
}

impl FileProperties {
    pub fn is_broadcast(&self) -> bool {
        self.flags & 1 != 0
    }

    /// The size of every data packet, if all data packets have the same size.
    pub fn fixed_packet_size(&self) -> Option<u32> {
        (self.min_packet_size == self.max_packet_size
            && self.max_packet_size > 0
            && self.max_packet_size < MAX_PACKET_SIZE)
            .then_some(self.max_packet_size)
    }

    fn parse(body: &[u8]) -> Result<Self> {
        let mut c = Cursor::new(body);
        // File ID (16), File Size (8), Creation Date (8), Data Packets Count (8).
        c.skip(16 + 8 + 8 + 8)?;
        let play_duration = c.u64()?;
        // Send Duration.
        c.skip(8)?;
        let preroll_ms = c.u64()?;
        let flags = c.u32()?;
        let min_packet_size = c.u32()?;
        let max_packet_size = c.u32()?;
        Ok(FileProperties { play_duration, preroll_ms, flags, min_packet_size, max_packet_size })
    }
}

/// The descrambling parameters of an Audio Spread error correction descriptor (spec 10.4).
#[derive(Copy, Clone, Debug)]
pub struct Spread {
    /// The number of virtual packets in a scrambled block (Span).
    pub span: u8,
    /// The size of a virtual packet in bytes.
    pub packet_len: u16,
    /// The size of a virtual chunk in bytes.
    pub chunk_len: u16,
}

/// An audio stream described by a Stream Properties Object (spec 3.3, 10.1).
#[derive(Clone, Debug)]
pub struct AudioStream {
    /// The stream number, 1 to 127.
    pub number: u8,
    /// The effective WAVE format tag (the sub-format tag if the format is `WAVE_FORMAT_EXTENSIBLE`).
    pub format_tag: u16,
    pub channels: u16,
    pub sample_rate: u32,
    pub avg_bytes_per_sec: u32,
    pub block_align: u16,
    pub bits_per_sample: u16,
    /// The entire type-specific data, which is a `WAVEFORMATEX` structure.
    pub wave_format: Box<[u8]>,
    /// The channel mask of a `WAVE_FORMAT_EXTENSIBLE` format.
    pub channel_mask: Option<u32>,
    /// The descrambling parameters, if the stream is scrambled.
    pub spread: Option<Spread>,
}

/// The result of reading the Header Object and the Data Object header.
pub struct Header {
    pub file: Option<FileProperties>,
    pub streams: Vec<AudioStream>,
    pub metadata: Option<MetadataRevision>,
    /// The position of the first data packet.
    pub data_pos: u64,
    /// The position one past the last data packet, if known.
    pub data_end: Option<u64>,
}

/// Reads a GUID.
pub fn read_guid<B: ReadBytes>(reader: &mut B) -> Result<Guid> {
    let mut g = [0u8; 16];
    reader.read_buf_exact(&mut g)?;
    Ok(g)
}

/// Reads the Header Object and every object up to and including the header of the Data Object.
/// The stream is left at the first data packet.
pub fn read_header(mss: &mut MediaSourceStream<'_>) -> Result<Header> {
    if read_guid(mss)? != HEADER_OBJECT {
        return decode_error("asf: missing header object");
    }

    let header_size = mss.read_u64()?;
    if header_size < OBJECT_HEADER_LEN + 6 {
        return decode_error("asf: invalid header object size");
    }
    // Number of Header Objects (4) and two reserved bytes. The count is not trusted: as in FFmpeg,
    // objects are read until the Data Object is found.
    mss.ignore_bytes(6)?;

    let stream_len = mss.byte_len();

    let mut file = None;
    let mut streams: Vec<AudioStream> = Vec::new();
    let mut meta = MetadataBuilder::new(meta::ASF_METADATA_INFO);
    let mut has_meta = false;

    for _ in 0..MAX_HEADER_OBJECTS {
        let obj_pos = mss.pos();
        let id = read_guid(mss)?;
        let size = mss.read_u64()?;

        if id == DATA_OBJECT {
            // File ID (16), Total Data Packets (8), Reserved (2).
            mss.ignore_bytes(16)?;
            let total_packets = mss.read_u64()?;
            mss.ignore_bytes(2)?;

            let data_pos = mss.pos();
            let data_end = data_end(obj_pos, size, total_packets, file.as_ref(), stream_len);
            let metadata = has_meta.then(|| meta.build());

            return Ok(Header { file, streams, metadata, data_pos, data_end });
        }

        if size < OBJECT_HEADER_LEN {
            return decode_error("asf: invalid object size");
        }

        let body_len = size - OBJECT_HEADER_LEN;

        if let Some(len) = stream_len {
            if body_len > len.saturating_sub(mss.pos()) {
                return decode_error("asf: object extends past the end of the stream");
            }
        }

        if id == CONTENT_ENCRYPTION_OBJECT || id == EXTENDED_CONTENT_ENCRYPTION_OBJECT {
            return unsupported_error("asf: DRM protected content");
        }

        let wanted = id == FILE_PROPERTIES_OBJECT
            || id == STREAM_PROPERTIES_OBJECT
            || id == HEADER_EXTENSION_OBJECT
            || id == CONTENT_DESCRIPTION_OBJECT
            || id == EXTENDED_CONTENT_DESCRIPTION_OBJECT;

        if !wanted || body_len > MAX_OBJECT_BODY_LEN {
            if wanted {
                warn!("asf: skipping oversized object of {body_len} bytes");
            }
            mss.ignore_bytes(body_len)?;
            continue;
        }

        let body = mss.read_boxed_slice_exact(body_len as usize)?;

        if id == FILE_PROPERTIES_OBJECT {
            match FileProperties::parse(&body) {
                Ok(props) => {
                    file.get_or_insert(props);
                }
                Err(err) => warn!("asf: ignoring invalid file properties object: {err}"),
            }
        }
        else if id == STREAM_PROPERTIES_OBJECT {
            if let Some(stream) = parse_stream_properties(&body)? {
                if streams.iter().any(|s| s.number == stream.number) {
                    warn!("asf: ignoring duplicate stream {}", stream.number);
                }
                else if streams.len() < MAX_STREAMS {
                    streams.push(stream);
                }
            }
        }
        else if id == CONTENT_DESCRIPTION_OBJECT {
            has_meta |= meta::read_content_description(&body, &mut meta)
                .map_err(|err| warn!("asf: ignoring invalid content description: {err}"))
                .is_ok();
        }
        else if id == EXTENDED_CONTENT_DESCRIPTION_OBJECT {
            has_meta |= meta::read_extended_content_description(&body, &mut meta)
                .map_err(|err| warn!("asf: ignoring invalid extended content description: {err}"))
                .is_ok();
        }
        else if id == HEADER_EXTENSION_OBJECT {
            has_meta |= parse_header_extension(&body, &mut meta)?;
        }
    }

    decode_error("asf: too many header objects")
}

/// Computes the position one past the last data packet, if it can be determined.
fn data_end(
    obj_pos: u64,
    size: u64,
    total_packets: u64,
    file: Option<&FileProperties>,
    stream_len: Option<u64>,
) -> Option<u64> {
    // Live (broadcast) streams have no meaningful sizes.
    if file.is_some_and(FileProperties::is_broadcast) {
        return None;
    }

    let mut end = if size >= OBJECT_HEADER_LEN + DATA_OBJECT_FIELDS_LEN {
        obj_pos.checked_add(size)
    }
    else {
        None
    };

    // The packet count bounds the data if every packet has the same size.
    let by_count = file.and_then(FileProperties::fixed_packet_size).and_then(|packet_size| {
        let data_start = obj_pos + OBJECT_HEADER_LEN + DATA_OBJECT_FIELDS_LEN;
        total_packets
            .checked_mul(u64::from(packet_size))
            .and_then(|len| data_start.checked_add(len))
            .filter(|_| total_packets > 0)
    });

    if let Some(by_count) = by_count {
        end = Some(end.map_or(by_count, |end| end.min(by_count)));
    }

    // Truncated files.
    match (end, stream_len) {
        (Some(e), Some(len)) => Some(e.min(len)),
        (e, _) => e,
    }
}

/// Parses a Stream Properties Object body (spec 3.3). Returns `None` for streams that are not
/// supported audio streams.
fn parse_stream_properties(body: &[u8]) -> Result<Option<AudioStream>> {
    let mut c = Cursor::new(body);

    let stream_type = c.take(16)?;
    let ec_type = c.take(16)?;
    // Time Offset.
    c.skip(8)?;
    let ts_len = c.u32()? as usize;
    let ec_len = c.u32()? as usize;
    let flags = c.u16()?;
    // Reserved.
    c.skip(4)?;

    let number = (flags & 0x7f) as u8;

    // The Encrypted Content Flag.
    if flags & 0x8000 != 0 {
        return unsupported_error("asf: DRM protected content");
    }

    if stream_type != AUDIO_MEDIA {
        debug!("asf: ignoring non-audio stream {number}");
        return Ok(None);
    }

    if number == 0 {
        warn!("asf: ignoring audio stream with invalid stream number 0");
        return Ok(None);
    }

    let ts_data = c.take(ts_len)?;
    let ec_data = c.take(ec_len)?;

    let mut t = Cursor::new(ts_data);
    // WAVEFORMATEX (16 bytes without cbSize).
    let mut format_tag = t.u16()?;
    let channels = t.u16()?;
    let sample_rate = t.u32()?;
    let avg_bytes_per_sec = t.u32()?;
    let block_align = t.u16()?;
    let bits_per_sample = t.u16()?;

    // WAVE_FORMAT_EXTENSIBLE: cbSize (2), valid bits (2), channel mask (4), SubFormat GUID (16),
    // of which the first two bytes are the actual format tag.
    let mut channel_mask = None;
    if format_tag == 0xfffe && ts_data.len() >= 18 + 22 {
        let mut e = Cursor::new(ts_data.get(18..).unwrap_or_default());
        // cbSize (2), valid bits per sample (2), channel mask (4), sub-format GUID (16).
        e.skip(4)?;
        channel_mask = Some(e.u32()?).filter(|&mask| mask != 0);
        format_tag = e.u16()?;
    }

    let spread = if ec_type == AUDIO_SPREAD { parse_spread(ec_data) } else { None };

    Ok(Some(AudioStream {
        number,
        format_tag,
        channels,
        sample_rate,
        avg_bytes_per_sec,
        block_align,
        bits_per_sample,
        wave_format: Box::from(ts_data),
        channel_mask,
        spread,
    }))
}

/// Parses an Audio Spread error correction descriptor (spec 10.4): Span (1), Virtual Packet Length
/// (2), Virtual Chunk Length (2), Silence Data Length (2), Silence Data.
///
/// As in FFmpeg, descrambling is only enabled if the parameters are consistent.
fn parse_spread(ec_data: &[u8]) -> Option<Spread> {
    let mut c = Cursor::new(ec_data);
    let span = c.u8().ok()?;
    let packet_len = c.u16().ok()?;
    let chunk_len = c.u16().ok()?;

    if span <= 1 || chunk_len == 0 || packet_len / chunk_len <= 1 || packet_len % chunk_len != 0 {
        return None;
    }

    Some(Spread { span, packet_len, chunk_len })
}

/// Parses the objects nested in a Header Extension Object body (spec 3.4). Returns true if any
/// tags were added.
fn parse_header_extension(body: &[u8], meta: &mut MetadataBuilder) -> Result<bool> {
    let mut c = Cursor::new(body);
    // Reserved Field 1 (GUID), Reserved Field 2 (2), Header Extension Data Size (4).
    c.skip(16 + 2)?;
    let data_size = c.u32()? as usize;
    let data = c.take(data_size.min(c.remaining()))?;

    let mut found = false;
    let mut c = Cursor::new(data);
    while c.remaining() >= OBJECT_HEADER_LEN as usize {
        let id = c.take(16)?;
        let size = c.u64()?;
        let Some(body_len) =
            size.checked_sub(OBJECT_HEADER_LEN).and_then(|len| usize::try_from(len).ok())
        else {
            break;
        };
        let Ok(obj) = c.take(body_len)
        else {
            break;
        };

        if id == METADATA_OBJECT || id == METADATA_LIBRARY_OBJECT {
            found |= meta::read_metadata_object(obj, meta)
                .map_err(|err| warn!("asf: ignoring invalid metadata object: {err}"))
                .is_ok();
        }
    }

    Ok(found)
}

/// Seeks to the start of `pos` and reads the objects following the Data Object looking for the
/// Simple Index Object (spec 6.1). Returns the index time interval in 100 ns units and the packet
/// numbers of the entries.
pub fn read_simple_index(
    mss: &mut MediaSourceStream<'_>,
    mut pos: u64,
    stream_len: u64,
) -> Result<Option<(u64, Vec<u32>)>> {
    use std::io::Seek;

    // Visit a bounded number of objects.
    for _ in 0..MAX_HEADER_OBJECTS {
        if pos.saturating_add(OBJECT_HEADER_LEN) > stream_len {
            return Ok(None);
        }

        mss.seek(SeekFrom::Start(pos))?;
        let id = read_guid(mss)?;
        let size = mss.read_u64()?;

        if size < OBJECT_HEADER_LEN {
            return Ok(None);
        }

        if id == SIMPLE_INDEX_OBJECT {
            // File ID (16), Index Entry Time Interval (8), Max Packet Count (4), Index Entries
            // Count (4).
            mss.ignore_bytes(16)?;
            let interval = mss.read_u64()?;
            mss.ignore_bytes(4)?;
            let count = u64::from(mss.read_u32()?);

            let body_len = size - OBJECT_HEADER_LEN;
            // The entries are 6 bytes each: Packet Number (4) and Packet Count (2).
            if count.saturating_mul(6) + 32 > body_len || pos.saturating_add(size) > stream_len {
                return Ok(None);
            }

            let mut entries = Vec::new();
            for _ in 0..count {
                entries.push(mss.read_u32()?);
                mss.ignore_bytes(2)?;
            }

            return Ok(Some((interval, entries)));
        }

        pos = match pos.checked_add(size) {
            Some(next) => next,
            None => return Ok(None),
        };
    }

    Ok(None)
}
