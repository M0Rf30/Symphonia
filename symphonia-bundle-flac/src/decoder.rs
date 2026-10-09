// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::cmp;
use std::convert::TryInto;

use symphonia_common::xiph::audio::flac::StreamInfo;
use symphonia_core::audio::{
    AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_FLAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoderOptions};
use symphonia_core::codecs::audio::{AudioDecoder, FinalizeResult, VerificationCheck};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Error, Result, decode_error, unsupported_error};
use symphonia_core::io::{BitReaderLtr, BufReader, ReadBitsLtr};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;
use symphonia_core::util::bits::sign_extend_leq32_to_i32;

use log::{debug, log_enabled, warn};

use super::frame::*;
use super::validate::Validator;

fn decorrelate_left_side(left: &[i32], side: &mut [i32]) {
    for (s, l) in side.iter_mut().zip(left) {
        *s = *l - *s;
    }
}

fn decorrelate_mid_side(mid: &mut [i32], side: &mut [i32]) {
    for (m, s) in mid.iter_mut().zip(side) {
        // Mid (M) is given as M = L/2 + R/2, while Side (S) is given as S = L - R.
        //
        // To calculate the individual channels, the following equations can be used:
        //      - L = S/2 + M
        //      - R = M - S/2
        //
        // Ideally, this would work, but since samples are represented as integers, division yields
        // the floor of the divided value. Therefore, the channel restoration equations actually
        // yield:
        //      - L = floor(S/2) + M
        //      - R = M - floor(S/2)
        //
        // This will produce incorrect samples whenever the sample S is odd. For example:
        //      - 2/2 = 1
        //      - 3/2 = 1 (should be 2 if rounded!)
        //
        // To get the proper rounding behaviour, the solution is to add one to the result if S is
        // odd:
        //      - L = floor(S/2) + M + (S%2) = M + (S%2) + floor(S/2)
        //      - R = M - floor(S/2) + (S%2) = M + (S%2) - floor(S/2)
        //
        // Further, to prevent loss of accuracy, instead of dividing S/2 and adding or subtracting
        // it from M, multiply M*2, then add or subtract S, and then divide the whole result by 2.
        // This gives one extra bit of precision for the intermediate computations.
        //
        // Conveniently, since M should be doubled, the LSB will always be 0. This allows S%2 to
        // be added simply by bitwise ORing S&1 to M<<1.
        //
        // Therefore the final equations yield:
        //      - L = (2*M + (S%2) + S) / 2
        //      - R = (2*M + (S%2) - S) / 2
        let mid = (*m << 1) | (*s & 1);
        let side = *s;
        *m = (mid + side) >> 1;
        *s = (mid - side) >> 1;
    }
}

fn decorrelate_right_side(right: &[i32], side: &mut [i32]) {
    for (s, r) in side.iter_mut().zip(right) {
        *s += *r;
    }
}

/// Free Lossless Audio Codec (FLAC) decoder.
pub struct FlacDecoder {
    params: AudioCodecParameters,
    is_validating: bool,
    validator: Validator,
    buf: AudioBuffer<i32>,
}

