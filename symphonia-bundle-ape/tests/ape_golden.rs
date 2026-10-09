// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Golden hashes of the full decode of every APE file of the sample corpus (including the
//! corrupt and tag-fuzzed ones, whose errors are part of the hash), so that decoder
//! optimisations are provably bit-identical. The test is a no-op unless `RMPD_SAMPLES` is set.
//!
//! Run with `GOLDEN_PRINT=1 RMPD_SAMPLES=... cargo test -p symphonia-bundle-ape --test ape_golden -- --nocapture`
//! to print the table for the checked-in `GOLDEN` constant.

use std::fs::File;
use std::path::{Path, PathBuf};

use symphonia_bundle_ape::{ApeDecoder, ApeReader};
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

fn feed(h: &mut Fnv, buf: &GenericAudioBufferRef<'_>) {
    let GenericAudioBufferRef::S32(b) = buf
    else {
        panic!("unhandled sample format");
    };
    let n = b.frames();
    let nch = b.spec().channels().count();
    for i in 0..n {
        for ch in 0..nch {
            h.write(&b.plane(ch).unwrap()[i].to_le_bytes());
        }
    }
}

/// `(hash, frames)` of the decode of one file. Open errors, per-packet decode errors and the
/// terminal read error are part of the hash.
fn hash_file(path: &Path) -> (u64, u64) {
    let mut h = Fnv::new();
    let mut frames = 0u64;
    let file = File::open(path).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut reader = match ApeReader::try_new(mss, FormatOptions::default()) {
        Ok(r) => r,
        Err(e) => {
            h.write(format!("OPEN:{e}").as_bytes());
            return (h.0, 0);
        }
    };
    let Some(CodecParameters::Audio(params)) = reader.tracks()[0].codec_params.clone()
    else {
        panic!("no audio codec params");
    };
    let mut decoder = match ApeDecoder::try_new(&params, &AudioDecoderOptions::default()) {
        Ok(d) => d,
        Err(e) => {
            h.write(format!("DEC:{e}").as_bytes());
            return (h.0, 0);
        }
    };
    loop {
        match reader.next_packet() {
            Ok(Some(packet)) => match decoder.decode_ref(&packet.as_packet_ref()) {
                Ok(buf) => {
                    frames += buf.frames() as u64;
                    feed(&mut h, &buf);
                }
                Err(e) => h.write(format!("ERR:{e}").as_bytes()),
            },
            Ok(None) => break,
            Err(e) => {
                h.write(format!("READ:{e}").as_bytes());
                break;
            }
        }
    }
    (h.0, frames)
}

fn ape_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir)
    else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "ape"))
        .collect();
    files.sort();
    files
}

/// `(label, hash, frames)` of every decode.
fn all_hashes(samples: &Path) -> Vec<(String, u64, u64)> {
    let mut out = Vec::new();
    for sub in ["ape", "tags", "corrupt", "tagfuzz"] {
        for f in ape_files(&samples.join(sub)) {
            let name = format!("{sub}/{}", f.file_name().unwrap().to_string_lossy());
            let (h, n) = hash_file(&f);
            out.push((name, h, n));
        }
    }
    out
}

#[test]
fn golden_decode_hashes() {
    let Some(samples) = std::env::var_os("RMPD_SAMPLES")
    else {
        return;
    };
    let hashes = all_hashes(Path::new(&samples));
    if std::env::var_os("GOLDEN_PRINT").is_some() {
        for (name, h, n) in &hashes {
            println!("    (\"{name}\", 0x{h:016x}, {n}),");
        }
        return;
    }
    for (name, h, n) in &hashes {
        let Some(g) = GOLDEN.iter().find(|g| g.0 == name)
        else {
            panic!("{name}: no golden hash (run with GOLDEN_PRINT=1)");
        };
        assert_eq!((g.1, g.2), (*h, *n), "{name}: decode differs from the golden output");
    }
    for g in GOLDEN {
        assert!(hashes.iter().any(|h| h.0 == g.0), "{}: file missing", g.0);
    }
}

