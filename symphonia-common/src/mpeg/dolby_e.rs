// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Dolby E frame header parsing and the word stream the Dolby E decoder reads.
//!
//! A Dolby E frame is a sequence of 16, 20, or 24-bit words, stored big-endian in 2, 3, and 3
//! bytes per word (the 20-bit words are in the upper 20 bits of their 3 bytes). The first word is
//! a sync word, which also tells the word size:
//!
//!  * 24-bit: `0x07888e`, 20-bit: `0x0788e`, 16-bit: `0x078e`.
//!
//! The lowest bit of the sync word tells if the words that follow are scrambled with a key word.
//! The header, the audio of each group of channels, and the metadata extension and metering
//! blocks are each scrambled with a key (the XOR of every word of the block).
//!
//! The parsing follows FFmpeg's `libavcodec/dolby_e_parse.c` (LGPL-2.1-or-later, Copyright (c)
//! 2017 foo86), as Dolby E has no public specification.

use symphonia_core::audio::{Channels, Position};
use symphonia_core::errors::{Result, decode_error};

/// The number of samples of every channel in a frame.
pub const FRAME_SAMPLES: usize = 1792;

/// The maximum number of channels of a frame.
pub const MAX_CHANNELS: usize = 8;

/// The maximum number of words that are converted at once (the size fields have 10 bits).
const MAX_WORDS: usize = 1024;

/// The size in bytes of the buffer of converted words, and its zeroed tail that bit reads past
/// the end of the words see.
const BUF_LEN: usize = MAX_WORDS * 3;
const BUF_PAD: usize = 8;

/// The highest valid program configuration.
const MAX_PROG_CONF: usize = 23;

/// The number of programs of each program configuration.
const NB_PROGRAMS: [u8; MAX_PROG_CONF + 1] =
    [2, 3, 2, 3, 4, 5, 4, 5, 6, 7, 8, 1, 2, 3, 3, 4, 5, 6, 1, 2, 3, 4, 1, 1];

/// The number of channels of each program configuration.
const NB_CHANNELS: [u8; MAX_PROG_CONF + 1] =
    [8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 6, 6, 6, 6, 6, 6, 6, 4, 4, 4, 4, 8, 8];