impl FlacDecoder {
    pub fn try_new(params: &AudioCodecParameters, options: &AudioDecoderOptions) -> Result<Self> {
        // This decoder only supports FLAC.
        if params.codec != CODEC_ID_FLAC {
            return unsupported_error("flac: invalid codec");
        }

        // Obtain the extra data.
        let extra_data = match params.extra_data.as_ref() {
            Some(buf) => buf,
            _ => return unsupported_error("flac: missing extra data"),
        };

        // Read the stream information block.
        let info = StreamInfo::read(&mut BufReader::new(extra_data))?;

        // Clone the codec parameters so that the parameters can be supplemented and/or amended.
        let mut params = params.clone();

        // Amend the provided codec parameters with information from the stream information block.
        params
            .with_sample_rate(info.sample_rate)
            .with_bits_per_sample(info.bits_per_sample)
            .with_max_frames_per_packet(u64::from(info.block_len_max))
            .with_channels(info.channels.clone());

        if let Some(md5) = info.md5 {
            params.with_verification_code(VerificationCheck::Md5(md5));
        }

        let spec = AudioSpec::new(info.sample_rate, info.channels.clone());
        let buf = AudioBuffer::new(spec, usize::from(info.block_len_max));

        // TODO: Verify packet integrity if the demuxer is not.
        // if !params.packet_data_integrity {
        //     return unsupported_error("flac: packet integrity is required");
        // }

        Ok(FlacDecoder {
            params,
            is_validating: options.verify,
            validator: Default::default(),
            buf,
        })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let mut reader = packet.as_buf_reader();

        // Synchronize to a frame and get the synchronization code.
        let sync = sync_frame(&mut reader)?;

        let header = read_frame_header(&mut reader, sync)?;

        // Use the bits per sample and sample rate as stated in the frame header, falling back to
        // the stream information if provided. If neither are available, return an error.
        let bits_per_sample = if let Some(bps) = header.bits_per_sample {
            bps
        }
        else if let Some(bps) = self.params.bits_per_sample {
            bps
        }
        else {
            return decode_error("flac: bits per sample not provided");
        };
        if bits_per_sample > u32::BITS {
            return decode_error("flac: invalid bit width");
        }

        // trace!("frame: [{:?}] strategy={:?}, n_samples={}, bps={}, channels={:?}",
        //     header.block_sequence,
        //     header.blocking_strategy,
        //     header.block_num_samples,
        //     bits_per_sample,
        //     &header.channel_assignment);

        // Reserve a writeable chunk in the buffer equal to the number of samples in the block.
        if header.block_num_samples as usize > self.buf.capacity() {
            return decode_error("flac: allocation would overflow buffer");
        }
        self.buf.clear();
        self.buf.render_uninit(Some(header.block_num_samples as usize));

        // Only Bitstream reading for subframes.
        {
            // Sub-frames don't have any byte-aligned content, so use a BitReader.
            let mut bs = BitReaderLtr::new(reader.read_buf_bytes_available_ref());

            // Read each subframe based on the channel assignment into a planar buffer.
            match header.channel_assignment {
                ChannelAssignment::Independant(channels) => {
                    for i in 0..channels as usize {
                        read_subframe(
                            &mut bs,
                            bits_per_sample,
                            self.buf
                                .plane_mut(i)
                                .ok_or(Error::DecodeError("flac: unexpected channel assignment"))?,
                        )?;
                    }
                }
                // For Left/Side, Mid/Side, and Right/Side channel configurations, the Side
                // (Difference) channel requires an extra bit per sample.
                ChannelAssignment::LeftSide => {
                    let (left, side) = self
                        .buf
                        .plane_pair_mut(0, 1)
                        .ok_or(Error::DecodeError("flac: unexpected channel assignment"))?;

                    read_subframe(&mut bs, bits_per_sample, left)?;
                    read_subframe(&mut bs, bits_per_sample + 1, side)?;

                    decorrelate_left_side(left, side);
                }
                ChannelAssignment::MidSide => {
                    let (mid, side) = self
                        .buf
                        .plane_pair_mut(0, 1)
                        .ok_or(Error::DecodeError("flac: unexpected channel assignment"))?;

                    read_subframe(&mut bs, bits_per_sample, mid)?;
                    read_subframe(&mut bs, bits_per_sample + 1, side)?;

                    decorrelate_mid_side(mid, side);
                }
                ChannelAssignment::RightSide => {
                    let (side, right) = self
                        .buf
                        .plane_pair_mut(0, 1)
                        .ok_or(Error::DecodeError("flac: unexpected channel assignment"))?;

                    read_subframe(&mut bs, bits_per_sample + 1, side)?;
                    read_subframe(&mut bs, bits_per_sample, right)?;

                    decorrelate_right_side(right, side);
                }
            }
        }

        // Feed the validator if validation is enabled.
        if self.is_validating {
            self.validator.update(&self.buf, bits_per_sample);
        }

        // The decoder uses a 32bit sample format as a common denominator, but that doesn't mean
        // the encoded audio samples are actually 32bit. Shift all samples in the output buffer
        // so that regardless the encoded bits/sample, the output is always 32bits/sample.
        if bits_per_sample < 32 {
            let shift = 32 - bits_per_sample;
            self.buf.apply(|sample| sample << shift);
        }

        Ok(())
    }
}

impl AudioDecoder for FlacDecoder {
    fn codec_info(&self) -> &CodecInfo {
        // Only one codec is supported.
        &Self::supported_codecs().first().expect("at least one codec registered").info
    }

