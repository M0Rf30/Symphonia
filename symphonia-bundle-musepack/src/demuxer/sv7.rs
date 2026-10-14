// Symphonia Musepack demuxer (SV4-SV7)
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SV4-SV7 frame demuxer. The older versions (SV4-SV6) only differ from SV7 in their header and
//! in the frame contents, which are the decoder's business: the two header parsers below set up
//! the same state.
//!
//! Header parsing is ported from libmpcdec `streaminfo.c` (`streaminfo_read_header_sv7` and
//! `streaminfo_read_header_sv6`) (BSD-3-Clause), see `NOTICE`.
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

use symphonia_core::errors::{Error, Result, SeekErrorKind, decode_error, seek_error};
use symphonia_core::io::{MediaSource, MediaSourceStream, ReadBytes};

use crate::bits::BitReader;
use crate::decoder_core::{Decoder as Core, FRAME_LENGTH, SYNTH_DELAY, ScfSync};

use super::{PACKET_TAG_PLAIN, PACKET_TAG_SILENCE, PACKET_TAG_SYNC, StreamInfo};

const SAMPLE_FREQS: [u32; 4] = [44100, 48000, 37800, 32000];
/// Maximum amount of audio-region data this demuxer will buffer for one SV7 stream.
const MAX_BUFFERED_BYTES: u64 = 512 * 1024 * 1024;

fn byte_swap_words(buf: &mut [u8]) {
    for chunk in buf.chunks_exact_mut(4) {
        chunk.swap(0, 3);
        chunk.swap(1, 2);
    }
}

/// The extra frame some streams need after their last one to push the synthesis filter's tail
/// out, see `read_header` (SV7) and `read_header_sv46`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flush {
    None,
    /// SV7: the frame is encoded in the bits following the last frame's 11-bit sample count.
    Encoded,
    /// SV4-SV6: the frame has no data, it is synthesized from silence.
    Silent,
}

pub(crate) struct Sv7State {
    /// Byte-swapped stream bytes. For SV7 this is the audio region only (header excluded, it
    /// starts at the first frame's first bit's byte); SV4-SV6 keep their header in front, see
    /// `read_header_sv46`.
    data: Vec<u8>,
    /// Current read position, in bits, into `data`.
    bit_pos: u64,
    /// Bit position of the first frame's length prefix.
    first_frame_bit: u64,
    stream_version: u32,
    /// Total number of frames in the stream (from the header's `frames` field); `next_packet`
    /// stops once this many have been emitted, regardless of any trailing bytes in `data` (e.g.
    /// an appended APEv2 tag).
    frames_total: u64,
    frames_emitted: u64,
    /// The extra packet after the last frame, if any.
    flush: Flush,
    /// Stream parameters needed to parse frames outside of the decoder (see `build_index`).
    max_band: i32,
    ms: bool,
    /// SV4-SV5 features of the stream, see `Decoder::set_sv46_layout`.
    raw_res: bool,
    is_start: Option<usize>,
    /// Frames per length prefix: `1` for every frame carrying its own (all SV7 streams and the
    /// usual SV4-SV6 ones), more for SV4-SV6 streams written with `-frm`, whose frames then only
    /// have a length prefix in front of every `block`th one.
    block: u64,
    /// Parses the frames of a stream with `block > 1` to find where each one ends, which none of
    /// the stream's own fields tell. Follows the stream as `next_packet` advances.
    parser: Option<Box<Core>>,
    /// Lazily built by the first seek.
    index: Option<FrameIndex>,
    /// Decoder state to attach to the next packet (set by a seek).
    pending_sync: Option<ScfSync>,
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
        samples = samples
            .checked_sub(padding)
            .ok_or(Error::DecodeError("musepack: SV7 gapless padding exceeds total samples"))?;
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

