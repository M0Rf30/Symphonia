// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tag writing for the files Symphonia reads.
//!
//! Symphonia's readers turn native tags into [`Tag`]s (a raw key/value plus an optional
//! [`StandardTag`](symphonia_core::meta::StandardTag)) and [`Visual`]s. This crate goes the other
//! way: [`write_file`] replaces a file's tags with exactly the given tags and visuals.
//!
//! * A tag whose `std` is set is written to the native field the format's *reader* maps to that
//!   standard tag (the serializers live next to those readers in `symphonia-metadata` and
//!   `symphonia-format-isomp4`, and their tests prove that reading back yields the same standard
//!   tag). A tag without `std` is written under its raw key as a free-form field (ID3v2 `TXXX`,
//!   MP4 `----:com.apple.iTunes:<key>`, Vorbis/APE `<KEY>`).
//! * Native data the tag model does not represent (unmanaged ID3v2 frames such as `PRIV` or
//!   `POPM`, FLAC `SEEKTABLE`/`CUESHEET`/`APPLICATION` blocks, unmanaged MP4 `ilst` atoms, other
//!   RIFF chunks, ...) is preserved, and the audio bitstream is never altered.
//! * Every write builds the new file next to the original and renames it over the original, so a
//!   failed write never leaves a half-written file.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]

mod ape;
mod aiff;
mod dsf;
mod flac;
mod mp4;
mod mpeg;
mod ogg;
mod riff;

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use symphonia_core::meta::{Tag, Visual};

/// Container families [`write_file`] handles, sniffed from the file's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    /// MPEG audio elementary stream (MP1/2/3, ADTS AAC): ID3v2 at the front, optional ID3v1 at
    /// the end.
    MpegAudio,
    Flac,
    OggVorbis,
    OggOpus,
    OggFlac,
    OggSpeex,
    /// ISO BMFF / QuickTime (`.m4a`, `.m4b`, `.mp4`).
    Mp4,
    Wav,
    Aiff,
    Dsf,
    WavPack,
    MonkeysAudio,
    Musepack,
    /// Not writable (Matroska/WebM, CAF, DFF, ...).
    Other,
}

impl Container {
    /// Whether [`write_file`] supports this container.
    pub fn is_writable(self) -> bool {
        self != Container::Other
    }
}

/// ID3v2 major version policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Id3v2Version {
    /// Keep v2.3 when the file already has a v2.3 tag, otherwise write v2.4.
    #[default]
    Preserve,
    V3,
    V4,
}

/// Options for [`write_file`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteOptions {
    pub id3v2_version: Id3v2Version,
    /// Padding to leave after a tag that lives at the front of the file (ID3v2, FLAC metadata,
    /// MP4 `free`), so later edits can be done without moving the audio. Existing padding is
    /// reused when the new tag fits in it.
    pub padding: usize,
}

impl Default for WriteOptions {
    fn default() -> Self {
        WriteOptions { id3v2_version: Id3v2Version::Preserve, padding: 4096 }
    }
}

