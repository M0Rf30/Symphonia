// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The MD5 checksum of a WavPack file is that of the audio as the *source file* stored it, not
//! of the decoded samples: the encoder records how the source differed from little-endian signed
//! (unsigned 8-bit) WAV audio in the "qmode" flags of the configuration, and the decoder
//! (`wvunpack`) undoes the differences, in `unpack_audio()` and `unpack_dsd_audio()` of
//! cli/wvunpack.c, before hashing. This module does that.

use symphonia_core::checksum::Md5;
use symphonia_core::io::Monitor;

/// Source audio is big-endian (`QMODE_BIG_ENDIAN`).
const QMODE_BIG_ENDIAN: u8 = 0x1;
/// Source 8-bit audio is signed (`QMODE_SIGNED_BYTES`).
const QMODE_SIGNED_BYTES: u8 = 0x2;
/// Source audio other than 8-bit is unsigned (`QMODE_UNSIGNED_WORDS`).
const QMODE_UNSIGNED_WORDS: u8 = 0x4;
/// Source channels were not in Microsoft order (`QMODE_REORDERED_CHANS`).
const QMODE_REORDERED_CHANS: u8 = 0x8;
/// Source DSD bytes are least significant bit first (`QMODE_DSD_LSB_FIRST`).
const QMODE_DSD_LSB_FIRST: u8 = 0x10;
/// Source DSD is in blocks per channel (`QMODE_DSD_IN_BLOCKS`).
const QMODE_DSD_IN_BLOCKS: u8 = 0x40;

/// The number of DSD bytes per channel in a block of a source that is blocked (`DSD_BLOCKSIZE`).
const DSD_BLOCKSIZE: usize = 4096;

/// The magic number at the start of the extra data of the codec parameters that carry the
/// source format of the audio for the MD5 checksum.
const EXTRA_MAGIC: &[u8; 4] = b"WVQM";

/// The extra data the reader gives the decoder: the magic number, the qmode, the length of the
/// channel reordering (the channel count of the layout) and the reordering.
pub(crate) fn encode_extra(qmode: u8, reorder: &[u8]) -> Box<[u8]> {
    let mut extra = EXTRA_MAGIC.to_vec();
    extra.push(qmode);
    extra.push(reorder.len().min(255) as u8);
    extra.extend_from_slice(&reorder[..reorder.len().min(255)]);
    extra.into_boxed_slice()
}

/// Hashes the audio of a stream the way the MD5 checksum of the file was made.
pub(super) struct Md5Source {
    md5: Md5,
    qmode: u8,
    /// `reorder[channel]` is the channel of the file that is the channel of the source.
    reorder: Vec<usize>,
    /// The interleaved samples of the packet, to be hashed by `update`.
    pub samples: Vec<i32>,
    bytes: Vec<u8>,
    /// A frame of samples, while reordering the channels.
    frame: Vec<i32>,
    /// The DSD bytes not yet hashed, of a source that is blocked.
    dsd_pending: Vec<u8>,
}

impl Md5Source {
    /// `extra` is the extra data of the codec parameters (see `encode_extra`).
    pub fn new(extra: Option<&[u8]>, num_channels: usize) -> Self {
        let mut qmode = 0;
        let mut reorder = Vec::new();

        if let Some([m0, m1, m2, m3, q, n, order @ ..]) = extra {
            if [*m0, *m1, *m2, *m3] == *EXTRA_MAGIC {
                qmode = *q;

                // `wvunpack` reorders if the layout has no more channels than the file.
                let n = usize::from(*n);
                if qmode & QMODE_REORDERED_CHANS != 0 && n <= num_channels && order.len() >= n {
                    reorder = (0..num_channels).collect();
                    for (dst, &src) in reorder.iter_mut().zip(&order[..n]) {
                        *dst = usize::from(src);
                    }
                    if reorder.iter().any(|&src| src >= num_channels) {
                        reorder.clear();
                    }
                }
            }
        }

        Md5Source {
            md5: Md5::default(),
            qmode,
            reorder,
            samples: Vec::new(),
            bytes: Vec::new(),
            frame: Vec::new(),
            dsd_pending: Vec::new(),
        }
    }

