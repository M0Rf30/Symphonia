// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![forbid(unsafe_code)]
#![warn(rust_2018_idioms)]

//! Dev-only helpers shared by the tests, benches and examples of the workspace.
//!
//! # Sample corpus
//!
//! Tests that need the external sample corpus read the directory from the `RMPD_SAMPLES`
//! environment variable and are skipped when it is unset or a file is absent. This is the only
//! place that environment variable is interpreted.
//!
//! # Requiring references
//!
//! When the `REQUIRE_REFERENCES` environment variable is set to anything but `0` or the empty
//! string (CI does so), a reference that would otherwise cause a silent skip makes the test panic
//! instead: a corpus file missing from a configured `RMPD_SAMPLES` directory
//! ([`sample_path`], [`open_sample`]) and an external tool missing from `PATH` ([`have_tool`],
//! [`require_reference`]). An unset `RMPD_SAMPLES` still skips, because the corpus is not
//! distributed. Tests that probe for external tools without going through this crate keep
//! skipping.

use std::fs::File;
use std::path::{Path, PathBuf};

use symphonia_core::io::MediaSourceStream;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x100_0000_01b3;

/// FNV-1a style hasher used to pin golden decode output.
///
/// Several mixing shapes exist because the pinned golden values of the different suites were
/// produced with different ones; each method reproduces one of them exactly:
///
/// * [`Fnv::write`]: classic byte-wise FNV-1a.
/// * [`Fnv::add_u32`]: byte-wise FNV-1a over the little-endian bytes of a word.
/// * [`Fnv::put`]: one XOR-multiply step per whole 32-bit word.
/// * [`Fnv::word`]: [`Fnv::put`] followed by `rotate_left(23)`.
#[derive(Clone, Copy, Debug)]
pub struct Fnv(pub u64);

impl Default for Fnv {
    fn default() -> Self {
        Self::new()
    }
}

impl Fnv {
    pub fn new() -> Self {
        Fnv(FNV_OFFSET)
    }

    /// Byte-wise FNV-1a.
    pub fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(FNV_PRIME);
        }
    }

    /// Byte-wise FNV-1a over the little-endian bytes of `v`.
    pub fn add_u32(&mut self, v: u32) {
        self.write(&v.to_le_bytes());
    }

    /// A single XOR-multiply step over the whole word.
    pub fn put(&mut self, v: u32) {
        self.0 = (self.0 ^ u64::from(v)).wrapping_mul(FNV_PRIME);
    }

    /// A single XOR-multiply-rotate step over the whole word.
    pub fn word(&mut self, v: u32) {
        self.put(v);
        self.0 = self.0.rotate_left(23);
    }

    pub fn finish(&self) -> u64 {
        self.0
    }
}

/// Deterministic xorshift64 generator.
#[derive(Clone, Debug)]
pub struct XorShift64(pub u64);

impl XorShift64 {
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A value in `0..n`. `n` must be non-zero.
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// Deterministic 32-bit LCG producing uniform noise in `[-1, 1)`.
#[derive(Clone, Debug)]
pub struct Lcg32(pub u32);

impl Lcg32 {
    pub fn next_f32(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1664525).wrapping_add(1013904223);
        ((self.0 >> 8) as f32 / (1u32 << 23) as f32) - 1.0
    }
}

/// Deterministic 64-bit LCG (PCG multiplier) returning the high 31 bits.
#[derive(Clone, Debug)]
pub struct Lcg64(pub u64);

impl Lcg64 {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

/// The sample corpus directory named by `RMPD_SAMPLES`, if set.
pub fn sample_dir() -> Option<PathBuf> {
    std::env::var_os("RMPD_SAMPLES").map(PathBuf::from)
}

/// Whether the `REQUIRE_REFERENCES` environment variable demands that references are present.
pub fn require_references() -> bool {
    std::env::var_os("REQUIRE_REFERENCES").is_some_and(|v| !v.is_empty() && v != "0")
}

/// Whether the external tool `name` can be executed (probed with `-version`, which ffmpeg and
/// ffprobe accept; a tool that rejects the argument still counts as present when it spawns).
///
/// # Panics
///
/// If the tool is missing and `REQUIRE_REFERENCES` is set.
pub fn have_tool(name: &str) -> bool {
    let found = std::process::Command::new(name)
        .arg("-version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok();

    if !found {
        require_reference(false, &format!("tool `{name}` not found in PATH"));
    }

    found
}

/// Reports a missing reference. Prints a skip notice and returns when `present` is false, or
/// panics with `what` when `REQUIRE_REFERENCES` is set. Does nothing if `present` is true.
///
/// # Panics
///
/// If `present` is false and `REQUIRE_REFERENCES` is set.
pub fn require_reference(present: bool, what: &str) {
    if present {
        return;
    }

    if require_references() {
        panic!("REQUIRE_REFERENCES is set but a reference is missing: {what}");
    }

    eprintln!("skipping: {what}");
}

/// The path of `name` in the sample corpus. Returns `None` (after printing a skip notice) when
/// `RMPD_SAMPLES` is unset or the file does not exist.
///
/// # Panics
///
/// If `RMPD_SAMPLES` is set, the file does not exist and `REQUIRE_REFERENCES` is set.
pub fn sample_path(name: impl AsRef<Path>) -> Option<PathBuf> {
    let Some(dir) = sample_dir()
    else {
        eprintln!("RMPD_SAMPLES not set; skipping");
        return None;
    };

    let path = dir.join(name);

    if path.exists() {
        Some(path)
    }
    else {
        require_reference(false, &format!("{} not found", path.display()));
        None
    }
}

/// Opens `path` as a [`MediaSourceStream`]. Panics with the path if it cannot be opened.
pub fn open_mss(path: &Path) -> MediaSourceStream<'static> {
    let file = File::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    MediaSourceStream::new(Box::new(file), Default::default())
}

/// Opens the sample `name` of the corpus as a [`MediaSourceStream`], or `None` (skip) when it is
/// not available.
pub fn open_sample(name: impl AsRef<Path>) -> Option<MediaSourceStream<'static>> {
    sample_path(name).map(|path| open_mss(&path))
}

/// Encodes `n` (< 2^28) as a 4-byte ID3v2 syncsafe integer.
pub fn syncsafe(n: usize) -> [u8; 4] {
    [((n >> 21) & 0x7f) as u8, ((n >> 14) & 0x7f) as u8, ((n >> 7) & 0x7f) as u8, (n & 0x7f) as u8]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn present_reference_never_panics() {
        require_reference(true, "anything");
    }

    #[test]
    fn missing_tool_is_not_found() {
        // Only meaningful when references are optional.
        if !require_references() {
            assert!(!have_tool("symphonia-no-such-tool"));
        }
    }
}
