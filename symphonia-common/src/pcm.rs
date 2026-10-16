// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Framing of headerless, constant-rate sample data into packets.
//!
//! Containers such as AU, VOC, IFF/8SVX and raw PCM files store audio as one contiguous run of
//! bytes that is not divided into packets. [`PcmStream`] simulates packetization over such a run
//! (PCM, G.711 and bit-packed ADPCM codes), and implements seeking within it.

use std::io::{self, Seek, SeekFrom};
use std::num::NonZero;

use symphonia_core::errors::{Error, Result, SeekErrorKind, seek_error};
use symphonia_core::formats::{SeekTo, SeekedTo, Track};
use symphonia_core::io::{MediaSource, MediaSourceStream, ReadBytes};
use symphonia_core::packet::Packet;
use symphonia_core::units::{Duration, Timestamp};

/// The maximum number of frames in a packet. Arbitrary: the same value as an MP3 frame.
pub const MAX_FRAMES_PER_PACKET: u64 = 1152;

/// The arrangement of the sample data in bytes: the data is a sequence of blocks of `block_size`
/// bytes, each holding `frames_per_block` frames (one sample of every channel).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PcmLayout {
    block_size: NonZero<u64>,
    frames_per_block: NonZero<u64>,
    blocks_per_packet: NonZero<u64>,
}

impl PcmLayout {
    /// Create a layout of blocks of `block_size` bytes holding `frames_per_block` frames. Returns
    /// `None` if either is 0.
    pub fn new(block_size: u32, frames_per_block: u32) -> Option<Self> {
        let block_size = NonZero::new(u64::from(block_size))?;
        let frames_per_block = NonZero::new(u64::from(frames_per_block))?;

        let blocks_per_packet =
            NonZero::new((MAX_FRAMES_PER_PACKET / frames_per_block.get()).max(1))?;

        Some(PcmLayout { block_size, frames_per_block, blocks_per_packet })
    }

    /// Create the layout of interleaved samples of `bits_per_sample` bits in `channels` channels.
    ///
    /// Samples that are a whole number of bytes make a block of one frame. Otherwise, the
    /// samples are packed in the bit stream, and a block of 8 frames is a whole number of bytes.
    pub fn from_sample_bits(bits_per_sample: u32, channels: u32) -> Option<Self> {
        if bits_per_sample == 0 || channels == 0 {
            return None;
        }

        if bits_per_sample % 8 == 0 {
            PcmLayout::new((bits_per_sample / 8).checked_mul(channels)?, 1)
        }
        else {
            PcmLayout::new(bits_per_sample.checked_mul(channels)?, 8)
        }
    }

    /// The size in bytes of a block.
    pub fn block_size(&self) -> u64 {
        self.block_size.get()
    }

    /// The number of frames in a block.
    pub fn frames_per_block(&self) -> u64 {
        self.frames_per_block.get()
    }

    /// The maximum number of frames in a packet.
    pub fn max_frames_per_packet(&self) -> u64 {
        self.blocks_per_packet.get() * self.frames_per_block.get()
    }

    /// The number of frames in `len` bytes of data. A partial block is not counted.
    pub fn num_frames(&self, len: u64) -> u64 {
        (len / self.block_size) * self.frames_per_block.get()
    }

    /// The offset in bytes of the block that contains the frame `ts`.
    pub fn data_pos(&self, ts: u64) -> u64 {
        (ts / self.frames_per_block) * self.block_size.get()
    }
}

/// Resolve the timestamp requested by a seek of the track `track_id`, which is `track`. The
/// timestamp must not be past the end of the track if its length is known.
pub fn required_ts(track: &Track, track_id: u32, to: SeekTo) -> Result<Timestamp> {
    let required_ts = match to {
        SeekTo::Timestamp { ts, track_id: id } if id == track_id => ts,
        SeekTo::Time { time, track_id: None } => {
            let tb = track.time_base.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;
            tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?
        }
        SeekTo::Time { time, track_id: Some(id) } if id == track_id => {
            let tb = track.time_base.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;
            tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?
        }
        _ => return seek_error(SeekErrorKind::Unseekable),
    };

    if required_ts.is_negative() {
        return seek_error(SeekErrorKind::OutOfRange);
    }

    if let Some(num_frames) = track.num_frames {
        if required_ts.get() as u64 > num_frames {
            return seek_error(SeekErrorKind::OutOfRange);
        }
    }

    Ok(required_ts)
}

