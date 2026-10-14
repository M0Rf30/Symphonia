// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The MLP / Dolby TrueHD access unit decoder.
//!
//! Both codecs share one bitstream design. An *access unit* contains a major sync header
//! (periodically), a directory of *substreams*, and the substreams themselves. Each substream
//! carries blocks of entropy coded residuals which are, per channel, run through a cascade of an
//! FIR and an IIR predictor, and are then rematrixed (with optional pseudo-random noise) to
//! recover the output channels.

use log::{debug, warn};

use symphonia_core::audio::{AudioMut, Position};
use symphonia_core::errors::{Result, decode_error, unsupported_error};

use crate::bits::BitReader;
use crate::crc;
use crate::huffman;
use crate::layout::ChannelList;
use crate::tables::NOISE_TABLE;

/// The last possible matrix channel for MLP.
const MAX_MATRIX_CHANNEL_MLP: usize = 5;
/// The last possible matrix channel for TrueHD.
const MAX_MATRIX_CHANNEL_TRUEHD: usize = 7;
/// The maximum number of channels in a stream (MLP: 5.1 + 2 noise channels, TrueHD: 7.1).
const MAX_CHANNELS: usize = 8;
/// The maximum number of primitive matrices for MLP.
const MAX_MATRICES_MLP: usize = 6;
/// The maximum number of primitive matrices for TrueHD.
const MAX_MATRICES_TRUEHD: usize = 8;
/// The maximum number of matrices in any stream.
const MAX_MATRICES: usize = 8;
/// The maximum number of substreams.
const MAX_SUBSTREAMS: usize = 4;
/// The maximum sample rate as a multiple of 48 kHz.
const MAX_RATEFACTOR: usize = 4;
/// The maximum sample rate.
const MAX_SAMPLERATE: u32 = MAX_RATEFACTOR as u32 * 48000;
/// The maximum number of samples in an access unit.
pub(crate) const MAX_BLOCKSIZE: usize = 40 * MAX_RATEFACTOR;
/// The next power of two above `MAX_BLOCKSIZE`.
const MAX_BLOCKSIZE_POW2: usize = 64 * MAX_RATEFACTOR;
/// The maximum number of taps in the FIR filter.
const MAX_FIR_ORDER: usize = 8;
/// The maximum number of taps in the IIR filter.
const MAX_IIR_ORDER: usize = 4;

const FIR: usize = 0;
const IIR: usize = 1;

/// Value of the lossless check data that indicates it is invalid.
const LOSSLESS_CHECK_INVALID: u32 = 0xffff_ffff;

// Bits of the parameter presence flags.
const PARAM_BLOCKSIZE: u8 = 1 << 7;
const PARAM_MATRIX: u8 = 1 << 6;
const PARAM_OUTSHIFT: u8 = 1 << 5;
const PARAM_QUANTSTEP: u8 = 1 << 4;
const PARAM_FIR: u8 = 1 << 3;
const PARAM_IIR: u8 = 1 << 2;
const PARAM_HUFFOFFSET: u8 = 1 << 1;
const PARAM_PRESENCE: u8 = 1 << 0;

/// Stream type signalled in the major sync for MLP.
const SYNC_MLP: u8 = 0xbb;
/// Stream type signalled in the major sync for TrueHD.
const SYNC_TRUEHD: u8 = 0xba;

/// Number of channels in each of the 13 TrueHD channel groups.
const THD_CHANCOUNT: [u8; 13] = [2, 1, 1, 2, 2, 2, 2, 1, 1, 2, 2, 1, 1];

/// Bit depth for each quantization code in the MLP major sync.
fn mlp_quant(code: u32) -> u8 {
    match code {
        0 => 16,
        1 => 20,
        2 => 24,
        _ => 0,
    }
}

/// The sample rate for the sample rate code in the major sync.
fn mlp_samplerate(code: u32) -> u32 {
    if code == 0xf { 0 } else { (if code & 8 != 0 { 44100 } else { 48000 }) << (code & 7) }
}

fn truehd_channels(chanmap: u32) -> u32 {
    THD_CHANCOUNT.iter().enumerate().map(|(i, &c)| u32::from(c) * ((chanmap >> i) & 1)).sum()
}

#[inline(always)]
fn msb_mask(bits: u8) -> i32 {
    (-1i32).wrapping_shl(u32::from(bits))
}

/// Parameters of one of the prediction filters.
#[derive(Copy, Clone, Default)]
struct FilterParams {
    /// Number of taps.
    order: u8,
    /// Right shift to apply to the output of the filter.
    shift: u8,
    /// Filter state (the most recent value first).
    state: [i32; MAX_FIR_ORDER],
}

/// Sample data coding information for one channel.
#[derive(Copy, Clone, Default)]
struct ChannelParams {
    filter_params: [FilterParams; 2],
    coeff: [[i32; MAX_FIR_ORDER]; 2],
    /// Offset to apply to residual values.
    huff_offset: i16,
    /// Sign/rounding-corrected version of `huff_offset`.
    sign_huff_offset: i32,
    /// Which codebook to use to read residuals.
    codebook: u8,
    /// Size of the residual suffix not encoded using VLC.
    huff_lsbs: u8,
}

#[derive(Copy, Clone)]
struct SubStream {
    /// Set if a valid restart header has been read. Otherwise the substream cannot be decoded.
    restart_seen: bool,
    /// Set if the end of the stream was signalled.
    end_of_stream: bool,

    // Restart header data.
    /// The noise type to be used in the rematrix stage: false for 0x31ea, true for 0x31eb.
    noise_type: bool,
    /// The index of the first channel coded in this substream.
    min_channel: usize,
    /// The index of the last channel coded in this substream.
    max_channel: usize,
    /// Bitmask of the channels coded in this substream.
    coded_channels: u64,
    /// The number of channels input into the rematrix stage, minus 1.
    max_matrix_channel: usize,
    /// For each output channel, the matrix channel assigned to it.
    ch_assign: [u8; MAX_CHANNELS],
    /// The channel layout of this substream.
    layout: ChannelList,
    /// Channel coding parameters.
    channel_params: [ChannelParams; MAX_CHANNELS],
    /// The left shift applied to random noise in 0x31ea substreams.
    noise_shift: u8,
    /// The current seed of the pseudorandom noise generator(s).
    noisegen_seed: u32,
    /// Set if the substream contains extra info to check the size of VLC blocks.
    data_check_present: bool,
    /// Bitmask of which parameter sets are conveyed in a decoding parameter block.
    param_presence_flags: u8,

    // Matrix data.
    num_primitive_matrices: usize,
    matrix_out_ch: [u8; MAX_MATRICES],
    /// Whether the LSBs of the matrix output are encoded in the bitstream.
    lsb_bypass: [bool; MAX_MATRICES],
    /// Matrix coefficients, stored as 2.14 fixed point.
    matrix_coeff: [[i32; MAX_CHANNELS]; MAX_MATRICES],
    /// Left shift to apply to noise values in 0x31eb substreams.
    matrix_noise_shift: [u8; MAX_MATRICES],

