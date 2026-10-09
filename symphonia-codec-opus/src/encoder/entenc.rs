// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Range encoder with a fixed-size output buffer. Ported from libopus `celt/entenc.c`
//! (BSD-3-Clause, see NOTICE).
//!
//! Unlike the growable, `#[cfg(test)]`-only encoder in [`crate::range`], this one mirrors the C
//! `ec_enc` exactly: the range-coded bytes are written from the front of a buffer of a fixed
//! `storage` size, the raw bits ([`RangeEncoder::enc_bits`]) from its back, and
//! [`RangeEncoder::shrink`] can move the raw-bit tail when the final packet size is decided late
//! (VBR). That is required to emit CBR packets of an exact size and to share the buffer layout
//! the decoder expects.

use crate::range::BITRES;

const EC_SYM_BITS: u32 = 8;
const EC_CODE_BITS: u32 = 32;
const EC_SYM_MAX: u32 = (1 << EC_SYM_BITS) - 1;
const EC_CODE_SHIFT: u32 = EC_CODE_BITS - EC_SYM_BITS - 1;
const EC_CODE_TOP: u32 = 1u32 << (EC_CODE_BITS - 1);
const EC_CODE_BOT: u32 = EC_CODE_TOP >> EC_SYM_BITS;
const EC_UINT_BITS: u32 = 8;
const EC_WINDOW_SIZE: i32 = 32;

#[inline]
fn ilog(v: u32) -> i32 {
    32 - v.leading_zeros() as i32
}

/// A range encoder writing into a fixed-size buffer. C: `ec_enc`.
#[derive(Clone)]
pub(crate) struct RangeEncoder {
    buf: Vec<u8>,
    storage: u32,
    end_offs: u32,
    end_window: u32,
    nend_bits: i32,
    nbits_total: i32,
    offs: u32,
    rng: u32,
    val: u32,
    ext: u32,
    rem: i32,
    error: bool,
}

impl RangeEncoder {
    /// C: `ec_enc_init`. `size` is the packet payload size in bytes.
    pub fn new(size: usize) -> Self {
        RangeEncoder {
            buf: vec![0u8; size],
            storage: size as u32,
            end_offs: 0,
            end_window: 0,
            nend_bits: 0,
            nbits_total: (EC_CODE_BITS + 1) as i32,
            offs: 0,
            rng: EC_CODE_TOP,
            val: 0,
            ext: 0,
            rem: -1,
            error: false,
        }
    }

    fn write_byte(&mut self, value: u32) {
        if self.offs + self.end_offs >= self.storage {
            self.error = true;
            return;
        }
        self.buf[self.offs as usize] = value as u8;
        self.offs += 1;
    }

    fn write_byte_at_end(&mut self, value: u32) {
        if self.offs + self.end_offs >= self.storage {
            self.error = true;
            return;
        }
        self.end_offs += 1;
        self.buf[(self.storage - self.end_offs) as usize] = value as u8;
    }

    /// C: `ec_enc_carry_out`.
    fn carry_out(&mut self, c: i32) {
        if c as u32 != EC_SYM_MAX {
            let carry = c >> EC_SYM_BITS;
            if self.rem >= 0 {
                self.write_byte((self.rem + carry) as u32);
            }
            if self.ext > 0 {
                let sym = (EC_SYM_MAX as i32 + carry) as u32 & EC_SYM_MAX;
                while self.ext > 0 {
                    self.write_byte(sym);
                    self.ext -= 1;
                }
            }
            self.rem = c & EC_SYM_MAX as i32;
        }
        else {
            self.ext += 1;
        }
    }

    #[inline]
    fn normalize(&mut self) {
        while self.rng <= EC_CODE_BOT {
            self.carry_out((self.val >> EC_CODE_SHIFT) as i32);
            self.val = (self.val << EC_SYM_BITS) & (EC_CODE_TOP - 1);
            self.rng <<= EC_SYM_BITS;
            self.nbits_total += EC_SYM_BITS as i32;
        }
    }

    /// C: `ec_encode`.
    pub fn encode(&mut self, fl: u32, fh: u32, ft: u32) {
        let r = self.rng / ft;
        if fl > 0 {
            self.val = self.val.wrapping_add(self.rng - r.wrapping_mul(ft - fl));
            self.rng = r.wrapping_mul(fh - fl);
        }
        else {
            self.rng -= r.wrapping_mul(ft - fh);
        }
        self.normalize();
    }

    /// C: `ec_encode_bin`.
    pub fn encode_bin(&mut self, fl: u32, fh: u32, bits: u32) {
        let r = self.rng >> bits;
        if fl > 0 {
            self.val = self.val.wrapping_add(self.rng - r.wrapping_mul((1u32 << bits) - fl));
            self.rng = r.wrapping_mul(fh - fl);
        }
        else {
            self.rng -= r.wrapping_mul((1u32 << bits) - fh);
        }
        self.normalize();
    }

