// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Frame header parsing and elementary stream framing of Dolby Digital (AC-3), Dolby Digital Plus
//! (E-AC-3), Dolby AC-4 sync frames, DTS Coherent Acoustics (DCA), and Dolby TrueHD/MLP.
//!
//! The frame sizes and durations follow the ATSC A/52 and ETSI TS 102 114 / TS 102 366 headers.
//! The grouping of frames into packets follows FFmpeg's parsers:
//!
//!  * An AC-3 or E-AC-3 packet is a whole access unit: an independent frame of substream 0 and the
//!    frames of the dependent substreams that follow it.
//!  * A DTS packet is a core substream frame and the extension substream (DTS-HD) following it.
//!  * A TrueHD/MLP packet is an access unit, including its 4-byte access unit header.

use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_AC3, CODEC_ID_AC4, CODEC_ID_DCA, CODEC_ID_EAC3, CODEC_ID_TRUEHD,
};

use super::es::{EsFrame, EsInfo, EsParser};
use super::sync_buf::SyncBuffer;

// -------------------------------------------------------------------------------------------------
// AC-3 and E-AC-3
// -------------------------------------------------------------------------------------------------

/// The frame sizes, in 16-bit words, of AC-3 by `frmsizecod` and `fscod` (48, 44.1, and 32 kHz).
const AC3_FRAME_SIZE_WORDS: [[u16; 3]; 38] = [
    [64, 69, 96],
    [64, 70, 96],
    [80, 87, 120],
    [80, 88, 120],
    [96, 104, 144],
    [96, 105, 144],
    [112, 121, 168],
    [112, 122, 168],
    [128, 139, 192],
    [128, 140, 192],
    [160, 174, 240],
    [160, 175, 240],
    [192, 208, 288],
    [192, 209, 288],
    [224, 243, 336],
    [224, 244, 336],
    [256, 278, 384],
    [256, 279, 384],
    [320, 348, 480],
    [320, 349, 480],
    [384, 417, 576],
    [384, 418, 576],
    [448, 487, 672],
    [448, 488, 672],
    [512, 557, 768],
    [512, 558, 768],
    [640, 696, 960],
    [640, 697, 960],
    [768, 835, 1152],
    [768, 836, 1152],
    [896, 975, 1344],
    [896, 976, 1344],
    [1024, 1114, 1536],
    [1024, 1115, 1536],
    [1152, 1253, 1728],
    [1152, 1254, 1728],
    [1280, 1393, 1920],
    [1280, 1394, 1920],
];

/// The sample rates by `fscod`.
const AC3_SAMPLE_RATES: [u32; 3] = [48000, 44100, 32000];

/// The number of audio blocks by `numblkscod` of E-AC-3.
const EAC3_BLOCKS: [u32; 4] = [1, 2, 3, 6];

/// The minimum number of bytes required to parse an AC-3 or E-AC-3 header.
pub const AC3_MIN_HEADER: usize = 8;

/// The number of samples per audio block.
const AC3_BLOCK_SAMPLES: u32 = 256;

/// The header of an AC-3 or E-AC-3 syncframe.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Ac3Header {
    /// True for E-AC-3 (`bsid` > 10), false for AC-3.
    pub eac3: bool,
    /// The bitstream identification.
    pub bsid: u8,
    /// The size of the frame in bytes.
    pub frame_size: usize,
    /// The sample rate in Hz.
    pub sample_rate: u32,
    /// The audio coding mode.
    pub acmod: u8,
    /// True if the low frequency effects channel is on.
    pub lfe: bool,
    /// The number of samples per channel in the frame.
    pub samples: u32,
    /// True if the frame is of a dependent substream of E-AC-3.
    pub dependent: bool,
    /// The substream ID of an E-AC-3 frame, always 0 for AC-3.
    pub substream_id: u8,
    /// True if the stream is in the byte-swapped (little-endian) form.
    pub swapped: bool,
}

