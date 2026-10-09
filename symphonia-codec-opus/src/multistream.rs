// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The multistream Opus decoder (RFC 7845 section 5.2 / libopus `src/opus_multistream_decoder.c`,
//! `struct OpusMSDecoder`, `opus_multistream_decode_native`), including the projection decoder
//! of mapping family 3 (RFC 8486, libopus `src/opus_projection_decoder.c`).
//! Ported from libopus (BSD-3-Clause), see NOTICE.
//!
//! A multistream packet contains `stream_count` self-delimited Opus packets back-to-back (the
//! last one NOT self-delimited, per `crate::packet::parse_impl`'s `self_delimited` flag), decoded
//! by `stream_count` independent [`crate::decoder::OpusDecoder`] instances (`coupled_count` of
//! them stereo, the rest mono), then the decoded channels are scattered into the output according
//! to [`crate::mapping::ChannelMapping::table`] (`255` entries produce silence). For mapping
//! family 3 the decoded channels (`stream_count + coupled_count` of them, in stream order) are
//! instead multiplied with the demixing matrix to produce the ambisonic output channels.

use crate::decoder::{DecodeError, OpusDecoder, Result, SampleRate};
use crate::mapping::ChannelMapping;

/// C: `get_left_channel`. Returns the next output channel index (after `prev`) whose mapping
/// table entry addresses the left channel of coupled stream `stream_id` (`stream_id*2`).
fn get_left_channel(table: &[u8], stream_id: u8, prev: i32) -> Option<usize> {
    let start = if prev < 0 { 0 } else { prev as usize + 1 };
    (start..table.len()).find(|&i| table[i] == stream_id.wrapping_mul(2))
}

/// C: `get_right_channel` (`stream_id*2+1`).
fn get_right_channel(table: &[u8], stream_id: u8, prev: i32) -> Option<usize> {
    let start = if prev < 0 { 0 } else { prev as usize + 1 };
    (start..table.len()).find(|&i| table[i] == stream_id.wrapping_mul(2) + 1)
}

/// C: `get_mono_channel` (`stream_id + nb_coupled_streams`).
fn get_mono_channel(table: &[u8], stream_id: u8, coupled_count: u8, prev: i32) -> Option<usize> {
    let start = if prev < 0 { 0 } else { prev as usize + 1 };
    (start..table.len()).find(|&i| table[i] == stream_id + coupled_count)
}

/// C: `copy_channel_out` (the float/`opus_val16` specialization `opus_copy_channel_out_float`).
fn scatter(
    out: &mut [f32],
    total_ch: usize,
    chan: usize,
    buf: &[f32],
    buf_stride: usize,
    buf_offset: usize,
    frame_size: usize,
) {
    for i in 0..frame_size {
        out[chan + i * total_ch] = buf[buf_offset + i * buf_stride];
    }
}

/// C: `mapping_matrix_multiply_channel_out_float` applied to every decoded channel in turn (the
/// projection decoder's `opus_projection_copy_channel_out_float`). `mix` holds `frame_size` frames
/// of `cols` interleaved decoded channels, `matrix` is column-major with `rows` rows, and `out`
/// receives `frame_size` frames of `rows` interleaved channels. Only the first `used_cols`
/// decoded channels contribute (libopus' identity mapping covers just `i < rows`). The
/// accumulation order (ascending decoded channel, each term `((1/32768) * m) * x`, no fused
/// multiply-add) reproduces libopus' float result exactly.
fn demix(
    matrix: &[i16],
    rows: usize,
    cols: usize,
    used_cols: usize,
    mix: &[f32],
    out: &mut [f32],
    frame_size: usize,
) {
    for i in 0..frame_size {
        let src = &mix[i * cols..(i + 1) * cols];
        let dst = &mut out[i * rows..(i + 1) * rows];
        for (row, d) in dst.iter_mut().enumerate() {
            let mut acc = 0.0f32;
            for (col, &x) in src.iter().enumerate().take(used_cols) {
                acc += ((1.0 / 32768.0) * f32::from(matrix[rows * col + row])) * x;
            }
            *d = acc;
        }
    }
}

