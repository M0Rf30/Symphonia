// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CRI ADX demuxer.
//!
//! The layout follows FFmpeg's `libavformat/adxdec.c` and `libavcodec/adxdec.c`:
//!
//! ```text
//! 0x00  u16   0x8000
//! 0x02  u16   offset of the copyright string + 2 (the header is `offset + 4` bytes long)
//! 0x04  u8    encoding (3: standard ADX; 2 and 4 are encoder-only variants; 0x10 and 0x11: AHX)
//! 0x05  u8    block size (18)
//! 0x06  u8    bits per sample (4)
//! 0x07  u8    channel count
//! 0x08  u32   sample rate
//! 0x0c  u32   total number of samples (per channel)
//! 0x10  u16   high-pass cutoff frequency
//! 0x12  u8    version, u8 revision (flags in the encrypted versions)
//! ...         loop information, (c)CRI
//! ```
//!
//! The header is followed by the 18 byte blocks of the channels, one per channel in turn. A group
//! of 32 samples per channel is therefore `18 * channels` bytes. A block whose first bit is set
//! marks the end of the audio (it holds the position of the end of the stream); anything after it
//! is not audio. All fields are big-endian.

use std::io::{Seek, SeekFrom};

use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_ADPCM_ADX;
use symphonia_core::common::FourCc;
use symphonia_core::errors::{Result, SeekErrorKind, decode_error, seek_error, unsupported_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::support_format;
use symphonia_core::units::{Duration, Timestamp};

use crate::common::{channels_for, frame_time_base, read_up_to, required_timestamp, unseekable};

const ADX_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"ADX ")),
    short_name: "adx",
    long_name: "CRI ADX",
};

/// The size in bytes of the block of one channel.
const BLOCK_LEN: u64 = 18;

/// The number of samples per channel in a block.
const BLOCK_SAMPLES: u64 = 32;

/// The largest number of channels (`MAX_CHANNELS` of `libavcodec/adx.h`).
const MAX_CHANNELS: usize = 6;

/// The number of block groups in a packet (`BLOCK_GROUPS` of `libavformat/adxdec.c`).
const GROUPS_PER_PACKET: u64 = 128;

/// The number of block groups decoded and discarded before the requested position of an accurate
/// seek. The state of the filter of the codec, the two previous samples of each channel, is not
/// stored in the stream; its response to a wrong state decays with the number of samples.
const SEEK_PREROLL_GROUPS: u64 = 8;

/// The shortest header, the fields up to the cutoff frequency (`libavcodec/adxdec.c`).
const MIN_HEADER_LEN: u64 = 24;

/// The copyright string that ends the header.
const COPYRIGHT: &[u8; 6] = b"(c)CRI";

/// The fields of a stream header.
struct AdxHeader {
    /// The whole header, which the decoder takes as its extra data.
    bytes: Box<[u8]>,
    channels: usize,
    sample_rate: u32,
    /// The total number of samples per channel, which is 0 if it was not written.
    total_samples: u32,
}

impl AdxHeader {
    fn read<B: ReadBytes>(src: &mut B) -> Result<Self> {
        let mut start = [0u8; 4];
        src.read_buf_exact(&mut start)?;

        if u16::from_be_bytes([start[0], start[1]]) != 0x8000 {
            return unsupported_error("adx: not an ADX stream");
        }

        let header_len = u64::from(u16::from_be_bytes([start[2], start[3]])) + 4;
        if header_len < MIN_HEADER_LEN {
            return decode_error("adx: invalid header size");
        }

        let mut bytes = vec![0u8; header_len as usize];
        bytes[..4].copy_from_slice(&start);
        src.read_buf_exact(&mut bytes[4..])?;

        if &bytes[bytes.len() - COPYRIGHT.len()..] != COPYRIGHT {
            return decode_error("adx: missing copyright string");
        }

        match bytes[4] {
            // The fixed coefficient encoding is the only one with a decoder in FFmpeg. The encodings
            // 2 and 4 (fixed and exponential scale), see `adx.c` of vgmstream, are only produced by
            // some encoders.
            3 => (),
            0x10 | 0x11 => return unsupported_error("adx: AHX streams are unsupported"),
            _ => return unsupported_error("adx: unsupported encoding"),
        }

        if u64::from(bytes[5]) != BLOCK_LEN || bytes[6] != 4 {
            return unsupported_error("adx: unsupported block layout");
        }

        // The encrypted versions 4.8 and 4.9 (see `adx.c` of vgmstream).
        if matches!(u16::from_be_bytes([bytes[0x12], bytes[0x13]]), 0x0408 | 0x0409) {
            return unsupported_error("adx: encrypted streams are unsupported");
        }

        let channels = usize::from(bytes[7]);
        if channels == 0 || channels > MAX_CHANNELS {
            return decode_error("adx: invalid number of channels");
        }

        let sample_rate = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let total_samples = u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);

