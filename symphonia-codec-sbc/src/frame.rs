// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SBC frame headers, bit allocation and sub-band sample unpacking.
//!
//! Follows A2DP v1.3 section 12.9 (frame header, CRC-8 and bit allocation), as implemented by
//! FFmpeg's `libavcodec/sbcdec.c` (`sbc_unpack_frame`) and `libavcodec/sbc.c`
//! (`ff_sbc_calculate_bits`, `ff_sbc_crc8`), and the frame length formula of
//! `libavcodec/sbc_parser.c`.

/// The sync word of an SBC frame.
pub const SBC_SYNCWORD: u8 = 0x9c;
/// The sync word of an mSBC (Bluetooth HFP wide-band speech) frame.
pub const MSBC_SYNCWORD: u8 = 0xad;
/// The number of blocks in an mSBC frame.
const MSBC_BLOCKS: usize = 15;
/// The length of an mSBC frame.
pub const MSBC_FRAME_LEN: usize = 57;
/// The number of extra bits that the decoder keeps in the sub-band samples (`SBCDEC_FIXED_EXTRA_BITS`).
const FIXED_EXTRA_BITS: u32 = 2;

/// The sampling frequencies by their header code.
const SAMPLE_RATES: [u32; 4] = [16000, 32000, 44100, 48000];

/// The channel mode.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Mono,
    DualChannel,
    Stereo,
    JointStereo,
}

/// The loudness offsets of the bit allocation of 4 sub-bands, by sampling frequency code
/// (`sbc_offset4`).
const OFFSET4: [[i32; 4]; 4] = [[-1, 0, 0, 0], [-2, 0, 0, 1], [-2, 0, 0, 1], [-2, 0, 0, 1]];

/// The loudness offsets of the bit allocation of 8 sub-bands (`sbc_offset8`).
const OFFSET8: [[i32; 8]; 4] = [
    [-2, 0, 0, 0, 0, 0, 0, 1],
    [-3, 0, 0, 0, 0, 0, 1, 2],
    [-4, 0, 0, 0, 0, 0, 1, 2],
    [-4, 0, 0, 0, 0, 0, 1, 2],
];

/// The header of an SBC frame.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    /// The sampling frequency code.
    frequency: u8,
    /// The number of blocks, 4, 8, 12, 16 (or 15 for mSBC).
    pub blocks: usize,
    pub mode: Mode,
    /// True if the bit allocation is SNR, and false if it is loudness.
    snr: bool,
    /// The number of sub-bands, 4 or 8.
    pub subbands: usize,
    pub bitpool: u8,
    /// True for an mSBC frame.
    pub msbc: bool,
}

impl FrameHeader {
    /// Parse the header at the start of `data`. Returns `None` if `data` is too short, or does
    /// not start with the sync word and a valid header. The CRC is not checked.
    pub fn parse(data: &[u8]) -> Option<FrameHeader> {
        let [sync, b1, b2, ..] = *data
        else {
            return None;
        };

        if sync == MSBC_SYNCWORD {
            // Fixed parameters: 16 kHz, mono, loudness, 8 sub-bands, bitpool 26.
            if b1 != 0 || b2 != 0 {
                return None;
            }

            return Some(FrameHeader {
                frequency: 0,
                blocks: MSBC_BLOCKS,
                mode: Mode::Mono,
                snr: false,
                subbands: 8,
                bitpool: 26,
                msbc: true,
            });
        }

        if sync != SBC_SYNCWORD {
            return None;
        }

        let mode = match (b1 >> 2) & 0x03 {
            0 => Mode::Mono,
            1 => Mode::DualChannel,
            2 => Mode::Stereo,
            _ => Mode::JointStereo,
        };

        let header = FrameHeader {
            frequency: (b1 >> 6) & 0x03,
            blocks: 4 * usize::from((b1 >> 4) & 0x03) + 4,
            mode,
            snr: (b1 >> 1) & 0x01 != 0,
            subbands: if b1 & 0x01 != 0 { 8 } else { 4 },
            bitpool: b2,
            msbc: false,
        };

        // The bitpool limit of the specification: 16 bits per sub-band and channel.
        let max_bitpool = match mode {
            Mode::Mono | Mode::DualChannel => 16 * header.subbands,
            Mode::Stereo | Mode::JointStereo => 32 * header.subbands,
        };

        if usize::from(b2) > max_bitpool {
            return None;
        }

        Some(header)
    }

