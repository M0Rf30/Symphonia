// Range Decoder - Rewritten from xiph/opus celt/entdec.c
// Copyright (c) 2001-2011 Timothy B. Terriberry
// Copyright (c) 2008-2009 Xiph.Org Foundation
// SPDX-License-Identifier: BSD-3-Clause

use symphonia_core::errors::Result;

/// Constants from mfrngcod.h
const EC_SYM_BITS: u32 = 8;
const EC_CODE_BITS: u32 = 32;
const EC_SYM_MAX: u32 = (1u32 << EC_SYM_BITS) - 1;  // 255
const EC_CODE_SHIFT: u32 = EC_CODE_BITS - EC_SYM_BITS - 1;  // 23
const EC_CODE_TOP: u32 = 1u32 << (EC_CODE_BITS - 1);  // 2^31
const EC_CODE_BOT: u32 = EC_CODE_TOP >> EC_SYM_BITS;  // 2^23
const EC_CODE_EXTRA: u32 = ((EC_CODE_BITS - 2) % EC_SYM_BITS) + 1;  // 7
const EC_UINT_BITS: u32 = 8;
const EC_WINDOW_SIZE: u32 = 32;  // sizeof(u32) * 8

/// Range decoder state
pub struct RangeDecoder<'a> {
    buf: &'a [u8],
    storage: usize,
    end_offs: usize,
    end_window: u32,
    nend_bits: i32,
    nbits_total: i32,
    offs: usize,
    rng: u32,
    val: u32,
    ext: u32,
    rem: i32,
    error: bool,
}

impl<'a> RangeDecoder<'a> {
    /// Initialize decoder - ec_dec_init()
    pub fn new(buf: &'a [u8]) -> Result<Self> {
        let storage = buf.len();
        let mut decoder = Self {
            buf,
            storage,
            end_offs: 0,
            end_window: 0,
            nend_bits: 0,
            nbits_total: (EC_CODE_BITS + 1 - ((EC_CODE_BITS - EC_CODE_EXTRA) / EC_SYM_BITS) * EC_SYM_BITS) as i32,
            offs: 0,
            rng: 1u32 << EC_CODE_EXTRA,
            val: 0,
            ext: 0,
            rem: 0,
            error: false,
        };

        decoder.rem = decoder.read_byte();
        decoder.val = decoder.rng - 1 - ((decoder.rem as u32) >> (EC_SYM_BITS - EC_CODE_EXTRA));
        decoder.normalize();

        Ok(decoder)
    }

    #[inline]
    fn read_byte(&mut self) -> i32 {
        if self.offs < self.storage {
            let byte = self.buf[self.offs];
            self.offs += 1;
            byte as i32
        } else {
            0
        }
    }

    #[inline]
    fn read_byte_from_end(&mut self) -> i32 {
        if self.end_offs < self.storage {
            self.end_offs += 1;
            self.buf[self.storage - self.end_offs] as i32
        } else {
            0
        }
    }

    /// Normalize - ec_dec_normalize()
    #[inline]
    fn normalize(&mut self) {
        while self.rng <= EC_CODE_BOT {
            self.nbits_total += EC_SYM_BITS as i32;
            self.rng <<= EC_SYM_BITS;

            let sym = self.rem;
            self.rem = self.read_byte();

            let combined = ((sym << EC_SYM_BITS as i32) | self.rem) >> (EC_SYM_BITS - EC_CODE_EXTRA);
            self.val = ((self.val << EC_SYM_BITS) + (EC_SYM_MAX & !(combined as u32))) & (EC_CODE_TOP - 1);
        }
    }

    /// Decode symbol - ec_decode()
    pub fn decode(&mut self, ft: u32) -> u32 {
        if ft == 0 {
            return 0;
        }
        self.ext = self.rng / ft;
        let s = self.val / self.ext;
        let clamped = s.min(ft);
        if clamped >= ft {
            0
        } else {
            ft.wrapping_sub(clamped).wrapping_sub(1)
        }
    }

    /// Decode binary - ec_decode_bin()
    pub fn decode_bin(&mut self, bits: u32) -> u32 {
        if bits >= 32 {
            return 0;
        }
        self.ext = self.rng >> bits;
        let s = self.val / self.ext;
        let ft = 1u32 << bits;
        let clamped = s.min(ft);
        if clamped >= ft {
            0
        } else {
            ft.wrapping_sub(clamped).wrapping_sub(1)
        }
    }