    /// Left shift to apply to Huffman-decoded residuals.
    quant_step_size: [u8; MAX_CHANNELS],
    /// Number of PCM samples in the current audio block.
    blocksize: usize,
    /// Number of PCM samples decoded so far in this access unit.
    blockpos: usize,
    /// Left shift to apply to decoded PCM values to get the final 24-bit output.
    output_shift: [i8; MAX_CHANNELS],
    /// Running XOR of all output samples.
    lossless_check_data: u32,
}

impl Default for SubStream {
    fn default() -> Self {
        SubStream {
            restart_seen: false,
            end_of_stream: false,
            noise_type: false,
            min_channel: 0,
            max_channel: 0,
            coded_channels: 0,
            max_matrix_channel: 0,
            ch_assign: [0; MAX_CHANNELS],
            layout: ChannelList::EMPTY,
            channel_params: [ChannelParams::default(); MAX_CHANNELS],
            noise_shift: 0,
            noisegen_seed: 0,
            data_check_present: false,
            param_presence_flags: 0,
            num_primitive_matrices: 0,
            matrix_out_ch: [0; MAX_MATRICES],
            lsb_bypass: [false; MAX_MATRICES],
            matrix_coeff: [[0; MAX_CHANNELS]; MAX_MATRICES],
            matrix_noise_shift: [0; MAX_MATRICES],
            quant_step_size: [0; MAX_CHANNELS],
            blocksize: 0,
            blockpos: 0,
            output_shift: [0; MAX_CHANNELS],
            lossless_check_data: LOSSLESS_CHECK_INVALID,
        }
    }
}

/// Information read from a major sync header.
struct MajorSync {
    stream_type: u8,
    header_size: usize,
    group1_bits: u8,
    group2_bits: u8,
    group1_samplerate: u32,
    group2_samplerate: u32,
    channel_arrangement: u32,
    channel_layout_mlp: Option<ChannelList>,
    channels_thd_stream1: u32,
    channels_thd_stream2: u32,
    layout_thd_stream1: Option<ChannelList>,
    layout_thd_stream2: Option<ChannelList>,
    access_unit_size: usize,
    access_unit_size_pow2: usize,
    num_substreams: usize,
    substream_info: u8,
}

/// The size of the major sync header in bytes. `buf` begins at the sync word.
fn major_sync_size(buf: &[u8]) -> Option<usize> {
    if buf.len() < 28 {
        return None;
    }

    let mut size = 28;

    if buf[..4] == [0xf8, 0x72, 0x6f, 0xba] {
        let has_extension = buf[25] & 1 != 0;
        if has_extension {
            let extensions = usize::from(buf[26] >> 4);
            size += 2 + extensions * 2;
        }
    }

    Some(size)
}

/// Read a major sync header. `buf` begins at the sync word.
fn read_major_sync(buf: &[u8]) -> Result<MajorSync> {
    let header_size = match major_sync_size(buf) {
        Some(size) if buf.len() >= size => size,
        _ => return decode_error("truehd: packet too short to read major sync"),
    };

    let checksum = crc::checksum16(&buf[..header_size - 2]);

    if checksum != u16::from_le_bytes([buf[header_size - 2], buf[header_size - 1]]) {
        return decode_error("truehd: major sync checksum error");
    }

    let mut br = BitReader::new(&buf[..header_size]);

    if br.read(24)? != 0x00f8_726f {
        return decode_error("truehd: invalid major sync word");
    }

    let stream_type = br.read(8)? as u8;

    let mut mh = MajorSync {
        stream_type,
        header_size,
        group1_bits: 0,
        group2_bits: 0,
        group1_samplerate: 0,
        group2_samplerate: 0,
        channel_arrangement: 0,
        channel_layout_mlp: None,
        channels_thd_stream1: 0,
        channels_thd_stream2: 0,
        layout_thd_stream1: None,
        layout_thd_stream2: None,
        access_unit_size: 0,
        access_unit_size_pow2: 0,
        num_substreams: 0,
        substream_info: 0,
    };

    let ratebits;

    match stream_type {
        SYNC_MLP => {
            mh.group1_bits = mlp_quant(br.read(4)?);
            mh.group2_bits = mlp_quant(br.read(4)?);

            ratebits = br.read(4)?;
            mh.group1_samplerate = mlp_samplerate(ratebits);
            mh.group2_samplerate = mlp_samplerate(br.read(4)?);

            br.skip(11)?;

            mh.channel_arrangement = br.read(5)?;
            mh.channel_layout_mlp = ChannelList::from_mlp_arrangement(mh.channel_arrangement);
        }
        SYNC_TRUEHD => {
            // TODO(upstream): It is not known if the bit depth is conveyed anywhere.
            mh.group1_bits = 24;
            mh.group2_bits = 0;

            ratebits = br.read(4)?;
            mh.group1_samplerate = mlp_samplerate(ratebits);
            mh.group2_samplerate = 0;

            br.skip(4)?;

            // Channel modifiers for substream 0 and 1 (2 bits each) only affect matrix encoding
            // metadata.
            br.skip(2)?;
            br.skip(2)?;

            let arrangement1 = br.read(5)?;
            mh.channel_arrangement = arrangement1;
            mh.channels_thd_stream1 = truehd_channels(arrangement1);
            mh.layout_thd_stream1 = ChannelList::from_truehd_map(arrangement1);

            // Channel modifier for substream 2.
            br.skip(2)?;

            let arrangement2 = br.read(13)?;
            mh.channels_thd_stream2 = truehd_channels(arrangement2);
            mh.layout_thd_stream2 = ChannelList::from_truehd_map(arrangement2);
        }
        _ => return decode_error("truehd: invalid major sync stream type"),
    }

    mh.access_unit_size = 40usize << (ratebits & 7);
    mh.access_unit_size_pow2 = 64usize << (ratebits & 7);

    // Skip 48 bits, is_vbr (1 bit) and peak_bitrate (15 bits).
    br.skip(48 + 1 + 15)?;

    mh.num_substreams = br.read(4)? as usize;

    br.skip(2)?;
    // extended_substream_info (2 bits).
    br.skip(2)?;
    mh.substream_info = br.read(8)? as u8;

    Ok(mh)
}

/// The substream directory of an access unit.
#[derive(Default)]
struct Directory {
    /// Size of the directory in bytes.
    header_size: usize,
    parity_present: [bool; MAX_SUBSTREAMS],
    data_len: [usize; MAX_SUBSTREAMS],
}