impl Ac3Header {
    /// Parse the header at the start of `buf`, which must have the sync word. Returns `None` if
    /// `buf` is too short or the header is not valid.
    pub fn parse(buf: &[u8]) -> Option<Ac3Header> {
        if buf.len() < AC3_MIN_HEADER {
            return None;
        }

        let swapped = match (buf[0], buf[1]) {
            (0x0b, 0x77) => false,
            (0x77, 0x0b) => true,
            _ => return None,
        };

        let mut b = [0u8; AC3_MIN_HEADER];

        if swapped {
            for i in (0..AC3_MIN_HEADER).step_by(2) {
                b[i] = buf[i + 1];
                b[i + 1] = buf[i];
            }
        }
        else {
            b.copy_from_slice(&buf[..AC3_MIN_HEADER]);
        }

        // The bsid is the high 5 bits of byte 5 in both AC-3 and E-AC-3.
        let bsid = b[5] >> 3;

        if bsid > 16 {
            return None;
        }

        if bsid <= 10 {
            let fscod = usize::from(b[4] >> 6);
            let frmsizecod = usize::from(b[4] & 0x3f);

            if fscod == 3 || frmsizecod > 37 {
                return None;
            }

            let shift = u32::from(bsid.max(8) - 8);
            let acmod = b[6] >> 5;

            // After acmod: cmixlev (2 bits) if acmod is not 2 and has a center channel (but not
            // mono), surmixlev (2 bits) if the acmod has surround channels, dsurmod (2 bits) if
            // acmod is 2. Then lfeon.
            let mut pos = 3;
            if acmod == 2 {
                pos += 2;
            }
            else {
                if acmod & 1 != 0 && acmod != 1 {
                    pos += 2;
                }
                if acmod & 4 != 0 {
                    pos += 2;
                }
            }

            let lfe = (u16::from_be_bytes([b[6], b[7]]) >> (15 - pos)) & 1 != 0;

            Some(Ac3Header {
                eac3: false,
                bsid,
                frame_size: usize::from(AC3_FRAME_SIZE_WORDS[frmsizecod][fscod]) * 2,
                sample_rate: AC3_SAMPLE_RATES[fscod] >> shift,
                acmod,
                lfe,
                samples: 6 * AC3_BLOCK_SAMPLES,
                dependent: false,
                substream_id: 0,
                swapped,
            })
        }
        else {
            let strmtyp = b[2] >> 6;

            // The reserved stream type.
            if strmtyp == 3 {
                return None;
            }

            let substream_id = (b[2] >> 3) & 0x7;
            let frame_size = ((usize::from(b[2] & 0x7) << 8 | usize::from(b[3])) + 1) << 1;

            if frame_size < AC3_MIN_HEADER {
                return None;
            }

            let fscod = usize::from(b[4] >> 6);

            let (sample_rate, blocks) = if fscod == 3 {
                let fscod2 = usize::from((b[4] >> 4) & 0x3);

                if fscod2 == 3 {
                    return None;
                }

                (AC3_SAMPLE_RATES[fscod2] / 2, 6)
            }
            else {
                let blocks = EAC3_BLOCKS[usize::from((b[4] >> 4) & 0x3)];
                (AC3_SAMPLE_RATES[fscod], blocks)
            };

            // acmod (3 bits) and lfeon (1 bit) follow fscod and numblkscod (or fscod2).
            let acmod = (b[4] >> 1) & 0x7;
            let lfe = b[4] & 1 != 0;

            Some(Ac3Header {
                eac3: true,
                bsid,
                frame_size,
                sample_rate,
                acmod,
                lfe,
                samples: blocks * AC3_BLOCK_SAMPLES,
                dependent: strmtyp == 1,
                substream_id,
                swapped,
            })
        }
    }

    /// The channels of the frame, for the audio coding mode and LFE flag of the header.
    pub fn channels(&self) -> Channels {
        let mut pos = match self.acmod {
            0 | 2 => Position::FRONT_LEFT | Position::FRONT_RIGHT,
            1 => Position::FRONT_CENTER,
            3 => Position::FRONT_LEFT | Position::FRONT_CENTER | Position::FRONT_RIGHT,
            4 => Position::FRONT_LEFT | Position::FRONT_RIGHT | Position::REAR_CENTER,
            5 => {
                Position::FRONT_LEFT
                    | Position::FRONT_CENTER
                    | Position::FRONT_RIGHT
                    | Position::REAR_CENTER
            }
            6 => {
                Position::FRONT_LEFT
                    | Position::FRONT_RIGHT
                    | Position::REAR_LEFT
                    | Position::REAR_RIGHT
            }
            _ => {
                Position::FRONT_LEFT
                    | Position::FRONT_CENTER
                    | Position::FRONT_RIGHT
                    | Position::SIDE_LEFT
                    | Position::SIDE_RIGHT
            }
        };

        if self.lfe {
            pos |= Position::LFE1;
        }

        Channels::Positioned(pos)
    }

    /// The codec ID of the stream the frame is of.
    pub fn codec(&self) -> symphonia_core::codecs::audio::AudioCodecId {
        if self.eac3 { CODEC_ID_EAC3 } else { CODEC_ID_AC3 }
    }

    /// The codec parameters of the stream.
    pub fn codec_params(&self) -> AudioCodecParameters {
        let mut params = AudioCodecParameters::new();
        params
            .for_codec(self.codec())
            .with_sample_rate(self.sample_rate)
            .with_channels(self.channels());
        params
    }
}

/// An AC-3 and E-AC-3 elementary stream parser. A frame is an access unit.
#[derive(Default)]
pub struct Ac3Es {
    buf: SyncBuffer,
    info: Option<EsInfo>,
    samples: u64,
}

impl Ac3Es {
    pub fn new() -> Self {
        Default::default()
    }

