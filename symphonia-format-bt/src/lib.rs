// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]

//! Readers of Bluetooth audio streams: SBC, aptX, aptX HD and LC3.
//!
//! | Reader            | Stream               | Typical extensions | Probed |
//! |-------------------|----------------------|--------------------|--------|
//! | [`SbcReader`]     | SBC and mSBC frames  | `.sbc`, `.msbc`    | Yes    |
//! | [`AptxReader`]    | aptX blocks          | `.aptx`            | No     |
//! | [`AptxHdReader`]  | aptX HD blocks       | `.aptxhd`          | No     |
//! | [`Lc3Reader`]     | LC3 / LC3plus file   | `.lc3`             | Yes    |
//!
//! The SBC and aptX streams are those of FFmpeg's `sbc`, `aptx` and `aptx_hd` demuxers, which
//! have no container: the file is a concatenation of frames (SBC), or of fixed-size blocks
//! (aptX). The LC3 file is the one written by the `elc3` tool of `liblc3` (see below).
//!
//! # SBC
//!
//! An SBC frame starts with the sync word `0x9c` (`0xad` for mSBC) followed by a header that has
//! the frame length, and a CRC. A stream is recognized by a chain of consecutive valid headers,
//! so [`SbcReader`] is probed. One frame is one packet, and the stream is seekable (to the
//! frame) when the media source is, as the parameters of the stream are constant. The duration is
//! computed from the length of the stream.
//!
//! # aptX
//!
//! aptX streams have no header, sync word or sampling rate: they are only identified by the
//! file extension, as in FFmpeg. The readers can therefore not be found by the probe, and have to
//! be created with `try_new`. The sampling rate is not in the stream, and is 48 kHz unless it is
//! given with `try_new_with_rate`.
//!
//! The decoder state of aptX is adaptive and the synchronization is carried by the parity of the
//! samples, so the readers seek to the start of the stream only.
//!
//! # LC3
//!
//! The `.lc3` file of `liblc3` (`tools/lc3bin.c`) is a little-endian header, which is recognized
//! by its 2-byte signature `0x1c 0xcc` and checked: the signature, the size of the header, the
//! sample rate in units of 100 Hz, the bit rate in units of 100 bit/s, the number of channels,
//! the frame duration in units of 10 µs, a reserved field, the number of samples per channel
//! (32 bits, without the delay of the codec), and for LC3plus the high resolution flag. The
//! header is followed by blocks: a 16-bit size and as many bytes, which hold one frame of every
//! channel. A block is a packet. The track has the delay of the codec, the number of samples
//! of the stream, and the padding that completes the last frame. The reader seeks to the frame
//! that holds the timestamp, but the decoder starts from a blank state there, so the first frame
//! after a seek is not exact.

use std::io::{ErrorKind, Seek, SeekFrom};

use symphonia_codec_aptx::{
    APTX_BLOCK_SIZE, APTX_HD_BLOCK_SIZE, DEFAULT_SAMPLE_RATE as APTX_DEFAULT_RATE,
};
use symphonia_codec_lc3::{delay_samples, frame_samples};
use symphonia_codec_sbc::FrameHeader;
use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_APTX, CODEC_ID_APTX_HD, CODEC_ID_LC3, CODEC_ID_SBC,
};
use symphonia_core::common::FourCc;
use symphonia_core::errors::{
    Error, Result, SeekErrorKind, decode_error, seek_error, unsupported_error,
};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::packet::Packet;
use symphonia_core::support_format;
use symphonia_core::units::{Duration, TimeBase, Timestamp};

/// The number of blocks of an aptX packet, as in FFmpeg's raw demuxers.
const APTX_PACKET_BLOCKS: usize = 256;

/// The number of samples per channel of an aptX block.
const APTX_BLOCK_SAMPLES: u64 = 4;

/// The number of bytes examined to score an SBC stream.
const SCORE_LEN: usize = 8 * 1024;