/// The MLP / TrueHD stream decoder.
pub(crate) struct MlpDecoder {
    /// Set if the stream is MLP rather than TrueHD.
    is_mlp: bool,
    /// Set if a valid major sync block has been read. Otherwise no decoding is possible.
    params_valid: bool,
    /// Current access unit has a major sync.
    is_major_sync_unit: bool,
    /// Number of substreams contained within the stream.
    num_substreams: usize,
    /// Which substreams carry the 2/6/8-channel presentations.
    substream_info: u8,
    /// Index of the last substream to decode. Further substreams are skipped.
    max_decoded_substream: usize,
    /// The stream needs channel reordering to match the canonical channel order.
    needs_reordering: bool,
    /// Number of PCM samples contained in each access unit.
    access_unit_size: usize,
    /// Next power of two above the number of samples in each access unit.
    access_unit_size_pow2: usize,
    /// Sample rate.
    sample_rate: u32,
    /// Bit depth of the first channel group.
    bits_per_sample: u8,
    /// The output channel positions and count, from the last restart header of the last decoded
    /// substream.
    out_channels: Position,
    out_num_channels: usize,

    substreams: [SubStream; MAX_SUBSTREAMS],

    matrix_changed: u32,
    filter_changed: [[u32; 2]; MAX_CHANNELS],

    noise_buffer: [i8; MAX_BLOCKSIZE_POW2],
    bypassed_lsbs: [[u8; MAX_CHANNELS]; MAX_BLOCKSIZE],
    sample_buffer: [[i32; MAX_CHANNELS]; MAX_BLOCKSIZE],
}

impl MlpDecoder {
    pub(crate) fn new() -> Box<Self> {
        Box::new(MlpDecoder {
            is_mlp: false,
            params_valid: false,
            is_major_sync_unit: false,
            num_substreams: 0,
            substream_info: 0,
            max_decoded_substream: 0,
            needs_reordering: false,
            access_unit_size: 0,
            access_unit_size_pow2: 0,
            sample_rate: 0,
            bits_per_sample: 0,
            out_channels: Position::empty(),
            out_num_channels: 0,
            substreams: [SubStream::default(); MAX_SUBSTREAMS],
            matrix_changed: 0,
            filter_changed: [[0; 2]; MAX_CHANNELS],
            noise_buffer: [0; MAX_BLOCKSIZE_POW2],
            bypassed_lsbs: [[0; MAX_CHANNELS]; MAX_BLOCKSIZE],
            sample_buffer: [[0; MAX_CHANNELS]; MAX_BLOCKSIZE],
        })
    }

    /// Discard all decoder state. Decoding resumes at the next major sync.
    pub(crate) fn reset(&mut self) {
        let fresh = MlpDecoder::new();
        *self = *fresh;
    }

    /// The sample rate, valid after a major sync has been decoded.
    pub(crate) fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The bit depth of the stream, valid after a major sync has been decoded.
    pub(crate) fn bits_per_sample(&self) -> u8 {
        self.bits_per_sample
    }

    /// The channel positions of the output, valid after a restart header has been decoded.
    pub(crate) fn out_channels(&self) -> Position {
        self.out_channels
    }

    /// The maximum number of samples per access unit.
    pub(crate) fn access_unit_size(&self) -> usize {
        self.access_unit_size
    }

    /// Read a major sync info header. Contains high level information about the stream (sample
    /// rate, channel arrangement, etc.).
    fn handle_major_sync(&mut self, buf: &[u8]) -> Result<usize> {
        let mh = read_major_sync(buf)?;

        if mh.group1_bits == 0 {
            return decode_error("truehd: invalid/unknown bits per sample");
        }
        if mh.group2_bits > mh.group1_bits {
            return decode_error("truehd: channel group 2 has more bits per sample than group 1");
        }
        if mh.group2_samplerate != 0 && mh.group2_samplerate != mh.group1_samplerate {
            return unsupported_error("truehd: channel groups with differing sample rates");
        }
        if mh.group1_samplerate == 0 {
            return decode_error("truehd: invalid/unknown sampling rate");
        }
        if mh.group1_samplerate > MAX_SAMPLERATE {
            return unsupported_error("truehd: sample rate exceeds the supported maximum");
        }
        if mh.access_unit_size > MAX_BLOCKSIZE || mh.access_unit_size_pow2 > MAX_BLOCKSIZE_POW2 {
            return unsupported_error("truehd: block size exceeds the supported maximum");
        }
        if mh.num_substreams == 0 {
            return decode_error("truehd: no substreams");
        }

        let is_mlp = mh.stream_type == SYNC_MLP;

        if is_mlp && mh.num_substreams > 2 {
            return decode_error("truehd: MLP only supports up to 2 substreams");
        }
        if mh.num_substreams > MAX_SUBSTREAMS {
            return unsupported_error("truehd: too many substreams");
        }

        self.is_mlp = is_mlp;
        self.access_unit_size = mh.access_unit_size;
        self.access_unit_size_pow2 = mh.access_unit_size_pow2;
        self.num_substreams = mh.num_substreams;
        self.substream_info = mh.substream_info;

        // Limit to decoding 3 substreams, as the 4th is used by Dolby Atmos for non-audio data.
        self.max_decoded_substream = (self.num_substreams - 1).min(2);

        self.sample_rate = mh.group1_samplerate;
        self.bits_per_sample = mh.group1_bits;

        self.params_valid = true;
        for s in &mut self.substreams {
            s.restart_seen = false;
        }

        // Set the layout for each substream. When there's more than one, the first substream is
        // stereo. Subsequent substreams' layouts are indicated in the major sync.
        if is_mlp {
            let layout = match mh.channel_layout_mlp {
                Some(l) => l,
                None => return unsupported_error("truehd: unsupported MLP channel arrangement"),
            };

            let substr = usize::from(mh.num_substreams > 1);
            if substr == 1 {
                self.substreams[0].layout = ChannelList::stereo();
            }
            self.substreams[substr].layout = layout;
        }
        else {
            let none_if_empty = |l: Option<ChannelList>| l.filter(|l| l.len() > 0);

            let layout1 = none_if_empty(mh.layout_thd_stream1);
            let layout2 = none_if_empty(mh.layout_thd_stream2);

            // A channel assignment that cannot be represented is only an error if it is used
            // for decoding.
            let unsupported_layout = (mh.layout_thd_stream1.is_none()
                && mh.channels_thd_stream1 > 0)
                || (mh.layout_thd_stream2.is_none() && mh.channels_thd_stream2 > 0);

            self.substreams[1].layout = layout1.unwrap_or(ChannelList::EMPTY);

            if mh.channels_thd_stream1 == 2 && mh.channels_thd_stream2 == 2 {
                self.substreams[0].layout = ChannelList::stereo();
            }
            if mh.num_substreams > 1 {
                self.substreams[0].layout = ChannelList::stereo();
            }
            if mh.num_substreams == 1
                && mh.channels_thd_stream1 == 1
                && mh.channels_thd_stream2 == 1
            {
                self.substreams[0].layout = ChannelList::mono();
            }
            if mh.num_substreams > 2 {
                self.substreams[2].layout = layout2.or(layout1).unwrap_or(ChannelList::EMPTY);
            }
            if mh.num_substreams == 2 {
                self.substreams[1].layout = layout2.or(layout1).unwrap_or(ChannelList::EMPTY);
            }

            // A single substream carries the full presentation. FFmpeg leaves the layout of
            // this substream unset, which makes such streams undecodable. Use the largest
            // presentation.
            if mh.num_substreams == 1 && self.substreams[0].layout.len() == 0 {
                self.substreams[0].layout = layout2.or(layout1).unwrap_or(ChannelList::EMPTY);
            }

            if unsupported_layout && self.substreams[self.max_decoded_substream].layout.len() == 0 {
                return unsupported_error("truehd: unsupported channel assignment");
            }
        }

        self.needs_reordering = mh.channel_arrangement >= 18 && mh.channel_arrangement <= 20;

        Ok(mh.header_size)
    }

