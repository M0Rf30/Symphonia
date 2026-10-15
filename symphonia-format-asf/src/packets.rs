// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Parsing of ASF data packets and reassembly of media objects.
//!
//! Section numbers refer to the Microsoft Advanced Systems Format (ASF) Specification, revision
//! 01.20.06 (5.2 Data Packet, 5.2.3 Payload Data). FFmpeg's `libavformat/asfdec_f.c`
//! (`asf_read_packet_header`, `asf_read_frame_header`, and `asf_parse_packet`) is the behavioural
//! reference, in particular for compressed payloads and for descrambling of spread audio.

use std::collections::VecDeque;
use std::io;

use log::{debug, warn};

use symphonia_core::errors::{Error, Result, decode_error};
use symphonia_core::io::{MediaSourceStream, ReadBytes};

use crate::objects::{MAX_PACKET_SIZE, Spread};

/// The largest media object that will be reassembled. Matches FFmpeg's limit of 1 << 24 bytes.
const MAX_OBJECT_SIZE: u32 = 1 << 24;

/// A reassembled, and if necessary descrambled, unit of media data from one stream.
#[derive(Debug)]
pub struct Unit {
    /// The stream number.
    pub stream: u8,
    /// The presentation time of the media object in milliseconds, including the preroll.
    pub time_ms: u32,
    /// The index of this unit within the media object, if the object was split in blocks.
    pub index: u32,
    pub data: Box<[u8]>,
    /// The position of the data packet that holds the start of the media object.
    pub pos: u64,
}

/// The media object that is being reassembled for a stream.
#[derive(Default)]
struct Assembly {
    active: bool,
    number: u32,
    size: usize,
    time_ms: u32,
    pos: u64,
    buf: Vec<u8>,
}

impl Assembly {
    fn clear(&mut self) {
        self.active = false;
        self.buf.clear();
    }
}

/// The per-stream configuration and state of the packet reader.
pub struct StreamState {
    pub number: u8,
    pub spread: Option<Spread>,
    /// If non-zero, media objects are split into blocks of this many bytes.
    pub block_len: usize,
    assembly: Assembly,
}

impl StreamState {
    pub fn new(number: u8, spread: Option<Spread>, block_len: usize) -> Self {
        StreamState { number, spread, block_len, assembly: Assembly::default() }
    }
}

/// Reads data packets and reassembles the media objects they carry.
pub struct PacketReader {
    min_size: u32,
    max_size: u32,
    data_end: Option<u64>,
    streams: Vec<StreamState>,
    /// Set when a packet header could not be parsed and the next packet cannot be located.
    lost_sync: bool,
}

/// Reads a field whose size is given by a 2-bit length type: 0 is absent, then 1, 2, or 4 bytes.
fn read_field<B: ReadBytes>(reader: &mut B, len_type: u8) -> io::Result<u32> {
    match len_type & 3 {
        0 => Ok(0),
        1 => reader.read_u8().map(u32::from),
        2 => reader.read_u16().map(u32::from),
        _ => reader.read_u32(),
    }
}

/// Returns true if the error is the end of the stream.
fn is_eof(err: &Error) -> bool {
    matches!(err, Error::IoError(e) if e.kind() == io::ErrorKind::UnexpectedEof)
}

/// Skips forward to `target`, if it is ahead.
fn skip_to(mss: &mut MediaSourceStream<'_>, target: u64) -> io::Result<()> {
    let pos = mss.pos();
    if target > pos { mss.ignore_bytes(target - pos) } else { Ok(()) }
}

impl PacketReader {
    pub fn new(
        min_size: u32,
        max_size: u32,
        data_end: Option<u64>,
        streams: Vec<StreamState>,
    ) -> Self {
        PacketReader { min_size, max_size, data_end, streams, lost_sync: false }
    }

    /// Discards partially reassembled media objects.
    pub fn reset(&mut self) {
        for stream in &mut self.streams {
            stream.assembly.clear();
        }
    }

