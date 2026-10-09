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

use symphonia_core::codecs::audio::AudioCodecId;
use symphonia_core::common::FourCc;

mod dff;
mod dff_info;
mod dsf;

pub use dff::DffReader;
pub use dsf::DsfReader;

/// Codec ID for DSD "DSD\0"
pub const CODEC_TYPE_DSD: AudioCodecId = AudioCodecId::new(FourCc::new(*b"DSD\0"));

// DSD sample rates (in Hz)
pub const DSD64_RATE: u32 = 2822400; // 64 * 44100
pub const DSD128_RATE: u32 = 5644800; // 128 * 44100
pub const DSD256_RATE: u32 = 11289600; // 256 * 44100
pub const DSD512_RATE: u32 = 22579200; // 512 * 44100
