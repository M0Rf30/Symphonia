// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Property/fuzz-style robustness test (wave 3 hardening): the decoder MUST NEVER panic on
//! arbitrary input -- malformed/truncated packets, bogus `OpusHead` extra data, random packet
//! sequences mixing modes/bandwidths/channels, lost packets, huge frame counts, zero-length
//! packets, or packets longer than 1275 bytes. Every call must return `Ok` or `Err`.
//!
//! Exercises three public surfaces:
//!   - [`OpusDecoder`] (single stream, mono/stereo, all sample rates).
//!   - [`MultistreamDecoder`] (random channel mappings).
//!   - [`OpusAudioDecoder`] (the Symphonia `AudioDecoder` integration), including random
//!     `OpusHead` extra-data bytes.
//!
//! Inputs come from two sources: (a) real RFC 8251 `.bit` vector packets, mutated by bit flips,
//! truncation, byte insertion, and TOC rewrites; (b) pure random bytes of random length
//! (including 0 and > 1275). A small deterministic xorshift64* PRNG is used instead of pulling in
//! a fuzzing crate, so failures are exactly reproducible from the printed seed.
//!
//! Uses `std::panic::catch_unwind` *only* to report the first failing input with a repro seed --
//! the whole point of this test is that it should never actually catch anything.
//!
//! Packet count defaults to 200_000 in `--release` and 3_000 in debug (arithmetic overflow checks
//! are only active in debug, so a full 200k there would be needlessly slow); override with
//! `OPUS_FUZZ_COUNT=<n>`.

mod common;

use std::panic::{self, AssertUnwindSafe};

use common::opus_demo::BitFile;
use symphonia_codec_opus::audio_decoder::OpusAudioDecoder;
use symphonia_codec_opus::decoder::{OpusDecoder, SampleRate};
use symphonia_codec_opus::mapping::{ChannelMapping, OpusHead};
use symphonia_codec_opus::multistream::MultistreamDecoder;
use symphonia_core::codecs::audio::well_known::CODEC_ID_OPUS;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder as _, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

/// Small, fast, deterministic xorshift64* PRNG. No external `rand` dependency: the whole point of
/// this test is a fully reproducible, self-contained fuzz corpus generator.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    fn gen_range(&mut self, lo: usize, hi_inclusive: usize) -> usize {
        if hi_inclusive <= lo {
            return lo;
        }
        lo + (self.next_u64() as usize) % (hi_inclusive - lo + 1)
    }

    /// True with probability `num/den`.
    fn chance(&mut self, num: u32, den: u32) -> bool {
        (self.next_u32() % den) < num
    }

    fn byte(&mut self) -> u8 {
        (self.next_u32() & 0xFF) as u8
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.byte()).collect()
    }
}

/// Applies one random mutation to `pkt` in place: bit flip, truncation, random byte insertion, or
/// a full TOC (first byte) rewrite -- covering every mode/bandwidth/frame-count-code/stereo
/// combination when repeated.
fn mutate_once(rng: &mut Rng, pkt: &mut Vec<u8>) {
    if pkt.is_empty() {
        // Nothing to flip/truncate; only insertion is meaningful.
        let n = rng.gen_range(0, 4);
        for _ in 0..n {
            pkt.push(rng.byte());
        }
        return;
    }
    match rng.gen_range(0, 4) {
        0 => {
            // Bit flip at a random position.
            let i = rng.gen_range(0, pkt.len() - 1);
            let bit = 1u8 << rng.gen_range(0, 7);
            pkt[i] ^= bit;
        }
        1 => {
            // Truncate to a random shorter (or equal, or zero) length.
            let n = rng.gen_range(0, pkt.len());
            pkt.truncate(n);
        }
        2 => {
            // Insert a run of random bytes at a random position.
            let at = rng.gen_range(0, pkt.len());
            let n = rng.gen_range(1, 16);
            let insert = rng.bytes(n);
            pkt.splice(at..at, insert);
        }
        _ => {
            // Rewrite the TOC byte to a fully random value (mode/bandwidth/frame-count-code/
            // stereo bit all become random, independent of the frame data that follows).
            pkt[0] = rng.byte();
        }
    }
}