/// The number of consecutive frames that identify an SBC stream.
const CONFIDENT_FRAMES: u32 = 4;

/// The most bytes skipped between two SBC frames before giving up.
const MAX_RESYNC: usize = 64 * 1024;

const SBC_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"SBC ")),
    short_name: "sbc",
    long_name: "Raw SBC (low-complexity subband codec)",
};

const APTX_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"APTX")),
    short_name: "aptx",
    long_name: "Raw aptX",
};

const APTX_HD_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"APTH")),
    short_name: "aptx_hd",
    long_name: "Raw aptX HD",
};

const LC3_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"LC3 ")),
    short_name: "lc3",
    long_name: "LC3 / LC3plus file",
};

/// The signature of an `.lc3` file (the 16-bit value `0xcc1c`, little-endian).
const LC3_SIGNATURE: [u8; 2] = [0x1c, 0xcc];

/// The size of the part of the header of an `.lc3` file that is used (the high resolution flag
/// is in the last two bytes, and is only there if the header is long enough).
const LC3_HEADER_MIN: usize = 18;
const LC3_HEADER_HR: usize = 20;

/// The most channels of an LC3 stream.
const LC3_MAX_CHANNELS: u16 = 8;

/// The first two bytes of every SBC frame: the sync word, and the second byte of the header,
/// which holds the sampling frequency, number of blocks, channel mode, allocation method and
/// number of sub-bands, all of whose combinations are valid.
static SBC_MARKER_BYTES: [[u8; 2]; 256] = {
    let mut m = [[0x9c, 0]; 256];
    let mut i = 0;
    while i < 256 {
        m[i][1] = i as u8;
        i += 1;
    }
    m
};

/// The markers of SBC and mSBC (`0xad 0x00 0x00`) frames.
static SBC_MARKERS: [&[u8]; 257] = {
    let mut m: [&[u8]; 257] = [&[0xad, 0x00, 0x00]; 257];
    let mut i = 0;
    while i < 256 {
        m[i] = &SBC_MARKER_BYTES[i];
        i += 1;
    }
    m
};

/// Reads from `src` until `buf` is full or the end of the stream. Returns the length read.
fn read_up_to<B: ReadBytes>(src: &mut B, buf: &mut [u8]) -> Result<usize> {
    let mut len = 0;

    while len < buf.len() {
        match src.read_buf(&mut buf[len..]) {
            Ok(0) => break,
            Ok(n) => len += n,
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
    }

    Ok(len)
}

/// Returns the number of bytes of the frame at the start of `buf` if it has a valid header.
fn sbc_frame_len(buf: &[u8]) -> Option<usize> {
    FrameHeader::parse(buf).map(|h| h.frame_len())
}

/// Score the data at the current position of `src` as an SBC stream.
fn score_sbc(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
    let mut buf = vec![0; SCORE_LEN];
    let len = read_up_to(&mut src, &mut buf)?;
    let eof = len < SCORE_LEN;
    let buf = &buf[..len];

    // Count the frames that are consecutive from the start of the data.
    let mut pos = 0;
    let mut frames = 0;

    while let Some(frame_len) = sbc_frame_len(&buf[pos..]) {
        // A frame that is not complete is only counted at the end of the stream.
        if pos + frame_len > buf.len() {
            break;
        }

        pos += frame_len;
        frames += 1;

        if frames >= CONFIDENT_FRAMES {
            return Ok(Score::Supported(255));
        }
    }

    // A short stream that is only frames.
    Ok(if eof && frames > 0 && pos == buf.len() {
        Score::Supported(if frames > 1 { 200 } else { 100 })
    }
    else {
        Score::Unsupported
    })
}

/// The stream of a reader.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Kind {
    Sbc,
    Aptx,
    AptxHd,
    Lc3,
}

