// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The QDesign Music Codec 2 (QDM2) decoder.
//!
//! QDM2 is an undocumented codec. This is a port of FFmpeg's `libavcodec/qdm2.c`, including its
//! behaviours for streams that it does not fully support, so that the output is the same as
//! FFmpeg's (see `NOTICE`). Names of the original are kept where practical.

use symphonia_core::dsp::complex::Complex;
use symphonia_core::dsp::fft::Ifft;
use symphonia_core::errors::{Result, decode_error, unsupported_error};

use crate::bits::{Bits, Vlc};
use crate::mpa::{SBLIMIT, SynthFilter, SynthWindow};
use crate::qdm2_tables::*;

const SOFTCLIP_THRESHOLD: i32 = 27600;
const HARDCLIP_THRESHOLD: i32 = 35716;

const MAX_CHANNELS: usize = 2;
const MAX_FRAME_SIZE: usize = 512;
const MPA_FRAME_SIZE: usize = 1152;
const MAX_TONES: usize = 1000;
/// Complex values per channel of the FFT. FFmpeg has 257, but tones may write up to 3 past the
/// highest offset.
const FFT_COMPLEX_LEN: usize = 264;

const SWITCHTABLE: [u8; 23] = [0, 5, 1, 5, 5, 5, 5, 5, 2, 5, 5, 5, 5, 5, 5, 5, 3, 5, 5, 5, 5, 5, 4];

/// The number of subbands in use, 8, 16 or 30 (`QDM2_SB_USED`).
fn sb_used_for(sub_sampling: usize) -> usize {
    if sub_sampling >= 2 { 30 } else { 8 << sub_sampling }
}

/// A subpacket. The data is at an offset into the block, since FFmpeg holds a pointer.
#[derive(Clone, Copy, Default)]
struct SubPacket {
    ty: i32,
    /// The size. Signed as FFmpeg's unsigned wrap-around of truncated packets is an error here.
    size: i64,
    data: usize,
}

impl SubPacket {
    /// A bit reader over the data of the subpacket (`init_get_bits8`).
    fn reader<'a>(&self, buf: &'a [u8]) -> Result<Bits<'a>> {
        if self.size < 0 {
            return decode_error("qdesign: invalid sub-packet size");
        }
        Ok(Bits::with_size(buf.get(self.data..).unwrap_or(&[]), self.size as usize))
    }
}

#[derive(Clone, Copy, Default)]
struct FftTone {
    level: f32,
    /// The channel and the index of the first complex value that the tone adds to.
    ch: usize,
    offset: usize,
    table: [f32; 5],
    phase: i32,
    phase_shift: i32,
    duration: usize,
    time_index: i32,
    cutoff: i32,
}

#[derive(Clone, Copy, Default)]
struct FftCoefficient {
    sub_packet: i16,
    channel: u8,
    offset: i16,
    exp: i16,
    phase: u8,
}

/// The prefix codes.
struct Vlcs {
    level: Vlc,
    diff: Vlc,
    run: Vlc,
    fft_level_exp_alt: Vlc,
    fft_level_exp: Vlc,
    fft_stereo_exp: Vlc,
    fft_stereo_phase: Vlc,
    tone_level_idx_hi1: Vlc,
    tone_level_idx_mid: Vlc,
    tone_level_idx_hi2: Vlc,
    type30: Vlc,
    type34: Vlc,
    fft_tone_offset: Vec<Vlc>,
}

impl Vlcs {
    fn new() -> Result<Self> {
        let mut fft_tone_offset = Vec::with_capacity(5);
        let mut start = 0;
        for size in TAB_FFT_TONE_OFFSET_SIZES {
            fft_tone_offset.push(Vlc::new(&TAB_FFT_TONE_OFFSET[start..start + size])?);
            start += size;
        }

        Ok(Vlcs {
            level: Vlc::new(&TAB_LEVEL)?,
            diff: Vlc::new(&TAB_DIFF)?,
            run: Vlc::new(&TAB_RUN)?,
            fft_level_exp_alt: Vlc::new(&FFT_LEVEL_EXP_ALT)?,
            fft_level_exp: Vlc::new(&FFT_LEVEL_EXP)?,
            fft_stereo_exp: Vlc::new(&FFT_STEREO_EXP)?,
            fft_stereo_phase: Vlc::new(&FFT_STEREO_PHASE)?,
            tone_level_idx_hi1: Vlc::new(&TAB_TONE_LEVEL_IDX_HI1)?,
            tone_level_idx_mid: Vlc::new(&TAB_TONE_LEVEL_IDX_MID)?,
            tone_level_idx_hi2: Vlc::new(&TAB_TONE_LEVEL_IDX_HI2)?,
            type30: Vlc::new(&TAB_TYPE30)?,
            type34: Vlc::new(&TAB_TYPE34)?,
            fft_tone_offset,
        })
    }
}

/// Tables that FFmpeg generates at start-up (`qdm2_tablegen.h`).
struct GeneratedTables {
    softclip: Vec<u16>,
    noise_table: Vec<f32>,
    random_dequant_index: [[u8; 5]; 256],
    random_dequant_type24: [[u8; 3]; 128],
    noise_samples: [f32; 128],
}

impl GeneratedTables {
    fn new() -> Self {
        // softclip_table_init
        let dfl = f64::from(SOFTCLIP_THRESHOLD - 32767);
        let delta = (1.0 / -dfl) as f32;
        let softclip = (0..=(HARDCLIP_THRESHOLD - SOFTCLIP_THRESHOLD))
            .map(|i| {
                let v = (f64::from(i as f32 * delta)).sin() * dfl;
                (SOFTCLIP_THRESHOLD - ((v as i32) & 0xffff)) as u16
            })
            .collect();

        // rnd_table_init
        let mut noise_table = vec![0f32; 4096 + 20];
        let mut seed = 0u64;
        let delta = (1.0f64 / 16384.0) as f32;
        for n in noise_table.iter_mut().take(4096) {
            seed = seed.wrapping_mul(214013).wrapping_add(2531011);
            let v = ((seed as i32) >> 16) & 0x7fff;
            *n = ((f64::from(delta * v as f32) - 1.0) * 1.3) as f32;
        }

        let mut random_dequant_index = [[0u8; 5]; 256];
        for (i, e) in random_dequant_index.iter_mut().enumerate() {
            let mut div = 81;
            let mut ldw = i as u32;
            for v in e.iter_mut() {
                *v = (ldw / div) as u8;
                ldw %= div;
                div /= 3;
            }
        }

        let mut random_dequant_type24 = [[0u8; 3]; 128];
        for (i, e) in random_dequant_type24.iter_mut().enumerate() {
            let mut div = 25;
            let mut ldw = i as u32;
            for v in e.iter_mut() {
                *v = (ldw / div) as u8;
                ldw %= div;
                div /= 5;
            }
        }

        // init_noise_samples
        let mut noise_samples = [0f32; 128];
        let mut seed = 0u32;
        for n in noise_samples.iter_mut() {
            seed = seed.wrapping_mul(214013).wrapping_add(2531011);
            *n = (f64::from(delta * ((seed >> 16) & 0x7fff) as f32) - 1.0) as f32;
        }

        GeneratedTables {
            softclip,
            noise_table,
            random_dequant_index,
            random_dequant_type24,
            noise_samples,
        }
    }
}

/// Reads a code and the optional escape and stage-3 extension (`qdm2_get_vlc`).
fn get_vlc(gb: &mut Bits<'_>, vlc: &Vlc, flag: bool) -> i32 {
    let mut value = vlc.get(gb);

    // Stage 2, a 3 bit exponent escape sequence.
    if value < 0 {
        let n = gb.get(3) + 1;
        value = gb.get(n) as i32;
    }

    // Stage 3, optional.
    if flag {
        if value >= 60 {
            return 0;
        }

        let mut tmp = VLC_STAGE3_VALUES[value as usize];

        if (value & !3) > 0 {
            tmp += gb.get((value >> 2) as u32) as i32;
        }
        value = tmp;
    }

    value
}