    /// The number of channels.
    pub fn channels(&self) -> usize {
        if self.mode == Mode::Mono { 1 } else { 2 }
    }

    /// The sampling frequency in Hz.
    pub fn sample_rate(&self) -> u32 {
        SAMPLE_RATES[usize::from(self.frequency)]
    }

    /// The number of samples per channel of the frame.
    pub fn samples(&self) -> usize {
        self.blocks * self.subbands
    }

    /// The length of the frame in bytes (A2DP 12.9, and `sbc_parse_header`).
    pub fn frame_len(&self) -> usize {
        let channels = self.channels();
        let joint = usize::from(self.mode == Mode::JointStereo);
        let dual = usize::from(self.mode == Mode::DualChannel);

        4 + (self.subbands * channels) / 2
            + ((dual + 1) * self.blocks * usize::from(self.bitpool) + joint * self.subbands)
                .div_ceil(8)
    }
}

/// The error of an SBC frame that cannot be unpacked.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UnpackError {
    /// The data is shorter than the frame.
    TooShort,
    /// The sync word or the mSBC header is wrong.
    Sync,
    /// The CRC-8 of the header does not match.
    Crc,
    /// The bitpool is out of bounds.
    Bitpool,
}

/// The unpacked sub-band samples of a frame.
pub struct Unpacked {
    pub header: FrameHeader,
    /// The sub-band samples by block, channel and sub-band.
    pub samples: [[[i32; 8]; 2]; 16],
}

impl Unpacked {
    pub fn new() -> Self {
        Unpacked {
            header: FrameHeader {
                frequency: 0,
                blocks: 0,
                mode: Mode::Mono,
                snr: false,
                subbands: 4,
                bitpool: 0,
                msbc: false,
            },
            samples: [[[0; 8]; 2]; 16],
        }
    }
}

/// CRC-8 with the polynomial `x^8 + x^4 + x^3 + x^2 + 1` and the initial value `0x0f`, over the
/// first `len` bits of `data` (`ff_sbc_crc8`).
fn crc8(data: &[u8], len: usize) -> u8 {
    let mut crc = 0x0fu8;

    for i in 0..len {
        let bit = (data[i >> 3] >> (7 - (i & 7))) & 1;
        let mask = (crc >> 7) ^ bit;
        crc = (crc << 1) ^ if mask != 0 { 0x1d } else { 0 };
    }

    crc
}