// Generated with `GOLDEN_PRINT=1`.
const GOLDEN: &[(&str, u64, u64)] = &[
    ("ape/ape_22k_mono.ape", 0x9e5fe048cc3ffe7e, 661500),
    ("ape/ape_24bit_192k_fast.ape", 0x7540a5f6d1d9692f, 3840000),
    ("ape/ape_24bit_44k.ape", 0x59925247946cf875, 882000),
    ("ape/ape_24bit_96k.ape", 0x3266af969814018b, 1920000),
    ("ape/ape_32bit.ape", 0x3266af969814018b, 1920000),
    ("ape/ape_48k.ape", 0x2363b75e2ddc7f23, 1440000),
    ("ape/ape_6ch.ape", 0x4c4121ec8822e6af, 1323000),
    ("ape/ape_8bit.ape", 0x1727772a6f10ab73, 1323000),
    ("ape/ape_8k_mono.ape", 0x592beedef6f8bfa1, 240000),
    ("ape/ape_extra_high.ape", 0xaa838c8d3b203175, 1323000),
    ("ape/ape_fast.ape", 0xaa838c8d3b203175, 1323000),
    ("ape/ape_high.ape", 0xaa838c8d3b203175, 1323000),
    ("ape/ape_insane.ape", 0xaa838c8d3b203175, 1323000),
    ("ape/ape_mono_high.ape", 0x9c235e25c96ba489, 1323000),
    ("ape/ape_normal.ape", 0xaa838c8d3b203175, 1323000),
    ("tags/ape_apev2_full.ape", 0xb1f6465d52276a0c, 264600),
    ("tags/ape_apev2_plus_id3v1.ape", 0xb1f6465d52276a0c, 264600),
    ("tags/ape_id3v2_3_prepended.ape", 0xb1f6465d52276a0c, 264600),
    ("tags/ape_id3v2_4_prepended.ape", 0xb1f6465d52276a0c, 264600),
    ("tags/ape_mac_native_tags.ape", 0xb1f6465d52276a0c, 264600),
    ("corrupt/corrupt_ape_corrupt_hdr.ape", 0x491baa515dc25e47, 0),
    ("corrupt/corrupt_ape_corrupt_mid.ape", 0xb3d42fa0eb0ed41c, 1175544),
    ("corrupt/corrupt_ape_ff_fill_mid.ape", 0xa81e3fa91d77c9d1, 1249272),
    ("corrupt/corrupt_ape_flip_bits.ape", 0x52953f7cdcbd04bb, 0),
    ("corrupt/corrupt_ape_head200.ape", 0xd6f95e4a52903d25, 0),
    ("corrupt/corrupt_ape_head8.ape", 0xd6f95e4a52903d25, 0),
    ("corrupt/corrupt_ape_trunc10.ape", 0x8af491444e3ef373, 73728),
    ("corrupt/corrupt_ape_trunc50.ape", 0xac09375daf9571b7, 663552),
    ("corrupt/corrupt_ape_trunc99.ape", 0xb9a531cdef97cf22, 1253376),
    ("corrupt/corrupt_ape_zero_hdr.ape", 0x491baa515dc25e47, 0),
    ("corrupt/random_1048576_ape.ape", 0x491baa515dc25e47, 0),
    ("corrupt/random_4096_ape.ape", 0x491baa515dc25e47, 0),
    ("corrupt/random_64_ape.ape", 0x491baa515dc25e47, 0),
    ("corrupt/text_ape.ape", 0x491baa515dc25e47, 0),
    ("corrupt/zero_byte_ape.ape", 0xd6f95e4a52903d25, 0),
    ("tagfuzz/apebad_binary_cover_bad_ape.ape", 0xb1f6465d52276a0c, 264600),
    ("tagfuzz/apebad_count_huge_ape.ape", 0xb1f6465d52276a0c, 264600),
    ("tagfuzz/apebad_item_len_huge_ape.ape", 0xb1f6465d52276a0c, 264600),
    ("tagfuzz/apebad_many_items_ape.ape", 0xb1f6465d52276a0c, 264600),
    ("tagfuzz/apebad_size_huge_ape.ape", 0xb1f6465d52276a0c, 264600),
    ("tagfuzz/fuzz_ape_apev2_full_00_u32.ape", 0x8e0c117ab577a421, 190872),
    ("tagfuzz/fuzz_ape_apev2_full_01_u32.ape", 0x8e0c117ab577a421, 190872),
    ("tagfuzz/fuzz_ape_apev2_full_02_u32.ape", 0x8e0c117ab577a421, 190872),
    ("tagfuzz/fuzz_ape_apev2_full_03_u64.ape", 0x2601a9484f034b6c, 190872),
    ("tagfuzz/fuzz_ape_apev2_full_04_flip.ape", 0x2601a9484f034b6c, 190872),
    ("tagfuzz/fuzz_ape_apev2_full_05_rand.ape", 0xb1f6465d52276a0c, 264600),
    ("tagfuzz/fuzz_ape_apev2_full_06_trunc.ape", 0x260526e61d658ca9, 0),
    ("tagfuzz/fuzz_ape_apev2_full_07_dup.ape", 0xb1f6465d52276a0c, 264600),
    ("tagfuzz/fuzz_ape_apev2_full_08_cut.ape", 0xb1f6465d52276a0c, 264600),
    ("tagfuzz/fuzz_ape_apev2_full_09_u32.ape", 0x8e0c117ab577a421, 190872),
    ("tagfuzz/fuzz_ape_apev2_full_10_u32.ape", 0x8e0c117ab577a421, 190872),
    ("tagfuzz/fuzz_ape_apev2_full_11_u32.ape", 0x8e0c117ab577a421, 190872),
    ("tagfuzz/fuzz_ape_apev2_full_12_u64.ape", 0x8e0c117ab577a421, 190872),
    ("tagfuzz/fuzz_ape_apev2_full_13_flip.ape", 0x8e0c117ab577a421, 190872),
    ("tagfuzz/fuzz_ape_apev2_full_14_rand.ape", 0x8e0c117ab577a421, 190872),
    ("tagfuzz/fuzz_ape_apev2_full_15_trunc.ape", 0x260526e61d658ca9, 0),
    ("tagfuzz/fuzz_ape_apev2_full_16_dup.ape", 0xb1f6465d52276a0c, 264600),
    ("tagfuzz/fuzz_ape_apev2_full_17_cut.ape", 0xfdb18fe1a886c695, 0),
    ("tagfuzz/fuzz_ape_apev2_full_18_u32.ape", 0x8e0c117ab577a421, 190872),
    ("tagfuzz/fuzz_ape_apev2_full_19_u32.ape", 0x8e0c117ab577a421, 190872),
    ("tagfuzz/fuzz_ape_apev2_full_20_u32.ape", 0x8e0c117ab577a421, 190872),
    ("tagfuzz/fuzz_ape_apev2_full_21_u64.ape", 0x8e0c117ab577a421, 190872),
    ("tagfuzz/fuzz_ape_apev2_full_22_flip.ape", 0x519f2c0919f7eeb2, 73728),
    ("tagfuzz/fuzz_ape_apev2_full_23_rand.ape", 0x2601a9484f034b6c, 190872),
];