    /// Find the next syncframe at or after the start of the buffer, discarding the data before
    /// it. Returns the header if the buffer holds the entire frame.
    fn sync(&mut self) -> Option<Ac3Header> {
        loop {
            let data = &self.buf[..];

            let pos = data.windows(2).position(|w| w == [0x0b, 0x77] || w == [0x77, 0x0b])?;

            if pos > 0 {
                self.buf.consume(pos);
                continue;
            }

            if data.len() < AC3_MIN_HEADER {
                return None;
            }

            match Ac3Header::parse(data) {
                Some(hdr) => return Some(hdr),
                // Not a valid header, resynchronize.
                None => self.buf.consume(1),
            }
        }
    }
}

impl EsParser for Ac3Es {
    fn push(&mut self, data: &[u8]) {
        self.buf.push(data);
    }

    fn next_frame(&mut self, flush: bool) -> Option<EsFrame> {
        loop {
            let first = self.sync()?;

            // Dependent substreams cannot start an access unit.
            if first.dependent || first.substream_id != 0 {
                self.buf.consume(first.frame_size.min(self.buf.len()));
                continue;
            }

            if self.buf.len() < first.frame_size {
                if flush {
                    self.buf.clear();
                }
                return None;
            }

            // Find the end of the access unit: the next independent frame of substream 0.
            let mut end = first.frame_size;

            loop {
                let rest = &self.buf[end..];

                if rest.len() < AC3_MIN_HEADER {
                    // The next frame header is not available yet.
                    if !flush && !rest.is_empty() {
                        return None;
                    }
                    if !flush {
                        return None;
                    }
                    break;
                }

                match Ac3Header::parse(rest) {
                    Some(next) if next.dependent || next.substream_id != 0 => {
                        if rest.len() < next.frame_size {
                            if !flush {
                                return None;
                            }
                            break;
                        }
                        end += next.frame_size;
                    }
                    _ => break,
                }
            }

            let start = self.buf.base();
            let data: Box<[u8]> = self.buf[..end].into();
            self.buf.consume(end);

            if self.info.is_none() {
                self.info = Some(EsInfo { params: first.codec_params(), rate: first.sample_rate });
                self.samples = u64::from(first.samples);
            }

            return Some(EsFrame {
                start,
                data,
                dur: u64::from(first.samples),
                trim_start: 0,
                trim_end: 0,
            });
        }
    }

    fn clear(&mut self) {
        self.buf.clear();
    }

    fn info(&self) -> Option<&EsInfo> {
        self.info.as_ref()
    }

    fn seek_start_ts(&self, target: i64) -> i64 {
        // The overlap of the transform is the only state carried over to the next frame.
        target - self.samples as i64
    }

    fn fresh(&self) -> Box<dyn EsParser> {
        Box::new(Ac3Es::new())
    }

    fn frame_grid(&self) -> u64 {
        self.samples
    }
}

// -------------------------------------------------------------------------------------------------
// AC-4
// -------------------------------------------------------------------------------------------------

/// The number of samples per channel of an AC-4 frame by `frame_rate_index` (ETSI TS 103 190-1
/// table 83, 48 kHz; the 44.1 kHz stream only uses index 13). Zero marks reserved indexes.
const AC4_FRAME_LEN: [u16; 16] =
    [1920, 1920, 2048, 1536, 1536, 960, 960, 1024, 768, 768, 512, 384, 384, 2048, 0, 0];

/// The shortest sync frame header: sync word and size, and the start of the table of contents.
const AC4_MIN_HEADER: usize = 11;

/// The header of an AC-4 sync frame (ETSI TS 103 190-1 annex C: `0xAC40` without and `0xAC41` with
/// a CRC-16 trailer).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Ac4Header {
    /// The size of the sync frame in bytes, including the sync word, the size and the CRC.
    pub frame_size: usize,
    /// The sample rate in Hz.
    pub sample_rate: u32,
    /// The number of samples per channel in the frame.
    pub samples: u32,
}

impl Ac4Header {
    /// Parse the header of the sync frame at the start of `buf` (at least [`AC4_MIN_HEADER`]
    /// bytes).
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < AC4_MIN_HEADER || buf[0] != 0xac || !matches!(buf[1], 0x40 | 0x41) {
            return None;
        }

        let crc = if buf[1] == 0x41 { 2 } else { 0 };
        let mut size = usize::from(u16::from_be_bytes([buf[2], buf[3]]));
        let mut offset = 4;
        if size == 0xffff {
            size = (usize::from(buf[4]) << 16) | (usize::from(buf[5]) << 8) | usize::from(buf[6]);
            offset = 7;
        }
        if size == 0 {
            return None;
        }

        // ac4_toc(): bitstream_version, sequence_counter, wait_frames, fs_index, frame_rate_index.
        let mut pos = offset * 8;
        let mut bits = |n: usize| {
            let mut v = 0u32;
            for _ in 0..n {
                let byte = buf.get(pos >> 3).copied().unwrap_or(0);
                v = (v << 1) | u32::from((byte >> (7 - (pos & 7))) & 1);
                pos += 1;
            }
            v
        };

