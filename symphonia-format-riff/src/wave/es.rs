// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Framing of compressed surround audio (AC-3, E-AC-3, DTS) stored in the `data` chunk of a WAVE
//! file, either as a raw elementary stream (`WAVE_FORMAT_DVM`, `WAVE_FORMAT_DTS2`, and the
//! corresponding extensible sub-formats) or wrapped in IEC 61937 bursts (see [`super::spdif`]).

use std::io::{ErrorKind, Seek, SeekFrom};

use symphonia_common::mpeg::es::EsParser;
use symphonia_common::mpeg::surround::{Ac3Es, DcaEs};
use symphonia_core::codecs::audio::AudioCodecId;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_AC3, CODEC_ID_DCA, CODEC_ID_EAC3};
use symphonia_core::errors::Result;
use symphonia_core::io::{MediaSourceStream, ReadBytes, SeekBuffered};
use symphonia_core::packet::Packet;
use symphonia_core::units::{Duration, Timestamp};

use super::spdif;

/// The number of bytes of the `data` chunk examined when looking for IEC 61937 bursts.
pub const PROBE_LEN: usize = 1 << 16;

/// The number of bytes the reader must be able to seek back after probing and parsing the first
/// frame.
pub const SEEKBACK_LEN: usize = 1 << 17;

/// The number of bytes read from the stream per read of a raw stream.
const RAW_CHUNK_LEN: u64 = 8192;

/// Creates the frame parser for `codec`.
pub fn make_parser(codec: AudioCodecId) -> Option<Box<dyn EsParser>> {
    match codec {
        CODEC_ID_AC3 | CODEC_ID_EAC3 => Some(Box::new(Ac3Es::new())),
        CODEC_ID_DCA => Some(Box::new(DcaEs::new())),
        _ => None,
    }
}

/// Probes the data at the start of the `data` chunk for IEC 61937 bursts and returns the codec.
pub fn probe_spdif(
    mss: &mut MediaSourceStream<'_>,
    data_start: u64,
    data_end: u64,
) -> Option<AudioCodecId> {
    mss.ensure_seekback_buffer(SEEKBACK_LEN);

    let len = data_end.saturating_sub(data_start).min(PROBE_LEN as u64) as usize;
    let mut buf = vec![0u8; len];
    let n = mss.read_buf(&mut buf).ok()?;
    mss.seek_buffered(data_start);

    spdif::probe(&buf[..n]).map(|p| p.codec)
}

/// The position of a unit (a frame of a raw stream, or a burst) in the `data` chunk and the
/// timestamp of its first frame.
#[derive(Copy, Clone, Debug)]
pub struct IndexEntry {
    pub pos: u64,
    pub ts: u64,
}

/// A reader of frames from the `data` chunk.
pub struct EsReader {
    parser: Box<dyn EsParser>,
    spdif: bool,
    /// The timestamp of the next frame.
    ts: u64,
    /// True if the parser is being drained of the frames of the data pushed.
    flushing: bool,
    eof: bool,
    /// The stream position that frame offsets reported by the parser are relative to.
    raw_base: u64,
    /// The unit the last frame was read from.
    last_unit: IndexEntry,
}

impl EsReader {
    /// Create a reader for the stream starting at the current position of the source, `base`.
    pub fn new(parser: Box<dyn EsParser>, spdif: bool, base: u64) -> Self {
        EsReader {
            parser,
            spdif,
            ts: 0,
            flushing: false,
            eof: false,
            raw_base: base,
            last_unit: IndexEntry { pos: base, ts: 0 },
        }
    }

    pub fn is_spdif(&self) -> bool {
        self.spdif
    }

    /// The stream parameters, once the first frame has been read.
    pub fn info(&self) -> Option<&symphonia_common::mpeg::es::EsInfo> {
        self.parser.info()
    }

    /// Restarts reading at `entry`, whose position the source must be positioned at.
    pub fn restart_at(&mut self, entry: IndexEntry) {
        self.parser = self.parser.fresh();
        self.ts = entry.ts;
        self.flushing = false;
        self.eof = false;
        self.raw_base = entry.pos;
        self.last_unit = entry;
    }

