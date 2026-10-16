// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(clippy::unwrap_used)]

//! Differential test of the decoder against FFmpeg's `ralf` decoder, on synthetic streams.
//!
//! The only real RALF file available is stereo and uses few of the coding modes, so the streams
//! are made of random blocks. Any bit string is a valid block as long as it is long enough (all
//! code tables are complete), and random bits exercise every mode of the codec: mono, all the
//! stereo modes, all block sizes, filters of all lengths and precisions, no filter, raw samples,
//! short and long codes, and the escape codes, with the arithmetic wrapping all the time.
//!
//! Blocks are independent, so the random blocks which decode are assembled into packets: with one
//! or many blocks, with a block that does not decode, in two halves, or too long for the frame.
//! The packets are written to a RealMedia file, which FFmpeg decodes. The test is skipped if the
//! `ffmpeg` executable is not available.
//!
//! Blocks with an invalid Exp-Golomb code are left out: FFmpeg's decoder takes its error code for
//! a value and carries on, while this one rejects the block.

mod common;

use std::process::{Command, Stdio};

use common::*;
use symphonia_testutil::XorShift64;

struct Config {
    name: &'static str,
    channels: u16,
    rate: u32,
    max_frame_size: u32,
}

const CONFIGS: [Config; 3] = [
    Config { name: "stereo", channels: 2, rate: 44100, max_frame_size: 16384 },
    Config { name: "mono", channels: 1, rate: 22050, max_frame_size: 16384 },
    // The frame size is the sample rate, 8000: two blocks of 4096 samples are too many.
    Config { name: "small_frame", channels: 2, rate: 8000, max_frame_size: 4096 },
];

/// The size of the packets of the first half of a split frame.
const HALF: usize = 8192;

impl Config {
    fn extra_data(&self) -> Vec<u8> {
        let mut d = b"LSD:".to_vec();
        d.extend_from_slice(&[1, 3, 0, 0]);
        d.extend_from_slice(&self.channels.to_be_bytes());
        d.extend_from_slice(&16u16.to_be_bytes());
        d.extend_from_slice(&self.rate.to_be_bytes());
        d.extend_from_slice(&self.max_frame_size.to_be_bytes());
        d.extend_from_slice(&[0; 4]);
        d
    }

    /// The largest size of a block, which is coded on 13 bits plus a bit per channel.
    fn max_block_size(&self) -> usize {
        (1 << (13 + self.channels)) - 1
    }
}

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    fn put(&mut self, value: u32, n: u32) {
        for i in (0..n).rev() {
            if self.bits % 8 == 0 {
                self.bytes.push(0);
            }
            if (value >> i) & 1 != 0 {
                *self.bytes.last_mut().unwrap() |= 0x80 >> (self.bits % 8);
            }
            self.bits += 1;
        }
    }
}

/// A block of a frame: its data, and whether its entry of the block table carries a time.
#[derive(Clone)]
struct Block {
    data: Vec<u8>,
    time: Option<u32>,
}

/// The block table of a frame: its size in bits, then the entries.
fn block_table(cfg: &Config, blocks: &[Block]) -> Vec<u8> {
    let mut w = BitWriter::default();

    for block in blocks {
        w.put(block.data.len() as u32, 13 + u32::from(cfg.channels));

        match block.time {
            Some(time) => {
                w.put(1, 1);
                w.put(time, 9);
            }
            None => w.put(0, 1),
        }
    }

    let mut table = (w.bits as u16).to_be_bytes().to_vec();
    table.extend(w.bytes);
    table
}

/// The frame of `blocks`: the block table followed by the blocks.
fn frame(cfg: &Config, blocks: &[Block]) -> Vec<u8> {
    let mut frame = block_table(cfg, blocks);

    for block in blocks {
        frame.extend_from_slice(&block.data);
    }

    frame
}

struct Gen<'a> {
    cfg: &'a Config,
    rng: XorShift64,
    /// Blocks which decode, and their number of samples.
    pool: Vec<(Block, usize)>,
    /// Pending packets in order, with the timestamp of the audio they hold, if known.
    packets: Vec<Vec<u8>>,
}