    // The decoder's output lags its input by `SYNTH_DELAY`, so the last `SYNTH_DELAY` samples
    // of a true-gapless stream whose last frame holds more than `FRAME_LENGTH - SYNTH_DELAY`
    // samples only leave the synthesis filter while the decoder runs one frame past the end of
    // the stream (libmpcdec decodes it as well). The demuxer emits that frame as an empty
    // "flush" packet.
    let flush_frame = is_true_gapless && last_frame_samples > FRAME_LENGTH as u32 - SYNTH_DELAY;
    let flush = if flush_frame { Flush::Encoded } else { Flush::None };
    let decoded_frames = frames + u64::from(flush_frame);
    let max_output = (decoded_frames * FRAME_LENGTH as u64).saturating_sub(u64::from(SYNTH_DELAY));
    let info = StreamInfo {
        stream_version: 7,
        sample_rate,
        channels: 2,
        max_band,
        ms,
        raw_res: false,
        is_start: None,
        block_pwr: 0,
        decoder_samples,
        beg_silence: 0,
        display_samples: samples.min(max_output),
        total_frames: Some(decoded_frames),
        gain_title,
        peak_title,
        gain_album,
        peak_album,
        encoder: None,
    };

    // The header's last 3 bytes (bits 168..192) are the start of the continuous audio
    // bitstream, not unused padding -- see the comment above `debug_assert_eq!(r.bit_pos(), 168)`.
    let mut data = header[21..24].to_vec();
    append_rest(mss, &mut data)?;
    // Byte-swap only the freshly-read audio bytes (the 3 carried-over header bytes are already
    // in the correct swapped order, having been swapped as part of the header's own 4-byte
    // words).
    byte_swap_words(&mut data[3..]);

    Ok((
        info,
        Sv7State {
            data,
            bit_pos: 0,
            first_frame_bit: 0,
            stream_version: 7,
            frames_total: frames,
            frames_emitted: 0,
            flush,
            max_band,
            ms,
            raw_res: false,
            is_start: None,
            block: 1,
            parser: None,
            index: None,
            pending_sync: None,
        },
    ))
}

/// The fields of an SV4-SV6 file header's first 32-bit word that identify and configure the
/// stream, ported from libmpcdec `streaminfo.c` (`streaminfo_read_header_sv6`).
pub(crate) struct Sv46Header {
    pub stream_version: u32,
    /// Fixed bitrate in kbit/s, `0` for variable bitrate streams.
    pub bitrate: u32,
    pub intensity_stereo: bool,
    pub ms: bool,
    /// The header's maximum band field, see [`Sv46Header::layout`].
    pub max_band: u32,
    /// Frames bundled by the encoder's bit allocation (`-frm`): only the first frame of each
    /// group is preceded by the 20-bit length prefix, which covers the whole group.
    pub block_size: u32,
}

/// The band layout of an SV4-SV5 stream, derived by [`Sv46Header::layout`].
pub(crate) struct Sv46Layout {
    /// Highest coded band.
    pub max_band: i32,
    /// First intensity stereo band, if the stream uses intensity stereo.
    pub is_start: Option<usize>,
    /// Whether the band resolutions use fixed-length codes (bitrates above 128 kbit/s).
    pub raw_res: bool,
}

impl Sv46Header {
    /// Parses the first four bytes of a file, `None` unless they carry an SV4, SV5 or SV6 header.
    /// The format has no magic number: this relies on the stream version field alone, so callers
    /// that probe should validate more (see `demuxer::score_sv46`).
    pub fn parse(first_word: [u8; 4]) -> Option<Self> {
        let w = u32::from_le_bytes(first_word);
        let stream_version = (w >> 11) & 0x3FF;
        (4..=6).contains(&stream_version).then_some(Sv46Header {
            stream_version,
            bitrate: (w >> 23) & 0x1FF,
            intensity_stereo: (w >> 22) & 1 != 0,
            ms: (w >> 21) & 1 != 0,
            max_band: (w >> 6) & 0x1F,
            block_size: (w & 0x3F).max(1),
        })
    }

    /// The band layout the stream's decoder works with, ported from the SV5 era decoder
    /// (`mppdec`, 18 Jun 2000), whose bandwidth and intensity stereo start band depend on the
    /// bitrate. With a zero `max_band` field the bandwidth is derived from the bitrate alone.
    pub fn layout(&self) -> Sv46Layout {
        let br = self.bitrate;
        let raw_res = br > 128;
        let field = self.max_band as i32;
        if !self.intensity_stereo {
            let max_band = if field != 0 {
                field
            }
            else if br == 0 {
                31
            }
            else if br < 0x41 {
                20
            }
            else if br < 0xA1 {
                26
            }
            else if br > 0x180 {
                31
            }
            else {
                29
            };
            return Sv46Layout { max_band, is_start: None, raw_res };
        }

        // Intensity stereo: the start band falls with the bitrate. Above 384 kbit/s the
        // reference decoder never sets one (so the whole spectrum is intensity coded).
        let (mut is_start, mut max_band) = (0usize, field);
        for (limit, start, band) in [(0x181, 16, 29), (0xA1, 12, 26), (0x71, 8, 26), (0x41, 4, 20)]
        {
            if br < limit {
                is_start = start;
                max_band = if field != 0 { field } else { band };
            }
        }
        is_start = is_start.min(max_band as usize);
        Sv46Layout { max_band, is_start: Some(is_start), raw_res }
    }