impl Kind {
    /// The size of an aptX block, or 0 for SBC and LC3.
    fn block_size(self) -> usize {
        match self {
            Kind::Sbc | Kind::Lc3 => 0,
            Kind::Aptx => APTX_BLOCK_SIZE,
            Kind::AptxHd => APTX_HD_BLOCK_SIZE,
        }
    }
}

/// The header of an `.lc3` file.
struct Lc3Header {
    /// The size of the header in bytes.
    size: u16,
    sample_rate: u32,
    channels: u16,
    frame_us: u32,
    hr_mode: bool,
    /// The number of samples per channel, without the delay. 0 if unknown.
    num_samples: u32,
    /// The delay of the codec, and the number of samples of a frame.
    delay: u32,
    frame_samples: u64,
}

impl Lc3Header {
    /// Parse and check the header at the start of `buf`.
    fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < LC3_HEADER_MIN || buf[..2] != LC3_SIGNATURE {
            return None;
        }

        let u16_at = |o: usize| u16::from_le_bytes([buf[o], buf[o + 1]]);

        let size = u16_at(2);

        if usize::from(size) < LC3_HEADER_MIN {
            return None;
        }

        let sample_rate = u32::from(u16_at(4)) * 100;
        let channels = u16_at(8);
        let frame_us = u32::from(u16_at(10)) * 10;
        let num_samples = u32::from(u16_at(14)) | (u32::from(u16_at(16)) << 16);

        // The high resolution flag is only in a header that has room for it.
        let hr_mode =
            usize::from(size) >= LC3_HEADER_HR && buf.len() >= LC3_HEADER_HR && u16_at(18) != 0;

        if channels == 0 || channels > LC3_MAX_CHANNELS {
            return None;
        }

        // Only the sample rates and frame durations of the codec are valid.
        let delay = delay_samples(frame_us, sample_rate, hr_mode)?;
        let frame_samples = u64::from(frame_samples(frame_us, sample_rate, hr_mode)?);

        Some(Lc3Header {
            size,
            sample_rate,
            channels,
            frame_us,
            hr_mode,
            num_samples,
            delay,
            frame_samples,
        })
    }
}

/// Score the data at the current position of `src` as an LC3 file.
fn score_lc3(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
    let mut hdr = [0u8; LC3_HEADER_HR];
    let len = read_up_to(&mut src, &mut hdr)?;

    Ok(if Lc3Header::parse(&hdr[..len]).is_some() {
        Score::Supported(255)
    }
    else {
        Score::Unsupported
    })
}

struct BtReader<'s> {
    reader: MediaSourceStream<'s>,
    kind: Kind,
    tracks: Vec<Track>,
    media_info: MediaInfo,
    metadata: MetadataLog,
    chapters: Option<ChapterGroup>,
    /// The position of the first frame.
    start_pos: u64,
    /// The timestamp of the next packet.
    next_ts: i64,
    /// The length of the SBC frames, of the first frame.
    frame_len: usize,
    /// The number of samples of an SBC or LC3 frame.
    frame_samples: u64,
}

