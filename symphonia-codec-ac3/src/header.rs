// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Syncframe header (syncinfo + bsi) parsing for AC-3 and E-AC-3.

use crate::bits::BitReader;
use crate::tables::FRAME_SIZE_WORDS;

/// The AC-3 syncword.
pub const SYNC_WORD: u16 = 0x0b77;

/// The minimum number of bytes needed to parse any syncframe header up to the point the frame
/// size is known.
pub const MIN_HEADER_BYTES: usize = 8;

/// Sample rates indexed by `fscod`.
const SAMPLE_RATES: [u32; 3] = [48_000, 44_100, 32_000];

/// Number of full bandwidth channels indexed by `acmod`.
pub const FBW_CHANNELS: [usize; 8] = [2, 1, 2, 3, 3, 4, 4, 5];

/// Number of audio blocks per E-AC-3 syncframe indexed by `numblkscod`.
const EAC3_BLOCKS: [usize; 4] = [1, 2, 3, 6];

#[derive(Debug, PartialEq, Eq)]
pub enum HeaderError {
    /// The syncword was not found.
    Sync,
    /// The bit stream identification is not AC-3 or E-AC-3.
    Bsid,
    /// The sample rate code is reserved.
    SampleRate,
    /// The frame size is invalid.
    FrameSize,
    /// A reserved frame type.
    FrameType,
}

/// E-AC-3 stream types.
pub const STRMTYP_INDEPENDENT: u8 = 0;
pub const STRMTYP_DEPENDENT: u8 = 1;
pub const STRMTYP_CONVERT: u8 = 2;

#[derive(Clone, Debug)]
pub struct FrameHeader {
    pub eac3: bool,
    /// Syncframe size in bytes.
    pub frame_size: usize,
    pub sample_rate: u32,
    /// Index of the sample rate for the bit allocation tables.
    pub sr_code: usize,
    /// Right shift implied by the half and quarter sample rate AC-3 bit stream ids.
    pub sr_shift: usize,
    pub acmod: u8,
    pub lfeon: bool,
    pub strmtyp: u8,
    pub substreamid: u8,
    pub num_blocks: usize,
    /// The frame uses a bit stream version (bsid 9 or 10) that decoders shall mute.
    pub muted: bool,
    /// The custom channel map of a dependent substream, if present.
    pub chanmap: Option<u16>,
    /// The bit offset of the first bit after the bit stream information.
    pub header_bits: usize,
}

impl FrameHeader {
    pub fn fbw_channels(&self) -> usize {
        FBW_CHANNELS[usize::from(self.acmod)]
    }

    pub fn channels(&self) -> usize {
        self.fbw_channels() + usize::from(self.lfeon)
    }
}

/// Parses the syncframe header at the start of `buf`.
pub fn parse(buf: &[u8]) -> Result<FrameHeader, HeaderError> {
    let mut br = BitReader::new(buf);

    if br.get(16) != u32::from(SYNC_WORD) {
        return Err(HeaderError::Sync);
    }

    // Read ahead to the bsid to tell AC-3 and E-AC-3 apart.
    let bsid = (buf.get(5).copied().unwrap_or(0) >> 3) & 0x1f;

    if bsid > 16 {
        return Err(HeaderError::Bsid);
    }

    let hdr = if bsid <= 10 { parse_ac3(&mut br, bsid) } else { parse_eac3(&mut br) }?;

    if br.overrun() {
        return Err(HeaderError::FrameSize);
    }

    Ok(hdr)
}

fn parse_ac3(br: &mut BitReader<'_>, bsid: u8) -> Result<FrameHeader, HeaderError> {
    br.skip(16); // crc1

    let fscod = br.get(2) as usize;
    if fscod == 3 {
        return Err(HeaderError::SampleRate);
    }

    let frmsizecod = br.get(6) as usize;
    if frmsizecod > 37 {
        return Err(HeaderError::FrameSize);
    }

    br.skip(5); // bsid
    br.skip(3); // bsmod

    let acmod = br.get(3) as u8;

    if (acmod & 1) != 0 && acmod != 1 {
        br.skip(2); // cmixlev
    }
    if (acmod & 4) != 0 {
        br.skip(2); // surmixlev
    }
    if acmod == 2 {
        br.skip(2); // dsurmod
    }

    let lfeon = br.bit();

    // dialnorm, compre/compr, langcode, audprodie; repeated for dual mono.
    for _ in 0..(if acmod == 0 { 2 } else { 1 }) {
        br.skip(5);
        if br.bit() {
            br.skip(8);
        }
        if br.bit() {
            br.skip(8);
        }
        if br.bit() {
            br.skip(7);
        }
    }

    br.skip(2); // copyrightb, origbs

    // timecod1e and timecod2e, or xbsi1e and xbsi2e of the alternate bit stream syntax (bsid 6).
    // Both pairs of optional fields are 14 bits wide.
    if br.bit() {
        br.skip(14);
    }
    if br.bit() {
        br.skip(14);
    }

    // addbsie
    if br.bit() {
        let len = br.get(6) as usize + 1;
        br.skip(len * 8);
    }

    let sr_shift = usize::from(bsid.max(8) - 8);

    Ok(FrameHeader {
        eac3: false,
        frame_size: usize::from(FRAME_SIZE_WORDS[frmsizecod][fscod]) * 2,
        sample_rate: SAMPLE_RATES[fscod] >> sr_shift,
        sr_code: fscod,
        sr_shift,
        acmod,
        lfeon,
        strmtyp: STRMTYP_INDEPENDENT,
        substreamid: 0,
        num_blocks: 6,
        // ETSI TS 102 366 clauses 4.4.1.2 and E.1.3.1.6: bsid 9 and 10 shall be muted.
        muted: bsid > 8,
        chanmap: None,
        header_bits: br.pos(),
    })
}

