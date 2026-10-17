// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! End-to-end tests of the Nintendo (ADP/DTK, AFC, AST) and Sony (VAG family) readers.
//!
//! The decoders are integer decoders. Synthetic streams of random, valid frames are built here and
//! the decoded samples are compared bit for bit with FFmpeg's demuxer of the same format. The
//! tests are skipped when `ffmpeg` is not on the `PATH`.

mod common;

use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_format_game::{AdpReader, AfcReader, AstReader, BrstmReader, VagReader};
use symphonia_testutil::XorShift64;

use common::{decode, ffmpeg_decode, mss, to_s16le};

fn random_bytes(rng: &mut XorShift64, n: usize) -> Vec<u8> {
    (0..n).map(|_| (rng.next_u64() >> 24) as u8).collect()
}

/// Random PlayStation ADPCM frames: a valid predictor and shift, a flag of 0, random nibbles.
fn psx_frames(rng: &mut XorShift64, frames: usize) -> Vec<u8> {
    let mut v = Vec::new();
    for _ in 0..frames {
        let mut f = random_bytes(rng, 16);
        f[0] = (((rng.next_u64() >> 20) % 5) as u8) << 4 | ((rng.next_u64() >> 20) % 13) as u8;
        f[1] = 0;
        v.extend_from_slice(&f);
    }
    v
}

fn check(name: &str, data: &[u8], format: &str, mut reader: Box<dyn FormatReader + '_>) {
    let got = decode(reader.as_mut(), false);
    let Some(want) = ffmpeg_decode(data, format, "s16le")
    else {
        eprintln!("{name}: ffmpeg not available, skipping");
        return;
    };
    let got = to_s16le(&got.samples);
    assert!(!got.is_empty(), "{name}: no samples");
    assert_eq!(got.len(), want.len(), "{name}: length");
    assert!(got == want, "{name}: samples differ from FFmpeg");
}

#[test]
fn adp_matches_ffmpeg() {
    let mut rng = XorShift64(7);
    let mut data = Vec::new();
    for _ in 0..200 {
        let mut b = random_bytes(&mut rng, 32);
        // The header bytes are repeated, with valid shifts (the high nibble of the range).
        b[0] &= 0x0f;
        b[1] &= 0x0f;
        b[2] = b[0];
        b[3] = b[1];
        data.extend_from_slice(&b);
    }
    let reader = AdpReader::try_new(mss(&data), FormatOptions::default()).unwrap();
    check("adp", &data, "adp", Box::new(reader));
}

#[test]
fn afc_matches_ffmpeg() {
    let mut rng = XorShift64(11);
    let groups = 300usize;
    let mut data = vec![0u8; 32];
    data[0..4].copy_from_slice(&((groups * 18) as u32).to_be_bytes());
    data[4..8].copy_from_slice(&((groups * 16) as u32).to_be_bytes());
    data[8..10].copy_from_slice(&32000u16.to_be_bytes());
    data.extend_from_slice(&random_bytes(&mut rng, groups * 18));
    let reader = AfcReader::try_new(mss(&data), FormatOptions::default()).unwrap();
    check("afc", &data, "afc", Box::new(reader));
}

#[test]
fn ast_matches_ffmpeg() {
    let mut rng = XorShift64(13);
    let (channels, per_channel, blocks) = (2usize, 9 * 40, 3usize);
    let mut data = vec![0u8; 64];
    data[0..4].copy_from_slice(b"STRM");
    data[8..10].copy_from_slice(&0u16.to_be_bytes());
    data[10..12].copy_from_slice(&16u16.to_be_bytes());
    data[12..14].copy_from_slice(&(channels as u16).to_be_bytes());
    data[16..20].copy_from_slice(&32000u32.to_be_bytes());
    data[20..24].copy_from_slice(&((blocks * 40 * 16) as u32).to_be_bytes());
    data[32..36].copy_from_slice(&(per_channel as u32).to_be_bytes());
    for _ in 0..blocks {
        let mut h = vec![0u8; 32];
        h[0..4].copy_from_slice(b"BLCK");
        h[4..8].copy_from_slice(&(per_channel as u32).to_be_bytes());
        data.extend_from_slice(&h);
        data.extend_from_slice(&random_bytes(&mut rng, per_channel * channels));
    }
    let reader = AstReader::try_new(mss(&data), FormatOptions::default()).unwrap();
    check("ast", &data, "ast", Box::new(reader));
}

