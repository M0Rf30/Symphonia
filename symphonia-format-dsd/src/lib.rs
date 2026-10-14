// Symphonia DSD Format Demuxer
// Copyright (c) 2026 M0Rf30
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]

//! DSD (DSF and DSDIFF) demuxers.
//!
//! # Timeline convention
//!
//! The readers expose the raw, uncompressed 1-bit DSD stream. The timeline of the track counts
//! *DSD samples per channel*, at the DSD sampling rate (e.g. 2822400 Hz for DSD64):
//!
//! * `AudioCodecParameters::sample_rate` is the DSD rate, and the track time base is
//!   `1 / sample_rate`.
//! * `Track::num_frames`, `Packet::pts`, `Packet::dur`, and seek timestamps are in DSD samples per
//!   channel. One *byte* of one channel holds 8 DSD samples, so the number of bytes per channel in
//!   a packet is `dur / 8` (rounded up).
//! * `max_frames_per_packet` and `frames_per_block` are also in DSD samples.
//!
//! Packet data is the DSD bytes of all channels: planar (the bytes of each channel are contiguous)
//! for DSF, and byte-interleaved for DSDIFF; see `AudioCodecParameters::channel_data_layout` and
//! `AudioCodecParameters::bit_order`. A packet always contains an equal number of bytes for each
//! channel. Padding (the unused tail of the last DSF block) is not part of the packet data.
//!
//! The pass-through decoder (`symphonia-codec-dsd`) outputs `U8` buffers where one frame is one
//! byte per channel (8 DSD samples). The `AudioSpec` of those buffers has a rate of
//! `sample_rate / 8`, so that `frames() / rate()` is the duration in seconds.
//!
//! # DST (compressed) DSDIFF
//!
//! DSDIFF files whose compression type is `DST ` (Direct Stream Transfer, ISO/IEC 14496-3
//! Subpart 10, as used by SACD) are exposed as a track with the codec [`CODEC_TYPE_DST`]. The
//! timeline is the same as for uncompressed DSD: `Packet::pts` and `Packet::dur` count the DSD
//! samples per channel that the packet decodes to (`sample_rate / frame_rate` samples per DST
//! frame, e.g. 37632 for DSD64 at 75 frames per second), and `max_frames_per_packet` is that
//! number. The decoded data is byte-interleaved, most significant bit first, like uncompressed
//! DSDIFF.
//!
//! One packet holds one DST frame (a `DSTF` chunk). The packet data is the following:
//!
//! | bytes  | meaning                                                                     |
//! |--------|-----------------------------------------------------------------------------|
//! | 0      | flags; bit 0 is set when a frame CRC follows (the frame has a `DSTC` chunk) |
//! | 1..5   | big-endian CRC-32 of the decoded DSD frame (zero if bit 0 is clear)         |
//! | rest   | the DST frame as stored in the `DSTF` chunk                                 |
//!
//! # Robustness
//!
//! Chunk sizes are never trusted beyond the end of the file: a truncated data chunk delivers the
//! complete samples that are present, then ends the stream.

use symphonia_core::codecs::audio::AudioCodecId;
use symphonia_core::common::FourCc;
use symphonia_core::errors::Result;
use symphonia_core::io::ReadBytes;

mod dff;
mod dff_info;
mod dsf;

pub use dff::DffReader;
pub use dsf::DsfReader;

/// Codec ID for DSD "DSD\0"
pub const CODEC_TYPE_DSD: AudioCodecId = AudioCodecId::new(FourCc::new(*b"DSD\0"));

/// Codec ID for DST (Direct Stream Transfer) compressed DSD "DST\0"
pub const CODEC_TYPE_DST: AudioCodecId = AudioCodecId::new(FourCc::new(*b"DST\0"));

/// The largest number of channels accepted. The DSF specification allows 1 to 6 channels and
/// DSDIFF any number greater than zero; this bound only protects against corrupt headers.
pub(crate) const MAX_DSD_CHANNELS: u32 = 64;

/// Fill `buf`, stopping early only at the end of the stream. Returns the number of bytes read.
pub(crate) fn read_up_to_into<B: ReadBytes>(reader: &mut B, buf: &mut [u8]) -> Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match reader.read_buf(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(got)
}

/// Read up to `len` bytes, stopping early only at the end of the stream.
pub(crate) fn read_up_to<B: ReadBytes>(reader: &mut B, len: usize) -> Result<Box<[u8]>> {
    let mut buf = vec![0u8; len];
    let got = read_up_to_into(reader, &mut buf)?;
    buf.truncate(got);
    Ok(buf.into_boxed_slice())
}

// DSD sample rates (in Hz)
pub const DSD64_RATE: u32 = 2822400; // 64 * 44100
pub const DSD128_RATE: u32 = 5644800; // 128 * 44100
pub const DSD256_RATE: u32 = 11289600; // 256 * 44100
pub const DSD512_RATE: u32 = 22579200; // 512 * 44100
