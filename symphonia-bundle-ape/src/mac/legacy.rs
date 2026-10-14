// Ported from the Monkey's Audio SDK 13.27 (Matthew T. Ashland, BSD 3-clause; see NOTICE in this
// directory): MACLib/Old/UnMACOld.cpp (DecompressFrameOld, CalculateOldChecksum),
// MACLib/Old/APEDecompressCoreOld.cpp (GenerateDecodedArrays, GenerateDecodedArray) and
// MACLib/Prepare.cpp (UnprepareOld).

//! The frame decoder of the files older than version 3.93 (3930).
//!
//! These files code every channel of a frame as a separate array of residuals, which is turned
//! into samples by an "anti-predictor" that works on the whole array. The two channels of a stereo
//! file are the "mid" and "side" values (X and Y).

use crate::mac::bitreader::ByteReader;
use crate::mac::crc::crc32;
use crate::mac::error::{ApeError, ApeResult};
use crate::mac::frame::{FrameInput, MAX_FRAME_PCM_BYTES};
use crate::mac::legacy_bits::{BitCursor, generate_array_old, generate_array_rice};
use crate::mac::legacy_predictor::{Coefficients, Kind, Scratch};
use crate::mac::range_coder::{EntropyState, RangeCoderV1};

const SPECIAL_FRAME_LEFT_SILENCE: u32 = 1;
const SPECIAL_FRAME_RIGHT_SILENCE: u32 = 2;
const SPECIAL_FRAME_PSEUDO_STEREO: u32 = 4;

/// The compressed stream of a frame: the versions before 3.90 are bit oriented, the later ones
/// are range coded.
enum Stream<'a> {
    Bits(BitCursor<'a>),
    Range(ByteReader<'a>),
}

impl Stream<'_> {
    /// `DecodeValue(DECODE_VALUE_METHOD_UNSIGNED_INT)`
    fn read_u32(&mut self) -> u32 {
        match self {
            Stream::Bits(bits) => bits.read_bits(32),
            Stream::Range(br) => br.next_u32(),
        }
    }

    /// `CUnBitArray*::GenerateArray`
    fn generate_array(&mut self, version: i32, out: &mut [i32]) -> ApeResult<()> {
        match self {
            Stream::Bits(bits) => {
                if version < 3860 {
                    generate_array_old(bits, out)
                }
                else {
                    generate_array_rice(bits, out, version)
                }
            }
            Stream::Range(br) => {
                // `GenerateArrayRange`
                let mut state = EntropyState::new();
                let mut rc = RangeCoderV1::new(br, version);
                for o in out.iter_mut() {
                    *o = rc.decode_value(&mut state, br)? as i32;
                }
                rc.finalize(br, version);
                Ok(())
            }
        }
    }
}

/// The decoder of the frames of the versions 3.0 to 3.92.
pub struct LegacyDecoder {
    version: i32,
    kind: Kind,
    channels: u16,
    bits_per_sample: u16,
    block_align: usize,
    /// Whether the frames have a CRC (otherwise an older checksum of the samples).
    uses_crc: bool,
    x: Vec<i32>,
    y: Vec<i32>,
    temp: Vec<i32>,
    scratch: Scratch,
}

impl LegacyDecoder {
    /// `uses_crc` is the CRC flag of the format flags in the header of the file.
    pub fn new(
        version: u16,
        channels: u16,
        bits_per_sample: u16,
        compression_level: u32,
        uses_crc: bool,
    ) -> ApeResult<Self> {
        let version = i32::from(version);

        // The versions 3891 to 3899 have no bit array in the SDK (their frames cannot be decoded).
        if (3891..3900).contains(&version) {
            return Err(ApeError::UnsupportedVersion(version as u16));
        }
        if channels > 2 {
            return Err(ApeError::InvalidFormat("more than two channels need version 3.99"));
        }

        Ok(LegacyDecoder {
            version,
            kind: Kind::new(compression_level, version)?,
            channels,
            bits_per_sample,
            block_align: usize::from(bits_per_sample / 8) * usize::from(channels),
            uses_crc,
            x: Vec::new(),
            y: Vec::new(),
            temp: Vec::new(),
            scratch: Scratch::default(),
        })
    }

