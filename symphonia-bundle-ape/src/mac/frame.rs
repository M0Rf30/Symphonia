// Vendored from ape-decoder 0.3.2 (https://github.com/OMBS-IO/ape-decoder, commit c7141a8).
// Copyright (c) 2026 ombs.io. Licensed under MIT OR Apache-2.0; see LICENSE-MIT, LICENSE-APACHE
// and NOTICE in this directory. Modified for Symphonia.

//! Stateful APE frame decoder for external demuxer integration.
//!
//! A frame is decoded in blocks of [`CHUNK`] samples per channel. The samples of a block are
//! range decoded and reconstructed by the predictors one after the other (the entropy decoding is
//! a long chain of dependent operations that the processor overlaps with the predictors of the
//! samples before it), then the whole block is decorrelated and written out as PCM bytes.

use crate::mac::crc::ape_crc;
use crate::mac::error::{ApeError, ApeResult};
use crate::mac::format::APE_FORMAT_FLAG_CRC;
use crate::mac::legacy::LegacyDecoder;
use crate::mac::nn_filter::COMPRESSION_INSANE;
use crate::mac::predictor::{Predictor3930, Predictor3950, Predictor3950_32};
use crate::mac::range_coder::{EntropyDecoder, EntropyState, RangeCoder, RangeCoderV1};
use crate::mac::unprepare::{unprepare_mono, unprepare_multichannel, unprepare_stereo};

use crate::mac::bitreader::ByteReader;

// Special frame codes (from Prepare.h)
const SPECIAL_FRAME_MONO_SILENCE: i32 = 1;
const SPECIAL_FRAME_LEFT_SILENCE: i32 = 1;
const SPECIAL_FRAME_RIGHT_SILENCE: i32 = 2;
const SPECIAL_FRAME_PSEUDO_STEREO: i32 = 4;

/// The number of blocks (samples per channel) that are decoded before they are formatted.
const CHUNK: usize = 4096;

/// The number of blocks that are decoded at a time from a truncated frame.
const TRUNCATED_CHUNK: usize = 256;

/// The largest PCM size of a frame that will be decoded.
pub const MAX_FRAME_PCM_BYTES: usize = 64 * 1024 * 1024;

/// What the frame decoding needs from a predictor.
trait Predictor {
    /// Whether the predictor of the second channel takes the previous sample of the first channel
    /// as input, which makes the decoding order of a stereo pair second channel first (versions
    /// from 3950).
    const CROSS_CHANNEL: bool;

    fn flush(&mut self);

    /// Reconstruct a sample from its entropy-decoded residual and the cross-channel value.
    fn decompress_value(&mut self, residual: i64, n_b: i32) -> i32;
}

impl Predictor for Predictor3950 {
    const CROSS_CHANNEL: bool = true;

    fn flush(&mut self) {
        Predictor3950::flush(self);
    }

    #[inline(always)]
    fn decompress_value(&mut self, residual: i64, n_b: i32) -> i32 {
        Predictor3950::decompress_value(self, residual, n_b)
    }
}

impl Predictor for Predictor3950_32 {
    const CROSS_CHANNEL: bool = true;

    fn flush(&mut self) {
        Predictor3950_32::flush(self);
    }

    #[inline(always)]
    fn decompress_value(&mut self, residual: i64, n_b: i32) -> i32 {
        Predictor3950_32::decompress_value(self, residual, n_b)
    }
}

impl Predictor for Predictor3930 {
    const CROSS_CHANNEL: bool = false;

    fn flush(&mut self) {
        Predictor3930::flush(self);
    }

    #[inline(always)]
    fn decompress_value(&mut self, residual: i64, _n_b: i32) -> i32 {
        Predictor3930::decompress_value(self, residual)
    }
}

/// The predictors of a decoder, one per channel.
enum Predictors {
    Path3930(Vec<Predictor3930>),
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

/// The buffers a frame decode works in.
struct Work<'a> {
    /// Scratch space for the reconstructed samples of a block, one vector per channel.
    scratch: &'a mut [Vec<i32>],
    entropy_states: &'a mut [EntropyState],
    /// The interleaved PCM output.
    pcm: &'a mut Vec<u8>,
}

