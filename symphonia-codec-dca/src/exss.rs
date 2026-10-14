// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Ported from FFmpeg libavcodec dca_exss.c (LGPL-2.1+), see NOTICE.

//! DTS-HD extension substream (EXSS) header parser (ETSI TS 102 114, section 7).

use symphonia_core::errors::{Result, decode_error};

use crate::bits::BitReader;

const EXSS_CORE: u32 = 0x010;
const EXSS_XBR: u32 = 0x020;
const EXSS_XXCH: u32 = 0x040;
const EXSS_X96: u32 = 0x080;
const EXSS_LBR: u32 = 0x100;
const EXSS_XLL: u32 = 0x200;
const EXSS_RSV1: u32 = 0x400;
const EXSS_RSV2: u32 = 0x800;

/// Maximum sampling frequencies indexed by the extension substream sample rate code.
pub(crate) const SAMPLING_FREQS: [u32; 16] = [
    8000, 16000, 32000, 64000, 128000, 22050, 44100, 88200, 176400, 352800, 12000, 24000, 48000,
    96000, 192000, 384000,
];

/// The number of individual channels in a speaker pair mask.
fn count_chs_for_mask(mask: u32) -> u32 {
    ((mask & 0xffff) | ((mask & 0xae66) << 16)).count_ones()
}

/// An audio asset descriptor of an extension substream.
#[derive(Clone, Debug, Default)]
pub struct ExssAsset {
    /// Audio asset identifier.
    pub asset_index: u32,
    /// Offset to the asset data from the start of the substream.
    pub asset_offset: usize,
    /// Size of the encoded asset data.
    pub asset_size: usize,
    /// PCM bit resolution.
    pub pcm_bit_res: u32,
    /// Maximum sample rate.
    pub max_sample_rate: u32,
    /// Total number of channels.
    pub nchannels_total: u32,
    /// Loudspeaker activity mask, if enabled.
    pub spkr_mask: u32,
    /// Coding components used in the asset (`ext::*` mask).
    pub extension_mask: u32,
    /// One to one channel to speaker mapping flag.
    pub one_to_one_map_ch_to_spkr: bool,
    /// Representation type (when the channel to speaker mapping is not one to one).
    pub representation_type: u32,
    /// DTS-HD stream ID.
    pub hd_stream_id: u32,

    pub core_offset: usize,
    pub core_size: usize,
    pub xbr_offset: usize,
    pub xbr_size: usize,
    pub xxch_offset: usize,
    pub xxch_size: usize,
    pub x96_offset: usize,
    pub x96_size: usize,
    pub lbr_offset: usize,
    pub lbr_size: usize,
    pub xll_offset: usize,
    pub xll_size: usize,
    pub xll_sync_present: bool,
    pub xll_delay_nframes: u32,
    pub xll_sync_offset: usize,
}

impl ExssAsset {
    pub fn has_lbr(&self) -> bool {
        self.extension_mask & EXSS_LBR != 0
    }

    pub fn has_xll(&self) -> bool {
        self.extension_mask & EXSS_XLL != 0
    }
}

struct Parser<'a> {
    gb: BitReader<'a>,
    exss_size_nbits: u32,
    static_fields_present: bool,
    mix_metadata_enabled: bool,
    nmixoutconfigs: usize,
    nmixoutchs: [u32; 4],
}

