// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Wave 3 hardening: verifies the decoder hot path performs zero heap allocations per packet.
//!
//! Wraps the system allocator in a counter that can be turned on/off, decodes a handful of
//! "warm-up" packets from each vector with counting disabled (this crate intentionally does all
//! its sizing up front in `OpusDecoder::try_new`/`CeltDecoder::new`, but any first-packet-only
//! lazy setup would also land here), then decodes the remainder of the vector with counting
//! enabled and asserts the count stayed at zero.
//!
//! Run with:
//!   OPUS_TESTVECTORS=/home/gianluca/M0Rf30/wt/ref/opus_newvectors \
//!     cargo test -p symphonia-codec-opus --release --test alloc -- --include-ignored

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use common::opus_demo::BitFile;
use symphonia_codec_opus::decoder::{OpusDecoder, SampleRate};

struct CountingAllocator;

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

fn testvectors_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("OPUS_TESTVECTORS").map(std::path::PathBuf::from)
}

/// Decodes `vector_index` at `channels`, with allocation counting enabled for every packet after
/// the first `warmup` non-lost packets. Returns the number of allocations observed during the
/// counted portion.
fn count_allocs_for_vector(bit_path: &std::path::Path, channels: u8, warmup: usize) -> usize {
    let data = std::fs::read(bit_path).unwrap();
    let bitfile = BitFile::parse(&data);

    let mut decoder = OpusDecoder::try_new(SampleRate::Hz48000, channels).unwrap();
    // Sized once, outside the counted region, like a real caller's reusable output buffer.
    const MAX_FRAME_SIZE: usize = 5760;
    let mut out = vec![0f32; MAX_FRAME_SIZE * channels as usize];

    let mut warmed_up = 0usize;
    for pkt in bitfile.iter() {
        if warmed_up < warmup {
            if pkt.lost {
                let frame_size = decoder.last_packet_duration().max(48000 / 100);
                let _ = decoder.decode(None, &mut out, frame_size);
            }
            else {
                let _ = decoder.decode(Some(&pkt.payload), &mut out, MAX_FRAME_SIZE);
                warmed_up += 1;
            }
            continue;
        }
        break;
    }

    ALLOC_COUNT.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);

    let mut counted = 0usize;
    for pkt in bitfile.iter().skip(warmed_up_packet_index(&bitfile, warmup)) {
        if pkt.lost {
            let frame_size = decoder.last_packet_duration().max(48000 / 100);
            let _ = decoder.decode(None, &mut out, frame_size);
        }
        else {
            let _ = decoder.decode(Some(&pkt.payload), &mut out, MAX_FRAME_SIZE);
        }
        counted += 1;
    }

    COUNTING.store(false, Ordering::Relaxed);
    let allocs = ALLOC_COUNT.load(Ordering::Relaxed);
    eprintln!(
        "{}: {} allocation(s) over {} counted packets (channels={channels})",
        bit_path.display(),
        allocs,
        counted
    );
    allocs
}

/// Index into the packet stream of the first packet *after* `warmup` non-lost packets have been
/// consumed (mirrors the warm-up loop above so the counted region starts at exactly the same
/// packet).
fn warmed_up_packet_index(bitfile: &BitFile, warmup: usize) -> usize {
    let mut warmed_up = 0usize;
    for (i, pkt) in bitfile.iter().enumerate() {
        if warmed_up >= warmup {
            return i;
        }
        if !pkt.lost {
            warmed_up += 1;
        }
    }
    bitfile.len()
}

/// Runs all three vectors in one test (not three separate `#[test]` fns): the counting allocator
/// is global process state, and Rust's default test harness runs `#[test]` fns concurrently on
/// separate threads, which would let one test's decode activity pollute another's count window.
#[test]
fn zero_alloc_hot_path() {
    let Some(dir) = testvectors_dir()
    else {
        eprintln!("OPUS_TESTVECTORS not set; skipping");
        return;
    };

    // Vector 01: pure CELT -- exercises `CeltDecoder::decode_with_ec`'s full per-band hot path
    // (`quant_all_bands`, `alg_unquant`, `clt_compute_allocation`) with no SILK/redundancy
    // involvement at all.
    let allocs = count_allocs_for_vector(&dir.join("testvector01.bit"), 2, 5);
    assert_eq!(allocs, 0, "vector01 (CELT) should perform zero heap allocations per decode");

    // Vector 05: Hybrid (SILK + CELT sharing one range coder per packet) -- exercises both the
    // SILK decode path and CELT's per-band hot path together, plus hybrid SILK/CELT PCM mixing.
    let allocs = count_allocs_for_vector(&dir.join("testvector05.bit"), 2, 5);
    assert_eq!(allocs, 0, "vector05 (Hybrid) should perform zero heap allocations per decode");

    // Vector 12: SILK-only with frequent CELT/SILK redundancy frames -- exercises the
    // `redundant_audio`/`pcm_silk` scratch and the redundancy bookkeeping in `opus_decode_frame`.
    let allocs = count_allocs_for_vector(&dir.join("testvector12.bit"), 2, 5);
    assert_eq!(allocs, 0, "vector12 (SILK + redundancy) should perform zero heap allocations per decode");
}