    fn reset(&mut self) {
        // No state is stored between packets, therefore do nothing.
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
        let mut result: FinalizeResult = Default::default();

        // If verifying...
        if self.is_validating {
            // Try to get the expected MD5 checksum and compare it against the decoded checksum.
            if let Some(VerificationCheck::Md5(expected)) = self.params.verification_check {
                let decoded = self.validator.md5();

                // Only generate the expected and decoded MD5 checksum strings if logging is
                // enabled at the debug level.
                if log_enabled!(log::Level::Debug) {
                    use std::fmt::Write;

                    let mut expected_s = String::with_capacity(32);
                    let mut decoded_s = String::with_capacity(32);

                    expected.iter().for_each(|b| {
                        write!(expected_s, "{b:02x}").expect("write to String never fails")
                    });
                    decoded.iter().for_each(|b| {
                        write!(decoded_s, "{b:02x}").expect("write to String never fails")
                    });

                    debug!("verification: expected md5 = {expected_s}");
                    debug!("verification: decoded md5  = {decoded_s}");
                }

                result.verify_ok = Some(decoded == expected)
            }
            else {
                warn!("verification requested but the expected md5 checksum was not provided");
            }
        }

        result
    }

    fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
        self.buf.as_generic_audio_buffer_ref()
    }
}

impl RegisterableAudioDecoder for FlacDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(FlacDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_FLAC, "flac", "Free Lossless Audio Codec")]
    }
}

// Subframe business

#[derive(Debug)]
enum SubFrameType {
    Constant,
    Verbatim,
    FixedLinear(u32),
    Linear(u32),
}

fn read_subframe<B: ReadBitsLtr>(bs: &mut B, frame_bps: u32, buf: &mut [i32]) -> Result<()> {
    // First sub-frame bit must always 0.
    if bs.read_bool()? {
        return decode_error("flac: subframe padding is not 0");
    }

    // Next 6 bits designate the sub-frame type.
    let subframe_type_enc = bs.read_bits_leq32(6)?;

    let subframe_type = match subframe_type_enc {
        0x00 => SubFrameType::Constant,
        0x01 => SubFrameType::Verbatim,
        0x08..=0x0f => {
            let order = subframe_type_enc & 0x07;
            // The Fixed Predictor only supports orders between 0 and 4.
            if order > 4 {
                return decode_error("flac: fixed predictor orders of greater than 4 are invalid");
            }
            SubFrameType::FixedLinear(order)
        }
        0x20..=0x3f => SubFrameType::Linear((subframe_type_enc & 0x1f) + 1),
        _ => {
            return decode_error("flac: subframe type set to reserved value");
        }
    };

    // Bit 7 of the sub-frame header designates if there are any dropped (wasted in FLAC terms)
    // bits per sample in the audio sub-block. If the bit is set, unary decode the number of
    // dropped bits per sample.
    let dropped_bps = if bs.read_bool()? { bs.read_unary_zeros()? + 1 } else { 0 };
    if dropped_bps > frame_bps {
        return decode_error(
            "flac: dropped bits per sample is greater than the frame bits per sample",
        );
    }

    // The bits per sample stated in the frame header is for the decoded audio sub-block samples.
    // However, it is likely that the lower order bits of all the samples are simply 0. Therefore,
    // the encoder will truncate `dropped_bps` of lower order bits for every sample in a sub-block.
    // The decoder simply needs to shift left all samples by `dropped_bps` after decoding the
    // sub-frame and obtaining the truncated audio sub-block samples.
    let bps = frame_bps - dropped_bps;

    // trace!("\tsubframe: type={:?}, bps={}, dropped_bps={}",
    //     &subframe_type,
    //     bps,
    //     dropped_bps);

    match subframe_type {
        SubFrameType::Constant => decode_constant(bs, bps, buf)?,
        SubFrameType::Verbatim => decode_verbatim(bs, bps, buf)?,
        SubFrameType::FixedLinear(order) => decode_fixed_linear(bs, bps, order, buf)?,
        SubFrameType::Linear(order) => decode_linear(bs, bps, order, buf)?,
    };

    // Shift the samples to account for the dropped bits.
    samples_shl(dropped_bps, buf);

    Ok(())
}

#[inline(always)]
fn samples_shl(shift: u32, buf: &mut [i32]) {
    if shift > 0 {
        for sample in buf.iter_mut() {
            *sample = sample.wrapping_shl(shift);
        }
    }
}