impl Parser<'_> {
    fn parse_xll_parameters(&mut self, asset: &mut ExssAsset) {
        let gb = &mut self.gb;

        // Size of XLL data in extension substream.
        asset.xll_size = gb.get(self.exss_size_nbits) as usize + 1;

        // XLL sync word present flag.
        asset.xll_sync_present = gb.get1();
        if asset.xll_sync_present {
            // Peak bit rate smoothing buffer size.
            gb.skip(4);

            // Number of bits for XLL decoding delay.
            let xll_delay_nbits = gb.get(5) + 1;

            // Initial XLL decoding delay in frames.
            asset.xll_delay_nframes = gb.get(xll_delay_nbits);

            // Number of bytes offset to XLL sync.
            asset.xll_sync_offset = gb.get(self.exss_size_nbits) as usize;
        }
        else {
            asset.xll_delay_nframes = 0;
            asset.xll_sync_offset = 0;
        }
    }

    fn parse_lbr_parameters(&mut self, asset: &mut ExssAsset) {
        let gb = &mut self.gb;

        // Size of LBR component in extension substream.
        asset.lbr_size = gb.get(14) as usize + 1;

        // LBR sync word present flag.
        if gb.get1() {
            // LBR sync distance.
            gb.skip(2);
        }
    }

    fn parse_descriptor(&mut self, asset: &mut ExssAsset) -> Result<()> {
        let descr_pos = self.gb.pos();

        // Size of audio asset descriptor in bytes.
        let descr_size = self.gb.get(9) as usize + 1;

        // Audio asset identifier.
        asset.asset_index = self.gb.get(3);

        let mut embedded_stereo = false;
        let mut embedded_6ch = false;

        // Per stream static metadata.
        if self.static_fields_present {
            let gb = &mut self.gb;

            // Asset type descriptor presence.
            if gb.get1() {
                gb.skip(4);
            }

            // Language descriptor presence.
            if gb.get1() {
                gb.skip(24);
            }

            // Additional textual information presence.
            if gb.get1() {
                // Byte size of additional text info.
                let text_size = gb.get(10) as usize + 1;

                // Sanity check available size.
                if gb.left() < (text_size * 8) as i64 {
                    return decode_error("dca: exss text info too long");
                }

                gb.skip(text_size * 8);
            }

            // PCM bit resolution.
            asset.pcm_bit_res = gb.get(5) + 1;

            // Maximum sample rate.
            asset.max_sample_rate = SAMPLING_FREQS[gb.get(4) as usize];

            // Total number of channels.
            asset.nchannels_total = gb.get(8) + 1;

            // One to one map channel to speakers.
            asset.one_to_one_map_ch_to_spkr = gb.get1();
            if asset.one_to_one_map_ch_to_spkr {
                let mut spkr_mask_nbits = 0;
                let mut nspeakers = [0u32; 8];

                // Embedded stereo flag.
                embedded_stereo = asset.nchannels_total > 2 && gb.get1();

                // Embedded 6 channels flag.
                embedded_6ch = asset.nchannels_total > 6 && gb.get1();

                // Speaker mask enabled flag.
                if gb.get1() {
                    // Number of bits for speaker activity mask.
                    spkr_mask_nbits = (gb.get(2) + 1) << 2;

                    // Loudspeaker activity mask.
                    asset.spkr_mask = gb.get(spkr_mask_nbits);
                }

                // Number of speaker remapping sets.
                let spkr_remap_nsets = gb.get(3) as usize;
                if spkr_remap_nsets != 0 && spkr_mask_nbits == 0 {
                    return decode_error("dca: exss speaker mask disabled with remapping sets");
                }

                // Standard loudspeaker layout mask.
                for n in &mut nspeakers[..spkr_remap_nsets] {
                    *n = count_chs_for_mask(gb.get(spkr_mask_nbits));
                }

                for &nspeaker in &nspeakers[..spkr_remap_nsets] {
                    // Number of channels to be decoded for speaker remapping.
                    let nch_for_remaps = gb.get(5) + 1;

                    for _ in 0..nspeaker {
                        // Decoded channels to output speaker mapping mask.
                        let remap_ch_mask = gb.get(nch_for_remaps);

                        // Loudspeaker remapping codes.
                        gb.skip(remap_ch_mask.count_ones() as usize * 5);
                    }
                }
            }
            else {
                asset.spkr_mask = 0;

                // Representation type.
                asset.representation_type = gb.get(3);
            }
        }

        let gb = &mut self.gb;

        // DRC, DNC and mixing metadata.

        // Dynamic range coefficient presence flag.
        let drc_present = gb.get1();

        // Code for dynamic range coefficient.
        if drc_present {
            gb.skip(8);
        }

        // Dialog normalization presence flag.
        if gb.get1() {
            gb.skip(5);
        }

        // DRC for stereo downmix.
        if drc_present && embedded_stereo {
            gb.skip(8);
        }

        // Mixing metadata presence flag.
        if self.mix_metadata_enabled && gb.get1() {
            // External mixing flag.
            gb.skip(1);

            // Post mixing / replacement gain adjustment.
            gb.skip(6);

            // DRC prior to mixing.
            if gb.get(2) == 3 {
                // Custom code for mixing DRC.
                gb.skip(8);
            }
            else {
                // Limit for mixing DRC.
                gb.skip(3);
            }

            // Scaling type for channels of main audio, scaling parameters of main audio.
            if gb.get1() {
                for i in 0..self.nmixoutconfigs {
                    gb.skip(6 * self.nmixoutchs[i] as usize);
                }
            }
            else {
                gb.skip(6 * self.nmixoutconfigs);
            }

            let mut nchannels_dmix = asset.nchannels_total;
            if embedded_6ch {
                nchannels_dmix += 6;
            }
            if embedded_stereo {
                nchannels_dmix += 2;
            }

            for i in 0..self.nmixoutconfigs {
                if self.nmixoutchs[i] == 0 {
                    return decode_error(
                        "dca: invalid speaker layout mask for mixing configuration",
                    );
                }
                for _ in 0..nchannels_dmix {
                    // Mix output mask.
                    let mix_map_mask = gb.get(self.nmixoutchs[i]);

                    // Mixing coefficients.
                    gb.skip(mix_map_mask.count_ones() as usize * 6);
                }
            }
        }

        // Decoder navigation data.

        // Coding mode for the asset.
        let coding_mode = gb.get(2);

        // Coding components used in asset.
        match coding_mode {
            0 => {
                // Coding mode that may contain multiple coding components.
                asset.extension_mask = gb.get(12);

                if asset.extension_mask & EXSS_CORE != 0 {
                    // Size of core component in extension substream.
                    asset.core_size = gb.get(14) as usize + 1;
                    // Core sync word present flag.
                    if gb.get1() {
                        // Core sync distance.
                        gb.skip(2);
                    }
                }

                if asset.extension_mask & EXSS_XBR != 0 {
                    // Size of XBR extension in extension substream.
                    asset.xbr_size = gb.get(14) as usize + 1;
                }

                if asset.extension_mask & EXSS_XXCH != 0 {
                    // Size of XXCH extension in extension substream.
                    asset.xxch_size = gb.get(14) as usize + 1;
                }

                if asset.extension_mask & EXSS_X96 != 0 {
                    // Size of X96 extension in extension substream.
                    asset.x96_size = gb.get(12) as usize + 1;
                }

                if asset.extension_mask & EXSS_LBR != 0 {
                    self.parse_lbr_parameters(asset);
                }

                if asset.extension_mask & EXSS_XLL != 0 {
                    self.parse_xll_parameters(asset);
                }

                if asset.extension_mask & EXSS_RSV1 != 0 {
                    self.gb.skip(16);
                }

                if asset.extension_mask & EXSS_RSV2 != 0 {
                    self.gb.skip(16);
                }
            }
            1 => {
                // Loss-less coding mode without CBR component.
                asset.extension_mask = EXSS_XLL;
                self.parse_xll_parameters(asset);
            }
            2 => {
                // Low bit rate mode.
                asset.extension_mask = EXSS_LBR;
                self.parse_lbr_parameters(asset);
            }
            _ => {
                // Auxiliary coding mode.
                asset.extension_mask = 0;

                // Size of auxiliary coded data.
                gb.skip(14);

                // Auxiliary codec identification.
                gb.skip(8);

                // Aux sync word present flag.
                if gb.get1() {
                    // Aux sync distance.
                    gb.skip(3);
                }
            }
        }

        if asset.extension_mask & EXSS_XLL != 0 {
            // DTS-HD stream ID.
            asset.hd_stream_id = self.gb.get(3);
        }

        // One to one mixing flag, per channel main audio scaling flag, main audio scaling codes,
        // decode asset in secondary decoder flag, revision 2 DRC metadata, reserved, zero pad.
        if !self.gb.seek(descr_pos + descr_size * 8) {
            return decode_error("dca: read past end of exss asset descriptor");
        }

        Ok(())
    }
}

