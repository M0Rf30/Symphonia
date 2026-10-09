// Symphonia Musepack demuxer (SV8)
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SV8 packet/chunk demuxer.
//!
//! Ported from libmpcdec `mpc_demux.c` (`mpc_demux_header`/`mpc_bits_get_block`/
//! `mpc_demux_decode_inner`) and `streaminfo.c` (`streaminfo_read_header_sv8`/
//! `streaminfo_gain`/`streaminfo_encoder_info`) (BSD-3-Clause), see `NOTICE`.
//!
//! Block/chunk boundaries are always byte-aligned in SV8 (only the individual Musepack frames
//! *within* one `AP` block's payload are bit-continuous, which is why this file works directly
//! with `MediaSourceStream` rather than `crate::bits::BitReader`; the frame payload bytes handed
//! to the decoder as `Packet::data` are simply the `AP` block's raw bytes).

use std::io::Seek;

use symphonia_core::errors::{decode_error, seek_error, Result, SeekErrorKind};
use symphonia_core::io::{MediaSource, MediaSourceStream, ReadBytes};

use crate::bits::BitReader;
use crate::decoder_core::{Decoder as Core, FRAME_LENGTH, SYNTH_DELAY};

use super::{StreamInfo, PACKET_TAG_NOISE, PACKET_TAG_PLAIN};

const SAMPLE_FREQS: [u32; 4] = [44100, 48000, 37800, 32000];

/// Ported from libmpcdec `mpc_bits_get_block`.
///
/// Reads a block's 2-byte key and size varint. Returns `(key, payload_size)`; `payload_size`
/// excludes the key+size header itself. Also validates the key is two uppercase ASCII letters
/// (`mpc_check_key`).
fn read_block_header(mss: &mut MediaSourceStream<'_>) -> Result<([u8; 2], u64)> {
    let mut key = [0u8; 2];
    mss.read_buf_exact(&mut key).map_err(symphonia_core::errors::Error::IoError)?;
    if !(65..=90).contains(&key[0]) || !(65..=90).contains(&key[1]) {
        return decode_error("musepack: invalid block key");
    }
    let mut raw_size: u64 = 0;
    let mut n: u64 = 2;
    for _ in 0..10 {
        let b = mss.read_byte().map_err(symphonia_core::errors::Error::IoError)?;
        raw_size = (raw_size << 7) | u64::from(b & 0x7F);
        n += 1;
        if b & 0x80 == 0 {
            break;
        }
    }
    let header_len = n;
    let payload_size = raw_size.saturating_sub(header_len);
    Ok((key, payload_size))
}

/// Ported from libmpcdec `streaminfo.c` (`streaminfo_read_header_sv8`), skipping the CRC check
/// (malformed streams are handled defensively rather than rejected outright, matching this
/// crate's "never panic, degrade gracefully" policy for network sources).
fn parse_sh(payload: &[u8]) -> Result<(u32, u64, u64, u32, i32, u32, bool, u8)> {
    let mut r = BitReader::new(payload);
    let _crc = r.read_bits(32);
    let stream_version = r.read_bits(8);
    if stream_version != 8 {
        return decode_error("musepack: unsupported SV8 stream_version");
    }
    let (samples, _) = r.read_size();
    let (beg_silence, _) = r.read_size();
    if beg_silence > samples {
        return decode_error("musepack: beg_silence exceeds samples");
    }
    let freq_idx = r.read_bits(3) as usize;
    let sample_rate = SAMPLE_FREQS.get(freq_idx).copied().unwrap_or(0);
    if sample_rate == 0 {
        return decode_error("musepack: invalid sample rate index");
    }
    let max_band = r.read_bits(5) as i32 + 1;
    let channels = r.read_bits(4) + 1;
    let ms = r.read_bit() != 0;
    let block_pwr = (r.read_bits(3) * 2) as u8;

    Ok((stream_version, samples, beg_silence, sample_rate, max_band, channels, ms, block_pwr))
}

/// Ported from libmpcdec `streaminfo.c` (`streaminfo_gain`).
fn parse_rg(payload: &[u8]) -> Option<(u16, u16, u16, u16)> {
    let mut r = BitReader::new(payload);
    let version = r.read_bits(8);
    if version != 1 {
        return None;
    }
    let gain_title = r.read_bits(16) as u16;
    let peak_title = r.read_bits(16) as u16;
    let gain_album = r.read_bits(16) as u16;
    let peak_album = r.read_bits(16) as u16;
    Some((gain_title, peak_title, gain_album, peak_album))
}