/// Computes the number of bits of each sub-band sample (`ff_sbc_calculate_bits`).
fn calculate_bits(h: &FrameHeader, scale_factor: &[[u8; 8]; 2], bits: &mut [[i32; 8]; 2]) {
    let subbands = h.subbands;
    let sf = usize::from(h.frequency);
    let bitpool = i32::from(h.bitpool);
    let channels = h.channels();

    let mut bitneed = [[0i32; 8]; 2];

    // The bit need of every sub-band of a channel.
    let fill = |bitneed: &mut [i32; 8], scale: &[u8; 8]| {
        let mut max_bitneed = 0;

        for sb in 0..subbands {
            let s = i32::from(scale[sb]);

            bitneed[sb] = if h.snr {
                s
            }
            else if s == 0 {
                -5
            }
            else {
                let offset = if subbands == 4 { OFFSET4[sf][sb] } else { OFFSET8[sf][sb] };
                let loudness = s - offset;
                if loudness > 0 { loudness / 2 } else { loudness }
            };

            max_bitneed = max_bitneed.max(bitneed[sb]);
        }

        max_bitneed
    };

    // Finds the bit slice, and the number of bits it allocates, over the channels `chs`.
    let slice = |bitneed: &[[i32; 8]; 2], chs: std::ops::Range<usize>, max_bitneed: i32| {
        let mut bitcount = 0;
        let mut slicecount = 0;
        let mut bitslice = max_bitneed + 1;

        // The count reaches the bitpool at the latest when every sub-band is fully allocated. The
        // lower bound only guards against a malformed bitpool.
        loop {
            bitslice -= 1;
            bitcount += slicecount;
            slicecount = 0;

            for ch in chs.clone() {
                for sb in 0..subbands {
                    let need = bitneed[ch][sb];

                    if need > bitslice + 1 && need < bitslice + 16 {
                        slicecount += 1;
                    }
                    else if need == bitslice + 1 {
                        slicecount += 2;
                    }
                }
            }

            if bitcount + slicecount >= bitpool || bitslice < -64 {
                break;
            }
        }

        if bitcount + slicecount == bitpool {
            bitcount += slicecount;
            bitslice -= 1;
        }

        (bitcount, bitslice)
    };

    let init_bits = |bitneed: &[i32; 8], bits: &mut [i32; 8], bitslice: i32| {
        for sb in 0..subbands {
            bits[sb] =
                if bitneed[sb] < bitslice + 2 { 0 } else { (bitneed[sb] - bitslice).min(16) };
        }
    };

    if matches!(h.mode, Mode::Mono | Mode::DualChannel) {
        for ch in 0..channels {
            let max_bitneed = fill(&mut bitneed[ch], &scale_factor[ch]);
            let (mut bitcount, bitslice) = slice(&bitneed, ch..ch + 1, max_bitneed);

            init_bits(&bitneed[ch], &mut bits[ch], bitslice);

            for sb in 0..subbands {
                if bitcount >= bitpool {
                    break;
                }

                if bits[ch][sb] >= 2 && bits[ch][sb] < 16 {
                    bits[ch][sb] += 1;
                    bitcount += 1;
                }
                else if bitneed[ch][sb] == bitslice + 1 && bitpool > bitcount + 1 {
                    bits[ch][sb] = 2;
                    bitcount += 2;
                }
            }

            for sb in 0..subbands {
                if bitcount >= bitpool {
                    break;
                }

                if bits[ch][sb] < 16 {
                    bits[ch][sb] += 1;
                    bitcount += 1;
                }
            }
        }
    }
    else {
        let mut max_bitneed = 0;
        for ch in 0..2 {
            max_bitneed = max_bitneed.max(fill(&mut bitneed[ch], &scale_factor[ch]));
        }

        let (mut bitcount, bitslice) = slice(&bitneed, 0..2, max_bitneed);

        for ch in 0..2 {
            init_bits(&bitneed[ch], &mut bits[ch], bitslice);
        }

        // Distribute the remaining bits, walking the sub-bands, and the channels within.
        let mut ch = 0;
        let mut sb = 0;
        while bitcount < bitpool {
            if bits[ch][sb] >= 2 && bits[ch][sb] < 16 {
                bits[ch][sb] += 1;
                bitcount += 1;
            }
            else if bitneed[ch][sb] == bitslice + 1 && bitpool > bitcount + 1 {
                bits[ch][sb] = 2;
                bitcount += 2;
            }

            if ch == 1 {
                ch = 0;
                sb += 1;
                if sb >= subbands {
                    break;
                }
            }
            else {
                ch = 1;
            }
        }

        ch = 0;
        sb = 0;
        while bitcount < bitpool {
            if bits[ch][sb] < 16 {
                bits[ch][sb] += 1;
                bitcount += 1;
            }

            if ch == 1 {
                ch = 0;
                sb += 1;
                if sb >= subbands {
                    break;
                }
            }
            else {
                ch = 1;
            }
        }
    }
}