    /// Reads one data packet and appends the media objects completed by it to `out`. Returns false
    /// at the end of the data.
    pub fn read_packet(
        &mut self,
        mss: &mut MediaSourceStream<'_>,
        out: &mut VecDeque<Unit>,
    ) -> Result<bool> {
        if self.lost_sync {
            return Ok(false);
        }

        let start = mss.pos();

        if self.data_end.is_some_and(|end| start >= end) {
            return Ok(false);
        }

        match self.parse_packet(mss, start, out) {
            Ok(()) => Ok(true),
            Err(err) if is_eof(&err) => Ok(false),
            Err(err) => Err(err),
        }
    }

    fn parse_packet(
        &mut self,
        mss: &mut MediaSourceStream<'_>,
        start: u64,
        out: &mut VecDeque<Unit>,
    ) -> Result<()> {
        // Packet header (spec 5.2.1, 5.2.2). The error correction data, if present, is first.
        let mut flags = mss.read_u8()?;

        if flags & 0x80 != 0 {
            // Error Correction Flags: the length of the error correction data is in the low 4
            // bits. FFmpeg only skips it if the reserved bits are zero.
            if flags & 0x60 == 0 {
                mss.ignore_bytes(u64::from(flags & 0x0f))?;
            }
            flags = mss.read_u8()?;
        }

        // Length Type Flags (spec 5.2.2) and Property Flags (spec 5.2.2).
        let property = mss.read_u8()?;
        let multiple = flags & 0x01 != 0;

        let packet_len = read_field(mss, flags >> 5)?;
        let _sequence = read_field(mss, flags >> 1)?;
        let padding = read_field(mss, flags >> 3)?;

        let send_len = if packet_len != 0 { packet_len } else { self.max_size };

        if send_len == 0 || send_len >= MAX_PACKET_SIZE || padding >= send_len {
            return self.lose_sync(mss, start, "asf: invalid data packet length");
        }

        // Send Time (4) and Duration (2).
        mss.ignore_bytes(6)?;

        // A packet shorter than the minimum packet size is padded to it.
        let end = start + u64::from(send_len.max(self.min_size));
        let payload_end = start + u64::from(send_len) - u64::from(padding);

        // Payload Flags (spec 5.2.3.1): the number of payloads and the type of their lengths.
        let (count, len_type) = if multiple {
            let pf = mss.read_u8()?;
            (pf & 0x3f, pf >> 6)
        }
        else {
            (1, 0)
        };

        if multiple && len_type == 0 {
            return self.lose_sync(mss, start, "asf: payload length type is 0");
        }

        if mss.pos() > payload_end {
            return self.lose_sync(mss, start, "asf: data packet header exceeds the packet");
        }

        for _ in 0..count {
            if !self.parse_payload(
                mss,
                start,
                property,
                multiple,
                len_type,
                end,
                payload_end,
                out,
            )? {
                break;
            }
        }

        skip_to(mss, end)?;
        Ok(())
    }

    /// Called if a data packet header is invalid. If every packet has the same size, the next
    /// packet is located and a decode error is returned, otherwise the end of the stream is
    /// declared since the next packet cannot be found.
    fn lose_sync(
        &mut self,
        mss: &mut MediaSourceStream<'_>,
        start: u64,
        msg: &'static str,
    ) -> Result<()> {
        if self.min_size == self.max_size && self.max_size > 0 && self.max_size < MAX_PACKET_SIZE {
            skip_to(mss, start + u64::from(self.max_size))?;
        }
        else {
            self.lost_sync = true;
        }
        decode_error(msg)
    }