/// Reads a signed code (`qdm2_get_se_vlc`).
fn get_se_vlc(vlc: &Vlc, gb: &mut Bits<'_>) -> i32 {
    let value = get_vlc(gb, vlc, false);

    if value & 1 != 0 { (value + 1) >> 1 } else { -(value >> 1) }
}

/// Reads a subpacket header (`qdm2_decode_sub_packet_header`). The data offset is relative to
/// the start of the buffer of `gb`.
fn decode_sub_packet_header(gb: &mut Bits<'_>) -> SubPacket {
    let mut ty = gb.get(8) as i32;

    if ty == 0 {
        return SubPacket { ty, size: 0, data: 0 };
    }

    let mut size = i64::from(gb.get(8));

    if ty & 0x80 != 0 {
        size = (size << 8) | i64::from(gb.get(8));
        ty &= 0x7f;
    }

    if ty == 0x7f {
        ty |= (gb.get(8) as i32) << 8;
    }

    SubPacket { ty, size, data: gb.pos() / 8 }
}

/// A noise value for the subband (`SB_DITHERING_NOISE`).
fn dither(noise_table: &[f32], noise_idx: &mut usize, sb: usize) -> f32 {
    let n = noise_table.get(*noise_idx).copied().unwrap_or(0.0);
    *noise_idx += 1;
    n * SB_NOISE_ATTENUATION[sb]
}

pub(crate) struct Qdm2 {
    // Parameters from the extra data.
    channels: usize,
    group_size: usize,
    fft_size: usize,
    checksum_size: usize,
    sample_rate: u32,

    // Parameters built from the above.
    group_order: usize,
    frame_size: usize,
    frequency_range: i32,
    sub_sampling: usize,
    coeff_per_sb_select: usize,
    cm_table_select: usize,

    vlcs: Vlcs,
    gtab: GeneratedTables,
    window: SynthWindow,
    ifft: Ifft,

    sub_packets: [SubPacket; 16],
    list_b: Vec<usize>,
    sub_packets_b: usize,
    list_d: Vec<usize>,

    fft_tones: Vec<FftTone>,
    fft_tone_start: usize,
    fft_tone_end: usize,
    fft_coefs: Vec<FftCoefficient>,
    fft_coefs_index: usize,
    fft_coefs_min_index: [i32; 5],
    fft_coefs_max_index: [i32; 5],
    fft_level_exp: [i32; 6],
    fft_complex: [[Complex<f32>; FFT_COMPLEX_LEN]; MAX_CHANNELS],
    fft_temp: Vec<Complex<f32>>,
    fft_scratch: Vec<Complex<f32>>,

    output_buffer: Vec<f32>,

    synth: [SynthFilter; MAX_CHANNELS],
    sb_samples: Vec<[f32; SBLIMIT]>,
    samples: Vec<f32>,

    tone_level: [[[f32; 64]; 30]; MAX_CHANNELS],
    /// `[ch][sb][j]`, flattened.
    coding_method: Vec<i8>,
    quantized_coeffs: [[[i8; 8]; 10]; MAX_CHANNELS],
    tone_level_idx_base: [[[i8; 8]; 30]; MAX_CHANNELS],
    tone_level_idx_hi1: [[[[i8; 8]; 8]; 3]; MAX_CHANNELS],
    tone_level_idx_mid: [[[i8; 8]; 26]; MAX_CHANNELS],
    tone_level_idx_hi2: [[i8; 26]; MAX_CHANNELS],

    has_errors: bool,
    superblocktype_2_3: bool,
    do_synth_filter: bool,

    sub_packet: usize,
    noise_idx: usize,
}