#[test]
fn vag_matches_ffmpeg() {
    let mut rng = XorShift64(17);
    let frames = 150usize;
    let mut data = vec![0u8; 0x30];
    data[0..4].copy_from_slice(b"VAGp");
    data[4..8].copy_from_slice(&3u32.to_be_bytes());
    data[12..16].copy_from_slice(&((frames * 16) as u32).to_be_bytes());
    data[16..20].copy_from_slice(&22050u32.to_be_bytes());
    data.extend_from_slice(&psx_frames(&mut rng, frames));
    let reader = VagReader::try_new(mss(&data), FormatOptions::default()).unwrap();
    check("vag", &data, "vag", Box::new(reader));
}

#[test]
fn svag_matches_ffmpeg() {
    let mut rng = XorShift64(19);
    let (frames, align) = (64usize, 16u32);
    let mut data = vec![0u8; 0x800];
    data[0..4].copy_from_slice(b"Svag");
    data[4..8].copy_from_slice(&((frames * 16 * 2) as u32).to_le_bytes());
    data[8..12].copy_from_slice(&44100u32.to_le_bytes());
    data[12..16].copy_from_slice(&2u32.to_le_bytes());
    data[16..20].copy_from_slice(&align.to_le_bytes());
    data.extend_from_slice(&psx_frames(&mut rng, frames * 2));
    let reader = VagReader::try_new(mss(&data), FormatOptions::default()).unwrap();
    check("svag", &data, "svag", Box::new(reader));
}

#[test]
fn msf_psx_matches_ffmpeg() {
    let mut rng = XorShift64(23);
    let frames = 100usize;
    let mut data = vec![0u8; 0x40];
    data[0..4].copy_from_slice(b"MSF\x01");
    data[4..8].copy_from_slice(&3u32.to_be_bytes());
    data[8..12].copy_from_slice(&1u32.to_be_bytes());
    data[12..16].copy_from_slice(&((frames * 16) as u32).to_be_bytes());
    data[16..20].copy_from_slice(&44100u32.to_be_bytes());
    data.extend_from_slice(&psx_frames(&mut rng, frames));
    let reader = VagReader::try_new(mss(&data), FormatOptions::default()).unwrap();
    check("msf", &data, "msf", Box::new(reader));
}

/// The parameters of a synthetic BRSTM, BFSTM or BCSTM stream.
struct Stream {
    /// "RSTM", "FSTM" or "CSTM".
    magic: &'static [u8; 4],
    little_endian: bool,
    /// 0: PCM 8-bit, 1: PCM 16-bit, 2: DSP-ADPCM.
    codec: u8,
    channels: usize,
    rate: u32,
    blocks: usize,
    /// The samples in a block, and the bytes of a block per channel.
    samples_per_block: usize,
    block_size: usize,
    last_samples: usize,
    last_used: usize,
    last_size: usize,
}

fn dsp_len(samples: usize) -> usize {
    8 * (samples / 14) + if samples % 14 == 0 { 0 } else { 1 + (samples % 14).div_ceil(2) }
}

fn put16(v: &mut Vec<u8>, le: bool, x: u16) {
    v.extend_from_slice(&if le { x.to_le_bytes() } else { x.to_be_bytes() });
}

fn put32(v: &mut Vec<u8>, le: bool, x: u32) {
    v.extend_from_slice(&if le { x.to_le_bytes() } else { x.to_be_bytes() });
}

fn set32(v: &mut [u8], le: bool, at: usize, x: u32) {
    v[at..at + 4].copy_from_slice(&if le { x.to_le_bytes() } else { x.to_be_bytes() });
}

