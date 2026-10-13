// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tag writing for RIFF WAVE files, and the chunk helpers shared with AIFF.
//!
//! A WAVE file carries tags in an `id3 ` chunk (ID3v2) and in a `LIST` chunk of type `INFO`. A
//! write rebuilds both from the given tags: the `id3 ` chunk is replaced (or added, or removed
//! when there is nothing to write), and the `INFO` list is rewritten, keeping the sub-chunks the
//! reader does not map to standard tags. Every other chunk (`fmt `, `data`, `fact`, `bext`,
//! `cue `, other `LIST` types, ...) is copied byte for byte, in order.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use symphonia_core::meta::{Tag, Visual};
use symphonia_metadata::embedded::riff::{is_managed_info_id, serialize_info};
use symphonia_metadata::id3v2::write::{self as id3w, Existing};

use crate::{Error, Id3v2Version, Result, WriteOptions, copy_range, read_up_to, replace_file};

/// The largest `id3 `/`INFO` payload that is read into memory.
const MAX_METADATA_PAYLOAD: u32 = 256 * 1024 * 1024;

/// A chunk of a RIFF-like container (a 4-byte id, a 4-byte size, the payload, and a pad byte if the
/// size is odd).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Chunk {
    pub id: [u8; 4],
    /// The offset of the chunk header.
    pub offset: u64,
    /// The size of the payload, excluding the pad byte.
    pub size: u32,
}

impl Chunk {
    pub fn payload_offset(&self) -> u64 {
        self.offset + 8
    }

    /// The offset one past the pad byte (which may be past the end of the file if the last chunk
    /// of the file has an odd size and no pad byte).
    fn padded_end(&self) -> u64 {
        self.payload_offset() + u64::from(self.size) + u64::from(self.size & 1)
    }

    pub fn is_id3(&self) -> bool {
        &self.id == b"id3 " || &self.id == b"ID3 "
    }
}

/// The chunks of a RIFF-like file.
pub(crate) struct Scan {
    pub file_len: u64,
    pub chunks: Vec<Chunk>,
    /// The offset one past the last chunk. Anything between this and the end of the file is not a
    /// chunk (trailing junk) and is kept as is.
    pub end: u64,
}

fn read_u32(b: &[u8], big_endian: bool) -> u32 {
    let b = [b[0], b[1], b[2], b[3]];
    if big_endian { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) }
}

/// Walks the chunks of `src`, a file with a 12-byte (`magic`, size, form) header. Chunks are
/// followed until the end of the file or a chunk header that is not valid (a printable ASCII id,
/// at least 8 bytes left), as the readers do not bound the walk by the size in the header.
pub(crate) fn scan(src: &mut File, big_endian: bool) -> Result<([u8; 4], [u8; 4], Scan)> {
    let file_len = src.metadata()?.len();
    src.seek(SeekFrom::Start(0))?;

    let mut head = [0u8; 12];
    if read_up_to(src, &mut head)? < 12 {
        return Err(Error::Malformed("file is shorter than a RIFF header".into()));
    }

    let magic = [head[0], head[1], head[2], head[3]];
    let form = [head[8], head[9], head[10], head[11]];

    let mut chunks = Vec::new();
    let mut pos = 12u64;

    while pos + 8 <= file_len {
        let mut hdr = [0u8; 8];
        src.seek(SeekFrom::Start(pos))?;
        if read_up_to(src, &mut hdr)? < 8 {
            break;
        }

        let id = [hdr[0], hdr[1], hdr[2], hdr[3]];
        if !id.iter().all(|b| (0x20..0x7f).contains(b)) {
            break;
        }

        let chunk = Chunk { id, offset: pos, size: read_u32(&hdr[4..8], big_endian) };

        if chunk.payload_offset() + u64::from(chunk.size) > file_len {
            return Err(Error::Malformed(format!(
                "chunk '{}' at offset {} extends past the end of the file",
                String::from_utf8_lossy(&id),
                pos
            )));
        }

        pos = chunk.padded_end().min(file_len);
        chunks.push(chunk);
    }

    Ok((magic, form, Scan { file_len, chunks, end: pos }))
}

