// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A pure-Rust Opus **encoder** (RFC 6716, CELT-only mode), behind the `encoder` cargo feature.
//!
//! The encoder is a port of the CELT half of the reference `libopus` encoder (float build,
//! BSD-3-Clause; see `NOTICE`), specialised for music streaming:
//!
//! - 48 kHz input, mono or stereo, interleaved `f32` samples in the nominal range `[-1, 1]`;
//! - 20 ms frames (960 samples per channel), one frame per Opus packet (TOC frame code 0);
//! - constant bitrate ([`BitrateMode::Cbr`]), variable bitrate ([`BitrateMode::Vbr`]) or
//!   constrained VBR ([`BitrateMode::ConstrainedVbr`]) at a configurable bitrate
//!   (6..=510 kbps; intended for 32..=256 kbps);
//! - a complexity knob (0..=10) that enables progressively more analysis (transient detection,
//!   TF analysis, spreading decision, two-pass energy coding, a second long MDCT for transients);
//! - automatic or explicit audio bandwidth (narrowband to fullband).
//!
//! The produced packets are ordinary Opus packets: the TOC byte selects "CELT-only, 20 ms" and
//! the payload is a standard CELT frame (coarse + fine band energies, TF/spread/allocation
//! side information, PVQ-coded band shapes, intensity/dual/mid-side stereo). Any conforming
//! decoder (including this crate's [`crate::OpusDecoder`]) can decode them.
//!
//! Features of libopus that are *not* implemented: SILK and hybrid modes, the pitch pre-filter
//! (the post-filter flag is always sent as off), the tonality analysis, LFE/surround masking,
//! frame sizes other than 20 ms and QEXT. They affect compression efficiency only, never
//! stream validity.
//!
//! # Example
//!
//! ```
//! use symphonia_codec_opus::encoder::{EncoderConfig, OpusEncoder, BitrateMode};
//!
//! let mut cfg = EncoderConfig::new(2, 128_000);
//! cfg.mode = BitrateMode::Vbr;
//! let mut enc = OpusEncoder::new(cfg).unwrap();
//! let pcm = vec![0.0f32; enc.frame_samples()]; // one 20 ms stereo frame
//! let packet = enc.encode(&pcm).unwrap();
//! assert_eq!(packet[0] >> 3, 31); // CELT-only, fullband, 20 ms
//! ```
//!
//! # Container
//!
//! RFC 7845 `OpusHead`/`OpusTags` headers are built by [`opus_head`] and [`opus_tags`]. With the
//! additional `ogg` feature, `ogg::OggOpusWriter` muxes the packets into an Ogg stream
//! (using the page writer of `symphonia-format-ogg`'s `writer` feature) with correct pre-skip
//! and granule positions.

mod analysis;
mod bands;
mod celt;
mod energy;
pub(crate) mod entenc;

#[cfg(feature = "ogg")]
pub mod ogg;

use std::fmt;

use self::celt::{CeltConfig, CeltEncoder, FRAME_SIZE};
use crate::packet::Bandwidth;

/// Samples per channel in one 20 ms Opus frame at 48 kHz.
pub const FRAME_SAMPLES_PER_CHANNEL: usize = FRAME_SIZE;

/// The decoder-side delay of the codec in samples at 48 kHz (the 2.5 ms MDCT overlap). The
/// RFC 7845 `pre-skip` of a stream produced by this encoder.
pub const PRE_SKIP: u16 = 120;

/// Lowest bitrate (bits per second) accepted by the encoder.
pub const MIN_BITRATE: u32 = 6_000;
/// Highest bitrate (bits per second) accepted by the encoder.
pub const MAX_BITRATE: u32 = 510_000;

/// How the encoder spends its bit budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BitrateMode {
    /// Every packet has exactly the same size (`bitrate / 400` bytes, rounded).
    Cbr,
    /// Unconstrained variable bitrate: the average approaches the target, individual packets
    /// may be much larger or smaller.
    #[default]
    Vbr,
    /// Variable bitrate with a bit reservoir that keeps the short-term rate close to the
    /// target (what libopus calls "constrained VBR").
    ConstrainedVbr,
}