    /// Number of frames the stream declares, from the header's second word.
    pub fn frame_count(&self, second_word: [u8; 4]) -> u64 {
        let w = u32::from_le_bytes(second_word);
        match self.stream_version {
            4 => u64::from(w >> 16).saturating_sub(1),
            5 => u64::from(w).saturating_sub(1),
            _ => u64::from(w),
        }
    }

    /// Bit position of the first frame's length prefix.
    pub fn first_frame_bit(&self) -> u64 {
        if self.stream_version == 4 { 48 } else { 64 }
    }
}

/// Ported from libmpcdec `streaminfo.c` (`streaminfo_read_header_sv6`) and the SV6 last-frame
/// handling of `mpc_decoder_decode_frame`, extended with what libmpcdec rejects: intensity
/// stereo, block sizes above one and fixed bitrates (see [`Sv46Header::layout`] and
/// [`Sv7State`]), whose semantics come from the SV5 era decoder.
///
/// SV4-SV6 share SV7's bitstream layout (a continuous sequence of 20-bit length-prefixed frames
/// in little-endian 32-bit words), only their header is different: it is part of the same
/// bitstream, so (unlike for SV7) the buffered data starts at the file's first byte and the
/// first frame at `first_frame_bit`. The streams carry the frame count of the header minus one
/// (SV4, SV5; the last frame they contain is invalid) or exactly (SV6); a SV6 stream also stores
/// its true length in the 11 bits following its last frame.
pub(crate) fn read_header_sv46(
    mss: &mut MediaSourceStream<'_>,
    first_word: [u8; 4],
) -> Result<(StreamInfo, Sv7State)> {
    let Some(header) = Sv46Header::parse(first_word)
    else {
        return decode_error("musepack: unsupported stream version");
    };
    let layout = header.layout();
    let block = u64::from(header.block_size);

    let mut data = first_word.to_vec();
    append_rest(mss, &mut data)?;
    if data.len() < 8 {
        return decode_error("musepack: truncated SV4-SV6 header");
    }
    let frames = header.frame_count([data[4], data[5], data[6], data[7]]);
    // The reference reads the bitstream as whole words, the missing bytes of a final partial one
    // being zero.
    data.resize(data.len().next_multiple_of(4), 0);
    byte_swap_words(&mut data);

    let stream_version = header.stream_version;
    if frames == 0 {
        return decode_error("musepack: zero-length SV4-SV6 stream");
    }

    let first_frame_bit = header.first_frame_bit();
    let frame_len = FRAME_LENGTH as u64;
    let delay = u64::from(SYNTH_DELAY);

    let mut state = Sv7State {
        data,
        bit_pos: first_frame_bit,
        first_frame_bit,
        stream_version,
        frames_total: frames,
        frames_emitted: 0,
        flush: Flush::None,
        max_band: layout.max_band,
        ms: header.ms,
        raw_res: layout.raw_res,
        is_start: layout.is_start,
        block,
        parser: None,
        index: None,
        pending_sync: None,
    };
    if block > 1 {
        state.parser = Some(Box::new(state.new_parser()));
    }

    // Number of audible samples, and whether the decoder has to run one frame past the end of the
    // stream to produce them all (as for SV7, see `read_header`).
    let samples = if stream_version < 6 {
        frames * frame_len - delay
    }
    else {
        // Walk to the end of the last frame, where the true length of its audible part is
        // stored. If the stream is cut short that field is absent, i.e. a full frame.
        let (end, complete) = state.frames_end();
        let mut r = BitReader::new(&state.data);
        r.set_bit_pos(end);
        let mut last_frame_samples = if complete { u64::from(r.read_bits(11)) } else { 0 };
        if last_frame_samples == 0 {
            last_frame_samples = frame_len;
        }

        // `FilterDecay` and the output length of the last frame, as in the reference.
        let decay = (last_frame_samples + delay) % frame_len;
        let last_frame_output =
            if last_frame_samples + delay >= frame_len { frame_len + decay } else { decay };
        if last_frame_samples + delay > frame_len {
            state.flush = Flush::Silent;
        }
        (frames - 1) * frame_len + last_frame_output - delay
    };

    let decoded_frames = frames + u64::from(state.flush == Flush::Silent);
    let max_output = decoded_frames * frame_len - delay;
    let info = StreamInfo {
        stream_version,
        sample_rate: 44100,
        channels: 2,
        max_band: layout.max_band,
        ms: header.ms,
        raw_res: layout.raw_res,
        is_start: layout.is_start,
        block_pwr: 0,
        decoder_samples: samples,
        beg_silence: 0,
        display_samples: samples.min(max_output),
        total_frames: Some(decoded_frames),
        gain_title: 0,
        peak_title: 0,
        gain_album: 0,
        peak_album: 0,
        encoder: None,
    };

    Ok((info, state))
}