    /// Read a restart header from a block in a substream. This contains parameters required to
    /// decode the audio that do not change very often. Generally (always) present only in blocks
    /// following a major sync.
    ///
    /// `data` is the substream data and `br` is positioned just after the 2 bits that signal the
    /// presence of the restart header.
    fn read_restart_header(
        &mut self,
        br: &mut BitReader<'_>,
        data: &[u8],
        substr: usize,
    ) -> Result<()> {
        let start_count = br.bit_pos();

        let std_max_matrix_channel =
            if self.is_mlp { MAX_MATRIX_CHANNEL_MLP } else { MAX_MATRIX_CHANNEL_TRUEHD };

        let sync_word = br.read(13)?;

        if sync_word != 0x31ea >> 1 {
            return decode_error("truehd: restart header sync incorrect");
        }

        let noise_type = br.read_bit()? != 0;

        if self.is_mlp && noise_type {
            return decode_error("truehd: MLP must have 0x31ea sync word");
        }

        // Output timestamp.
        br.skip(16)?;

        let min_channel = br.read(4)? as usize;
        let max_channel = br.read(4)? as usize;
        let max_matrix_channel = br.read(4)? as usize;

        if max_matrix_channel > std_max_matrix_channel {
            return decode_error("truehd: max matrix channel exceeds the maximum");
        }

        // This should happen for TrueHD streams with >6 channels and MLP's noise type. It is not
        // yet known if this is allowed.
        if max_matrix_channel > MAX_MATRIX_CHANNEL_MLP && !noise_type {
            return unsupported_error("truehd: more channels than supported by the decoder");
        }

        if max_channel + 1 > MAX_CHANNELS || max_channel + 1 < min_channel {
            return decode_error("truehd: invalid channel range");
        }

        let is_max_substr = substr == self.max_decoded_substream;

        {
            let s = &mut self.substreams[substr];
            s.min_channel = min_channel;
            s.max_channel = max_channel;
            s.coded_channels = ((1u64 << (max_channel + 1 - min_channel)) - 1) << min_channel;
            s.max_matrix_channel = max_matrix_channel;
            s.noise_type = noise_type;
        }

        let noise_shift = br.read(4)? as u8;
        let noisegen_seed = br.read(23)?;

        br.skip(19)?;

        let data_check_present = br.read_bit()? != 0;
        let lossless_check = br.read(8)? as u8;

        {
            let s = &mut self.substreams[substr];
            s.noise_shift = noise_shift;
            s.noisegen_seed = noisegen_seed;
            s.data_check_present = data_check_present;

            if is_max_substr && s.lossless_check_data != LOSSLESS_CHECK_INVALID {
                let check = xor_32_to_8(s.lossless_check_data);
                if check != lossless_check {
                    warn!(
                        "truehd: lossless check failed - expected {:02x}, calculated {:02x}",
                        lossless_check, check
                    );
                }
            }
        }

        br.skip(16)?;

        let is_truehd = !self.is_mlp;
        let mut ch_assign = [0u8; MAX_CHANNELS];

        for ch in 0..=max_matrix_channel {
            let value = br.read(6)? as usize;

            let assigned = if is_truehd {
                // The value indexes the channels present in the layout in Dolby order. The
                // channel is placed at its canonical position in the output.
                let layout = &self.substreams[substr].layout;
                layout.get(value).map(|pos| layout.canonical_index(pos))
            }
            else {
                Some(value)
            };

            match assigned {
                Some(out) if out <= max_matrix_channel => ch_assign[out] = ch as u8,
                _ => return unsupported_error("truehd: invalid output channel assignment"),
            }
        }

        // Verify the restart header checksum. The header can only be verified if it starts at
        // the very beginning of the substream (after the two flag bits).
        let checksum = br.read(8)? as u8;

        if start_count == 2 {
            match crc::restart_checksum(data, br.bit_pos() - 8 - start_count) {
                Some(calc) if calc == checksum => (),
                _ => warn!("truehd: restart header checksum error"),
            }
        }

        // Set default decoding parameters.
        let s = &mut self.substreams[substr];

        s.ch_assign = ch_assign;
        s.param_presence_flags = 0xff;
        s.num_primitive_matrices = 0;
        s.blocksize = 8;
        s.lossless_check_data = 0;
        s.output_shift = [0; MAX_CHANNELS];
        s.quant_step_size = [0; MAX_CHANNELS];

        for cp in &mut s.channel_params[min_channel..=max_channel] {
            cp.filter_params[FIR].order = 0;
            cp.filter_params[IIR].order = 0;
            cp.filter_params[FIR].shift = 0;
            cp.filter_params[IIR].shift = 0;

            // Default audio coding is 24-bit raw PCM.
            cp.huff_offset = 0;
            cp.sign_huff_offset = -(1 << 23);
            cp.codebook = 0;
            cp.huff_lsbs = 24;
        }

        if is_max_substr {
            self.out_channels = s.layout.mask();
            self.out_num_channels = s.layout.len();

            if self.is_mlp && self.needs_reordering {
                const FL: Position = Position::FRONT_LEFT;
                const FR: Position = Position::FRONT_RIGHT;
                const FC: Position = Position::FRONT_CENTER;
                const LFE: Position = Position::LFE1;
                const BL: Position = Position::REAR_LEFT;
                const BR: Position = Position::REAR_RIGHT;

                let mask = s.layout.mask();

                if mask == (FL | FR | LFE | BL | BR) || mask == (FL | FR | FC | BL | BR) {
                    let i = s.ch_assign[4];
                    s.ch_assign[4] = s.ch_assign[3];
                    s.ch_assign[3] = s.ch_assign[2];
                    s.ch_assign[2] = i;
                }
                else if mask == (FL | FR | FC | LFE | BL | BR) {
                    s.ch_assign.swap(2, 4);
                    s.ch_assign.swap(3, 5);
                }
            }
        }

        Ok(())
    }