/// C: `struct OpusMSDecoder` (conceptually; the C version packs decoder state inline in one
/// allocation, which a `Vec<OpusDecoder>` supersedes here).
pub struct MultistreamDecoder {
    mapping: ChannelMapping,
    layout_channels: u8,
    sample_rate: SampleRate,
    decoders: Vec<OpusDecoder>,
    /// The effective per-channel mapping table used for scattering: identical to
    /// `mapping.table` except for mapping family 0 (mono/stereo), where the header carries no
    /// explicit table (`RFC 7845` implicit mapping) and this field holds the equivalent
    /// identity table (`[0]` or `[0, 1]`) so [`Self::decode`] never needs to special-case it.
    effective_table: Vec<u8>,
}

impl MultistreamDecoder {
    /// C: `opus_multistream_decoder_init`.
    pub fn try_new(
        sample_rate: SampleRate,
        layout_channels: u8,
        mapping: ChannelMapping,
    ) -> Result<Self> {
        if mapping.stream_count == 0 {
            return Err(DecodeError::InvalidChannelCount);
        }
        let max_channel = mapping.stream_count as u16 + mapping.coupled_count as u16;
        if max_channel > 255 {
            return Err(DecodeError::InvalidChannelCount);
        }
        let projection = mapping.family == 3;
        let effective_table = if projection {
            // Mapping family 3: libopus initializes the multistream decoder with the trivial
            // mapping `mapping[i] = i` so that each decoded channel pairs with a matrix column.
            // The decoded channels are collected in a `stream_count + coupled_count` wide buffer
            // (identity table) and mixed down to `layout_channels` afterwards.
            let n_in = mapping.stream_count as usize + mapping.coupled_count as usize;
            if layout_channels as usize > n_in
                || mapping.demixing_matrix.len() != layout_channels as usize * n_in
            {
                return Err(DecodeError::InvalidChannelCount);
            }
            (0..n_in as u8).collect()
        }
        else if mapping.table.is_empty() {
            // Family 0: implicit identity mapping (C: `opus_decoder.c` uses a plain
            // `OpusDecoder` directly for family 0, never `OpusMSDecoder`; this crate always
            // routes through `MultistreamDecoder` for uniformity, so synthesize the equivalent
            // table here instead).
            match layout_channels {
                1 => vec![0u8],
                2 => vec![0u8, 1u8],
                _ => return Err(DecodeError::InvalidChannelCount),
            }
        }
        else {
            for &idx in &mapping.table {
                if idx != 255 && (idx as u16) >= max_channel {
                    return Err(DecodeError::InvalidChannelCount);
                }
            }
            mapping.table.clone()
        };

        let mut decoders = Vec::with_capacity(mapping.stream_count as usize);
        for i in 0..mapping.stream_count {
            let ch = if i < mapping.coupled_count { 2 } else { 1 };
            decoders.push(OpusDecoder::try_new(sample_rate, ch)?);
        }
        Ok(MultistreamDecoder { mapping, layout_channels, sample_rate, decoders, effective_table })
    }

    pub fn stream_count(&self) -> usize {
        self.decoders.len()
    }

    pub fn layout_channels(&self) -> u8 {
        self.layout_channels
    }

    /// C: `OPUS_RESET_STATE` applied to every sub-decoder.
    pub fn reset(&mut self) {
        for dec in &mut self.decoders {
            dec.reset();
        }
    }

    /// C: `OPUS_SET_GAIN`, applied to every sub-decoder (RFC 7845 `OpusHead.output_gain`).
    pub fn set_gain(&mut self, gain: i16) {
        for dec in &mut self.decoders {
            dec.set_gain(gain);
        }
    }

    /// C: `opus_multistream_packet_validate`. Verifies every per-stream sub-packet parses and
    /// that all streams agree on the decoded sample count; returns that sample count.
    fn packet_validate(&self, mut data: &[u8], fs: u32) -> Result<usize> {
        let nb_streams = self.decoders.len();
        let mut samples = 0usize;
        for s in 0..nb_streams {
            if data.is_empty() {
                return Err(DecodeError::Packet(crate::packet::PacketError::InvalidPacket));
            }
            let parsed = crate::packet::parse_impl(data, s != nb_streams - 1)?;
            let tmp_samples = crate::packet::get_nb_samples(data, fs)? as usize;
            if s != 0 && samples != tmp_samples {
                return Err(DecodeError::Packet(crate::packet::PacketError::InvalidPacket));
            }
            samples = tmp_samples;
            data = &data[parsed.packet_len..];
        }
        Ok(samples)
    }

