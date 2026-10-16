// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The QDesign Music Codec 1 (QDMC) decoder.
//!
//! QDMC is an undocumented codec. This is a port of FFmpeg's `libavcodec/qdmc.c` (see `NOTICE`),
//! including its behaviours, so that the output matches FFmpeg's. Names of the original are kept
//! where practical.

use symphonia_core::dsp::complex::Complex;
use symphonia_core::dsp::fft::Ifft;
use symphonia_core::errors::{Result, decode_error, unsupported_error};

use crate::bits::{Bits, Vlc};
use crate::qdm2::parse_extra_data;
use crate::qdmc_tables::*;

/// The number of bits of the frame counter of one packet, chosen from the sample rate.
fn frame_bits_for(sample_rate: u32) -> u32 {
    if sample_rate >= 32000 {
        13
    }
    else if sample_rate >= 16000 {
        12
    }
    else {
        11
    }
}

/// A QDMC tone (`QDMCTone`).
#[derive(Clone, Copy, Default)]
struct Tone {
    mode: u8,
    phase: u8,
    offset: usize,
    freq: i16,
    amplitude: i16,
}

/// The maximum number of tones per group (`FF_ARRAY_ELEMS(s->tones[group])`).
const MAX_TONES_PER_GROUP: usize = 8192;

/// The sine table of the decoder (`sin_table`), 512 samples of a sine wave.
fn make_sin_table() -> Vec<f32> {
    (0..512).map(|i| (2.0 * std::f64::consts::PI * i as f64 * 0.001953125).sin() as f32).collect()
}

pub(crate) struct Qdmc {
    // Parameters from the extra data.
    channels: usize,
    sample_rate: u32,
    checksum_size: usize,

    // Parameters built from the extra data.
    frame_bits: u32,
    frame_size: usize,
    subframe_size: usize,
    band_index: usize,
    ifft_size: usize,

    // Decoder state (`QDMCContext`).
    noise: [[[u8; 17]; 19]; 2],
    tones: Vec<Vec<Tone>>,
    nb_tones: [usize; 5],
    cur_tone: [usize; 5],
    alt_sin: Vec<Vec<f32>>,
    fft_buffer: [Vec<f32>; 4],
    noise2_buffer: Vec<f32>,
    noise_buffer: Vec<f32>,
    buffer: Vec<f32>,
    buffer_offset: usize,
    fft_offset: usize,
    rndval: u32,

    cmplx_in: [Vec<Complex<f32>>; 2],
    cmplx_out: [Vec<Complex<f32>>; 2],
    ifft: Ifft,
    sin_table: Vec<f32>,

    vlcs: [Vlc; 6],
}