/// Whether `window`, the start of a stream, is the start of a supported SV4-SV6 stream.
///
/// Those versions have no magic number, so besides the header (which `Sv46Header::parse`
/// already narrows down) this parses the first frames the way the reference decoder validates
/// every frame: a frame is only accepted if its bitstream ends exactly where its length prefix
/// says it does, which random data virtually never satisfies. For the streams whose prefix covers
/// a group of frames (`-frm`) the first group has to end there (`at_start`: the window is the
/// start of the file, where only those streams are looked for).
pub(crate) fn looks_like_sv46(window: &[u8], at_start: bool) -> bool {
    const FRAMES_TO_CHECK: usize = 4;

    let Some(header) = window.first_chunk().and_then(|w| Sv46Header::parse(*w))
    else {
        return false;
    };

    let mut data = window[..window.len() & !3].to_vec();
    byte_swap_words(&mut data);
    let total_bits = (data.len() as u64) * 8;

    let layout = header.layout();
    let mut parser = Core::new(header.stream_version, layout.max_band, header.ms, 2);
    parser.set_sv46_layout(layout.raw_res, layout.is_start);
    let mut scratch = Vec::new();
    let mut pos = header.first_frame_bit();

    if header.block_size > 1 {
        // The group check can fall short of proof (see below), and a marker is a lot likelier to
        // be a coincidence than a stream start: these are only taken from the file's start.
        if !at_start {
            return false;
        }
        let Some(&second) = window.get(4..8).and_then(|w| w.first_chunk::<4>())
        else {
            return false;
        };
        if pos + 20 > total_bits {
            return false;
        }
        let mut r = BitReader::new(&data);
        r.set_bit_pos(pos);
        let group_bits = u64::from(r.read_bits(20));
        let group_start = r.bit_pos();
        if group_bits == 0 {
            return false;
        }
        let frames = u64::from(header.block_size).min(header.frame_count(second));
        let (mut parsed, mut used) = (0, 0);
        for _ in 0..frames {
            parser.skip_frame(&mut r);
            if r.bit_pos() > total_bits {
                break;
            }
            parsed += 1;
            used = r.bit_pos() - group_start;
        }
        return if parsed == frames {
            // The whole group is in the window: it has to end where the prefix says.
            used == group_bits
        }
        else {
            // The window ends inside the group, so only its size can be checked: the frames of
            // a group are alike, so the length has to be about what the frames seen so far
            // extrapolate to. (Random data satisfies this far less often than the exact check
            // above, but not never.)
            parsed >= 2 && {
                let estimate = used / parsed * frames;
                (estimate / 4..=estimate * 4).contains(&group_bits)
            }
        };
    }

    let mut checked = 0;
    while checked < FRAMES_TO_CHECK && pos + 20 <= total_bits {
        let mut r = BitReader::new(&data);
        r.set_bit_pos(pos);
        let bit_len = u64::from(r.read_bits(20));
        if bit_len == 0 || pos + 20 + bit_len > total_bits {
            break;
        }
        scratch.clear();
        extract_bits(&data, pos + 20, bit_len, &mut scratch);
        let mut frame = BitReader::new(&scratch);
        parser.skip_frame(&mut frame);
        if frame.bit_pos() != bit_len {
            return false;
        }
        pos += 20 + bit_len;
        checked += 1;
    }
    checked > 0
}