/// The "sample rate" of each frame rate code (0 is invalid). This is the rate at which a frame of
/// 1792 samples has the duration of a video frame (23.976, 24, 25, 29.97, and 30 fps), as FFmpeg
/// reports it; the PCM carrying the stream runs at 48 kHz.
const SAMPLE_RATE: [u32; 16] = [0, 42965, 43008, 44800, 53706, 53760, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// The channels of a frame with `nb_channels` channels: 4.0, 5.1 (side), or 7.1.
pub fn channels(nb_channels: usize) -> Channels {
    let mut pos = Position::FRONT_LEFT | Position::FRONT_RIGHT | Position::FRONT_CENTER;

    match nb_channels {
        4 => pos |= Position::REAR_CENTER,
        6 => pos |= Position::LFE1 | Position::SIDE_LEFT | Position::SIDE_RIGHT,
        _ => {
            pos |= Position::LFE1
                | Position::REAR_LEFT
                | Position::REAR_RIGHT
                | Position::SIDE_LEFT
                | Position::SIDE_RIGHT
        }
    }

    Channels::Positioned(pos)
}

/// The words of a frame, and the bit reader of the words that were converted last.
pub struct WordStream {
    word_bits: u32,
    word_bytes: usize,
    key_present: bool,
    /// The byte offset of the next word of the frame to convert or skip.
    pos: usize,
    /// The number of words of the frame after `pos`.
    left: usize,
    /// The converted words: a bit string that the bit reader reads.
    buf: Box<[u8; BUF_LEN + BUF_PAD]>,
    bit_pos: usize,
    bit_len: usize,
}

impl Default for WordStream {
    fn default() -> Self {
        Self::new()
    }
}

impl WordStream {
    pub fn new() -> Self {
        WordStream {
            word_bits: 16,
            word_bytes: 2,
            key_present: false,
            pos: 0,
            left: 0,
            buf: Box::new([0; BUF_LEN + BUF_PAD]),
            bit_pos: 0,
            bit_len: 0,
        }
    }

    /// Start reading the frame `data`: read the sync word, and position at the word after it.
    pub fn start(&mut self, data: &[u8]) -> Result<()> {
        if data.len() < 3 {
            return decode_error("dolby_e: frame too short");
        }

        let hdr = u32::from_be_bytes([0, data[0], data[1], data[2]]);

        self.word_bits = if hdr & 0xff_fffe == 0x07_888e {
            24
        }
        else if hdr & 0xff_ffe0 == 0x07_88e0 {
            20
        }
        else if hdr & 0xff_fe00 == 0x07_8e00 {
            16
        }
        else {
            return decode_error("dolby_e: invalid frame header");
        };

        self.word_bytes = (self.word_bits as usize + 7) >> 3;
        self.pos = self.word_bytes;
        self.left = data.len() / self.word_bytes - 1;
        self.key_present = (hdr >> (24 - self.word_bits)) & 1 != 0;

        Ok(())
    }

    /// The size of a word in bits: 16, 20, or 24.
    pub fn word_bits(&self) -> u32 {
        self.word_bits
    }

    /// True if the blocks of the frame are scrambled with a key word.
    pub fn key_present(&self) -> bool {
        self.key_present
    }

    /// Skip `n` words of the frame.
    pub fn skip_words(&mut self, n: usize) -> Result<()> {
        if n > self.left {
            return decode_error("dolby_e: frame too short");
        }

        self.pos += n * self.word_bytes;
        self.left -= n;
        Ok(())
    }

    /// If the frame has a key, skip and return the key word (otherwise, 0).
    pub fn parse_key(&mut self, data: &[u8]) -> Result<u32> {
        if !self.key_present {
            return Ok(0);
        }

        let at = self.pos;
        self.skip_words(1)?;

        let word = data.get(at..at + self.word_bytes).unwrap_or(&[]);
        let mut key = 0u32;
        for &b in word {
            key = key << 8 | u32::from(b);
        }

        // A 20-bit word is in the upper bits of its 3 bytes.
        Ok(if self.word_bits == 20 { key >> 4 } else { key })
    }

    /// Descramble `nb_words` words from the current position (without skipping them) with `key`,
    /// and start the bit reader at the first of them.
    pub fn convert(&mut self, data: &[u8], nb_words: usize, key: u32) -> Result<()> {
        if nb_words > MAX_WORDS || nb_words > self.left {
            return decode_error("dolby_e: frame too short");
        }

        let src = match data.get(self.pos..self.pos + nb_words * self.word_bytes) {
            Some(src) => src,
            None => return decode_error("dolby_e: frame too short"),
        };

        let len = match self.word_bits {
            16 => {
                for (d, s) in self.buf.chunks_exact_mut(2).zip(src.chunks_exact(2)) {
                    let w = u16::from_be_bytes([s[0], s[1]]) ^ key as u16;
                    d.copy_from_slice(&w.to_be_bytes());
                }
                nb_words * 2
            }
            20 => {
                let mut acc = 0u64;
                let mut n = 0;
                let mut len = 0;

                for s in src.chunks_exact(3) {
                    let w = (u32::from_be_bytes([0, s[0], s[1], s[2]]) >> 4) ^ key;
                    acc = (acc << 20) | u64::from(w & 0xf_ffff);
                    n += 20;

                    while n >= 8 {
                        n -= 8;
                        self.buf[len] = (acc >> n) as u8;
                        len += 1;
                    }

                    acc &= (1 << n) - 1;
                }

                if n > 0 {
                    self.buf[len] = (acc << (8 - n)) as u8;
                    len += 1;
                }

                len
            }
            _ => {
                for (d, s) in self.buf.chunks_exact_mut(3).zip(src.chunks_exact(3)) {
                    let w = u32::from_be_bytes([0, s[0], s[1], s[2]]) ^ key;
                    d.copy_from_slice(&w.to_be_bytes()[1..]);
                }
                nb_words * 3
            }
        };

        // Bit reads past the end of the words see zeros.
        self.buf[len..len + BUF_PAD].fill(0);

        self.bit_pos = 0;
        self.bit_len = nb_words * self.word_bits as usize;
        Ok(())
    }

    /// Read `n` (at most 32) bits. Reads past the end of the converted words return zeros.
    pub fn get_bits(&mut self, n: u32) -> u32 {
        let start = self.bit_pos;
        self.bit_pos += n as usize;

        if n == 0 || start >= self.bit_len {
            return 0;
        }

        let byte = start >> 3;
        let mut word = [0; 8];
        word.copy_from_slice(&self.buf[byte..byte + 8]);

        ((u64::from_be_bytes(word) << (start & 7)) >> (64 - n)) as u32
    }

    /// Read `n` (between 1 and 32) bits as a two's complement signed value.
    pub fn get_sbits(&mut self, n: u32) -> i32 {
        (self.get_bits(n) << (32 - n)) as i32 >> (32 - n)
    }

    /// Read 1 bit.
    pub fn get_bit(&mut self) -> bool {
        self.get_bits(1) != 0
    }

    /// Skip `n` bits.
    pub fn skip_bits(&mut self, n: usize) {
        self.bit_pos += n;
    }

    /// The number of bits left to read in the converted words (negative after reading past the
    /// end).
    pub fn bits_left(&self) -> isize {
        self.bit_len as isize - self.bit_pos as isize
    }
}

/// The header of a Dolby E frame: the coded values of the metadata up to the gains of the
/// channels.
#[derive(Clone, Debug)]
pub struct DolbyEHeader {
    pub prog_conf: usize,
    pub nb_channels: usize,
    pub nb_programs: usize,
    /// The frame rate code (selects the sample rate) and that of the original material.
    pub fr_code: usize,
    pub fr_code_orig: usize,
    pub sample_rate: u32,
    /// The size in words of the (scrambled block of the) audio of each channel.
    pub ch_size: [u16; MAX_CHANNELS],
    /// The size in words of the metadata extension and the metering blocks.
    pub mtd_ext_size: u16,
    pub meter_size: u16,
    /// The revision of the encoder of each channel.
    pub rev_id: [u8; MAX_CHANNELS],
    /// The gain applied at the start and at the end of the frame, to each channel.
    pub begin_gain: [u16; MAX_CHANNELS],
    pub end_gain: [u16; MAX_CHANNELS],
}

impl DolbyEHeader {
    /// Parse the header of the frame `data`. On success, `words` is positioned after the
    /// metadata, at the audio of the first channels.
    pub fn parse(words: &mut WordStream, data: &[u8]) -> Result<Self> {
        words.start(data)?;

        let key = words.parse_key(data)?;
        words.convert(data, 1, key)?;

        words.skip_bits(4);
        let mtd_size = words.get_bits(10) as usize;

        if mtd_size == 0 {
            return decode_error("dolby_e: invalid metadata size");
        }

        words.convert(data, mtd_size, key)?;

        words.skip_bits(14);
        let prog_conf = words.get_bits(6) as usize;

        if prog_conf > MAX_PROG_CONF {
            return decode_error("dolby_e: invalid program configuration");
        }

        let nb_channels = usize::from(NB_CHANNELS[prog_conf]);
        let nb_programs = usize::from(NB_PROGRAMS[prog_conf]);

        let fr_code = words.get_bits(4) as usize;
        let fr_code_orig = words.get_bits(4) as usize;
        let sample_rate = SAMPLE_RATE[fr_code];

        if sample_rate == 0 || SAMPLE_RATE[fr_code_orig] == 0 {
            return decode_error("dolby_e: invalid frame rate code");
        }

        words.skip_bits(88);

        let mut header = DolbyEHeader {
            prog_conf,
            nb_channels,
            nb_programs,
            fr_code,
            fr_code_orig,
            sample_rate,
            ch_size: [0; MAX_CHANNELS],
            mtd_ext_size: 0,
            meter_size: 0,
            rev_id: [0; MAX_CHANNELS],
            begin_gain: [0; MAX_CHANNELS],
            end_gain: [0; MAX_CHANNELS],
        };

        for size in &mut header.ch_size[..nb_channels] {
            *size = words.get_bits(10) as u16;
        }

        header.mtd_ext_size = words.get_bits(8) as u16;
        header.meter_size = words.get_bits(8) as u16;

        words.skip_bits(10 * nb_programs);

        for ch in 0..nb_channels {
            header.rev_id[ch] = words.get_bits(4) as u8;
            words.skip_bits(1);
            header.begin_gain[ch] = words.get_bits(10) as u16;
            header.end_gain[ch] = words.get_bits(10) as u16;
        }

        if words.bits_left() < 0 {
            return decode_error("dolby_e: read past the end of the metadata");
        }

        words.skip_words(mtd_size + 1)?;

        Ok(header)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_bit_reader() {
        let mut ws = WordStream::new();
        // A 16-bit frame without a key: the sync word, and two words.
        let data = [0x07, 0x8e, 0xa5, 0x5a, 0x0f, 0xf0];
        ws.start(&data).unwrap();
        assert_eq!(ws.word_bits(), 16);
        assert!(!ws.key_present());
        ws.convert(&data, 2, 0).unwrap();
        assert_eq!(ws.get_bits(4), 0xa);
        assert_eq!(ws.get_sbits(4), 5);
        assert_eq!(ws.get_sbits(4), 5);
        assert_eq!(ws.get_bits(12), 0xa0f);
        assert_eq!(ws.bits_left(), 8);
        assert_eq!(ws.get_bits(8), 0xf0);
        assert_eq!(ws.bits_left(), 0);
        // Reads past the end are zeros.
        assert_eq!(ws.get_bits(8), 0);
        assert_eq!(ws.bits_left(), -8);
    }

    #[test]
    fn verify_key_and_20_bit_words() {
        let mut ws = WordStream::new();
        // A 20-bit frame with a key (the sync word has its low bit set).
        let data = [0x07, 0x88, 0xf0, 0x12, 0x34, 0x50, 0xab, 0xcd, 0xe0, 0x00, 0x01, 0x20];
        ws.start(&data).unwrap();
        assert_eq!(ws.word_bits(), 20);
        assert!(ws.key_present());
        let key = ws.parse_key(&data).unwrap();
        assert_eq!(key, 0x12345);
        ws.convert(&data, 2, key).unwrap();
        assert_eq!(ws.get_bits(20), 0xabcde ^ 0x12345);
        assert_eq!(ws.get_bits(20), 0x00012 ^ 0x12345);
        assert_eq!(ws.bits_left(), 0);
    }

    #[test]
    fn rejects_invalid_headers() {
        let mut ws = WordStream::new();
        assert!(ws.start(&[0x12, 0x34, 0x56, 0x78]).is_err());
        assert!(ws.start(&[0x07]).is_err());
    }
}