fn decode_constant<B: ReadBitsLtr>(bs: &mut B, bps: u32, buf: &mut [i32]) -> Result<()> {
    let const_sample = sign_extend_leq32_to_i32(bs.read_bits_leq32(bps)?, bps);

    for sample in buf.iter_mut() {
        *sample = const_sample;
    }

    Ok(())
}

fn decode_verbatim<B: ReadBitsLtr>(bs: &mut B, bps: u32, buf: &mut [i32]) -> Result<()> {
    for sample in buf.iter_mut() {
        *sample = sign_extend_leq32_to_i32(bs.read_bits_leq32(bps)?, bps);
    }

    Ok(())
}

fn decode_fixed_linear<B: ReadBitsLtr>(
    bs: &mut B,
    bps: u32,
    order: u32,
    buf: &mut [i32],
) -> Result<()> {
    if order as usize > buf.len() {
        return decode_error("flac: fixed predictor order is greater than the block size");
    }

    // The first `order` samples are encoded verbatim to warm-up the LPC decoder.
    decode_verbatim(bs, bps, &mut buf[..order as usize])?;

    // Decode the residuals for the predicted samples.
    decode_residual(bs, order, buf)?;

    // Run the Fixed predictor (appends to residuals).
    //
    // TODO: The fixed predictor uses 64-bit accumulators by default to support bps > 26. On 64-bit
    // machines, this is preferable, but on 32-bit machines if bps <= 26, run a 32-bit predictor,
    // and fallback to the 64-bit predictor if necessary (which is basically never).
    fixed_predict(order, buf);

    Ok(())
}

fn decode_linear<B: ReadBitsLtr>(bs: &mut B, bps: u32, order: u32, buf: &mut [i32]) -> Result<()> {
    if order as usize > buf.len() {
        return decode_error("flac: predictor order is greater than the block size");
    }

    // The order of the Linear Predictor should be between 1 and 32.
    debug_assert!(order > 0 && order <= 32);

    // The first `order` samples are encoded verbatim to warm-up the LPC decoder.
    decode_verbatim(bs, bps, &mut buf[0..order as usize])?;

    // Quantized linear predictor (QLP) coefficients precision in bits (1-16).
    let qlp_precision = bs.read_bits_leq32(4)? + 1;

    if qlp_precision > 15 {
        return decode_error("flac: qlp precision set to reserved value");
    }

    // QLP coefficients bit shift [-16, 15].
    let qlp_coeff_shift = sign_extend_leq32_to_i32(bs.read_bits_leq32(5)?, 5);

    if qlp_coeff_shift >= 0 {
        let mut qlp_coeffs = [0i32; 32];

        for c in qlp_coeffs.iter_mut().rev().take(order as usize) {
            *c = sign_extend_leq32_to_i32(bs.read_bits_leq32(qlp_precision)?, qlp_precision);
        }

        decode_residual(bs, order, buf)?;

        // Helper function to dispatch to a predictor with a maximum order of N.
        #[inline(always)]
        fn lpc<const N: usize>(
            order: u32,
            coeffs: &[i32; 32],
            coeff_shift: i32,
            bps: u32,
            buf: &mut [i32],
        ) {
            let coeffs_n = (&coeffs[32 - N..32]).try_into().expect("slice has exactly N elements");
            lpc_predict::<N>(order as usize, coeffs_n, coeff_shift as u32, bps, buf);
        }

        // Pick the best length linear predictor to use based on the order. Most FLAC streams use
        // the subset format and have an order <= 12. Therefore, for orders <= 12, dispatch to
        // predictors that roughly match the order. If a predictor is too long for a given order,
        // then there will be wasted computations. On the other hand, it is not worth the code bloat
        // to specialize for every order <= 12.
        match order {
            0..=4 => lpc::<4>(order, &qlp_coeffs, qlp_coeff_shift, bps, buf),
            5..=6 => lpc::<6>(order, &qlp_coeffs, qlp_coeff_shift, bps, buf),
            7..=8 => lpc::<8>(order, &qlp_coeffs, qlp_coeff_shift, bps, buf),
            9..=10 => lpc::<10>(order, &qlp_coeffs, qlp_coeff_shift, bps, buf),
            11..=12 => lpc::<12>(order, &qlp_coeffs, qlp_coeff_shift, bps, buf),
            _ => lpc::<32>(order, &qlp_coeffs, qlp_coeff_shift, bps, buf),
        };
    }
    else {
        return unsupported_error("flac: lpc shifts less than 0 are not supported");
    }

    Ok(())
}