/// Ported from libmpcdec `streaminfo.c` (`streaminfo_encoder_info`/`mpc_get_encoder_string`).
fn parse_ei(payload: &[u8]) -> Option<String> {
    let mut r = BitReader::new(payload);
    let _profile = r.read_bits(7);
    let _pns = r.read_bit();
    let major = r.read_bits(8);
    let minor = r.read_bits(8);
    let build = r.read_bits(8);
    let tag = if minor & 1 != 0 { "Unstable" } else { "Stable" };
    Some(format!("{tag} {major}.{minor}.{build}"))
}

/// The `EI` block's "noise substitution used" flag.
fn ei_pns(payload: &[u8]) -> bool {
    let mut r = BitReader::new(payload);
    let _profile = r.read_bits(7);
    r.read_bit() != 0
}

pub(crate) struct Sv8State {
    /// Byte offset of the first `AP`/`SE` block, i.e. where linear packet scanning restarts.
    data_start: u64,
    /// Parsed `ST` seek table, if one was found and passed sanity checks. `None` means seeking
    /// always falls back to the linear block-size scan.
    seek_table: Option<super::seek_table::SeekTable>,
    /// Set if the stream may contain noise substitution, see `noise_state_at`.
    noise_cfg: Option<NoiseCfg>,
    /// Lazily built by the first seek: the noise generator state on entry to every packet.
    noise_index: Option<Vec<[u32; 2]>>,
    /// Noise generator state to attach to the next packet (set by a seek).
    pending_noise: Option<[u32; 2]>,
}

pub(crate) fn read_header(mss: &mut MediaSourceStream<'_>) -> Result<(StreamInfo, Sv8State)> {
    let mut info = StreamInfo {
        stream_version: 8,
        sample_rate: 0,
        channels: 0,
        max_band: -1,
        ms: false,
        block_pwr: 0,
        decoder_samples: 0,
        beg_silence: 0,
        display_samples: 0,
        total_frames: None,
        gain_title: 0,
        peak_title: 0,
        gain_album: 0,
        peak_album: 0,
        encoder: None,
    };
    let mut samples = 0u64;
    let mut beg_silence = 0u64;
    let mut sh_seen = false;
    let mut pns = true;
    let mut seek_table: Option<super::seek_table::SeekTable> = None;
    // `si.header_position`: byte offset of the "MPCK" magic. This crate does not (yet) skip a
    // leading ID3v2 tag before probing, so the magic is always at offset 0 in practice.
    let header_position: u64 = 0;

    loop {
        let block_start = mss.pos();
        let (key, payload_size) = read_block_header(mss)?;

        if &key == b"AP" || &key == b"SE" {
            if !sh_seen {
                return decode_error("musepack: missing SH block before audio");
            }
            // Leave the stream positioned at the start of this block for the packet loop.
            mss.seek(std::io::SeekFrom::Start(block_start))
                .map_err(symphonia_core::errors::Error::IoError)?;
            info.decoder_samples = samples;
            info.beg_silence = beg_silence;
            info.display_samples = samples.saturating_sub(beg_silence);
            // The decoder's output lags its input by `SYNTH_DELAY`, so `samples` of output
            // (before `beg_silence`) need that many extra decoded samples. A zero sample count
            // means the length is unknown (the stream is then played until its `SE` block).
            info.total_frames = (samples != 0)
                .then(|| (samples + u64::from(SYNTH_DELAY)).div_ceil(FRAME_LENGTH as u64));
            // Without an `EI` block the encoder is unknown, so assume noise substitution may be
            // in use.
            let noise_cfg = pns.then(|| NoiseCfg {
                max_band: info.max_band,
                ms: info.ms,
                channels: info.channels,
                block_frames: info.block_frames(),
                total_frames: info.total_frames,
            });
            return Ok((
                info,
                Sv8State {
                    data_start: block_start,
                    seek_table,
                    noise_cfg,
                    noise_index: None,
                    pending_noise: None,
                },
            ));
        }

        if payload_size > 16 * 1024 * 1024 {
            return decode_error("musepack: header block implausibly large");
        }
        let mut payload = vec![0u8; payload_size as usize];
        mss.read_buf_exact(&mut payload).map_err(symphonia_core::errors::Error::IoError)?;

        match &key {
            b"SH" => {
                let (sv, s, bs, sr, mb, ch, ms, bp) = parse_sh(&payload)?;
                info.stream_version = sv;
                samples = s;
                beg_silence = bs;
                info.sample_rate = sr;
                info.max_band = mb;
                info.channels = ch;
                info.ms = ms;
                info.block_pwr = bp;
                sh_seen = true;
            }
            b"RG" => {
                if let Some((gt, pt, ga, pa)) = parse_rg(&payload) {
                    info.gain_title = gt;
                    info.peak_title = pt;
                    info.gain_album = ga;
                    info.peak_album = pa;
                }
            }
            b"EI" => {
                info.encoder = parse_ei(&payload);
                pns = ei_pns(&payload);
            }
            b"SO" if mss.is_seekable() && seek_table.is_none() => {
                let resume_pos = mss.pos();
                if let Some(st) = try_parse_seek_table(mss, &payload, block_start, header_position, &info)
                {
                    seek_table = Some(st);
                }
                // Whether or not this succeeded, resume normal header scanning where we left
                // off (right after the SO block's own payload).
                mss.seek(std::io::SeekFrom::Start(resume_pos))
                    .map_err(symphonia_core::errors::Error::IoError)?;
            }
            // "ST" appearing directly in the header loop (rather than via an "SO" pointer) is
            // unexpected in a well-formed stream; skip it like any other unrecognized block.
            _ => {}
        }
    }
}