fn parse_eac3(br: &mut BitReader<'_>) -> Result<FrameHeader, HeaderError> {
    let strmtyp = br.get(2) as u8;
    if strmtyp == 3 {
        return Err(HeaderError::FrameType);
    }

    let substreamid = br.get(3) as u8;
    let frame_size = (br.get(11) as usize + 1) * 2;
    if frame_size < MIN_HEADER_BYTES {
        return Err(HeaderError::FrameSize);
    }

    // A sample rate code of 3 selects a reduced sample rate, given by fscod2, and implies six
    // blocks per syncframe (ATSC A/52:2018 Annex E, clauses 2.3.1.4 and 2.3.1.5).
    let fscod = br.get(2) as usize;
    let (sr_code, sr_shift, num_blocks) = if fscod == 3 {
        let fscod2 = br.get(2) as usize;
        if fscod2 == 3 {
            return Err(HeaderError::SampleRate);
        }
        // The reduced rates are the halves of the 48, 44.1 and 32 kHz rates, which is also how
        // the bit allocation tables are indexed.
        (fscod2, 1, 6)
    }
    else {
        (fscod, 0, EAC3_BLOCKS[br.get(2) as usize])
    };

    let acmod = br.get(3) as u8;
    let lfeon = br.bit();

    br.skip(5); // bsid

    let two = if acmod == 0 { 2 } else { 1 };

    // dialnorm, compre/compr
    for _ in 0..two {
        br.skip(5);
        if br.bit() {
            br.skip(8);
        }
    }

    // chanmape, chanmap
    let mut chanmap = None;
    if strmtyp == STRMTYP_DEPENDENT && br.bit() {
        chanmap = Some(br.get(16) as u16);
    }

    // mixmdate
    if br.bit() {
        if acmod > 2 {
            br.skip(2); // dmixmod
            if (acmod & 1) != 0 {
                br.skip(6); // ltrtcmixlev, lorocmixlev
            }
            if (acmod & 4) != 0 {
                br.skip(6); // ltrtsurmixlev, lorosurmixlev
            }
        }
        if lfeon && br.bit() {
            br.skip(5); // lfemixlevcod
        }
        if strmtyp == STRMTYP_INDEPENDENT {
            for _ in 0..two {
                if br.bit() {
                    br.skip(6); // pgmscl
                }
            }
            if br.bit() {
                br.skip(6); // extpgmscl
            }
            match br.get(2) {
                1 => br.skip(5),
                2 => br.skip(12),
                3 => {
                    let size = (br.get(5) as usize + 2) * 8;
                    br.skip(size);
                }
                _ => (),
            }
            if acmod < 2 {
                for _ in 0..two {
                    if br.bit() {
                        br.skip(8 + 6); // panmean, paninfo
                    }
                }
            }
            // frmmixcfginfoe
            if br.bit() {
                if num_blocks == 1 {
                    br.skip(5);
                }
                else {
                    for _ in 0..num_blocks {
                        if br.bit() {
                            br.skip(5);
                        }
                    }
                }
            }
        }
    }

    // infomdate
    if br.bit() {
        br.skip(3 + 2); // bsmod, copyrightb, origbs
        if acmod == 2 {
            br.skip(4); // dsurmod, dheadphonmod
        }
        if acmod >= 6 {
            br.skip(2); // dsurexmod
        }
        for _ in 0..two {
            if br.bit() {
                br.skip(8); // mixlevel, roomtyp, adconvtyp
            }
        }
        if fscod < 3 {
            br.skip(1); // sourcefscod
        }
    }

    // convsync
    if strmtyp == STRMTYP_INDEPENDENT && num_blocks != 6 {
        br.skip(1);
    }

    // blkid, frmsizecod of a converted stream
    if strmtyp == STRMTYP_CONVERT && (num_blocks == 6 || br.bit()) {
        br.skip(6);
    }

    // addbsie
    if br.bit() {
        let len = br.get(6) as usize + 1;
        br.skip(len * 8);
    }

    Ok(FrameHeader {
        eac3: true,
        frame_size,
        sample_rate: SAMPLE_RATES[sr_code] >> sr_shift,
        sr_code,
        sr_shift,
        acmod,
        lfeon,
        strmtyp,
        substreamid,
        num_blocks,
        muted: false,
        chanmap,
        header_bits: br.pos(),
    })
}