/// Applies 1-3 mutations, then occasionally appends extra random bytes to push the packet past
/// the 1275-byte single-frame limit.
fn mutate_packet(rng: &mut Rng, base: &[u8]) -> Vec<u8> {
    let mut pkt = base.to_vec();
    let rounds = rng.gen_range(1, 3);
    for _ in 0..rounds {
        mutate_once(rng, &mut pkt);
    }
    if rng.chance(1, 20) {
        let extra = rng.gen_range(1, 2000);
        pkt.extend(rng.bytes(extra));
    }
    pkt
}

/// A packet of pure random bytes, random length (including 0 and > 1275, RFC 6716's single-frame
/// size cap) with no relationship to any real Opus stream.
fn random_packet(rng: &mut Rng) -> Vec<u8> {
    let len = if rng.chance(1, 50) {
        0
    }
    else if rng.chance(1, 20) {
        rng.gen_range(1276, 4000) // exceeds the 1275-byte cap.
    }
    else {
        rng.gen_range(1, 400)
    };
    rng.bytes(len)
}

/// Real packets pulled from every `.bit` vector under `OPUS_TESTVECTORS`, if set. Empty (with a
/// diagnostic print, not a failure) when the env var is absent so this test still runs -- with a
/// smaller, purely-synthetic corpus -- outside the conformance harness's environment.
fn load_corpus() -> Vec<Vec<u8>> {
    let mut corpus = Vec::new();
    if let Some(dir) = std::env::var_os("OPUS_TESTVECTORS") {
        let dir = std::path::PathBuf::from(dir);
        for n in 1..=12 {
            let path = dir.join(format!("testvector{n:02}.bit"));
            if let Ok(data) = std::fs::read(&path) {
                let bf = BitFile::parse(&data);
                for p in bf.iter() {
                    if !p.lost {
                        corpus.push(p.payload.clone());
                    }
                }
            }
        }
    }
    if corpus.is_empty() {
        eprintln!("robustness: OPUS_TESTVECTORS not set/found; using only synthetic packets");
    }
    corpus
}

const SAMPLE_RATES: [SampleRate; 5] =
    [SampleRate::Hz8000, SampleRate::Hz12000, SampleRate::Hz16000, SampleRate::Hz24000, SampleRate::Hz48000];

/// Frame sizes to request from the decoder: every valid multiple of 2.5ms up to 120ms at 48kHz,
/// plus a few intentionally-invalid values (0, 1, a huge value) to exercise the `BadArgument`/
/// `BufferTooSmall` paths without ever panicking.
fn random_frame_size(rng: &mut Rng, fs: u32) -> usize {
    let f2_5 = fs as usize / 400;
    if rng.chance(1, 20) {
        return *[0usize, 1, 3, fs as usize * 2].get(rng.gen_range(0, 3)).unwrap();
    }
    let mult = rng.gen_range(1, 48); // up to 120ms.
    f2_5 * mult
}

fn run_catch<F: FnOnce()>(label: &str, seed: u64, f: F) {
    let result = panic::catch_unwind(AssertUnwindSafe(f));
    if let Err(e) = result {
        let msg = e
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| e.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        panic!("robustness: PANIC in {label} at seed {seed}: {msg}");
    }
}