fn be32(data: &[u8], pos: usize) -> Option<u32> {
    let b = data.get(pos..pos + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// The parameters of the `QDCA` atom.
pub(crate) struct QdcaParams {
    pub channels: u32,
    pub sample_rate: u32,
    pub bit_rate: u32,
    pub group_size: u32,
    pub fft_size: u32,
    pub checksum_size: u32,
}

/// Finds the `frma` atom naming `fourcc` in the extra data, and parses the `QDCA` atom that
/// follows it. FFmpeg's muxer writes the `QDCA` atom directly (no `wave`/`frma` wrapper) when
/// remuxing, so a bare `QDCA` atom at the start of the extra data is also accepted.
pub(crate) fn parse_extra_data(extra: &[u8], fourcc: &[u8; 4]) -> Result<QdcaParams> {
    if extra.len() < 48 {
        return decode_error("qdesign: extra data missing or truncated");
    }

    let mut pos = if extra.get(4..8) == Some(b"QDCA") {
        0
    }
    else {
        let mut marker = [0u8; 8];
        marker[..4].copy_from_slice(b"frma");
        marker[4..].copy_from_slice(fourcc);

        // Like FFmpeg, look for the marker while more than 8 bytes remain.
        let mut pos = 0;
        while extra.len() - pos > 8 {
            if extra[pos..pos + 8] == marker {
                break;
            }
            pos += 1;
        }

        pos + 8
    };
    let size = be32(extra, pos).unwrap_or(0) as usize;
    pos += 4;

    if size > extra.len() - pos {
        return decode_error("qdesign: extra data size too small");
    }

    if extra.get(pos..pos + 4) != Some(b"QDCA") {
        return decode_error("qdesign: invalid extra data, expecting QDCA");
    }
    // The tag, then the unknown (always 1) version field.
    pos += 8;

    let mut field = || {
        let v = be32(extra, pos);
        pos += 4;
        v.ok_or(())
    };

    let r = (|| -> std::result::Result<QdcaParams, ()> {
        Ok(QdcaParams {
            channels: field()?,
            sample_rate: field()?,
            bit_rate: field()?,
            group_size: field()?,
            fft_size: field()?,
            checksum_size: field()?,
        })
    })();

    match r {
        Ok(p) => Ok(p),
        Err(()) => decode_error("qdesign: not enough extra data"),
    }
}

impl Qdm2 {
    pub(crate) fn new(extra: &[u8]) -> Result<Box<Self>> {
        let p = parse_extra_data(extra, b"QDM2")?;

        if p.channels == 0 || p.channels as usize > MAX_CHANNELS {
            return decode_error("qdesign: invalid number of channels");
        }
        let channels = p.channels as usize;

        if p.checksum_size >= 1 << 28 || p.checksum_size <= 1 {
            return decode_error("qdesign: data block size invalid");
        }

        if p.fft_size == 0 || p.fft_size > i32::MAX as u32 || p.group_size > i32::MAX as u32 {
            return decode_error("qdesign: invalid FFT or group size");
        }

        let fft_order = p.fft_size.ilog2() as usize + 1;

        // FFmpeg fails on an unknown FFT order.
        if !(7..=9).contains(&fft_order) {
            return unsupported_error("qdesign: unsupported FFT order");
        }

        if p.group_size == 0 {
            return decode_error("qdesign: invalid group size");
        }

        let group_order = p.group_size.ilog2() as usize + 1;
        // 16 iterations per super block.
        let frame_size = p.group_size as usize / 16;

        if frame_size > MAX_FRAME_SIZE || frame_size == 0 {
            return decode_error("qdesign: invalid frame size");
        }

        let sub_sampling = fft_order - 7;
        let frequency_range = 255 / (1 << (2 - sub_sampling));

        if (frame_size * 4) >> sub_sampling > MPA_FRAME_SIZE {
            return unsupported_error("qdesign: large frames");
        }

        let tmp = match sub_sampling * 2 + channels - 1 {
            0 => 40,
            1 => 48,
            2 => 56,
            3 => 72,
            4 => 80,
            5 => 100,
            _ => sub_sampling as u32,
        };

        let bit_rate = i64::from(p.bit_rate);
        let mut cm_table_select = 0;
        if i64::from(tmp * 1000) < bit_rate {
            cm_table_select = 1;
        }
        if i64::from(tmp * 1440) < bit_rate {
            cm_table_select = 2;
        }
        if i64::from(tmp * 1760) < bit_rate {
            cm_table_select = 3;
        }
        if i64::from(tmp * 2240) < bit_rate {
            cm_table_select = 4;
        }

        let coeff_per_sb_select = if bit_rate <= 8000 {
            0
        }
        else if bit_rate < 16000 {
            1
        }
        else {
            2
        };

        let fft_size = p.fft_size as usize;
        if fft_size != 1 << (fft_order - 1) {
            return decode_error("qdesign: FFT size not a power of 2");
        }

        Ok(Box::new(Qdm2 {
            channels,
            group_size: p.group_size as usize,
            fft_size,
            checksum_size: p.checksum_size as usize,
            sample_rate: p.sample_rate,
            group_order,
            frame_size,
            frequency_range,
            sub_sampling,
            coeff_per_sb_select,
            cm_table_select,
            vlcs: Vlcs::new()?,
            gtab: GeneratedTables::new(),
            window: SynthWindow::new(),
            ifft: Ifft::new(fft_size),
            sub_packets: [SubPacket::default(); 16],
            list_b: Vec::new(),
            sub_packets_b: 0,
            list_d: Vec::new(),
            fft_tones: vec![FftTone::default(); MAX_TONES],
            fft_tone_start: 0,
            fft_tone_end: 0,
            fft_coefs: vec![FftCoefficient::default(); MAX_TONES],
            fft_coefs_index: 0,
            fft_coefs_min_index: [0; 5],
            fft_coefs_max_index: [0; 5],
            fft_level_exp: [0; 6],
            fft_complex: [[Complex::default(); FFT_COMPLEX_LEN]; MAX_CHANNELS],
            fft_temp: vec![Complex::default(); fft_size],
            fft_scratch: vec![Complex::default(); fft_size],
            output_buffer: vec![0.0; MAX_FRAME_SIZE * MAX_CHANNELS * 2],
            synth: [SynthFilter::new(), SynthFilter::new()],
            sb_samples: vec![[0.0; SBLIMIT]; MAX_CHANNELS * 128],
            samples: vec![0.0; MAX_CHANNELS * MPA_FRAME_SIZE],
            tone_level: [[[0.0; 64]; 30]; MAX_CHANNELS],
            coding_method: vec![0; MAX_CHANNELS * 30 * 64],
            quantized_coeffs: [[[0; 8]; 10]; MAX_CHANNELS],
            tone_level_idx_base: [[[0; 8]; 30]; MAX_CHANNELS],
            tone_level_idx_hi1: [[[[0; 8]; 8]; 3]; MAX_CHANNELS],
            tone_level_idx_mid: [[[0; 8]; 26]; MAX_CHANNELS],
            tone_level_idx_hi2: [[0; 26]; MAX_CHANNELS],
            has_errors: false,
            superblocktype_2_3: false,
            do_synth_filter: false,
            sub_packet: 0,
            noise_idx: 0,
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
        16 * self.frame_size
    }

    /// The size of a block in bytes.
    pub(crate) fn block_size(&self) -> usize {
        self.checksum_size
    }

    /// Decodes a block (`qdm2_decode_frame`) of `block_size()` bytes. Interleaved samples are
    /// appended to `out`.
    pub(crate) fn decode_block(&mut self, buf: &[u8], out: &mut Vec<i16>) -> Result<()> {
        if buf.len() < self.checksum_size {
            return decode_error("qdesign: packet shorter than a block");
        }

        self.sub_packet = 0;

        let chunk = self.channels * self.frame_size;
        let start = out.len();
        out.resize(start + 16 * chunk, 0);

        for i in 0..16 {
            let slot = &mut out[start + i * chunk..start + (i + 1) * chunk];
            if let Err(err) = self.decode_sub_block(buf, slot) {
                out.truncate(start);
                return Err(err);
            }
        }

        Ok(())
    }

    /// `qdm2_decode`.
    fn decode_sub_block(&mut self, buf: &[u8], out: &mut [i16]) -> Result<()> {
        let frame_size = self.frame_size * self.channels;

        // Copy the old block, clear the new block of output samples.
        self.output_buffer.copy_within(frame_size..2 * frame_size, 0);
        self.output_buffer[frame_size..2 * frame_size].fill(0.0);

        // Decode a block of QDM2 compressed data.
        if self.sub_packet == 0 {
            self.has_errors = false;
            self.decode_super_block(buf)?;
        }

        // Parse subpackets.
        if !self.has_errors {
            if self.sub_packet == 2 {
                self.decode_fft_packets(buf)?;
            }

            self.fft_tone_synthesizer(self.sub_packet);
        }

        // Sound synthesis stage 1 (FFT).
        for ch in 0..self.channels {
            self.calculate_fft(ch);
        }

        // Sound synthesis stage 2 (MPEG audio like synthesis filter).
        if !self.has_errors && self.do_synth_filter {
            self.synthesis_filter(self.sub_packet);
        }

        self.sub_packet = (self.sub_packet + 1) % 16;

        // Clip and convert the output to 16-bit signed samples.
        for (o, &f) in out.iter_mut().zip(self.output_buffer.iter()) {
            let mut value = f as i32;

            if value > SOFTCLIP_THRESHOLD {
                value = if value > HARDCLIP_THRESHOLD {
                    32767
                }
                else {
                    i32::from(self.gtab.softclip[(value - SOFTCLIP_THRESHOLD) as usize])
                };
            }
            else if value < -SOFTCLIP_THRESHOLD {
                value = if value < -HARDCLIP_THRESHOLD {
                    -32767
                }
                else {
                    -i32::from(self.gtab.softclip[(-value - SOFTCLIP_THRESHOLD) as usize])
                };
            }

            *o = value as i16;
        }

        Ok(())
    }

    fn sb_used(&self) -> usize {
        sb_used_for(self.sub_sampling)
    }

    /// Replaces 8 elements with their average value (`average_quantized_coeffs`).
    fn average_quantized_coeffs(&mut self) {
        let n = COEFF_PER_SB_FOR_AVG[self.coeff_per_sb_select][self.sb_used() - 1] + 1;

        for ch in 0..self.channels {
            for i in 0..n {
                let mut sum: i32 = self.quantized_coeffs[ch][i].iter().map(|&c| i32::from(c)).sum();

                sum /= 8;
                if sum > 0 {
                    sum -= 1;
                }

                self.quantized_coeffs[ch][i] = [sum as i8; 8];
            }
        }
    }

    /// Builds subband samples from noise weighted by the tone levels
    /// (`build_sb_samples_from_noise`).
    fn build_sb_samples_from_noise(&mut self, sb: usize) {
        if self.noise_idx >= 3840 {
            self.noise_idx -= 3840;
        }

        for ch in 0..self.channels {
            for j in 0..64 {
                let level = self.tone_level[ch][sb][j];
                let n = dither(&self.gtab.noise_table, &mut self.noise_idx, sb) * level;
                self.sb_samples[ch * 128 + j * 2][sb] = n;
                let n = dither(&self.gtab.noise_table, &mut self.noise_idx, sb) * level;
                self.sb_samples[ch * 128 + j * 2 + 1][sb] = n;
            }
        }
    }

    fn cm_index(ch: usize, sb: usize, j: usize) -> usize {
        (ch * 30 + sb) * 64 + j
    }

    /// Returns true if the coding method is invalid (`fix_coding_method_array`).
    fn fix_coding_method_array(&mut self, sb: usize, channels: usize) -> bool {
        for ch in 0..channels {
            let mut j = 0;

            while j < 64 {
                let cm = i32::from(self.coding_method[Self::cm_index(ch, sb, j)]);

                if cm < 8 {
                    return true;
                }

                let (run, case_val) = if cm - 8 > 22 {
                    (1, 8)
                }
                else {
                    match SWITCHTABLE[(cm - 8) as usize] {
                        0 => (10, 10),
                        1 => (1, 16),
                        2 => (5, 24),
                        3 => (3, 30),
                        4 => (1, 30),
                        _ => (1, 8),
                    }
                };

                for k in 0..run {
                    if j + k < 128 {
                        let sbjk = sb + (j + k) / 64;

                        if sbjk > 29 {
                            continue;
                        }

                        let other =
                            i32::from(self.coding_method[Self::cm_index(ch, sbjk, (j + k) % 64)]);

                        if other > cm && k > 0 {
                            // Not debugged, almost never used in FFmpeg. The memsets of FFmpeg
                            // may run past the array.
                            let base = Self::cm_index(ch, sb, j + k);
                            let len = self.coding_method.len();
                            for idx in base..(base + k).min(len) {
                                self.coding_method[idx] = case_val;
                            }
                            for idx in base..(base + 3).min(len) {
                                self.coding_method[idx] = case_val;
                            }
                        }
                    }
                }

                j += run;
            }
        }

        false
    }

    /// `fill_tone_level_array`.
    fn fill_tone_level_array(&mut self, flag: bool) {
        let sel = self.coeff_per_sb_select;

        for ch in 0..self.channels {
            for sb in 0..30 {
                for i in 0..8 {
                    let tab = COEFF_PER_SB_FOR_DEQUANT[sel][sb];
                    let mut tmp = if tab < LAST_COEFF[sel] - 1 {
                        i32::from(self.quantized_coeffs[ch][tab + 1][i])
                            * DEQUANT_TABLE[sel][tab + 1][sb]
                            + i32::from(self.quantized_coeffs[ch][tab][i])
                                * DEQUANT_TABLE[sel][tab][sb]
                    }
                    else {
                        i32::from(self.quantized_coeffs[ch][tab][i]) * DEQUANT_TABLE[sel][tab][sb]
                    };

                    if tmp < 0 {
                        tmp += 0xff;
                    }
                    self.tone_level_idx_base[ch][sb][i] = ((tmp / 256) & 0xff) as i8;
                }
            }
        }

        let sb_used = self.sb_used();
        let t23 = self.superblocktype_2_3;

        if t23 && !flag {
            for sb in 0..sb_used {
                for ch in 0..self.channels {
                    for i in 0..64 {
                        let idx = self.tone_level_idx_base[ch][sb][i / 8];

                        self.tone_level[ch][sb][i] = if idx < 0 {
                            0.0
                        }
                        else {
                            FFT_TONE_LEVEL_TABLE[0][(idx & 0x3f) as usize]
                        };
                    }
                }
            }
        }
        else {
            let tab = if t23 { 0 } else { 1 };

            for sb in 0..sb_used {
                for ch in 0..self.channels {
                    for i in 0..64 {
                        let base = i32::from(self.tone_level_idx_base[ch][sb][i / 8]);

                        let tmp = if (4..=23).contains(&sb) {
                            base - i32::from(self.tone_level_idx_hi1[ch][sb / 8][i / 8][i % 8])
                                - i32::from(self.tone_level_idx_mid[ch][sb - 4][i / 8])
                                - i32::from(self.tone_level_idx_hi2[ch][sb - 4])
                        }
                        else if sb > 4 {
                            base - i32::from(self.tone_level_idx_hi1[ch][2][i / 8][i % 8])
                                - i32::from(self.tone_level_idx_hi2[ch][sb - 4])
                        }
                        else {
                            base
                        };

                        // The stored index is a signed byte, see the first case above.
                        self.tone_level[ch][sb][i] = if tmp < 0 || (!t23 && tmp == 0) {
                            0.0
                        }
                        else {
                            FFT_TONE_LEVEL_TABLE[tab][(tmp & 0x3f) as usize]
                        };
                    }
                }
            }
        }
    }

    /// `fill_coding_method_array`.
    fn fill_coding_method_array(&mut self) -> Result<()> {
        if !self.superblocktype_2_3 {
            // FFmpeg has no sample for this case and requests one.
            return unsupported_error("qdesign: !superblocktype_2_3 is not supported");
        }

        for ch in 0..self.channels {
            for sb in 0..30 {
                for j in 0..64 {
                    self.coding_method[Self::cm_index(ch, sb, j)] =
                        CODING_METHOD_TABLE[self.cm_table_select][sb];
                }
            }
        }

        Ok(())
    }

    /// `synthfilt_build_sb_samples`.
    fn synthfilt_build_sb_samples(
        &mut self,
        gb: &mut Bits<'_>,
        length: usize,
        sb_min: usize,
        sb_max: usize,
    ) -> Result<()> {
        let mut type34_div = 0f32;
        let mut type34_predictor;
        let mut samples = [0f32; 10];
        let mut sign_bits = [0u32; 16];

        if length == 0 {
            // If no data use noise.
            for sb in sb_min..sb_max {
                self.build_sb_samples_from_noise(sb);
            }
            return Ok(());
        }

        for sb in sb_min..sb_max {
            let mut channels = self.channels;

            let joined_stereo = if self.channels <= 1 || sb < 12 {
                0
            }
            else if sb >= 24 {
                1
            }
            else if gb.bits_left() >= 1 {
                gb.get1() as usize
            }
            else {
                0
            };

            if joined_stereo != 0 {
                if gb.bits_left() >= 16 {
                    for s in sign_bits.iter_mut() {
                        *s = gb.get1();
                    }
                }

                for j in 0..64 {
                    let c1 = self.coding_method[Self::cm_index(1, sb, j)];
                    let c0 = &mut self.coding_method[Self::cm_index(0, sb, j)];
                    if c1 > *c0 {
                        *c0 = c1;
                    }
                }

                if self.fix_coding_method_array(sb, self.channels) {
                    self.build_sb_samples_from_noise(sb);
                    continue;
                }
                channels = 1;
            }

            for ch in 0..channels {
                if self.noise_idx >= 3840 {
                    self.noise_idx -= 3840;
                }

                let zero_encoding = if gb.bits_left() >= 1 { gb.get1() != 0 } else { false };
                type34_predictor = 0.0f32;
                let mut type34_first = true;

                let mut j = 0;
                while j < 128 {
                    let run;
                    let nt = &self.gtab.noise_table;

                    match self.coding_method[Self::cm_index(ch, sb, j / 2)] {
                        8 => {
                            if gb.bits_left() >= 10 {
                                if zero_encoding {
                                    for k in 0..5 {
                                        if j + 2 * k >= 128 {
                                            break;
                                        }
                                        samples[2 * k] = if gb.get1() != 0 {
                                            DEQUANT_1BIT[joined_stereo][2 * gb.get1() as usize]
                                        }
                                        else {
                                            0.0
                                        };
                                    }
                                }
                                else {
                                    let n = gb.get(8) as usize;
                                    if n >= 243 {
                                        return decode_error("qdesign: invalid 8bit codeword");
                                    }

                                    for k in 0..5 {
                                        samples[2 * k] = DEQUANT_1BIT[joined_stereo]
                                            [usize::from(self.gtab.random_dequant_index[n][k])];
                                    }
                                }
                                for k in 0..5 {
                                    samples[2 * k + 1] = dither(nt, &mut self.noise_idx, sb);
                                }
                            }
                            else {
                                for s in samples.iter_mut() {
                                    *s = dither(nt, &mut self.noise_idx, sb);
                                }
                            }
                            run = 10;
                        }
                        10 => {
                            if gb.bits_left() >= 1 {
                                let mut f = 0.81f32;

                                if gb.get1() != 0 {
                                    f = -f;
                                }
                                let ns =
                                    self.gtab.noise_samples[((sb + 1) * (j + 5 * ch + 1)) & 127];
                                // Computed in double precision, as in C.
                                f = (f64::from(f) - f64::from(ns) * 9.0 / 40.0) as f32;
                                samples[0] = f;
                            }
                            else {
                                samples[0] = dither(nt, &mut self.noise_idx, sb);
                            }
                            run = 1;
                        }
                        16 => {
                            if gb.bits_left() >= 10 {
                                if zero_encoding {
                                    for k in 0..5 {
                                        if j + k >= 128 {
                                            break;
                                        }
                                        samples[k] = if gb.get1() == 0 {
                                            0.0
                                        }
                                        else {
                                            DEQUANT_1BIT[joined_stereo][2 * gb.get1() as usize]
                                        };
                                    }
                                }
                                else {
                                    let n = gb.get(8) as usize;
                                    if n >= 243 {
                                        return decode_error("qdesign: invalid 8bit codeword");
                                    }

                                    for k in 0..5 {
                                        samples[k] = DEQUANT_1BIT[joined_stereo]
                                            [usize::from(self.gtab.random_dequant_index[n][k])];
                                    }
                                }
                            }
                            else {
                                for s in samples.iter_mut().take(5) {
                                    *s = dither(nt, &mut self.noise_idx, sb);
                                }
                            }
                            run = 5;
                        }
                        24 => {
                            if gb.bits_left() >= 7 {
                                let n = gb.get(7) as usize;
                                if n >= 125 {
                                    return decode_error("qdesign: invalid 7bit codeword");
                                }

                                for k in 0..3 {
                                    samples[k] =
                                        ((f64::from(self.gtab.random_dequant_type24[n][k]) - 2.0)
                                            * 0.5) as f32;
                                }
                            }
                            else {
                                for s in samples.iter_mut().take(3) {
                                    *s = dither(nt, &mut self.noise_idx, sb);
                                }
                            }
                            run = 3;
                        }
                        30 => {
                            if gb.bits_left() >= 4 {
                                let index = get_vlc(gb, &self.vlcs.type30, false);
                                match usize::try_from(index)
                                    .ok()
                                    .and_then(|i| TYPE30_DEQUANT.get(i))
                                {
                                    Some(&v) => samples[0] = v,
                                    None => {
                                        return decode_error("qdesign: type30 index out of range");
                                    }
                                }
                            }
                            else {
                                samples[0] = dither(nt, &mut self.noise_idx, sb);
                            }
                            run = 1;
                        }
                        34 => {
                            if gb.bits_left() >= 7 {
                                if type34_first {
                                    type34_div = (1u32 << gb.get(2)) as f32;
                                    samples[0] =
                                        ((f64::from(gb.get(5) as f32) - 16.0) / 15.0) as f32;
                                    type34_predictor = samples[0];
                                    type34_first = false;
                                }
                                else {
                                    let index = get_vlc(gb, &self.vlcs.type34, false);
                                    match usize::try_from(index)
                                        .ok()
                                        .and_then(|i| TYPE34_DELTA.get(i))
                                    {
                                        Some(&v) => {
                                            samples[0] = v / type34_div + type34_predictor;
                                            type34_predictor = samples[0];
                                        }
                                        None => {
                                            return decode_error(
                                                "qdesign: type34 index out of range",
                                            );
                                        }
                                    }
                                }
                            }
                            else {
                                samples[0] = dither(nt, &mut self.noise_idx, sb);
                            }
                            run = 1;
                        }
                        _ => {
                            samples[0] = dither(nt, &mut self.noise_idx, sb);
                            run = 1;
                        }
                    }

                    if joined_stereo != 0 {
                        for k in 0..run {
                            if j + k >= 128 {
                                break;
                            }
                            self.sb_samples[j + k][sb] =
                                self.tone_level[0][sb][(j + k) / 2] * samples[k];

                            if self.channels == 2 {
                                let tl = self.tone_level[1][sb][(j + k) / 2];
                                self.sb_samples[128 + j + k][sb] = if sign_bits[(j + k) / 8] != 0 {
                                    tl * -samples[k]
                                }
                                else {
                                    tl * samples[k]
                                };
                            }
                        }
                    }
                    else {
                        for k in 0..run {
                            if j + k < 128 {
                                self.sb_samples[ch * 128 + j + k][sb] =
                                    self.tone_level[ch][sb][(j + k) / 2] * samples[k];
                            }
                        }
                    }

                    j += run;
                }
            }
        }

        Ok(())
    }

    /// `init_quantized_coeffs_elem0`.
    fn init_quantized_coeffs_elem0(
        vlcs: &Vlcs,
        coeffs: &mut [i8; 8],
        gb: &mut Bits<'_>,
    ) -> Result<()> {
        if gb.bits_left() < 16 {
            return decode_error("qdesign: truncated sub-packet");
        }
        let mut level = get_vlc(gb, &vlcs.level, false);

        coeffs[0] = level as i8;

        let mut i = 0usize;
        while i < 7 {
            if gb.bits_left() < 16 {
                return decode_error("qdesign: truncated sub-packet");
            }
            let run = get_vlc(gb, &vlcs.run, false) + 1;

            if i as i32 + run >= 8 {
                return decode_error("qdesign: invalid run");
            }

            if gb.bits_left() < 16 {
                return decode_error("qdesign: truncated sub-packet");
            }
            let diff = get_se_vlc(&vlcs.diff, gb);

            for k in 1..=run {
                coeffs[i + k as usize] = (level + ((k * diff) / run)) as i8;
            }

            level += diff;
            i += run as usize;
        }

        Ok(())
    }

    /// `init_tone_level_dequantization`.
    fn init_tone_level_dequantization(&mut self, gb: &mut Bits<'_>) -> Result<()> {
        for ch in 0..self.channels {
            Self::init_quantized_coeffs_elem0(&self.vlcs, &mut self.quantized_coeffs[ch][0], gb)?;

            if gb.bits_left() < 16 {
                self.quantized_coeffs[ch][0] = [0; 8];
                break;
            }
        }

        let n = self.sub_sampling + 1;

        for sb in 0..n {
            for ch in 0..self.channels {
                for j in 0..8 {
                    if gb.bits_left() < 1 {
                        break;
                    }
                    if gb.get1() != 0 {
                        for k in 0..8 {
                            if gb.bits_left() < 16 {
                                break;
                            }
                            self.tone_level_idx_hi1[ch][sb][j][k] =
                                get_vlc(gb, &self.vlcs.tone_level_idx_hi1, false) as i8;
                        }
                    }
                    else {
                        self.tone_level_idx_hi1[ch][sb][j] = [0; 8];
                    }
                }
            }
        }

        let n = self.sb_used() - 4;

        for sb in 0..n {
            for ch in 0..self.channels {
                if gb.bits_left() < 16 {
                    break;
                }
                let v = get_vlc(gb, &self.vlcs.tone_level_idx_hi2, false) as i8;
                self.tone_level_idx_hi2[ch][sb] = v;
                if sb > 19 {
                    self.tone_level_idx_hi2[ch][sb] = v.wrapping_sub(16);
                }
                else {
                    self.tone_level_idx_mid[ch][sb] = [-16; 8];
                }
            }
        }

        let n = self.sb_used() - 5;

        for sb in 0..n {
            for ch in 0..self.channels {
                for j in 0..8 {
                    if gb.bits_left() < 16 {
                        break;
                    }
                    self.tone_level_idx_mid[ch][sb][j] =
                        (get_vlc(gb, &self.vlcs.tone_level_idx_mid, false) - 32) as i8;
                }
            }
        }

        Ok(())
    }

    /// `process_subpacket_9`.
    fn process_subpacket_9(&mut self, buf: &[u8], sp: SubPacket) -> Result<()> {
        let mut gb = sp.reader(buf)?;

        let n = COEFF_PER_SB_FOR_AVG[self.coeff_per_sb_select][self.sb_used() - 1] + 1;

        for i in 1..n {
            for ch in 0..self.channels {
                let mut level = get_vlc(&mut gb, &self.vlcs.level, false);
                self.quantized_coeffs[ch][i][0] = level as i8;

                let mut j = 0usize;
                while j < 7 {
                    let run = get_vlc(&mut gb, &self.vlcs.run, false) + 1;
                    let diff = get_se_vlc(&self.vlcs.diff, &mut gb);

                    if j as i32 + run >= 8 {
                        return decode_error("qdesign: invalid run");
                    }

                    for k in 1..=run {
                        self.quantized_coeffs[ch][i][j + k as usize] =
                            (level + ((k * diff) / run)) as i8;
                    }

                    level += diff;
                    j += run as usize;
                }
            }
        }

        for ch in 0..self.channels {
            self.quantized_coeffs[ch][0] = [0; 8];
        }

        Ok(())
    }

    /// `process_subpacket_10`.
    fn process_subpacket_10(&mut self, buf: &[u8], sp: Option<SubPacket>) -> Result<()> {
        if let Some(sp) = sp {
            let mut gb = sp.reader(buf)?;
            self.init_tone_level_dequantization(&mut gb)?;
            self.fill_tone_level_array(true);
        }
        else {
            self.fill_tone_level_array(false);
        }
        Ok(())
    }

    /// `process_subpacket_11`.
    fn process_subpacket_11(&mut self, buf: &[u8], sp: Option<SubPacket>) -> Result<()> {
        let mut gb = Bits::new(&[]);
        let mut length = 0;

        if let Some(sp) = sp {
            gb = sp.reader(buf)?;
            length = sp.size as usize * 8;
        }

        if length >= 32 {
            let c = gb.get(13);

            if c > 3 {
                self.fill_coding_method_array()?;
            }
        }

        self.synthfilt_build_sb_samples(&mut gb, length, 0, 8)
    }

    /// `process_subpacket_12`.
    fn process_subpacket_12(&mut self, buf: &[u8], sp: Option<SubPacket>) -> Result<()> {
        // FFmpeg initialises the reader with a size of zero, so the subpacket is never read and
        // the samples are noise. This is reproduced.
        let mut gb = Bits::with_size(&[], 0);
        let mut length = 0;

        if let Some(sp) = sp {
            sp.reader(buf)?;
            length = sp.size as usize * 8;
        }

        let sb_used = self.sb_used();
        self.synthfilt_build_sb_samples(&mut gb, length, 8, sb_used)
    }

    /// The first subpacket of a type in list D.
    fn search_list_d(&self, ty: i32) -> Option<SubPacket> {
        self.list_d.iter().map(|&i| self.sub_packets[i]).find(|p| p.ty == ty)
    }

    /// `process_synthesis_subpackets`.
    fn process_synthesis_subpackets(&mut self, buf: &[u8]) -> Result<()> {
        let n9 = self.search_list_d(9);
        if let Some(n9) = n9 {
            self.process_subpacket_9(buf, n9)?;
        }

        let n10 = self.search_list_d(10);
        self.process_subpacket_10(buf, n10)?;

        let n11 = self.search_list_d(11);
        let use11 = if n9.is_some() && n10.is_some() { n11 } else { None };
        self.process_subpacket_11(buf, use11)?;

        let n12 = self.search_list_d(12);
        let use12 = if n9.is_some() && n10.is_some() { n12 } else { None };
        self.process_subpacket_12(buf, use12)
    }

    /// Decodes the super block and fills the packet lists (`qdm2_decode_super_block`).
    fn decode_super_block(&mut self, buf: &[u8]) -> Result<()> {
        let cs = self.checksum_size;

        self.tone_level_idx_hi1 = [[[[0; 8]; 8]; 3]; MAX_CHANNELS];
        self.tone_level_idx_mid = [[[0; 8]; 26]; MAX_CHANNELS];
        self.tone_level_idx_hi2 = [[0; 26]; MAX_CHANNELS];

        self.sub_packets_b = 0;

        // Average elements in quantized_coeffs.
        self.average_quantized_coeffs();

        let mut gb = Bits::with_size(buf, cs);
        let header = decode_sub_packet_header(&mut gb);

        if header.ty < 2 || header.ty >= 8 {
            self.has_errors = true;
            return decode_error("qdesign: bad superblock type");
        }

        self.superblocktype_2_3 = header.ty == 2 || header.ty == 3;
        let mut packet_bytes = cs as i64 - (gb.pos() / 8) as i64;

        let header_buf = buf.get(header.data..).unwrap_or(&[]);
        let header_size = header.size as usize;
        let mut gb = Bits::with_size(header_buf, header_size);

        if header.ty == 2 || header.ty == 4 || header.ty == 5 {
            let mut csum = 257 * gb.get(8) as i32;
            csum += 2 * gb.get(8) as i32;

            // qdm2_packet_checksum
            for &b in &buf[..cs] {
                csum -= i32::from(b);
            }

            if csum & 0xffff != 0 {
                self.has_errors = true;
                return decode_error("qdesign: bad packet checksum");
            }
        }

        self.list_b.clear();
        self.list_d.clear();

        for e in self.fft_level_exp.iter_mut() {
            *e -= 1;
            if *e < 0 {
                *e = 0;
            }
        }

        let mut next_index = 0usize;
        let mut i = 0usize;

        while packet_bytes > 0 {
            if i >= self.sub_packets.len() {
                return unsupported_error("qdesign: too many packet bytes");
            }

            if i > 0 {
                // Seek to the next block.
                gb = Bits::with_size(header_buf, header_size);
                gb.skip(next_index * 8);

                if next_index >= header_size {
                    break;
                }
            }

            // Decode the subpacket.
            let mut packet = decode_sub_packet_header(&mut gb);
            packet.data += header.data;
            next_index = packet.size as usize + gb.pos() / 8;
            let sub_packet_size = i64::from(packet.size > 0xff) + packet.size + 2;

            if packet.ty == 0 {
                break;
            }

            if sub_packet_size > packet_bytes {
                if packet.ty != 10 && packet.ty != 11 && packet.ty != 12 {
                    break;
                }
                packet.size += packet_bytes - sub_packet_size;
            }

            packet_bytes -= sub_packet_size;
            self.sub_packets[i] = packet;

            // Add the subpacket to the related list.
            match packet.ty {
                8 => return unsupported_error("qdesign: packet type 8"),
                9..=12 => {
                    // Packets for the MPEG audio like synthesis filter.
                    self.list_d.push(i);
                }
                13 => {
                    for e in self.fft_level_exp.iter_mut() {
                        *e = gb.get(6) as i32;
                    }
                }
                14 => {
                    for e in self.fft_level_exp.iter_mut() {
                        *e = get_vlc(&mut gb, &self.vlcs.fft_level_exp, false);
                    }
                }
                15 => return unsupported_error("qdesign: packet type 15"),
                16..=47 if FFT_SUBPACKETS[(packet.ty - 16) as usize] == 0 => {
                    // Packets for the FFT.
                    self.list_b.push(i);
                    self.sub_packets_b += 1;
                }
                _ => (),
            }

            i += 1;
        }

        if !self.list_d.is_empty() {
            self.process_synthesis_subpackets(buf)?;
            self.do_synth_filter = true;
        }
        else if self.do_synth_filter {
            self.process_subpacket_10(buf, None)?;
            self.process_subpacket_11(buf, None)?;
            self.process_subpacket_12(buf, None)?;
        }

        Ok(())
    }

    /// `qdm2_fft_init_coefficient`.
    fn fft_init_coefficient(
        &mut self,
        sub_packet: i32,
        offset: i32,
        duration: usize,
        channel: usize,
        exp: i32,
        phase: i32,
    ) {
        if self.fft_coefs_min_index[duration] < 0 {
            self.fft_coefs_min_index[duration] = self.fft_coefs_index as i32;
        }

        self.fft_coefs[self.fft_coefs_index] = FftCoefficient {
            sub_packet: (if sub_packet >= 16 { sub_packet - 16 } else { sub_packet }) as i16,
            channel: channel as u8,
            offset: offset as i16,
            exp: exp as i16,
            phase: phase as u8,
        };
        self.fft_coefs_index += 1;
    }

    /// `qdm2_fft_decode_tones`.
    fn fft_decode_tones(&mut self, duration: usize, gb: &mut Bits<'_>, b: bool) -> Result<()> {
        let group_size = self.group_size as i32;
        let duration_i = duration as i32;

        let mut local_int_4 = 0i32;
        let mut local_int_28 = 0i32;
        let local_int_20 = 2i32;
        let local_int_8 = 4 - duration_i;
        let local_int_10 = 1i32 << (self.group_order as i32 - duration_i - 1);
        let mut offset = 1i32;

        let vlc_idx = local_int_8 as usize;

        while gb.bits_left() > 0 {
            if self.superblocktype_2_3 {
                loop {
                    let n = get_vlc(gb, &self.vlcs.fft_tone_offset[vlc_idx], true);
                    if n >= 2 {
                        offset += n - 2;
                        break;
                    }

                    if gb.bits_left() < 0 {
                        return decode_error("qdesign: overread in fft tones");
                    }
                    offset = 1;
                    if n == 0 {
                        local_int_4 += local_int_10;
                        local_int_28 += 1 << local_int_8;
                    }
                    else {
                        local_int_4 += 8 * local_int_10;
                        local_int_28 += 8 << local_int_8;
                    }
                }
            }
            else {
                if local_int_10 <= 2 {
                    return decode_error("qdesign: fft tones stuck");
                }
                offset += get_vlc(gb, &self.vlcs.fft_tone_offset[vlc_idx], true);
                while offset >= local_int_10 - 1 {
                    offset += 1 - (local_int_10 - 1);
                    local_int_4 += local_int_10;
                    local_int_28 += 1 << local_int_8;
                }
            }

            if local_int_4 >= group_size {
                return decode_error("qdesign: fft tone position out of range");
            }

            let local_int_14 = offset >> local_int_8;
            if local_int_14 < 0 || local_int_14 as usize >= FFT_LEVEL_INDEX_TABLE.len() {
                return decode_error("qdesign: fft tone offset out of range");
            }

            let (channel, stereo) = if self.channels > 1 {
                let channel = gb.get1() as usize;
                let stereo = gb.get1() as usize;
                (channel, stereo)
            }
            else {
                (0, 0)
            };

            let exp_vlc = if b { &self.vlcs.fft_level_exp } else { &self.vlcs.fft_level_exp_alt };
            let mut exp = get_vlc(gb, exp_vlc, false);
            exp += self.fft_level_exp[usize::from(FFT_LEVEL_INDEX_TABLE[local_int_14 as usize])];
            exp = exp.max(0);

            let phase = gb.get(3) as i32;
            let mut stereo_exp = 0;
            let mut stereo_phase = 0;

            if stereo != 0 {
                stereo_exp = exp - get_vlc(gb, &self.vlcs.fft_stereo_exp, false);
                stereo_phase = phase - get_vlc(gb, &self.vlcs.fft_stereo_phase, false);
                if stereo_phase < 0 {
                    stereo_phase += 8;
                }
            }

            if self.frequency_range > local_int_14 + 1 {
                let sub_packet = local_int_20 + local_int_28;

                if self.fft_coefs_index + stereo >= MAX_TONES {
                    return decode_error("qdesign: too many fft coefficients");
                }

                self.fft_init_coefficient(sub_packet, offset, duration, channel, exp, phase);
                if stereo != 0 {
                    self.fft_init_coefficient(
                        sub_packet,
                        offset,
                        duration,
                        1 - channel,
                        stereo_exp,
                        stereo_phase,
                    );
                }
            }
            offset += 1;
        }

        Ok(())
    }

    /// `qdm2_decode_fft_packets`.
    fn decode_fft_packets(&mut self, buf: &[u8]) -> Result<()> {
        if self.list_b.is_empty() {
            return decode_error("qdesign: no fft packets");
        }

        // Reset the minimum indexes for the FFT coefficients.
        self.fft_coefs_index = 0;
        self.fft_coefs_min_index = [-1; 5];

        // Process subpackets ordered by type, largest type first.
        let mut max = 256;

        for i in 0..self.sub_packets_b {
            let mut packet = None;
            let mut min = 0;

            // Find the subpacket with the largest type less than max.
            for j in 0..self.sub_packets_b {
                let p = self.sub_packets[self.list_b[j]];
                if p.ty > min && p.ty < max {
                    min = p.ty;
                    packet = Some(p);
                }
            }

            max = min;

            // Check for errors (?).
            let Some(packet) = packet
            else {
                return decode_error("qdesign: missing fft packet");
            };

            if i == 0
                && (packet.ty < 16
                    || packet.ty >= 48
                    || FFT_SUBPACKETS[(packet.ty - 16) as usize] != 0)
            {
                return decode_error("qdesign: invalid fft packet");
            }

            // Decode the FFT tones.
            let mut gb = packet.reader(buf)?;

            let unknown_flag =
                packet.ty >= 32 && packet.ty < 48 && FFT_SUBPACKETS[(packet.ty - 16) as usize] == 0;

            let ty = packet.ty;

            // The results of the tone decoding are ignored by FFmpeg.
            if (17..24).contains(&ty) || (33..40).contains(&ty) {
                let duration = self.sub_sampling as i32 + 5 - (ty & 15);

                if (0..4).contains(&duration) {
                    let _ = self.fft_decode_tones(duration as usize, &mut gb, unknown_flag);
                }
            }
            else if ty == 31 {
                for j in 0..4 {
                    let _ = self.fft_decode_tones(j, &mut gb, unknown_flag);
                }
            }
            else if ty == 46 {
                for e in self.fft_level_exp.iter_mut() {
                    *e = gb.get(6) as i32;
                }
                for j in 0..4 {
                    let _ = self.fft_decode_tones(j, &mut gb, unknown_flag);
                }
            }
        }

        // Calculate the maximum indexes for the FFT coefficients.
        let mut j = -1i32;
        for i in 0..5 {
            if self.fft_coefs_min_index[i] >= 0 {
                if j >= 0 {
                    self.fft_coefs_max_index[j as usize] = self.fft_coefs_min_index[i];
                }
                j = i as i32;
            }
        }
        if j >= 0 {
            self.fft_coefs_max_index[j as usize] = self.fft_coefs_index as i32;
        }

        Ok(())
    }

    /// `qdm2_fft_generate_tone`.
    fn fft_generate_tone(&mut self, tone: &mut FftTone) {
        const ISCALE: f64 = 2.0 * std::f64::consts::PI / 512.0;

        tone.phase += tone.phase_shift;

        // Calculate the current level (maximum amplitude) of the tone.
        let level = FFT_TONE_ENVELOPE_TABLE[tone.duration][tone.time_index as usize] * tone.level;
        let angle = f64::from(tone.phase) * ISCALE;
        let c = Complex {
            re: (f64::from(level) * angle.cos()) as f32,
            im: (f64::from(level) * angle.sin()) as f32,
        };

        let cx = &mut self.fft_complex[tone.ch];
        let base = tone.offset;

        // Generate the FFT coefficients for the tone.
        if tone.duration >= 3 || tone.cutoff >= 3 {
            cx[base].im += c.im;
            cx[base].re += c.re;
            cx[base + 1].im -= c.im;
            cx[base + 1].re -= c.re;
        }
        else {
            let t = &tone.table;
            let f = [
                t[3] - t[0],
                -t[4],
                (1.0 - f64::from(t[2]) - f64::from(t[3])) as f32,
                (f64::from(t[1] + t[4]) - 1.0) as f32,
                t[0] - t[1],
                t[2],
            ];

            for (i, &fi) in f.iter().take(2).enumerate() {
                let idx = FFT_CUTOFF_INDEX_TABLE[tone.cutoff as usize][i];
                let idx = (base as i32 + idx) as usize;
                cx[idx].re += c.re * fi;
                cx[idx].im += c.im * if tone.cutoff <= i as i32 { -fi } else { fi };
            }
            for i in 0..4 {
                cx[base + i].re += c.re * f[i + 2];
                cx[base + i].im += c.im * f[i + 2];
            }
        }

        // Copy the tone if it has not yet died out.
        tone.time_index += 1;
        if tone.time_index < (1 << (5 - tone.duration)) - 1 {
            self.fft_tones[self.fft_tone_end] = *tone;
            self.fft_tone_end = (self.fft_tone_end + 1) % MAX_TONES;
        }
    }

    fn tone_level_for(&self, exp: i16) -> f32 {
        if exp < 0 {
            0.0
        }
        else {
            FFT_TONE_LEVEL_TABLE[usize::from(!self.superblocktype_2_3)][(exp & 63) as usize]
        }
    }

    /// `qdm2_fft_tone_synthesizer`.
    fn fft_tone_synthesizer(&mut self, sub_packet: usize) {
        const ISCALE: f64 = 0.25 * std::f64::consts::PI;

        for ch in 0..self.channels {
            self.fft_complex[ch][..self.fft_size].fill(Complex::default());
        }

        // Apply FFT tones with duration 4 (1 FFT period).
        if self.fft_coefs_min_index[4] >= 0 {
            for i in self.fft_coefs_min_index[4]..self.fft_coefs_max_index[4] {
                let coef = self.fft_coefs[i as usize];

                if coef.sub_packet as usize != sub_packet {
                    break;
                }

                let ch = if self.channels == 1 { 0 } else { usize::from(coef.channel) };
                let level = self.tone_level_for(coef.exp);

                let angle = f64::from(coef.phase) * ISCALE;
                let re = (f64::from(level) * angle.cos()) as f32;
                let im = (f64::from(level) * angle.sin()) as f32;

                let off = coef.offset as usize;
                let cx = &mut self.fft_complex[ch];
                cx[off].re += re;
                cx[off].im += im;
                cx[off + 1].re -= re;
                cx[off + 1].im -= im;
            }
        }

        // Generate existing FFT tones. The end is captured at the start, as in FFmpeg.
        let end = self.fft_tone_end;
        while end != self.fft_tone_start {
            let mut tone = self.fft_tones[self.fft_tone_start];
            self.fft_generate_tone(&mut tone);
            self.fft_tone_start = (self.fft_tone_start + 1) % MAX_TONES;
        }

        // Create and generate new FFT tones with duration 0 (long) to 3 (short).
        for i in 0..4 {
            if self.fft_coefs_min_index[i] < 0 {
                continue;
            }

            let mut j = self.fft_coefs_min_index[i];
            while j < self.fft_coefs_max_index[i] {
                let coef = self.fft_coefs[j as usize];

                if coef.sub_packet as usize != sub_packet {
                    break;
                }

                let four_i = 4 - i as i32;
                let offset = i32::from(coef.offset) >> four_i;
                let ch = if self.channels == 1 { 0 } else { usize::from(coef.channel) };

                if offset < self.frequency_range {
                    let cutoff = if offset < 2 {
                        offset
                    }
                    else if offset >= 60 {
                        3
                    }
                    else {
                        2
                    };

                    let sub = (i32::from(coef.offset) - (offset << four_i)) as usize;

                    let mut tone = FftTone {
                        level: self.tone_level_for(coef.exp),
                        ch,
                        offset: offset as usize,
                        table: FFT_TONE_SAMPLE_TABLE[i][sub],
                        phase: 64 * i32::from(coef.phase) - (offset << 8) - 128,
                        phase_shift: (2 * i32::from(coef.offset) + 1) << (7 - four_i),
                        duration: i,
                        time_index: 0,
                        cutoff,
                    };

                    self.fft_generate_tone(&mut tone);
                }
                j += 1;
            }
            self.fft_coefs_min_index[i] = j;
        }
    }

    /// `qdm2_calculate_fft`: the inverse real FFT of a channel, added to the output buffer.
    ///
    /// The transform is FFmpeg's inverse `AV_TX_FLOAT_RDFT` (`ff_tx_rdft_c2r`), of length `2 * fft_size`: a complex inverse FFT of length `fft_size` that is
    /// pre-processed by the real-to-complex post-processing step.
    fn calculate_fft(&mut self, ch: usize) {
        let n = self.fft_size;
        let len = 2 * n;
        let len2 = n;
        let len4 = n / 2;

        let cx = &mut self.fft_complex[ch];
        cx[0].re *= 2.0;
        cx[0].im = 0.0;
        cx[n].re = 0.0;
        cx[n].im = 0.0;

        // The combined factors of the inverse transform, with `m = 1` (`ff_tx_rdft_init`).
        const FACT: [f32; 8] = [0.5, 0.5, 1.0, -1.0, 0.5, -0.5, -0.5, 0.5];

        let data = &mut self.fft_scratch;
        data.copy_from_slice(&cx[..n]);
        let last = cx[n];

        // data[len2] is only used for its real part, which is stored in data[0].im.
        data[0].im = last.re;

        let t0 = data[0].re;
        data[0].re = t0 + data[0].im;
        data[0].im = t0 - data[0].im;
        data[0].re *= FACT[0];
        data[0].im *= FACT[1];
        data[len4].re *= FACT[2];
        data[len4].im *= FACT[3];

        let f = 2.0 * std::f64::consts::PI / len as f64;

        for i in 1..len4 {
            let a = data[i];
            let b = data[len2 - i];

            let t0re = FACT[4] * (a.re + b.re);
            let t0im = FACT[5] * (a.im - b.im);
            let t1re = FACT[6] * (a.im + b.im);
            let t1im = FACT[7] * (a.re - b.re);

            let tcos = (i as f64 * f).cos() as f32;
            let tsin = (((len - i * 4) as f64 / 4.0) * f).cos() as f32;

            // Apply the twiddle factors to the odd FFT and add to the even FFT.
            let t2re = t1re * tcos - t1im * tsin;
            let t2im = t1re * tsin + t1im * tcos;

            data[i] = Complex { re: t0re + t2re, im: t2im - t0im };
            data[len2 - i] = Complex { re: t0re - t2re, im: t2im + t0im };
        }

        // The inverse FFT of FFmpeg is not normalised, but the Symphonia one is.
        self.ifft.ifft(&self.fft_scratch, &mut self.fft_temp);
        let scale = n as f32;

        let gain = 1.0f32;
        let channels = self.channels;
        let mut idx = ch;

        for i in 0..n {
            let t = self.fft_temp[i];
            self.output_buffer[idx] += t.re * scale * gain;
            self.output_buffer[idx + channels] += t.im * scale * gain;
            idx += 2 * channels;
        }
    }

    /// `qdm2_synthesis_filter`.
    fn synthesis_filter(&mut self, index: usize) {
        // Copy the subband samples.
        let sb_used = self.sb_used();

        for ch in 0..self.channels {
            for i in 0..8 {
                for k in sb_used..SBLIMIT {
                    self.sb_samples[ch * 128 + 8 * index + i][k] = 0.0;
                }
            }
        }

        for ch in 0..self.channels {
            let mut ptr = ch;

            for i in 0..8 {
                self.synth[ch].synth(
                    &self.window,
                    &self.sb_samples[ch * 128 + 8 * index + i],
                    &mut self.samples[ptr..],
                    self.channels,
                );
                ptr += 32 * self.channels;
            }
        }

        // Add the samples to the output buffer.
        let sub_sampling = 4 >> self.sub_sampling;

        for ch in 0..self.channels {
            for i in 0..self.frame_size {
                self.output_buffer[self.channels * i + ch] +=
                    8388608.0 * self.samples[self.channels * sub_sampling * i + ch];
            }
        }
    }
}