/// Unpack the frame at the start of `data`, and dequantize the sub-band samples
/// (`sbc_unpack_frame`). Returns the length of the packed frame, as the bits it used rounded up
/// to bytes.
pub fn unpack_frame(data: &[u8], out: &mut Unpacked) -> Result<usize, UnpackError> {
    if data.len() < 4 {
        return Err(UnpackError::TooShort);
    }

    let h = FrameHeader::parse(data).ok_or_else(|| {
        // Tell a bad bitpool from a bad sync word.
        if data[0] == SBC_SYNCWORD { UnpackError::Bitpool } else { UnpackError::Sync }
    })?;
    out.header = h;

    let len_bits = data.len() * 8;
    let channels = h.channels();
    let subbands = h.subbands;

    // The parts of the header covered by the CRC: the two bytes after the sync word, the joint
    // stereo flags, and the scale factors.
    let mut crc_header = [0u8; 11];
    crc_header[0] = data[1];
    crc_header[1] = data[2];
    let mut crc_pos = 16;
    let mut consumed = 32;

    let mut joint = 0u8;
    if h.mode == Mode::JointStereo {
        if len_bits < consumed + subbands {
            return Err(UnpackError::TooShort);
        }

        // The flag of the last sub-band is not transmitted.
        for sb in 0..subbands - 1 {
            joint |= ((data[4] >> (7 - sb)) & 1) << sb;
        }

        crc_header[crc_pos / 8] = if subbands == 4 { data[4] & 0xf0 } else { data[4] };
        consumed += subbands;
        crc_pos += subbands;
    }

    if len_bits < consumed + 4 * subbands * channels {
        return Err(UnpackError::TooShort);
    }

    let mut scale_factor = [[0u8; 8]; 2];
    for ch in 0..channels {
        for sb in 0..subbands {
            let sf = (data[consumed >> 3] >> (4 - (consumed & 7))) & 0x0f;
            scale_factor[ch][sb] = sf;
            crc_header[crc_pos >> 3] |= sf << (4 - (crc_pos & 7));

            consumed += 4;
            crc_pos += 4;
        }
    }

    if data[3] != crc8(&crc_header, crc_pos) {
        return Err(UnpackError::Crc);
    }

    let mut bits = [[0i32; 8]; 2];
    calculate_bits(&h, &scale_factor, &mut bits);

    let mut levels = [[0u32; 8]; 2];
    for ch in 0..channels {
        for sb in 0..subbands {
            levels[ch][sb] = (1u32 << bits[ch][sb]) - 1;
        }
    }

    for blk in 0..h.blocks {
        for ch in 0..channels {
            for sb in 0..subbands {
                if levels[ch][sb] == 0 {
                    out.samples[blk][ch][sb] = 0;
                    continue;
                }

                let shift = u32::from(scale_factor[ch][sb]) + 1 + FIXED_EXTRA_BITS;

                let mut audio_sample = 0u64;
                for bit in 0..bits[ch][sb] {
                    if consumed >= len_bits {
                        return Err(UnpackError::TooShort);
                    }

                    if (data[consumed >> 3] >> (7 - (consumed & 7))) & 1 != 0 {
                        audio_sample |= 1 << (bits[ch][sb] - bit - 1);
                    }

                    consumed += 1;
                }

                let scaled = (((audio_sample << 1) | 1) << shift) / u64::from(levels[ch][sb]);
                out.samples[blk][ch][sb] = (scaled as i32).wrapping_sub(1 << shift);
            }
        }
    }

    if h.mode == Mode::JointStereo {
        for blk in 0..h.blocks {
            for sb in 0..subbands {
                if joint & (1 << sb) != 0 {
                    let [l, r, ..] = &mut out.samples[blk][..]
                    else {
                        continue;
                    };
                    let temp = l[sb].wrapping_add(r[sb]);
                    r[sb] = l[sb].wrapping_sub(r[sb]);
                    l[sb] = temp;
                }
            }
        }
    }

    Ok(consumed.div_ceil(8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msbc_header() {
        let h = FrameHeader::parse(&[0xad, 0, 0, 0]).unwrap();
        assert_eq!(h.frame_len(), MSBC_FRAME_LEN);
        assert_eq!(h.samples(), 120);
        assert_eq!(h.sample_rate(), 16000);
    }

    #[test]
    fn sbc_frame_len() {
        // 44.1 kHz, 16 blocks, joint stereo, loudness, 8 sub-bands, bitpool 53 (A2DP "high
        // quality"): 119 bytes.
        let h = FrameHeader::parse(&[0x9c, 0xbd, 53, 0]).unwrap();
        assert_eq!(h.frame_len(), 119);
        assert_eq!(h.samples(), 128);
        assert_eq!(h.sample_rate(), 44100);
    }

    #[test]
    fn rejects_bad_bitpool() {
        // Mono, 4 sub-bands: at most 64.
        assert!(FrameHeader::parse(&[0x9c, 0x00, 65, 0]).is_none());
        assert!(FrameHeader::parse(&[0x9c, 0x00, 64, 0]).is_some());
    }
}