/// Builds a complete chunk: header, payload and pad byte.
pub(crate) fn make_chunk(id: [u8; 4], payload: &[u8], big_endian: bool) -> Result<Vec<u8>> {
    let len = u32::try_from(payload.len())
        .map_err(|_| Error::Unsupported("chunk larger than 4 GiB".into()))?;

    let mut out = Vec::with_capacity(8 + payload.len() + 1);
    out.extend_from_slice(&id);
    out.extend_from_slice(&if big_endian { len.to_be_bytes() } else { len.to_le_bytes() });
    out.extend_from_slice(payload);
    if len & 1 == 1 {
        out.push(0);
    }
    Ok(out)
}

/// Reads a chunk's payload into memory.
pub(crate) fn read_payload(src: &mut File, chunk: &Chunk) -> Result<Vec<u8>> {
    if chunk.size > MAX_METADATA_PAYLOAD {
        return Err(Error::Unsupported(format!(
            "'{}' chunk of {} bytes is too large to rewrite",
            String::from_utf8_lossy(&chunk.id),
            chunk.size
        )));
    }

    src.seek(SeekFrom::Start(chunk.payload_offset()))?;
    let mut buf = vec![0u8; chunk.size as usize];
    src.read_exact(&mut buf)?;
    Ok(buf)
}

/// Builds the ID3v2 tag for `tags` and `visuals`, keeping the unmanaged frames of the tags in the
/// file's existing ID3 chunks. Empty if there is nothing to write.
pub(crate) fn build_id3(
    src: &mut File,
    scan: &Scan,
    tags: &[Tag],
    visuals: &[Visual],
    opts: &WriteOptions,
) -> Result<Vec<u8>> {
    let mut old = Vec::new();

    for chunk in scan.chunks.iter().filter(|c| c.is_id3()) {
        old.push(read_payload(src, chunk)?);
    }

    Ok(serialize_id3(&old, tags, visuals, opts))
}

/// Serializes `tags` and `visuals` as an ID3v2 tag with no padding, keeping the unmanaged frames
/// of the existing tags in `old` (each holding a tag at its start, possibly followed by padding).
/// Empty if there is nothing to write.
pub(crate) fn serialize_id3(
    old: &[Vec<u8>],
    tags: &[Tag],
    visuals: &[Visual],
    opts: &WriteOptions,
) -> Vec<u8> {
    let mut existing = Existing::default();

    for payload in old {
        if let Some(len) = id3w::tag_len(payload).filter(|&len| len <= payload.len()) {
            existing.parse_tag(&payload[..len]);
        }
    }

    let version = match opts.id3v2_version {
        Id3v2Version::V3 => id3w::Version::V3,
        Id3v2Version::V4 => id3w::Version::V4,
        Id3v2Version::Preserve if existing.version() == Some(3) => id3w::Version::V3,
        Id3v2Version::Preserve => id3w::Version::V4,
    };

    let id3_opts = id3w::WriteOptions { version, padding: 0, reuse_len: None };

    id3w::serialize(tags, visuals, &existing, &id3_opts)
}

/// A piece of the rebuilt file.
pub(crate) enum Item {
    /// An unchanged chunk of the original file, by index into [`Scan::chunks`].
    Copy(usize),
    /// A complete new chunk (see [`make_chunk`]).
    New(Vec<u8>),
}

/// Writes the rebuilt file: the 12-byte header (with the corrected size), `items`, then the
/// trailing junk of the original, if any.
pub(crate) fn emit(
    src: &mut File,
    out: &mut impl Write,
    scan: &Scan,
    magic: [u8; 4],
    form: [u8; 4],
    big_endian: bool,
    items: &[Item],
) -> Result<()> {
    let padded = |c: &Chunk| 8 + u64::from(c.size) + u64::from(c.size & 1);

    let body: u64 = 4 + items
        .iter()
        .map(|item| match item {
            Item::Copy(i) => padded(&scan.chunks[*i]),
            Item::New(bytes) => bytes.len() as u64,
        })
        .sum::<u64>();

    let size = u32::try_from(body)
        .map_err(|_| Error::Unsupported("file would exceed the 4 GiB RIFF limit".into()))?;

    out.write_all(&magic)?;
    out.write_all(&if big_endian { size.to_be_bytes() } else { size.to_le_bytes() })?;
    out.write_all(&form)?;

    for item in items {
        match item {
            Item::Copy(i) => {
                let chunk = &scan.chunks[*i];
                let available = chunk.padded_end().min(scan.file_len) - chunk.offset;
                copy_range(src, out, chunk.offset, available)?;
                if available < padded(chunk) {
                    // The last chunk of the file lacked its pad byte, which is now needed.
                    out.write_all(&[0])?;
                }
            }
            Item::New(bytes) => out.write_all(bytes)?,
        }
    }

    copy_range(src, out, scan.end, scan.file_len - scan.end)
}