impl Qdmc {
    pub(crate) fn new(extra: &[u8]) -> Result<Box<Self>> {
        let p = parse_extra_data(extra, b"QDMC")?;

        if p.channels == 0 || p.channels > 2 {
            return decode_error("qdesign: invalid number of channels");
        }
        let channels = p.channels as usize;

        if p.checksum_size >= 1 << 28 {
            return decode_error("qdesign: data block size too large");
        }

        let frame_bits = frame_bits_for(p.sample_rate);
        let frame_size = 1usize << frame_bits;
        let subframe_size = frame_size >> 5;

        let fft_order = p.fft_size.ilog2() as usize + 1;
        if !(7..=9).contains(&fft_order) {
            return unsupported_error("qdesign: unsupported FFT order");
        }
        if p.fft_size as usize != 1 << (fft_order - 1) {
            return decode_error("qdesign: FFT size not a power of two");
        }

        // The bit-rate dependent band index, as `qdmc_decode_init`.
        let x = if p.sample_rate >= 32000 {
            28000
        }
        else if p.sample_rate >= 16000 {
            20000
        }
        else {
            16000
        };
        let x = if channels == 2 { 3 * x / 2 } else { x };
        let sel = ((p.bit_rate as f64 * 3.0 / x as f64 + 0.5).floor() as i64).clamp(0, 6) as usize;
        let band_index = NOISE_BANDS_SELECTOR[sel];

        let ifft_size = 1usize << fft_order;
        let sin_table = make_sin_table();

        // The `alt_sin` table (`qdmc_decode_init`).
        let mut alt_sin = vec![Vec::new(); 5];
        for g in 1..=5u32 {
            for j in 0..(1usize << g) - 1 {
                alt_sin[(5 - g) as usize].push(sin_table[((j + 1) << (8 - g)) & 0x1FF]);
            }
        }

        // The prefix codes (`qdmc_init_static_data`).
        let vlcs = std::array::from_fn(|i| {
            let start = HUFF_SIZES[..i].iter().sum::<usize>();
            Vlc::new(&QDMC_HUFFTAB[start..start + HUFF_SIZES[i]])
        })
        .map(Result::unwrap);

        // The noise envelope (`make_noises`).
        let mut noise_buffer = vec![0.0f32; 4096 * 2];
        for j in 0..NOISE_BANDS_SIZE[band_index] {
            let n0 = QDMC_NODES[j + 21 * band_index] as usize;
            let n1 = QDMC_NODES[j + 21 * band_index + 1] as usize;
            let n2 = QDMC_NODES[j + 21 * band_index + 2] as usize;

            let mut nptr = 256 * j;
            for i in 0..n1 - n0 {
                noise_buffer[nptr] = i as f32 / (n1 - n0) as f32;
                nptr += 1;
            }

            let mut diff = n2 - n1;
            nptr = (j << 8) + n1 - n0;
            for _i in n1..n2 {
                noise_buffer[nptr] = diff as f32 / (n2 - n1) as f32;
                nptr += 1;
                diff -= 1;
            }
        }

        Ok(Box::new(Qdmc {
            channels,
            sample_rate: p.sample_rate,
            checksum_size: p.checksum_size as usize,
            frame_bits,
            frame_size,
            subframe_size,
            band_index,
            ifft_size,
            noise: [[[0; 17]; 19]; 2],
            tones: vec![Vec::new(); 5],
            nb_tones: [0; 5],
            cur_tone: [0; 5],
            alt_sin,
            fft_buffer: [
                vec![0.0f32; 8192 * 2],
                vec![0.0f32; 8192 * 2],
                vec![0.0f32; 8192 * 2],
                vec![0.0f32; 8192 * 2],
            ],
            noise2_buffer: vec![0.0f32; 4096 * 2],
            noise_buffer,
            buffer: vec![0.0f32; 2 * 32768 * 2],
            buffer_offset: 0,
            fft_offset: 0,
            rndval: 0,
            cmplx_in: [vec![Complex::default(); ifft_size], vec![Complex::default(); ifft_size]],
            cmplx_out: [vec![Complex::default(); ifft_size], vec![Complex::default(); ifft_size]],
            ifft: Ifft::new(ifft_size),
            sin_table,
            vlcs,
        }))
    }

    pub(crate) fn channels(&self) -> usize {
        self.channels
    }

    pub(crate) fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The number of samples per channel decoded from each block.
    pub(crate) fn frames_per_block(&self) -> usize {
        self.frame_size
    }

    /// The size of a block in bytes.
    pub(crate) fn block_size(&self) -> usize {
        self.checksum_size
    }

    /// Decodes a block (`qdmc_decode_frame`) of `block_size()` bytes. Interleaved samples are
    /// appended to `out`.
    pub(crate) fn decode_block(&mut self, buf: &[u8], out: &mut Vec<i16>) -> Result<()> {
        if buf.len() < self.checksum_size {
            return decode_error("qdesign: packet shorter than a block");
        }

        let mut bits = Bits::with_size(buf, self.checksum_size);

        self.fft_offset = self.frame_size - self.fft_offset;

        self.decode_frame(&mut bits, out)
    }

    /// The VLC accessor (`qdmc_get_vlc`).
    fn get_vlc(&self, bits: &mut Bits<'_>, table: usize, flag: bool) -> Result<i32> {
        if bits.bits_left() < 1 {
            return decode_error("qdesign: invalid data");
        }

        let mut v = self.vlcs[table].get(bits);
        if v < 0 {
            let n = bits.get(3) + 1;
            v = bits.get(n) as i32;
        }

        if flag {
            if !(0..CODE_PREFIX.len() as i32).contains(&v) {
                return decode_error("qdesign: invalid prefix code");
            }
            v = CODE_PREFIX[v as usize] as i32 + bits.get((v as u32) >> 2) as i32;
        }

        Ok(v)
    }