/// Builds the stream, as laid out by the demuxer of FFmpeg.
fn build_stream(s: &Stream, seed: u64) -> Vec<u8> {
    let mut rng = XorShift64(seed);
    let le = s.little_endian;
    let bfstm = s.magic != b"RSTM";
    let ch = s.channels;
    let dsp = s.codec == 2;
    let total = (s.blocks - 1) * s.samples_per_block + s.last_samples;

    let mut f = vec![0u8; 0x40];
    f[..4].copy_from_slice(s.magic);
    f[4..6].copy_from_slice(&if le { [0xff, 0xfe] } else { [0xfe, 0xff] });
    let pos = 0x40usize;

    // The header block: fields at `pos + 0x28`, the tables after.
    let mut head = vec![0u8; 0x200];
    head[..4].copy_from_slice(if bfstm { b"INFO" } else { b"HEAD" });
    set32(&mut head, le, 4, 0x200);
    set32(&mut head, le, 12, 0x20);

    let mut fields = vec![s.codec, 0, ch as u8, 0];
    if bfstm {
        put32(&mut fields, le, s.rate);
    }
    else {
        put16(&mut fields, le, s.rate as u16);
        put16(&mut fields, le, 0);
    }
    put32(&mut fields, le, 0);
    put32(&mut fields, le, total as u32);
    let start_field = fields.len();
    if !bfstm {
        put32(&mut fields, le, 0);
    }
    for v in [s.blocks, s.block_size, s.samples_per_block, s.last_used, s.last_samples, s.last_size]
    {
        put32(&mut fields, le, v as u32);
    }
    head[0x28..0x28 + fields.len()].copy_from_slice(&fields);

    let coeffs: Vec<Vec<u16>> = (0..ch)
        .map(|_| (0..16).map(|_| (rng.next_u64() >> 40) as u16 & 0x3fff).collect())
        .collect();

    if dsp {
        if !bfstm {
            // The offset of the channel information, then the table of the offsets.
            set32(&mut head, le, 28, 0x60 - 16);
            set32(&mut head, le, 0x60, 0x70);
            for c in 0..ch {
                set32(&mut head, le, 0x68 + 8 * c + 4, (0x80 + 0x20 * c) as u32);
            }
            // `toffset` is 0x80, so the table of the offsets ends there.
            for (c, table) in coeffs.iter().enumerate() {
                for (i, &x) in table.iter().enumerate() {
                    let at = 0x90 + 0x20 * c + 2 * i;
                    head[at..at + 2].copy_from_slice(&if le {
                        x.to_le_bytes()
                    }
                    else {
                        x.to_be_bytes()
                    });
                }
            }
        }
        else {
            set32(&mut head, le, 28, 0x60 - 16);
            // `toffset = 0x60 + value + channels * 8 - 8`: the tables at 0x80.
            let value = 0x80 - 0x60 - 8 * ch + 8;
            set32(&mut head, le, 0x60, value as u32);
            for (c, table) in coeffs.iter().enumerate() {
                for (i, &x) in table.iter().enumerate() {
                    let at = 0x80 + 46 * c + 2 * i;
                    head[at..at + 2].copy_from_slice(&if le {
                        x.to_le_bytes()
                    }
                    else {
                        x.to_be_bytes()
                    });
                }
            }
        }
    }

    // The history table of the blocks.
    let mut seek = Vec::new();
    if dsp {
        let n = s.blocks * ch * 4;
        seek = random_bytes(&mut rng, n);
    }

    let mut data = Vec::new();
    for b in 0..s.blocks {
        let last = b + 1 == s.blocks;
        let (size, used, samples) = if last {
            (s.last_size, s.last_used, s.last_samples)
        }
        else {
            (s.block_size, s.block_size, s.samples_per_block)
        };
        for _ in 0..ch {
            let mut block = random_bytes(&mut rng, size);
            if dsp {
                // Valid frame headers: the exponent below 12.
                let mut i = 0;
                while i < used.min(dsp_len(samples)) {
                    block[i] &= 0x7c | 0x03;
                    block[i] = (block[i] & 0x70) | (block[i] & 0x0f).min(11);
                    i += 8;
                }
            }
            data.extend_from_slice(&block);
        }
    }

    if !bfstm {
        f[6] = 1;
        f[7] = 0;
        f[12..14].copy_from_slice(&if le { 0x40u16.to_le_bytes() } else { 0x40u16.to_be_bytes() });
        f.extend_from_slice(&head);
        if dsp {
            f.extend_from_slice(b"ADPC");
            put32(&mut f, le, 8 + seek.len() as u32);
            f.extend_from_slice(&seek);
        }
        f.extend_from_slice(b"DATA");
        let chunk = f.len() - 4;
        put32(&mut f, le, 0);
        while f.len() % 0x20 != 0 {
            f.push(0);
        }
        let start = f.len();
        set32(&mut f, le, chunk + 4, (start - chunk) as u32);
        f.extend_from_slice(&data);
        // The start of the data, in the HEAD chunk.
        set32(&mut f, le, pos + 0x28 + start_field, start as u32);
    }
    else {
        // The header, then INFO, SEEK and DATA.
        f[6..8].copy_from_slice(&if le { 0x40u16.to_le_bytes() } else { 0x40u16.to_be_bytes() });
        let section_count = 2u16;
        f[16..18].copy_from_slice(&if le {
            section_count.to_le_bytes()
        }
        else {
            section_count.to_be_bytes()
        });
        f.extend_from_slice(&head);
        let seek_chunk = f.len();
        if dsp {
            f.extend_from_slice(b"SEEK");
            put32(&mut f, le, 8 + seek.len() as u32);
            f.extend_from_slice(&seek);
        }
        let data_chunk = f.len();
        f.extend_from_slice(b"DATA");
        put32(&mut f, le, 32 + data.len() as u32);
        // The 24 bytes that FFmpeg skips in a DSP-ADPCM stream.
        f.extend_from_slice(&[0u8; 24]);
        f.extend_from_slice(&data);
        // The sections: INFO, DATA.
        let mut sec = Vec::new();
        put16(&mut sec, le, 0x4000);
        put16(&mut sec, le, 0);
        put32(&mut sec, le, pos as u32);
        put32(&mut sec, le, 0x200);
        put16(&mut sec, le, 0x4002);
        put16(&mut sec, le, 0);
        put32(&mut sec, le, data_chunk as u32);
        put32(&mut sec, le, 0);
        f[20..20 + sec.len()].copy_from_slice(&sec);
        let _ = seek_chunk;
    }

    f
}