/// Encoder configuration. Create with [`EncoderConfig::new`] and tweak the public fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    /// Number of channels: 1 or 2.
    pub channels: u8,
    /// Target bitrate in bits per second, including the one-byte TOC of every packet.
    pub bitrate: u32,
    /// Rate-control mode.
    pub mode: BitrateMode,
    /// Computational effort, 0 (fastest) to 10 (best quality). Default 9.
    pub complexity: u8,
    /// Coded audio bandwidth. `None` selects one from the per-channel bitrate.
    pub bandwidth: Option<Bandwidth>,
}

impl EncoderConfig {
    /// A configuration for `channels` channels at `bitrate` bits per second, with VBR, the
    /// default complexity and automatic bandwidth.
    pub fn new(channels: u8, bitrate: u32) -> Self {
        EncoderConfig {
            channels,
            bitrate,
            mode: BitrateMode::default(),
            complexity: 9,
            bandwidth: None,
        }
    }

    fn validate(&self) -> Result<(), EncoderError> {
        if self.channels != 1 && self.channels != 2 {
            return Err(EncoderError::InvalidConfig("channels must be 1 or 2"));
        }
        if !(MIN_BITRATE..=MAX_BITRATE).contains(&self.bitrate) {
            return Err(EncoderError::InvalidConfig("bitrate must be within 6000..=510000 bps"));
        }
        if self.complexity > 10 {
            return Err(EncoderError::InvalidConfig("complexity must be within 0..=10"));
        }
        if self.bandwidth == Some(Bandwidth::Mediumband) {
            return Err(EncoderError::InvalidConfig("CELT-only mode has no mediumband"));
        }
        Ok(())
    }

    /// The bandwidth that is actually used.
    fn effective_bandwidth(&self) -> Bandwidth {
        self.bandwidth.unwrap_or_else(|| {
            let per_channel = self.bitrate / self.channels as u32;
            if per_channel < 10_000 {
                Bandwidth::Narrowband
            }
            else if per_channel < 14_000 {
                Bandwidth::Wideband
            }
            else if per_channel < 20_000 {
                Bandwidth::Superwideband
            }
            else {
                Bandwidth::Fullband
            }
        })
    }
}

/// Errors reported by the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderError {
    /// The configuration is invalid; the message says why.
    InvalidConfig(&'static str),
    /// `encode` was given a number of samples that is not exactly one frame.
    InvalidFrameLength { expected: usize, got: usize },
}

impl fmt::Display for EncoderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncoderError::InvalidConfig(why) => write!(f, "invalid encoder configuration: {why}"),
            EncoderError::InvalidFrameLength { expected, got } => {
                write!(f, "invalid frame length: expected {expected} samples, got {got}")
            }
        }
    }
}

impl std::error::Error for EncoderError {}

/// Number of coded CELT bands for a bandwidth (the decoder derives the same from the TOC).
fn celt_end_band(bw: Bandwidth) -> i32 {
    match bw {
        Bandwidth::Narrowband => 13,
        Bandwidth::Mediumband | Bandwidth::Wideband => 17,
        Bandwidth::Superwideband => 19,
        Bandwidth::Fullband => 21,
    }
}

/// The TOC byte of a CELT-only, 20 ms, single-frame packet (RFC 6716 section 3.1).
fn toc_byte(bw: Bandwidth, stereo: bool) -> u8 {
    let config: u8 = match bw {
        Bandwidth::Narrowband => 16,
        Bandwidth::Mediumband | Bandwidth::Wideband => 20,
        Bandwidth::Superwideband => 24,
        Bandwidth::Fullband => 28,
    } + 3; // 20 ms
    (config << 3) | ((stereo as u8) << 2)
}

/// A CELT-only Opus encoder. See the [module documentation](self).
pub struct OpusEncoder {
    cfg: EncoderConfig,
    celt: CeltEncoder,
    toc: u8,
    /// Samples (per channel) accepted so far through [`OpusEncoder::push`].
    total_input: u64,
    /// Frames encoded so far (by any entry point).
    frames_encoded: u64,
    /// Partial frame buffered by [`OpusEncoder::push`].
    pending: Vec<f32>,
}

