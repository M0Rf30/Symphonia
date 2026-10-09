// Vendored from ape-decoder 0.3.2 (https://github.com/OMBS-IO/ape-decoder, commit c7141a8).
// Copyright (c) 2026 ombs.io. Licensed under MIT OR Apache-2.0; see LICENSE-MIT, LICENSE-APACHE
// and NOTICE in this directory. Modified for Symphonia.

use crate::mac::bitreader::BitReader;
use crate::mac::crc::ape_crc;
use crate::mac::entropy::EntropyState;
use crate::mac::error::{ApeError, ApeResult};
use crate::mac::predictor::{Predictor3950, Predictor3950_32};
use crate::mac::range_coder::RangeCoder;
use crate::mac::unprepare;

// Special frame codes (from Prepare.h)
const SPECIAL_FRAME_MONO_SILENCE: i32 = 1;
const SPECIAL_FRAME_LEFT_SILENCE: i32 = 1;
const SPECIAL_FRAME_RIGHT_SILENCE: i32 = 2;
const SPECIAL_FRAME_PSEUDO_STEREO: i32 = 4;

enum Predictors {
    Path16(Vec<Predictor3950>),
    Path32(Vec<Predictor3950_32>),
}

// ---------------------------------------------------------------------------
// Frame decode implementations (shared between owned and borrowed paths)
// ---------------------------------------------------------------------------

fn try_decode_frame_16(
    frame_data: &[u8],
    seek_remainder: u32,
    frame_blocks: usize,
    version: i32,
    channels: u16,
    bits: u16,
    block_align: usize,
    predictors: &mut [Predictor3950],
    entropy_states: &mut [EntropyState],
    range_coder: &mut RangeCoder,
) -> ApeResult<Vec<u8>> {
    let mut br = BitReader::from_frame_bytes(frame_data, seek_remainder * 8);

    // --- StartFrame ---
    let mut stored_crc = br.decode_value_x_bits(32);
    let mut special_codes: i32 = 0;
    if version > 3820 {
        if stored_crc & 0x80000000 != 0 {
            special_codes = br.decode_value_x_bits(32) as i32;
        }
        stored_crc &= 0x7FFFFFFF;
    }

    for p in predictors.iter_mut() {
        p.flush();
    }
    for s in entropy_states.iter_mut() {
        s.flush();
    }
    range_coder.flush_bit_array(&mut br);

    let mut last_x: i32 = 0;
    let pcm_size =
        frame_blocks.checked_mul(block_align).ok_or(ApeError::InvalidFormat("frame too large"))?;
    if pcm_size > 64 * 1024 * 1024 {
        return Err(ApeError::InvalidFormat("frame PCM size exceeds 64 MB"));
    }
    let mut pcm_output = Vec::with_capacity(pcm_size);

    let decode_result: ApeResult<()> = (|| {
        if channels == 2 {
            if (special_codes & SPECIAL_FRAME_LEFT_SILENCE) != 0
                && (special_codes & SPECIAL_FRAME_RIGHT_SILENCE) != 0
            {
                for _ in 0..frame_blocks {
                    unprepare::unprepare(&[0, 0], channels, bits, &mut pcm_output)?;
                }
            }
            else if (special_codes & SPECIAL_FRAME_PSEUDO_STEREO) != 0 {
                for _ in 0..frame_blocks {
                    let val = entropy_states[0].decode_value_range(range_coder, &mut br)?;
                    let x = predictors[0].decompress_value(val, 0);
                    unprepare::unprepare(&[x, 0], channels, bits, &mut pcm_output)?;
                }
            }
            else if version >= 3950 {
                for _ in 0..frame_blocks {
                    let ny = entropy_states[1].decode_value_range(range_coder, &mut br)?;
                    let nx = entropy_states[0].decode_value_range(range_coder, &mut br)?;
                    let y = predictors[1].decompress_value(ny, last_x as i64);
                    let x = predictors[0].decompress_value(nx, y as i64);
                    last_x = x;
                    unprepare::unprepare(&[x, y], channels, bits, &mut pcm_output)?;
                }
            }
            else {
                for _ in 0..frame_blocks {
                    let ex = entropy_states[0].decode_value_range(range_coder, &mut br)?;
                    let ey = entropy_states[1].decode_value_range(range_coder, &mut br)?;
                    let x = predictors[0].decompress_value(ex, 0);
                    let y = predictors[1].decompress_value(ey, 0);
                    unprepare::unprepare(&[x, y], channels, bits, &mut pcm_output)?;
                }
            }
        }
        else if channels == 1 {
            if (special_codes & SPECIAL_FRAME_MONO_SILENCE) != 0 {
                for _ in 0..frame_blocks {
                    unprepare::unprepare(&[0], channels, bits, &mut pcm_output)?;
                }
            }
            else {
                for _ in 0..frame_blocks {
                    let val = entropy_states[0].decode_value_range(range_coder, &mut br)?;
                    let decoded = predictors[0].decompress_value(val, 0);
                    unprepare::unprepare(&[decoded], channels, bits, &mut pcm_output)?;
                }
            }
        }
        else {
            let ch = channels as usize;
            let mut values = vec![0i32; ch];
            for _ in 0..frame_blocks {
                for c in 0..ch {
                    let val = entropy_states[c].decode_value_range(range_coder, &mut br)?;
                    values[c] = predictors[c].decompress_value(val, 0);
                }
                unprepare::unprepare(&values, channels, bits, &mut pcm_output)?;
            }
        }
        Ok(())
    })();

    decode_result?;

    // --- EndFrame ---
    range_coder.finalize(&mut br);
    let computed_crc = ape_crc(&pcm_output);
    if computed_crc != stored_crc {
        return Err(ApeError::InvalidChecksum);
    }

    // Post-processing transforms (applied AFTER CRC, matching C++ GetData behavior)
    apply_post_processing(&mut pcm_output, bits, channels);

    Ok(pcm_output)
}