        if bits(2) == 3 {
            // variable_bits(2): the version extension, groups of two bits and a continuation bit.
            for _ in 0..8 {
                bits(2);
                if bits(1) == 0 {
                    break;
                }
            }
        }
        bits(10);
        if bits(1) != 0 && bits(3) > 0 {
            bits(2);
        }
        let fs_index = bits(1);
        let samples = AC4_FRAME_LEN[bits(4) as usize];
        if samples == 0 {
            return None;
        }

        Some(Ac4Header {
            frame_size: offset + size + crc,
            sample_rate: if fs_index != 0 { 48000 } else { 44100 },
            samples: u32::from(samples),
        })
    }
}

/// An AC-4 elementary stream parser for sync frames (`0xAC40`/`0xAC41`, ATSC 3.0 and MPEG-TS). A
/// frame is an access unit.
#[derive(Default)]
pub struct Ac4Es {
    buf: SyncBuffer,
    info: Option<EsInfo>,
    samples: u64,
}

impl Ac4Es {
    pub fn new() -> Self {
        Default::default()
    }
}

impl EsParser for Ac4Es {
    fn push(&mut self, data: &[u8]) {
        self.buf.push(data);
    }

    fn next_frame(&mut self, flush: bool) -> Option<EsFrame> {
        loop {
            let data = &self.buf[..];
            let pos = data.windows(2).position(|w| w == [0xac, 0x40] || w == [0xac, 0x41])?;

            if pos > 0 {
                self.buf.consume(pos);
                continue;
            }
            if data.len() < AC4_MIN_HEADER {
                if flush {
                    self.buf.clear();
                }
                return None;
            }
            let Some(hdr) = Ac4Header::parse(data)
            else {
                // Not a valid header, resynchronize.
                self.buf.consume(1);
                continue;
            };
            if data.len() < hdr.frame_size {
                if flush {
                    self.buf.clear();
                }
                return None;
            }

            let start = self.buf.base();
            let frame: Box<[u8]> = data[..hdr.frame_size].into();
            self.buf.consume(hdr.frame_size);

            if self.info.is_none() {
                let mut params = AudioCodecParameters::new();
                params.for_codec(CODEC_ID_AC4).with_sample_rate(hdr.sample_rate);
                self.info = Some(EsInfo { params, rate: hdr.sample_rate });
            }
            self.samples = u64::from(hdr.samples);

            return Some(EsFrame {
                start,
                data: frame,
                dur: u64::from(hdr.samples),
                trim_start: 0,
                trim_end: 0,
            });
        }
    }

    fn clear(&mut self) {
        self.buf.clear();
    }

    fn info(&self) -> Option<&EsInfo> {
        self.info.as_ref()
    }

    fn seek_start_ts(&self, target: i64) -> i64 {
        // The overlap of the transform is the only state carried over to the next frame; the
        // decoder needs an I-frame, which the stream provides about once a second, to resume.
        target - self.samples as i64
    }

    fn fresh(&self) -> Box<dyn EsParser> {
        Box::new(Ac4Es::new())
    }

    fn frame_grid(&self) -> u64 {
        self.samples
    }
}

// -------------------------------------------------------------------------------------------------
// DTS
// -------------------------------------------------------------------------------------------------

/// The sample rates by `SFREQ` of the DTS core.
const DCA_SAMPLE_RATES: [u32; 16] =
    [0, 8000, 16000, 32000, 0, 0, 11025, 22050, 44100, 0, 0, 12000, 24000, 48000, 0, 0];

/// The number of full bandwidth channels by `AMODE` of the DTS core.
const DCA_CHANNELS: [u8; 16] = [1, 2, 2, 2, 2, 3, 3, 4, 4, 5, 6, 6, 6, 7, 8, 8];

/// The extension substream sync word.
const DCA_SYNC_EXSS: u32 = 0x6458_2025;

/// The sizes of the sync words of the DTS core in the buffer and the minimum size of the header
/// that must be available to parse it.
const DCA_MIN_HEADER: usize = 16;

/// The bitstream form of a DTS core frame.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DcaForm {
    /// 16-bit words, big-endian.
    Be16,
    /// 16-bit words, little-endian.
    Le16,
    /// 14-bit words in 16-bit big-endian words.
    Be14,
    /// 14-bit words in 16-bit little-endian words.
    Le14,
}

/// The header of a DTS core substream frame.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DcaHeader {
    /// The bitstream form.
    pub form: DcaForm,
    /// The size of the frame in the stream, in bytes.
    pub frame_size: usize,
    /// The number of samples per channel in the frame.
    pub samples: u32,
    /// The sample rate in Hz of the core.
    pub sample_rate: u32,
    /// The audio mode.
    pub amode: u8,
    /// True if there is a low frequency effects channel.
    pub lfe: bool,
}