    /// Checks the packet label and checksum (`skip_label`).
    fn skip_label(&mut self, bits: &mut Bits<'_>) -> Result<()> {
        let label = bits.get(32);
        let checksum = bits.get(16);
        // `sum` is a `uint16_t` in FFmpeg, so the accumulation wraps modulo 2^16.
        let sum = (226u32 + bits.sum_bytes(6, self.checksum_size)) & 0xffff;

        if label != u32::from_le_bytes(*b"QMC\x01") {
            return decode_error("qdesign: invalid label");
        }
        if sum != checksum {
            return decode_error("qdesign: invalid checksum");
        }

        Ok(())
    }

    fn read_noise_data(&mut self, bits: &mut Bits<'_>) -> Result<()> {
        for ch in 0..self.channels {
            for band in 0..NOISE_BANDS_SIZE[self.band_index] {
                let mut v = self.get_vlc(bits, 0, false)?;

                v = if v & 1 != 0 { v + 1 } else { -v };

                let mut lastval = v / 2;
                self.noise[ch][band][0] = (lastval - 1) as u8;

                let mut j = 0usize;
                while j < 15 {
                    let mut len = self.get_vlc(bits, 1, true)?;
                    len += 1;

                    v = self.get_vlc(bits, 0, false)?;

                    let newval = if v & 1 != 0 { lastval + (v + 1) / 2 } else { lastval - v / 2 };

                    let idx = j + 1;
                    if len as usize + idx > 16 {
                        return decode_error("qdesign: invalid data");
                    }

                    for (k, idx) in (idx..=j + len as usize).enumerate().map(|(k, i)| (k + 1, i)) {
                        self.noise[ch][band][idx] =
                            (lastval + k as i32 * (newval - lastval) / len - 1) as u8;
                    }

                    lastval = newval;
                    j += len as usize;
                }
            }
        }

        Ok(())
    }

    fn add_tone(
        &mut self,
        group: usize,
        offset: usize,
        freq: i16,
        stereo_mode: u8,
        amplitude: i16,
        phase: u8,
    ) {
        let index = self.nb_tones[group];
        if index >= MAX_TONES_PER_GROUP {
            return;
        }
        if self.tones[group].len() <= index {
            self.tones[group].push(Tone::default());
        }
        self.tones[group][index] = Tone { offset, freq, mode: stereo_mode, amplitude, phase };
        self.nb_tones[group] += 1;
    }

    fn read_wave_data(&mut self, bits: &mut Bits<'_>) -> Result<()> {
        for group in 0..5usize {
            let group_size = 1usize << (self.frame_bits as usize - group - 1);
            let group_bits = 4 - group;
            let mut pos2 = 0usize;
            let mut off = 0usize;

            let mut i = 1i32;
            loop {
                let v = self.get_vlc(bits, 3, true)?;

                let mut freq = i + v;
                i = freq + 1;
                while freq >= group_size as i32 - 1 {
                    freq += 2 - group_size as i32;
                    pos2 += group_size;
                    off += 1 << group_bits;
                }

                if pos2 >= self.frame_size {
                    break;
                }

                let mut stereo_mode = 0u8;
                if self.channels > 1 {
                    stereo_mode = bits.get(2) as u8;
                }

                let amp = self.get_vlc(bits, 2, false)?;
                let phase = bits.get(3);

                let mut amp2 = 0i32;
                let mut phase2 = 0i32;
                if stereo_mode > 1 {
                    amp2 = self.get_vlc(bits, 4, false)?;
                    amp2 = amp - amp2;

                    phase2 = self.get_vlc(bits, 5, false)?;
                    phase2 = phase as i32 - phase2;
                    if phase2 < 0 {
                        phase2 += 8;
                    }
                }

                if (freq as usize >> group_bits) + 1 < self.subframe_size {
                    self.add_tone(
                        group,
                        off,
                        freq as i16,
                        stereo_mode & 1,
                        amp as i16,
                        phase as u8,
                    );
                    if stereo_mode > 1 {
                        self.add_tone(
                            group,
                            off,
                            freq as i16,
                            !stereo_mode & 1,
                            amp2 as i16,
                            phase2 as u8,
                        );
                    }
                }
            }
        }

        Ok(())
    }