    /// Read parameters for one of the prediction filters.
    fn read_filter_params(
        &mut self,
        br: &mut BitReader<'_>,
        substr: usize,
        channel: usize,
        filter: usize,
    ) -> Result<()> {
        let max_order = if filter == IIR { MAX_IIR_ORDER } else { MAX_FIR_ORDER };

        let changed = self.filter_changed[channel][filter];
        self.filter_changed[channel][filter] += 1;

        if changed > 1 {
            return decode_error("truehd: filters may change only once per access unit");
        }

        let cp = &mut self.substreams[substr].channel_params[channel];

        let order = br.read(4)? as usize;
        if order > max_order {
            return decode_error("truehd: filter order exceeds the maximum");
        }
        cp.filter_params[filter].order = order as u8;

        if order > 0 {
            cp.filter_params[filter].shift = br.read(4)? as u8;

            let coeff_bits = br.read(5)?;
            let coeff_shift = br.read(3)?;

            if coeff_bits < 1 || coeff_bits > 16 {
                return decode_error("truehd: filter coeff_bits must be between 1 and 16");
            }
            if coeff_bits + coeff_shift > 16 {
                return decode_error("truehd: sum of coeff_bits and coeff_shift exceeds 16");
            }

            for i in 0..order {
                cp.coeff[filter][i] = br.read_signed(coeff_bits)?.wrapping_shl(coeff_shift);
            }

            if br.read_bit()? != 0 {
                if filter == FIR {
                    return decode_error("truehd: FIR filter has state data specified");
                }

                let state_bits = br.read(4)?;
                let state_shift = br.read(4)?;

                for i in 0..order {
                    cp.filter_params[filter].state[i] = if state_bits != 0 {
                        br.read_signed(state_bits)?.wrapping_shl(state_shift)
                    }
                    else {
                        0
                    };
                }
            }
        }

        Ok(())
    }

    /// Read parameters for primitive matrices.
    fn read_matrix_params(&mut self, br: &mut BitReader<'_>, substr: usize) -> Result<()> {
        let max_primitive_matrices =
            if self.is_mlp { MAX_MATRICES_MLP } else { MAX_MATRICES_TRUEHD };

        let changed = self.matrix_changed;
        self.matrix_changed += 1;

        if changed > 1 {
            return decode_error("truehd: matrices may change only once per access unit");
        }

        let s = &mut self.substreams[substr];

        let result = (|| -> Result<()> {
            s.num_primitive_matrices = br.read(4)? as usize;

            if s.num_primitive_matrices > max_primitive_matrices {
                return decode_error("truehd: too many primitive matrices");
            }

            for mat in 0..s.num_primitive_matrices {
                s.matrix_out_ch[mat] = br.read(4)? as u8;
                let frac_bits = br.read(4)?;
                s.lsb_bypass[mat] = br.read_bit()? != 0;

                if usize::from(s.matrix_out_ch[mat]) > s.max_matrix_channel {
                    return decode_error("truehd: invalid channel specified as output of matrix");
                }
                if frac_bits > 14 {
                    return decode_error("truehd: too many fractional bits specified");
                }

                let max_chan = s.max_matrix_channel + if s.noise_type { 0 } else { 2 };

                for ch in 0..=max_chan {
                    let mut coeff_val = 0;
                    if br.read_bit()? != 0 {
                        coeff_val = br.read_signed(frac_bits + 2)?;
                    }

                    s.matrix_coeff[mat][ch] = coeff_val.wrapping_shl(14 - frac_bits);
                }

                s.matrix_noise_shift[mat] = if s.noise_type { br.read(4)? as u8 } else { 0 };
            }

            Ok(())
        })();

        if result.is_err() {
            s.num_primitive_matrices = 0;
            s.matrix_out_ch = [0; MAX_MATRICES];
        }

        result
    }

    /// Read channel parameters.
    fn read_channel_params(
        &mut self,
        br: &mut BitReader<'_>,
        substr: usize,
        ch: usize,
    ) -> Result<()> {
        let flags = self.substreams[substr].param_presence_flags;

        if flags & PARAM_FIR != 0 && br.read_bit()? != 0 {
            self.read_filter_params(br, substr, ch, FIR)?;
        }

        if flags & PARAM_IIR != 0 && br.read_bit()? != 0 {
            self.read_filter_params(br, substr, ch, IIR)?;
        }

        let cp = &mut self.substreams[substr].channel_params[ch];

        let fir_order = cp.filter_params[FIR].order;
        let iir_order = cp.filter_params[IIR].order;

        if fir_order + iir_order > 8 {
            return decode_error("truehd: total filter orders too high");
        }

        if fir_order != 0
            && iir_order != 0
            && cp.filter_params[FIR].shift != cp.filter_params[IIR].shift
        {
            return decode_error("truehd: FIR and IIR filters must use the same precision");
        }

        // The FIR and IIR filters must have the same precision. To simplify the filtering code,
        // only the precision of the FIR filter is considered. If only the IIR filter is employed,
        // the FIR filter precision is set to that of the IIR filter, so that the filtering code
        // can use it.
        if fir_order == 0 && iir_order != 0 {
            cp.filter_params[FIR].shift = cp.filter_params[IIR].shift;
        }

        if flags & PARAM_HUFFOFFSET != 0 && br.read_bit()? != 0 {
            cp.huff_offset = br.read_signed(15)? as i16;
        }

        cp.codebook = br.read(2)? as u8;
        cp.huff_lsbs = br.read(5)? as u8;

        if cp.codebook > 0 && cp.huff_lsbs > 24 {
            cp.huff_lsbs = 0;
            return decode_error("truehd: invalid huff_lsbs");
        }

        Ok(())
    }