/// The form of the DTS core sync word at the start of `buf`.
fn dca_core_form(buf: &[u8]) -> Option<DcaForm> {
    match buf {
        [0x7f, 0xfe, 0x80, 0x01, ..] => Some(DcaForm::Be16),
        [0xfe, 0x7f, 0x01, 0x80, ..] => Some(DcaForm::Le16),
        [0x1f, 0xff, 0xe8, 0x00, 0x07, b, ..] if b & 0xf0 == 0xf0 => Some(DcaForm::Be14),
        [0xff, 0x1f, 0x00, 0xe8, b, 0x07, ..] if b & 0xf0 == 0xf0 => Some(DcaForm::Le14),
        _ => None,
    }
}

/// Convert the start of a DTS frame in any form to the 16-bit big-endian form. Returns the number
/// of bytes written.
fn dca_to_be16(form: DcaForm, src: &[u8], dst: &mut [u8; DCA_MIN_HEADER]) -> usize {
    match form {
        DcaForm::Be16 => {
            let n = src.len().min(DCA_MIN_HEADER);
            dst[..n].copy_from_slice(&src[..n]);
            n
        }
        DcaForm::Le16 => {
            let n = src.len().min(DCA_MIN_HEADER) & !1;
            for i in (0..n).step_by(2) {
                dst[i] = src[i + 1];
                dst[i + 1] = src[i];
            }
            n
        }
        DcaForm::Be14 | DcaForm::Le14 => {
            let mut acc = 0u32;
            let mut nbits = 0;
            let mut n = 0;

            for pair in src.chunks_exact(2) {
                let word = if form == DcaForm::Be14 {
                    u16::from_be_bytes([pair[0], pair[1]])
                }
                else {
                    u16::from_le_bytes([pair[0], pair[1]])
                };

                acc = (acc << 14) | u32::from(word & 0x3fff);
                nbits += 14;

                while nbits >= 8 && n < DCA_MIN_HEADER {
                    nbits -= 8;
                    dst[n] = (acc >> nbits) as u8;
                    n += 1;
                }

                if n == DCA_MIN_HEADER {
                    break;
                }
            }

            n
        }
    }
}

impl DcaHeader {
    /// Parse the header of the core substream frame at the start of `buf`. Returns `None` if the
    /// header is not valid, or there are not enough bytes to parse it.
    pub fn parse(buf: &[u8]) -> Option<DcaHeader> {
        let form = dca_core_form(buf)?;

        let mut h = [0u8; DCA_MIN_HEADER];
        let n = dca_to_be16(form, buf, &mut h);

        // The header up to the LFE flag is 11 bytes.
        if n < 11 {
            return None;
        }

        let bits = u128::from_be_bytes(h);
        let field = |off: u32, len: u32| -> u32 {
            ((bits >> (128 - off - len)) & ((1u128 << len) - 1)) as u32
        };

        // Frame type (1), deficit samples (5), CRC present (1).
        let deficit = field(33, 5) + 1;

        if deficit != 32 {
            return None;
        }

        let blocks = field(39, 7) + 1;

        // The number of PCM sample blocks must be a multiple of 8.
        if blocks & 7 != 0 {
            return None;
        }

        let fsize = field(46, 14) as usize + 1;

        if fsize < 96 {
            return None;
        }

        let amode = field(60, 6) as u8;

        let sample_rate = DCA_SAMPLE_RATES[field(66, 4) as usize];

        if sample_rate == 0 {
            return None;
        }

        // Skip the transmission bit rate (5), fixed bit (1), DRC (1), time stamp (1), aux (1),
        // HDCD (1), extension audio descriptor (3), extended coding (1), audio sync insertion (1).
        let lfe_flag = field(85, 2);

        if lfe_flag == 3 || usize::from(amode) >= DCA_CHANNELS.len() {
            return None;
        }

        // 14-bit forms carry 14 bits per 16-bit word.
        let frame_size = match form {
            DcaForm::Be16 | DcaForm::Le16 => fsize,
            DcaForm::Be14 | DcaForm::Le14 => (fsize * 8).div_ceil(14) * 2,
        };

        Some(DcaHeader {
            form,
            frame_size,
            samples: blocks * 32,
            sample_rate,
            amode,
            lfe: lfe_flag != 0,
        })
    }

    /// The number of channels of the core, including the LFE channel.
    pub fn channel_count(&self) -> u16 {
        u16::from(DCA_CHANNELS[usize::from(self.amode)]) + u16::from(self.lfe)
    }

    /// The codec parameters of the stream.
    pub fn codec_params(&self) -> AudioCodecParameters {
        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_DCA)
            .with_sample_rate(self.sample_rate)
            .with_channels(Channels::Discrete(self.channel_count()));
        params
    }
}

/// The size in bytes of the extension substream (DTS-HD) at the start of `buf`, if it has the
/// sync word and the header is complete.
pub fn dca_exss_size(buf: &[u8]) -> Option<usize> {
    if buf.len() < 10 || u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) != DCA_SYNC_EXSS {
        return None;
    }

    // UserDefinedBits (8), nExtSSIndex (2), bHeaderSizeType (1), then the header size and frame
    // size: 8 and 16 bits, or 12 and 20 bits.
    let bits = u64::from_be_bytes([0, 0, buf[4], buf[5], buf[6], buf[7], buf[8], buf[9]]);

    let size =
        if buf[5] & 0x20 != 0 { ((bits >> 5) & 0xfffff) + 1 } else { ((bits >> 13) & 0xffff) + 1 };

    Some(size as usize)
}