    fn lin_calc(&mut self, amplitude: f32, node1: usize, node2: usize, index: usize) {
        let scale = 0.5 * amplitude;
        let mut subframe_size = self.subframe_size;
        if subframe_size >= node2 {
            subframe_size = node2;
        }
        let length = (subframe_size - node1) & 0xFFFC;
        let mut j = node1;
        let mut noise_ptr = 256 * index;

        for _ in (0..length).step_by(4) {
            self.noise2_buffer[j] += scale * self.noise_buffer[noise_ptr];
            self.noise2_buffer[j + 1] += scale * self.noise_buffer[noise_ptr + 1];
            self.noise2_buffer[j + 2] += scale * self.noise_buffer[noise_ptr + 2];
            self.noise2_buffer[j + 3] += scale * self.noise_buffer[noise_ptr + 3];
            j += 4;
            noise_ptr += 4;
        }

        let k_start = length + node1;
        let noise_ptr_start = length + (index << 8);
        let count = (subframe_size - node1) - length;
        for (k, noise_ptr) in
            (k_start..k_start + count).zip(noise_ptr_start..noise_ptr_start + count)
        {
            self.noise2_buffer[k] += scale * self.noise_buffer[noise_ptr];
        }
    }

    fn add_noise(&mut self, ch: usize, current_subframe: usize) {
        let base = self.fft_offset + self.subframe_size * current_subframe;

        self.noise2_buffer.fill(0.0);

        for i in 0..NOISE_BANDS_SIZE[self.band_index] {
            if QDMC_NODES[i + 21 * self.band_index] as usize > self.subframe_size - 1 {
                break;
            }

            let aindex = self.noise[ch][i][current_subframe / 2] as i32;
            let amplitude = if aindex > 0 { AMPLITUDE_TAB[(aindex & 0x3F) as usize] } else { 0.0 };

            self.lin_calc(
                amplitude,
                QDMC_NODES[21 * self.band_index + i] as usize,
                QDMC_NODES[21 * self.band_index + i + 2] as usize,
                i,
            );
        }

        for j in 2..self.subframe_size - 1 {
            self.rndval = 214013u32.wrapping_mul(self.rndval).wrapping_add(2531011);
            let rnd_im =
                ((self.rndval & 0x7FFF) as f32 - 16384.0) * 0.000030517578 * self.noise2_buffer[j];
            self.rndval = 214013u32.wrapping_mul(self.rndval).wrapping_add(2531011);
            let rnd_re =
                ((self.rndval & 0x7FFF) as f32 - 16384.0) * 0.000030517578 * self.noise2_buffer[j];

            self.fft_buffer[ch][base + j] += rnd_im;
            self.fft_buffer[2 + ch][base + j] += rnd_re;
            self.fft_buffer[ch][base + j + 1] -= rnd_im;
            self.fft_buffer[2 + ch][base + j + 1] -= rnd_re;
        }
    }

    fn add_wave(
        &mut self,
        offset: usize,
        freqs: i16,
        group: usize,
        stereo_mode: u8,
        amp: i16,
        phase: u8,
    ) {
        let stereo_mode = if self.channels == 1 { 0 } else { stereo_mode as usize };
        let group_bits = 4 - group;
        let pos = (freqs as usize) >> (4 - group);
        let amplitude = AMPLITUDE_TAB[(amp as usize) & 0x3F];
        let mut imptr = self.fft_offset + self.subframe_size * offset + pos;
        let mut reptr = imptr;
        let mut pindex = ((phase as i32) << 6) - ((2 * (freqs as i32 >> (4 - group)) + 1) << 7);

        for j in 0..(1usize << (group_bits + 1)) - 1 {
            pindex += (2 * freqs as i32 + 1) << (7 - group_bits);
            let level = amplitude * self.alt_sin[group][j];
            let im = level * self.sin_table[(pindex & 0x1FF) as usize];
            let re = level * self.sin_table[((pindex + 128) & 0x1FF) as usize];

            let (im_half, re_half) = self.fft_buffer.split_at_mut(2);
            let im_buf = &mut im_half[stereo_mode];
            let re_buf = &mut re_half[stereo_mode];
            im_buf[imptr] += im;
            im_buf[imptr + 1] -= im;
            re_buf[reptr] += re;
            re_buf[reptr + 1] -= re;
            imptr += self.subframe_size;
            reptr += self.subframe_size;

            if imptr >= 2 * self.frame_size {
                imptr = pos;
                reptr = pos;
            }
        }
    }