fn check_stream(name: &str, s: &Stream, format: &str, seed: u64) {
    let data = build_stream(s, seed);
    let mut reader = BrstmReader::try_new(mss(&data), FormatOptions::default()).unwrap();

    if s.codec == 2 {
        check(name, &data, format, Box::new(reader));
        return;
    }

    // FFmpeg applies the sample count of the last block to its bytes, which cuts the PCM short:
    // only the whole blocks are compared, and the last one must hold all its samples.
    let Some(want) = ffmpeg_decode(&data, format, "s16le").filter(|_| s.codec == 1)
    else {
        return;
    };
    let got = to_s16le(&decode(&mut reader, false).samples);
    let whole = (s.blocks - 1) * s.samples_per_block * 2;
    assert_eq!(got.len(), (whole + 2 * s.last_samples) * s.channels, "{name}: length");
    assert!(got[..whole] == want[..whole], "{name}: samples differ from FFmpeg");
}

#[test]
fn brstm_dsp_matches_ffmpeg() {
    for (name, ch, le) in [("brstm dsp stereo", 2, false), ("brstm dsp mono le", 1, true)] {
        let s = Stream {
            magic: b"RSTM",
            little_endian: le,
            codec: 2,
            channels: ch,
            rate: 32000,
            blocks: 3,
            samples_per_block: 14 * 16,
            block_size: 8 * 16,
            last_samples: 14 * 5 + 6,
            last_used: dsp_len(14 * 5 + 6),
            last_size: 8 * 8,
        };
        check_stream(name, &s, "brstm", 31);
    }
}

#[test]
fn brstm_pcm16_matches_ffmpeg() {
    for (name, le) in [("brstm pcm16 be", false), ("brstm pcm16 le", true)] {
        let s = Stream {
            magic: b"RSTM",
            little_endian: le,
            codec: 1,
            channels: 1,
            rate: 22050,
            blocks: 3,
            samples_per_block: 100,
            block_size: 200,
            last_samples: 37,
            last_used: 74,
            last_size: 80,
        };
        check_stream(name, &s, "brstm", 37);
    }
}

#[test]
fn bfstm_dsp_matches_ffmpeg() {
    for (name, magic, ch, le) in [
        ("bfstm dsp be", b"FSTM", 2, false),
        ("bfstm dsp le", b"FSTM", 2, true),
        ("bcstm dsp le", b"CSTM", 1, true),
    ] {
        let s = Stream {
            magic,
            little_endian: le,
            codec: 2,
            channels: ch,
            rate: 44100,
            blocks: 2,
            samples_per_block: 14 * 20,
            block_size: 8 * 20,
            last_samples: 14 * 7 + 3,
            last_used: dsp_len(14 * 7 + 3),
            last_size: 8 * 9,
        };
        check_stream(name, &s, "bfstm", 41);
    }
}