/// Feeds one packet through a single-stream [`OpusDecoder`], covering `decode_native`'s
/// `self_delimited`/`decode_fec` flags and both `None` (PLC) and `Some` data.
fn fuzz_single_stream(rng: &mut Rng, dec: &mut OpusDecoder, pkt: &[u8], seed: u64) {
    let fs = dec.sample_rate().as_hz();
    let mut out = vec![0f32; (fs as usize / 25 * 3) * dec.channels() as usize + 64];
    let frame_size = random_frame_size(rng, fs);
    let decode_fec = rng.chance(1, 4);
    let self_delimited = rng.chance(1, 4);
    let data_opt = if rng.chance(1, 10) { None } else { Some(pkt) };
    run_catch("OpusDecoder::decode_native", seed, || {
        let _ = dec.decode_native(data_opt, &mut out, frame_size, decode_fec, self_delimited);
    });
    // Also exercise the plain `decode` (PLC-only) entry point directly, matching real playback
    // (a lost network packet -> `data = None`).
    if rng.chance(1, 8) {
        run_catch("OpusDecoder::decode(None)", seed, || {
            let _ = dec.decode(None, &mut out, frame_size.max(fs as usize / 400));
        });
    }
}

/// A random, but internally-consistent (per `OpusHead::parse`'s own validation) channel mapping,
/// used both to build [`MultistreamDecoder`]s directly and to synthesize `OpusHead` extra-data.
struct RandHead {
    channel_count: u8,
    mapping: ChannelMapping,
    output_gain: i16,
}

fn random_mapping(rng: &mut Rng) -> RandHead {
    if rng.chance(1, 2) {
        let channel_count = if rng.chance(1, 2) { 1 } else { 2 };
        return RandHead { channel_count, mapping: family0(channel_count), output_gain: rng.next_u32() as i16 };
    }
    let stream_count = rng.gen_range(1, 8) as u8;
    let coupled_count = rng.gen_range(0, stream_count as usize) as u8;
    let channel_count = rng.gen_range(1, 8).max((stream_count + coupled_count) as usize) as u8;
    let max_index = stream_count as u16 + coupled_count as u16;
    let table: Vec<u8> = (0..channel_count)
        .map(|_| if rng.chance(1, 10) { 255 } else { rng.gen_range(0, max_index.saturating_sub(1) as usize) as u8 })
        .collect();
    RandHead {
        channel_count,
        mapping: ChannelMapping { family: 1, stream_count, coupled_count, table },
        output_gain: rng.next_u32() as i16,
    }
}

fn family0(channels: u8) -> ChannelMapping {
    ChannelMapping { family: 0, stream_count: 1, coupled_count: (channels == 2) as u8, table: Vec::new() }
}

fn fuzz_multistream(rng: &mut Rng, pkt: &[u8], seed: u64) {
    let head = random_mapping(rng);
    let sr = SAMPLE_RATES[rng.gen_range(0, SAMPLE_RATES.len() - 1)];
    let frame_size = random_frame_size(rng, sr.as_hz());
    let decode_fec = rng.chance(1, 3);
    run_catch("MultistreamDecoder::try_new+decode", seed, || {
        if let Ok(mut dec) = MultistreamDecoder::try_new(sr, head.channel_count, head.mapping.clone()) {
            dec.set_gain(head.output_gain);
            let mut out = vec![0f32; frame_size * head.channel_count.max(1) as usize + 64];
            let _ = dec.decode(Some(pkt), &mut out, frame_size, decode_fec);
            let _ = dec.decode(None, &mut out, frame_size.max(sr.as_hz() as usize / 400), false);
        }
    });
}


/// Builds random `OpusHead` extra-data bytes: sometimes a well-formed header (correct magic,
/// random-but-internally-consistent mapping), sometimes pure garbage, sometimes a truncated
/// well-formed header.
fn random_opus_head_bytes(rng: &mut Rng) -> Vec<u8> {
    if rng.chance(1, 3) {
        let n = rng.gen_range(0, 40);
        return rng.bytes(n);
    }
    let head = random_mapping(rng);
    let mut buf = Vec::new();
    buf.extend_from_slice(b"OpusHead");
    buf.push(if rng.chance(19, 20) { 1 } else { rng.byte() }); // version
    buf.push(head.channel_count);
    buf.extend_from_slice(&rng.next_u32().to_le_bytes()[..2]); // pre_skip
    buf.extend_from_slice(&rng.next_u32().to_le_bytes()); // input_sample_rate
    buf.extend_from_slice(&head.output_gain.to_le_bytes());
    buf.push(head.mapping.family);
    if head.mapping.family != 0 {
        buf.push(head.mapping.stream_count);
        buf.push(head.mapping.coupled_count);
        buf.extend_from_slice(&head.mapping.table);
    }
    if rng.chance(1, 5) {
        let cut = rng.gen_range(0, buf.len());
        buf.truncate(cut);
    }
    buf
}