impl Work<'_> {
    fn reborrow(&mut self) -> Work<'_> {
        Work { scratch: self.scratch, entropy_states: self.entropy_states, pcm: self.pcm }
    }
}
/// The compressed bytes of a frame and where in them the frame starts.
pub struct FrameInput<'a> {
    /// The bytes of the frame, from the start of the 32-bit word the frame starts in, and with the
    /// bytes after the frame that the decoder may read ahead.
    pub data: &'a [u8],
    /// The number of bytes of the first word that precede the frame.
    pub seek_remainder: u32,
    /// The bit of that position the frame starts at (versions up to 3.80, whose frames do not
    /// start on a byte boundary).
    pub seek_bit: u32,
    /// The data ends before the frame does: the file is truncated.
    pub truncated: bool,
}

/// A stateful APE frame decoder that works on raw compressed frame bytes.
///
/// `FrameDecoder` only performs decoding: the demuxer manages all I/O and supplies the compressed
/// frame data as byte slices.
pub struct FrameDecoder {
    core: Core,
}

enum Core {
    /// Versions 3.93 and later.
    Block(BlockDecoder),
    /// Versions before 3.93.
    Legacy(LegacyDecoder),
}

/// The decoder of the versions 3.93 and later, which decode the channels sample by sample.
struct BlockDecoder {
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

/// The oldest file version that is supported. The versions before it are not produced by any of the
/// Monkey's Audio releases that are still in use.
const MIN_VERSION: u16 = 3000;

impl FrameDecoder {
    /// Create a new `FrameDecoder` with the given APE stream parameters.
    ///
    /// * `version` -- APE file version (e.g., 3990), at least 3000.
    /// * `channels` -- Number of audio channels (1-32; more than two need version 3990).
    /// * `bits_per_sample` -- Bits per sample (8, 16, 24, or 32; 32 needs version 3950).
    /// * `compression_level` -- Compression level (1000-5000; 5000 needs version 3950).
    /// * `format_flags` -- The format flags of the header of a file older than version 3980
    ///   (zero for the later versions, which do not have them).
    ///
    /// Returns an error if the parameters are invalid (unsupported version, zero channels, or
    /// unsupported bit depth or compression level).
    pub fn new(
        version: u16,
        channels: u16,
        bits_per_sample: u16,
        compression_level: u16,
        format_flags: u16,
    ) -> ApeResult<Self> {
        if version < MIN_VERSION {
            return Err(ApeError::UnsupportedVersion(version));
        }
        if channels == 0 {
            return Err(ApeError::InvalidFormat("channel count must be >= 1"));
        }
        if !matches!(bits_per_sample, 8 | 16 | 24 | 32) {
            return Err(ApeError::InvalidFormat("bits per sample must be 8, 16, 24, or 32"));
        }
        // The predictors of the SDK throw for all other levels.
        if !matches!(compression_level, 1000 | 2000 | 3000 | 4000 | 5000) {
            return Err(ApeError::InvalidFormat("unsupported compression level"));
        }
        if u32::from(compression_level) == COMPRESSION_INSANE && version < 3950 {
            return Err(ApeError::InvalidFormat("the extra insane level needs version 3.95"));
        }
        if bits_per_sample == 32 && version < 3950 {
            return Err(ApeError::InvalidFormat("32 bit samples need version 3.95"));
        }
        if channels > 2 && version < 3990 {
            return Err(ApeError::InvalidFormat("more than two channels need version 3.99"));
        }

        let core = if version < 3930 {
            Core::Legacy(LegacyDecoder::new(
                version,
                channels,
                bits_per_sample,
                u32::from(compression_level),
                format_flags & APE_FORMAT_FLAG_CRC != 0,
            )?)
        }
        else {
            Core::Block(BlockDecoder::new(version, channels, bits_per_sample, compression_level))
        };

        Ok(FrameDecoder { core })
    }