    /// C: `ec_enc_bit_logp`. The probability of a one is `1/(1<<logp)`.
    pub fn enc_bit_logp(&mut self, val: bool, logp: u32) {
        let r = self.rng;
        let l = self.val;
        let s = r >> logp;
        let r = r - s;
        if val {
            self.val = l.wrapping_add(r);
        }
        self.rng = if val { s } else { r };
        self.normalize();
    }

    /// C: `ec_enc_icdf`.
    pub fn enc_icdf(&mut self, s: i32, icdf: &[u8], ftb: u32) {
        let r = self.rng >> ftb;
        if s > 0 {
            let prev = icdf[(s - 1) as usize] as u32;
            self.val = self.val.wrapping_add(self.rng - r.wrapping_mul(prev));
            self.rng = r.wrapping_mul(prev - icdf[s as usize] as u32);
        }
        else {
            self.rng -= r.wrapping_mul(icdf[s as usize] as u32);
        }
        self.normalize();
    }

    /// C: `ec_enc_uint`.
    pub fn enc_uint(&mut self, fl: u32, ft: u32) {
        assert!(ft > 1);
        let ft = ft - 1;
        let ftb = ilog(ft);
        if ftb > EC_UINT_BITS as i32 {
            let ftb = (ftb - EC_UINT_BITS as i32) as u32;
            let ft_s = (ft >> ftb) + 1;
            let fl_s = fl >> ftb;
            self.encode(fl_s, fl_s + 1, ft_s);
            self.enc_bits(fl & ((1u32 << ftb) - 1), ftb);
        }
        else {
            self.encode(fl, fl + 1, ft + 1);
        }
    }

    /// C: `ec_enc_bits`. Writes `bits` raw bits (`1..=25`) to the tail of the buffer.
    pub fn enc_bits(&mut self, fl: u32, bits: u32) {
        debug_assert!(bits > 0 && bits <= 25);
        let mut window = self.end_window;
        let mut used = self.nend_bits;
        if used + bits as i32 > EC_WINDOW_SIZE {
            loop {
                self.write_byte_at_end(window & EC_SYM_MAX);
                window >>= EC_SYM_BITS;
                used -= EC_SYM_BITS as i32;
                if used < EC_SYM_BITS as i32 {
                    break;
                }
            }
        }
        window |= fl << used;
        used += bits as i32;
        self.end_window = window;
        self.nend_bits = used;
        self.nbits_total += bits as i32;
    }

    /// C: `ec_enc_shrink`. Moves the raw-bit tail so that the packet is `size` bytes long.
    pub fn shrink(&mut self, size: usize) {
        let size = size as u32;
        assert!(self.offs + self.end_offs <= size);
        if size != self.storage {
            let end = self.end_offs as usize;
            let src = (self.storage - self.end_offs) as usize;
            let dst = (size - self.end_offs) as usize;
            if size > self.storage {
                self.buf.resize(size as usize, 0);
            }
            self.buf.copy_within(src..src + end, dst);
            self.storage = size;
        }
    }

    /// C: `ec_tell`. Whole bits used so far (a slight over-estimate).
    pub fn tell(&self) -> i32 {
        self.nbits_total - ilog(self.rng)
    }

    /// C: `ec_tell_frac`. As [`Self::tell`] but in `1/(1<<BITRES)` bit units.
    pub fn tell_frac(&self) -> i32 {
        const CORRECTION: [u32; 8] = [35733, 38967, 42495, 46340, 50535, 55109, 60097, 65535];
        let nbits = (self.nbits_total as u32) << BITRES;
        let l = ilog(self.rng);
        let r = self.rng >> (l - 16);
        let mut b = (r >> 12).wrapping_sub(8);
        b += (r > CORRECTION[b as usize]) as u32;
        let l = ((l as u32) << 3) + b;
        (nbits - l) as i32
    }

    /// C: `enc->nbits_total += tell - ec_tell(enc)`. Makes the coder believe `tell` bits have
    /// been used, so every later budget check sees the packet as full (used for silent frames).
    pub fn force_tell(&mut self, tell: i32) {
        self.nbits_total += tell - self.tell();
    }

    /// C: `enc->rng`, the current range; after the last symbol it is the "final range" that the
    /// decoder reproduces (a cheap end-to-end consistency check).
    pub fn rng(&self) -> u32 {
        self.rng
    }

    /// C: `enc->storage`, the current packet size in bytes.
    pub fn storage(&self) -> u32 {
        self.storage
    }

