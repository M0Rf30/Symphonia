// Vendored from ape-decoder 0.3.2 (https://github.com/OMBS-IO/ape-decoder, commit c7141a8).
// Copyright (c) 2026 ombs.io. Licensed under MIT OR Apache-2.0; see LICENSE-MIT, LICENSE-APACHE
// and NOTICE in this directory. Modified for Symphonia.

//! Stateful APE frame decoder for external demuxer integration.
//!
//! A frame is decoded in blocks of [`CHUNK`] samples per channel, in three phases: the residuals
//! of the block are range decoded, they are run through the neural network filter cascade of
//! their channel, and finally the second prediction stage and the channel decorrelation
//! reconstruct the PCM samples. This is possible because the entropy decoding does not depend on
//! the predictors, and the neural network filters do not depend on the other channels.

use crate::mac::crc::ape_crc;
use crate::mac::error::{ApeError, ApeResult};
use crate::mac::predictor::{Predictor3950, Predictor3950_32};
use crate::mac::range_coder::{EntropyState, RangeCoder};
use crate::mac::unprepare::{unprepare_mono, unprepare_multichannel, unprepare_stereo};

use crate::mac::bitreader::ByteReader;

// Special frame codes (from Prepare.h)
const SPECIAL_FRAME_MONO_SILENCE: i32 = 1;
const SPECIAL_FRAME_LEFT_SILENCE: i32 = 1;
const SPECIAL_FRAME_RIGHT_SILENCE: i32 = 2;
const SPECIAL_FRAME_PSEUDO_STEREO: i32 = 4;

/// The number of blocks (samples per channel) that are decoded phase by phase.
const CHUNK: usize = 4096;

/// The largest PCM size of a frame that will be decoded.
const MAX_FRAME_PCM_BYTES: usize = 64 * 1024 * 1024;

/// What the frame decoding needs from a predictor.
trait Predictor {
    /// Whether more than two channels are decoded (the 32-bit path only does mono and stereo).
    const MULTICHANNEL: bool;

    fn flush(&mut self);

    /// Reconstruct a sample from its entropy-decoded residual and the cross-channel value.
    fn decompress_value(&mut self, residual: i64, n_b: i32) -> i32;
}

impl Predictor for Predictor3950 {
    const MULTICHANNEL: bool = true;

    fn flush(&mut self) {
        Predictor3950::flush(self);
    }

    #[inline(always)]
    fn decompress_value(&mut self, residual: i64, n_b: i32) -> i32 {
        Predictor3950::decompress_value(self, residual, n_b)
    }
}

impl Predictor for Predictor3950_32 {
    const MULTICHANNEL: bool = false;

    fn flush(&mut self) {
        Predictor3950_32::flush(self);
    }

    #[inline(always)]
    fn decompress_value(&mut self, residual: i64, n_b: i32) -> i32 {
        Predictor3950_32::decompress_value(self, residual, n_b)
    }
}

/// The predictors of a decoder, one per channel.
enum Predictors {
    Path16(Vec<Predictor3950>),
    Path32(Vec<Predictor3950_32>),
}

/// The stream parameters a frame decode needs.
struct Params {
    version: i32,
    channels: u16,
    bits_per_sample: u16,
    block_align: usize,
}

/// A stateful APE frame decoder that works on raw compressed frame bytes.
///
/// `FrameDecoder` only performs decoding: the demuxer manages all I/O and supplies the compressed
/// frame data as byte slices.
pub struct FrameDecoder {
    predictors: Predictors,
    entropy_states: Vec<EntropyState>,
    /// Scratch space for the reconstructed samples of a block, one vector per channel.
    scratch: Vec<Vec<i32>>,
    version: i32,
    channels: u16,
    bits_per_sample: u16,
    block_align: usize,
    interim_mode: bool,
}

