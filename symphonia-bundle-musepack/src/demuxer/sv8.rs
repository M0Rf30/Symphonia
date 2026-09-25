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
use crate::decoder_core::FRAME_LENGTH;

use super::StreamInfo;

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

pub(crate) struct Sv8State {
    /// Byte offset of the first `AP`/`SE` block, i.e. where linear packet scanning restarts.
    data_start: u64,
    /// Parsed `ST` seek table, if one was found and passed sanity checks. `None` means seeking
    /// always falls back to the linear block-size scan.
    seek_table: Option<super::seek_table::SeekTable>,
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
        gain_title: 0,
        peak_title: 0,
        gain_album: 0,
        peak_album: 0,
        encoder: None,
    };
    let mut samples = 0u64;
    let mut beg_silence = 0u64;
    let mut sh_seen = false;
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
            return Ok((info, Sv8State { data_start: block_start, seek_table }));
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
                let mut data = vec![0u8; payload_size as usize];
                mss.read_buf_exact(&mut data).map_err(symphonia_core::errors::Error::IoError)?;
                return Ok(Some(data));
            }
            // Unexpected block (e.g. a chapter block) between audio packets: skip it.
            mss.ignore_bytes(payload_size).map_err(symphonia_core::errors::Error::IoError)?;
        }
    }

    /// Linear scan seek: walk `AP` block headers (sizes only, no audio decode) from the start of
    /// the audio data until the running sample count reaches `target_sample`, then reposition
    /// the reader there. Returns the actual sample position sought to (frame-accurate: at or
    /// before `target_sample`).
    pub fn seek(
        &mut self,
        mss: &mut MediaSourceStream<'_>,
        target_sample: u64,
        block_pwr: u8,
    ) -> Result<u64> {
        if !mss.is_seekable() {
            return seek_error(SeekErrorKind::Unseekable);
        }
        let block_samples = (1u64 << block_pwr.min(31)) * FRAME_LENGTH as u64;

        // Fast path: jump close to the target using the `ST` seek table (if present and
        // sane), then fall through to the same verified linear scan to land exactly on an `AP`
        // block boundary. Any inconsistency in the table (bad alignment, seek failure) is
        // ignored and we simply fall back to scanning from the very start.
        let mut start_pos = self.data_start;
        let mut decoded = 0u64;
        if let Some(st) = &self.seek_table {
            let spacing_samples = st.spacing_frames().saturating_mul(FRAME_LENGTH as u64);
            if spacing_samples > 0 && !st.entries.is_empty() {
                let idx = ((target_sample / spacing_samples) as usize).min(st.entries.len() - 1);
                let bitpos = st.entries[idx];
                if bitpos % 8 == 0 {
                    let byte_pos = bitpos / 8;
                    if mss.seek(std::io::SeekFrom::Start(byte_pos)).is_ok() {
                        start_pos = byte_pos;
                        decoded = (idx as u64).saturating_mul(spacing_samples);
                    }
                }
            }
        }

        mss.seek(std::io::SeekFrom::Start(start_pos))
            .map_err(symphonia_core::errors::Error::IoError)?;

        let jumped = start_pos != self.data_start;
        let mut used_fallback = false;

        loop {
            let block_start = mss.pos();
            let (key, payload_size) = match read_block_header(mss) {
                Ok(v) => v,
                Err(e) => {
                    // A seek-table jump landing on garbage is the only way this should happen
                    // this early; restart from the verified start-of-audio position instead of
                    // silently returning a wrong position. Otherwise (not jumped, or already
                    // retried once), this is a normal end-of-stream and `decoded` is the answer.
                    if jumped && !used_fallback {
                        used_fallback = true;
                        decoded = 0;
                        mss.seek(std::io::SeekFrom::Start(self.data_start))
                            .map_err(symphonia_core::errors::Error::IoError)?;
                        continue;
                    }
                    let _ = e;
                    break;
                }
            };
            if &key == b"SE" {
                break;
            }
            if &key == b"AP" {
                if decoded + block_samples > target_sample {
                    mss.seek(std::io::SeekFrom::Start(block_start))
                        .map_err(symphonia_core::errors::Error::IoError)?;
                    return Ok(decoded);
                }
                decoded += block_samples;
                mss.ignore_bytes(payload_size).map_err(symphonia_core::errors::Error::IoError)?;
            }
            else {
                mss.ignore_bytes(payload_size).map_err(symphonia_core::errors::Error::IoError)?;
            }
        }
        // Target beyond end of stream: seek to the last known block boundary.
        Ok(decoded)
    }
}