fn decode_residual<B: ReadBitsLtr>(
    bs: &mut B,
    n_prelude_samples: u32,
    buf: &mut [i32],
) -> Result<()> {
    let method_enc = bs.read_bits_leq32(2)?;

    // The FLAC specification defines two residual coding methods: Rice and Rice2. The
    // only difference between the two is the bit width of the Rice parameter. Note the
    // bit width based on the residual encoding method and use the same code path for
    // both cases.
    let param_bit_width = match method_enc {
        0x0 => 4,
        0x1 => 5,
        _ => {
            return decode_error("flac: residual method set to reserved value");
        }
    };

    // Read the partition order.
    let order = bs.read_bits_leq32(4)?;

    // The number of partitions is equal to 2^order.
    let n_partitions = 1usize << order;

    // In general, all partitions have the same number of samples such that the sum of all partition
    // lengths equal the block length. The number of samples in a partition can therefore be
    // calculated with block_size / 2^order *in general*. However, since there are warm-up samples
    // stored verbatim, the first partition has n_prelude_samples less samples. Likewise, if there
    // is only one partition, then it too has n_prelude_samples less samples.
    let n_partition_samples = buf.len() >> order;

    // The size of the first (and/or only) partition as per the specification is n_partition_samples
    // minus the number of warm-up samples (which is the predictor order). Ensure the number of
    // samples in these types of partitions cannot be negative.
    if n_prelude_samples as usize > n_partition_samples {
        return decode_error("flac: residual partition too small for given predictor order");
    }

    // Ensure that the sum of all partition lengths equal the block size.
    if n_partitions * n_partition_samples != buf.len() {
        return decode_error("flac: block size is not same as encoded residual");
    }

    // trace!("\t\tresidual: n_partitions={}, n_partition_samples={}, n_prelude_samples={}",
    //     n_partitions,
    //     n_partition_samples,
    //     n_prelude_samples);

    // Decode the first partition as it may have less than n_partition_samples samples.
    decode_rice_partition(
        bs,
        param_bit_width,
        &mut buf[n_prelude_samples as usize..n_partition_samples],
    )?;

    // Decode the remaining partitions.
    for buf_chunk in buf[n_partition_samples..].chunks_mut(n_partition_samples) {
        decode_rice_partition(bs, param_bit_width, buf_chunk)?;
    }

    Ok(())
}

fn decode_rice_partition<B: ReadBitsLtr>(
    bs: &mut B,
    param_bit_width: u32,
    buf: &mut [i32],
) -> Result<()> {
    // Read the encoding parameter, generally the Rice parameter.
    let rice_param = bs.read_bits_leq32(param_bit_width)?;

    // If the Rice parameter is all 1s (e.g., 0xf for a 4bit parameter, 0x1f for a 5bit parameter),
    // then it indicates that residuals in this partition are not Rice encoded, rather they are
    // binary encoded. Conversely, if the parameter is less than this value, the residuals are Rice
    // encoded.
    if rice_param < (1 << param_bit_width) - 1 {
        // println!("\t\t\tPartition (Rice): n_residuals={}, rice_param={}", buf.len(), rice_param);

        // Read each rice encoded residual and store in buffer.
        for sample in buf.iter_mut() {
            let q = bs.read_unary_zeros()?;
            let r = bs.read_bits_leq32(rice_param)?;
            *sample = rice_signed_to_i32((q << rice_param) | r);
        }
    }
    else {
        let residual_bits = bs.read_bits_leq32(5)?;

        // trace!(
        //     "\t\t\tpartition (Binary): n_residuals={}, residual_bits={}",
        //     buf.len(),
        //     residual_bits
        // );

        // Read each binary encoded residual and store in buffer.
        for sample in buf.iter_mut() {
            *sample = sign_extend_leq32_to_i32(bs.read_bits_leq32(residual_bits)?, residual_bits);
        }
    }

    Ok(())
}