    pub fn decode_frame(
        &mut self,
        input: &FrameInput<'_>,
        frame_blocks: usize,
        pcm: &mut Vec<u8>,
    ) -> ApeResult<usize> {
        if frame_blocks == 0 {
            return Err(ApeError::InvalidFormat("empty frame"));
        }

        // The arrays of these versions depend on each other, so a frame cannot be partially
        // decoded.
        if input.truncated {
            return Err(ApeError::DecodingError("truncated frame"));
        }

        let pcm_size = frame_blocks
            .checked_mul(self.block_align)
            .filter(|&size| size <= MAX_FRAME_PCM_BYTES)
            .ok_or(ApeError::InvalidFormat("frame PCM size exceeds 64 MB"))?;

        let version = self.version;

        let mut stream = if version < 3900 {
            let start = u64::from(input.seek_remainder) * 8 + u64::from(input.seek_bit);
            Stream::Bits(BitCursor::new(input.data, start))
        }
        else {
            Stream::Range(ByteReader::new(input.data, input.seek_remainder))
        };

        // The checksum, and the special codes if the file has them.
        let mut special_codes = 0u32;
        let stored_crc;

        if !self.uses_crc {
            stored_crc = match &mut stream {
                Stream::Bits(bits) => bits.decode_rice_unsigned(30)?,
                // The range coded streams only know the 32 bit value, so this reads nothing.
                Stream::Range(_) => 0,
            };
            if stored_crc == 0 {
                special_codes = SPECIAL_FRAME_LEFT_SILENCE | SPECIAL_FRAME_RIGHT_SILENCE;
            }
        }
        else {
            let mut crc = stream.read_u32();

            if version > 3820 {
                if crc & 0x8000_0000 != 0 {
                    special_codes = stream.read_u32();
                }
                crc &= 0x7FFF_FFFF;
            }
            stored_crc = crc;
        }

        // The arrays are `frame_blocks` long, whatever the size of the previous frame was.
        if self.x.len() < frame_blocks {
            self.x.resize(frame_blocks, 0);
            self.y.resize(frame_blocks, 0);
            self.temp.resize(frame_blocks, 0);
        }
        let n = frame_blocks;

        // --- GenerateDecodedArrays ---
        if self.channels == 2 {
            if special_codes & SPECIAL_FRAME_LEFT_SILENCE != 0
                && special_codes & SPECIAL_FRAME_RIGHT_SILENCE != 0
            {
                self.x[..n].fill(0);
                self.y[..n].fill(0);
            }
            else if special_codes & SPECIAL_FRAME_PSEUDO_STEREO != 0 {
                self.generate_decoded_array(&mut stream, input, n, false)?;
                self.y[..n].fill(0);
            }
            else {
                self.generate_decoded_array(&mut stream, input, n, false)?;
                self.generate_decoded_array(&mut stream, input, n, true)?;
            }
        }
        else if special_codes & SPECIAL_FRAME_LEFT_SILENCE != 0 {
            self.x[..n].fill(0);
        }
        else {
            self.generate_decoded_array(&mut stream, input, n, false)?;
        }

        // --- UnprepareOld ---
        pcm.clear();
        pcm.resize(pcm_size, 0);
        self.unprepare(n, pcm);

        let mut crc = crc32(pcm);
        if version > 3820 {
            crc >>= 1;
        }

        // --- the check ---
        let checksum = if self.uses_crc { crc } else { self.old_checksum(n) };
        if checksum != stored_crc {
            return Err(ApeError::InvalidChecksum);
        }

        Ok(frame_blocks)
    }

