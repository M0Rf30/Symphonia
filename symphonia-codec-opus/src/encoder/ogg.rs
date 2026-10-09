// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Ogg Opus muxing (RFC 7845), behind the `ogg` cargo feature.
//!
//! [`OggOpusWriter`] couples [`OpusEncoder`] with the page writer of `symphonia-format-ogg`
//! (feature `writer`, which is where the generic Ogg page code and its CRC live). It writes the
//! `OpusHead` page, the `OpusTags` page, then the audio packets, with the granule positions,
//! pre-skip and end-trimming RFC 7845 requires, so that a decoder outputs exactly the samples
//! that were written.

use std::io::{self, Write};

use symphonia_format_ogg::writer::OggPacketWriter;

use super::{
    EncoderConfig, FRAME_SAMPLES_PER_CHANNEL, OpusEncoder, PRE_SKIP, opus_head, opus_tags,
};

/// Vendor string written to the `OpusTags` header.
pub const VENDOR: &str = concat!("symphonia-codec-opus ", env!("CARGO_PKG_VERSION"));

/// Streams interleaved 48 kHz `f32` PCM into an Ogg Opus file/stream.
pub struct OggOpusWriter<W: Write> {
    ogg: OggPacketWriter<W>,
    enc: OpusEncoder,
    /// Number of audio packets already handed to the Ogg writer.
    packets_written: u64,
    /// The most recent packet, held back until we know whether it is the last one (the last
    /// packet carries the end-of-stream flag and the end-trimming granule position).
    held: Option<Vec<u8>>,
}

impl<W: Write> OggOpusWriter<W> {
    /// Starts a stream: encodes with `cfg`, tags it with `comments` (`NAME=value` pairs) and
    /// writes the two header pages immediately. `input_sample_rate` is the rate of the original
    /// source (informational, stored in `OpusHead`; the encoder itself always takes 48 kHz).
    pub fn new(
        sink: W,
        serial: u32,
        cfg: EncoderConfig,
        input_sample_rate: u32,
        comments: &[(&str, &str)],
    ) -> io::Result<Self> {
        let enc =
            OpusEncoder::new(cfg).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let mut ogg = OggPacketWriter::new(sink, serial);
        // The identification header is alone on the first page, the comment header starts the
        // next one; audio starts on a fresh page (RFC 7845 section 3).
        ogg.write_packet(&opus_head(cfg.channels, PRE_SKIP, input_sample_rate, 0), 0, false)?;
        ogg.flush_page()?;
        ogg.write_packet(&opus_tags(VENDOR, comments), 0, false)?;
        ogg.flush_page()?;
        Ok(OggOpusWriter { ogg, enc, packets_written: 0, held: None })
    }

    /// Sets the body size at which an audio page is emitted (default 4096 bytes).
    pub fn set_page_target(&mut self, bytes: usize) {
        self.ogg.set_page_target(bytes);
    }

    /// The underlying encoder (e.g. to change the bitrate mid-stream).
    pub fn encoder_mut(&mut self) -> &mut OpusEncoder {
        &mut self.enc
    }

    /// Encodes and writes interleaved samples (`channels` per frame, nominal range +-1.0).
    pub fn write_samples(&mut self, pcm: &[f32]) -> io::Result<()> {
        let packets = self.enc.push(pcm);
        for p in packets {
            self.push_packet(p)?;
        }
        Ok(())
    }

    /// Finishes the stream: encodes the buffered tail, writes the last page with the
    /// end-of-stream flag and the granule position that trims the decoder output to exactly
    /// the number of samples written, and returns the sink.
    pub fn finish(mut self) -> io::Result<W> {
        let tail = self.enc.finish();
        for p in tail {
            self.push_packet(p)?;
        }
        let total = PRE_SKIP as u64 + self.enc.samples_pushed();
        let last = self.held.take().expect("finish() always emits at least one packet");
        // RFC 7845 section 4: the granule position of the last page may be smaller than the
        // sum of the packet durations, which tells the decoder to trim the end.
        debug_assert!(total > (self.packets_written) * FRAME_SAMPLES_PER_CHANNEL as u64);
        debug_assert!(total <= (self.packets_written + 1) * FRAME_SAMPLES_PER_CHANNEL as u64);
        self.ogg.write_packet(&last, total as i64, true)?;
        self.ogg.finish()
    }

    fn push_packet(&mut self, packet: Vec<u8>) -> io::Result<()> {
        if let Some(prev) = self.held.replace(packet) {
            self.packets_written += 1;
            let granule = self.packets_written * FRAME_SAMPLES_PER_CHANNEL as u64;
            self.ogg.write_packet(&prev, granule as i64, false)?;
        }
        Ok(())
    }
}