        // The same limit as FFmpeg: it keeps `sample_rate * channels * 18 * 8` below 2^31.
        if sample_rate < 1 || u64::from(sample_rate) > i32::MAX as u64 / (channels as u64 * 144) {
            return decode_error("adx: invalid sample rate");
        }

        Ok(AdxHeader { bytes: bytes.into_boxed_slice(), channels, sample_rate, total_samples })
    }
}

/// Returns true if a block group holds an end of audio block.
fn is_end_group(group: &[u8]) -> bool {
    group.chunks_exact(BLOCK_LEN as usize).any(|block| block[0] & 0x80 != 0)
}

/// CRI ADX reader.
pub struct AdxReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    /// The position of the first block group.
    data_start: u64,
    /// The size in bytes of a block group.
    group_len: u64,
    /// The index of the next block group to read.
    next_group: u64,
    /// The number of block groups of audio, if it is known.
    num_groups: Option<u64>,
    /// The number of valid frames, if it is known.
    num_frames: Option<u64>,
    /// Set once the end of the audio was read.
    eof: bool,
}

impl<'s> AdxReader<'s> {
    pub fn try_new(mut source: MediaSourceStream<'s>, _options: FormatOptions) -> Result<Self> {
        let base = source.pos();
        let header = AdxHeader::read(&mut source)?;
        let data_start = base + header.bytes.len() as u64;
        let group_len = BLOCK_LEN * header.channels as u64;

        // Find the number of block groups of audio. A final group with an end of audio block
        // is not audio. The end of audio block in the middle of the stream is found when it is
        // read.
        let mut num_groups =
            source.byte_len().map(|len| len.saturating_sub(data_start) / group_len);

        if let Some(groups) = num_groups.filter(|&g| g > 0) {
            if source.is_seekable() {
                let mut last = vec![0u8; group_len as usize];
                source.seek(SeekFrom::Start(data_start + (groups - 1) * group_len))?;
                source.read_buf_exact(&mut last)?;
                source.seek(SeekFrom::Start(data_start))?;

                if is_end_group(&last) {
                    num_groups = Some(groups - 1);
                }
            }
        }

        // A total of 0 was written by an encoder that did not know the length, and a total that
        // exceeds the data is damaged.
        let available = num_groups.map(|groups| groups * BLOCK_SAMPLES);
        let total = u64::from(header.total_samples);
        let num_frames = match (total, available) {
            (0, available) => available,
            (total, Some(available)) => Some(total.min(available)),
            (total, None) => Some(total),
        };

        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_ADPCM_ADX)
            .with_sample_rate(header.sample_rate)
            .with_bits_per_coded_sample(4)
            .with_channels(channels_for(header.channels))
            .with_max_frames_per_packet(GROUPS_PER_PACKET * BLOCK_SAMPLES)
            .with_extra_data(header.bytes);

        let time_base = frame_time_base(header.sample_rate);

        let mut track = Track::new(0);
        track.time_base = time_base;
        track.num_frames = num_frames;
        track.duration = num_frames.map(Duration::new);
        // The frames after the total are the padding of the last block group.
        track.padding = match (num_frames, available) {
            (Some(frames), Some(available)) if available > frames => {
                u32::try_from(available - frames).ok()
            }
            _ => None,
        };
        track.with_codec_params(CodecParameters::Audio(params));

        let mut media_info = MediaInfo::new();
        media_info.time_base = time_base;
        media_info.duration = num_frames.map(Duration::new);

        Ok(AdxReader {
            reader: source,
            media_info,
            tracks: vec![track],
            metadata: MetadataLog::default(),
            data_start,
            group_len,
            next_group: 0,
            num_groups,
            num_frames,
            eof: false,
        })
    }
}

impl Scoreable for AdxReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        let mut start = [0u8; 8];

        if src.read_buf_exact(&mut start).is_err() || start[..2] != [0x80, 0x00] {
            return Ok(Score::Unsupported);
        }

        // The copyright string ends the header (`adx_probe` of `libavformat/adxdec.c`).
        let header_len = u64::from(u16::from_be_bytes([start[2], start[3]])) + 4;
        if header_len < MIN_HEADER_LEN {
            return Ok(Score::Unsupported);
        }

        let mut tail = [0u8; COPYRIGHT.len()];
        let ahead = header_len - start.len() as u64 - tail.len() as u64;

        if src.ignore_bytes(ahead).is_err() || src.read_buf_exact(&mut tail).is_err() {
            return Ok(Score::Unsupported);
        }

        Ok(if &tail == COPYRIGHT { Score::Supported(255) } else { Score::Unsupported })
    }
}