fn set_exss_offsets(asset: &mut ExssAsset) -> bool {
    let mut offs = asset.asset_offset;
    let mut size = asset.asset_size;

    let mut take = |present: bool, off: &mut usize, comp_size: usize| -> bool {
        if present {
            *off = offs;
            if comp_size > size {
                return false;
            }
            offs += comp_size;
            size -= comp_size;
        }
        true
    };

    let mask = asset.extension_mask;
    take(mask & EXSS_CORE != 0, &mut asset.core_offset, asset.core_size)
        && take(mask & EXSS_XBR != 0, &mut asset.xbr_offset, asset.xbr_size)
        && take(mask & EXSS_XXCH != 0, &mut asset.xxch_offset, asset.xxch_size)
        && take(mask & EXSS_X96 != 0, &mut asset.x96_offset, asset.x96_size)
        && take(mask & EXSS_LBR != 0, &mut asset.lbr_offset, asset.lbr_size)
        && take(mask & EXSS_XLL != 0, &mut asset.xll_offset, asset.xll_size)
}

/// Parse an extension substream header and all of its audio assets, and return the asset to
/// decode. `data` starts at the EXSS sync word.
pub fn parse(data: &[u8]) -> Result<ExssAsset> {
    let mut p = Parser {
        gb: BitReader::new(data),
        exss_size_nbits: 0,
        static_fields_present: false,
        mix_metadata_enabled: false,
        nmixoutconfigs: 0,
        nmixoutchs: [0; 4],
    };

    // Extension substream sync word.
    p.gb.skip(32);

    // User defined bits.
    p.gb.skip(8);

    // Extension substream index.
    let exss_index = p.gb.get(2);

    // Flag indicating short or long header size.
    let wide_hdr = p.gb.get(1);

    // Extension substream header length.
    let header_size = p.gb.get(8 + 4 * wide_hdr) as usize + 1;

    p.exss_size_nbits = 16 + 4 * wide_hdr;

    // Number of bytes of extension substream.
    let exss_size = p.gb.get(p.exss_size_nbits) as usize + 1;
    if exss_size > data.len() {
        return decode_error("dca: packet too short for exss frame");
    }

    // Without static fields there is a single presentation with a single asset.
    let mut nassets = 1;

    // Bit mask of the assets of this substream that are active in the first audio presentation.
    let mut active_asset_mask = 0u32;

    // Per stream static fields presence flag.
    p.static_fields_present = p.gb.get1();
    if p.static_fields_present {
        let gb = &mut p.gb;

        // Reference clock code.
        gb.skip(2);

        // Extension substream frame duration.
        gb.skip(3);

        // Timecode presence flag.
        if gb.get1() {
            // Timecode data.
            gb.skip(36);
        }

        // Number of defined audio presentations.
        let npresents = gb.get(3) as usize + 1;

        // Number of audio assets in extension substream.
        nassets = gb.get(3) as usize + 1;

        // Active extension substream mask for each audio presentation.
        let mut active_exss_mask = [0u32; 8];
        for mask in &mut active_exss_mask[..npresents] {
            *mask = gb.get(exss_index + 1);
        }

        // Active audio asset mask for each audio presentation and active extension substream.
        // The first presentation is the one that is decoded: record the asset mask of this
        // substream if it takes part in it.
        for (i, &mask) in active_exss_mask[..npresents].iter().enumerate() {
            for ss in 0..=exss_index {
                if mask & (1 << ss) != 0 {
                    let asset_mask = gb.get(8);
                    if i == 0 && ss == exss_index {
                        active_asset_mask = asset_mask;
                    }
                }
            }
        }

        // Mixing metadata enable flag.
        p.mix_metadata_enabled = gb.get1();
        if p.mix_metadata_enabled {
            // Mixing metadata adjustment level.
            gb.skip(2);

            // Number of bits for mixer output speaker activity mask.
            let spkr_mask_nbits = (gb.get(2) + 1) << 2;

            // Number of mixing configurations.
            p.nmixoutconfigs = gb.get(2) as usize + 1;

            // Speaker layout mask for mixer output channels.
            for i in 0..p.nmixoutconfigs {
                p.nmixoutchs[i] = count_chs_for_mask(gb.get(spkr_mask_nbits));
            }
        }
    }

    // Size of encoded asset data in bytes.
    let mut assets = Vec::with_capacity(nassets);
    let mut offset = header_size;
    for _ in 0..nassets {
        let asset_size = p.gb.get(p.exss_size_nbits) as usize + 1;
        assets.push(ExssAsset { asset_offset: offset, asset_size, ..Default::default() });
        offset += asset_size;
        if offset > exss_size {
            return decode_error("dca: exss asset out of bounds");
        }
    }

    // Audio asset descriptors.
    for asset in &mut assets {
        p.parse_descriptor(asset)?;
        if !set_exss_offsets(asset) {
            return decode_error("dca: invalid extension size in exss asset descriptor");
        }
    }

    // Backward compatible core present, backward compatible core substream index, backward
    // compatible core asset index, reserved, byte align, CRC16 of extension substream header.
    if !p.gb.seek(header_size * 8) {
        return decode_error("dca: read past end of exss header");
    }

    // Decode the lowest numbered asset that is active in the first audio presentation, otherwise
    // the first asset.
    let selected = (0..assets.len())
        .find(|&i| {
            let id = assets[i].asset_index;
            id < 8 && active_asset_mask & (1 << id) != 0
        })
        .unwrap_or(0);

    Ok(assets.swap_remove(selected))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct BitWriter {
        bits: Vec<bool>,
    }

    impl BitWriter {
        fn put(&mut self, nbits: u32, value: u32) {
            for i in (0..nbits).rev() {
                self.bits.push(value >> i & 1 != 0);
            }
        }

        fn into_bytes(mut self) -> Vec<u8> {
            while self.bits.len() % 8 != 0 {
                self.bits.push(false);
            }
            self.bits.chunks(8).map(|c| c.iter().fold(0, |b, &x| b << 1 | u8::from(x))).collect()
        }
    }

    /// Put a descriptor of an asset holding only a core component of `core_size` bytes.
    fn put_core_descriptor(w: &mut BitWriter, asset_index: u32, core_size: u32) {
        w.put(9, 8); // Descriptor size (9 bytes).
        w.put(3, asset_index);
        w.put(3, 0); // No asset type, language or text.
        w.put(5, 15); // 16-bit.
        w.put(4, 12); // 48 kHz.
        w.put(8, 1); // 2 channels.
        w.put(1, 0); // Not one to one mapped.
        w.put(3, 0); // Representation type.
        w.put(2, 0); // No DRC, no dialog normalization.
        w.put(2, 0); // Coding mode 0.
        w.put(12, 0x010); // Core only.
        w.put(14, core_size - 1);
        w.put(1, 0); // No core sync word.
        w.put(5, 0); // Pad to 9 bytes (72 bits).
    }

    /// An EXSS with two core assets of 4 and 6 bytes of which `active` is active.
    fn two_asset_exss(active: u32) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.put(32, 0x6458_2025);
        w.put(8, 0); // User defined bits.
        w.put(2, 0); // Substream index.
        w.put(1, 0); // Short header.
        let mut body = BitWriter::default();
        body.put(1, 1); // Static fields.
        body.put(2, 0);
        body.put(3, 0);
        body.put(1, 0); // No timecode.
        body.put(3, 1); // Two presentations: 1 + 1.
        body.put(3, 1); // Two assets.
        body.put(1, 1); // Presentation 0: substream 0 active.
        body.put(1, 1); // Presentation 1: substream 0 active.
        body.put(8, active); // Asset mask of presentation 0.
        body.put(8, 1); // Asset mask of presentation 1.
        body.put(1, 0); // No mixing metadata.
        body.put(16, 3); // Asset 0 size - 1.
        body.put(16, 5); // Asset 1 size - 1.
        put_core_descriptor(&mut body, 0, 4);
        put_core_descriptor(&mut body, 1, 6);
        let body_bits = body.bits.len();
        // Fixed fields: 32 + 8 + 2 + 1 + 8 (header size) + 16 (exss size) = 67 bits.
        let header_len = (67 + body_bits).div_ceil(8) + 2;
        let total = header_len + 10;
        w.put(8, header_len as u32 - 1);
        w.put(16, total as u32 - 1);
        w.bits.extend(body.bits);
        let mut out = w.into_bytes();
        out.resize(total, 0);
        out
    }

    #[test]
    fn verify_multiple_assets_select_active() {
        let asset = parse(&two_asset_exss(0b10)).expect("two asset exss header");
        assert_eq!(asset.asset_index, 1);
        assert_eq!(asset.core_size, 6);
        assert_eq!(asset.asset_size, 6);
        assert!(asset.asset_offset > 4);

        let asset = parse(&two_asset_exss(0b01)).expect("two asset exss header");
        assert_eq!(asset.asset_index, 0);
        assert_eq!(asset.core_size, 4);
        assert_eq!(asset.asset_size, 4);
    }
}