impl Gen<'_> {
    fn random_bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.rng.next_u64() as u8).collect()
    }

    fn random_time(&mut self) -> Option<u32> {
        (self.rng.below(3) == 0).then(|| self.rng.below(512) as u32)
    }

    /// Fills the pool with random blocks that decode. The size of a candidate is drawn from a
    /// range which is itself random: small blocks are the most likely to be short.
    fn fill_pool(&mut self, count: usize) {
        let mut dec = decoder(&self.cfg.extra_data());
        let max = self.cfg.max_block_size();
        let mut tries = 0;

        while self.pool.len() < count {
            tries += 1;
            assert!(tries < 200_000, "too few random blocks decode");

            let limit = [100, 400, 1500, 6000, 30000][self.rng.below(5)].min(max);
            let size = 8 + self.rng.below(limit - 8);
            let block = Block { data: self.random_bytes(size), time: self.random_time() };
            let packet = frame(self.cfg, std::slice::from_ref(&block));

            // A block of a packet of its own. A block that is not known to be valid in this
            // decoder is never given to FFmpeg's.
            match decode_one(&mut dec, 0, &packet) {
                Some(d) if d.frames > 0 => self.pool.push((block, d.frames)),
                _ => (),
            }
        }
    }

    fn pick(&mut self) -> Block {
        let i = self.rng.below(self.pool.len());
        self.pool[i].0.clone()
    }

    fn random_blocks(&mut self, n: usize) -> Vec<Block> {
        (0..n)
            .map(|_| {
                let mut block = self.pick();
                block.time = self.random_time();
                block
            })
            .collect()
    }

    /// Pads the last block of the frame with random bytes, so that the frame is `len` bytes long.
    /// The extra bytes follow the data of the block, which does not read them.
    fn pad_to(&mut self, blocks: &mut [Block], len: usize) {
        let current = frame(self.cfg, blocks).len();
        assert!(current <= len);

        let pad = self.random_bytes(len - current);
        let last = blocks.last_mut().unwrap();
        last.data.extend(pad);
        assert!(last.data.len() <= self.cfg.max_block_size());
    }

    /// Adds a frame as one packet, or two if it is longer than `HALF`.
    fn add_frame(&mut self, blocks: &[Block]) {
        let frame = frame(self.cfg, blocks);

        if frame.len() <= HALF && frame.len() != HALF {
            self.packets.push(frame);
        }
        else {
            self.packets.push(frame[..HALF].to_vec());
            // The second half repeats the block table, and holds the rest of the frame.
            let table_len = block_table(self.cfg, blocks).len();
            let mut second = frame[..table_len].to_vec();
            second.extend_from_slice(&frame[HALF..]);
            self.packets.push(second);
        }
    }

    /// A frame that needs to be split in two: more than `HALF` bytes, and a second half that fits.
    fn split_frame(&mut self) -> Vec<Block> {
        loop {
            let n = 1 + self.rng.below(5);
            let mut blocks = self.random_blocks(n);
            let len = frame(self.cfg, &blocks).len();

            if len > HALF + 1 && len < 12000 {
                return blocks;
            }

            // Too short or too long. Pad the short ones, if the size of a block permits.
            if len <= HALF + 1 && blocks.iter().map(|b| b.data.len()).sum::<usize>() < 30000 {
                let target = HALF + 2 + self.rng.below(3000);
                if target - len + blocks.last().unwrap().data.len() <= self.cfg.max_block_size() {
                    self.pad_to(&mut blocks, target);
                    return blocks;
                }
            }
        }
    }

    fn generate(&mut self) {
        self.fill_pool(150);

        // One packet per block.
        for i in 0..self.pool.len() {
            let block = self.pool[i].0.clone();
            self.add_frame(&[block]);
        }

        // Packets of many blocks.
        for _ in 0..60 {
            let n = 2 + self.rng.below(6);
            let blocks = self.random_blocks(n);
            if frame(self.cfg, &blocks).len() < HALF {
                self.add_frame(&blocks);
            }
        }

        // A block that cannot decode, after good blocks (the rest of the packet is dropped), or
        // before (no output). It is too short to hold anything.
        for i in 0..20 {
            let count = if i % 2 == 0 { 1 + self.rng.below(3) } else { 0 };
            let mut blocks = self.random_blocks(count);
            let short = self.rng.below(4);
            blocks.push(Block { data: self.random_bytes(short), time: None });
            blocks.extend(self.random_blocks(1));
            if frame(self.cfg, &blocks).len() < HALF {
                self.add_frame(&blocks);
            }
        }

        // Frames in two packets.
        for _ in 0..25 {
            let blocks = self.split_frame();
            self.add_frame(&blocks);
        }

        // A packet of exactly the size of a first half, which is one, and the packet which
        // follows, which does not continue it.
        for _ in 0..4 {
            let count = 1 + self.rng.below(2);
            let mut blocks = self.random_blocks(count);
            let len = frame(self.cfg, &blocks).len();
            if len < HALF
                && blocks.last().unwrap().data.len() + HALF - len <= self.cfg.max_block_size()
            {
                self.pad_to(&mut blocks, HALF);
                self.add_frame(&blocks);
                let other = self.random_blocks(1);
                self.add_frame(&other);
                let other = self.random_blocks(2);
                if frame(self.cfg, &other).len() < HALF {
                    self.add_frame(&other);
                }
            }
        }

        // Garbage.
        for _ in 0..30 {
            let n = 1 + self.rng.below(12);
            let p = self.random_bytes(n);
            self.packets.push(p);
        }

        // Frames with more blocks than the frame can hold, which are cut short.
        if self.cfg.name == "small_frame" {
            for _ in 0..40 {
                let n = 2 + self.rng.below(3);
                let blocks = self.random_blocks(n);
                if frame(self.cfg, &blocks).len() < HALF {
                    self.add_frame(&blocks);
                }
            }
        }
    }
}

