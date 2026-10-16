// Symphonia Shorten bundle
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Shorten block decoder.
//!
//! A Shorten stream is a bit stream of commands (`FN_*`). Each audio command codes one block of one
//! channel: Rice coded residuals and either a fixed polynomial predictor (`FN_DIFF0..3`) or
//! transmitted quantized LPC coefficients (`FN_QLPC`). The offset ("mean") of every channel is
//! tracked over the last `nmean` blocks and removed before prediction.
//!
//! Ported from `shorten_decode_frame` and `decode_subframe_lpc` of FFmpeg's
//! `libavcodec/shorten.c` (LGPL-2.1+), see `NOTICE`. All arithmetic reproduces the 32-bit
//! wrapping arithmetic of the C code.

use symphonia_core::errors::{Result, decode_error, unsupported_error};

use crate::bits::{BitReader, eof};
use crate::header::{
    FNSIZE, Header, LPCQSIZE, MAX_BLOCKSIZE, VERBATIM_BYTE_SIZE, VERBATIM_CKSIZE_SIZE, read_uint,
};

/// Rice parameter of the residual energy (`ENERGYSIZE`).
const ENERGYSIZE: u32 = 3;
/// Rice parameter of the bit shift (`BITSHIFTSIZE`).
const BITSHIFTSIZE: u32 = 2;
/// Fractional bits of the transmitted LPC coefficients (`LPCQUANT`).
const LPCQUANT: u32 = 5;

const FN_DIFF3: u32 = 3;
const FN_QUIT: u32 = 4;
const FN_BLOCKSIZE: u32 = 5;
const FN_BITSHIFT: u32 = 6;
const FN_QLPC: u32 = 7;
const FN_ZERO: u32 = 8;
const FN_VERBATIM: u32 = 9;

/// The coefficients of the fixed polynomial predictors `FN_DIFF0..3` (`fixed_coeffs`).
const FIXED_COEFFS: [[i32; 3]; 4] = [[0, 0, 0], [1, 0, 0], [2, -1, 0], [3, -3, 1]];

/// The outcome of decoding one group of commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Group {
    /// One block of every channel was decoded.
    Audio,
    /// An `FN_QUIT` command ended the stream.
    Quit,
}

/// Decoder state: everything that carries over from one block to the next.
pub(crate) struct Core {
    header: Header,
    nwrap: usize,
    mean_len: usize,
    /// The current block size.
    blocksize: usize,
    bitshift: u32,
    /// Per channel: `nwrap` samples of history followed by the current block.
    decoded: Vec<Vec<i32>>,
    /// Per channel: the running means of the last `nmean` blocks.
    offset: Vec<Vec<i32>>,
    coeffs: Vec<i32>,
}

impl Core {
    pub fn new(header: &Header) -> Self {
        let nwrap = header.nwrap();
        let mean_len = header.nmean.max(1) as usize;
        let mut core = Core {
            header: *header,
            nwrap,
            mean_len,
            blocksize: header.blocksize,
            bitshift: 0,
            decoded: vec![vec![0; nwrap + header.blocksize]; header.channels],
            offset: vec![vec![0; mean_len]; header.channels],
            coeffs: vec![0; nwrap],
        };
        core.reset();
        core
    }

    /// Returns to the state at the start of the stream.
    pub fn reset(&mut self) {
        self.blocksize = self.header.blocksize;
        self.bitshift = 0;
        let mean = self.header.file_type.initial_mean();
        for chan in &mut self.decoded {
            chan.fill(0);
        }
        for chan in &mut self.offset {
            chan.fill(mean);
        }
    }

    /// The block size of the most recently decoded group.
    pub fn blocksize(&self) -> usize {
        self.blocksize
    }

    /// The samples of `channel` in the most recently decoded group (after the bit shift).
    pub fn samples(&self, channel: usize) -> &[i32] {
        &self.decoded[channel][self.nwrap..self.nwrap + self.blocksize]
    }

    /// The length in bytes of the serialized state ([`Core::write_state`]).
    pub fn state_len(&self) -> usize {
        5 + 4 * self.header.channels * (self.nwrap + self.mean_len)
    }