    /// Decode a compressed frame to raw PCM bytes, replacing the contents of `pcm`.
    ///
    /// * `input` -- The compressed bytes of the frame.
    /// * `frame_blocks` -- Number of audio blocks (samples per channel) in this frame.
    /// * `pcm` -- Output buffer; its allocation is reused across frames.
    ///
    /// Returns the number of blocks decoded, which is only less than `frame_blocks` if the input
    /// is truncated.
    pub fn decode_frame(
        &mut self,
        input: &FrameInput<'_>,
        frame_blocks: usize,
        pcm: &mut Vec<u8>,
    ) -> ApeResult<usize> {
        match &mut self.core {
            Core::Block(decoder) => decoder.decode_frame(input, frame_blocks, pcm),
            Core::Legacy(decoder) => decoder.decode_frame(input, frame_blocks, pcm),
        }
    }
}

impl BlockDecoder {
    fn new(version: u16, channels: u16, bits_per_sample: u16, compression_level: u16) -> Self {
        let v = i32::from(version);
        let comp = u32::from(compression_level);
        let nch = usize::from(channels);

        let predictors = if version < 3950 {
            Predictors::Path3930((0..nch).map(|_| Predictor3930::new(comp, v)).collect())
        }
        else if bits_per_sample >= 32 {
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

        BlockDecoder {
            predictors,
            entropy_states,
            scratch: vec![vec![0; CHUNK]; nch],
            version: v,
            channels,
            bits_per_sample,
            block_align,
            interim_mode: false,
        }
    }

    fn decode_frame(
        &mut self,
        input: &FrameInput<'_>,
        frame_blocks: usize,
        pcm: &mut Vec<u8>,
    ) -> ApeResult<usize> {
        let params = Params {
            version: self.version,
            channels: self.channels,
            bits_per_sample: self.bits_per_sample,
            block_align: self.block_align,
        };

        let mut work =
            Work { scratch: &mut self.scratch, entropy_states: &mut self.entropy_states, pcm };

        match &mut self.predictors {
            Predictors::Path3930(predictors) => {
                decode_frame_with(&params, input, frame_blocks, predictors, work)
            }
            Predictors::Path16(predictors) => {
                let result =
                    decode_frame_with(&params, input, frame_blocks, predictors, work.reborrow());

                match result {
                    Err(ApeError::InvalidChecksum)
                        if self.bits_per_sample == 24 && !self.interim_mode =>
                    {
                        self.interim_mode = true;
                        for p in predictors.iter_mut() {
                            p.set_interim_mode(true);
                        }
                        decode_frame_with(&params, input, frame_blocks, predictors, work)
                    }
                    result => result,
                }
            }
            Predictors::Path32(predictors) => {
                decode_frame_with(&params, input, frame_blocks, predictors, work)
            }
        }
    }
}

/// Decode a frame with the range decoder of the file version.
fn decode_frame_with<P: Predictor>(
    params: &Params,
    input: &FrameInput<'_>,
    frame_blocks: usize,
    predictors: &mut [P],
    work: Work<'_>,
) -> ApeResult<usize> {
    if params.version >= 3990 {
        decode_frame_impl::<P, RangeCoder>(params, input, frame_blocks, predictors, work)
    }
    else {
        decode_frame_impl::<P, RangeCoderV1>(params, input, frame_blocks, predictors, work)
    }
}

fn decode_frame_impl<P: Predictor, E: EntropyDecoder>(
    params: &Params,
    input: &FrameInput<'_>,
    frame_blocks: usize,
    predictors: &mut [P],
    work: Work<'_>,
) -> ApeResult<usize> {
    let Work { scratch, entropy_states, pcm } = work;
    let Params { version, channels, bits_per_sample: bits, block_align } = *params;
    let mut br = ByteReader::new(input.data, input.seek_remainder);

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
    let mut rc = E::start(&mut br, version);

    let pcm_size =
        frame_blocks.checked_mul(block_align).ok_or(ApeError::InvalidFormat("frame too large"))?;
    if pcm_size > MAX_FRAME_PCM_BYTES {
        return Err(ApeError::InvalidFormat("frame PCM size exceeds 64 MB"));
    }

    pcm.clear();
    pcm.resize(pcm_size, 0);

    // The previous X output, which the Y predictor of the next block takes as input.
    let mut last_x: i32 = 0;

    // The blocks of a truncated frame are decoded in smaller steps, so that little of what the
    // data holds is lost with the block that runs out of data.
    let chunk = if input.truncated { TRUNCATED_CHUNK } else { CHUNK };

    let mut done = 0;
    let mut ran_out_of_data = false;
    while done < frame_blocks {
        let n = (frame_blocks - done).min(chunk);
        let out = &mut pcm[done * block_align..(done + n) * block_align];

        // The samples of the block are decoded one after the other, since the entropy decoding
        // and the predictors are independent chains of dependent operations that the processor
        // can overlap. The entropy decoding error that stops the block, if any, is held back
        // until the blocks decoded before it are reconstructed, since they may fail in a way that
        // takes precedence.
        let mut n_ok = n;
        let mut entropy_error = None;
        let unprepared: ApeResult<()> = if channels == 2 {
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
                    match rc.decode(e0, &mut br) {
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
                // The entropy decoding of a pair of samples is the longest chain of dependent
                // operations, so it runs one pair ahead of the reconstruction of the samples: the
                // processor overlaps that with the reconstruction of the pair before it, which is
                // placed first so that the mispredicted branches of the entropy decoding do not
                // discard its work.
                let decode_pair = |rc: &mut E,
                                   br: &mut ByteReader<'_>,
                                   e0: &mut EntropyState,
                                   e1: &mut EntropyState|
                 -> ApeResult<(i64, i64)> {
                    if P::CROSS_CHANNEL {
                        // The second channel is coded first.
                        let ny = rc.decode(e1, br)?;
                        let nx = rc.decode(e0, br)?;
                        Ok((nx, ny))
                    }
                    else {
                        let nx = rc.decode(e0, br)?;
                        let ny = rc.decode(e1, br)?;
                        Ok((nx, ny))
                    }
                };

                let mut next = decode_pair(&mut rc, &mut br, e0, e1);
                for (i, (x, y)) in s0.iter_mut().zip(s1.iter_mut()).enumerate() {
                    let (nx, ny) = match next {
                        Ok(pair) => pair,
                        Err(e) => {
                            entropy_error = Some(e);
                            n_ok = i;
                            break;
                        }
                    };
                    if P::CROSS_CHANNEL {
                        *y = p1.decompress_value(ny, last_x);
                        *x = p0.decompress_value(nx, *y);
                        last_x = *x;
                    }
                    else {
                        *x = p0.decompress_value(nx, 0);
                        *y = p1.decompress_value(ny, 0);
                    }
                    if i + 1 < n {
                        next = decode_pair(&mut rc, &mut br, e0, e1);
                    }
                }
            }

            // s0 and s1 are the "mid" and "side" values, which are decorrelated here.
            unprepare_stereo(bits, &s0[..n_ok], &s1[..n_ok], &mut out[..n_ok * block_align])
        }
        else if channels == 1 {
            let s0 = &mut scratch[0][..n];
            let p0 = &mut predictors[0];
            let e0 = &mut entropy_states[0];

            if (special_codes & SPECIAL_FRAME_MONO_SILENCE) != 0 {
                s0.fill(0);
            }
            else {
                // As for stereo, the decoding of a sample runs ahead of the reconstruction of the
                // sample before it.
                let mut next = rc.decode(e0, &mut br);
                for (i, x) in s0.iter_mut().enumerate() {
                    let v = match next {
                        Ok(v) => v,
                        Err(e) => {
                            entropy_error = Some(e);
                            n_ok = i;
                            break;
                        }
                    };
                    *x = p0.decompress_value(v, 0);
                    if i + 1 < n {
                        next = rc.decode(e0, &mut br);
                    }
                }
            }

            unprepare_mono(bits, &s0[..n_ok], &mut out[..n_ok * block_align])
        }
        else {
            // More than two channels; the samples of a block are coded channel by channel.
            'blocks: for i in 0..n {
                for ((p, e), s) in
                    predictors.iter_mut().zip(entropy_states.iter_mut()).zip(scratch.iter_mut())
                {
                    match rc.decode(e, &mut br) {
                        Ok(v) => s[i] = p.decompress_value(v, 0),
                        Err(err) => {
                            entropy_error = Some(err);
                            n_ok = i;
                            break 'blocks;
                        }
                    }
                }
            }

            unprepare_multichannel(bits, scratch, n_ok, &mut out[..n_ok * block_align])
        };

        // The block that read beyond the data of a truncated frame is made of zero padding rather
        // than of what the encoder wrote: it ends the frame, whatever it decoded to.
        if input.truncated && br.is_past_end() {
            ran_out_of_data = true;
            break;
        }

        unprepared?;
        if let Some(e) = entropy_error {
            return Err(e);
        }
        done += n;
    }

