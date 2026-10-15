// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bit-stream unpacking (RFC 3951 section 3.2, via FFmpeg `unpack_frame`).

/// The frame length of an iLBC stream.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// 20 ms frames: 160 samples in 38 bytes.
    Ms20,
    /// 30 ms frames: 240 samples in 50 bytes.
    Ms30,
}

impl Mode {
    /// Samples per frame.
    pub const fn samples(self) -> usize {
        match self {
            Mode::Ms20 => 160,
            Mode::Ms30 => 240,
        }
    }

    /// Bytes per frame.
    pub const fn bytes(self) -> usize {
        match self {
            Mode::Ms20 => 38,
            Mode::Ms30 => 50,
        }
    }

    /// Number of sub-blocks (`NSUB`).
    pub const fn nsub(self) -> usize {
        match self {
            Mode::Ms20 => 4,
            Mode::Ms30 => 6,
        }
    }

    /// Number of LSF sets (`LPC_N`).
    pub const fn lpc_n(self) -> usize {
        match self {
            Mode::Ms20 => 1,
            Mode::Ms30 => 2,
        }
    }

    /// Length of the scalar-quantised start state (`STATE_SHORT_LEN`).
    pub const fn state_short_len(self) -> usize {
        match self {
            Mode::Ms20 => 57,
            Mode::Ms30 => 58,
        }
    }
}

/// MSB-first bit reader. Reads past the end return zero bits, like FFmpeg's padded reader.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn get(&mut self, n: usize) -> usize {
        let mut v = 0;
        for _ in 0..n {
            let byte = self.data.get(self.pos >> 3).copied().unwrap_or(0);
            v = (v << 1) | usize::from((byte >> (7 - (self.pos & 7))) & 1);
            self.pos += 1;
        }
        v
    }
}

/// The unpacked parameters of a frame (`ILBCFrame`). All fields are zero-extended bit-fields.
pub struct Frame {
    pub lsf: [usize; 6],
    pub cb_index: [usize; 15],
    pub gain_index: [usize; 15],
    pub ifm: usize,
    pub state_first: bool,
    pub idx: [usize; 58],
    pub start: usize,
    /// The "empty frame" flag (the final bit of the frame); set when the encoder signalled a
    /// frame that must be concealed.
    pub flag: bool,
}