    /// Serializes the state that carries over between groups: the block size, the bit shift, the
    /// sample history and the running means. Layout: `blocksize(u32)`, `bitshift(u8)`, then per
    /// channel `nwrap` history samples and `max(1, nmean)` means, all little-endian `i32`.
    pub fn write_state(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.blocksize as u32).to_le_bytes());
        out.push(self.bitshift as u8);
        for (decoded, offset) in self.decoded.iter().zip(&self.offset) {
            for v in decoded[..self.nwrap].iter().chain(offset) {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
    }

    /// Restores the state serialized by [`Core::write_state`] from the start of `data`.
    pub fn read_state(&mut self, data: &[u8]) -> Result<()> {
        let Some(data) = data.get(..self.state_len()) else {
            return decode_error("shorten: truncated decoder state");
        };
        let word = |chunk: &[u8]| i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);

        let blocksize = word(&data[..4]) as u32 as usize;
        let bitshift = u32::from(data[4]);
        if !(1..=self.header.blocksize).contains(&blocksize) || bitshift > 32 {
            return decode_error("shorten: invalid decoder state");
        }
        self.blocksize = blocksize;
        self.bitshift = bitshift;

        let mut words = data[5..].chunks_exact(4).map(word);
        for (decoded, offset) in self.decoded.iter_mut().zip(&mut self.offset) {
            for v in decoded[..self.nwrap].iter_mut().chain(offset.iter_mut()) {
                *v = words.next().unwrap_or(0);
            }
        }
        Ok(())
    }

    /// Decodes commands up to and including one audio command for each channel.
    ///
    /// With `reconstruct` unset the residuals are parsed but no samples are produced, which keeps
    /// the position tracking of the demuxer cheap; only the block size and bit shift are tracked
    /// then. The block size and bit shift are restored if the group fails to parse, so that a
    /// group cut short by the end of the data can be retried with more data.
    pub fn decode_group(&mut self, br: &mut BitReader<'_>, reconstruct: bool) -> Result<Group> {
        let (blocksize, bitshift) = (self.blocksize, self.bitshift);
        let result = self.decode_commands(br, reconstruct);
        if result.is_err() {
            self.blocksize = blocksize;
            self.bitshift = bitshift;
        }
        result
    }

    fn decode_commands(&mut self, br: &mut BitReader<'_>, reconstruct: bool) -> Result<Group> {
        let version = self.header.version;
        let mut chan = 0;

        while chan < self.header.channels {
            if br.bits_left() < 3 + FNSIZE as usize {
                return Err(eof());
            }

            match br.golomb_u(FNSIZE)? {
                FN_VERBATIM => {
                    let len = br.golomb_u(VERBATIM_CKSIZE_SIZE)? as i32;
                    if len < 0 || len as usize > br.bits_left() {
                        return decode_error("shorten: invalid verbatim chunk length");
                    }
                    for _ in 0..len {
                        br.golomb_u(VERBATIM_BYTE_SIZE)?;
                    }
                }
                FN_BITSHIFT => {
                    let bitshift = br.golomb_u(BITSHIFTSIZE)?;
                    if bitshift > 32 {
                        return decode_error("shorten: invalid bit shift");
                    }
                    self.bitshift = bitshift;
                }
                FN_BLOCKSIZE => {
                    let blocksize = read_uint(br, version, self.blocksize.ilog2())? as usize;
                    if blocksize > self.blocksize {
                        return unsupported_error("shorten: increasing the block size");
                    }
                    if blocksize == 0 || blocksize > MAX_BLOCKSIZE {
                        return decode_error("shorten: invalid block size");
                    }
                    self.blocksize = blocksize;
                }
                FN_QUIT => return Ok(Group::Quit),
                cmd @ (0..=FN_DIFF3 | FN_QLPC | FN_ZERO) => {
                    self.decode_channel(br, cmd, chan, reconstruct)?;
                    chan += 1;
                }
                _ => return decode_error("shorten: unknown function"),
            }
        }

        Ok(Group::Audio)
    }

    /// Decodes one audio command: the block of channel `chan`.
    fn decode_channel(
        &mut self,
        br: &mut BitReader<'_>,
        cmd: u32,
        chan: usize,
        reconstruct: bool,
    ) -> Result<()> {
        let version = self.header.version;

        // The Rice parameter of the residuals.
        let mut residual_size = 0i32;
        if cmd != FN_ZERO {
            residual_size = br.golomb_u(ENERGYSIZE)? as i32;
            // Version 0 differed in the definition of the signed Rice code.
            if version == 0 {
                residual_size = residual_size.wrapping_sub(1);
            }
            if residual_size as u32 > 30 {
                return decode_error("shorten: unsupported residual size");
            }
        }
        let residual_size = residual_size as u32;

        if !reconstruct {
            return self.skip_channel(br, cmd, residual_size);
        }

        // The mean of the previous blocks, which is subtracted from the signal.
        let nmean = self.header.nmean;
        let offset = &self.offset[chan];
        let coffset = if nmean == 0 {
            offset[0]
        } else {
            // Version 0 streams have `nmean == -1`: no means, so the offset is zero.
            let mut sum = if version < 2 { 0 } else { nmean / 2 };
            for &v in &offset[..nmean.max(0) as usize] {
                sum = sum.wrapping_add(v);
            }
            let mut mean = sum.wrapping_div(nmean);
            if version >= 2 && self.bitshift != 0 {
                mean = (mean >> (self.bitshift - 1)) >> 1;
            }
            mean
        };

        let (nwrap, blocksize) = (self.nwrap, self.blocksize);
        let decoded = &mut self.decoded[chan];

        if cmd == FN_ZERO {
            decoded[nwrap..nwrap + blocksize].fill(0);
        } else {
            // The predictor: the transmitted coefficients or a fixed polynomial.
            let (order, coeffs, qshift): (usize, &[i32], u32) = if cmd == FN_QLPC {
                let order = br.golomb_u(LPCQSIZE)? as usize;
                if order > nwrap {
                    return decode_error("shorten: invalid prediction order");
                }
                for c in &mut self.coeffs[..order] {
                    *c = br.golomb_s(LPCQUANT)?;
                }
                (order, &self.coeffs[..order], LPCQUANT)
            } else {
                (cmd as usize, &FIXED_COEFFS[cmd as usize][..cmd as usize], 0)
            };

            // Remove the offset from the history used for prediction.
            if cmd == FN_QLPC && coffset != 0 {
                for v in &mut decoded[nwrap - order..nwrap] {
                    *v = v.wrapping_sub(coffset);
                }
            }

            let init_sum = if order == 0 {
                coffset
            } else if cmd == FN_QLPC {
                self.header.lpcqoffset()
            } else {
                0
            };

            for i in 0..blocksize {
                let at = nwrap + i;
                let mut sum = init_sum;
                for (j, &c) in coeffs.iter().enumerate() {
                    sum = sum.wrapping_add(c.wrapping_mul(decoded[at - j - 1]));
                }
                decoded[at] = br.golomb_s(residual_size)?.wrapping_add(sum >> qshift);
            }

            // Add the offset back to the decoded samples.
            if cmd == FN_QLPC && coffset != 0 {
                for v in &mut decoded[nwrap..nwrap + blocksize] {
                    *v = v.wrapping_add(coffset);
                }
            }
        }

        // Update the means with this block.
        if nmean > 0 {
            let mut sum: i64 = if version < 2 { 0 } else { (blocksize / 2) as i64 };
            for &v in &decoded[nwrap..nwrap + blocksize] {
                sum += i64::from(v);
            }
            let mean = sum / blocksize as i64;

            let offset = &mut self.offset[chan];
            offset.copy_within(1..nmean as usize, 0);
            offset[nmean as usize - 1] = if version < 2 {
                mean as i32
            } else if self.bitshift == 32 {
                0
            } else {
                mean.wrapping_mul(1i64 << self.bitshift) as i32
            };
        }

        // Keep the last samples as history for the next block.
        decoded.copy_within(blocksize..blocksize + nwrap, 0);

        // Restore the zero bits the encoder removed.
        match self.bitshift {
            0 => (),
            32 => decoded[nwrap..nwrap + blocksize].fill(0),
            shift => {
                for v in &mut decoded[nwrap..nwrap + blocksize] {
                    *v = v.wrapping_mul(1i32.wrapping_shl(shift));
                }
            }
        }

        Ok(())
    }

    /// Parses the rest of an audio command without reconstructing the samples.
    fn skip_channel(&mut self, br: &mut BitReader<'_>, cmd: u32, residual_size: u32) -> Result<()> {
        if cmd == FN_ZERO {
            return Ok(());
        }
        if cmd == FN_QLPC {
            let order = br.golomb_u(LPCQSIZE)? as usize;
            if order > self.nwrap {
                return decode_error("shorten: invalid prediction order");
            }
            for _ in 0..order {
                br.golomb_s(LPCQUANT)?;
            }
        }
        for _ in 0..self.blocksize {
            br.golomb_s(residual_size)?;
        }
        Ok(())
    }
}