    if ran_out_of_data {
        pcm.truncate(done * block_align);
        return Ok(done);
    }

    // --- EndFrame ---
    let computed_crc = ape_crc(pcm);
    if computed_crc != stored_crc {
        return Err(ApeError::InvalidChecksum);
    }

    Ok(frame_blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(data: &[u8], seek_remainder: u32) -> FrameInput<'_> {
        FrameInput { data, seek_remainder, seek_bit: 0, truncated: false }
    }

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
                    FrameDecoder::new(3990, channels, bits, 2000, 0).expect("valid frame");
                let mut decoded = Vec::new();
                decoder.decode_frame(&input(&frame, 0), blocks, &mut decoded).expect("valid frame");
                assert_eq!(decoded, pcm, "{channels} channels, {bits} bits, {blocks} blocks");
            }
        }
    }

    #[test]
    fn rejects_a_wrong_checksum() {
        let frame = silent_frame(&[0; 8], 3);
        let mut decoder = FrameDecoder::new(3990, 2, 16, 2000, 0).expect("valid frame");
        let mut out = Vec::new();
        assert!(matches!(
            decoder.decode_frame(&input(&frame, 0), 3, &mut out),
            Err(ApeError::InvalidChecksum)
        ));
        assert!(decoder.decode_frame(&input(&frame, 0), 2, &mut out).is_ok());
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

        let mut decoder = FrameDecoder::new(3990, 2, 16, 2000, 0).expect("valid parameters");
        let mut decoded = Vec::new();
        decoder.decode_frame(&input(&file, 3), 4, &mut decoded).expect("valid frame");
        assert_eq!(decoded, pcm);
    }

    #[test]
    fn rejects_invalid_parameters() {
        assert!(matches!(
            FrameDecoder::new(2990, 2, 16, 2000, 0),
            Err(ApeError::UnsupportedVersion(2990))
        ));
        // The versions the SDK has no decoder for, or levels it throws for.
        assert!(matches!(
            FrameDecoder::new(3895, 2, 16, 2000, 2),
            Err(ApeError::UnsupportedVersion(3895))
        ));
        assert!(FrameDecoder::new(3940, 2, 16, 5000, 0).is_err());
        assert!(FrameDecoder::new(3940, 3, 16, 2000, 0).is_err());
        assert!(FrameDecoder::new(3990, 2, 16, 6000, 0).is_err());
        assert!(FrameDecoder::new(3990, 2, 16, 0, 0).is_err());
        assert!(FrameDecoder::new(3990, 0, 16, 2000, 0).is_err());
        assert!(FrameDecoder::new(3990, 2, 12, 2000, 0).is_err());
        for version in [3800, 3880, 3910, 3940, 3950, 3990] {
            assert!(FrameDecoder::new(version, 2, 16, 2000, 2).is_ok(), "{version}");
        }
    }
}