/// Find the next DTS sync word, core or extension substream, at or after `from`.
fn dca_find_marker(buf: &[u8], from: usize) -> Option<usize> {
    (from..buf.len().saturating_sub(5)).find(|&i| {
        dca_core_form(&buf[i..]).is_some() || buf[i..].starts_with(&DCA_SYNC_EXSS.to_be_bytes())
    })
}

/// A DTS elementary stream parser. A frame is a core substream frame and the extension substream
/// after it, if any.
#[derive(Default)]
pub struct DcaEs {
    buf: SyncBuffer,
    info: Option<EsInfo>,
    samples: u64,
}

impl DcaEs {
    pub fn new() -> Self {
        Default::default()
    }
}

impl EsParser for DcaEs {
    fn push(&mut self, data: &[u8]) {
        self.buf.push(data);
    }

    fn next_frame(&mut self, flush: bool) -> Option<EsFrame> {
        loop {
            // Skip to the next core frame. Extension substreams without a core are not decoded.
            let pos = match dca_find_marker(&self.buf, 0) {
                Some(pos) => pos,
                None => {
                    // Keep the tail that may hold a partial sync word.
                    let keep = self.buf.len().min(5);
                    self.buf.consume(self.buf.len() - keep);
                    if flush {
                        self.buf.clear();
                    }
                    return None;
                }
            };

            self.buf.consume(pos);

            let Some(hdr) = DcaHeader::parse(&self.buf)
            else {
                if let Some(size) = dca_exss_size(&self.buf) {
                    if self.buf.len() < size && !flush {
                        return None;
                    }
                    self.buf.consume(size.min(self.buf.len()));
                }
                else if dca_core_form(&self.buf).is_none() || self.buf.len() >= DCA_MIN_HEADER {
                    // Not a valid frame.
                    self.buf.consume(1);
                }
                else if flush {
                    self.buf.clear();
                    return None;
                }
                else {
                    return None;
                };
                continue;
            };

            // Look for the end of the frame: the next sync word at or after the minimum size.
            let min = hdr.frame_size;

            let end = match dca_find_marker(&self.buf, min) {
                Some(next) if next - min < 4 && dca_exss_size(&self.buf[next..]).is_some() => {
                    // A core followed by its extension substream.
                    let size = dca_exss_size(&self.buf[next..]).unwrap_or(0);
                    if self.buf.len() < next + size {
                        if !flush {
                            return None;
                        }
                        self.buf.len()
                    }
                    else {
                        next + size
                    }
                }
                Some(next) => next,
                None if flush && self.buf.len() >= min => self.buf.len(),
                None => return None,
            };

            let start = self.buf.base();
            let data: Box<[u8]> = self.buf[..end].into();
            self.buf.consume(end);

            if self.info.is_none() {
                self.info = Some(EsInfo { params: hdr.codec_params(), rate: hdr.sample_rate });
                self.samples = u64::from(hdr.samples);
            }

            return Some(EsFrame {
                start,
                data,
                dur: u64::from(hdr.samples),
                trim_start: 0,
                trim_end: 0,
            });
        }
    }

    fn clear(&mut self) {
        self.buf.clear();
    }

    fn info(&self) -> Option<&EsInfo> {
        self.info.as_ref()
    }

    fn seek_start_ts(&self, target: i64) -> i64 {
        // The subband filter and its overlap carry state over from the previous frame.
        target - self.samples as i64
    }

    fn fresh(&self) -> Box<dyn EsParser> {
        Box::new(DcaEs::new())
    }

    fn frame_grid(&self) -> u64 {
        self.samples
    }
}

// -------------------------------------------------------------------------------------------------
// TrueHD and MLP
// -------------------------------------------------------------------------------------------------

/// The size of the access unit header in bytes.
pub const MLP_AU_HEADER: usize = 4;

/// The minimum number of bytes required to parse the major sync of an access unit: the access
/// unit header and the part of the major sync with the substream count.
const MLP_MIN_MAJOR_SYNC: usize = MLP_AU_HEADER + 17;

/// The header of a major sync of a TrueHD or MLP stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MlpHeader {
    /// True for MLP, false for TrueHD.
    pub mlp: bool,
    /// The sample rate in Hz of the first group.
    pub sample_rate: u32,
    /// The number of samples per channel in an access unit.
    pub samples: u32,
    /// The number of substreams.
    pub num_substreams: u8,
    /// The channels, or the number of channels if the layout is not known.
    pub channels: Channels,
}