impl<'s> BtReader<'s> {
    fn try_new_sbc(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        let start_pos = mss.pos();

        let mut hdr = [0; 4];
        let len = read_up_to(&mut mss, &mut hdr)?;

        let Some(header) = FrameHeader::parse(&hdr[..len])
        else {
            return unsupported_error("sbc: no frame found");
        };

        let channels = if header.channels() == 1 {
            Channels::Positioned(Position::FRONT_LEFT)
        }
        else {
            Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT)
        };

        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_SBC)
            .with_sample_rate(header.sample_rate())
            .with_channels(channels);

        let frame_len = header.frame_len();
        let frame_samples = header.samples() as u64;

        let mut track = Track::new(0);
        track
            .with_time_base(
                TimeBase::try_from_recip(header.sample_rate())
                    .ok_or(Error::DecodeError("sbc: invalid rate"))?,
            )
            .with_codec_params(CodecParameters::Audio(params))
            .with_start_ts(Timestamp::new(0));

        if let Some(total) = mss.byte_len() {
            // The frames are all of the same length. A trailing part of a frame is not counted.
            let frames = total.saturating_sub(start_pos) / frame_len as u64;
            track.with_num_frames(frames * frame_samples);
            track.with_duration(Duration::new(frames * frame_samples));
        }

        // Return to the first frame.
        mss.seek_buffered(start_pos);

        Ok(BtReader {
            reader: mss,
            kind: Kind::Sbc,
            media_info: MediaInfo::from_track(&track),
            tracks: vec![track],
            metadata: opts.external_data.metadata.unwrap_or_default(),
            chapters: opts.external_data.chapters,
            start_pos,
            next_ts: 0,
            frame_len,
            frame_samples,
        })
    }

    fn try_new_aptx(
        kind: Kind,
        mss: MediaSourceStream<'s>,
        opts: FormatOptions,
        rate: u32,
    ) -> Result<Self> {
        if rate == 0 {
            return unsupported_error("aptx: invalid sample rate");
        }

        let codec = if kind == Kind::AptxHd { CODEC_ID_APTX_HD } else { CODEC_ID_APTX };

        let mut params = AudioCodecParameters::new();
        params
            .for_codec(codec)
            .with_sample_rate(rate)
            .with_channels(Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT));

        let start_pos = mss.pos();

        let mut track = Track::new(0);
        track
            .with_time_base(
                TimeBase::try_from_recip(rate).ok_or(Error::DecodeError("aptx: invalid rate"))?,
            )
            .with_codec_params(CodecParameters::Audio(params))
            .with_start_ts(Timestamp::new(0));

        if let Some(total) = mss.byte_len() {
            let blocks = total.saturating_sub(start_pos) / kind.block_size() as u64;
            track.with_num_frames(blocks * APTX_BLOCK_SAMPLES);
            track.with_duration(Duration::new(blocks * APTX_BLOCK_SAMPLES));
        }

        Ok(BtReader {
            reader: mss,
            kind,
            media_info: MediaInfo::from_track(&track),
            tracks: vec![track],
            metadata: opts.external_data.metadata.unwrap_or_default(),
            chapters: opts.external_data.chapters,
            start_pos,
            next_ts: 0,
            frame_len: 0,
            frame_samples: 0,
        })
    }

    fn next_sbc_packet(&mut self) -> Result<Option<Packet>> {
        let mut hdr = [0u8; 3];

        if read_up_to(&mut self.reader, &mut hdr)? < hdr.len() {
            return Ok(None);
        }

        // Find the next frame, skipping the bytes that are not one.
        let mut skipped = 0;

        let header = loop {
            if let Some(header) = FrameHeader::parse(&hdr) {
                break header;
            }

            if skipped >= MAX_RESYNC {
                return decode_error("sbc: lost synchronization");
            }

            let mut next = [0u8; 1];
            if read_up_to(&mut self.reader, &mut next)? == 0 {
                return Ok(None);
            }

            hdr = [hdr[1], hdr[2], next[0]];
            skipped += 1;
        };

        let len = header.frame_len();
        let mut data = vec![0u8; len];
        data[..hdr.len()].copy_from_slice(&hdr);

        // A frame that is cut by the end of the stream is dropped.
        if read_up_to(&mut self.reader, &mut data[hdr.len()..])? < len - hdr.len() {
            return Ok(None);
        }

        let dur = header.samples() as u64;
        let ts = self.next_ts;
        self.next_ts += dur as i64;

        let mut packet = Packet::new(0, Timestamp::new(ts), Duration::new(dur), data);
        packet.dts = Timestamp::new(ts);
        Ok(Some(packet))
    }

    fn try_new_lc3(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        let mut hdr = [0u8; LC3_HEADER_HR];
        let len = read_up_to(&mut mss, &mut hdr)?;

        let Some(header) = Lc3Header::parse(&hdr[..len])
        else {
            return unsupported_error("lc3: invalid file header");
        };

        // Go to the first block.
        mss.seek_buffered(mss.pos() - len as u64 + u64::from(header.size));
        let start_pos = mss.pos();

        let channels = match header.channels {
            1 => Channels::Positioned(Position::FRONT_LEFT),
            2 => Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT),
            n => Channels::Discrete(n),
        };

        // The decoder is configured by the frame duration, the error protection mode (0), and
        // the high resolution flag.
        let mut extra_data = Vec::with_capacity(6);
        extra_data.extend_from_slice(&((header.frame_us / 10) as u16).to_le_bytes());
        extra_data.extend_from_slice(&0u16.to_le_bytes());
        extra_data.extend_from_slice(&u16::from(header.hr_mode).to_le_bytes());

        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_LC3)
            .with_sample_rate(header.sample_rate)
            .with_channels(channels)
            .with_extra_data(extra_data.into_boxed_slice());

        let mut track = Track::new(0);
        track
            .with_time_base(
                TimeBase::try_from_recip(header.sample_rate)
                    .ok_or(Error::DecodeError("lc3: invalid rate"))?,
            )
            .with_codec_params(CodecParameters::Audio(params))
            .with_start_ts(Timestamp::new(0))
            .with_delay(header.delay);

        if header.num_samples > 0 {
            let samples = u64::from(header.num_samples);
            let total = (samples + u64::from(header.delay)).div_ceil(header.frame_samples)
                * header.frame_samples;

            track.with_num_frames(samples).with_duration(Duration::new(samples));
            track.with_padding((total - samples - u64::from(header.delay)) as u32);
        }

        Ok(BtReader {
            reader: mss,
            kind: Kind::Lc3,
            media_info: MediaInfo::from_track(&track),
            tracks: vec![track],
            metadata: opts.external_data.metadata.unwrap_or_default(),
            chapters: opts.external_data.chapters,
            start_pos,
            next_ts: 0,
            frame_len: 0,
            frame_samples: header.frame_samples,
        })
    }

    fn next_lc3_packet(&mut self) -> Result<Option<Packet>> {
        let mut size = [0u8; 2];

        if read_up_to(&mut self.reader, &mut size)? < size.len() {
            return Ok(None);
        }

        let len = usize::from(u16::from_le_bytes(size));

        // An empty block ends the stream, as in `lc3bin.c`.
        if len == 0 {
            return Ok(None);
        }

        let mut data = vec![0u8; len];

        // A block that is cut by the end of the stream is dropped.
        if read_up_to(&mut self.reader, &mut data)? < len {
            return Ok(None);
        }

        let ts = self.next_ts;
        self.next_ts += self.frame_samples as i64;

        let mut packet =
            Packet::new(0, Timestamp::new(ts), Duration::new(self.frame_samples), data);
        packet.dts = Timestamp::new(ts);
        Ok(Some(packet))
    }

    /// Seek to the first block at or before the frame `idx`: skip the blocks from the start.
    fn seek_lc3(&mut self, idx: u64) -> Result<u64> {
        self.reader.seek(SeekFrom::Start(self.start_pos))?;

        for done in 0..idx {
            let mut size = [0u8; 2];

            if read_up_to(&mut self.reader, &mut size)? < size.len() {
                return seek_error(SeekErrorKind::OutOfRange);
            }

            let len = u16::from_le_bytes(size);

            if len == 0 {
                // The end of the stream: stop before the end marker.
                self.reader.seek(SeekFrom::Current(-2))?;
                return Ok(done);
            }

            self.reader.seek(SeekFrom::Current(i64::from(len)))?;
        }

        Ok(idx)
    }

    fn next_aptx_packet(&mut self) -> Result<Option<Packet>> {
        let block_size = self.kind.block_size();
        let mut data = vec![0u8; APTX_PACKET_BLOCKS * block_size];

        let len = read_up_to(&mut self.reader, &mut data)?;
        let blocks = len / block_size;

        // A part of a block at the end of the stream is dropped.
        if blocks == 0 {
            return Ok(None);
        }

        data.truncate(blocks * block_size);

        let dur = blocks as u64 * APTX_BLOCK_SAMPLES;
        let ts = self.next_ts;
        self.next_ts += dur as i64;

        let mut packet = Packet::new(0, Timestamp::new(ts), Duration::new(dur), data);
        packet.dts = Timestamp::new(ts);
        Ok(Some(packet))
    }

    fn next_packet_inner(&mut self) -> Result<Option<Packet>> {
        match self.kind {
            Kind::Sbc => self.next_sbc_packet(),
            Kind::Aptx | Kind::AptxHd => self.next_aptx_packet(),
            Kind::Lc3 => self.next_lc3_packet(),
        }
    }

    fn required_ts(&self, to: SeekTo) -> Result<Timestamp> {
        let ts = match to {
            SeekTo::Timestamp { ts, track_id: 0 } => ts,
            SeekTo::Time { time, track_id: None | Some(0) } => {
                let tb =
                    self.tracks[0].time_base.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;
                tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?
            }
            _ => return seek_error(SeekErrorKind::Unseekable),
        };

        if ts.is_negative() {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        Ok(ts)
    }

    fn seek(&mut self, to: SeekTo) -> Result<SeekedTo> {
        let required_ts = self.required_ts(to)?;

        if !self.reader.is_seekable() {
            return seek_error(SeekErrorKind::ForwardOnly);
        }

        if self.kind == Kind::Lc3 {
            if let Some(dur) = self.tracks[0].duration {
                if required_ts.get() as u64 >= dur.get() {
                    return seek_error(SeekErrorKind::OutOfRange);
                }
            }

            // The timestamps count the delay of the codec.
            let idx = self.seek_lc3(required_ts.get() as u64 / self.frame_samples)?;
            let actual_ts = (idx * self.frame_samples) as i64;
            self.next_ts = actual_ts;

            return Ok(SeekedTo { track_id: 0, required_ts, actual_ts: Timestamp::new(actual_ts) });
        }

        let idx = match self.kind {
            Kind::Sbc => {
                if let Some(dur) = self.tracks[0].duration {
                    if required_ts.get() as u64 >= dur.get() {
                        return seek_error(SeekErrorKind::OutOfRange);
                    }
                }

                required_ts.get() as u64 / self.frame_samples
            }
            // The state of the decoder cannot be recovered from the middle of the stream.
            Kind::Aptx | Kind::AptxHd | Kind::Lc3 => {
                if required_ts.get() != 0 {
                    return seek_error(SeekErrorKind::Unseekable);
                }
                0
            }
        };

        let (pos, actual_ts) = match self.kind {
            Kind::Sbc => {
                (self.start_pos + idx * self.frame_len as u64, (idx * self.frame_samples) as i64)
            }
            Kind::Aptx | Kind::AptxHd | Kind::Lc3 => (self.start_pos, 0),
        };

        self.reader.seek(SeekFrom::Start(pos))?;
        self.next_ts = actual_ts;

        Ok(SeekedTo { track_id: 0, required_ts, actual_ts: Timestamp::new(actual_ts) })
    }
}