fn try_decode_frame_32(
    frame_data: &[u8],
    seek_remainder: u32,
    frame_blocks: usize,
    version: i32,
    channels: u16,
    bits: u16,
    block_align: usize,
    predictors: &mut [Predictor3950_32],
    entropy_states: &mut [EntropyState],
    range_coder: &mut RangeCoder,
) -> ApeResult<Vec<u8>> {
    let mut br = BitReader::from_frame_bytes(frame_data, seek_remainder * 8);

    let mut stored_crc = br.decode_value_x_bits(32);
    let mut special_codes: i32 = 0;
    if version > 3820 {
        if stored_crc & 0x80000000 != 0 {
            special_codes = br.decode_value_x_bits(32) as i32;
        }
        stored_crc &= 0x7FFFFFFF;
    }

    for p in predictors.iter_mut() {
        p.flush();
    }
    for s in entropy_states.iter_mut() {
        s.flush();
    }
    range_coder.flush_bit_array(&mut br);

    let mut last_x: i64 = 0;
    let pcm_size =
        frame_blocks.checked_mul(block_align).ok_or(ApeError::InvalidFormat("frame too large"))?;
    if pcm_size > 64 * 1024 * 1024 {
        return Err(ApeError::InvalidFormat("frame PCM size exceeds 64 MB"));
    }
    let mut pcm_output = Vec::with_capacity(pcm_size);

    if channels == 2 {
        if (special_codes & SPECIAL_FRAME_LEFT_SILENCE) != 0
            && (special_codes & SPECIAL_FRAME_RIGHT_SILENCE) != 0
        {
            for _ in 0..frame_blocks {
                unprepare::unprepare(&[0, 0], channels, bits, &mut pcm_output)?;
            }
        }
        else if (special_codes & SPECIAL_FRAME_PSEUDO_STEREO) != 0 {
            for _ in 0..frame_blocks {
                let val = entropy_states[0].decode_value_range(range_coder, &mut br)?;
                let x = predictors[0].decompress_value(val, 0);
                unprepare::unprepare(&[x as i32, 0], channels, bits, &mut pcm_output)?;
            }
        }
        else {
            for _ in 0..frame_blocks {
                let ny = entropy_states[1].decode_value_range(range_coder, &mut br)?;
                let nx = entropy_states[0].decode_value_range(range_coder, &mut br)?;
                let y = predictors[1].decompress_value(ny, last_x);
                let x = predictors[0].decompress_value(nx, y as i64);
                last_x = x as i64;
                unprepare::unprepare(&[x as i32, y as i32], channels, bits, &mut pcm_output)?;
            }
        }
    }
    else if channels == 1 {
        if (special_codes & SPECIAL_FRAME_MONO_SILENCE) != 0 {
            for _ in 0..frame_blocks {
                unprepare::unprepare(&[0], channels, bits, &mut pcm_output)?;
            }
        }
        else {
            for _ in 0..frame_blocks {
                let val = entropy_states[0].decode_value_range(range_coder, &mut br)?;
                let decoded = predictors[0].decompress_value(val, 0);
                unprepare::unprepare(&[decoded as i32], channels, bits, &mut pcm_output)?;
            }
        }
    }

    range_coder.finalize(&mut br);
    let computed_crc = ape_crc(&pcm_output);
    if computed_crc != stored_crc {
        return Err(ApeError::InvalidChecksum);
    }

    // Post-processing transforms (applied AFTER CRC, matching C++ GetData behavior)
    apply_post_processing(&mut pcm_output, bits, channels);

    Ok(pcm_output)
}