/// A contiguous run of sample data in a stream.
#[derive(Copy, Clone, Debug)]
pub struct PcmStream {
    /// The arrangement of the data.
    pub layout: PcmLayout,
    /// The stream position of the first byte of the data.
    pub start: u64,
    /// The stream position after the last byte of the data, if known.
    pub end: Option<u64>,
    /// The ID of the track of the packets.
    pub track_id: u32,
}

impl PcmStream {
    /// Read the next packet of at most `layout.max_frames_per_packet()` frames from `reader`,
    /// which is positioned in the data. Returns `None` at the end of the data. A truncated stream
    /// ends at the last whole block.
    pub fn next_packet(&self, reader: &mut MediaSourceStream<'_>) -> Result<Option<Packet>> {
        let pos = reader.pos();

        if pos < self.start {
            return Ok(None);
        }

        let block_size = self.layout.block_size.get();

        let blocks_left = match self.end {
            Some(end) if pos >= end => 0,
            Some(end) => (end - pos) / block_size,
            None => u64::MAX,
        };

        let blocks = blocks_left.min(self.layout.blocks_per_packet.get());

        if blocks == 0 {
            return Ok(None);
        }

        let ts = Timestamp::try_from(self.layout.num_frames(pos - self.start))
            .map_err(|_| Error::DecodeError("pcm: timestamp overflow"))?;

        let want = (blocks * block_size) as usize;

        let mut buf = Vec::new();

        buf.try_reserve_exact(want)
            .map_err(|_| Error::IoError(io::Error::from(io::ErrorKind::OutOfMemory)))?;

        // The stream may be shorter than the data claims: reaching the end of it is not an error.
        match io::Read::read_to_end(&mut io::Read::take(&mut *reader, want as u64), &mut buf) {
            Ok(_) => (),
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => (),
            Err(err) => return Err(err.into()),
        }

        let blocks_read = buf.len() as u64 / block_size;

        if blocks_read == 0 {
            return Ok(None);
        }

        buf.truncate((blocks_read * block_size) as usize);

        let dur = Duration::new(blocks_read * self.layout.frames_per_block());

        Ok(Some(Packet::new(self.track_id, ts, dur, buf.into_boxed_slice())))
    }

    /// Seek `reader` to the packet that contains the timestamp requested by `to`.
    ///
    /// Packets begin at multiples of `layout.max_frames_per_packet()` frames, so that the packets
    /// have the same timestamps whether or not the stream was seeked.
    pub fn seek(
        &self,
        reader: &mut MediaSourceStream<'_>,
        track: &Track,
        to: SeekTo,
    ) -> Result<SeekedTo> {
        let required_ts = required_ts(track, self.track_id, to)?;

        let max_frames = Duration::new(self.layout.max_frames_per_packet());

        let actual_ts = required_ts
            .align_towards_zero(max_frames)
            .ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?;

        let seek_pos = self
            .layout
            .data_pos(actual_ts.get() as u64)
            .checked_add(self.start)
            .ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?;

        if reader.is_seekable() {
            reader.seek(SeekFrom::Start(seek_pos))?;
        }
        else {
            // Forward seeks are emulated by consuming bytes.
            let pos = reader.pos();

            if seek_pos < pos {
                return seek_error(SeekErrorKind::ForwardOnly);
            }

            reader.ignore_bytes(seek_pos - pos)?;
        }

        Ok(SeekedTo { track_id: self.track_id, actual_ts, required_ts })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_layouts() {
        let pcm16 = PcmLayout::from_sample_bits(16, 2).unwrap();
        assert_eq!((pcm16.block_size(), pcm16.frames_per_block()), (4, 1));
        assert_eq!(pcm16.max_frames_per_packet(), 1152);
        assert_eq!(pcm16.num_frames(10), 2);

        // 3-bit mono samples: 8 frames are 3 bytes.
        let g726 = PcmLayout::from_sample_bits(3, 1).unwrap();
        assert_eq!((g726.block_size(), g726.frames_per_block()), (3, 8));
        assert_eq!(g726.max_frames_per_packet(), 1152);
        assert_eq!(g726.num_frames(7), 16);
        assert_eq!(g726.data_pos(20), 6);

        assert!(PcmLayout::from_sample_bits(0, 1).is_none());
        assert!(PcmLayout::from_sample_bits(8, 0).is_none());
    }
}