/// Appends the rest of `mss` to `data`, up to [`MAX_BUFFERED_BYTES`].
fn append_rest(mss: &mut MediaSourceStream<'_>, data: &mut Vec<u8>) -> Result<()> {
    let remaining =
        mss.byte_len().map(|len| len.saturating_sub(mss.pos())).unwrap_or(MAX_BUFFERED_BYTES);
    let start = data.len();
    data.resize(start + remaining.min(MAX_BUFFERED_BYTES) as usize, 0);
    // `MediaSourceStream::read_buf` performs a single underlying read and may return fewer
    // bytes than requested even before EOF (e.g. limited by its internal ring buffer); loop
    // until the buffer is full or a `0`-byte read signals end-of-stream.
    let mut filled = start;
    while filled < data.len() {
        let n = mss.read_buf(&mut data[filled..]).map_err(Error::IoError)?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    data.truncate(filled);
    Ok(())
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

/// Number of frames between two stored [`ScfSync`] checkpoints.
const CHECKPOINT_INTERVAL: u64 = 64;

/// Random-access index over the buffered audio region, built lazily by the first seek.
struct FrameIndex {
    /// Bit position of each frame's first bit (its length prefix, if it has one), for every
    /// complete frame present in the buffered data.
    frame_pos: Vec<u64>,
    /// SV4-SV7 inter-frame decoder state at the start of every `CHECKPOINT_INTERVAL`th frame.
    checkpoints: Vec<ScfSync>,
}

/// Where a frame's bitstream lies, see [`locate`].
struct Span {
    /// Bit position of the first bit after the frame's length prefix, if any.
    start: u64,
    /// Length of the frame's bitstream in bits.
    len: u64,
}

/// Finds the bitstream of frame number `frame`, whose first bit (length prefix included, if it
/// has one) is at `pos`, parsing it with `parser` so that the parser's inter-frame state advances
/// to the next frame.
///
/// Streams with a prefix in front of every frame (`block == 1`) state the length, and the frame
/// is parsed exactly as the decoder will see it (from its own zero-padded copy, so a frame that
/// over- or under-runs its declared length cannot desync the state). For the others only the
/// parse tells where a frame ends.
fn locate(
    parser: &mut Core,
    data: &[u8],
    pos: u64,
    frame: u64,
    block: u64,
    scratch: &mut Vec<u8>,
) -> Span {
    let mut r = BitReader::new(data);
    r.set_bit_pos(pos);
    if block == 1 {
        let len = u64::from(r.read_bits(20));
        let start = r.bit_pos();
        scratch.clear();
        extract_bits(data, start, len, scratch);
        parser.skip_frame(&mut BitReader::new(scratch));
        Span { start, len }
    }
    else {
        if frame % block == 0 {
            r.skip_bits(20);
        }
        let start = r.bit_pos();
        parser.skip_frame(&mut r);
        Span { start, len: r.bit_pos() - start }
    }
}

impl Sv7State {
    /// A decoder core set up like the stream's decoder, to parse frames outside of it.
    fn new_parser(&self) -> Core {
        let mut core = Core::new(self.stream_version, self.max_band, self.ms, 2);
        core.set_sv46_layout(self.raw_res, self.is_start);
        core
    }

    /// Bit position just past the last frame, and whether all of the stream's frames are
    /// present in the buffered data.
    fn frames_end(&self) -> (u64, bool) {
        let total_bits = (self.data.len() as u64) * 8;
        let mut pos = self.first_frame_bit;
        let mut r = BitReader::new(&self.data);
        if self.block == 1 {
            for _ in 0..self.frames_total {
                if pos + 20 > total_bits {
                    return (pos, false);
                }
                r.set_bit_pos(pos);
                pos += 20 + u64::from(r.read_bits(20));
            }
        }
        else {
            let mut parser = self.new_parser();
            let mut scratch = Vec::new();
            for frame in 0..self.frames_total {
                if pos + 20 > total_bits {
                    return (pos, false);
                }
                let span = locate(&mut parser, &self.data, pos, frame, self.block, &mut scratch);
                pos = span.start + span.len;
            }
        }
        (pos, true)
    }

    /// Walks every frame once, parsing (but not synthesizing) its bitstream, to record where each
    /// frame starts and what the decoder's scale-factor state is on entry to every
    /// `CHECKPOINT_INTERVAL`th frame. Parsing a frame is a small fraction of the cost of decoding
    /// it, and the result is reused by every later seek.
    fn build_index(&self) -> FrameIndex {
        let total_bits = (self.data.len() as u64) * 8;
        let mut parser = self.new_parser();
        let mut frame_pos = Vec::new();
        let mut checkpoints = Vec::new();
        let mut pos = self.first_frame_bit;

        let mut scratch = Vec::new();
        for frame in 0..self.frames_total {
            if pos + 20 > total_bits {
                break;
            }
            let sync = (frame % CHECKPOINT_INTERVAL == 0).then(|| parser.scf_sync());
            let span = locate(&mut parser, &self.data, pos, frame, self.block, &mut scratch);
            if span.start + span.len > total_bits {
                // Truncated final frame: not seekable-to (it is still emitted linearly).
                break;
            }
            checkpoints.extend(sync);
            frame_pos.push(pos);
            pos = span.start + span.len;
        }
        FrameIndex { frame_pos, checkpoints }
    }
}

/// The decoder state on entry to `frame`, which must be indexed.
fn sync_at(state: &Sv7State, index: &FrameIndex, frame: u64) -> ScfSync {
    let ckpt = (frame / CHECKPOINT_INTERVAL) as usize;
    let mut parser = state.new_parser();
    parser.set_scf_sync(&index.checkpoints[ckpt]);
    let mut scratch = Vec::new();
    for f in (ckpt as u64 * CHECKPOINT_INTERVAL)..frame {
        locate(&mut parser, &state.data, index.frame_pos[f as usize], f, state.block, &mut scratch);
    }
    parser.scf_sync()
}

impl Sv7State {
    /// Returns the next packet's payload: a one-byte tag (`0`, or `1` followed by an encoded
    /// [`ScfSync`] if this is the first packet after a seek) followed by the realigned frame
    /// bits. See `crate::decoder::MpcDecoder` for the consumer.
    pub fn next_packet(&mut self, _mss: &mut MediaSourceStream<'_>) -> Result<Option<Vec<u8>>> {
        if self.frames_emitted >= self.frames_total {
            if self.flush == Flush::None || self.frames_emitted > self.frames_total {
                return Ok(None);
            }
            if self.flush == Flush::Silent {
                self.frames_emitted += 1;
                return Ok(Some(vec![PACKET_TAG_SILENCE]));
            }
            // Like libmpcdec, decode the flush frame from the bits that follow the last frame's
            // 11-bit last-frame sample count (bits past the end read as zero).
            self.bit_pos += 11;
        }
        else if self.bit_pos + 20 > (self.data.len() as u64) * 8 {
            // Ran out of buffered data before the header's declared frame count; treat as a
            // truncated (e.g. network-cut) stream rather than panicking or erroring.
            return Ok(None);
        }

        let (frame_start, bit_len) = match self.parser.as_mut() {
            Some(parser) => {
                let span = locate(
                    parser,
                    &self.data,
                    self.bit_pos,
                    self.frames_emitted,
                    self.block,
                    &mut Vec::new(),
                );
                (span.start, span.len)
            }
            None => {
                let mut r = BitReader::new(&self.data);
                r.set_bit_pos(self.bit_pos);
                let bit_len = u64::from(r.read_bits(20));
                (r.bit_pos(), bit_len)
            }
        };

        if bit_len > 8 * 1024 * 1024 {
            return decode_error("musepack: implausible SV7 frame length");
        }

        let mut packet = Vec::with_capacity(1 + bit_len.div_ceil(8) as usize);
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
    /// from there (it carries the recovered scale-factor state, see [`ScfSync`]).
    pub fn seek_frame(&mut self, frame: u64) -> Result<()> {
        if self.index.is_none() {
            self.index = Some(self.build_index());
        }
        let index = self.index.as_ref().expect("index was just built");
        let Some(&pos) = index.frame_pos.get(frame as usize)
        else {
            return seek_error(SeekErrorKind::OutOfRange);
        };
        let sync = sync_at(self, index, frame);
        if let Some(parser) = self.parser.as_mut() {
            parser.set_scf_sync(&sync);
        }
        self.pending_sync = Some(sync);
        self.bit_pos = pos;
        self.frames_emitted = frame;
        Ok(())
    }
}