macro_rules! bt_reader_impl {
    ($name:ident, $info:expr) => {
        impl FormatReader for $name<'_> {
            fn format_info(&self) -> &FormatInfo {
                &$info
            }

            fn media_info(&self) -> &MediaInfo {
                &self.0.media_info
            }

            fn next_packet(&mut self) -> Result<Option<Packet>> {
                self.0.next_packet_inner()
            }

            fn metadata(&mut self) -> Metadata<'_> {
                self.0.metadata.metadata()
            }

            fn chapters(&self) -> Option<&ChapterGroup> {
                self.0.chapters.as_ref()
            }

            fn tracks(&self) -> &[Track] {
                &self.0.tracks
            }

            fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
                self.0.seek(to)
            }

            fn into_inner<'a>(self: Box<Self>) -> MediaSourceStream<'a>
            where
                Self: 'a,
            {
                self.0.reader
            }
        }
    };
}

/// Raw SBC and mSBC stream reader.
pub struct SbcReader<'s>(BtReader<'s>);

impl<'s> SbcReader<'s> {
    /// Create a reader. The first frame must be at the current position of the stream.
    pub fn try_new(mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        Ok(SbcReader(BtReader::try_new_sbc(mss, opts)?))
    }
}

impl Scoreable for SbcReader<'_> {
    fn score(src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        score_sbc(src)
    }
}