#[inline(always)]
fn rice_signed_to_i32(word: u32) -> i32 {
    // Input  => 0  1  2  3  4  5  6  7  8  9  10
    // Output => 0 -1  1 -2  2 -3  3 -4  4 -5   5
    //
    //  - If even: output = input / 2
    //  - If odd:  output = -(input + 1) / 2
    //                    =  (input / 2) - 1

    // Divide the input by 2 and convert to signed.
    let div2 = (word >> 1) as i32;

    // Using the LSB of the input, create a new signed integer that's either
    // -1 (0b1111_11110) or 0 (0b0000_0000). For odd inputs, this will be -1, for even
    // inputs it'll be 0.
    let sign = -((word & 0x1) as i32);

    // XOR the div2 result with the sign. If sign is 0, the XOR produces div2. If sign is -1, then
    // -div2 - 1 is returned.
    //
    // Example:  input = 9 => div2 = 0b0000_0100, sign = 0b1111_11110
    //
    //           div2 ^ sign =   0b0000_0100
    //                         ^ 0b1111_1110
    //                           -----------
    //                           0b1111_1011  (-5)
    div2 ^ sign
}

#[test]
fn verify_rice_signed_to_i32() {
    assert_eq!(rice_signed_to_i32(0), 0);
    assert_eq!(rice_signed_to_i32(1), -1);
    assert_eq!(rice_signed_to_i32(2), 1);
    assert_eq!(rice_signed_to_i32(3), -2);
    assert_eq!(rice_signed_to_i32(4), 2);
    assert_eq!(rice_signed_to_i32(5), -3);
    assert_eq!(rice_signed_to_i32(6), 3);
    assert_eq!(rice_signed_to_i32(7), -4);
    assert_eq!(rice_signed_to_i32(8), 4);
    assert_eq!(rice_signed_to_i32(9), -5);
    assert_eq!(rice_signed_to_i32(10), 5);

    assert_eq!(rice_signed_to_i32(u32::max_value()), -2_147_483_648);
}

fn fixed_predict(order: u32, buf: &mut [i32]) {
    debug_assert!(order <= 4);

    // The Fixed Predictor is just a hard-coded version of the Linear Predictor up to order 4 and
    // with fixed coefficients. Some cases may be simplified such as orders 0 and 1. For orders 2
    // through 4, use the same IIR-style algorithm as the Linear Predictor.
    //
    // The prediction is only ever truncated to 32 bits and added to the residual, and wrapping
    // 32-bit arithmetic is a ring homomorphism of wrapping 64-bit arithmetic, so it is not
    // necessary to widen to 64 bits.
    match order {
        // A 0th order predictor always predicts 0, and therefore adds nothing to any of the samples
        // in buf. Do nothing.
        0 => (),
        // A 1st order predictor always returns the previous sample since the polynomial is:
        // s(i) = 1*s(i),
        1 => {
            for i in 1..buf.len() {
                buf[i] = buf[i].wrapping_add(buf[i - 1]);
            }
        }
        // A 2nd order predictor uses the polynomial: s(i) = 2*s(i-1) - 1*s(i-2).
        2 => {
            for i in 2..buf.len() {
                let p = buf[i - 1].wrapping_mul(2).wrapping_sub(buf[i - 2]);
                buf[i] = buf[i].wrapping_add(p);
            }
        }
        // A 3rd order predictor uses the polynomial: s(i) = 3*s(i-1) - 3*s(i-2) + 1*s(i-3).
        3 => {
            for i in 3..buf.len() {
                let p = buf[i - 1]
                    .wrapping_mul(3)
                    .wrapping_sub(buf[i - 2].wrapping_mul(3))
                    .wrapping_add(buf[i - 3]);
                buf[i] = buf[i].wrapping_add(p);
            }
        }
        // A 4th order predictor uses the polynomial:
        // s(i) = 4*s(i-1) - 6*s(i-2) + 4*s(i-3) - 1*s(i-4).
        4 => {
            for i in 4..buf.len() {
                let p = buf[i - 1]
                    .wrapping_mul(4)
                    .wrapping_sub(buf[i - 2].wrapping_mul(6))
                    .wrapping_add(buf[i - 3].wrapping_mul(4))
                    .wrapping_sub(buf[i - 4]);
                buf[i] = buf[i].wrapping_add(p);
            }
        }
        _ => unreachable!(),
    };
}