/// Ported from `mpc_demux_SP`: follows an `SO` block's pointer to the `ST` block and parses it.
/// Returns `None` (falls back to linear-scan seeking) on any parse failure or failed sanity
/// check.
fn try_parse_seek_table(
    mss: &mut MediaSourceStream<'_>,
    so_payload: &[u8],
    so_block_start: u64,
    header_position: u64,
    info: &StreamInfo,
) -> Option<super::seek_table::SeekTable> {
    let mut r = BitReader::new(so_payload);
    let (ptr, _) = r.read_size();
    // Derived (see `demuxer::seek_table` module docs / the crate's development notes): the
    // reference's `((ptr - size) << 3) + cur` bit-position arithmetic, with `cur` = the SO
    // block's payload start and `size` = the SO block's own header length, algebraically
    // simplifies to `so_block_start + ptr` bytes.
    let target = so_block_start.checked_add(ptr)?;

    mss.seek(std::io::SeekFrom::Start(target)).ok()?;
    let (key, st_size) = read_block_header(mss).ok()?;
    if &key != b"ST" || st_size > 4 * 1024 * 1024 {
        return None;
    }
    let mut st_payload = vec![0u8; st_size as usize];
    mss.read_buf_exact(&mut st_payload).ok()?;

    let table = super::seek_table::SeekTable::parse(
        &st_payload,
        header_position,
        info.block_pwr,
        info.decoder_samples.max(info.display_samples),
    )?;

    let max_bit_pos = mss.byte_len().map(|len| len.saturating_mul(8)).unwrap_or(u64::MAX);
    if !table.sanity_check(max_bit_pos) {
        return None;
    }
    Some(table)
}

impl Sv8State {
    /// Ported from libmpcdec `mpc_demux_decode_inner`'s SV8 branch (block scanning only; actual
    /// bitstream decode happens in `decoder_core::Decoder::decode_frame`, once per packet, in
    /// the `AudioDecoder`).
    pub fn next_packet(&mut self, mss: &mut MediaSourceStream<'_>) -> Result<Option<Vec<u8>>> {
        loop {
            let (key, payload_size) = read_block_header(mss)?;
            if &key == b"SE" {
                return Ok(None);
            }
            if &key == b"AP" {
                if payload_size > 32 * 1024 * 1024 {
                    return decode_error("musepack: audio block implausibly large");
                }
                // Payload prefixed with a tag byte (and, after a seek, the noise generator state;
                // see `noise_state_at`). `crate::decoder::MpcDecoder` is the consumer.
                let mut data = Vec::with_capacity(1 + 8 + payload_size as usize);
                match self.pending_noise.take() {
                    Some([r1, r2]) => {
                        data.push(PACKET_TAG_NOISE);
                        data.extend_from_slice(&r1.to_le_bytes());
                        data.extend_from_slice(&r2.to_le_bytes());
                    }
                    None => data.push(PACKET_TAG_PLAIN),
                }
                let start = data.len();
                data.resize(start + payload_size as usize, 0);
                mss.read_buf_exact(&mut data[start..])
                    .map_err(symphonia_core::errors::Error::IoError)?;
                return Ok(Some(data));
            }
            // Unexpected block (e.g. a chapter block) between audio packets: skip it.
            mss.ignore_bytes(payload_size).map_err(symphonia_core::errors::Error::IoError)?;
        }
    }