impl<'s> ProbeableFormat<'s> for SbcReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'s>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + 's>> {
        Ok(Box::new(SbcReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        static DATA: [ProbeFormatData; 1] = [support_format!(
            SBC_INFO,
            &["sbc", "msbc"],
            &["audio/x-sbc", "audio/sbc"],
            &SBC_MARKERS
        )];
        &DATA
    }
}

bt_reader_impl!(SbcReader, SBC_INFO);

/// Raw aptX stream reader. The stream has no signature, so it is not found by the probe.
pub struct AptxReader<'s>(BtReader<'s>);

impl<'s> AptxReader<'s> {
    /// Create a reader of a stream of 48 kHz.
    pub fn try_new(mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        Self::try_new_with_rate(mss, opts, APTX_DEFAULT_RATE)
    }

    /// Create a reader of a stream of the sampling rate `rate` in Hz.
    pub fn try_new_with_rate(
        mss: MediaSourceStream<'s>,
        opts: FormatOptions,
        rate: u32,
    ) -> Result<Self> {
        Ok(AptxReader(BtReader::try_new_aptx(Kind::Aptx, mss, opts, rate)?))
    }
}

bt_reader_impl!(AptxReader, APTX_INFO);

/// Raw aptX HD stream reader. The stream has no signature, so it is not found by the probe.
pub struct AptxHdReader<'s>(BtReader<'s>);