/// Errors returned by [`write_file`].
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The container (or a variant of it, e.g. RF64) is not writable.
    Unsupported(String),
    /// The file's structure is invalid.
    Malformed(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Unsupported(s) => write!(f, "unsupported: {s}"),
            Error::Malformed(s) => write!(f, "malformed file: {s}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Replaces `path`'s tags with exactly `tags` and `visuals` (see the crate docs for what is
/// preserved).
pub fn write_file(path: &Path, tags: &[Tag], visuals: &[Visual], opts: &WriteOptions) -> Result<()> {
    match sniff(path)? {
        Container::MpegAudio => mpeg::write(path, tags, visuals, opts),
        Container::Flac => flac::write(path, tags, visuals, opts),
        Container::OggVorbis | Container::OggOpus | Container::OggFlac | Container::OggSpeex => {
            ogg::write(path, tags, visuals, opts)
        }
        Container::Mp4 => mp4::write(path, tags, visuals, opts),
        Container::Wav => riff::write(path, tags, visuals, opts),
        Container::Aiff => aiff::write(path, tags, visuals, opts),
        Container::Dsf => dsf::write(path, tags, visuals, opts),
        Container::WavPack | Container::MonkeysAudio | Container::Musepack => {
            ape::write(path, tags, visuals, opts)
        }
        Container::Other => Err(Error::Unsupported(path.display().to_string())),
    }
}

/// Identifies `path`'s container from its leading bytes. A leading ID3v2
/// tag is skipped first (FLAC and APE files sometimes carry one).
pub fn sniff(path: &Path) -> Result<Container> {
    let mut file = File::open(path)?;
    let mut head = [0u8; 10];
    let mut start = 0u64;
    if read_up_to(&mut file, &mut head)? == 10 && &head[..3] == b"ID3" {
        let size = synchsafe(&head[6..10]);
        let footer = if head[5] & 0x10 != 0 { 10 } else { 0 };
        start = 10 + u64::from(size) + footer;
    }
    file.seek(SeekFrom::Start(start))?;
    let mut buf = [0u8; 64];
    let n = read_up_to(&mut file, &mut buf)?;
    let b = &buf[..n];
    let container = if b.starts_with(b"fLaC") {
        Container::Flac
    } else if b.starts_with(b"OggS") && n >= 28 + 8 {
        // First page body starts after the 27-byte header + lacing table.
        let segs = usize::from(b[26]);
        let body = &b[(27 + segs).min(n)..];
        if body.starts_with(b"\x01vorbis") {
            Container::OggVorbis
        } else if body.starts_with(b"OpusHead") {
            Container::OggOpus
        } else if body.starts_with(b"\x7fFLAC") {
            Container::OggFlac
        } else if body.starts_with(b"Speex   ") {
            Container::OggSpeex
        } else {
            Container::Other
        }
    } else if n >= 12 && &b[4..8] == b"ftyp" {
        Container::Mp4
    } else if n >= 12 && (b.starts_with(b"RIFF") || b.starts_with(b"RF64")) && &b[8..12] == b"WAVE"
    {
        Container::Wav
    } else if n >= 12 && b.starts_with(b"FORM") && (&b[8..12] == b"AIFF" || &b[8..12] == b"AIFC")
    {
        Container::Aiff
    } else if b.starts_with(b"DSD ") {
        Container::Dsf
    } else if b.starts_with(b"wvpk") {
        Container::WavPack
    } else if b.starts_with(b"MAC ") {
        Container::MonkeysAudio
    } else if b.starts_with(b"MPCK") || b.starts_with(b"MP+") {
        Container::Musepack
    } else if n >= 2 && b[0] == 0xFF && b[1] & 0xE0 == 0xE0 {
        Container::MpegAudio
    } else if start > 0 {
        // An ID3v2 tag followed by something that isn't a known container
        // header: MPEG audio whose first frame is preceded by junk.
        Container::MpegAudio
    } else {
        Container::Other
    };
    Ok(container)
}

/// Decodes a 4-byte ID3v2 synchsafe integer.
#[must_use]
pub(crate) fn synchsafe(b: &[u8]) -> u32 {
    b.iter().take(4).fold(0, |acc, &x| (acc << 7) | u32::from(x & 0x7F))
}

/// Reads until `buf` is full or EOF; returns the byte count.
pub(crate) fn read_up_to(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(k) => filled += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Copies `len` bytes of `src` starting at `offset` into `dst`.
pub(crate) fn copy_range(src: &mut File, dst: &mut impl Write, offset: u64, len: u64) -> Result<()> {
    src.seek(SeekFrom::Start(offset))?;
    let copied = io::copy(&mut (&mut *src).take(len), dst)?;
    if copied != len {
        return Err(Error::Malformed(format!(
            "file ended {} bytes early",
            len - copied
        )));
    }
    Ok(())
}

/// Rewrites `path` safely: `build` receives the original file (opened for
/// reading) and a buffered writer for the replacement, created next to the
/// original so the final `rename` is atomic on the same filesystem. On
/// success the replacement is flushed, synced, given the original's
/// permissions and renamed over `path`; on any error it is deleted and the
/// original is left untouched.
pub(crate) fn replace_file(
    path: &Path,
    build: impl FnOnce(&mut File, &mut BufWriter<File>) -> Result<()>,
) -> Result<()> {
    let mut src = File::open(path)?;
    let perms = src.metadata()?.permissions();
    let tmp = temp_path(path);
    let result = (|| {
        let mut out = BufWriter::new(
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?,
        );
        build(&mut src, &mut out)?;
        let file = out.into_inner().map_err(|e| Error::Io(e.into_error()))?;
        file.sync_all()?;
        fs::set_permissions(&tmp, perms)?;
        drop(src);
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.aulos-tag-{}.tmp", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aulos-tagio-{}-{}",
            std::process::id(),
            name
        ));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn sniff_skips_a_leading_id3v2_tag() {
        let mut bytes = b"ID3\x04\x00\x00\x00\x00\x00\x02\0\0".to_vec();
        bytes.extend_from_slice(b"fLaC\0\0\0\x22");
        let p = temp_file("id3.flac", &bytes);
        assert_eq!(sniff(&p).unwrap(), Container::Flac);
    }

    #[test]
    fn replace_file_keeps_the_original_when_build_fails() {
        let p = temp_file("keep.bin", b"original");
        let err = replace_file(&p, |_, out| {
            out.write_all(b"partial")?;
            Err(Error::Malformed("boom".into()))
        });
        assert!(err.is_err());
        assert_eq!(fs::read(&p).unwrap(), b"original");
        assert!(!temp_path(&p).exists());
    }

    #[test]
    fn replace_file_swaps_in_the_new_content() {
        let p = temp_file("swap.bin", b"0123456789");
        replace_file(&p, |src, out| {
            out.write_all(b"new:")?;
            copy_range(src, out, 5, 5)
        })
        .unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"new:56789");
    }
}
