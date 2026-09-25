// Symphonia Musepack demuxer (SV7)
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SV7 frame demuxer.
//!
//! Header parsing is ported from libmpcdec `streaminfo.c` (`streaminfo_read_header_sv7`)
//! (BSD-3-Clause), see `NOTICE`.
//!
//! # Packet representation (SV7 is not byte-aligned)
//!
//! Unlike SV8, an SV7 stream's frames are *not* byte-aligned: each frame is prefixed by a 20-bit
//! bit-length field (read directly by `mpc_demux_decode_inner`'s SV7 branch in the reference),
//! and the next frame's 20-bit length field follows immediately after the current frame's last
//! bit -- there is no padding to a byte boundary between frames. libmpcdec's own demuxer/decoder
//! split doesn't need to solve this because they share one continuous `mpc_bits_reader` across
//! calls; Symphonia's packet-oriented model requires each `Packet` to be an independent byte
//! buffer, so this demuxer performs the bit-alignment libmpcdec avoids:
//!
//! For each frame, the demuxer reads the 20-bit length prefix `L`, then copies exactly `L` bits
//! starting at the *current* (arbitrary) bit position into a fresh buffer, left-shifting so bit 0
//! of the frame lands at bit 0 of byte 0 of `Packet::data` (zero-padding the final byte). The
//! decoder (`decoder_core::Decoder::decode_frame`) then reads that buffer with a fresh
//! `BitReader` starting at bit 0; because SV7 frame content is fully determined by Huffman-coded
//! residuals (never a fixed bit count), it naturally stops consuming bits once the frame's data
//! is exhausted, so the zero-padding after `L` bits is never read. The 20-bit length itself is
//! *not* passed to the decoder -- it is only used by this demuxer to find frame boundaries (and,
//! for seeking, to skip frames without decoding them).
//!
//! Additionally, SV7 stores its header and audio data as a stream of 4-byte little-endian words
//! that must be byte-swapped into big-endian order before bit-reading (`MPC_BUFFER_SWAP` in the
//! reference). This demuxer buffers the whole audio region (after the fixed-size stream header)
//! in memory once, byte-swapped, and serves frames from it; see the crate-level docs for the
//! resulting memory-use caveat for very large SV7 files.

use symphonia_core::errors::{decode_error, seek_error, Error, Result, SeekErrorKind};
use symphonia_core::io::{MediaSource, MediaSourceStream, ReadBytes};

use crate::bits::BitReader;
use crate::decoder_core::{FRAME_LENGTH, SYNTH_DELAY};

use super::StreamInfo;

const SAMPLE_FREQS: [u32; 4] = [44100, 48000, 37800, 32000];
/// Maximum amount of audio-region data this demuxer will buffer for one SV7 stream.
const MAX_BUFFERED_BYTES: u64 = 512 * 1024 * 1024;

fn byte_swap_words(buf: &mut [u8]) {
    for chunk in buf.chunks_exact_mut(4) {
        chunk.swap(0, 3);
        chunk.swap(1, 2);
    }
}

pub(crate) struct Sv7State {
    /// Byte-swapped audio-region bytes (header excluded; starts at the first frame's first
    /// bit's byte).
    data: Vec<u8>,
    /// Current read position, in bits, into `data`.
    bit_pos: u64,
    /// Total number of frames in the stream (from the header's `frames` field); `next_packet`
    /// stops once this many have been emitted, regardless of any trailing bytes in `data` (e.g.
    /// an appended APEv2 tag).
    frames_total: u64,
    frames_emitted: u64,
}