/// The positions of the TrueHD channel assignment groups, in bit order.
const TRUEHD_GROUPS: [Position; 13] = [
    Position::FRONT_LEFT.union(Position::FRONT_RIGHT),
    Position::FRONT_CENTER,
    Position::LFE1,
    Position::SIDE_LEFT.union(Position::SIDE_RIGHT),
    Position::TOP_FRONT_LEFT.union(Position::TOP_FRONT_RIGHT),
    Position::FRONT_LEFT_CENTER.union(Position::FRONT_RIGHT_CENTER),
    Position::REAR_LEFT.union(Position::REAR_RIGHT),
    Position::REAR_CENTER,
    Position::TOP_CENTER,
    Position::SURROUND_DIRECT_LEFT.union(Position::SURROUND_DIRECT_RIGHT),
    Position::FRONT_LEFT_WIDE.union(Position::FRONT_RIGHT_WIDE),
    Position::TOP_FRONT_CENTER,
    Position::LFE2,
];

impl MlpHeader {
    /// Parse the major sync of the access unit at the start of `buf`: it must begin with the
    /// 4-byte access unit header. Returns `None` if there is no major sync or it is not valid.
    pub fn parse(buf: &[u8]) -> Option<MlpHeader> {
        if buf.len() < MLP_MIN_MAJOR_SYNC {
            return None;
        }

        let sync = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);

        if sync & 0xffff_fffe != 0xf872_6fba {
            return None;
        }

        let mlp = sync & 1 != 0;
        let ratebits = if mlp { buf[9] >> 4 } else { buf[8] >> 4 };

        // 0xf signals that the group is not present.
        if ratebits == 0xf {
            return None;
        }

        let sample_rate = (if ratebits & 8 != 0 { 44100 } else { 48000 }) << (ratebits & 7);

        let channels = if mlp {
            // The channel arrangement is the low 5 bits of the 16 bits after the sample rates.
            const FL: Position = Position::FRONT_LEFT;
            const FR: Position = Position::FRONT_RIGHT;
            const FC: Position = Position::FRONT_CENTER;
            const LFE: Position = Position::LFE1;
            const BL: Position = Position::REAR_LEFT;
            const BR: Position = Position::REAR_RIGHT;
            const BC: Position = Position::REAR_CENTER;

            let mask = match buf[11] & 0x1f {
                0 => FC,
                1 => FL.union(FR),
                2 => FL.union(FR).union(BC),
                3 => FL.union(FR).union(BL).union(BR),
                4 => FL.union(FR).union(LFE),
                5 => FL.union(FR).union(LFE).union(BC),
                6 => FL.union(FR).union(LFE).union(BL).union(BR),
                7 => FL.union(FR).union(FC),
                8 => FL.union(FR).union(FC).union(BC),
                9 => FL.union(FR).union(FC).union(BL).union(BR),
                10 => FL.union(FR).union(FC).union(LFE),
                11 => FL.union(FR).union(FC).union(LFE).union(BC),
                12 => FL.union(FR).union(FC).union(LFE).union(BL).union(BR),
                13 => FL.union(FR).union(FC).union(BC),
                14 => FL.union(FR).union(FC).union(BL).union(BR),
                15 => FL.union(FR).union(FC).union(LFE),
                16 => FL.union(FR).union(FC).union(LFE).union(BC),
                17 => FL.union(FR).union(FC).union(LFE).union(BL).union(BR),
                18 => FL.union(FR).union(LFE).union(BL).union(BR),
                19 => FL.union(FR).union(FC).union(BL).union(BR),
                20 => FL.union(FR).union(FC).union(LFE).union(BL).union(BR),
                _ => return None,
            };

            Channels::Positioned(mask)
        }
        else {
            // The 5-bit channel assignment of the first stream, or the 13-bit assignment of the
            // second stream if it is present. Both assign the groups in the same bit order.
            let stream1 = u32::from(buf[9] & 0x0f) << 1 | u32::from(buf[10] >> 7);
            let stream2 = u32::from(buf[10] & 0x1f) << 8 | u32::from(buf[11]);
            let assignment = if stream2 != 0 { stream2 } else { stream1 };

            let mut mask = Position::empty();
            for (bit, group) in TRUEHD_GROUPS.iter().enumerate() {
                if assignment & (1 << bit) != 0 {
                    mask |= *group;
                }
            }

            Channels::Positioned(mask)
        };

        Some(MlpHeader {
            mlp,
            sample_rate,
            samples: 40 << (ratebits & 7),
            num_substreams: buf[MLP_AU_HEADER + 16] >> 4,
            channels,
        })
    }

    /// The codec parameters of the stream.
    pub fn codec_params(&self) -> AudioCodecParameters {
        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_TRUEHD)
            .with_sample_rate(self.sample_rate)
            .with_channels(self.channels.clone());
        params
    }
}

/// The length in bytes of the access unit that starts at `buf`, from its header.
pub fn mlp_access_unit_len(buf: &[u8]) -> Option<usize> {
    match buf {
        [b0, b1, ..] => Some((usize::from(b0 & 0x0f) << 8 | usize::from(*b1)) * 2),
        _ => None,
    }
}