impl OpusEncoder {
    /// Creates an encoder. Fails if `cfg` is invalid.
    pub fn new(cfg: EncoderConfig) -> Result<Self, EncoderError> {
        cfg.validate()?;
        let bw = cfg.effective_bandwidth();
        let celt = CeltEncoder::new(Self::celt_config(&cfg, bw));
        Ok(OpusEncoder {
            cfg,
            celt,
            toc: toc_byte(bw, cfg.channels == 2),
            total_input: 0,
            frames_encoded: 0,
            pending: Vec::new(),
        })
    }

    fn celt_config(cfg: &EncoderConfig, bw: Bandwidth) -> CeltConfig {
        CeltConfig {
            channels: cfg.channels as usize,
            bitrate: cfg.bitrate as i32,
            vbr: cfg.mode != BitrateMode::Cbr,
            constrained_vbr: cfg.mode != BitrateMode::Vbr,
            complexity: cfg.complexity as i32,
            end: celt_end_band(bw),
        }
    }

    /// The configuration in effect.
    pub fn config(&self) -> &EncoderConfig {
        &self.cfg
    }

    /// Interleaved samples (`channels * 960`) consumed by one [`Self::encode`] call.
    pub fn frame_samples(&self) -> usize {
        FRAME_SIZE * self.cfg.channels as usize
    }

    /// The RFC 7845 pre-skip (in 48 kHz samples) to put in the `OpusHead` of a stream produced
    /// by this encoder: the number of decoded samples at the start that are encoder delay.
    pub fn pre_skip(&self) -> u16 {
        PRE_SKIP
    }

    /// The final range-coder state of the last encoded packet. A conforming decoder ends its
    /// decode of the same packet with an identical value (for this crate's decoder:
    /// [`crate::OpusDecoder::final_range`]), which makes a cheap end-to-end consistency check.
    pub fn final_range(&self) -> u32 {
        self.celt.final_range()
    }

    /// Changes the target bitrate between frames. The bandwidth is re-derived if it was left on
    /// automatic (the TOC of the following packets changes accordingly, which is legal in Opus).
    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<(), EncoderError> {
        self.reconfigure(EncoderConfig { bitrate, ..self.cfg })
    }

    /// Changes the complexity (0..=10) between frames.
    pub fn set_complexity(&mut self, complexity: u8) -> Result<(), EncoderError> {
        self.reconfigure(EncoderConfig { complexity, ..self.cfg })
    }

    /// Changes the rate-control mode between frames.
    pub fn set_mode(&mut self, mode: BitrateMode) -> Result<(), EncoderError> {
        self.reconfigure(EncoderConfig { mode, ..self.cfg })
    }

    fn reconfigure(&mut self, cfg: EncoderConfig) -> Result<(), EncoderError> {
        cfg.validate()?;
        let bw = cfg.effective_bandwidth();
        *self.celt.config_mut() = Self::celt_config(&cfg, bw);
        self.toc = toc_byte(bw, cfg.channels == 2);
        self.cfg = cfg;
        Ok(())
    }

    /// Encodes exactly one 20 ms frame (`channels * 960` interleaved samples) into one Opus
    /// packet. In CBR mode every packet has the same length.
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u8>, EncoderError> {
        let expected = self.frame_samples();
        if pcm.len() != expected {
            return Err(EncoderError::InvalidFrameLength { expected, got: pcm.len() });
        }
        // Total packet size in CBR: bitrate/400 bytes (20 ms), rounded, minus the TOC byte.
        let cbr_bytes = ((self.cfg.bitrate as usize + 200) / 400).saturating_sub(1).max(2);
        let payload = self.celt.encode_frame(pcm, cbr_bytes);
        self.frames_encoded += 1;
        let mut packet = Vec::with_capacity(payload.len() + 1);
        packet.push(self.toc);
        packet.extend_from_slice(&payload);
        Ok(packet)
    }