/// Ported from libmpcdec `streaminfo.c` (`streaminfo_read_header_sv7`).
pub(crate) fn read_header(
    mss: &mut MediaSourceStream<'_>,
    version_byte: u8,
) -> Result<(StreamInfo, Sv7State)> {
    let stream_version = u32::from(version_byte & 0x0F);
    if stream_version != 7 {
        return decode_error("musepack: unsupported SV7 stream_version");
    }

    let mut header = [0u8; 24];
    mss.read_buf_exact(&mut header).map_err(Error::IoError)?;
    byte_swap_words(&mut header);

    let mut r = BitReader::new(&header);
    let frames = u64::from(r.read_bits(16)) << 16 | u64::from(r.read_bits(16));
    let _intensity_stereo = r.read_bit();
    let ms = r.read_bit() != 0;
    let max_band = r.read_bits(6) as i32;
    let _profile = r.read_bits(4);
    let _link = r.read_bits(2);
    let sample_rate = SAMPLE_FREQS[r.read_bits(2) as usize];
    let _estimated_peak = r.read_bits(16);
    let gain_title = r.read_bits(16) as u16;
    let peak_title = r.read_bits(16) as u16;
    let gain_album = r.read_bits(16) as u16;
    let peak_album = r.read_bits(16) as u16;
    let is_true_gapless = r.read_bit() != 0;
    let mut last_frame_samples = r.read_bits(11);
    let _fast_seek = r.read_bit();
    let _unused = r.read_bits(19);
    let _encoder_version = r.read_bits(8);
    // `r.bit_pos()` is now 168 (byte-aligned: 21 of the header's 24 bytes), matching
    // `streaminfo_read_header_sv7`'s exact bit consumption. The remaining 3 bytes are *not*
    // padding: libmpcdec's demuxer shares one continuous bit reader across the header and the
    // audio data, so the first frame's 20-bit length prefix begins right here, not at a fresh
    // byte-24 boundary. Carry them forward as the start of the audio bitstream (see
    // `Sv7State`'s docs above).
    debug_assert_eq!(r.bit_pos(), 168);

    if frames == 0 {
        return decode_error("musepack: zero-length SV7 stream");
    }
    if last_frame_samples > FRAME_LENGTH as u32 {
        return decode_error("musepack: invalid SV7 last-frame sample count");
    }
    if last_frame_samples == 0 {
        last_frame_samples = FRAME_LENGTH as u32;
    }

    let mut samples = frames * FRAME_LENGTH as u64;
    if is_true_gapless {
        let padding = FRAME_LENGTH as u64 - u64::from(last_frame_samples);
        samples = samples.checked_sub(padding).ok_or(Error::DecodeError(
            "musepack: SV7 gapless padding exceeds total samples",
        ))?;
    }
    else {
        samples = samples.checked_sub(u64::from(SYNTH_DELAY)).ok_or(Error::DecodeError(
            "musepack: SV7 sample count too small for synthesis delay",
        ))?;
    }

    if max_band == 0 || max_band >= 32 || sample_rate == 0 {
        return decode_error("musepack: invalid SV7 stream header");
    }

    // Ported from `mpc_decoder_set_streaminfo`: true-gapless SV7 streams round the decoder's
    // internal sample count up to a frame boundary (the exact count above is used for
    // `reported_samples()`/duration display instead).
    let decoder_samples = if is_true_gapless {
        samples.div_ceil(FRAME_LENGTH as u64) * FRAME_LENGTH as u64
    }
    else {
        samples
    };

    let info = StreamInfo {
        stream_version: 7,
        sample_rate,
        channels: 2,
        max_band,
        ms,
        block_pwr: 0,
        decoder_samples,
        beg_silence: 0,
        display_samples: samples,
        gain_title,
        peak_title,
        gain_album,
        peak_album,
        encoder: None,
    };

    let remaining = mss
        .byte_len()
        .map(|len| len.saturating_sub(mss.pos()))
        .unwrap_or(MAX_BUFFERED_BYTES);
    let to_read = remaining.min(MAX_BUFFERED_BYTES) as usize;
    // The header's last 3 bytes (bits 168..192) are the start of the continuous audio
    // bitstream, not unused padding -- see the comment above `debug_assert_eq!(r.bit_pos(), 168)`.
    let mut data = header[21..24].to_vec();
    data.resize(3 + to_read, 0);
    // `MediaSourceStream::read_buf` performs a single underlying read and may return fewer
    // bytes than requested even before EOF (e.g. limited by its internal ring buffer); loop
    // until the buffer is full or a `0`-byte read signals end-of-stream.
    let mut filled = 3usize;
    while filled < data.len() {
        let n = mss.read_buf(&mut data[filled..]).map_err(Error::IoError)?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    data.truncate(filled);
    // Byte-swap only the freshly-read audio bytes (the 3 carried-over header bytes are already
    // in the correct swapped order, having been swapped as part of the header's own 4-byte
    // words).
    byte_swap_words(&mut data[3..]);

    Ok((info, Sv7State { data, bit_pos: 0, frames_total: frames, frames_emitted: 0 }))
}

/// Copies `bit_len` bits starting at `bit_start` of `src` into a fresh, zero-padded byte buffer
/// with the frame's first bit realigned to bit 0 of byte 0. See the module-level docs.
fn extract_bits(src: &[u8], bit_start: u64, bit_len: u64) -> Vec<u8> {
    let n_bytes = ((bit_len + 7) / 8) as usize;
    let mut out = vec![0u8; n_bytes];
    for i in 0..bit_len {
        let src_bit = bit_start + i;
        let byte_idx = (src_bit >> 3) as usize;
        let bit_idx = 7 - (src_bit & 7) as u32;
        let bit = src.get(byte_idx).map(|b| (b >> bit_idx) & 1).unwrap_or(0);
        if bit != 0 {
            out[(i / 8) as usize] |= 1 << (7 - (i % 8) as u32);
        }
    }
    out
}

impl Sv7State {
    pub fn next_packet(&mut self, _mss: &mut MediaSourceStream<'_>) -> Result<Option<Vec<u8>>> {
        if self.frames_emitted >= self.frames_total {
            return Ok(None);
        }
        if self.bit_pos + 20 > (self.data.len() as u64) * 8 {
            // Ran out of buffered data before the header's declared frame count; treat as a
            // truncated (e.g. network-cut) stream rather than panicking or erroring.
            return Ok(None);
        }

        let mut r = BitReader::new(&self.data);
        r.set_bit_pos(self.bit_pos);
        let bit_len = u64::from(r.read_bits(20));
        let frame_start = r.bit_pos();

        if bit_len > 8 * 1024 * 1024 {
            return decode_error("musepack: implausible SV7 frame length");
        }

        // Ported from `mpc_demux_decode_inner`'s SV7 branch: the *last* frame in the stream is
        // immediately followed by an extra 11-bit "true last-frame sample count" field (read by
        // `decoder_core::Decoder::decode_frame`'s trailing-adjustment code), which is not
        // included in the 20-bit length prefix itself. Include those 11 bits only for the final
        // packet so that read lands on real bitstream content instead of zero-padding.
        let is_last_frame = self.frames_emitted + 1 == self.frames_total;
        let extract_len = if is_last_frame { bit_len + 11 } else { bit_len };

        let packet = extract_bits(&self.data, frame_start, extract_len);
        self.bit_pos = frame_start + bit_len;
        self.frames_emitted += 1;

        Ok(Some(packet))
    }

    /// Frame-accurate seek via linear scan (reading each frame's 20-bit length prefix and
    /// skipping its payload, without decoding). Returns the actual sample position (a multiple
    /// of `FRAME_LENGTH`, at or before `target_sample`).
    pub fn seek(&mut self, _mss: &mut MediaSourceStream<'_>, target_sample: u64) -> Result<u64> {
        let target_frame = target_sample / FRAME_LENGTH as u64;
        let target_frame = target_frame.min(self.frames_total.saturating_sub(1));

        let mut bit_pos = 0u64;
        let mut frame_idx = 0u64;
        while frame_idx < target_frame {
            if bit_pos + 20 > (self.data.len() as u64) * 8 {
                return seek_error(SeekErrorKind::OutOfRange);
            }
            let mut r = BitReader::new(&self.data);
            r.set_bit_pos(bit_pos);
            let bit_len = u64::from(r.read_bits(20));
            bit_pos = r.bit_pos() + bit_len;
            frame_idx += 1;
        }

        self.bit_pos = bit_pos;
        self.frames_emitted = frame_idx;
        Ok(frame_idx * FRAME_LENGTH as u64)
    }
}