    /// Parses one payload (spec 5.2.3). Returns false if the rest of the packet must be skipped.
    #[allow(clippy::too_many_arguments)]
    fn parse_payload(
        &mut self,
        mss: &mut MediaSourceStream<'_>,
        packet_pos: u64,
        property: u8,
        multiple: bool,
        len_type: u8,
        end: u64,
        payload_end: u64,
        out: &mut VecDeque<Unit>,
    ) -> Result<bool> {
        // Payload header: Stream Number and Key Frame bit (1), Media Object Number, Offset Into
        // Media Object (or the presentation time of a compressed payload), Replicated Data
        // Length, Replicated Data.
        let stream = mss.read_u8()? & 0x7f;
        let number = read_field(mss, property >> 4)?;
        let offset = read_field(mss, property >> 2)?;
        let rep_len = read_field(mss, property)?;

        let remaining = end.saturating_sub(mss.pos());
        if u64::from(rep_len) > remaining {
            warn!("asf: replicated data length {rep_len} exceeds the packet");
            return Ok(false);
        }

        // A compressed payload (replicated data length 1) has a presentation time delta. Other
        // valid payloads have an object size and presentation time.
        let mut obj_size = 0;
        let mut time_ms = 0;
        let mut delta = 0;
        let kind = match rep_len {
            1 => {
                delta = mss.read_u8()?;
                PayloadKind::Compressed
            }
            0 | 2..=7 => {
                mss.ignore_bytes(u64::from(rep_len))?;
                PayloadKind::Invalid
            }
            _ => {
                obj_size = mss.read_u32()?;
                time_ms = mss.read_u32()?;
                mss.ignore_bytes(u64::from(rep_len - 8))?;
                PayloadKind::Fragment
            }
        };

        // Payload Length (spec 5.2.3.2) if there are multiple payloads, otherwise the payload
        // extends to the padding.
        let data_len = if multiple {
            u64::from(read_field(mss, len_type)?)
        }
        else {
            payload_end.saturating_sub(mss.pos())
        };

        if data_len > end.saturating_sub(mss.pos()) {
            warn!("asf: payload length {data_len} exceeds the packet");
            return Ok(false);
        }

        let index = self.streams.iter().position(|s| s.number == stream);

        // Skip payloads that are not wanted, are not valid, or are too large to buffer.
        let skip = match (index, kind) {
            (None, _) | (_, PayloadKind::Invalid) => true,
            (_, PayloadKind::Compressed) => data_len > u64::from(MAX_OBJECT_SIZE),
            (_, PayloadKind::Fragment) => {
                obj_size == 0 || obj_size > MAX_OBJECT_SIZE || data_len > u64::from(obj_size)
            }
        };

        let Some(index) = index.filter(|_| !skip)
        else {
            if matches!(kind, PayloadKind::Invalid) {
                debug!("asf: skipping payload with replicated data length {rep_len}");
            }
            mss.ignore_bytes(data_len)?;
            return Ok(true);
        };

        let data = mss.read_boxed_slice_exact(data_len as usize)?;

        match kind {
            PayloadKind::Compressed => {
                // The offset field is the presentation time of the first sub-payload.
                self.add_compressed(index, offset, delta, &data, packet_pos, out);
            }
            _ => {
                let frag = Fragment { number, offset, obj_size, time_ms, pos: packet_pos };
                self.add_fragment(index, &frag, &data, out);
            }
        }

        Ok(true)
    }

    /// Adds the sub-payloads of a compressed payload (FFmpeg `asf_parse_packet`): each is a
    /// length byte and a complete media object. Their presentation times start at `start_ms`
    /// and advance by `delta_ms`.
    fn add_compressed(
        &mut self,
        index: usize,
        start_ms: u32,
        delta_ms: u8,
        data: &[u8],
        pos: u64,
        out: &mut VecDeque<Unit>,
    ) {
        let mut rest = data;
        let mut time_ms = start_ms;

        while let Some((&len, tail)) = rest.split_first() {
            let len = usize::from(len);
            let Some((sub, tail)) = tail.split_at_checked(len)
            else {
                warn!("asf: compressed payload is truncated");
                break;
            };
            rest = tail;

            if let Some(stream) = self.streams.get(index) {
                emit(stream, time_ms, pos, sub.to_vec(), out);
            }
            time_ms = time_ms.saturating_add(u32::from(delta_ms));
        }
    }