    /// Positions the reader at the start of the `block`th `AP` packet (0-based).
    ///
    /// If a trustworthy `ST` seek table is present it is used to jump close to the target; the
    /// final approach (and the whole search when there is no table, or when the table entry does
    /// not point at an `AP` block) is a scan over block headers only, without reading payloads.
    /// Returns `OutOfRange` if the stream has fewer than `block + 1` packets.
    pub fn seek_block(
        &mut self,
        mss: &mut MediaSourceStream<'_>,
        block: u64,
        block_pwr: u8,
    ) -> Result<()> {
        if !mss.is_seekable() {
            return seek_error(SeekErrorKind::Unseekable);
        }

        // Each seek table entry is the position of the packet starting every `2^seek_pwr` frames;
        // `seek_pwr >= block_pwr`, so entries always sit on packet boundaries.
        let mut start_pos = self.data_start;
        let mut scanned = 0u64;
        let mut jumped = false;
        if let Some(st) = &self.seek_table {
            let blocks_per_entry = st.spacing_frames() >> block_pwr.min(31);
            if blocks_per_entry > 0 && !st.entries.is_empty() {
                let idx = ((block / blocks_per_entry) as usize).min(st.entries.len() - 1);
                let bitpos = st.entries[idx];
                if bitpos % 8 == 0 && bitpos / 8 >= self.data_start {
                    start_pos = bitpos / 8;
                    scanned = idx as u64 * blocks_per_entry;
                    jumped = true;
                }
            }
        }

        // Recover the noise generator state first: it needs a pass over the whole stream, which
        // moves the reader.
        let noise = self.noise_state_at(mss, block);

        loop {
            mss.seek(std::io::SeekFrom::Start(start_pos))
                .map_err(symphonia_core::errors::Error::IoError)?;
            match scan_to_block(mss, scanned, block, jumped) {
                Some(Ok(())) => {
                    self.pending_noise = noise;
                    return Ok(());
                }
                Some(Err(e)) => return Err(e),
                // The table entry did not point at an `AP` block: distrust the table and rescan
                // from the start of the audio data.
                None => {
                    start_pos = self.data_start;
                    scanned = 0;
                    jumped = false;
                    self.seek_table = None;
                }
            }
        }
    }

    /// The noise-substitution generator state on entry to packet `block`, if it matters.
    ///
    /// Every SV8 packet starts with a key frame and so decodes independently of its
    /// predecessors, except for the pseudo-random generator behind `Res == -1` (noise
    /// substitution), which runs on across packets. Streams that cannot contain noise
    /// substitution (the `EI` block says the encoder's PNS was off) skip this; for all others the
    /// first seek parses (but does not synthesize) every packet once to record the state at each.
    fn noise_state_at(
        &mut self,
        mss: &mut MediaSourceStream<'_>,
        block: u64,
    ) -> Option<[u32; 2]> {
        let cfg = self.noise_cfg.as_ref()?;
        if self.noise_index.is_none() {
            self.noise_index = Some(build_noise_index(mss, self.data_start, cfg));
        }
        self.noise_index.as_ref()?.get(block as usize).copied()
    }
}

/// What [`build_noise_index`] needs to parse frames.
pub(crate) struct NoiseCfg {
    max_band: i32,
    ms: bool,
    channels: u32,
    block_frames: u64,
    total_frames: Option<u64>,
}

/// Parses every `AP` packet from `data_start` on and returns the noise generator state on entry
/// to each. Stops quietly at the end of the stream or at the first unreadable block, so a
/// damaged tail only loses the states of the packets past it.
fn build_noise_index(
    mss: &mut MediaSourceStream<'_>,
    data_start: u64,
    cfg: &NoiseCfg,
) -> Vec<[u32; 2]> {
    let mut states = Vec::new();
    if mss.seek(std::io::SeekFrom::Start(data_start)).is_err() {
        return states;
    }
    let mut parser = Core::new(8, cfg.max_band, cfg.ms, cfg.channels);
    loop {
        let Ok((key, size)) = read_block_header(mss)
        else {
            break;
        };
        if &key == b"SE" {
            break;
        }
        if &key != b"AP" {
            if mss.ignore_bytes(size).is_err() {
                break;
            }
            continue;
        }
        let mut payload = vec![0u8; size.min(32 * 1024 * 1024) as usize];
        if mss.read_buf_exact(&mut payload).is_err() {
            break;
        }

        let first_frame = states.len() as u64 * cfg.block_frames;
        let frames = match cfg.total_frames {
            Some(total) => cfg.block_frames.min(total.saturating_sub(first_frame)),
            None => cfg.block_frames,
        };
        states.push(parser.noise_state());
        let mut r = BitReader::new(&payload);
        for i in 0..frames {
            parser.skip_frame_sv8(&mut r, i == 0);
        }
    }
    states
}