    /// `CAPEDecompressCoreOld::GenerateDecodedArray`: decode and anti-predict one channel.
    fn generate_decoded_array(
        &mut self,
        stream: &mut Stream<'_>,
        input: &FrameInput<'_>,
        n: usize,
        second_channel: bool,
    ) -> ApeResult<()> {
        if input.data.len() <= input.seek_remainder as usize {
            return Err(ApeError::InvalidFormat("empty frame"));
        }

        let version = self.version;
        let out = if second_channel { &mut self.y[..n] } else { &mut self.x[..n] };
        let mut coefficients = Coefficients { count: 0, a: [0; 64], b: [0; 64] };

        if self.kind.in_place() {
            stream.generate_array(version, out)?;
            self.kind.anti_predict(version, out, &mut [], &coefficients, &mut self.scratch);
            return Ok(());
        }

        if let Some((count_bits, value_bits)) = self.kind.coefficient_bits() {
            let Stream::Bits(bits) = stream
            else {
                return Err(ApeError::InvalidFormat("coefficients in a range coded stream"));
            };

            let count = bits.decode_x_bits(count_bits) as usize;
            for z in 0..=count {
                coefficients.a[z] = i64::from(bits.decode_x_bits(value_bits));
                coefficients.b[z] = i64::from(bits.decode_x_bits(value_bits));
            }
            coefficients.count = count;
        }

        let temp = &mut self.temp[..n];
        stream.generate_array(version, temp)?;
        self.kind.anti_predict(version, temp, out, &coefficients, &mut self.scratch);

        Ok(())
    }

    /// `CPrepare::UnprepareOld`: convert the arrays to PCM bytes.
    fn unprepare(&self, n: usize, pcm: &mut [u8]) {
        let (x, y) = (&self.x[..n], &self.y[..n]);
        let new_8bit = self.version > 3830;

        if self.channels == 2 {
            match self.bits_per_sample {
                16 => {
                    for ((&x, &y), out) in x.iter().zip(y).zip(pcm.chunks_exact_mut(4)) {
                        let r = x.wrapping_sub(y / 2) as i16;
                        let l = i32::from(r).wrapping_add(y) as i16;
                        out[..2].copy_from_slice(&r.to_le_bytes());
                        out[2..].copy_from_slice(&l.to_le_bytes());
                    }
                }
                8 => {
                    let offset = if new_8bit { 128 } else { 0 };
                    for ((&x, &y), out) in x.iter().zip(y).zip(pcm.chunks_exact_mut(2)) {
                        let r = x.wrapping_sub(y / 2).wrapping_add(offset) as u8;
                        out[0] = r;
                        out[1] = i32::from(r).wrapping_add(y) as u8;
                    }
                }
                _ => {
                    for ((&x, &y), out) in x.iter().zip(y).zip(pcm.chunks_exact_mut(6)) {
                        let r = x.wrapping_sub(y / 2);
                        write_24bit(r, &mut out[..3]);
                        write_24bit(r.wrapping_add(y), &mut out[3..]);
                    }
                }
            }
        }
        else {
            match self.bits_per_sample {
                8 => {
                    let offset = if new_8bit { 128 } else { 0 };
                    for (&x, out) in x.iter().zip(pcm.iter_mut()) {
                        *out = x.wrapping_add(offset) as u8;
                    }
                }
                24 => {
                    for (&x, out) in x.iter().zip(pcm.chunks_exact_mut(3)) {
                        write_24bit(x, out);
                    }
                }
                _ => {
                    for (&x, out) in x.iter().zip(pcm.chunks_exact_mut(2)) {
                        out.copy_from_slice(&(x as i16).to_le_bytes());
                    }
                }
            }
        }
    }

    /// `CUnMACOld::CalculateOldChecksum`
    fn old_checksum(&self, n: usize) -> u32 {
        let x = &self.x[..n];

        if self.channels == 2 {
            x.iter().zip(&self.y[..n]).fold(0u32, |sum, (&x, &y)| {
                let r = x.wrapping_sub(y / 2);
                let l = r.wrapping_add(y);
                sum.wrapping_add((i64::from(r).abs() + i64::from(l).abs()) as u32)
            })
        }
        else {
            x.iter().fold(0u32, |sum, &x| sum.wrapping_add(i64::from(x).unsigned_abs() as u32))
        }
    }
}

/// The 24-bit sample encoding of the SDK: negative values are offset by `0x800000` and have bit
/// 23 set.
#[inline(always)]
fn write_24bit(value: i32, out: &mut [u8]) {
    let temp =
        if value < 0 { (value.wrapping_add(0x80_0000) as u32) | 0x80_0000 } else { value as u32 };
    out[0] = temp as u8;
    out[1] = (temp >> 8) as u8;
    out[2] = (temp >> 16) as u8;
}