    fn add_wave0(&mut self, offset: usize, freqs: i16, stereo_mode: u8, amp: i16, phase: u8) {
        let stereo_mode = if self.channels == 1 { 0 } else { stereo_mode as usize };
        let level = AMPLITUDE_TAB[(amp as usize) & 0x3F];
        let im = level * self.sin_table[(((phase as i32) << 6) & 0x1FF) as usize];
        let re = level * self.sin_table[((((phase as i32) << 6) + 128) & 0x1FF) as usize];
        let pos = self.fft_offset + freqs as usize + self.subframe_size * offset;

        let (im_half, re_half) = self.fft_buffer.split_at_mut(2);
        let im_buf = &mut im_half[stereo_mode];
        let re_buf = &mut re_half[stereo_mode];
        im_buf[pos] += im;
        re_buf[pos] += re;
        im_buf[pos + 1] -= im;
        re_buf[pos + 1] -= re;
    }

    fn add_waves(&mut self, current_subframe: usize) {
        for g in 0..4usize {
            let mut w = self.cur_tone[g];
            while w < self.nb_tones[g] {
                let t = self.tones[g][w];
                if current_subframe < t.offset {
                    break;
                }
                self.add_wave(t.offset, t.freq, g, t.mode, t.amplitude, t.phase);
                w += 1;
            }
            self.cur_tone[g] = w;
        }
        let mut w = self.cur_tone[4];
        while w < self.nb_tones[4] {
            let t = self.tones[4][w];
            if current_subframe < t.offset {
                break;
            }
            self.add_wave0(t.offset, t.freq, t.mode, t.amplitude, t.phase);
            w += 1;
        }
        self.cur_tone[4] = w;
    }

    fn decode_frame(&mut self, bits: &mut Bits<'_>, out: &mut Vec<i16>) -> Result<()> {
        self.skip_label(bits)?;

        self.nb_tones.fill(0);
        self.cur_tone.fill(0);

        self.read_noise_data(bits)?;
        self.read_wave_data(bits)?;

        let n = self.ifft_size;
        let channels = self.channels;
        let subframe_size = self.subframe_size;
        let frame_size = self.frame_size;
        let fft_offset = self.fft_offset;
        // The write pointer of this frame (`buffer_ptr`).
        let buffer_base = channels * self.buffer_offset;

        for sub in 0..32usize {
            for ch in 0..channels {
                self.add_noise(ch, sub);
            }

            self.add_waves(sub);

            for ch in 0..channels {
                for i in 0..subframe_size {
                    let fft = fft_offset + sub * subframe_size + i;
                    self.cmplx_in[ch][i] =
                        Complex { re: self.fft_buffer[2 + ch][fft], im: self.fft_buffer[ch][fft] };
                    self.cmplx_in[ch][subframe_size + i] = Complex::default();
                }
            }

            for ch in 0..channels {
                self.ifft.ifft(&self.cmplx_in[ch], &mut self.cmplx_out[ch]);
            }

            // The inverse FFT of FFmpeg is not normalised, but the Symphonia one is.
            let scale = n as f32;

            for i in 0..2 * subframe_size {
                for ch in 0..channels {
                    let idx = buffer_base + channels * (sub * subframe_size + i) + ch;
                    self.buffer[idx] += self.cmplx_out[ch][i].re * scale;
                }
            }

            for i in 0..channels * subframe_size {
                let r = self.buffer[buffer_base + channels * sub * subframe_size + i];
                out.push(r.clamp(-32768.0, 32767.0) as i16);
            }

            for ch in 0..channels {
                for i in fft_offset + sub * subframe_size..fft_offset + (sub + 1) * subframe_size {
                    self.fft_buffer[ch][i] = 0.0;
                    self.fft_buffer[2 + ch][i] = 0.0;
                }
            }

            for i in 0..subframe_size * channels {
                self.buffer[buffer_base + channels * (sub * subframe_size + frame_size) + i] = 0.0;
            }
        }

        self.buffer_offset += frame_size;
        if self.buffer_offset >= 32768 - frame_size {
            let copy_len = 2 * frame_size * channels;
            self.buffer.copy_within(
                channels * self.buffer_offset..channels * self.buffer_offset + copy_len,
                0,
            );
            self.buffer_offset = 0;
        }

        Ok(())
    }
}