    /// Reads the next frame as a packet, reading from the source up to `end`.
    pub fn next_packet(
        &mut self,
        reader: &mut MediaSourceStream<'_>,
        end: u64,
    ) -> Result<Option<Packet>> {
        loop {
            if let Some(frame) = self.parser.next_frame(self.flushing) {
                let Ok(pts) = Timestamp::try_from(self.ts)
                else {
                    return Ok(None);
                };

                if !self.spdif {
                    self.last_unit = IndexEntry { pos: self.raw_base + frame.start, ts: self.ts };
                }

                self.ts = self.ts.saturating_add(frame.dur);

                return Ok(Some(Packet::new(0, pts, Duration::from(frame.dur), frame.data)));
            }

            if self.eof {
                return Ok(None);
            }

            self.flushing = false;

            if self.spdif { self.fill_burst(reader, end)? } else { self.fill_raw(reader, end)? }
        }
    }

    fn fill_raw(&mut self, reader: &mut MediaSourceStream<'_>, end: u64) -> Result<()> {
        let len = end.saturating_sub(reader.pos()).min(RAW_CHUNK_LEN) as usize;
        let mut buf = vec![0u8; len];

        let n = if len > 0 { reader.read_buf(&mut buf)? } else { 0 };

        if n == 0 {
            self.eof = true;
            self.flushing = true;
        }
        else {
            self.parser.push(&buf[..n]);
        }

        Ok(())
    }

    /// Reads the next burst, and pushes its payload to the parser.
    fn fill_burst(&mut self, reader: &mut MediaSourceStream<'_>, end: u64) -> Result<()> {
        match self.read_burst(reader, end) {
            Ok(true) => (),
            Ok(false) => self.eof = true,
            Err(err) if err.kind() == ErrorKind::UnexpectedEof => self.eof = true,
            Err(err) => return Err(err.into()),
        }

        // A burst holds whole frames, so the parser is not waiting on further data.
        self.flushing = true;
        Ok(())
    }

    fn read_burst(
        &mut self,
        reader: &mut MediaSourceStream<'_>,
        end: u64,
    ) -> std::io::Result<bool> {
        // Find the synchronisation words.
        let mut state = 0u32;

        let burst = loop {
            if reader.pos() >= end {
                return Ok(false);
            }

            state = (state << 8) | u32::from(reader.read_u8()?);

            let word = state.to_be_bytes();

            if word != spdif::SYNC_LE && word != spdif::SYNC_BE {
                continue;
            }

            let pos = reader.pos() - 4;

            let mut preamble = [0u8; spdif::BURST_PREAMBLE_LEN];
            preamble[..4].copy_from_slice(&word);
            reader.read_buf_exact(&mut preamble[4..])?;

            match spdif::parse_burst(&preamble) {
                Some(burst) => break (pos, burst),
                // Unsupported data type or not a burst. Keep looking.
                None => state = 0,
            }
        };

        let (pos, burst) = burst;

        let mut payload = vec![0u8; burst.payload_len];
        reader.read_buf_exact(&mut payload)?;
        spdif::unwrap_payload(&mut payload, burst.little_endian);

        // Skip the padding to the next burst.
        let pad = (burst.stride - spdif::BURST_PREAMBLE_LEN - burst.payload_len) as u64;
        reader.ignore_bytes(pad.min(end.saturating_sub(reader.pos())))?;

        self.last_unit = IndexEntry { pos, ts: self.ts };
        self.parser.push(&payload);

        Ok(true)
    }