/// Scans block headers from the current position, where block number `scanned` starts, until the
/// start of `AP` block number `target`, leaving the reader positioned there.
///
/// `Some(Err(OutOfRange))` if the stream ends first. `None` if `checked` and the first block is
/// not an `AP` block (i.e. the position was an untrustworthy seek table entry).
fn scan_to_block(
    mss: &mut MediaSourceStream<'_>,
    mut scanned: u64,
    target: u64,
    checked: bool,
) -> Option<Result<()>> {
    let mut first = true;
    loop {
        let block_start = mss.pos();
        let header = read_block_header(mss);
        let (key, payload_size) = match header {
            Ok(v) => v,
            Err(_) if checked && first => return None,
            Err(_) => return Some(seek_error(SeekErrorKind::OutOfRange)),
        };
        if checked && first && &key != b"AP" {
            return None;
        }
        first = false;

        if &key == b"SE" {
            return Some(seek_error(SeekErrorKind::OutOfRange));
        }
        if &key == b"AP" {
            if scanned == target {
                return Some(
                    mss.seek(std::io::SeekFrom::Start(block_start))
                        .map(|_| ())
                        .map_err(symphonia_core::errors::Error::IoError),
                );
            }
            scanned += 1;
        }
        if let Err(e) = mss.ignore_bytes(payload_size) {
            return Some(Err(symphonia_core::errors::Error::IoError(e)));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::demuxer::seek_table::SeekTable;

    const FIXTURE: &[u8] = include_bytes!("../../tests/fixture_stereo_q5.mpc");

    fn open() -> (MediaSourceStream<'static>, StreamInfo, Sv8State) {
        let mut mss =
            MediaSourceStream::new(Box::new(Cursor::new(FIXTURE.to_vec())), Default::default());
        assert_eq!(&mss.read_quad_bytes().unwrap(), b"MPCK");
        let (info, state) = read_header(&mut mss).unwrap();
        (mss, info, state)
    }

    /// Byte position of every `AP` block, found by walking the block headers.
    fn ap_positions(mss: &mut MediaSourceStream<'_>, state: &Sv8State) -> Vec<u64> {
        mss.seek(std::io::SeekFrom::Start(state.data_start)).unwrap();
        let mut positions = Vec::new();
        loop {
            let pos = mss.pos();
            let (key, size) = read_block_header(mss).unwrap();
            if &key == b"SE" {
                return positions;
            }
            if &key == b"AP" {
                positions.push(pos);
            }
            mss.ignore_bytes(size).unwrap();
        }
    }

    #[test]
    fn seek_block_finds_every_packet_with_and_without_a_table() {
        let (mut mss, info, mut state) = open();
        let positions = ap_positions(&mut mss, &state);
        assert!(!positions.is_empty());

        let check = |mss: &mut MediaSourceStream<'_>, state: &mut Sv8State| {
            // Out of order, to exercise backwards and forwards jumps.
            for b in (0..positions.len()).rev().chain(0..positions.len()) {
                state.seek_block(mss, b as u64, info.block_pwr).unwrap();
                assert_eq!(mss.pos(), positions[b], "block {b}");
            }
            let past = positions.len() as u64;
            assert!(matches!(
                state.seek_block(mss, past, info.block_pwr),
                Err(symphonia_core::errors::Error::SeekError(SeekErrorKind::OutOfRange))
            ));
        };

        // No table: linear scan.
        check(&mut mss, &mut state);

        // A correct table, one entry per packet.
        let entries = positions.iter().map(|p| p * 8).collect();
        state.seek_table = Some(SeekTable { entries, seek_pwr: u32::from(info.block_pwr) });
        check(&mut mss, &mut state);
        assert!(state.seek_table.is_some(), "a valid table is kept");

        // A table pointing into the middle of a block is distrusted, not followed.
        let entries = positions.iter().map(|p| (p + 3) * 8).collect();
        state.seek_table = Some(SeekTable { entries, seek_pwr: u32::from(info.block_pwr) });
        check(&mut mss, &mut state);
    }
}