impl FrameDecoder {
    /// Create a new `FrameDecoder` with the given APE stream parameters.
    ///
    /// * `version` -- APE file version (e.g., 3990). Must be >= 3950.
    /// * `channels` -- Number of audio channels (1-32).
    /// * `bits_per_sample` -- Bits per sample (8, 16, 24, or 32).
    /// * `compression_level` -- Compression level (1000-5000).
    ///
    /// Returns an error if the parameters are invalid (unsupported version, zero channels, or
    /// unsupported bit depth).
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

        let v = i32::from(version);
        let comp = u32::from(compression_level);
        let nch = usize::from(channels);

        let predictors = if bits_per_sample >= 32 {
            Predictors::Path32((0..nch).map(|_| Predictor3950_32::new(comp, v)).collect())
        }
        else {
            Predictors::Path16(
                (0..nch).map(|_| Predictor3950::new(comp, v, bits_per_sample)).collect(),
            )
        };

        let entropy_states = (0..nch).map(|_| EntropyState::new()).collect();
        let bytes_per_sample = usize::from(bits_per_sample / 8);
        let block_align = bytes_per_sample * nch;

        Ok(FrameDecoder {
            predictors,
            entropy_states,
            scratch: vec![vec![0; CHUNK]; nch],
            version: v,
            channels,
            bits_per_sample,
            block_align,
            interim_mode: false,
        })
    }

    /// Decode a compressed frame to raw PCM bytes.
    ///
    /// * `frame_data` -- Compressed frame bytes (including alignment prefix).
    /// * `seek_remainder` -- Byte alignment offset for this frame.
    /// * `frame_blocks` -- Number of audio blocks (samples per channel) in this frame.
    pub fn decode_frame(
        &mut self,
        frame_data: &[u8],
        seek_remainder: u32,
        frame_blocks: usize,
    ) -> ApeResult<Vec<u8>> {
        let params = Params {
            version: self.version,
            channels: self.channels,
            bits_per_sample: self.bits_per_sample,
            block_align: self.block_align,
        };

        match &mut self.predictors {
            Predictors::Path16(predictors) => {
                let result = decode_frame_impl(
                    &params,
                    frame_data,
                    seek_remainder,
                    frame_blocks,
                    predictors,
                    &mut self.scratch,
                    &mut self.entropy_states,
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
                        decode_frame_impl(
                            &params,
                            frame_data,
                            seek_remainder,
                            frame_blocks,
                            predictors,
                            &mut self.scratch,
                            &mut self.entropy_states,
                        )
                    }
                    Err(e) => Err(e),
                }
            }
            Predictors::Path32(predictors) => decode_frame_impl(
                &params,
                frame_data,
                seek_remainder,
                frame_blocks,
                predictors,
                &mut self.scratch,
                &mut self.entropy_states,
            ),
        }
    }
}