    /// Update state - ec_dec_update()
    pub fn update(&mut self, fl: u32, fh: u32, ft: u32) {
        let s = self.ext.wrapping_mul(ft.wrapping_sub(fh));
        self.val = self.val.wrapping_sub(s);
        self.rng = if fl > 0 {
            self.ext.wrapping_mul(fh.wrapping_sub(fl))
        } else {
            self.rng.wrapping_sub(s)
        };
        self.normalize();
    }

    /// Decode bit with log probability - ec_dec_bit_logp()
    pub fn decode_bit_logp(&mut self, logp: u32) -> bool {
        let r = self.rng;
        let d = self.val;
        let s = r >> logp;
        let ret = d < s;

        if ret {
            self.rng = s;
        } else {
            self.val = d - s;
            self.rng = r - s;
        }

        self.normalize();
        ret
    }

    /// Decode using ICDF table - ec_dec_icdf()
    /// Table contains cumulative frequencies in DESCENDING order
    pub fn decode_icdf(&mut self, icdf: &[u8], ftb: u32) -> usize {
        let mut s = self.rng;
        let d = self.val;
        let r = s >> ftb;
        let mut ret = 0usize;
        let mut t;

        loop {
            t = s;
            if ret >= icdf.len() {
                break;
            }
            s = r * (icdf[ret] as u32);
            if d < s {
                break;
            }
            ret += 1;
        }

        self.val = d - s;
        self.rng = t - s;
        self.normalize();

        ret
    }

    /// Decode unsigned integer - ec_dec_uint()
    pub fn decode_uint(&mut self, mut ft: u32) -> u32 {
        if ft <= 1 {
            return 0;
        }

        ft -= 1;
        let mut ftb = 0u32;
        let mut tmp = ft;
        while tmp > 0 {
            ftb += 1;
            tmp >>= 1;
        }

        if ftb > EC_UINT_BITS {
            let ftb2 = ftb - EC_UINT_BITS;
            let ft2 = ((ft >> ftb2) + 1).max(1);
            let s = self.decode(ft2);
            self.update(s, s + 1, ft2);
            let t = (s << ftb2) | self.decode_bits(ftb2);
            if t <= ft {
                return t;
            }
            self.error = true;
            ft
        } else {
            let ft2 = ft + 1;
            let s = self.decode(ft2);
            self.update(s, s + 1, ft2);
            s
        }
    }

    /// Read raw bits from end - ec_dec_bits()
    pub fn decode_bits(&mut self, bits: u32) -> u32 {
        let mut window = self.end_window;
        let mut available = self.nend_bits;

        while (available as u32) < bits {
            if available as u32 > EC_WINDOW_SIZE - EC_SYM_BITS {
                break;
            }
            window |= (self.read_byte_from_end() as u32) << available;
            available += EC_SYM_BITS as i32;
        }

        let ret = window & ((1u32 << bits) - 1);
        window >>= bits;
        available -= bits as i32;

        self.end_window = window;
        self.nend_bits = available;
        self.nbits_total += bits as i32;

        ret
    }

    /// Get total bits read (fractional)
    pub fn tell_frac(&self) -> u32 {
        (self.nbits_total - self.nend_bits) as u32
    }

    /// Get number of bits available for reading
    pub fn bits_left(&self) -> u32 {
        let bytes_left = self.storage.saturating_sub(self.offs).saturating_sub(self.end_offs);
        (bytes_left as u32) * 8 + (self.nend_bits as u32)
    }

    /// Get bytes available
    pub fn available(&self) -> usize {
        self.storage.saturating_sub(self.offs).saturating_sub(self.end_offs)
    }

    /// Check for errors
    pub fn has_error(&self) -> bool {
        self.error
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decoder_init() {
        let data = vec![0x0b, 0xe4, 0xc1, 0x36];
        let decoder = RangeDecoder::new(&data).unwrap();
        assert!(!decoder.has_error());
        assert_eq!(decoder.rng, 1u32 << EC_CODE_EXTRA);
    }
}