/// Decodes `rm` with FFmpeg, and returns the interleaved samples.
fn ffmpeg_decode(name: &str, rm: &[u8]) -> Option<Vec<i16>> {
    let dir = std::env::temp_dir().join(format!("symphonia-ralf-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.rm"));
    std::fs::write(&path, rm).unwrap();

    let out = Command::new("ffmpeg")
        .args(["-v", "quiet", "-nostdin", "-i"])
        .arg(&path)
        .args(["-f", "s16le", "-"])
        .stderr(Stdio::null())
        .output();

    std::fs::remove_dir_all(&dir).ok();

    let out = out.ok()?;
    assert!(out.status.success(), "ffmpeg failed on {name}");

    Some(out.stdout.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect())
}

#[test]
fn matches_ffmpeg_on_random_streams() {
    if Command::new("ffmpeg").arg("-version").stdout(Stdio::null()).status().is_err() {
        eprintln!("ffmpeg not available; skipping");
        return;
    }

    for (n, cfg) in CONFIGS.iter().enumerate() {
        let mut gn = Gen {
            cfg,
            rng: XorShift64(0x9e37_79b9_7f4a_7c15 + n as u64),
            pool: Vec::new(),
            packets: Vec::new(),
        };
        gn.generate();

        let extra_data = cfg.extra_data();

        // Decode, giving each packet a timestamp that follows the audio before it.
        let mut dec = decoder(&extra_data);
        let mut ms = 0.0;
        let mut packets = Vec::new();
        let mut decoded = Vec::new();

        for payload in std::mem::take(&mut gn.packets) {
            let d = decode_one(&mut dec, ms as u32, &payload);

            if let Some(d) = &d {
                ms += d.frames as f64 * 1000.0 / f64::from(cfg.rate);
            }

            packets.push((ms as u32, payload));
            decoded.push(d);
        }

        let rm = write_rm(&RalfStream { extra_data, packets: packets.clone() });

        let Some(reference) = ffmpeg_decode(cfg.name, &rm)
        else {
            eprintln!("ffmpeg not available; skipping");
            return;
        };

        // Compare packet by packet, to name the first one to differ.
        let channels = usize::from(cfg.channels);
        let mut pos = 0;
        let mut frames = 0;
        let mut good = 0;
        let mut empty = 0;
        let mut failed = 0;
        let mut halves = 0;

        for (i, d) in decoded.iter().enumerate() {
            let Some(d) = d
            else {
                failed += 1;
                continue;
            };

            if d.frames == 0 {
                // A first half, or a table of no blocks.
                empty += 1;
                halves += usize::from(packets[i].1.len() == HALF);
                continue;
            }

            let end = pos + d.samples.len();

            assert!(
                end <= reference.len() && d.samples == reference[pos..end],
                "{}: packet {i} ({} bytes, {} frames) differs from FFmpeg's",
                cfg.name,
                packets[i].1.len(),
                d.frames,
            );

            pos = end;
            frames += d.frames;
            good += 1;
        }

        assert_eq!(pos, reference.len(), "{}: FFmpeg decoded more", cfg.name);

        println!(
            "{}: {} packets: {good} decoded ({frames} frames, {} samples), {halves} first \
             halves, {} without output, {failed} rejected; FFmpeg: identical",
            cfg.name,
            packets.len(),
            frames * channels,
            empty - halves,
        );

        // The streams must be rich enough to mean something.
        assert!(good > 100, "{}: too few packets decoded", cfg.name);
        assert!(halves > 10, "{}: too few split frames", cfg.name);
        assert!(failed > 10, "{}: too few rejected packets", cfg.name);
    }
}