/// Apply format-flag-dependent transforms to decoded PCM data.
///
/// These are applied AFTER CRC verification and match the C++ `GetData()` behavior.
/// For WAV-sourced files (the common case), all flags are 0 and this is a no-op.
fn apply_post_processing(pcm: &mut [u8], bits: u16, _channels: u16) {
    // The format flags are embedded in the APE header and control how the raw
    // PCM bytes should be transformed for the output format. Since our decoder
    // targets the same format as the source, these transforms are only needed
    // when the source was in a non-standard format.
    //
    // Note: In the current implementation, format flags are exposed via ApeInfo
    // but the caller is responsible for checking them. The transforms below
    // would be applied when the corresponding flags are set, but since all
    // our test fixtures are standard WAV (flags = 0), they're not exercised.
    //
    // The transforms are documented here for future implementation if needed:
    //
    // APE_FORMAT_FLAG_FLOATING_POINT: apply FloatTransform to each 32-bit sample
    // APE_FORMAT_FLAG_SIGNED_8_BIT: add 128 (wrapping) to each byte
    // APE_FORMAT_FLAG_BIG_ENDIAN: byte-swap each sample
    let _ = (pcm, bits);
}

// ---------------------------------------------------------------------------
// FrameDecoder — stateful frame decoder for external demuxer integration
// ---------------------------------------------------------------------------

/// A stateful APE frame decoder that works on raw compressed frame bytes.
///
/// `FrameDecoder` only performs decoding: the demuxer manages all I/O and supplies the compressed
/// frame data as byte slices.
pub struct FrameDecoder {
    predictors: Predictors,
    entropy_states: Vec<EntropyState>,
    range_coder: RangeCoder,
    version: i32,
    channels: u16,
    bits_per_sample: u16,
    block_align: usize,
    interim_mode: bool,
}

impl FrameDecoder {
    /// Create a new `FrameDecoder` with the given APE stream parameters.
    ///
    /// * `version` — APE file version (e.g., 3990). Must be >= 3950.
    /// * `channels` — Number of audio channels (1–32).
    /// * `bits_per_sample` — Bits per sample (8, 16, 24, or 32).
    /// * `compression_level` — Compression level (1000–5000).
    ///
    /// Returns an error if the parameters are invalid (unsupported version,
    /// zero channels, or unsupported bit depth).
    pub fn new(
        version: u16,
        channels: u16,
        bits_per_sample: u16,
        compression_level: u16,
    ) -> ApeResult<Self> {
        if version < 3950 {
            return Err(ApeError::UnsupportedVersion(version));
        }
        if channels == 0 {
            return Err(ApeError::InvalidFormat("channel count must be >= 1"));
        }
        if !matches!(bits_per_sample, 8 | 16 | 24 | 32) {
            return Err(ApeError::InvalidFormat("bits per sample must be 8, 16, 24, or 32"));
        }

        let v = version as i32;
        let comp = compression_level as u32;

        let predictors = if bits_per_sample >= 32 {
            Predictors::Path32((0..channels).map(|_| Predictor3950_32::new(comp, v)).collect())
        }
        else {
            Predictors::Path16(
                (0..channels).map(|_| Predictor3950::new(comp, v, bits_per_sample)).collect(),
            )
        };

        let entropy_states = (0..channels).map(|_| EntropyState::new()).collect();
        let bytes_per_sample = (bits_per_sample / 8) as usize;
        let block_align = bytes_per_sample * channels as usize;

        Ok(FrameDecoder {
            predictors,
            entropy_states,
            range_coder: RangeCoder::new(),
            version: v,
            channels,
            bits_per_sample,
            block_align,
            interim_mode: false,
        })
    }

    /// Decode a compressed frame to raw PCM bytes.
    ///
    /// * `frame_data` — Compressed frame bytes (including alignment prefix),
    ///   as read by the demuxer.
    /// * `seek_remainder` — Byte alignment offset for this frame,
    ///   as computed by the demuxer.
    /// * `frame_blocks` — Number of audio blocks (samples per channel) in this frame.
    pub fn decode_frame(
        &mut self,
        frame_data: &[u8],
        seek_remainder: u32,
        frame_blocks: usize,
    ) -> ApeResult<Vec<u8>> {
        match &mut self.predictors {
            Predictors::Path16(predictors) => {
                let result = try_decode_frame_16(
                    frame_data,
                    seek_remainder,
                    frame_blocks,
                    self.version,
                    self.channels,
                    self.bits_per_sample,
                    self.block_align,
                    predictors,
                    &mut self.entropy_states,
                    &mut self.range_coder,
                );

                match result {
                    Ok(pcm) => Ok(pcm),
                    Err(ApeError::InvalidChecksum)
                        if self.bits_per_sample == 24 && !self.interim_mode =>
                    {
                        self.interim_mode = true;
                        for p in predictors.iter_mut() {
                            p.set_interim_mode(true);
                        }
                        try_decode_frame_16(
                            frame_data,
                            seek_remainder,
                            frame_blocks,
                            self.version,
                            self.channels,
                            self.bits_per_sample,
                            self.block_align,
                            predictors,
                            &mut self.entropy_states,
                            &mut self.range_coder,
                        )
                    }
                    Err(e) => Err(e),
                }
            }
            Predictors::Path32(predictors) => try_decode_frame_32(
                frame_data,
                seek_remainder,
                frame_blocks,
                self.version,
                self.channels,
                self.bits_per_sample,
                self.block_align,
                predictors,
                &mut self.entropy_states,
                &mut self.range_coder,
            ),
        }
    }
}