/// Splits the sub-chunks of an INFO list payload (after the `INFO` form type), calling `f` with
/// the id and the complete sub-chunk (header, payload, pad byte). A truncated sub-chunk ends the
/// walk.
fn for_each_info_chunk(mut buf: &[u8], mut f: impl FnMut([u8; 4], Vec<u8>)) {
    while buf.len() >= 8 {
        let id = [buf[0], buf[1], buf[2], buf[3]];
        let len = read_u32(&buf[4..8], false) as usize;

        if len > buf.len() - 8 {
            break;
        }

        let mut chunk = buf[..8 + len].to_vec();
        if len & 1 == 1 {
            chunk.push(0);
        }
        f(id, chunk);

        buf = &buf[(8 + len + (len & 1)).min(buf.len())..];
    }
}

pub(crate) fn write(
    path: &Path,
    tags: &[Tag],
    visuals: &[Visual],
    opts: &WriteOptions,
) -> Result<()> {
    replace_file(path, |src, out| {
        // RF64 stores sizes out of band (ds64), so its chunk sizes cannot be walked as RIFF.
        let mut marker = [0u8; 4];
        read_up_to(src, &mut marker)?;
        if &marker == b"RF64" || &marker == b"BW64" {
            return Err(Error::Unsupported("RF64/BW64 files are not writable".into()));
        }

        let (magic, form, scan) = scan(src, false)?;

        if &magic != b"RIFF" {
            return Err(Error::Unsupported("not a RIFF file".into()));
        }

        if &form != b"WAVE" {
            return Err(Error::Unsupported("RIFF form is not WAVE".into()));
        }

        // Find the INFO lists, and keep their unmanaged sub-chunks.
        let mut is_info = vec![false; scan.chunks.len()];
        let mut unmanaged: Vec<u8> = Vec::new();

        for (i, chunk) in scan.chunks.iter().enumerate() {
            if &chunk.id != b"LIST" || chunk.size < 4 {
                continue;
            }

            let mut form = [0u8; 4];
            src.seek(SeekFrom::Start(chunk.payload_offset()))?;
            src.read_exact(&mut form)?;

            if &form != b"INFO" {
                continue;
            }

            is_info[i] = true;

            let payload = read_payload(src, chunk)?;
            for_each_info_chunk(&payload[4..], |id, sub_chunk| {
                if !is_managed_info_id(id) {
                    unmanaged.extend_from_slice(&sub_chunk);
                }
            });
        }

        let mut info_payload = serialize_info(tags);
        info_payload.extend_from_slice(&unmanaged);

        let info_chunk = if info_payload.is_empty() {
            None
        }
        else {
            let mut list = b"INFO".to_vec();
            list.extend_from_slice(&info_payload);
            Some(make_chunk(*b"LIST", &list, false)?)
        };

        let id3 = build_id3(src, &scan, tags, visuals, opts)?;
        let id3_chunk = if id3.is_empty() { None } else { Some(make_chunk(*b"id3 ", &id3, false)?) };

        // Replace the first INFO list and the first ID3 chunk in place, drop the others, and add
        // the new chunks after the last chunk if the file had none.
        let mut info_chunk = info_chunk;
        let mut id3_chunk = id3_chunk;
        let mut seen_info = false;
        let mut seen_id3 = false;
        let mut items = Vec::with_capacity(scan.chunks.len() + 2);

        for (i, chunk) in scan.chunks.iter().enumerate() {
            if is_info[i] {
                if !std::mem::replace(&mut seen_info, true) {
                    items.extend(info_chunk.take().map(Item::New));
                }
            }
            else if chunk.is_id3() {
                if !std::mem::replace(&mut seen_id3, true) {
                    items.extend(id3_chunk.take().map(Item::New));
                }
            }
            else {
                items.push(Item::Copy(i));
            }
        }

        items.extend(info_chunk.map(Item::New));
        items.extend(id3_chunk.map(Item::New));

        emit(src, out, &scan, magic, form, false, &items)
    })
}