    /// Read decoding parameters that change more often than those in the restart header.
    fn read_decoding_params(&mut self, br: &mut BitReader<'_>, substr: usize) -> Result<()> {
        let mut recompute_sho = 0u32;

        if self.substreams[substr].param_presence_flags & PARAM_PRESENCE != 0 && br.read_bit()? != 0
        {
            self.substreams[substr].param_presence_flags = br.read(8)? as u8;
        }

        let flags = self.substreams[substr].param_presence_flags;

        if flags & PARAM_BLOCKSIZE != 0 && br.read_bit()? != 0 {
            let blocksize = br.read(9)? as usize;

            if blocksize < 8 || blocksize > self.access_unit_size {
                self.substreams[substr].blocksize = 0;
                return decode_error("truehd: invalid blocksize");
            }

            self.substreams[substr].blocksize = blocksize;
        }

        if flags & PARAM_MATRIX != 0 && br.read_bit()? != 0 {
            self.read_matrix_params(br, substr)?;
        }

        if flags & PARAM_OUTSHIFT != 0 && br.read_bit()? != 0 {
            let s = &mut self.substreams[substr];

            for ch in 0..=s.max_matrix_channel {
                s.output_shift[ch] = br.read_signed(4)? as i8;

                if s.output_shift[ch] < 0 {
                    // Negative output shifts are not supported.
                    s.output_shift[ch] = 0;
                }
            }
        }

        if flags & PARAM_QUANTSTEP != 0 && br.read_bit()? != 0 {
            let s = &mut self.substreams[substr];

            for ch in 0..=s.max_channel {
                s.quant_step_size[ch] = br.read(4)? as u8;
                recompute_sho |= 1 << ch;
            }
        }

        let (min_channel, max_channel) = {
            let s = &self.substreams[substr];
            (s.min_channel, s.max_channel)
        };

        let mut ret = Ok(());

        for ch in min_channel..=max_channel {
            let present = match br.read_bit() {
                Ok(bit) => bit != 0,
                Err(e) => {
                    ret = Err(e);
                    break;
                }
            };

            if present {
                recompute_sho |= 1 << ch;

                if let Err(e) = self.read_channel_params(br, substr, ch) {
                    ret = Err(e);
                    break;
                }
            }
        }

        let s = &mut self.substreams[substr];

        for ch in 0..=s.max_channel {
            if recompute_sho & (1 << ch) != 0 {
                let cp = &mut s.channel_params[ch];

                if cp.codebook > 0 && cp.huff_lsbs < s.quant_step_size[ch] {
                    if ret.is_ok() {
                        ret = decode_error("truehd: quant_step_size larger than huff_lsbs");
                    }
                    s.quant_step_size[ch] = 0;
                }

                cp.sign_huff_offset = calculate_sign_huff(cp, s.quant_step_size[ch]);
            }
        }

        ret
    }

    /// Read a sample, consisting of either, both or neither of entropy-coded MSBs and plain LSBs
    /// for each channel of a substream.
    #[inline(always)]
    fn read_huff_channels(
        &mut self,
        br: &mut BitReader<'_>,
        substr: usize,
        pos: usize,
    ) -> Result<()> {
        let s = &self.substreams[substr];
        let row = pos + s.blockpos;

        for mat in 0..s.num_primitive_matrices {
            if s.lsb_bypass[mat] {
                self.bypassed_lsbs[row][mat] = br.read_bit()? as u8;
            }
        }

        for channel in s.min_channel..=s.max_channel {
            let cp = &s.channel_params[channel];
            let codebook = usize::from(cp.codebook);
            let quant_step_size = s.quant_step_size[channel];
            let lsb_bits = i32::from(cp.huff_lsbs) - i32::from(quant_step_size);

            let mut result = 0i32;

            if codebook > 0 {
                result = huffman::read_symbol(br, codebook)?;
            }

            if lsb_bits > 0 {
                result = result
                    .wrapping_shl(lsb_bits as u32)
                    .wrapping_add(br.read(lsb_bits as u32)? as i32);
            }

            result = result.wrapping_add(cp.sign_huff_offset);
            result = result.wrapping_shl(u32::from(quant_step_size));

            self.sample_buffer[row][channel] = result;
        }

        Ok(())
    }

    /// Generate PCM samples using the prediction filters and residual values read from the data
    /// stream, and update the filter state.
    fn filter_channel(&mut self, substr: usize, channel: usize) {
        let s = &mut self.substreams[substr];
        let blocksize = s.blocksize;
        let blockpos = s.blockpos;
        let mask = i64::from(msb_mask(s.quant_step_size[channel]));
        let cp = &mut s.channel_params[channel];

        let fir_order = usize::from(cp.filter_params[FIR].order);
        let iir_order = usize::from(cp.filter_params[IIR].order);
        let shift = u32::from(cp.filter_params[FIR].shift);

        // History buffers where the most recent sample is at the lowest index of the window, and
        // the window moves towards lower indices as samples are produced.
        let mut firbuf = [0i32; MAX_BLOCKSIZE + MAX_FIR_ORDER];
        let mut iirbuf = [0i32; MAX_BLOCKSIZE + MAX_FIR_ORDER];

        firbuf[MAX_BLOCKSIZE..].copy_from_slice(&cp.filter_params[FIR].state);
        iirbuf[MAX_BLOCKSIZE..].copy_from_slice(&cp.filter_params[IIR].state);

        let mut p = MAX_BLOCKSIZE;

        for row in &mut self.sample_buffer[blockpos..blockpos + blocksize] {
            let residual = i64::from(row[channel]);
            let mut accum = 0i64;

            for order in 0..fir_order {
                accum += i64::from(firbuf[p + order]) * i64::from(cp.coeff[FIR][order]);
            }
            for order in 0..iir_order {
                accum += i64::from(iirbuf[p + order]) * i64::from(cp.coeff[IIR][order]);
            }

            accum >>= shift;
            let result = ((accum + residual) & mask) as i32;

            p -= 1;
            firbuf[p] = result;
            iirbuf[p] = (i64::from(result) - accum) as i32;

            row[channel] = result;
        }

        cp.filter_params[FIR].state.copy_from_slice(&firbuf[p..p + MAX_FIR_ORDER]);
        cp.filter_params[IIR].state.copy_from_slice(&iirbuf[p..p + MAX_FIR_ORDER]);
    }

    /// Read a block of PCM residual data (or actual if no filtering active).
    fn read_block_data(&mut self, br: &mut BitReader<'_>, substr: usize) -> Result<()> {
        let mut expected_stream_pos = 0;

        if self.substreams[substr].data_check_present {
            expected_stream_pos = br.bit_pos();
            expected_stream_pos += br.read(16)? as usize;
            debug!("truehd: substreams with VLC block size check info");
        }

        let (blocksize, blockpos) = {
            let s = &self.substreams[substr];
            (s.blocksize, s.blockpos)
        };

        if blockpos + blocksize > self.access_unit_size {
            return decode_error("truehd: too many audio samples in access unit");
        }

        for lsbs in &mut self.bypassed_lsbs[blockpos..blockpos + blocksize] {
            *lsbs = [0; MAX_CHANNELS];
        }

        for i in 0..blocksize {
            self.read_huff_channels(br, substr, i)?;
        }

        let (min_channel, max_channel) = {
            let s = &self.substreams[substr];
            (s.min_channel, s.max_channel)
        };

        for ch in min_channel..=max_channel {
            self.filter_channel(substr, ch);
        }

        self.substreams[substr].blockpos += blocksize;

        if self.substreams[substr].data_check_present {
            if br.bit_pos() != expected_stream_pos {
                warn!("truehd: block data length mismatch");
            }
            br.skip(8)?;
        }

        Ok(())
    }