    /// Hash the `frames` frames of `num_channels` interleaved samples in `samples`, of a stream
    /// with samples of `width` bytes (or of DSD bytes).
    pub fn update(&mut self, num_channels: usize, width: usize, is_dsd: bool) {
        if num_channels == 0 {
            self.samples.clear();
            return;
        }

        if !self.reorder.is_empty() {
            self.frame.resize(num_channels, 0);
            for samples in self.samples.chunks_exact_mut(num_channels) {
                for (dst, &src) in self.frame.iter_mut().zip(&self.reorder) {
                    *dst = samples[src];
                }
                samples.copy_from_slice(&self.frame);
            }
        }

        self.bytes.clear();

        if is_dsd {
            // Bytes, in blocks of `DSD_BLOCKSIZE` per channel if the source is blocked.
            if self.qmode & QMODE_DSD_IN_BLOCKS == 0 {
                self.bytes.extend(self.samples.iter().map(|&b| b as u8));
            }
            else {
                self.dsd_pending.extend(self.samples.iter().map(|&b| b as u8));

                let block_len = DSD_BLOCKSIZE * num_channels;
                let whole = self.dsd_pending.len() / block_len * block_len;
                for block in self.dsd_pending[..whole].chunks_exact(block_len) {
                    push_dsd_block(&mut self.bytes, block, num_channels, self.qmode);
                }
                self.dsd_pending.drain(..whole);
            }
        }
        else {
            // `store_samples()` in wvunpack.c.
            let unsigned = self.qmode & QMODE_UNSIGNED_WORDS != 0
                || (width == 1 && self.qmode & QMODE_SIGNED_BYTES == 0);
            let big_endian = self.qmode & QMODE_BIG_ENDIAN != 0;
            let width = width.clamp(1, 4);
            let offset = 1u32 << (8 * width - 1);

            for &sample in &self.samples {
                let v = if unsigned { (sample as u32).wrapping_add(offset) } else { sample as u32 };
                let le = v.to_le_bytes();
                if big_endian {
                    self.bytes.extend(le[..width].iter().rev());
                }
                else {
                    self.bytes.extend_from_slice(&le[..width]);
                }
            }
        }

        self.md5.process_buf_bytes(&self.bytes);
        self.samples.clear();
    }

    /// Finish hashing, and return the checksum.
    pub fn finalize(&mut self, num_channels: usize) -> [u8; 16] {
        // A last, partial, block of a source that is blocked is padded with zeros.
        if !self.dsd_pending.is_empty() && num_channels > 0 {
            let block_len = DSD_BLOCKSIZE * num_channels;
            self.dsd_pending.resize(block_len, 0);

            self.bytes.clear();
            push_dsd_block(&mut self.bytes, &self.dsd_pending, num_channels, self.qmode);
            self.dsd_pending.clear();
            self.md5.process_buf_bytes(&self.bytes);
        }

        self.md5.md5()
    }
}

/// Append a block of `DSD_BLOCKSIZE` interleaved DSD bytes per channel as the bytes by channel.
fn push_dsd_block(out: &mut Vec<u8>, block: &[u8], num_channels: usize, qmode: u8) {
    for channel in 0..num_channels {
        out.extend(
            block
                .iter()
                .skip(channel)
                .step_by(num_channels)
                .map(|&b| if qmode & QMODE_DSD_LSB_FIRST != 0 { b.reverse_bits() } else { b }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md5_of(bytes: &[u8]) -> [u8; 16] {
        let mut md5 = Md5::default();
        md5.process_buf_bytes(bytes);
        md5.md5()
    }

    #[test]
    fn verify_default_is_little_endian_wav() {
        let mut source = Md5Source::new(None, 2);
        source.samples.extend([1, -2, 0x1234, -0x1234]);
        source.update(2, 2, false);
        assert_eq!(source.finalize(2), md5_of(&[1, 0, 0xfe, 0xff, 0x34, 0x12, 0xcc, 0xed]));

        // 8-bit audio is unsigned.
        let mut source = Md5Source::new(None, 1);
        source.samples.extend([-128, 0, 127]);
        source.update(1, 1, false);
        assert_eq!(source.finalize(1), md5_of(&[0, 128, 255]));
    }

    #[test]
    fn verify_source_formats() {
        // Big-endian signed 16-bit (AIFF) and unsigned 16-bit words.
        let extra = encode_extra(QMODE_BIG_ENDIAN, &[]);
        let mut source = Md5Source::new(Some(&extra), 1);
        source.samples.extend([0x0102, -1]);
        source.update(1, 2, false);
        assert_eq!(source.finalize(1), md5_of(&[1, 2, 0xff, 0xff]));

        let extra = encode_extra(QMODE_UNSIGNED_WORDS, &[]);
        let mut source = Md5Source::new(Some(&extra), 1);
        source.samples.extend([0, -1]);
        source.update(1, 2, false);
        assert_eq!(source.finalize(1), md5_of(&[0, 0x80, 0xff, 0x7f]));

        // The channels of the source are not in the order of the file.
        let extra = encode_extra(QMODE_REORDERED_CHANS, &[1, 0]);
        let mut source = Md5Source::new(Some(&extra), 2);
        source.samples.extend([1, 2, 3, 4]);
        source.update(2, 1, true);
        assert_eq!(source.finalize(2), md5_of(&[2, 1, 4, 3]));
    }

    #[test]
    fn verify_blocked_dsd() {
        // Blocked and least significant bit first: the bytes by channel, and the last block
        // padded.
        let extra = encode_extra(QMODE_DSD_IN_BLOCKS | QMODE_DSD_LSB_FIRST, &[]);
        let mut source = Md5Source::new(Some(&extra), 2);
        source.samples.extend([1, 2, 3, 4, 5, 6]);
        source.update(2, 1, true);
        // Nothing is hashed before the block is complete.
        let mut expected = vec![0u8; 2 * DSD_BLOCKSIZE];
        for (i, v) in [1u8, 3, 5].into_iter().enumerate() {
            expected[i] = v.reverse_bits();
        }
        for (i, v) in [2u8, 4, 6].into_iter().enumerate() {
            expected[DSD_BLOCKSIZE + i] = v.reverse_bits();
        }
        assert_eq!(source.finalize(2), md5_of(&expected));
    }
}