    /// C: `opus_multistream_decode_native`. Splits `data` into per-stream self-delimited packets
    /// via `crate::packet::parse_impl(..., self_delimited = true)` (all but the last stream) /
    /// `false` (the last), decodes each with its own [`OpusDecoder`], and scatters the decoded
    /// channels into `out` per `self.mapping.table`.
    pub fn decode(
        &mut self,
        data: Option<&[u8]>,
        out: &mut [f32],
        frame_size: usize,
        decode_fec: bool,
    ) -> Result<usize> {
        if frame_size == 0 {
            return Err(DecodeError::BadArgument);
        }
        let fs = self.sample_rate.as_hz();
        let mut frame_size = frame_size.min((fs as usize / 25) * 3);
        // Defensive: `scatter` indexes `out` at `chan + i*total_ch` up to `frame_size`; check the
        // caller-supplied buffer is large enough up front so a caller sizing mistake (or a bogus
        // `frame_size` echoed back from a malformed packet on the PLC path) turns into a clean
        // `BufferTooSmall` rather than an out-of-bounds-index panic inside `scatter`.
        match frame_size.checked_mul(self.layout_channels as usize) {
            Some(need) if out.len() >= need => {}
            _ => return Err(DecodeError::BufferTooSmall),
        }
        let mut buf = vec![0f32; 2 * frame_size];

        // Mapping family 3 (projection): decode into a `stream_count + coupled_count` channel
        // intermediate buffer, demixed into `out` at the end.
        let projection = self.mapping.family == 3;
        let n_in = self.mapping.stream_count as usize + self.mapping.coupled_count as usize;
        let mut mix = if projection { vec![0f32; frame_size * n_in] } else { Vec::new() };

        let do_plc = data.is_none();
        if !do_plc {
            let d = data.unwrap();
            if d.len() < 2 * self.decoders.len() - 1 {
                return Err(DecodeError::Packet(crate::packet::PacketError::InvalidPacket));
            }
            let validated = self.packet_validate(d, fs)?;
            if validated > frame_size {
                return Err(DecodeError::BufferTooSmall);
            }
        }

        let mut data_ptr = data;
        let nb_streams = self.decoders.len();
        for s in 0..nb_streams {
            let is_coupled = s < self.mapping.coupled_count as usize;
            let self_delimited = s != nb_streams - 1;

            if !do_plc && data_ptr.map_or(true, |d| d.is_empty()) {
                return Err(DecodeError::Internal);
            }

            let (n, packet_offset) = self.decoders[s].decode_native(
                if do_plc { None } else { data_ptr },
                &mut buf,
                frame_size,
                decode_fec,
                self_delimited,
            )?;
            if !do_plc {
                data_ptr = Some(&data_ptr.unwrap()[packet_offset..]);
            }
            if n == 0 {
                return Err(DecodeError::Internal);
            }
            frame_size = n;

            let total_ch = if projection { n_in } else { self.layout_channels as usize };
            if is_coupled {
                let mut prev = -1i32;
                while let Some(chan) = get_left_channel(&self.effective_table, s as u8, prev) {
                    scatter(
                        if projection { &mut mix[..] } else { &mut out[..] },
                        total_ch,
                        chan,
                        &buf,
                        2,
                        0,
                        frame_size,
                    );
                    prev = chan as i32;
                }
                prev = -1;
                while let Some(chan) = get_right_channel(&self.effective_table, s as u8, prev) {
                    scatter(
                        if projection { &mut mix[..] } else { &mut out[..] },
                        total_ch,
                        chan,
                        &buf,
                        2,
                        1,
                        frame_size,
                    );
                    prev = chan as i32;
                }
            }
            else {
                let mut prev = -1i32;
                while let Some(chan) = get_mono_channel(
                    &self.effective_table,
                    s as u8,
                    self.mapping.coupled_count,
                    prev,
                ) {
                    scatter(
                        if projection { &mut mix[..] } else { &mut out[..] },
                        total_ch,
                        chan,
                        &buf,
                        1,
                        0,
                        frame_size,
                    );
                    prev = chan as i32;
                }
            }
        }

        if projection {
            let rows = self.layout_channels as usize;
            demix(&self.mapping.demixing_matrix, rows, n_in, rows.min(n_in), &mix, out, frame_size);
            return Ok(frame_size);
        }

        // Handle muted channels (mapping index 255).
        let total_ch = self.layout_channels as usize;
        for (c, &idx) in self.effective_table.iter().enumerate() {
            if idx == 255 {
                for i in 0..frame_size {
                    out[c + i * total_ch] = 0.0;
                }
            }
        }

        Ok(frame_size)
    }
}