    /// Generate two channels of noise, used in the matrix when restart sync word == 0x31ea.
    fn generate_2_noise_channels(&mut self, substr: usize) {
        let s = &mut self.substreams[substr];
        let mut seed = s.noisegen_seed;
        let maxchan = s.max_matrix_channel;
        let noise_shift = u32::from(s.noise_shift);

        for row in &mut self.sample_buffer[..s.blockpos] {
            let seed_shr7 = (seed >> 7) as u16;
            row[maxchan + 1] = i32::from((seed >> 15) as u8 as i8) << noise_shift;
            row[maxchan + 2] = i32::from(seed_shr7 as u8 as i8) << noise_shift;

            seed = (seed << 16) ^ u32::from(seed_shr7) ^ (u32::from(seed_shr7) << 5);
        }

        s.noisegen_seed = seed;
    }

    /// Generate a block of noise, used when restart sync word == 0x31eb.
    fn fill_noise_buffer(&mut self, substr: usize) {
        let s = &mut self.substreams[substr];
        let mut seed = s.noisegen_seed;

        for noise in &mut self.noise_buffer[..self.access_unit_size_pow2] {
            let seed_shr15 = (seed >> 15) as u8;
            *noise = NOISE_TABLE[usize::from(seed_shr15)];

            seed = (seed << 8) ^ u32::from(seed_shr15) ^ (u32::from(seed_shr15) << 5);
        }

        s.noisegen_seed = seed;
    }

    /// Apply the channel matrices to the decoded samples of a substream.
    fn rematrix(&mut self, substr: usize) -> Result<()> {
        let s = &self.substreams[substr];

        if self.out_num_channels != s.max_matrix_channel + 1 {
            return decode_error("truehd: channel count mismatch");
        }

        if s.blockpos == 0 {
            return decode_error("truehd: no samples to output");
        }

        let mut maxchan = s.max_matrix_channel;

        if !s.noise_type {
            self.generate_2_noise_channels(substr);
            maxchan += 2;
        }
        else {
            self.fill_noise_buffer(substr);
        }

        let s = &self.substreams[substr];
        let blockpos = s.blockpos;

        // Apply the channel matrices in turn to reconstruct the original audio samples.
        for mat in 0..s.num_primitive_matrices {
            let dest_ch = usize::from(s.matrix_out_ch[mat]);
            let coeffs = &s.matrix_coeff[mat];
            let matrix_noise_shift = u32::from(s.matrix_noise_shift[mat]);
            let mask = i64::from(msb_mask(s.quant_step_size[dest_ch]));

            let mut index = s.num_primitive_matrices - mat;
            let index2 = 2 * index + 1;

            for (row, bypass) in
                self.sample_buffer[..blockpos].iter_mut().zip(&self.bypassed_lsbs[..blockpos])
            {
                let mut accum = 0i64;

                for src_ch in 0..=maxchan {
                    accum += i64::from(row[src_ch]) * i64::from(coeffs[src_ch]);
                }

                if matrix_noise_shift != 0 {
                    index &= self.access_unit_size_pow2 - 1;
                    accum +=
                        i64::from(i32::from(self.noise_buffer[index]) << (matrix_noise_shift + 7));
                    index += index2;
                }

                row[dest_ch] = (((accum >> 14) & mask) as i32).wrapping_add(i32::from(bypass[mat]));
            }
        }

        Ok(())
    }

    /// Write the decoded samples of the last decoded access unit, as left-justified 32-bit
    /// samples, to `buf`. `buf` must have `num_samples` frames and the output channel count.
    pub(crate) fn pack_output<A: AudioMut<i32>>(&mut self, buf: &mut A) {
        let substr = self.max_decoded_substream;
        let is32 = self.bits_per_sample > 16;
        let s = &mut self.substreams[substr];

        let mut check = s.lossless_check_data;

        for out_ch in 0..=s.max_matrix_channel {
            let mat_ch = usize::from(s.ch_assign[out_ch]);
            let shift = s.output_shift[mat_ch] as u32;

            let plane = match buf.plane_mut(out_ch) {
                Some(plane) => plane,
                None => continue,
            };

            for (dst, row) in plane.iter_mut().zip(&self.sample_buffer[..s.blockpos]) {
                let sample = row[mat_ch].wrapping_mul(1i32.wrapping_shl(shift));

                check ^= ((sample & 0xff_ffff) as u32) << mat_ch;

                *dst = if is32 {
                    sample.wrapping_mul(256)
                }
                else {
                    i32::from((sample >> 8) as i16) << 16
                };
            }
        }

        s.lossless_check_data = check;
    }

    /// The number of samples in the last decoded access unit.
    pub(crate) fn num_samples(&self) -> usize {
        self.substreams[self.max_decoded_substream].blockpos
    }

    /// Decode an access unit.
    ///
    /// Returns `Ok(true)` if samples are available for `pack_output`, or `Ok(false)` if the
    /// access unit was skipped because no major sync has been seen yet.
    pub(crate) fn decode(&mut self, buf: &[u8]) -> Result<bool> {
        if buf.len() < 4 {
            return decode_error("truehd: packet too short");
        }

        let length = usize::from(u16::from_be_bytes([buf[0], buf[1]]) & 0xfff) * 2;

        if length < 4 || length > buf.len() {
            return decode_error("truehd: invalid access unit length");
        }

        let au = &buf[..length];
        let mut header_size = 4;

        self.is_major_sync_unit = false;

        // The major sync word is 31 bits long (the LSB of the stream type is not part of it).
        if au.len() >= 8 && au[4..7] == [0xf8, 0x72, 0x6f] && au[7] >> 1 == SYNC_TRUEHD >> 1 {
            match self.handle_major_sync(&au[4..]) {
                Ok(size) => header_size += size,
                Err(e) => {
                    self.params_valid = false;
                    return Err(e);
                }
            }
            self.is_major_sync_unit = true;
        }

        if !self.params_valid {
            // Stream parameters not seen; skip the access unit.
            return Ok(false);
        }

        let dir = match self.read_directory(au, header_size) {
            Ok(dir) => dir,
            Err(e) => {
                self.params_valid = false;
                return Err(e);
            }
        };

        let mut start = header_size + dir.header_size;

        for substr in 0..=self.max_decoded_substream {
            let data_len = dir.data_len[substr];
            let data = &au[start..start + data_len];
            start += data_len;

            self.decode_substream(substr, data, dir.parity_present[substr])?;
        }

        self.rematrix(self.max_decoded_substream)?;

        for s in &mut self.substreams[..=self.max_decoded_substream] {
            if s.end_of_stream {
                s.lossless_check_data = LOSSLESS_CHECK_INVALID;
                s.end_of_stream = false;
                self.params_valid = false;
            }
        }

        Ok(true)
    }