    /// Adds a fragment of a media object, and emits the object if it is complete.
    fn add_fragment(
        &mut self,
        index: usize,
        frag: &Fragment,
        data: &[u8],
        out: &mut VecDeque<Unit>,
    ) {
        let Some(stream) = self.streams.get_mut(index)
        else {
            return;
        };

        let size = frag.obj_size as usize;
        let offset = frag.offset as usize;

        if offset.checked_add(data.len()).is_none_or(|end| end > size) {
            warn!("asf: invalid media object fragment");
            stream.assembly.clear();
            return;
        }

        if offset == 0 {
            // The start of a new media object. Any incomplete object is lost.
            if stream.assembly.active {
                debug!("asf: dropping incomplete media object {}", stream.assembly.number);
            }
            let asm = &mut stream.assembly;
            asm.buf.clear();
            asm.active = true;
            asm.number = frag.number;
            asm.size = size;
            asm.time_ms = frag.time_ms;
            asm.pos = frag.pos;
        }
        else {
            let asm = &mut stream.assembly;
            if !asm.active || asm.number != frag.number || asm.size != size {
                // The start of the object was not seen, e.g. after a seek.
                return;
            }
            if offset < asm.buf.len() {
                // A repeated fragment.
                return;
            }
            if offset > asm.buf.len() {
                debug!("asf: media object {} has a gap, dropping it", asm.number);
                asm.clear();
                return;
            }
        }

        stream.assembly.buf.extend_from_slice(data);

        if stream.assembly.buf.len() == size {
            let asm = &mut stream.assembly;
            asm.active = false;
            let buf = std::mem::take(&mut asm.buf);
            let (time_ms, pos) = (asm.time_ms, asm.pos);
            emit(stream, time_ms, pos, buf, out);
        }
    }
}

#[derive(Copy, Clone)]
enum PayloadKind {
    Fragment,
    Compressed,
    Invalid,
}

struct Fragment {
    number: u32,
    offset: u32,
    obj_size: u32,
    time_ms: u32,
    pos: u64,
}

/// Descrambles (if the stream is scrambled) a media object and appends it to `out`, split into
/// blocks if the stream asks for it.
fn emit(stream: &StreamState, time_ms: u32, pos: u64, mut data: Vec<u8>, out: &mut VecDeque<Unit>) {
    if let Some(spread) = stream.spread {
        descramble(&mut data, spread);
    }

    let unit = |index: u32, data: &[u8]| Unit {
        stream: stream.number,
        time_ms,
        index,
        data: Box::from(data),
        pos,
    };

    let block = stream.block_len;

    if block > 0 && data.len() > block {
        if data.len() % block != 0 {
            debug!("asf: media object of {} bytes is not a multiple of {block}", data.len());
        }
        for (i, chunk) in data.chunks_exact(block).enumerate() {
            out.push_back(unit(i as u32, chunk));
        }
    }
    else {
        out.push_back(unit(0, &data));
    }
}

/// Reverses the scrambling of spread audio (spec 10.4). The object is a matrix of `span` rows of
/// virtual packets of `packet_len / chunk_len` chunks that was transmitted column by column.
///
/// Matches FFmpeg's `asf_parse_packet`: the object is left untouched if its size is not
/// `packet_len * span`.
fn descramble(data: &mut Vec<u8>, spread: Spread) {
    let span = usize::from(spread.span);
    let packet_len = usize::from(spread.packet_len);
    let chunk = usize::from(spread.chunk_len);

    if span <= 1 || chunk == 0 || data.len() != packet_len * span {
        debug!("asf: scrambled object has an unexpected size of {}", data.len());
        return;
    }

    let mut out = Vec::with_capacity(data.len());

    for offset in (0..data.len()).step_by(chunk) {
        let off = offset / chunk;
        let row = off / span;
        let col = off % span;
        let idx = row + col * packet_len / chunk;

        match data.get(idx * chunk..(idx + 1) * chunk) {
            Some(src) => out.extend_from_slice(src),
            None => return,
        }
    }

    *data = out;
}