fn decode_frame_impl<P: Predictor>(
    params: &Params,
    frame_data: &[u8],
    seek_remainder: u32,
    frame_blocks: usize,
    predictors: &mut [P],
    scratch: &mut [Vec<i32>],
    entropy_states: &mut [EntropyState],
) -> ApeResult<Vec<u8>> {
    let Params { version, channels, bits_per_sample: bits, block_align } = *params;
    let mut br = ByteReader::new(frame_data, seek_remainder);

    // --- StartFrame ---
    let mut stored_crc = br.next_u32();
    let mut special_codes: i32 = 0;
    if version > 3820 {
        if stored_crc & 0x80000000 != 0 {
            special_codes = br.next_u32() as i32;
        }
        stored_crc &= 0x7FFFFFFF;
    }

    for p in predictors.iter_mut() {
        p.flush();
    }
    for s in entropy_states.iter_mut() {
        s.flush();
    }
    let mut rc = RangeCoder::new(&mut br);

    let pcm_size =
        frame_blocks.checked_mul(block_align).ok_or(ApeError::InvalidFormat("frame too large"))?;
    if pcm_size > MAX_FRAME_PCM_BYTES {
        return Err(ApeError::InvalidFormat("frame PCM size exceeds 64 MB"));
    }

    // The 32-bit path only decodes mono and stereo, and produces no output otherwise.
    let decodes = channels <= 2 || P::MULTICHANNEL;
    let mut pcm = if decodes { vec![0u8; pcm_size] } else { Vec::new() };

    // The previous X output, which the Y predictor of the next block takes as input.
    let mut last_x: i32 = 0;

    let mut done = 0;
    while decodes && done < frame_blocks {
        let n = (frame_blocks - done).min(CHUNK);
        let out = &mut pcm[done * block_align..(done + n) * block_align];

        // The samples of the block are decoded one after the other, since the entropy decoding
        // and the predictors are independent chains of dependent operations that the processor
        // can overlap. The entropy decoding error that stops the block, if any, is held back
        // until the blocks decoded before it are reconstructed, since they may fail in a way that
        // takes precedence.
        let mut n_ok = n;
        let mut entropy_error = None;

        if channels == 2 {
            let (s0, s1) = {
                let (a, b) = scratch.split_at_mut(1);
                (&mut a[0][..n], &mut b[0][..n])
            };
            let (p0, p1) = {
                let (a, b) = predictors.split_at_mut(1);
                (&mut a[0], &mut b[0])
            };
            let (e0, e1) = {
                let (a, b) = entropy_states.split_at_mut(1);
                (&mut a[0], &mut b[0])
            };

            if (special_codes & SPECIAL_FRAME_LEFT_SILENCE) != 0
                && (special_codes & SPECIAL_FRAME_RIGHT_SILENCE) != 0
            {
                s0.fill(0);
                s1.fill(0);
            }
            else if (special_codes & SPECIAL_FRAME_PSEUDO_STEREO) != 0 {
                s1.fill(0);
                for (i, x) in s0.iter_mut().enumerate() {
                    match rc.decode_value(e0, &mut br) {
                        Ok(v) => *x = p0.decompress_value(v, 0),
                        Err(e) => {
                            entropy_error = Some(e);
                            n_ok = i;
                            break;
                        }
                    }
                }
            }
            else {
                for (i, (x, y)) in s0.iter_mut().zip(s1.iter_mut()).enumerate() {
                    // The second channel is coded first.
                    let ny = match rc.decode_value(e1, &mut br) {
                        Ok(v) => v,
                        Err(e) => {
                            entropy_error = Some(e);
                            n_ok = i;
                            break;
                        }
                    };
                    let nx = match rc.decode_value(e0, &mut br) {
                        Ok(v) => v,
                        Err(e) => {
                            entropy_error = Some(e);
                            n_ok = i;
                            break;
                        }
                    };
                    *y = p1.decompress_value(ny, last_x);
                    *x = p0.decompress_value(nx, *y);
                    last_x = *x;
                }
            }

            // s0 and s1 are the "mid" and "side" values, which are decorrelated here.
            unprepare_stereo(bits, &s0[..n_ok], &s1[..n_ok], &mut out[..n_ok * block_align])?;
        }
        else if channels == 1 {
            let s0 = &mut scratch[0][..n];
            let p0 = &mut predictors[0];
            let e0 = &mut entropy_states[0];

            if (special_codes & SPECIAL_FRAME_MONO_SILENCE) != 0 {
                s0.fill(0);
            }
            else {
                for (i, x) in s0.iter_mut().enumerate() {
                    match rc.decode_value(e0, &mut br) {
                        Ok(v) => *x = p0.decompress_value(v, 0),
                        Err(e) => {
                            entropy_error = Some(e);
                            n_ok = i;
                            break;
                        }
                    }
                }
            }

            unprepare_mono(bits, &s0[..n_ok], &mut out[..n_ok * block_align])?;
        }
        else {
            // More than two channels; the samples of a block are coded channel by channel.
            'blocks: for i in 0..n {
                for ((p, e), s) in
                    predictors.iter_mut().zip(entropy_states.iter_mut()).zip(scratch.iter_mut())
                {
                    match rc.decode_value(e, &mut br) {
                        Ok(v) => s[i] = p.decompress_value(v, 0),
                        Err(err) => {
                            entropy_error = Some(err);
                            n_ok = i;
                            break 'blocks;
                        }
                    }
                }
            }

            unprepare_multichannel(bits, scratch, n_ok, &mut out[..n_ok * block_align])?;
        }

        if let Some(e) = entropy_error {
            return Err(e);
        }
        done += n;
    }

    // --- EndFrame ---
    let computed_crc = ape_crc(&pcm);
    if computed_crc != stored_crc {
        return Err(ApeError::InvalidChecksum);
    }

    Ok(pcm)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame that is flagged as silent: the checksum and the special codes, then the (unused)
    /// range coder bytes.
    fn silent_frame(pcm: &[u8], special_codes: u32) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.extend_from_slice(&(ape_crc(pcm) | 0x8000_0000).to_le_bytes());
        frame.extend_from_slice(&special_codes.to_le_bytes());
        frame.extend_from_slice(&[0; 16]);
        frame
    }

    #[test]
    fn decodes_silent_frames() {
        // (channels, bits, special codes, the silent sample bytes)
        let cases: [(u16, u16, u32, &[u8]); 7] = [
            (1, 16, SPECIAL_FRAME_MONO_SILENCE as u32, &[0, 0]),
            (1, 8, SPECIAL_FRAME_MONO_SILENCE as u32, &[128]),
            (1, 24, SPECIAL_FRAME_MONO_SILENCE as u32, &[0, 0, 0]),
            (2, 16, 3, &[0, 0, 0, 0]),
            (2, 8, 3, &[128, 128]),
            (2, 24, 3, &[0; 6]),
            (2, 32, 3, &[0; 8]),
        ];
        // Block counts around the size of the blocks the frame is decoded in.
        for blocks in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 3 * CHUNK + 7] {
            for (channels, bits, special_codes, sample) in cases {
                let pcm: Vec<u8> =
                    sample.iter().copied().cycle().take(blocks * sample.len()).collect();
                let frame = silent_frame(&pcm, special_codes);
                let mut decoder =
                    FrameDecoder::new(3990, channels, bits, 2000).expect("valid frame");
                let decoded = decoder.decode_frame(&frame, 0, blocks).expect("valid frame");
                assert_eq!(decoded, pcm, "{channels} channels, {bits} bits, {blocks} blocks");
            }
        }
    }

    #[test]
    fn rejects_a_wrong_checksum() {
        let frame = silent_frame(&[0; 8], 3);
        let mut decoder = FrameDecoder::new(3990, 2, 16, 2000).expect("valid frame");
        assert!(matches!(decoder.decode_frame(&frame, 0, 3), Err(ApeError::InvalidChecksum)));
        assert!(decoder.decode_frame(&frame, 0, 2).is_ok());
    }

    #[test]
    fn decodes_a_frame_after_an_alignment_prefix() {
        let pcm = [0u8; 16];
        let header = silent_frame(&pcm, 3);

        // The frame starts 3 stream bytes in. The stream bytes are the file bytes of every group
        // of four in reverse order.
        let mut file = vec![0u8; header.len() + 4];
        for k in 0..header.len() {
            file[(3 + k) ^ 3] = header[k ^ 3];
        }

        let mut decoder = FrameDecoder::new(3990, 2, 16, 2000).expect("valid parameters");
        assert_eq!(decoder.decode_frame(&file, 3, 4).expect("valid frame"), pcm);
    }

    #[test]
    fn rejects_invalid_parameters() {
        assert!(matches!(
            FrameDecoder::new(3940, 2, 16, 2000),
            Err(ApeError::UnsupportedVersion(3940))
        ));
        assert!(FrameDecoder::new(3990, 0, 16, 2000).is_err());
        assert!(FrameDecoder::new(3990, 2, 12, 2000).is_err());
    }

    #[test]
    fn a_32_bit_frame_of_more_than_two_channels_has_no_output() {
        // The 32-bit path only decodes mono and stereo, and the checksum of no data is 0.
        let frame = silent_frame(&[], 0);
        let mut decoder = FrameDecoder::new(3990, 3, 32, 2000).expect("valid frame");
        assert!(decoder.decode_frame(&frame, 0, 10).expect("valid frame").is_empty());
    }
}
