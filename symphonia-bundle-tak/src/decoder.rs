// Symphonia TAK Bundle
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The TAK frame decoder.
//!
//! Ported from FFmpeg's `libavcodec/takdec.c` and `libavcodec/takdsp.c` (LGPL-2.1+, Paul B Mahol).
//! The arithmetic is the 32-bit wrapping arithmetic of the reference, including the truncations to
//! 16 bits, so that the output is bit-exact with it.

use symphonia_core::audio::{
    AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_TAK;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use crate::bits::BitReader;
use crate::info::{CODEC_MONO_STEREO, CODEC_MULTICHANNEL, FrameHeader, StreamInfo, check_crc};
use crate::tables::{MC_DMODES, PREDICTOR_SIZES, XCODES};

/// The most samples a frame can hold.
const MAX_FRAME_SAMPLES: usize = 16384;
/// The most channels the decoder supports.
const MAX_CHANNELS: usize = 6;
/// The most predictors a subframe has.
const MAX_PREDICTORS: usize = 256;
/// The size of the window of residues of the predictor filters.
const RESIDUES_LEN: usize = 544;
/// The most subframes per channel.
const MAX_SUBFRAMES: usize = 8;
/// The size of the shortest frame header, in bytes.
const MIN_FRAME_HEADER_BYTES: usize = 8;

/// The parameters of the multichannel decorrelation of a channel (`MCDParam`).
#[derive(Copy, Clone, Default)]
struct McdParam {
    present: bool,
    index: usize,
    chan1: usize,
    chan2: usize,
}

/// The state that the decoding of a frame uses, other than the samples.
struct Scratch {
    /// The size in samples of the segments of residues (`uval`).
    uval: usize,
    subframe_scale: i32,
    bps: u32,
    nb_samples: usize,
    coding_mode: [i8; 128],
    predictors: [i16; MAX_PREDICTORS],
    filter: [i16; MAX_PREDICTORS],
    tfilter: [u32; MAX_PREDICTORS],
    residues: [i16; RESIDUES_LEN],
}

fn align_up_4(v: u32) -> u32 {
    (v + 3) & !3
}

/// The `get_bits_esc4` of the reference: an optional 4-bit value, plus one.
fn get_bits_esc4(br: &mut BitReader<'_>) -> u32 {
    if br.get1() { br.get(4) + 1 } else { 0 }
}

/// Sums the products of 16-bit values in 32-bit wrapping arithmetic.
#[inline]
fn dot(a: &[i16], b: &[i16]) -> i32 {
    a.iter()
        .zip(b)
        .fold(0i32, |s, (&x, &y)| s.wrapping_add(i32::from(x).wrapping_mul(i32::from(y))))
}

/// Clips to the range of a signed 13-bit integer (`av_clip_intp2(v, 13)`).
#[inline]
fn clip13(v: i32) -> i32 {
    v.clamp(-(1 << 13), (1 << 13) - 1)
}

/// Returns the buffers of two distinct channels, in the order of the arguments.
fn pair_mut(chans: &mut [Vec<i32>], c1: usize, c2: usize) -> Result<(&mut [i32], &mut [i32])> {
    if c1 == c2 || c1 >= chans.len() || c2 >= chans.len() {
        return decode_error("tak: invalid decorrelation channels");
    }

    if c1 < c2 {
        let (lo, hi) = chans.split_at_mut(c2);
        Ok((&mut lo[c1], &mut hi[0]))
    }
    else {
        let (lo, hi) = chans.split_at_mut(c1);
        Ok((&mut hi[0], &mut lo[c2]))
    }
}

/// Integrates the first `length` values of `coeffs` (`decode_lpc`).
fn decode_lpc(coeffs: &mut [i32], mode: u32, length: usize) {
    if length < 2 || coeffs.len() < length {
        return;
    }

    match mode {
        1 => {
            let mut a1 = coeffs[0] as u32;
            let mut p = 1;
            for _ in 0..(length - 1) >> 1 {
                coeffs[p] = coeffs[p].wrapping_add(a1 as i32);
                coeffs[p + 1] = coeffs[p + 1].wrapping_add(coeffs[p]);
                a1 = coeffs[p + 1] as u32;
                p += 2;
            }
            if (length - 1) & 1 != 0 {
                coeffs[p] = coeffs[p].wrapping_add(a1 as i32);
            }
        }
        2 => {
            let mut a1 = coeffs[1] as u32;
            let mut a2 = a1.wrapping_add(coeffs[0] as u32);
            coeffs[1] = a2 as i32;
            if length > 2 {
                let mut p = 2;
                for _ in 0..(length - 2) >> 1 {
                    let a3 = (coeffs[p] as u32).wrapping_add(a1);
                    let a4 = a3.wrapping_add(a2);
                    coeffs[p] = a4 as i32;
                    a1 = (coeffs[p + 1] as u32).wrapping_add(a3);
                    a2 = a1.wrapping_add(a4);
                    coeffs[p + 1] = a2 as i32;
                    p += 2;
                }
                if length & 1 != 0 {
                    coeffs[p] = coeffs[p].wrapping_add(a1.wrapping_add(a2) as i32);
                }
            }
        }
        3 => {
            let a1 = coeffs[1] as u32;
            let a2 = a1.wrapping_add(coeffs[0] as u32);
            coeffs[1] = a2 as i32;
            if length > 2 {
                let mut a3 = coeffs[2] as u32;
                let mut a4 = a3.wrapping_add(a1);
                let mut a5 = a4.wrapping_add(a2);
                coeffs[2] = a5 as i32;
                for c in &mut coeffs[3..length] {
                    a3 = a3.wrapping_add(*c as u32);
                    a4 = a4.wrapping_add(a3);
                    a5 = a5.wrapping_add(a4);
                    *c = a5 as i32;
                }
            }
        }
        _ => (),
    }
}

/// Decodes a segment of residues of one coding mode (`decode_segment`).
fn decode_segment(br: &mut BitReader<'_>, mode: i8, out: &mut [i32]) -> Result<()> {
    if mode == 0 {
        out.fill(0);
        return Ok(());
    }

    let code = match usize::try_from(mode).ok().and_then(|m| XCODES.get(m - 1)) {
        Some(code) => code,
        None => return decode_error("tak: invalid coding mode"),
    };

    for o in out.iter_mut() {
        let mut x = br.get(code.init);

        if x >= code.escape && br.get1() {
            x |= 1 << code.init;

            if x >= code.aescape {
                let scale = br.get_unary(9);

                if scale == 9 {
                    let mut scale_bits = br.get(3);

                    if scale_bits > 0 {
                        if scale_bits == 7 {
                            scale_bits += br.get(5);
                            if scale_bits > 29 {
                                return decode_error("tak: invalid residue scale");
                            }
                        }
                        let scale = br.get(scale_bits).wrapping_add(1);
                        x = x.wrapping_add(code.scale.wrapping_mul(scale));
                    }
                    x = x.wrapping_add(code.bias);
                }
                else {
                    x = x.wrapping_add(code.scale.wrapping_mul(scale)).wrapping_sub(code.escape);
                }
            }
            else {
                x = x.wrapping_sub(code.escape);
            }
        }

        *o = ((x >> 1) as i32) ^ -((x & 1) as i32);
    }

    Ok(())
}

/// Decodes `length` residues (`decode_residues`).
fn decode_residues(
    sc: &mut Scratch,
    br: &mut BitReader<'_>,
    out: &mut [i32],
    length: usize,
) -> Result<()> {
    if length > sc.nb_samples {
        return decode_error("tak: invalid residue count");
    }

    let Some(out) = out.get_mut(..length)
    else {
        return decode_error("tak: invalid residue count");
    };

    if br.get1() {
        let uval = sc.uval;

        let mut wlength = length / uval;
        let mut rval = length - wlength * uval;

        if rval < uval / 2 {
            rval += uval;
        }
        else {
            wlength += 1;
        }

        if wlength <= 1 || wlength > 128 {
            return decode_error("tak: invalid residue segment count");
        }

        let mut mode = br.get(6) as i32;
        sc.coding_mode[0] = mode as i8;

        for i in 1..wlength {
            match br.get_unary(6) {
                6 => mode = br.get(6) as i32,
                c @ 3..=5 => {
                    // mode += sign ? 1 - c : c - 1
                    let step = c as i32 - 1;
                    mode += if br.get1() { -step } else { step };
                }
                2 => mode += 1,
                1 => mode -= 1,
                _ => (),
            }
            sc.coding_mode[i] = mode as i8;
        }

        let mut i = 0;
        let mut off = 0;
        while i < wlength {
            let mut len = 0;
            let mode = sc.coding_mode[i];

            loop {
                len += if i >= wlength - 1 { rval } else { uval };
                i += 1;

                if i == wlength || sc.coding_mode[i] != mode {
                    break;
                }
            }

            let Some(seg) = out.get_mut(off..off + len)
            else {
                return decode_error("tak: invalid residue segment");
            };
            decode_segment(br, mode, seg)?;
            off += len;
        }
    }
    else {
        let mode = br.get(6) as i8;
        decode_segment(br, mode, out)?;
    }

    if br.bits_left() < 0 {
        return decode_error("tak: truncated residues");
    }

    Ok(())
}

/// Decodes a subframe of `size` samples at `pos` of the channel `dec` (`decode_subframe`).
fn decode_subframe(
    sc: &mut Scratch,
    br: &mut BitReader<'_>,
    dec: &mut [i32],
    mut pos: usize,
    mut size: usize,
    prev_size: usize,
) -> Result<()> {
    if dec.len() < pos + size {
        return decode_error("tak: invalid subframe");
    }

    if !br.get1() {
        return decode_residues(sc, br, &mut dec[pos..], size);
    }

    let order = PREDICTOR_SIZES[br.get(4) as usize];

    if prev_size > 0 && br.get1() {
        // The first residues are those of the end of the previous subframe.
        if order > prev_size {
            return decode_error("tak: invalid predictor order");
        }

        pos -= order;
        size += order;
    }
    else {
        if order > size {
            return decode_error("tak: invalid predictor order");
        }

        let lpc_mode = br.get(2);
        if lpc_mode > 2 {
            return decode_error("tak: invalid subframe lpc mode");
        }

        decode_residues(sc, br, &mut dec[pos..], order)?;

        if lpc_mode != 0 {
            decode_lpc(&mut dec[pos..pos + order], lpc_mode, order);
        }
    }

    let seg = &mut dec[pos..pos + size];

    let dshift = get_bits_esc4(br);
    let psize = u32::from(br.get1()) + 6;

    let mut filter_quant = 10;
    if br.get1() {
        filter_quant -= br.get(3) + 1;
        if filter_quant < 3 {
            return decode_error("tak: invalid filter quantization");
        }
    }

    if br.bits_left() < i64::from(2 * 10 + 2 * psize) {
        return decode_error("tak: truncated subframe");
    }

    let pscale = 1i32 << (10 - psize);

    sc.predictors[0] = br.get_signed(10) as i16;
    sc.predictors[1] = br.get_signed(10) as i16;
    sc.predictors[2] = br.get_signed(psize).wrapping_mul(pscale) as i16;
    sc.predictors[3] = br.get_signed(psize).wrapping_mul(pscale) as i16;

    if order > 4 {
        let tmp = psize - u32::from(br.get1());
        let mut x = 0;

        for i in 4..order {
            if i & 3 == 0 {
                x = tmp - br.get(2);
            }
            sc.predictors[i] = br.get_signed(x).wrapping_mul(pscale) as i16;
        }
    }

    // Convert the reflection coefficients to those of the direct form filter.
    sc.tfilter[0] = (i32::from(sc.predictors[0]) * 64) as u32;
    for i in 1..order {
        let p = i32::from(sc.predictors[i]) as u32;
        let (mut a, mut b) = (0, i - 1);

        for _ in 0..(i + 1) >> 1 {
            let ta = sc.tfilter[a];
            let tb = sc.tfilter[b];
            let x = ta.wrapping_add(((p.wrapping_mul(tb).wrapping_add(256)) as i32 >> 9) as u32);
            sc.tfilter[b] =
                tb.wrapping_add(((p.wrapping_mul(ta).wrapping_add(256)) as i32 >> 9) as u32);
            sc.tfilter[a] = x;
            a += 1;
            b = b.wrapping_sub(1);
        }

        sc.tfilter[i] = (i32::from(sc.predictors[i]) * 64) as u32;
    }

    let k = 15 - filter_quant;
    let x = 1i32 << (32 - k);
    let y = 1i32 << (k - 1);

    for i in 0..order / 2 {
        let j = order - 1 - i;
        sc.filter[j] = x.wrapping_sub((sc.tfilter[i] as i32).wrapping_add(y) >> k) as i16;
        sc.filter[i] = x.wrapping_sub((sc.tfilter[j] as i32).wrapping_add(y) >> k) as i16;
    }

    decode_residues(sc, br, &mut seg[order..], size - order)?;

    for (r, s) in sc.residues.iter_mut().zip(&seg[..order]) {
        *r = (*s >> dshift) as i16;
    }

    let y = RESIDUES_LEN - order;
    let mut x = size - order;
    let mut dp = order;

    while x > 0 {
        let tmp = y.min(x);

        for i in 0..tmp {
            let v = (1i32 << (filter_quant - 1))
                .wrapping_add(dot(&sc.residues[i..i + order], &sc.filter[..order]));
            let v = clip13(v >> filter_quant).wrapping_mul(1 << dshift).wrapping_sub(seg[dp]);

            seg[dp] = v;
            dp += 1;
            sc.residues[order + i] = (v >> dshift) as i16;
        }

        x -= tmp;
        if x > 0 {
            sc.residues.copy_within(y..y + order, 0);
        }
    }

    Ok(())
}

/// The decoded state of a channel that the last stage of the frame decoding needs.
#[derive(Copy, Clone, Default)]
struct ChannelState {
    lpc_mode: u32,
    sample_shift: u32,
}

/// Decodes a channel (`decode_channel`).
fn decode_channel(
    sc: &mut Scratch,
    br: &mut BitReader<'_>,
    dec: &mut [i32],
    state: &mut ChannelState,
) -> Result<()> {
    state.sample_shift = get_bits_esc4(br);
    if state.sample_shift >= sc.bps {
        return decode_error("tak: invalid sample shift");
    }

    dec[0] = br.get_signed(sc.bps - state.sample_shift);
    state.lpc_mode = br.get(2);
    let nb_subframes = br.get(3) as usize + 1;

    let mut left = sc.nb_samples as i32 - 1;
    let mut subframe_len = [0i32; MAX_SUBFRAMES];

    if nb_subframes > 1 {
        if br.bits_left() < ((nb_subframes - 1) * 6) as i64 {
            return decode_error("tak: truncated channel");
        }

        let mut prev = 0;
        for len in subframe_len.iter_mut().take(nb_subframes - 1) {
            let v = br.get(6) as i32;

            // The reference holds the lengths in 16 bits.
            *len = i32::from(((v - prev) * sc.subframe_scale) as i16);
            if *len <= 0 {
                return decode_error("tak: invalid subframe length");
            }

            left -= *len;
            prev = v;
        }

        if left <= 0 {
            return decode_error("tak: invalid subframe length");
        }
    }
    subframe_len[nb_subframes - 1] = left;

    let mut pos = 1;
    let mut prev = 0;
    for &len in subframe_len.iter().take(nb_subframes) {
        decode_subframe(sc, br, dec, pos, len as usize, prev)?;
        pos += len as usize;
        prev = len as usize;
    }

    Ok(())
}

/// Decorrelates two channels (`decorrelate`, and the functions of `takdsp.c`).
fn decorrelate(
    sc: &mut Scratch,
    br: &mut BitReader<'_>,
    chans: &mut [Vec<i32>],
    c1: usize,
    c2: usize,
    dmode: u32,
) -> Result<()> {
    let nb_samples = sc.nb_samples;
    let (a, b) = pair_mut(chans, c1, c2)?;

    let off = usize::from(dmode > 5);
    let (mut p1, mut p2) = (&mut a[off..nb_samples], &mut b[off..nb_samples]);
    let (mut bp1, mut bp2) = (p1[0], p2[0]);

    // The modes below 6 decorrelate the first sample too, and restore it later: the slices hold
    // `nb_samples` samples for them, and `nb_samples - 1` for the others.
    let length = p1.len();

    match dmode {
        1 => {
            // Left/side.
            for (a, b) in p1.iter().zip(p2.iter_mut()) {
                *b = a.wrapping_add(*b);
            }
        }
        2 => {
            // Side/right.
            for (a, b) in p1.iter_mut().zip(p2.iter()) {
                *a = b.wrapping_sub(*a);
            }
        }
        3 => {
            // Side/mid.
            for (a, b) in p1.iter_mut().zip(p2.iter_mut()) {
                let side = (*a as u32).wrapping_sub((*b >> 1) as u32);
                *a = side as i32;
                *b = side.wrapping_add(*b as u32) as i32;
            }
        }
        4 | 5 => {
            // Side/left and side/right with a scale factor.
            if dmode == 4 {
                std::mem::swap(&mut p1, &mut p2);
                std::mem::swap(&mut bp1, &mut bp2);
            }

            let dshift = get_bits_esc4(br);
            let dfactor = br.get_signed(10) as u32;

            for (a, b) in p1.iter_mut().zip(p2.iter()) {
                let scaled = dfactor.wrapping_mul((*b >> dshift) as u32).wrapping_add(128);
                let scaled = ((scaled as i32) >> 8) as u32;
                *a = (scaled << dshift).wrapping_sub(*a as u32) as i32;
            }
        }
        6 | 7 => {
            if dmode == 6 {
                std::mem::swap(&mut p1, &mut p2);
            }

            if length < 256 {
                return decode_error("tak: too short for the decorrelation filter");
            }

            let dshift = get_bits_esc4(br);
            let order = 8usize << br.get1() as usize;
            let dval1 = br.get1();
            let dval2 = br.get1();

            let mut code_size = 0;
            for i in 0..order {
                if i & 3 == 0 {
                    code_size = 14 - br.get(3);
                }
                sc.filter[i] = br.get_signed(code_size) as i16;
            }

            let half = order / 2;
            let length2 = length - (order - 1);

            // Decorrelate the beginning and the ending samples, that the filter does not reach.
            if dval1 {
                for i in 0..half {
                    p1[i] = p1[i].wrapping_add(p2[i]);
                }
            }
            if dval2 {
                for i in length2 + half..length {
                    p1[i] = p1[i].wrapping_add(p2[i]);
                }
            }

            for (r, s) in sc.residues.iter_mut().zip(&p2[..order]) {
                *r = (*s >> dshift) as i16;
            }

            let mut p2_pos = order;
            let mut p1_pos = half;
            let max_chunk = RESIDUES_LEN - order;
            let mut remaining = length2;

            while remaining > 0 {
                let tmp = remaining.min(max_chunk);

                for i in 0..tmp - usize::from(tmp == remaining) {
                    sc.residues[order + i] = (p2[p2_pos] >> dshift) as i16;
                    p2_pos += 1;
                }

                for i in 0..tmp {
                    let v =
                        512i32.wrapping_add(dot(&sc.residues[i..i + order], &sc.filter[..order]));
                    p1[p1_pos] = clip13(v >> 10).wrapping_mul(1 << dshift).wrapping_sub(p1[p1_pos]);
                    p1_pos += 1;
                }

                sc.residues.copy_within(tmp..tmp + order, 0);
                remaining -= tmp;
            }
        }
        _ => (),
    }

    if (1..6).contains(&dmode) {
        p1[0] = bp1;
        p2[0] = bp2;
    }

    Ok(())
}

/// Checks that the stream information is decodable (the checks of `tak_decode_frame`).
fn validate_info(info: &StreamInfo) -> Result<()> {
    if info.codec != CODEC_MONO_STEREO && info.codec != CODEC_MULTICHANNEL {
        return unsupported_error("tak: unsupported codec type");
    }
    if info.data_type != 0 {
        return decode_error("tak: unsupported data type");
    }
    if info.codec == CODEC_MONO_STEREO && info.channels > 2 {
        return decode_error("tak: invalid number of channels");
    }
    if info.channels as usize > MAX_CHANNELS {
        return unsupported_error("tak: unsupported number of channels");
    }
    if info.frame_samples == 0 || info.frame_samples as usize > MAX_FRAME_SAMPLES {
        return decode_error("tak: invalid frame size");
    }
    if !matches!(info.bps, 8 | 16 | 24) {
        return unsupported_error("tak: unsupported bits per sample");
    }
    Ok(())
}

/// TAK decoder.
pub struct TakDecoder {
    params: AudioCodecParameters,
    buf: AudioBuffer<i32>,
    verify: bool,
    /// The current stream information; frame headers that hold it replace it.
    info: StreamInfo,
    sc: Scratch,
    chans: Vec<Vec<i32>>,
    states: [ChannelState; MAX_CHANNELS],
    mcd: [McdParam; MAX_CHANNELS],
}

impl TakDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_TAK {
            return unsupported_error("tak: invalid codec");
        }

        let extra = match params.extra_data.as_deref() {
            Some(extra) => extra,
            None => return decode_error("tak: missing stream info"),
        };
        let info = StreamInfo::from_bytes(extra)?;
        validate_info(&info)?;

        let Some(channels) = params.channels.clone()
        else {
            return decode_error("tak: missing channels");
        };

        if channels.count() != info.channels as usize {
            return decode_error("tak: channels do not match the stream info");
        }

        let sample_rate = info.sample_rate;
        let shift = match sample_rate {
            0..=11024 => 3,
            11025..=22049 => 2,
            22050..=44099 => 1,
            _ => 0,
        };
        let base = align_up_4((sample_rate + 511) >> 9);

        let sc = Scratch {
            uval: (base << shift) as usize,
            subframe_scale: (base << 1) as i32,
            bps: info.bps,
            nb_samples: 0,
            coding_mode: [0; 128],
            predictors: [0; MAX_PREDICTORS],
            filter: [0; MAX_PREDICTORS],
            tfilter: [0; MAX_PREDICTORS],
            residues: [0; RESIDUES_LEN],
        };

        let spec = AudioSpec::new(sample_rate, channels);

        Ok(TakDecoder {
            params: params.clone(),
            buf: AudioBuffer::new(spec, MAX_FRAME_SAMPLES),
            verify: opts.verify,
            chans: vec![vec![0; MAX_FRAME_SAMPLES]; info.channels as usize],
            info,
            sc,
            states: Default::default(),
            mcd: Default::default(),
        })
    }

    /// Decodes the frame in `data` to the channel buffers. Returns the number of samples.
    fn decode_frame(&mut self, data: &[u8]) -> Result<usize> {
        if data.len() < MIN_FRAME_HEADER_BYTES {
            return decode_error("tak: packet too short");
        }

        let header = FrameHeader::parse(data)?;

        if let Some(info) = header.info {
            // The streams hold the same stream information in every frame that has it.
            if info.sample_rate != self.info.sample_rate
                || info.channels != self.info.channels
                || info.bps != self.info.bps
            {
                return decode_error("tak: the stream parameters changed");
            }
            validate_info(&info)?;
            self.info = info;
        }

        validate_info(&self.info)?;

        let nb_samples = if header.last_frame_samples != 0 {
            header.last_frame_samples
        }
        else {
            self.info.frame_samples
        } as usize;

        if nb_samples > MAX_FRAME_SAMPLES {
            return decode_error("tak: invalid frame size");
        }

        let nb_channels = self.info.channels as usize;
        let bps = self.info.bps;

        self.sc.nb_samples = nb_samples;
        self.sc.bps = bps;

        let mut br = BitReader::new(data);
        br.skip(header.size * 8);

        let Self { sc, chans, states, mcd, info, .. } = self;

        if nb_samples < 16 {
            // Too short to be predicted: the samples are stored as they are.
            for chan in chans.iter_mut().take(nb_channels) {
                for s in &mut chan[..nb_samples] {
                    *s = br.get_signed(bps);
                }
            }
        }
        else if info.codec == CODEC_MONO_STEREO {
            for c in 0..nb_channels {
                decode_channel(sc, &mut br, &mut chans[c], &mut states[c])?;
            }

            if nb_channels == 2 {
                // The reference reads, and does not use, the layout of the subframes.
                if br.get1() {
                    br.get(6);
                }

                let dmode = br.get(3);
                decorrelate(sc, &mut br, chans, 0, 1, dmode)?;
            }
        }
        else {
            let count;

            if br.get1() {
                let mut ch_mask = 0u32;

                count = br.get(4) as usize + 1;
                if count > nb_channels {
                    return decode_error("tak: invalid decorrelation channel count");
                }

                for m in mcd.iter_mut().take(count) {
                    let nbit = br.get(4) as usize;

                    if nbit >= nb_channels || ch_mask & (1 << nbit) != 0 {
                        return decode_error("tak: invalid decorrelation channel");
                    }

                    m.present = br.get1();
                    if m.present {
                        m.index = br.get(2) as usize;
                        m.chan2 = br.get(4) as usize;

                        if m.chan2 >= nb_channels {
                            return decode_error("tak: invalid decorrelation channel");
                        }

                        if m.index == 1 {
                            if nbit == m.chan2 || ch_mask & (1 << m.chan2) != 0 {
                                return decode_error("tak: invalid decorrelation channel");
                            }
                            ch_mask |= 1 << m.chan2;
                        }
                        else if ch_mask & (1 << m.chan2) == 0 {
                            return decode_error("tak: invalid decorrelation channel");
                        }
                    }

                    m.chan1 = nbit;
                    ch_mask |= 1 << nbit;
                }
            }
            else {
                count = nb_channels;
                for (i, m) in mcd.iter_mut().enumerate().take(count) {
                    m.present = false;
                    m.chan1 = i;
                }
            }

            for &m in &mcd[..count] {
                if m.present && m.index == 1 {
                    decode_channel(sc, &mut br, &mut chans[m.chan2], &mut states[m.chan2])?;
                }

                decode_channel(sc, &mut br, &mut chans[m.chan1], &mut states[m.chan1])?;

                if m.present {
                    decorrelate(sc, &mut br, chans, m.chan2, m.chan1, MC_DMODES[m.index])?;
                }
            }
        }

        if nb_samples >= 16 {
            for (chan, state) in chans.iter_mut().zip(states.iter()).take(nb_channels) {
                let chan = &mut chan[..nb_samples];

                if state.lpc_mode != 0 {
                    decode_lpc(chan, state.lpc_mode, nb_samples);
                }

                if state.sample_shift > 0 {
                    for s in chan.iter_mut() {
                        *s = s.wrapping_mul(1 << state.sample_shift);
                    }
                }
            }
        }

        br.align();
        br.skip(24);

        if br.bits_left() < 0 {
            return decode_error("tak: truncated frame");
        }

        if self.verify && !check_crc(&data[header.size..br.bit_pos() / 8]) {
            return decode_error("tak: frame crc mismatch");
        }

        Ok(nb_samples)
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let nb_samples = self.decode_frame(packet.data)?;

        self.buf.clear();
        self.buf.render_uninit(Some(nb_samples));

        // The samples are converted as the reference does for its sample formats: unsigned 8-bit,
        // signed 16-bit, and the 24-bit samples in the upper bits of 32 bits; then they are
        // normalized to the 32-bit width.
        let bps = self.info.bps;

        for (c, chan) in self.chans.iter().enumerate() {
            let Some(plane) = self.buf.plane_mut(c)
            else {
                return decode_error("tak: invalid channel count");
            };

            for (o, &s) in plane.iter_mut().zip(&chan[..nb_samples]) {
                *o = match bps {
                    8 => i32::from(s as i8) << 24,
                    16 => i32::from(s as i16) << 16,
                    _ => s.wrapping_shl(8),
                };
            }
        }

        Ok(())
    }
}

impl AudioDecoder for TakDecoder {
    fn codec_info(&self) -> &CodecInfo {
        &Self::supported_codecs()[0].info
    }

    fn reset(&mut self) {
        // The frames are independent of each other.
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    fn decode_ref(&mut self, packet: &PacketRef<'_>) -> Result<GenericAudioBufferRef<'_>> {
        if let Err(e) = self.decode_inner(packet) {
            self.buf.clear();
            Err(e)
        }
        else {
            Ok(self.buf.as_generic_audio_buffer_ref())
        }
    }

    fn finalize(&mut self) -> FinalizeResult {
        Default::default()
    }

    fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
        self.buf.as_generic_audio_buffer_ref()
    }
}

impl RegisterableAudioDecoder for TakDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(TakDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_TAK, "tak", "TAK (Tom's lossless Audio Kompressor)")]
    }
}