    /// Streaming interface: accepts any number of interleaved samples and returns the packets
    /// that are complete; the remainder is buffered for the next call. Use together with
    /// [`Self::finish`] (do not mix with direct [`Self::encode`] calls on the same stream).
    pub fn push(&mut self, pcm: &[f32]) -> Vec<Vec<u8>> {
        let ch = self.cfg.channels as usize;
        let n = pcm.len() / ch * ch;
        self.total_input += (n / ch) as u64;
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(&pcm[..n]);
        let frame = self.frame_samples();
        let mut out = Vec::with_capacity(buf.len() / frame);
        let mut chunks = buf.chunks_exact(frame);
        for chunk in &mut chunks {
            out.push(self.encode(chunk).expect("frame length is exact"));
        }
        self.pending = chunks.remainder().to_vec();
        out
    }

    /// Ends a stream started with [`Self::push`]: zero-pads and encodes the buffered partial
    /// frame, then enough extra silence for the decoder to output every input sample after
    /// the pre-skip. Together with [`PRE_SKIP`] the stream then decodes to exactly
    /// [`Self::samples_pushed`] samples per channel.
    pub fn finish(&mut self) -> Vec<Vec<u8>> {
        let frame = self.frame_samples();
        let mut out = Vec::new();
        let needed = self.total_input + PRE_SKIP as u64;
        if !self.pending.is_empty() {
            let mut buf = std::mem::take(&mut self.pending);
            buf.resize(frame, 0.0);
            out.push(self.encode(&buf).expect("frame length is exact"));
        }
        let silence = vec![0.0f32; frame];
        while self.frames_encoded * (FRAME_SIZE as u64) < needed {
            out.push(self.encode(&silence).expect("frame length is exact"));
        }
        out
    }

    /// Total samples per channel accepted by [`Self::push`] so far.
    pub fn samples_pushed(&self) -> u64 {
        self.total_input
    }
}

/// Builds the RFC 7845 section 5.1 `OpusHead` identification header for a channel-mapping
/// family 0 stream (mono or stereo).
///
/// `pre_skip` is in 48 kHz samples (see [`PRE_SKIP`]), `input_sample_rate` the rate of the
/// original source (informational) and `output_gain_q8` the Q7.8 dB gain to apply on playback.
pub fn opus_head(
    channels: u8,
    pre_skip: u16,
    input_sample_rate: u32,
    output_gain_q8: i16,
) -> Vec<u8> {
    let mut v = Vec::with_capacity(19);
    v.extend_from_slice(b"OpusHead");
    v.push(1); // version
    v.push(channels);
    v.extend_from_slice(&pre_skip.to_le_bytes());
    v.extend_from_slice(&input_sample_rate.to_le_bytes());
    v.extend_from_slice(&output_gain_q8.to_le_bytes());
    v.push(0); // channel mapping family 0
    v
}