fn fuzz_audio_decoder(rng: &mut Rng, pkt: &[u8], seed: u64) {
    let extra = random_opus_head_bytes(rng);
    // `OpusHead::parse` itself must never panic, whether or not the decoder is ever built.
    run_catch("OpusHead::parse", seed, || {
        let _ = OpusHead::parse(&extra);
    });

    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_OPUS);
    params.with_extra_data(extra.into_boxed_slice());
    let opts = AudioDecoderOptions::default();

    run_catch("OpusAudioDecoder::try_new+decode_ref", seed, || {
        if let Ok(mut dec) = OpusAudioDecoder::try_new(&params, &opts) {
            let packet_ref = PacketRef::new(0, Timestamp::from(0i64), Duration::from(960u64), pkt);
            let _ = dec.decode_ref(&packet_ref);
            dec.reset();
            let packet_ref2 = PacketRef::new(0, Timestamp::from(0i64), Duration::from(960u64), pkt);
            let _ = dec.decode_ref(&packet_ref2);
        }
    });
}

fn fuzz_count() -> usize {
    if let Ok(v) = std::env::var("OPUS_FUZZ_COUNT") {
        if let Ok(n) = v.parse() {
            return n;
        }
    }
    if cfg!(debug_assertions) {
        3_000
    }
    else {
        200_000
    }
}

/// The main property/fuzz test: never panic, across all three public decode surfaces, on a mix
/// of mutated-real and pure-random packets. See module docs for the full rationale.
#[test]
fn never_panics_on_malformed_input() {
    let corpus = load_corpus();
    let count = fuzz_count();
    let mut rng = Rng::new(0xC0FF_EE12_3456_789A);

    // A handful of persistent single-stream decoders (reused across many packets, like a real
    // playback session) so we also exercise state carried across calls (prev_mode, redundancy,
    // PLC history), not just fresh-decoder-per-packet.
    let mut persistent: Vec<OpusDecoder> = SAMPLE_RATES
        .iter()
        .flat_map(|&sr| [OpusDecoder::try_new(sr, 1).unwrap(), OpusDecoder::try_new(sr, 2).unwrap()])
        .collect();

    for i in 0..count {
        let seed = rng.next_u64();
        let pkt = if !corpus.is_empty() && rng.chance(3, 4) {
            let base = &corpus[rng.gen_range(0, corpus.len() - 1)];
            mutate_packet(&mut rng, base)
        }
        else {
            random_packet(&mut rng)
        };

        // Single-stream: exercise every persistent decoder occasionally, and a fresh one
        // (to test the very-first-packet path) occasionally.
        let idx = i % persistent.len();
        {
            let (before, rest) = persistent.split_at_mut(idx);
            let (dec, after) = rest.split_first_mut().unwrap();
            let _ = (before, after);
            fuzz_single_stream(&mut rng, dec, &pkt, seed);
        }
        if rng.chance(1, 20) {
            let sr = SAMPLE_RATES[rng.gen_range(0, SAMPLE_RATES.len() - 1)];
            let ch = if rng.chance(1, 2) { 1 } else { 2 };
            let mut fresh = OpusDecoder::try_new(sr, ch).unwrap();
            fuzz_single_stream(&mut rng, &mut fresh, &pkt, seed);
        }

        fuzz_multistream(&mut rng, &pkt, seed);
        fuzz_audio_decoder(&mut rng, &pkt, seed);
    }
}
