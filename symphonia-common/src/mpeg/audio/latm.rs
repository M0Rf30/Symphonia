// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Support for AAC in LATM (ISO/IEC 14496-3 §1.7.3): the `StreamMuxConfig()` and
//! `AudioMuxElement()` syntax, shared by the LOAS (`AudioSyncStream()`) and MPEG-TS demuxers.

use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::io::{BitReaderLtr, FiniteBitStream, ReadBitsLtr};

use super::AudioSpecificConfig;

/// The parts of a LATM `StreamMuxConfig()` that are required to demultiplex a single program and
/// layer.
#[derive(Clone, Debug)]
pub struct StreamMuxConfig {
    /// The audio specific config of the stream.
    pub asc: AudioSpecificConfig,
    /// The audio specific config as bytes, for the decoder.
    pub extra_data: Box<[u8]>,
    /// The number of sub-frames (payloads) in each audio mux element, minus 1.
    pub num_sub_frames: usize,
    /// `frameLengthType` of the stream.
    pub frame_length_type: u8,
    /// `frameLength` of the stream, if `frame_length_type` is 1.
    pub frame_length: usize,
}

impl StreamMuxConfig {
    /// Read a `StreamMuxConfig()` (ISO/IEC 14496-3 §1.7.3.1). Only streams of a single program
    /// and layer are supported.
    pub fn read<B: ReadBitsLtr + FiniteBitStream>(bs: &mut B, data: &[u8]) -> Result<Self> {
        let audio_mux_version = bs.read_bool()?;

        // audioMuxVersionA is reserved for future use.
        if audio_mux_version && bs.read_bool()? {
            return unsupported_error("loas: unsupported audio mux version");
        }

        if audio_mux_version {
            // taraBufferFullness
            let _ = Self::read_latm_value(bs)?;
        }

        // allStreamsSameTimeFraming
        let _ = bs.read_bool()?;
        let num_sub_frames = bs.read_bits_leq32(6)? as usize;
        let num_program = bs.read_bits_leq32(4)? + 1;
        let num_layer = bs.read_bits_leq32(3)? + 1;

        if num_program != 1 || num_layer != 1 {
            return unsupported_error("loas: only a single program and layer is supported");
        }

        // The first (and only) program and layer always has its own audio specific config.
        let asc_len = if audio_mux_version { Some(Self::read_latm_value(bs)?) } else { None };

        let asc_start = (data.len() * 8) as u64 - bs.bits_left();
        let mut asc = AudioSpecificConfig::read_core_from(bs)?;

        // The audio specific config is not the last element of the stream mux config, so look for
        // its optional extension without consuming the bits after it.
        let mut peek = BitReaderLtr::new(data);
        peek.ignore_bits(((data.len() * 8) as u64 - bs.bits_left()) as u32)?;
        let ext_bits = asc.read_sync_extension(&mut peek)?;
        bs.ignore_bits(ext_bits as u32)?;

        let asc_end = (data.len() * 8) as u64 - bs.bits_left();

        let mut asc_bits = asc_end - asc_start;

        if let Some(asc_len) = asc_len {
            // The audio specific config is followed by fill bits up to its signalled length.
            let asc_len = u64::from(asc_len);

            if asc_len < asc_bits {
                return decode_error("loas: invalid audio specific config length");
            }

            bs.ignore_bits((asc_len - asc_bits) as u32)?;
            asc_bits = asc_len;
        }

        let extra_data = copy_bits(data, asc_start as usize, asc_bits as usize);

        let frame_length_type = bs.read_bits_leq32(3)? as u8;
        let mut frame_length = 0;

        match frame_length_type {
            0 => {
                // latmBufferFullness
                let _ = bs.read_bits_leq32(8)?;
            }
            1 => {
                // The payload length is 20 bytes plus the frame length.
                frame_length = bs.read_bits_leq32(9)? as usize + 20;
            }
            _ => return unsupported_error("loas: unsupported frame length type"),
        }

        // otherDataPresent
        if bs.read_bool()? {
            if audio_mux_version {
                let _ = Self::read_latm_value(bs)?;
            }
            else {
                loop {
                    let escape = bs.read_bool()?;
                    let _ = bs.read_bits_leq32(8)?;

                    if !escape {
                        break;
                    }
                }
            }
        }

        // crcCheckPresent
        if bs.read_bool()? {
            let _crc = bs.read_bits_leq32(8)?;
        }

        // Validate the audio specific config is something that can be decoded as a LATM stream.
        if asc.channels.is_none() {
            return decode_error("loas: missing channel configuration");
        }

        Ok(StreamMuxConfig { asc, extra_data, num_sub_frames, frame_length_type, frame_length })
    }

    /// Read a `LatmGetValue()`.
    fn read_latm_value<B: ReadBitsLtr>(bs: &mut B) -> Result<u32> {
        let num_bytes = bs.read_bits_leq32(2)? + 1;
        let mut value = 0u32;

        for _ in 0..num_bytes {
            value = (value << 8) | bs.read_bits_leq32(8)?;
        }

        Ok(value)
    }
}

/// Copy `n_bits` bits from `data` starting at the bit offset `bit_offset` into a new
/// byte-aligned buffer.
pub fn copy_bits(data: &[u8], bit_offset: usize, n_bits: usize) -> Box<[u8]> {
    let mut out = vec![0u8; n_bits.div_ceil(8)];

    for i in 0..n_bits {
        let src = bit_offset + i;
        let bit = (data[src / 8] >> (7 - src % 8)) & 1;
        out[i / 8] |= bit << (7 - i % 8);
    }

    out.into_boxed_slice()
}

/// Read an `AudioMuxElement(1)` (ISO/IEC 14496-3 §1.7.3.2) from the body of a LOAS frame. If the
/// element carries a stream mux config, it replaces `config`. Returns the raw data block of every
/// sub-frame in the element.
pub fn read_audio_mux_element(
    data: &[u8],
    config: &mut Option<StreamMuxConfig>,
) -> Result<Vec<Box<[u8]>>> {
    let mut bs = BitReaderLtr::new(data);

    let use_same_stream_mux = bs.read_bool()?;

    if !use_same_stream_mux {
        *config = Some(StreamMuxConfig::read(&mut bs, data)?);
    }

    // A stream mux config must have been received.
    let Some(config) = config.as_ref()
    else {
        return Ok(vec![]);
    };

    let mut payloads = Vec::with_capacity(config.num_sub_frames + 1);

    for _ in 0..=config.num_sub_frames {
        // PayloadLengthInfo()
        let len = match config.frame_length_type {
            0 => {
                let mut len = 0usize;

                loop {
                    let tmp = bs.read_bits_leq32(8)?;
                    len += tmp as usize;

                    if tmp != 255 {
                        break;
                    }
                }

                len
            }
            _ => config.frame_length,
        };

        // PayloadMux(): the raw data block is not necessarily byte-aligned in the mux element.
        if bs.bits_left() < (len as u64) * 8 {
            return decode_error("loas: payload exceeds the audio mux element");
        }

        let mut payload = vec![0u8; len];

        for byte in payload.iter_mut() {
            *byte = bs.read_bits_leq32(8)? as u8;
        }

        payloads.push(payload.into_boxed_slice());
    }

    Ok(payloads)
}