/// Generalized Linear Predictive Coding (LPC) decoder. The exact number of coefficients given is
/// specified by `order`. Coefficients must be stored in reverse order in `coeffs` with the first
/// coefficient at index 31. Coefficients at indices less than 31 - `order` must be 0.
/// It is expected that the first `order` samples in `buf` are warm-up samples, and that they (and
/// any valid reconstructed sample) fit in `bps` bits.
///
/// Where it can be proven that no intermediate value overflows, a 32-bit predictor is used,
/// otherwise a 64-bit predictor is used. Both produce identical results for all inputs.
#[inline(always)]
fn lpc_predict<const N: usize>(
    order: usize,
    coeffs: &[i32; N],
    coeff_shift: u32,
    bps: u32,
    buf: &mut [i32],
) {
    // The 32-bit predictor needs a native packed 32-bit multiply to be faster than the 64-bit
    // predictor. Baseline x86-64 (SSE2) lacks one, so emulating it is a net loss there.
    const NATIVE_MUL_I32X4: bool = cfg!(any(
        target_feature = "sse4.1",
        target_arch = "aarch64",
        all(target_arch = "arm", target_feature = "neon")
    ));

    if NATIVE_MUL_I32X4 {
        lpc_predict_narrow::<N>(order, coeffs, coeff_shift, bps, buf);
    }
    else {
        lpc_predict_wide::<N>(order, coeffs, coeff_shift, buf, order);
    }
}

/// Linear Predictive Coding (LPC) decoder using 32-bit arithmetic when it is provably exact, see
/// [`lpc_predict`].
fn lpc_predict_narrow<const N: usize>(
    order: usize,
    coeffs: &[i32; N],
    coeff_shift: u32,
    bps: u32,
    buf: &mut [i32],
) {
    // Order must be less than or equal to the number of coefficients.
    debug_assert!(order <= coeffs.len());

    // Order must be less than to equal to the number of samples the buffer can hold.
    debug_assert!(order <= buf.len());

    // Samples of a valid stream are within [-m, m). With the sum of the magnitudes of all
    // coefficients known, the magnitude of a prediction is bounded by `abs_sum * m`. If that fits
    // in an i32, then the prediction can be computed exactly with 32-bit (wrapping) arithmetic
    // while the inputs are in range.
    let m = 1i64 << (bps.clamp(1, 32) - 1);
    let abs_sum: i64 = coeffs.iter().map(|&c| i64::from(c.unsigned_abs())).sum();

    if abs_sum * m > i64::from(i32::MAX) {
        lpc_predict_wide::<N>(order, coeffs, coeff_shift, buf, order);
        return;
    }

    // The coefficients in order of increasing lag (distance to the predicted sample).
    let mut lag_coeffs = *coeffs;
    lag_coeffs.reverse();

    // The filter is evaluated in transposed form: `partial[j]` is the part of the prediction of
    // sample `i + j` that is already known from samples before `i`. Each new sample contributes
    // to all the following N predictions at once, and the state stays in registers rather than
    // being reloaded from the sample buffer (which was just written to).
    let mut partial = [0i32; N];

    for i in 0..buf.len() {
        let sample = if i < order {
            buf[i]
        }
        else {
            let sample = buf[i].wrapping_add(partial[0] >> coeff_shift);
            buf[i] = sample;

            // If this sample is out of range, then the bound no longer holds for the following
            // samples. Finish the block with the wide predictor.
            if i64::from(sample) < -m || i64::from(sample) >= m {
                lpc_predict_wide::<N>(order, coeffs, coeff_shift, buf, i + 1);
                return;
            }

            sample
        };

        let mut next = [0i32; N];
        for j in 0..N - 1 {
            next[j] = partial[j + 1].wrapping_add(lag_coeffs[j].wrapping_mul(sample));
        }
        next[N - 1] = lag_coeffs[N - 1].wrapping_mul(sample);
        partial = next;
    }
}

/// 64-bit Linear Predictive Coding (LPC) decoder, predicting samples `from..` where `from` is at
/// least `order`. See [`lpc_predict`].
fn lpc_predict_wide<const N: usize>(
    order: usize,
    coeffs: &[i32; N],
    coeff_shift: u32,
    buf: &mut [i32],
    from: usize,
) {
    // The main, efficient, predictor loop needs N previous samples to run. Since order <= N,
    // calculate enough samples to reach N.
    for i in from..cmp::min(N, buf.len()) {
        let predicted = coeffs[N - order..N]
            .iter()
            .zip(&buf[i - order..i])
            .map(|(&c, &sample)| c as i64 * sample as i64)
            .sum::<i64>();

        buf[i] = buf[i].wrapping_add((predicted >> coeff_shift) as i32);
    }

    // Main predictor loop. Calculate each sample by applying what is essentially an IIR filter.
    for i in cmp::max(from, N)..buf.len() {
        let predicted = coeffs
            .iter()
            .zip(&buf[i - N..i])
            .map(|(&c, &s)| i64::from(c) * i64::from(s))
            .sum::<i64>();

        buf[i] = buf[i].wrapping_add((predicted >> coeff_shift) as i32);
    }
}