/// Verify the parity nibble of the access unit header and substream headers of an access unit
/// without a major sync, that has `num_substreams` substreams.
fn mlp_parity_ok(au: &[u8], num_substreams: u8) -> bool {
    let mut parity = 0u8;
    let mut p = 0;

    for i in -1..i32::from(num_substreams) {
        let Some(&[a, b]) = au.get(p..p + 2).and_then(|s| <&[u8; 2]>::try_from(s).ok())
        else {
            return false;
        };

        parity ^= a ^ b;
        p += 2;

        if i < 0 || a & 0x80 != 0 {
            let Some(&[c, d]) = au.get(p..p + 2).and_then(|s| <&[u8; 2]>::try_from(s).ok())
            else {
                return false;
            };

            parity ^= c ^ d;
            p += 2;
        }
    }

    ((parity >> 4) ^ parity) & 0xf == 0xf
}

/// A TrueHD and MLP elementary stream parser. A frame is an access unit.
#[derive(Default)]
pub struct TrueHdEs {
    buf: SyncBuffer,
    info: Option<EsInfo>,
    samples: u64,
    num_substreams: u8,
    in_sync: bool,
}

impl TrueHdEs {
    pub fn new() -> Self {
        Default::default()
    }
}

impl EsParser for TrueHdEs {
    fn push(&mut self, data: &[u8]) {
        self.buf.push(data);
    }

    fn next_frame(&mut self, flush: bool) -> Option<EsFrame> {
        loop {
            if !self.in_sync {
                // Find the next major sync: it is 4 bytes into the access unit.
                let data = &self.buf[..];

                let pos = data.windows(4).position(|w| {
                    u32::from_be_bytes([w[0], w[1], w[2], w[3]]) & 0xffff_fffe == 0xf872_6fba
                });

                match pos {
                    Some(pos) if pos >= MLP_AU_HEADER => {
                        self.buf.consume(pos - MLP_AU_HEADER);
                        self.in_sync = true;
                    }
                    Some(pos) => {
                        // The access unit header of this major sync was not received.
                        self.buf.consume(pos + 1);
                        continue;
                    }
                    None => {
                        // Keep the tail that may hold the access unit header and a partial sync
                        // word.
                        let keep = self.buf.len().min(MLP_AU_HEADER + 3);
                        self.buf.consume(self.buf.len() - keep);
                        if flush {
                            self.buf.clear();
                        }
                        return None;
                    }
                }
            }

            let len = match mlp_access_unit_len(&self.buf) {
                Some(len) if len >= MLP_AU_HEADER => len,
                Some(_) => {
                    self.in_sync = false;
                    self.buf.consume(1);
                    continue;
                }
                None => return None,
            };

            if self.buf.len() < len {
                if flush {
                    self.buf.clear();
                }
                return None;
            }

            let is_major = self.buf.len() >= 8
                && u32::from_be_bytes([self.buf[4], self.buf[5], self.buf[6], self.buf[7]])
                    & 0xffff_fffe
                    == 0xf872_6fba;

            if is_major {
                let Some(hdr) = MlpHeader::parse(&self.buf[..len])
                else {
                    self.in_sync = false;
                    self.buf.consume(1);
                    continue;
                };

                self.num_substreams = hdr.num_substreams;
                self.samples = u64::from(hdr.samples);

                if self.info.is_none() {
                    self.info = Some(EsInfo { params: hdr.codec_params(), rate: hdr.sample_rate });
                }
            }
            else if !mlp_parity_ok(&self.buf[..len], self.num_substreams) {
                self.in_sync = false;
                self.buf.consume(1);
                continue;
            }

            let start = self.buf.base();
            let data: Box<[u8]> = self.buf[..len].into();
            self.buf.consume(len);

            return Some(EsFrame { start, data, dur: self.samples, trim_start: 0, trim_end: 0 });
        }
    }

    fn clear(&mut self) {
        self.buf.clear();
        // The data that follows starts an access unit (a PES packet or a seek position). If it
        // does not, the length and parity checks fall back to the major sync search.
        self.in_sync = self.info.is_some();
    }

    fn info(&self) -> Option<&EsInfo> {
        self.info.as_ref()
    }

    fn seek_start_ts(&self, target: i64) -> i64 {
        // Decoding starts at a major sync, which has all the state the decoder needs. They are
        // typically 16 access units apart (ffmpeg's muxer: at most 128), so start that far back to
        // be sure that a major sync follows.
        target - (128 * self.samples.max(40)) as i64
    }

    fn fresh(&self) -> Box<dyn EsParser> {
        // The stream has been probed: its access units are accepted without a major sync.
        Box::new(TrueHdEs {
            info: self.info.clone(),
            samples: self.samples,
            num_substreams: self.num_substreams,
            in_sync: self.info.is_some(),
            ..Default::default()
        })
    }

    fn frame_grid(&self) -> u64 {
        self.samples
    }
}
