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
use crate::decoder_core::{Decoder as Core, Sv7Sync, FRAME_LENGTH, SYNTH_DELAY};

use super::{StreamInfo, PACKET_TAG_PLAIN, PACKET_TAG_SYNC};

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
    /// Stream parameters needed to parse frames outside of the decoder (see `build_index`).
    max_band: i32,
    ms: bool,
    /// Lazily built by the first seek.
    index: Option<FrameIndex>,
    /// Decoder state to attach to the next packet (set by a seek).
    pending_sync: Option<Sv7Sync>,
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

    // The decoder's output lags its input by `SYNTH_DELAY`, so a stream of `frames` frames can
    // never yield more than `frames * FRAME_LENGTH - SYNTH_DELAY` samples. A true-gapless
    // last-frame count above `FRAME_LENGTH - SYNTH_DELAY` would otherwise claim samples that
    // were never encoded (libmpcdec silently stops short in that case as well).
    let max_output = (frames * FRAME_LENGTH as u64).saturating_sub(u64::from(SYNTH_DELAY));

    let info = StreamInfo {
        stream_version: 7,
        sample_rate,
        channels: 2,
        max_band,
        ms,
        block_pwr: 0,
        decoder_samples,
        beg_silence: 0,
        display_samples: samples.min(max_output),
        total_frames: Some(frames),
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

    Ok((
        info,
        Sv7State {
            data,
            bit_pos: 0,
            frames_total: frames,
            frames_emitted: 0,
            max_band,
            ms,
            index: None,
            pending_sync: None,
        },
    ))
}

/// Appends `bit_len` bits starting at `bit_start` of `src` to `out`, with the first bit realigned
/// to bit 0 of a fresh byte and the final byte zero-padded. Bits past the end of `src` read as
/// zero. See the module-level docs.
fn extract_bits(src: &[u8], bit_start: u64, bit_len: u64, out: &mut Vec<u8>) {
    let n_bytes = bit_len.div_ceil(8) as usize;
    let byte0 = (bit_start >> 3) as usize;
    let shift = (bit_start & 7) as u32;
    let at = |i: usize| src.get(byte0 + i).copied().unwrap_or(0);

    let base = out.len();
    out.extend(
        (0..n_bytes).map(|i| {
            if shift == 0 { at(i) } else { (at(i) << shift) | (at(i + 1) >> (8 - shift)) }
        }),
    );
    // Zero the padding bits after the last frame bit.
    let tail = (bit_len & 7) as u32;
    if tail != 0 {
        out[base + n_bytes - 1] &= 0xFFu8 << (8 - tail);
    }
}

/// Number of frames between two stored [`Sv7Sync`] checkpoints.
const CHECKPOINT_INTERVAL: u64 = 64;

/// Random-access index over the buffered audio region, built lazily by the first seek.
struct FrameIndex {
    /// Bit position of each frame's 20-bit length prefix, for every complete frame present in
    /// the buffered data.
    frame_pos: Vec<u64>,
    /// SV7 inter-frame decoder state at the start of every `CHECKPOINT_INTERVAL`th frame.
    checkpoints: Vec<Sv7Sync>,
}

impl Sv7State {
    /// Walks every frame once, parsing (but not synthesizing) its bitstream, to record where each
    /// frame starts and what the decoder's scale-factor state is on entry to every
    /// `CHECKPOINT_INTERVAL`th frame. Parsing a frame is a small fraction of the cost of decoding
    /// it, and the result is reused by every later seek.
    fn build_index(&self) -> FrameIndex {
        let total_bits = (self.data.len() as u64) * 8;
        let mut parser = Core::new(7, self.max_band, self.ms, 2);
        let mut frame_pos = Vec::new();
        let mut checkpoints = Vec::new();
        let mut pos = 0u64;

        let mut scratch = Vec::new();
        for frame in 0..self.frames_total {
            if pos + 20 > total_bits {
                break;
            }
            let mut r = BitReader::new(&self.data);
            r.set_bit_pos(pos);
            let bit_len = u64::from(r.read_bits(20));
            if pos + 20 + bit_len > total_bits {
                // Truncated final frame: not seekable-to (it is still emitted linearly).
                break;
            }
            if frame % CHECKPOINT_INTERVAL == 0 {
                checkpoints.push(parser.sv7_sync());
            }
            frame_pos.push(pos);
            pos = parse_frame(&mut parser, &self.data, pos, &mut scratch);
        }
        FrameIndex { frame_pos, checkpoints }
    }
}

/// Parses the frame whose length prefix is at `pos` exactly as the decoder will see it (from its
/// own zero-padded copy, so a frame that over- or under-runs its declared length cannot desync
/// the state), and returns the position of the next frame's length prefix.
fn parse_frame(parser: &mut Core, data: &[u8], pos: u64, scratch: &mut Vec<u8>) -> u64 {
    let mut r = BitReader::new(data);
    r.set_bit_pos(pos);
    let bit_len = u64::from(r.read_bits(20));
    let start = r.bit_pos();
    scratch.clear();
    extract_bits(data, start, bit_len, scratch);
    parser.skip_frame_sv7(&mut BitReader::new(scratch));
    start + bit_len
}

/// The decoder state on entry to `frame`, which must be indexed.
fn sync_at(data: &[u8], max_band: i32, ms: bool, index: &FrameIndex, frame: u64) -> Sv7Sync {
    let ckpt = (frame / CHECKPOINT_INTERVAL) as usize;
    let mut parser = Core::new(7, max_band, ms, 2);
    parser.set_sv7_sync(&index.checkpoints[ckpt]);
    let mut scratch = Vec::new();
    for f in (ckpt as u64 * CHECKPOINT_INTERVAL)..frame {
        parse_frame(&mut parser, data, index.frame_pos[f as usize], &mut scratch);
    }
    parser.sv7_sync()
}

impl Sv7State {
    /// Returns the next packet's payload: a one-byte tag (`0`, or `1` followed by an encoded
    /// [`Sv7Sync`] if this is the first packet after a seek) followed by the realigned frame
    /// bits. See `crate::decoder::MpcDecoder` for the consumer.
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

        let mut packet = Vec::with_capacity(1 + ((bit_len + 7) / 8) as usize);
        match self.pending_sync.take() {
            Some(sync) => {
                packet.push(PACKET_TAG_SYNC);
                sync.write_to(&mut packet);
            }
            None => packet.push(PACKET_TAG_PLAIN),
        }
        // The 11-bit "true last-frame sample count" that follows the final frame in the
        // reference bitstream is not passed on: gapless trimming is expressed through the
        // packet's `trim_end` instead (see `decoder_core::Decoder::decode_frame`).
        extract_bits(&self.data, frame_start, bit_len, &mut packet);
        self.bit_pos = frame_start + bit_len;
        self.frames_emitted += 1;

        Ok(Some(packet))
    }

    /// Positions the reader at the start of `frame` so that the next packet decodes correctly
    /// from there (it carries the recovered scale-factor state, see [`Sv7Sync`]).
    pub fn seek_frame(&mut self, frame: u64) -> Result<()> {
        if self.index.is_none() {
            self.index = Some(self.build_index());
        }
        let index = self.index.as_ref().expect("index was just built");
        let Some(&pos) = index.frame_pos.get(frame as usize) else {
            return seek_error(SeekErrorKind::OutOfRange);
        };
        self.pending_sync = Some(sync_at(&self.data, self.max_band, self.ms, index, frame));
        self.bit_pos = pos;
        self.frames_emitted = frame;
        Ok(())
    }
}