/// Builds the RFC 7845 section 5.2 `OpusTags` comment header: a vendor string followed by
/// `NAME=value` user comments (names are case-insensitive ASCII; they are written as given).
pub fn opus_tags(vendor: &str, comments: &[(&str, &str)]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"OpusTags");
    v.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    v.extend_from_slice(vendor.as_bytes());
    v.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for (name, value) in comments {
        let len = name.len() + 1 + value.len();
        v.extend_from_slice(&(len as u32).to_le_bytes());
        v.extend_from_slice(name.as_bytes());
        v.push(b'=');
        v.extend_from_slice(value.as_bytes());
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::OpusHead;
    use crate::packet::{self, OpusMode, Toc};

    #[test]
    fn toc_bytes_match_rfc6716() {
        assert_eq!(toc_byte(Bandwidth::Fullband, false), 0xF8);
        assert_eq!(toc_byte(Bandwidth::Fullband, true), 0xFC);
        assert_eq!(toc_byte(Bandwidth::Superwideband, false), 0xD8);
        assert_eq!(toc_byte(Bandwidth::Wideband, true), 0xBC);
        assert_eq!(toc_byte(Bandwidth::Narrowband, false), 0x98);
        for bw in [
            Bandwidth::Narrowband,
            Bandwidth::Wideband,
            Bandwidth::Superwideband,
            Bandwidth::Fullband,
        ] {
            let toc = Toc::new(toc_byte(bw, true));
            assert_eq!(toc.mode(), OpusMode::CeltOnly);
            assert_eq!(toc.bandwidth(), bw);
            assert!(toc.stereo());
            assert_eq!(toc.frame_code(), 0);
            assert_eq!(toc.samples_per_frame(48000), 960);
        }
    }

    #[test]
    fn head_round_trips_through_parser() {
        let bytes = opus_head(2, PRE_SKIP, 44100, -256);
        assert_eq!(bytes.len(), 19);
        let head = OpusHead::parse(&bytes).unwrap();
        assert_eq!(head.channel_count, 2);
        assert_eq!(head.pre_skip, PRE_SKIP);
        assert_eq!(head.input_sample_rate, 44100);
        assert_eq!(head.output_gain, -256);
        assert_eq!(head.mapping.family, 0);
    }

    #[test]
    fn tags_layout() {
        let t = opus_tags("symphonia", &[("TITLE", "x"), ("ARTIST", "yz")]);
        assert_eq!(&t[..8], b"OpusTags");
        assert_eq!(u32::from_le_bytes(t[8..12].try_into().unwrap()), 9);
        assert_eq!(&t[12..21], b"symphonia");
        assert_eq!(u32::from_le_bytes(t[21..25].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(t[25..29].try_into().unwrap()), 7);
        assert_eq!(&t[29..36], b"TITLE=x");
        assert_eq!(u32::from_le_bytes(t[36..40].try_into().unwrap()), 9);
        assert_eq!(&t[40..49], b"ARTIST=yz");
        assert_eq!(t.len(), 49);
    }

    #[test]
    fn config_validation() {
        assert!(OpusEncoder::new(EncoderConfig::new(3, 64000)).is_err());
        assert!(OpusEncoder::new(EncoderConfig::new(2, 100)).is_err());
        assert!(OpusEncoder::new(EncoderConfig::new(2, 1_000_000)).is_err());
        let mut c = EncoderConfig::new(2, 64000);
        c.complexity = 11;
        assert!(OpusEncoder::new(c).is_err());
        let mut c = EncoderConfig::new(2, 64000);
        c.bandwidth = Some(Bandwidth::Mediumband);
        assert!(OpusEncoder::new(c).is_err());
        let mut enc = OpusEncoder::new(EncoderConfig::new(1, 64000)).unwrap();
        assert_eq!(
            enc.encode(&[0.0; 10]),
            Err(EncoderError::InvalidFrameLength { expected: 960, got: 10 })
        );
        assert!(enc.set_bitrate(1).is_err());
    }

    #[test]
    fn auto_bandwidth_follows_bitrate() {
        let bw = |ch, br| EncoderConfig::new(ch, br).effective_bandwidth();
        assert_eq!(bw(2, 64000), Bandwidth::Fullband);
        assert_eq!(bw(2, 32000), Bandwidth::Superwideband);
        assert_eq!(bw(2, 24000), Bandwidth::Wideband);
        assert_eq!(bw(1, 8000), Bandwidth::Narrowband);
        assert_eq!(bw(1, 32000), Bandwidth::Fullband);
    }

    #[test]
    fn packets_parse_as_single_frame_celt() {
        let mut enc = OpusEncoder::new(EncoderConfig::new(2, 96000)).unwrap();
        let pcm: Vec<f32> = (0..1920).map(|i| ((i / 2) as f32 * 0.05).sin() * 0.3).collect();
        let p = enc.encode(&pcm).unwrap();
        let parsed = packet::parse(&p).unwrap();
        assert_eq!(parsed.frames.len(), 1);
        assert_eq!(parsed.toc.mode(), OpusMode::CeltOnly);
        assert_eq!(parsed.frames[0].len, p.len() - 1);
    }
}