impl Frame {
    /// Unpacks `data`. `data` must hold at least `mode.bytes()` bytes.
    pub fn unpack(mode: Mode, data: &[u8]) -> Frame {
        let mut f = Frame {
            lsf: [0; 6],
            cb_index: [0; 15],
            gain_index: [0; 15],
            ifm: 0,
            state_first: false,
            idx: [0; 58],
            start: 0,
            flag: false,
        };
        let mut gb = Bits { data, pos: 0 };

        f.lsf[0] = gb.get(6);
        f.lsf[1] = gb.get(7);
        f.lsf[2] = gb.get(7);

        if mode == Mode::Ms20 {
            f.start = gb.get(2);
            f.state_first = gb.get(1) != 0;
            f.ifm = gb.get(6);
            f.cb_index[0] = gb.get(6) << 1;
            f.gain_index[0] = gb.get(2) << 3;
            f.gain_index[1] = gb.get(1) << 3;
            f.cb_index[3] = gb.get(7) << 1;
            f.gain_index[3] = gb.get(1) << 4;
            f.gain_index[4] = gb.get(1) << 3;
            f.gain_index[6] = gb.get(1) << 4;
        }
        else {
            f.lsf[3] = gb.get(6);
            f.lsf[4] = gb.get(7);
            f.lsf[5] = gb.get(7);
            f.start = gb.get(3);
            f.state_first = gb.get(1) != 0;
            f.ifm = gb.get(6);
            f.cb_index[0] = gb.get(4) << 3;
            f.gain_index[0] = gb.get(1) << 4;
            f.gain_index[1] = gb.get(1) << 3;
            f.cb_index[3] = gb.get(6) << 2;
            f.gain_index[3] = gb.get(1) << 4;
            f.gain_index[4] = gb.get(1) << 3;
        }

        for j in 0..48 {
            f.idx[j] = gb.get(1) << 2;
        }

        if mode == Mode::Ms20 {
            for j in 48..57 {
                f.idx[j] = gb.get(1) << 2;
            }

            f.gain_index[1] |= gb.get(1) << 2;
            f.gain_index[3] |= gb.get(2) << 2;
            f.gain_index[4] |= gb.get(1) << 2;
            f.gain_index[6] |= gb.get(1) << 3;
            f.gain_index[7] = gb.get(2) << 2;
        }
        else {
            for j in 48..58 {
                f.idx[j] = gb.get(1) << 2;
            }

            f.cb_index[0] |= gb.get(2) << 1;
            f.gain_index[0] |= gb.get(1) << 3;
            f.gain_index[1] |= gb.get(1) << 2;
            f.cb_index[3] |= gb.get(1) << 1;
            f.cb_index[6] = gb.get(1) << 7;
            f.cb_index[6] |= gb.get(6) << 1;
            f.cb_index[9] = gb.get(7) << 1;
            f.cb_index[12] = gb.get(3) << 5;
            f.cb_index[12] |= gb.get(4) << 1;
            f.gain_index[3] |= gb.get(2) << 2;
            f.gain_index[4] |= gb.get(2) << 1;
            f.gain_index[6] = gb.get(2) << 3;
            f.gain_index[7] = gb.get(2) << 2;
            f.gain_index[9] = gb.get(1) << 4;
            f.gain_index[10] = gb.get(1) << 3;
            f.gain_index[12] = gb.get(1) << 4;
            f.gain_index[13] = gb.get(1) << 3;
        }

        for j in 0..56 {
            f.idx[j] |= gb.get(2);
        }

        if mode == Mode::Ms20 {
            f.idx[56] |= gb.get(2);
            f.cb_index[0] |= gb.get(1);
            f.cb_index[1] = gb.get(7);
            f.cb_index[2] = gb.get(6) << 1;
            f.cb_index[2] |= gb.get(1);
            f.gain_index[0] |= gb.get(3);
            f.gain_index[1] |= gb.get(2);
            f.gain_index[2] = gb.get(3);
            f.cb_index[3] |= gb.get(1);
            f.cb_index[4] = gb.get(6) << 1;
            f.cb_index[4] |= gb.get(1);
            f.cb_index[5] = gb.get(7);
            f.cb_index[6] = gb.get(8);
            f.cb_index[7] = gb.get(8);
            f.cb_index[8] = gb.get(8);
            f.gain_index[3] |= gb.get(2);
            f.gain_index[4] |= gb.get(2);
            f.gain_index[5] = gb.get(3);
            f.gain_index[6] |= gb.get(3);
            f.gain_index[7] |= gb.get(2);
            f.gain_index[8] = gb.get(3);
        }
        else {
            f.idx[56] |= gb.get(2);
            f.idx[57] |= gb.get(2);
            f.cb_index[0] |= gb.get(1);
            f.cb_index[1] = gb.get(7);
            f.cb_index[2] = gb.get(4) << 3;
            f.cb_index[2] |= gb.get(3);
            f.gain_index[0] |= gb.get(3);
            f.gain_index[1] |= gb.get(2);
            f.gain_index[2] = gb.get(3);
            f.cb_index[3] |= gb.get(1);
            f.cb_index[4] = gb.get(4) << 3;
            f.cb_index[4] |= gb.get(3);
            f.cb_index[5] = gb.get(7);
            f.cb_index[6] |= gb.get(1);
            f.cb_index[7] = gb.get(5) << 3;
            f.cb_index[7] |= gb.get(3);
            f.cb_index[8] = gb.get(8);
            f.cb_index[9] |= gb.get(1);
            f.cb_index[10] = gb.get(4) << 4;
            f.cb_index[10] |= gb.get(4);
            f.cb_index[11] = gb.get(8);
            f.cb_index[12] |= gb.get(1);
            f.cb_index[13] = gb.get(3) << 5;
            f.cb_index[13] |= gb.get(5);
            f.cb_index[14] = gb.get(8);
            f.gain_index[3] |= gb.get(2);
            f.gain_index[4] |= gb.get(1);
            f.gain_index[5] = gb.get(3);
            f.gain_index[6] |= gb.get(3);
            f.gain_index[7] |= gb.get(2);
            f.gain_index[8] = gb.get(3);
            f.gain_index[9] |= gb.get(4);
            f.gain_index[10] |= gb.get(1) << 2;
            f.gain_index[10] |= gb.get(2);
            f.gain_index[11] = gb.get(3);
            f.gain_index[12] |= gb.get(4);
            f.gain_index[13] |= gb.get(3);
            f.gain_index[14] = gb.get(3);
        }

        f.flag = gb.get(1) != 0;
        f
    }
}