    /// Scans the stream from `start` to `end` and returns the position and timestamp of every
    /// unit. The reader is left at an unspecified position.
    pub fn build_index(
        &mut self,
        reader: &mut MediaSourceStream<'_>,
        start: u64,
        end: u64,
    ) -> Result<Vec<IndexEntry>> {
        reader.seek(SeekFrom::Start(start))?;
        self.restart_at(IndexEntry { pos: start, ts: 0 });

        let mut index: Vec<IndexEntry> = Vec::new();

        while self.next_packet(reader, end)?.is_some() {
            if index.last().is_none_or(|last| last.pos != self.last_unit.pos) {
                index.push(self.last_unit);
            }
        }

        Ok(index)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use symphonia_core::codecs::CodecParameters;
    use symphonia_core::formats::prelude::*;
    use symphonia_core::io::MediaSourceStream;

    use super::super::WavReader;
    use super::super::spdif::tests::make_burst;
    use super::*;

    /// A 48 kHz stereo AC-3 frame of 192 bytes (1536 samples), with a payload tag.
    fn ac3_frame(tag: u8) -> Vec<u8> {
        let mut f = vec![0x0b, 0x77, 0, 0, 0x04, 0x40, 0x40, 0x00];
        f.resize(192, tag);
        f
    }

    /// A 48 kHz stereo DTS core frame of 512 bytes (512 samples), with a payload tag.
    fn dts_frame(tag: u8) -> Vec<u8> {
        let mut bits: Vec<bool> = Vec::new();
        let mut put = |v: u32, n: u32| (0..n).rev().for_each(|i| bits.push((v >> i) & 1 != 0));
        put(0x7ffe_8001, 32);
        put(1, 1); // Frame type.
        put(31, 5); // Deficit samples.
        put(0, 1); // CRC present.
        put(15, 7); // Blocks - 1.
        put(511, 14); // Frame size - 1.
        put(2, 6); // Stereo.
        put(13, 4); // 48 kHz.
        put(0, 5 + 1 + 1 + 1 + 1 + 1 + 3 + 1 + 1); // Rate through audio sync insertion.
        put(0, 2); // LFE.
        while bits.len() % 8 != 0 {
            bits.push(false);
        }
        let mut f: Vec<u8> =
            bits.chunks(8).map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | u8::from(b))).collect();
        f.resize(512, tag);
        f
    }

    fn chunk(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(tag);
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(payload);
        if payload.len() & 1 == 1 {
            out.push(0);
        }
        out
    }

    fn fmt_base(tag: u16, channels: u16, bits: u16) -> Vec<u8> {
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&tag.to_le_bytes());
        fmt.extend_from_slice(&channels.to_le_bytes());
        fmt.extend_from_slice(&48000u32.to_le_bytes());
        fmt.extend_from_slice(
            &(48000u32 * u32::from(channels) * u32::from(bits) / 8).to_le_bytes(),
        );
        fmt.extend_from_slice(&(channels * bits / 8).to_le_bytes());
        fmt.extend_from_slice(&bits.to_le_bytes());
        fmt
    }

    fn wav(fmt: &[u8], data: &[u8]) -> Vec<u8> {
        let body = [chunk(b"fmt ", fmt), chunk(b"data", data)].concat();
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(4 + body.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(&body);
        out
    }

    fn extensible_fmt(guid: [u8; 16], channels: u16, mask: u32) -> Vec<u8> {
        let mut fmt = fmt_base(0xfffe, channels, 16);
        fmt.extend_from_slice(&22u16.to_le_bytes());
        fmt.extend_from_slice(&16u16.to_le_bytes());
        fmt.extend_from_slice(&mask.to_le_bytes());
        fmt.extend_from_slice(&guid);
        fmt
    }

    fn base_guid(tag: u32) -> [u8; 16] {
        let mut g =
            [0, 0, 0, 0, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71];
        g[..4].copy_from_slice(&tag.to_le_bytes());
        g
    }

    fn open(bytes: Vec<u8>) -> WavReader<'static> {
        let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
        WavReader::try_new(mss, FormatOptions::default()).unwrap()
    }

    fn packets(reader: &mut WavReader<'_>) -> Vec<Packet> {
        std::iter::from_fn(|| reader.next_packet().unwrap()).collect()
    }

    fn audio_params(reader: &WavReader<'_>) -> symphonia_core::codecs::audio::AudioCodecParameters {
        match reader.tracks()[0].codec_params.as_ref().unwrap() {
            CodecParameters::Audio(p) => p.clone(),
            _ => panic!("not audio"),
        }
    }

    fn check_raw(reader: &mut WavReader<'_>, codec: AudioCodecId, frames: &[Vec<u8>], dur: u64) {
        let params = audio_params(reader);
        assert_eq!(params.codec, codec);
        assert_eq!(params.sample_rate, Some(48000));

        let packets = packets(reader);
        assert_eq!(packets.len(), frames.len());
        for (i, (p, f)) in packets.iter().zip(frames).enumerate() {
            assert_eq!(&*p.data, &f[..]);
            assert_eq!(p.pts.get() as u64, i as u64 * dur);
            assert_eq!(p.dur.get(), dur);
        }
    }

    #[test]
    fn raw_ac3_tag_0x2000() {
        let frames: Vec<_> = (1..=5).map(ac3_frame).collect();
        let mut reader = open(wav(&fmt_base(0x2000, 2, 0), &frames.concat()));
        assert_eq!(reader.tracks()[0].num_frames, Some(5 * 1536));
        check_raw(&mut reader, CODEC_ID_AC3, &frames, 1536);
    }

    #[test]
    fn raw_ac3_extensible_base_guid() {
        let frames: Vec<_> = (1..=4).map(ac3_frame).collect();
        let fmt = extensible_fmt(base_guid(0x2000), 2, 0x3);
        let mut reader = open(wav(&fmt, &frames.concat()));
        check_raw(&mut reader, CODEC_ID_AC3, &frames, 1536);
    }

    #[test]
    fn raw_ac3_extensible_dolby_guid() {
        let guid = [
            0x2c, 0x80, 0x6d, 0xe0, 0x46, 0xdb, 0xcf, 0x11, 0xb4, 0xd1, 0x00, 0x80, 0x5f, 0x6c,
            0xbb, 0xea,
        ];
        let frames: Vec<_> = (1..=3).map(ac3_frame).collect();
        let mut reader = open(wav(&extensible_fmt(guid, 2, 0x3), &frames.concat()));
        check_raw(&mut reader, CODEC_ID_AC3, &frames, 1536);
    }

    #[test]
    fn raw_dts_tag_0x2001() {
        let frames: Vec<_> = (1..=6).map(dts_frame).collect();
        let mut reader = open(wav(&fmt_base(0x2001, 2, 0), &frames.concat()));
        check_raw(&mut reader, CODEC_ID_DCA, &frames, 512);
    }

    #[test]
    fn raw_dts_extensible_base_guid() {
        let frames: Vec<_> = (1..=3).map(dts_frame).collect();
        let fmt = extensible_fmt(base_guid(0x2001), 2, 0x3);
        let mut reader = open(wav(&fmt, &frames.concat()));
        check_raw(&mut reader, CODEC_ID_DCA, &frames, 512);
    }

    #[test]
    fn eac3_guid_is_mapped() {
        // Only the format chunk is checked: an E-AC-3 stream has no raw frames to read here.
        let guid = [
            0xaf, 0x87, 0xfb, 0xa7, 0x02, 0x2d, 0xfb, 0x42, 0xa4, 0xd4, 0x05, 0xcd, 0x93, 0x84,
            0x3b, 0xdd,
        ];
        let (codec, spdif) = super::super::chunks::es_codec_for_guid(&guid).unwrap();
        assert_eq!(codec, CODEC_ID_EAC3);
        assert!(!spdif);
    }

    fn spdif_stream(data_type: u16, frames: &[Vec<u8>], stride: usize, le: bool) -> Vec<u8> {
        frames.iter().flat_map(|f| make_burst(data_type, f, stride, le)).collect()
    }

    #[test]
    fn spdif_ac3_in_pcm_wav() {
        let frames: Vec<_> = (1..=6).map(ac3_frame).collect();
        let data = spdif_stream(0x01, &frames, 6144, true);
        let mut reader = open(wav(&fmt_base(1, 2, 16), &data));

        // The carrier is 16-bit stereo, so the samples of the track are the carrier's.
        assert_eq!(reader.tracks()[0].num_frames, Some(6 * 1536));
        check_raw(&mut reader, CODEC_ID_AC3, &frames, 1536);
    }

    #[test]
    fn spdif_dts_in_pcm_wav() {
        let frames: Vec<_> = (1..=6).map(dts_frame).collect();
        let data = spdif_stream(0x0b, &frames, 2048, true);
        let mut reader = open(wav(&fmt_base(1, 2, 16), &data));
        check_raw(&mut reader, CODEC_ID_DCA, &frames, 512);
    }

    #[test]
    fn spdif_ac3_tag_0x0092() {
        let frames: Vec<_> = (1..=4).map(ac3_frame).collect();
        let data = spdif_stream(0x01, &frames, 6144, true);
        let mut reader = open(wav(&fmt_base(0x0092, 2, 16), &data));
        check_raw(&mut reader, CODEC_ID_AC3, &frames, 1536);
    }

    #[test]
    fn tag_0x0092_without_bursts_is_raw() {
        let frames: Vec<_> = (1..=4).map(ac3_frame).collect();
        let mut reader = open(wav(&fmt_base(0x0092, 2, 16), &frames.concat()));
        check_raw(&mut reader, CODEC_ID_AC3, &frames, 1536);
    }

    #[test]
    fn spdif_big_endian_carrier() {
        let frames: Vec<_> = (1..=4).map(ac3_frame).collect();
        let data = spdif_stream(0x01, &frames, 6144, false);
        let mut reader = open(wav(&fmt_base(1, 2, 16), &data));
        check_raw(&mut reader, CODEC_ID_AC3, &frames, 1536);
    }

    #[test]
    fn spdif_with_junk_before_bursts() {
        let frames: Vec<_> = (1..=4).map(ac3_frame).collect();
        let mut data = vec![0x55u8; 4096];
        data.extend(spdif_stream(0x01, &frames, 6144, true));
        let mut reader = open(wav(&fmt_base(1, 2, 16), &data));
        let got = packets(&mut reader);
        assert_eq!(got.len(), 4);
    }

    #[test]
    fn plain_pcm_stereo_is_not_spdif() {
        let data: Vec<u8> = (0..40000u32).map(|i| (i % 251) as u8).collect();
        let mut reader = open(wav(&fmt_base(1, 2, 16), &data));
        let params = audio_params(&reader);
        assert_eq!(params.codec, symphonia_core::codecs::audio::well_known::CODEC_ID_PCM_S16LE);
        let total: u64 = packets(&mut reader).iter().map(|p| p.dur.get()).sum();
        assert_eq!(total, 10000);
    }

    #[test]
    fn truncated_spdif_burst_ends_cleanly() {
        let frames: Vec<_> = (1..=4).map(ac3_frame).collect();
        let mut data = spdif_stream(0x01, &frames, 6144, true);
        data.truncate(3 * 6144 + 100);
        let mut reader = open(wav(&fmt_base(1, 2, 16), &data));
        assert_eq!(packets(&mut reader).len(), 3);
    }

    #[test]
    fn seek_raw_and_spdif() {
        let frames: Vec<_> = (0..40).map(ac3_frame).collect();
        let raw = wav(&fmt_base(0x2000, 2, 0), &frames.concat());
        let wrapped = wav(&fmt_base(1, 2, 16), &spdif_stream(0x01, &frames, 6144, true));

        for file in [raw, wrapped] {
            let mut reader = open(file);

            let required = 20 * 1536 + 100;
            let to = SeekTo::Timestamp { ts: Timestamp::new(required), track_id: 0 };
            let seeked = reader.seek(SeekMode::Accurate, to).unwrap();

            // One frame is read before the target for the decoder's overlap.
            assert_eq!(seeked.actual_ts.get(), 19 * 1536);

            let p = reader.next_packet().unwrap().unwrap();
            assert_eq!(p.pts.get(), 19 * 1536);
            assert_eq!(p.data[8], 19);

            // Seeking back to the start, and past the end.
            let to = SeekTo::Timestamp { ts: Timestamp::new(0), track_id: 0 };
            assert_eq!(reader.seek(SeekMode::Accurate, to).unwrap().actual_ts.get(), 0);
            assert_eq!(reader.next_packet().unwrap().unwrap().data[8], 0);

            let to = SeekTo::Timestamp { ts: Timestamp::new(100 * 1536), track_id: 0 };
            assert!(reader.seek(SeekMode::Accurate, to).is_err());
        }
    }

    #[test]
    fn empty_compressed_stream_is_an_error() {
        let mss = MediaSourceStream::new(
            Box::new(Cursor::new(wav(&fmt_base(0x2000, 2, 0), &[0u8; 64]))),
            Default::default(),
        );
        assert!(WavReader::try_new(mss, FormatOptions::default()).is_err());
    }

    #[test]
    fn parser_for_codecs() {
        assert!(make_parser(CODEC_ID_AC3).is_some());
        assert!(make_parser(CODEC_ID_EAC3).is_some());
        assert!(make_parser(CODEC_ID_DCA).is_some());
        assert!(make_parser(symphonia_core::codecs::audio::well_known::CODEC_ID_MP3).is_none());
    }
}