impl ProbeableFormat<'_> for AdxReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(AdxReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(ADX_FORMAT_INFO, &["adx"], &["audio/x-adx"], &[b"\x80\x00"])]
    }
}

impl FormatReader for AdxReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &ADX_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        if self.eof {
            return Ok(None);
        }

        let groups = match self.num_groups {
            Some(total) => total.saturating_sub(self.next_group).min(GROUPS_PER_PACKET),
            None => GROUPS_PER_PACKET,
        };

        let mut buf = vec![0u8; (groups * self.group_len) as usize];
        let len = read_up_to(&mut self.reader, &mut buf)?;

        // A partial group at the end of the stream is dropped.
        let mut groups = len as u64 / self.group_len;

        // The audio ends at the first group with an end of audio block.
        if let Some(end) = buf[..(groups * self.group_len) as usize]
            .chunks_exact(self.group_len as usize)
            .position(is_end_group)
        {
            groups = end as u64;
            self.eof = true;
        }

        if groups < GROUPS_PER_PACKET && self.num_groups.is_none() {
            // The length is unknown and the source is exhausted.
            self.eof = true;
        }

        if groups == 0 {
            self.eof = true;
            return Ok(None);
        }

        buf.truncate((groups * self.group_len) as usize);

        let pts = self.next_group * BLOCK_SAMPLES;
        let block_frames = groups * BLOCK_SAMPLES;
        self.next_group += groups;

        // The frames after the total number of samples are padding.
        let dur = match self.num_frames {
            Some(total) => total.saturating_sub(pts).min(block_frames),
            None => block_frames,
        };

        Ok(Some(
            PacketBuilder::new()
                .track_id(0)
                .pts(Timestamp::new(pts as i64))
                .dur(Duration::new(dur))
                .trim_end(Duration::new(block_frames - dur))
                .data(buf)
                .build(),
        ))
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn seek(&mut self, mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        if !self.reader.is_seekable() {
            return unseekable();
        }

        let Some(time_base) = self.media_info.time_base
        else {
            return unseekable();
        };

        let required_ts = required_timestamp(to, time_base)?;

        if self.num_frames.is_some_and(|frames| required_ts >= frames) {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        let mut group = required_ts / BLOCK_SAMPLES;

        // The filter of the codec starts from silence; decode some groups before the required
        // position so that its state converges.
        if mode == SeekMode::Accurate {
            group = group.saturating_sub(SEEK_PREROLL_GROUPS);
        }

        self.reader.seek(SeekFrom::Start(self.data_start + group * self.group_len))?;
        self.next_group = group;
        self.eof = false;

        Ok(SeekedTo {
            track_id: 0,
            required_ts: Timestamp::new(required_ts as i64),
            actual_ts: Timestamp::new((group * BLOCK_SAMPLES) as i64),
        })
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

    use symphonia_core::errors::Error;

    use super::*;

    /// A stereo or mono ADX stream of `groups` groups with the block scale `scale`, followed by
    /// an end of audio group if `end` is set.
    fn stream(channels: u8, total: u32, groups: u32, end: bool) -> Vec<u8> {
        let mut h = vec![0u8; 0x24];
        h[0] = 0x80;
        h[3] = 0x20;
        h[4] = 3;
        h[5] = 18;
        h[6] = 4;
        h[7] = channels;
        h[8..12].copy_from_slice(&44100u32.to_be_bytes());
        h[12..16].copy_from_slice(&total.to_be_bytes());
        h[16..18].copy_from_slice(&500u16.to_be_bytes());
        h[0x12] = 3;
        h[0x1e..].copy_from_slice(COPYRIGHT);

        for g in 0..groups {
            for _ in 0..channels {
                let mut block = [0u8; 18];
                block[1] = 1 + (g % 100) as u8;
                block[2..].fill(0x1f);
                h.extend_from_slice(&block);
            }
        }

        if end {
            for _ in 0..channels {
                let mut block = [0u8; 18];
                block[0] = 0x80;
                block[1] = 0x01;
                h.extend_from_slice(&block);
            }
        }

        h
    }

    fn open(data: Vec<u8>) -> Result<AdxReader<'static>> {
        let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
        AdxReader::try_new(mss, FormatOptions::default())
    }

    fn packets(reader: &mut AdxReader<'_>) -> Vec<(i64, u64, u64, usize)> {
        let mut out = Vec::new();
        while let Some(p) = reader.next_packet().unwrap() {
            out.push((p.pts.get(), p.dur.get(), p.trim_end.get(), p.data.len()));
        }
        out
    }

    #[test]
    fn end_block_is_not_audio() {
        let mut reader = open(stream(2, 10 * 32, 10, true)).unwrap();
        let track = &reader.tracks()[0];
        assert_eq!(track.num_frames, Some(320));
        assert_eq!(track.padding, None);
        assert_eq!(packets(&mut reader), vec![(0, 320, 0, 360)]);
    }

    #[test]
    fn total_shorter_than_groups_is_padding() {
        let mut reader = open(stream(1, 300, 10, false)).unwrap();
        let track = &reader.tracks()[0];
        assert_eq!(track.num_frames, Some(300));
        assert_eq!(track.padding, Some(20));
        assert_eq!(packets(&mut reader), vec![(0, 300, 20, 180)]);
    }

    #[test]
    fn zero_total_uses_data_length() {
        let reader = open(stream(1, 0, 10, true)).unwrap();
        assert_eq!(reader.tracks()[0].num_frames, Some(320));
    }

    #[test]
    fn total_beyond_data_is_clamped() {
        let reader = open(stream(1, 100_000, 10, false)).unwrap();
        assert_eq!(reader.tracks()[0].num_frames, Some(320));
    }

    #[test]
    fn partial_group_is_dropped() {
        let mut data = stream(2, 0, 3, false);
        data.truncate(data.len() - 5);
        let mut reader = open(data).unwrap();
        assert_eq!(packets(&mut reader), vec![(0, 64, 0, 72)]);
    }

    #[test]
    fn packets_hold_128_groups() {
        let mut reader = open(stream(1, 0, 300, true)).unwrap();
        let sizes: Vec<_> = packets(&mut reader).iter().map(|p| (p.0, p.1)).collect();
        assert_eq!(sizes, vec![(0, 4096), (4096, 4096), (8192, 1408)]);
    }

    #[test]
    fn end_block_in_the_middle_stops_the_stream() {
        let mut data = stream(1, 0, 10, false);
        data[0x24 + 4 * 18] = 0x80;
        let mut reader = open(data).unwrap();
        assert_eq!(packets(&mut reader), vec![(0, 128, 0, 72)]);
        assert!(reader.next_packet().unwrap().is_none());
    }

    #[test]
    fn seeks_with_preroll() {
        let mut reader = open(stream(2, 0, 100, true)).unwrap();
        let to = || SeekTo::Timestamp { ts: Timestamp::new(50 * 32 + 7), track_id: 0 };

        let coarse = reader.seek(SeekMode::Coarse, to()).unwrap();
        assert_eq!(coarse.actual_ts.get(), 50 * 32);
        assert_eq!(reader.next_packet().unwrap().unwrap().pts.get(), 50 * 32);

        let accurate = reader.seek(SeekMode::Accurate, to()).unwrap();
        assert_eq!(accurate.required_ts.get(), 50 * 32 + 7);
        assert_eq!(accurate.actual_ts.get(), (50 - SEEK_PREROLL_GROUPS as i64) * 32);

        let past = SeekTo::Timestamp { ts: Timestamp::new(100 * 32), track_id: 0 };
        assert!(reader.seek(SeekMode::Accurate, past).is_err());

        // The reader works again after the end of audio.
        while reader.next_packet().unwrap().is_some() {}
        reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::ZERO, track_id: 0 })
            .unwrap();
        assert!(reader.next_packet().unwrap().is_some());
    }

    #[test]
    fn rejects_unsupported_streams() {
        // AHX.
        let mut data = stream(1, 0, 1, false);
        data[4] = 0x11;
        assert!(matches!(open(data), Err(Error::Unsupported(_))));

        // Encrypted.
        let mut data = stream(1, 0, 1, false);
        data[0x12] = 4;
        data[0x13] = 9;
        assert!(matches!(open(data), Err(Error::Unsupported(_))));

        // No copyright string.
        let mut data = stream(1, 0, 1, false);
        data[0x20] = 0;
        assert!(open(data).is_err());

        // Seven channels.
        assert!(open(stream(7, 0, 1, false)).is_err());

        // A header that is longer than the stream.
        let mut data = stream(1, 0, 0, false);
        data.truncate(0x20);
        assert!(open(data).is_err());
    }
}