impl<'s> AptxHdReader<'s> {
    /// Create a reader of a stream of 48 kHz.
    pub fn try_new(mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        Self::try_new_with_rate(mss, opts, APTX_DEFAULT_RATE)
    }

    /// Create a reader of a stream of the sampling rate `rate` in Hz.
    pub fn try_new_with_rate(
        mss: MediaSourceStream<'s>,
        opts: FormatOptions,
        rate: u32,
    ) -> Result<Self> {
        Ok(AptxHdReader(BtReader::try_new_aptx(Kind::AptxHd, mss, opts, rate)?))
    }
}

bt_reader_impl!(AptxHdReader, APTX_HD_INFO);

/// LC3 and LC3plus file reader (the `.lc3` file of `liblc3`).
pub struct Lc3Reader<'s>(BtReader<'s>);

impl<'s> Lc3Reader<'s> {
    /// Create a reader. The header of the file must be at the current position of the stream.
    pub fn try_new(mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        Ok(Lc3Reader(BtReader::try_new_lc3(mss, opts)?))
    }
}

impl Scoreable for Lc3Reader<'_> {
    fn score(src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        score_lc3(src)
    }
}

impl<'s> ProbeableFormat<'s> for Lc3Reader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'s>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + 's>> {
        Ok(Box::new(Lc3Reader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        static DATA: [ProbeFormatData; 1] =
            [support_format!(LC3_INFO, &["lc3"], &["audio/lc3"], &[&LC3_SIGNATURE])];
        &DATA
    }
}

bt_reader_impl!(Lc3Reader, LC3_INFO);