    /// C: `ec_enc_done`. Flushes the coder, zero-fills the gap between the range-coded head and
    /// the raw-bit tail, and returns the finished packet payload (`storage` bytes).
    pub fn done(mut self) -> (Vec<u8>, bool) {
        // Output the minimum number of bits that ensures the symbols encoded so far decode
        // correctly regardless of the bits that follow.
        let mut l = EC_CODE_BITS as i32 - ilog(self.rng);
        let mut msk = (EC_CODE_TOP - 1) >> l;
        let mut end = self.val.wrapping_add(msk) & !msk;
        if (end | msk) >= self.val.wrapping_add(self.rng) {
            l += 1;
            msk >>= 1;
            end = self.val.wrapping_add(msk) & !msk;
        }
        while l > 0 {
            self.carry_out((end >> EC_CODE_SHIFT) as i32);
            end = (end << EC_SYM_BITS) & (EC_CODE_TOP - 1);
            l -= EC_SYM_BITS as i32;
        }
        // Flush a buffered byte.
        if self.rem >= 0 || self.ext > 0 {
            self.carry_out(0);
        }
        // Flush buffered extra bits.
        let mut window = self.end_window;
        let mut used = self.nend_bits;
        while used >= EC_SYM_BITS as i32 {
            self.write_byte_at_end(window & EC_SYM_MAX);
            window >>= EC_SYM_BITS;
            used -= EC_SYM_BITS as i32;
        }
        // Clear any excess space and add any remaining extra bits to the last byte.
        if !self.error {
            let lo = self.offs as usize;
            let hi = (self.storage - self.end_offs) as usize;
            self.buf[lo..hi].fill(0);
            if used > 0 {
                if self.end_offs >= self.storage {
                    self.error = true;
                }
                else {
                    let l = -l;
                    // If we've busted, don't add too many extra bits to the last byte; it would
                    // corrupt the range coder data, and that's more important.
                    if self.offs + self.end_offs >= self.storage && l < used {
                        window &= (1u32 << l) - 1;
                        self.error = true;
                    }
                    self.buf[(self.storage - self.end_offs - 1) as usize] |= window as u8;
                }
            }
        }
        self.buf.truncate(self.storage as usize);
        (self.buf, self.error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::range::RangeDecoder;

    #[test]
    fn mixed_symbols_round_trip_through_decoder() {
        let mut enc = RangeEncoder::new(64);
        let icdf = [25u8, 23, 2, 0];
        enc.enc_bit_logp(true, 3);
        enc.enc_icdf(2, &icdf, 5);
        enc.enc_uint(777, 1000);
        enc.enc_bits(0x155, 9);
        enc.encode(3, 4, 11);
        enc.enc_bit_logp(false, 1);
        enc.enc_uint(5, 7);
        enc.enc_bits(1, 1);
        let before = enc.tell();
        assert!(before > 0 && before < 64 * 8);
        let (buf, err) = enc.done();
        assert!(!err);
        assert_eq!(buf.len(), 64);

        let mut dec = RangeDecoder::new(&buf);
        assert!(dec.dec_bit_logp(3));
        assert_eq!(dec.dec_icdf(&icdf, 5), 2);
        assert_eq!(dec.dec_uint(1000), 777);
        assert_eq!(dec.dec_bits(9), 0x155);
        let s = dec.decode(11);
        assert_eq!(s, 3);
        dec.update(3, 4, 11);
        assert!(!dec.dec_bit_logp(1));
        assert_eq!(dec.dec_uint(7), 5);
        assert_eq!(dec.dec_bits(1), 1);
    }

    #[test]
    fn shrink_moves_raw_bits() {
        let mut enc = RangeEncoder::new(100);
        enc.enc_uint(12345, 60000);
        enc.enc_bits(0xABCDE, 20);
        enc.enc_bits(0x3, 2);
        enc.shrink(20);
        assert_eq!(enc.storage(), 20);
        let (buf, err) = enc.done();
        assert!(!err);
        assert_eq!(buf.len(), 20);
        let mut dec = RangeDecoder::new(&buf);
        assert_eq!(dec.dec_uint(60000), 12345);
        assert_eq!(dec.dec_bits(20), 0xABCDE);
        assert_eq!(dec.dec_bits(2), 0x3);
    }

    #[test]
    fn tell_matches_reference_test_encoder() {
        use crate::range::test_encoder::RangeEncoder as Reference;
        let mut a = RangeEncoder::new(256);
        let mut b = Reference::new();
        for i in 0..100u32 {
            let ft = 3 + (i * 7) % 200;
            let fl = i % ft;
            a.encode(fl, fl + 1, ft);
            b.encode(fl, fl + 1, ft);
            assert_eq!(a.tell(), b.tell());
            assert_eq!(a.tell_frac() as u32, b.tell_frac());
        }
        a.enc_bits(0x1f, 5);
        b.enc_bits(0x1f, 5);
        assert_eq!(a.tell(), b.tell());
    }

    #[test]
    fn overflow_sets_error() {
        let mut enc = RangeEncoder::new(2);
        for i in 0..40u32 {
            enc.encode(i % 5, i % 5 + 1, 5);
        }
        let (_, err) = enc.done();
        assert!(err);
    }
}
