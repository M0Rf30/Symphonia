// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A decoder of LZO1X streams, used to undo the lzo `ContentCompression` of Matroska tracks.
//!
//! The stream format is described by the Linux kernel's `Documentation/lzo.rst` and implemented
//! by its `lib/lzo/lzo1x_decompress_safe.c` (and by FFmpeg's `libavutil/lzo.c`). The stream is a
//! sequence of instructions, each a literal run or a match against the output, ended by the
//! end-of-stream marker (an M4 match with a distance of 0x4000: `11 00 00`).

use symphonia_core::errors::{Result, decode_error};

/// The maximum size of a decompressed frame. No frame of an audio codec is anywhere close.
const MAX_OUTPUT: usize = 1 << 28;

struct Input<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Input<'_> {
    fn byte(&mut self) -> Result<u8> {
        let Some(&b) = self.data.get(self.pos)
        else {
            return decode_error("mkv: truncated lzo stream");
        };
        self.pos += 1;
        Ok(b)
    }

    /// Read the length extension of a run whose 4 or 5 bit length field is zero: every zero byte
    /// adds 255, and the first non-zero byte ends the extension.
    fn extended_length(&mut self, base: usize) -> Result<usize> {
        let mut len = base;
        loop {
            let b = self.byte()?;
            if b == 0 {
                len += 255;
                if len > MAX_OUTPUT {
                    return decode_error("mkv: lzo length too large");
                }
            }
            else {
                return Ok(len + usize::from(b));
            }
        }
    }

    fn literals(&mut self, n: usize, out: &mut Vec<u8>) -> Result<()> {
        let Some(bytes) = self.data.get(self.pos..self.pos + n)
        else {
            return decode_error("mkv: truncated lzo stream");
        };
        if out.len() + n > MAX_OUTPUT {
            return decode_error("mkv: lzo output too large");
        }
        out.extend_from_slice(bytes);
        self.pos += n;
        Ok(())
    }
}

fn copy_match(out: &mut Vec<u8>, dist: usize, len: usize) -> Result<()> {
    if dist == 0 || dist > out.len() {
        return decode_error("mkv: lzo match distance too far back");
    }
    if out.len() + len > MAX_OUTPUT {
        return decode_error("mkv: lzo output too large");
    }
    // The copy may overlap itself.
    let start = out.len() - dist;
    for i in 0..len {
        out.push(out[start + i]);
    }
    Ok(())
}

/// Decompress a raw LZO1X stream.
pub(crate) fn lzo1x_decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut inp = Input { data, pos: 0 };
    let mut out = Vec::with_capacity(data.len().saturating_mul(2));

    // The number of literals that followed the previous instruction: 0 to 3 (carried in the low
    // two bits of the previous instruction), or 4 after a literal run (or more than 3 literals
    // at the start of the stream).
    let mut state = 0usize;

    // A first byte above 17 is a literal run of that byte minus 17.
    if data.first().is_some_and(|&b| b > 17) {
        let n = usize::from(inp.byte()?) - 17;
        inp.literals(n, &mut out)?;
        state = n.min(4);
    }

    loop {
        let t = usize::from(inp.byte()?);

        let (dist, len, trailing) = match t {
            // A literal run if no literals preceded, else an M1 match.
            0..=15 if state == 0 => {
                let n = if t == 0 { inp.extended_length(15)? } else { t } + 3;
                inp.literals(n, &mut out)?;
                state = 4;
                continue;
            }
            0..=15 => {
                let h = usize::from(inp.byte()?);
                if state == 4 {
                    // M1 match of 3 bytes, beyond the 2 KiB offset.
                    ((t >> 2) + (h << 2) + 2049, 3, t & 3)
                }
                else {
                    // M1 match of 2 bytes.
                    ((t >> 2) + (h << 2) + 1, 2, t & 3)
                }
            }
            // M4 match: 16 KiB to 48 KiB away, or the end of the stream.
            16..=31 => {
                let len = if t & 7 == 0 { inp.extended_length(7)? } else { t & 7 } + 2;
                let s = usize::from(inp.byte()?) | (usize::from(inp.byte()?) << 8);
                let dist = ((t & 8) << 11) + (s >> 2);
                if dist == 0 {
                    return Ok(out);
                }
                (dist + 0x4000, len, s & 3)
            }
            // M3 match: up to 16 KiB away.
            32..=63 => {
                let len = if t & 31 == 0 { inp.extended_length(31)? } else { t & 31 } + 2;
                let s = usize::from(inp.byte()?) | (usize::from(inp.byte()?) << 8);
                ((s >> 2) + 1, len, s & 3)
            }
            // M2 match: up to 2 KiB away.
            _ => {
                let h = usize::from(inp.byte()?);
                (((t >> 2) & 7) + (h << 3) + 1, (t >> 5) + 1, t & 3)
            }
        };

        copy_match(&mut out, dist, len)?;
        inp.literals(trailing, &mut out)?;
        state = trailing;
    }
}

#[cfg(test)]
mod tests {
    use super::lzo1x_decompress;

    const EOS: [u8; 3] = [0x11, 0x00, 0x00];

    #[test]
    fn verify_initial_literal_run() {
        // 17 + 5 literals, then the end marker.
        let mut s = vec![22];
        s.extend_from_slice(b"hello");
        s.extend_from_slice(&EOS);
        assert_eq!(lzo1x_decompress(&s).unwrap(), b"hello");
    }

    #[test]
    fn verify_m2_overlapping_match() {
        // Literal run of 4 (first byte 21), then M2: length 3 ((t >> 5) + 1 with t = 0x40 | 0),
        // distance 1 + (t >> 2 & 7) + (h << 3) = 1 with h = 0, no trailing literals.
        let mut s = vec![21];
        s.extend_from_slice(b"abcd");
        s.extend_from_slice(&[0x40 | 0x20, 0x00]);
        s.extend_from_slice(&EOS);
        // t = 0x60: length (3 + 1) = 4 copies of 'd'.
        assert_eq!(lzo1x_decompress(&s).unwrap(), b"abcddddd");
    }

    #[test]
    fn verify_m3_match_and_trailing_literals() {
        // 6 literals via the first byte, then M3 (t = 0x20 | 2 -> length 4), S = (dist-1) << 2 | 2
        // with distance 6, followed by 2 trailing literals.
        let mut s = vec![23];
        s.extend_from_slice(b"abcdef");
        s.push(0x22);
        let sv = ((6u16 - 1) << 2) | 2;
        s.extend_from_slice(&sv.to_le_bytes());
        s.extend_from_slice(b"XY");
        s.extend_from_slice(&EOS);
        assert_eq!(lzo1x_decompress(&s).unwrap(), b"abcdefabcdXY");
    }

    #[test]
    fn verify_literal_run_instruction() {
        // Initial run of 1 literal leaves state 1 => next t < 16 is an M1 match, so start with
        // state 0 instead: a literal run instruction t = 2 (5 literals), then the end marker.
        let mut s = vec![2];
        s.extend_from_slice(b"12345");
        s.extend_from_slice(&EOS);
        assert_eq!(lzo1x_decompress(&s).unwrap(), b"12345");
    }

    #[test]
    fn verify_rejects_bad_streams() {
        // Match before any output.
        assert!(lzo1x_decompress(&[0x40, 0x00, 0x11, 0, 0]).is_err());
        // Truncated.
        assert!(lzo1x_decompress(&[22, b'a']).is_err());
        assert!(lzo1x_decompress(&[]).is_err());
    }
}