    /// Read and verify the substream directory of an access unit.
    fn read_directory(&self, au: &[u8], header_size: usize) -> Result<Directory> {
        let length = au.len();

        let mut dir = Directory::default();
        let mut substream_start = 0;

        let mut br = BitReader::new(&au[header_size.min(length)..]);

        for substr in 0..self.num_substreams {
            let extraword_present = br.read_bit()? != 0;
            let nonrestart_substr = br.read_bit()? != 0;
            let checkdata_present = br.read_bit()? != 0;
            br.skip(1)?;

            let mut end = br.read(12)? as usize * 2;

            dir.header_size += 2;

            if extraword_present {
                if self.is_mlp {
                    return decode_error("truehd: there must be no extraword for MLP");
                }
                br.skip(16)?;
                dir.header_size += 2;
            }

            if length < header_size + dir.header_size {
                return decode_error("truehd: insufficient data for headers");
            }

            if nonrestart_substr == self.is_major_sync_unit {
                return decode_error("truehd: invalid nonrestart_substr");
            }

            if end + header_size + dir.header_size > length {
                debug!("truehd: indicated length of substream {} goes off end of packet", substr);
                end = length - header_size - dir.header_size;
            }

            if end < substream_start {
                return decode_error("truehd: substream end offset is smaller than its start");
            }

            if substr > self.max_decoded_substream {
                continue;
            }

            dir.parity_present[substr] = checkdata_present;
            dir.data_len[substr] = end - substream_start;
            substream_start = end;
        }

        let parity_bits =
            crc::parity(&au[..4]) ^ crc::parity(&au[header_size..header_size + dir.header_size]);

        if (((parity_bits >> 4) ^ parity_bits) & 0xf) != 0xf {
            return decode_error("truehd: parity check failed");
        }

        Ok(dir)
    }

    /// Decode a single substream of an access unit.
    fn decode_substream(&mut self, substr: usize, data: &[u8], parity_present: bool) -> Result<()> {
        let mut br = BitReader::new(data);
        let bit_len = br.bit_len();

        self.matrix_changed = 0;
        self.filter_changed = [[0; 2]; MAX_CHANNELS];
        self.substreams[substr].blockpos = 0;

        loop {
            if br.read_bit()? != 0 {
                if br.read_bit()? != 0 {
                    // A restart header should be present.
                    if let Err(e) = self.read_restart_header(&mut br, data, substr) {
                        debug!("truehd: restart header error in substream {}: {}", substr, e);
                        return self.next_substream(substr);
                    }
                    self.substreams[substr].restart_seen = true;
                }

                if !self.substreams[substr].restart_seen {
                    return self.next_substream(substr);
                }

                if let Err(e) = self.read_decoding_params(&mut br, substr) {
                    debug!("truehd: decoding parameter error in substream {}: {}", substr, e);
                    return self.next_substream(substr);
                }
            }

            if !self.substreams[substr].restart_seen {
                return self.next_substream(substr);
            }

            if self.skip_substream(substr) {
                return self.next_substream(substr);
            }

            self.read_block_data(&mut br, substr)?;

            if br.bit_pos() >= bit_len {
                return decode_error("truehd: substream length mismatch");
            }

            if br.read_bit()? != 0 {
                break;
            }
        }

        br.skip(((16 - (br.bit_pos() & 15)) & 15) as u32)?;

        if bit_len - br.bit_pos().min(bit_len) >= 32 {
            if br.read(16)? != 0xd234 {
                return decode_error("truehd: invalid end of stream marker");
            }

            let shorten_by = br.read(16)?;

            if !self.is_mlp && shorten_by & 0x2000 != 0 {
                let s = &mut self.substreams[substr];
                s.blockpos -= ((shorten_by & 0x1fff) as usize).min(s.blockpos);
            }
            else if self.is_mlp && shorten_by != 0xd234 {
                return decode_error("truehd: invalid end of stream marker");
            }

            debug!("truehd: end of stream indicated");
            self.substreams[substr].end_of_stream = true;
        }

        if parity_present {
            if bit_len - br.bit_pos().min(bit_len) != 16 || data.len() < 2 {
                return decode_error("truehd: substream length mismatch");
            }

            let body = &data[..data.len() - 2];
            let parity = crc::parity(body);
            let checksum = crc::checksum8(body);

            if (br.read(8)? as u8 ^ parity) != 0xa9 {
                warn!("truehd: substream {} parity check failed", substr);
            }
            if br.read(8)? as u8 != checksum {
                warn!("truehd: substream {} checksum failed", substr);
            }
        }

        if bit_len != br.bit_pos() {
            return decode_error("truehd: substream length mismatch");
        }

        self.next_substream(substr)
    }

    fn next_substream(&self, substr: usize) -> Result<()> {
        if !self.substreams[substr].restart_seen {
            debug!("truehd: no restart header present in substream {}", substr);
        }
        Ok(())
    }

    /// Check if the substream must not be decoded because its channels overlap the channels of
    /// another substream that supersedes it. This is the case for presentations that carry the
    /// same channels in multiple substreams.
    fn skip_substream(&self, substr: usize) -> bool {
        let s = &self.substreams[substr];
        let max = &self.substreams[self.max_decoded_substream];

        let nch = self.out_num_channels;
        let si = self.substream_info;

        if ((nch == 6 && ((si >> 2) & 0x3) != 0x3)
            || (nch == 8
                && ((si >> 4) & 0x7) != 0x7
                && ((si >> 4) & 0x7) != 0x6
                && ((si >> 4) & 0x7) != 0x3))
            && substr > 0
            && substr < self.max_decoded_substream
            && s.min_channel <= self.substreams[substr - 1].max_channel
        {
            debug!("truehd: previous substream channels overlap substream {}, skipping", substr);
            return true;
        }

        if substr != self.max_decoded_substream && (s.coded_channels & max.coded_channels) != 0 {
            debug!("truehd: substream {} channels overlap the final substream, skipping", substr);
            return true;
        }

        false
    }
}

fn xor_32_to_8(value: u32) -> u8 {
    let value = value ^ (value >> 16);
    let value = value ^ (value >> 8);
    value as u8
}

fn calculate_sign_huff(cp: &ChannelParams, quant_step_size: u8) -> i32 {
    let lsb_bits = i32::from(cp.huff_lsbs) - i32::from(quant_step_size);
    let sign_shift = lsb_bits + if cp.codebook != 0 { 2 - i32::from(cp.codebook) } else { -1 };

    let mut sign_huff_offset = i32::from(cp.huff_offset);

    if cp.codebook > 0 {
        sign_huff_offset = sign_huff_offset.wrapping_sub(7i32.wrapping_shl(lsb_bits as u32));
    }

    if sign_shift >= 0 {
        sign_huff_offset = sign_huff_offset.wrapping_sub(1i32.wrapping_shl(sign_shift as u32));
    }

    sign_huff_offset
}