#[cfg(test)]
mod tests {
    use super::{fixed_predict, lpc_predict, lpc_predict_narrow};

    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            (self.next() >> 11) % n
        }
    }

    /// The straightforward 64-bit predictor all optimized predictors must match exactly.
    fn lpc_reference(order: usize, coeffs: &[i32], shift: u32, buf: &mut [i32]) {
        let n = coeffs.len();
        for i in order..buf.len() {
            let taps = order.min(i);
            let predicted: i64 =
                (0..taps).map(|k| i64::from(coeffs[n - 1 - k]) * i64::from(buf[i - 1 - k])).sum();
            buf[i] = buf[i].wrapping_add((predicted >> shift) as i32);
        }
    }

    fn check_lpc<const N: usize>(rng: &mut XorShift) {
        for _ in 0..3000 {
            let order = 1 + rng.below(N as u64) as usize;
            let bps = [4, 8, 12, 16, 20, 24, 32][rng.below(7) as usize];
            let precision = 1 + rng.below(15) as u32;
            let shift = rng.below(16) as u32;
            let len = order + rng.below(100) as usize;

            // Occasionally use tiny coefficients so the 32-bit predictor is eligible at high bps.
            let coeff_bits = if rng.below(2) == 0 { precision } else { precision.min(5) };
            let mut coeffs = [0i32; N];
            for c in coeffs.iter_mut().rev().take(order) {
                let v = rng.next() as u32 >> (32 - coeff_bits);
                *c = ((v << (32 - coeff_bits)) as i32) >> (32 - coeff_bits);
            }

            // Residuals are either small, bps wide (valid-ish), or arbitrary (corrupt stream).
            let res_bits = [1, bps.min(32), 32][rng.below(3) as usize];
            let mut buf: Vec<i32> = (0..len)
                .map(|i| {
                    let bits = if i < order { bps } else { res_bits };
                    ((rng.next() as u32 >> (32 - bits)) << (32 - bits)) as i32 >> (32 - bits)
                })
                .collect();

            let mut expected = buf.clone();
            lpc_reference(order, &coeffs, shift, &mut expected);
            let mut narrow = buf.clone();
            lpc_predict::<N>(order, &coeffs, shift, bps, &mut buf);
            lpc_predict_narrow::<N>(order, &coeffs, shift, bps, &mut narrow);
            assert_eq!(buf, expected, "N={N} order={order} bps={bps} shift={shift}");
            assert_eq!(narrow, expected, "narrow N={N} order={order} bps={bps} shift={shift}");
        }
    }

    #[test]
    fn verify_lpc_predict_matches_reference() {
        let mut rng = XorShift(0x2545_f491_4f6c_dd1d);
        check_lpc::<4>(&mut rng);
        check_lpc::<6>(&mut rng);
        check_lpc::<8>(&mut rng);
        check_lpc::<12>(&mut rng);
        check_lpc::<32>(&mut rng);
    }

    #[test]
    fn verify_fixed_predict_matches_reference() {
        let mut rng = XorShift(0x9e37_79b9_7f4a_7c15);
        const COEFFS: [&[i64]; 5] = [&[], &[1], &[-1, 2], &[1, -3, 3], &[-1, 4, -6, 4]];

        for _ in 0..2000 {
            let order = rng.below(5) as usize;
            let len = order + rng.below(64) as usize;
            let mut buf: Vec<i32> = (0..len).map(|_| rng.next() as i32).collect();

            let mut expected = buf.clone();
            for i in order..len {
                let p: i64 = COEFFS[order]
                    .iter()
                    .enumerate()
                    .map(|(k, &c)| c * i64::from(expected[i - order + k]))
                    .sum();
                expected[i] = expected[i].wrapping_add(p as i32);
            }

            fixed_predict(order as u32, &mut buf);
            assert_eq!(buf, expected, "order={order}");
        }
    }
}
