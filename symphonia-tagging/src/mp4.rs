// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! MP4 / ISO base media files (`.m4a`, `.m4b`, `.mp4`).
//!
//! The tags live in `moov/udta/meta/ilst`. A write rebuilds that atom (creating `udta`, `meta` and
//! the `mdir` handler when absent) with [`symphonia_format_isomp4::writer::build_ilst`], keeps every
//! other box of the file as it is, and removes tag copies the reader would otherwise also see
//! (`ID32` boxes, and `ilst`/`ID32` inside a top-level `meta`).
//!
//! The `moov` box and the `free`/`skip` boxes right behind it form the *region* the new `moov` is
//! written to:
//!
//! * if the new `moov` fits the region, with either no room to spare or room for a `free` box, and
//!   the room is not much more than [`WriteOptions::padding`], the region keeps its size and the
//!   room is filled with a `free` box, so nothing after it moves;
//! * otherwise the region becomes the new `moov` plus `padding` bytes of `free`, unless nothing
//!   follows it (`moov` at the end of the file), where no padding is needed.
//!
//! When the region changes size, every chunk offset that points behind it is shifted: the
//! `stco`/`co64` tables of every `trak` (`stco` tables are upgraded to `co64` when an offset no
//! longer fits 32 bits), `saio` offsets, and the `moof` offsets of a `tfra` in `mfra`. Fragmented
//! files whose `tfhd` boxes carry an explicit base data offset, and `sidx` boxes in front of the
//! region, cannot be shifted and are reported as unsupported.
//!
//! Boxes are copied from the original in chunks; only `moov` (and the tiny boxes that carry
//! offsets or tags) is held in memory.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use symphonia_core::meta::{Tag, Visual};
use symphonia_format_isomp4::writer::{build_ilst, mdir_hdlr_box};

use crate::{Error, Result, WriteOptions, copy_range, read_at, read_up_to, replace_file};

const MOOV: [u8; 4] = *b"moov";
const TRAK: [u8; 4] = *b"trak";
const MDIA: [u8; 4] = *b"mdia";
const MINF: [u8; 4] = *b"minf";
const STBL: [u8; 4] = *b"stbl";
const UDTA: [u8; 4] = *b"udta";
const META: [u8; 4] = *b"meta";
const HDLR: [u8; 4] = *b"hdlr";
const ILST: [u8; 4] = *b"ilst";
const ID32: [u8; 4] = *b"ID32";
const FREE: [u8; 4] = *b"free";
const SKIP: [u8; 4] = *b"skip";
const STCO: [u8; 4] = *b"stco";
const CO64: [u8; 4] = *b"co64";
const SAIO: [u8; 4] = *b"saio";
const MOOF: [u8; 4] = *b"moof";
const TRAF: [u8; 4] = *b"traf";
const TFHD: [u8; 4] = *b"tfhd";
const MFRA: [u8; 4] = *b"mfra";
const TFRA: [u8; 4] = *b"tfra";
const SIDX: [u8; 4] = *b"sidx";

pub(crate) fn write(
    path: &Path,
    tags: &[Tag],
    visuals: &[Visual],
    opts: &WriteOptions,
) -> Result<()> {
    replace_file(path, |src, out| rewrite(src, out, tags, visuals, opts))
}

/// A box at the top level of the file.
#[derive(Clone, Copy, Debug)]
struct TopBox {
    start: u64,
    end: u64,
    header_len: u64,
    kind: [u8; 4],
}

impl TopBox {
    fn payload_len(&self) -> u64 {
        self.end - self.start - self.header_len
    }
}

/// A box of the tree that is held in memory.
#[derive(Debug)]
struct Node {
    kind: [u8; 4],
    body: Body,
}

#[derive(Debug)]
enum Body {
    /// A box whose payload is opaque.
    Leaf(Vec<u8>),
    /// A box that holds boxes, after `prefix` (the version and flags of a full box).
    Container { prefix: Vec<u8>, children: Vec<Node> },
    /// Bytes behind the last box of a parent that are too few to be a box (QuickTime ends a `udta`
    /// with a 32-bit zero).
    Trailer(Vec<u8>),
}

impl Node {
    fn leaf(kind: [u8; 4], payload: Vec<u8>) -> Node {
        Node { kind, body: Body::Leaf(payload) }
    }

    fn container(kind: [u8; 4], prefix: Vec<u8>, children: Vec<Node>) -> Node {
        Node { kind, body: Body::Container { prefix, children } }
    }
}

/// A part of the old file replaced by new bytes: `[old_start, old_end)` is now `new_len` long.
#[derive(Clone, Copy, Debug)]
struct Edit {
    old_start: u64,
    old_end: u64,
    new_len: u64,
}

impl Edit {
    fn changes_size(&self) -> bool {
        self.new_len != self.old_end - self.old_start
    }
}

/// Where a chunk offset of the old file is in the new file.
fn map_offset(edits: &[Edit], offset: u64) -> Result<u64> {
    let mut shifted = i128::from(offset);

    for edit in edits.iter().filter(|e| e.old_end <= offset) {
        shifted += i128::from(edit.new_len) - i128::from(edit.old_end - edit.old_start);
    }

    u64::try_from(shifted)
        .map_err(|_| Error::Malformed(format!("chunk offset {offset} is invalid")))
}

fn name(kind: &[u8; 4]) -> String {
    kind.iter().map(|&b| char::from(b)).collect()
}

/// Reads the sizes of the boxes of the file, front to back, without reading their payloads.
///
/// Also returns the offset of the first byte behind the last box: the file may end with fewer than
/// 8 bytes (padding) that make no box.
fn scan_top_level(src: &mut File, len: u64) -> Result<(Vec<TopBox>, u64)> {
    let mut boxes = Vec::new();
    let mut pos = 0u64;

    while len - pos >= 8 {
        src.seek(SeekFrom::Start(pos))?;

        let mut head = [0u8; 16];
        let n = read_up_to(src, &mut head)?;

        let size32 = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
        let kind = [head[4], head[5], head[6], head[7]];

        let (header_len, size) = match size32 {
            // The box extends to the end of the file.
            0 => (8, len - pos),
            1 if n >= 16 => {
                let mut be = [0u8; 8];
                be.copy_from_slice(&head[8..16]);
                (16, u64::from_be_bytes(be))
            }
            1 => return Err(Error::Malformed(format!("box '{}' is cut short", name(&kind)))),
            size => (8, u64::from(size)),
        };

        if size < header_len {
            return Err(Error::Malformed(format!(
                "box '{}' at {pos} is smaller than its header",
                name(&kind)
            )));
        }

        // A last box that claims more than the file holds ends with the file.
        let end = pos.saturating_add(size).min(len);

        boxes.push(TopBox { start: pos, end, header_len, kind });
        pos = end;
    }

    Ok((boxes, pos))
}

fn read_payload(src: &mut File, top: &TopBox) -> Result<Vec<u8>> {
    read_at(src, top.start + top.header_len, top.payload_len())
}

/// Reads the header of the box at the start of `buf`, which ends where its parent does (so that a
/// box with a size of 0 extends to the end of `buf`). Returns the box's kind, the length of its
/// header and its total size.
fn parse_header(buf: &[u8]) -> Result<([u8; 4], usize, usize)> {
    let size32 = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let kind = [buf[4], buf[5], buf[6], buf[7]];

    let (header_len, size) = match size32 {
        0 => (8, buf.len() as u64),
        1 => {
            if buf.len() < 16 {
                return Err(Error::Malformed(format!("box '{}' is cut short", name(&kind))));
            }
            let mut be = [0u8; 8];
            be.copy_from_slice(&buf[8..16]);
            (16, u64::from_be_bytes(be))
        }
        size => (8, u64::from(size)),
    };

    if size < header_len as u64 || size > buf.len() as u64 {
        return Err(Error::Malformed(format!("box '{}' does not fit its parent", name(&kind))));
    }

    Ok((kind, header_len, size as usize))
}

/// How the payload of `kind` inside `parent` is split into boxes: the length of the full box
/// header it starts with, if it holds boxes at all. Only the boxes between `moov` and the sample
/// tables, and the metadata boxes, are interpreted.
fn container_prefix(kind: &[u8; 4], parent: &[u8; 4]) -> Option<usize> {
    match (parent, kind) {
        (&MOOV, &TRAK) | (&MOOV, &UDTA) | (&TRAK, &MDIA) | (&MDIA, &MINF) | (&MINF, &STBL) => {
            Some(0)
        }
        (&UDTA, &META) => Some(4),
        _ => None,
    }
}

fn parse_children(buf: &[u8], parent: &[u8; 4]) -> Result<Vec<Node>> {
    let mut nodes = Vec::new();
    let mut pos = 0usize;

    while pos < buf.len() {
        let rest = &buf[pos..];

        if rest.len() < 8 {
            nodes.push(Node { kind: [0; 4], body: Body::Trailer(rest.to_vec()) });
            break;
        }

        let (kind, header_len, size) = parse_header(rest)?;
        let payload = &rest[header_len..size];

        let node = match container_prefix(&kind, parent) {
            Some(prefix_len) if payload.len() >= prefix_len => Node::container(
                kind,
                payload[..prefix_len].to_vec(),
                parse_children(&payload[prefix_len..], &kind)?,
            ),
            _ => Node::leaf(kind, payload.to_vec()),
        };

        nodes.push(node);
        pos += size;
    }

    Ok(nodes)
}

/// The length of a box with a payload of `payload` bytes.
fn box_len(payload: u64) -> u64 {
    if payload + 8 <= u64::from(u32::MAX) { payload + 8 } else { payload + 16 }
}

fn node_len(node: &Node) -> u64 {
    match &node.body {
        Body::Trailer(bytes) => bytes.len() as u64,
        Body::Leaf(payload) => box_len(payload.len() as u64),
        Body::Container { prefix, children } => {
            box_len(prefix.len() as u64 + children.iter().map(node_len).sum::<u64>())
        }
    }
}

/// Starts a box: writes its header with a placeholder size and returns its position.
fn begin_box(out: &mut Vec<u8>, kind: &[u8; 4]) -> usize {
    let at = out.len();
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(kind);
    at
}

/// Finishes the box started at `at`: patches in its size, switching to a 64-bit size if the box
/// needs it.
fn end_box(out: &mut Vec<u8>, at: usize) {
    let size = (out.len() - at) as u64;

    match u32::try_from(size) {
        Ok(size) => out[at..at + 4].copy_from_slice(&size.to_be_bytes()),
        Err(_) => {
            out[at..at + 4].copy_from_slice(&1u32.to_be_bytes());
            out.splice(at + 8..at + 8, (size + 8).to_be_bytes());
        }
    }
}

fn write_node(out: &mut Vec<u8>, node: &Node) {
    match &node.body {
        Body::Trailer(bytes) => out.extend_from_slice(bytes),
        Body::Leaf(payload) => {
            let at = begin_box(out, &node.kind);
            out.extend_from_slice(payload);
            end_box(out, at);
        }
        Body::Container { prefix, children } => {
            let at = begin_box(out, &node.kind);
            out.extend_from_slice(prefix);
            for child in children {
                write_node(out, child);
            }
            end_box(out, at);
        }
    }
}

/// What the `hdlr` box of a `meta` box says it holds.
enum Handler {
    /// iTunes-style `ilst` metadata (or no handler at all).
    Mdir,
    /// ID3v2 tags in `ID32` boxes.
    Id32,
    Other([u8; 4]),
}

fn meta_handler(children: &[Node]) -> Handler {
    for child in children {
        if let (HDLR, Body::Leaf(payload)) = (child.kind, &child.body) {
            if payload.len() >= 12 {
                return match &payload[8..12] {
                    b"mdir" => Handler::Mdir,
                    b"ID32" => Handler::Id32,
                    other => Handler::Other([other[0], other[1], other[2], other[3]]),
                };
            }
        }
    }

    Handler::Mdir
}

/// What is removed from the file's metadata boxes, to be written anew.
#[derive(Default)]
struct Removed {
    /// The payloads of the removed `ilst` boxes, back to back.
    ilst: Vec<u8>,
    /// The `hdlr` box of the first removed iTunes-style `meta` box.
    hdlr: Option<Node>,
    /// The version and flags of that `meta` box.
    prefix: Option<Vec<u8>>,
    /// Boxes of the removed `meta` boxes that are not tags and are kept (anything but `hdlr`,
    /// `ilst`, `ID32` and padding).
    kept: Vec<Node>,
}

impl Removed {
    /// Takes the tags out of the children of a `meta` box of the `mdir` kind. `ID32`, `free`
    /// and `skip` boxes are dropped.
    fn take_mdir_meta(&mut self, prefix: Vec<u8>, children: Vec<Node>) {
        self.prefix.get_or_insert(prefix);

        for child in children {
            match (child.kind, child.body) {
                (ILST, Body::Leaf(payload)) => self.ilst.extend_from_slice(&payload),
                (ID32 | FREE | SKIP, _) => (),
                (HDLR, body) => {
                    if self.hdlr.is_none() {
                        self.hdlr = Some(Node { kind: HDLR, body });
                    }
                }
                (kind, body) => self.kept.push(Node { kind, body }),
            }
        }
    }
}

/// Removes the tags from the children of a `udta` box: `ID32` boxes and `meta` boxes (the one
/// that holds `ilst` is rebuilt later). Returns where the first removed box was.
fn strip_udta(children: &mut Vec<Node>, removed: &mut Removed) -> Result<Option<usize>> {
    let mut kept = Vec::with_capacity(children.len());
    let mut first = None;

    for node in std::mem::take(children) {
        match (node.kind, node.body) {
            (ID32, _) => {
                first.get_or_insert(kept.len());
            }
            (META, Body::Container { prefix, children: kids }) => match meta_handler(&kids) {
                Handler::Mdir => {
                    first.get_or_insert(kept.len());
                    removed.take_mdir_meta(prefix, kids);
                }
                Handler::Id32 => {
                    first.get_or_insert(kept.len());
                }
                Handler::Other(handler) => {
                    return Err(Error::Unsupported(format!(
                        "the '{}' metadata handler of the user data",
                        name(&handler)
                    )));
                }
            },
            (kind, body) => kept.push(Node { kind, body }),
        }
    }

    *children = kept;
    Ok(first)
}

/// Builds the `meta` box that holds the new `ilst`, or `None` if there is nothing to store.
fn build_meta(removed: &mut Removed, ilst: Option<Vec<u8>>) -> Option<Node> {
    if ilst.is_none() && removed.kept.is_empty() {
        return None;
    }

    let hdlr =
        removed.hdlr.take().unwrap_or_else(|| Node::leaf(HDLR, mdir_hdlr_box()[8..].to_vec()));

    let mut children = vec![hdlr];
    children.append(&mut removed.kept);

    if let Some(ilst) = ilst {
        // The serializer writes a 32-bit header.
        children.push(Node::leaf(ILST, ilst[8..].to_vec()));
    }

    Some(Node::container(META, removed.prefix.take().unwrap_or_else(|| vec![0; 4]), children))
}

/// Takes the tags out of a `meta` box at the top level of the file. Returns the bytes that replace
/// the box (none: the box is removed), or `None` if the box holds no tags and stays as it is.
fn strip_top_level_meta(
    src: &mut File,
    top: &TopBox,
    removed: &mut Removed,
) -> Result<Option<Option<Vec<u8>>>> {
    let payload = read_payload(src, top)?;

    if payload.len() < 4 {
        return Ok(None);
    }

    let mut children = parse_children(&payload[4..], &META)?;

    if matches!(meta_handler(&children), Handler::Other(_)) {
        return Ok(None);
    }

    if !children.iter().any(|child| matches!(child.kind, ILST | ID32)) {
        return Ok(None);
    }

    let mut kept = Vec::new();
    for child in std::mem::take(&mut children) {
        match (child.kind, child.body) {
            (ILST, Body::Leaf(ilst)) => removed.ilst.extend_from_slice(&ilst),
            (ID32 | FREE | SKIP, _) => (),
            (kind, body) => kept.push(Node { kind, body }),
        }
    }

    // Only the handler is left: the box carries nothing.
    if kept.iter().all(|child| child.kind == HDLR) {
        return Ok(Some(None));
    }

    let mut bytes = Vec::new();
    write_node(&mut bytes, &Node::container(META, payload[..4].to_vec(), kept));

    Ok(Some(Some(bytes)))
}

/// Calls `f` on every box that holds file offsets in the sample tables of every track.
fn visit_offset_boxes(
    nodes: &mut [Node],
    f: &mut dyn FnMut(&mut Node) -> Result<()>,
) -> Result<()> {
    for node in nodes {
        match (node.kind, &mut node.body) {
            (TRAK | MDIA | MINF, Body::Container { children, .. }) => {
                visit_offset_boxes(children, f)?
            }
            (STBL, Body::Container { children, .. }) => {
                for child in children {
                    if matches!(child.kind, STCO | CO64 | SAIO) {
                        f(child)?;
                    }
                }
            }
            _ => (),
        }
    }

    Ok(())
}

fn be_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

fn be_u64(buf: &[u8], at: usize) -> u64 {
    let mut be = [0u8; 8];
    be.copy_from_slice(&buf[at..at + 8]);
    u64::from_be_bytes(be)
}

/// The number of `width` byte entries a table whose count is at `count_at` holds, after checking
/// that they fit.
fn table_len(payload: &[u8], count_at: usize, width: usize, kind: &[u8; 4]) -> Result<usize> {
    let malformed = || Error::Malformed(format!("'{}' box is cut short", name(kind)));

    if payload.len() < count_at + 4 {
        return Err(malformed());
    }

    let count = be_u32(payload, count_at) as usize;

    match count.checked_mul(width).and_then(|n| n.checked_add(count_at + 4)) {
        Some(end) if end <= payload.len() => Ok(count),
        _ => Err(malformed()),
    }
}

/// Moves the offsets in `node` (an `stco`, `co64` or `saio` box). Returns whether an `stco` offset
/// no longer fits 32 bits. Nothing is changed if `dry_run`.
fn shift_offsets(node: &mut Node, edits: &[Edit], dry_run: bool) -> Result<bool> {
    let Body::Leaf(payload) = &mut node.body
    else {
        return Ok(false);
    };

    let mut overflow = false;

    match node.kind {
        STCO => {
            let count = table_len(payload, 4, 4, &STCO)?;

            for i in 0..count {
                let at = 8 + 4 * i;
                let new = map_offset(edits, u64::from(be_u32(payload, at)))?;

                match u32::try_from(new) {
                    Ok(new) if !dry_run => payload[at..at + 4].copy_from_slice(&new.to_be_bytes()),
                    Ok(_) => (),
                    Err(_) => overflow = true,
                }
            }
        }
        CO64 => {
            let count = table_len(payload, 4, 8, &CO64)?;

            for i in 0..count {
                let at = 8 + 8 * i;
                let new = map_offset(edits, be_u64(payload, at))?;

                if !dry_run {
                    payload[at..at + 8].copy_from_slice(&new.to_be_bytes());
                }
            }
        }
        SAIO => {
            // Version 0 holds 32-bit offsets, version 1 64-bit ones. With the flag set, an
            // auxiliary information type precedes the count.
            if payload.len() < 4 {
                return Err(Error::Malformed("'saio' box is cut short".into()));
            }

            let wide = payload[0] != 0;
            let count_at = if payload[3] & 1 != 0 { 12 } else { 4 };
            let width = if wide { 8 } else { 4 };
            let count = table_len(payload, count_at, width, &SAIO)?;

            for i in 0..count {
                let at = count_at + 4 + width * i;

                let old = if wide { be_u64(payload, at) } else { u64::from(be_u32(payload, at)) };
                let new = map_offset(edits, old)?;

                if wide {
                    if !dry_run {
                        payload[at..at + 8].copy_from_slice(&new.to_be_bytes());
                    }
                }
                else {
                    let new = u32::try_from(new).map_err(|_| {
                        Error::Unsupported(
                            "sample auxiliary information offsets beyond 4 GiB".into(),
                        )
                    })?;

                    if !dry_run {
                        payload[at..at + 4].copy_from_slice(&new.to_be_bytes());
                    }
                }
            }
        }
        _ => (),
    }

    Ok(overflow)
}

/// Turns every `stco` box into a `co64` box.
fn upgrade_stco(nodes: &mut [Node]) -> Result<()> {
    visit_offset_boxes(nodes, &mut |node| {
        if node.kind != STCO {
            return Ok(());
        }

        if let Body::Leaf(payload) = &mut node.body {
            let count = table_len(payload, 4, 4, &STCO)?;

            let mut wide = Vec::with_capacity(8 + 8 * count);
            wide.extend_from_slice(&payload[..8]);

            for i in 0..count {
                wide.extend_from_slice(&u64::from(be_u32(payload, 8 + 4 * i)).to_be_bytes());
            }

            *payload = wide;
            node.kind = CO64;
        }

        Ok(())
    })
}

/// A box found in the payload of another box: its kind and the range of its payload.
struct Child {
    kind: [u8; 4],
    body: std::ops::Range<usize>,
}

/// The boxes in `buf`, a box payload.
fn child_boxes(buf: &[u8]) -> Result<Vec<Child>> {
    let mut found = Vec::new();
    let mut pos = 0usize;

    while buf.len() - pos >= 8 {
        let (kind, header_len, size) = parse_header(&buf[pos..])?;
        found.push(Child { kind, body: pos + header_len..pos + size });
        pos += size;
    }

    Ok(found)
}

/// Whether any track fragment of a `moof` payload gives its data offsets relative to an explicit
/// base data offset (an absolute file position) in its `tfhd`.
fn moof_has_explicit_base(payload: &[u8]) -> Result<bool> {
    for fragment in child_boxes(payload)? {
        if fragment.kind != TRAF {
            continue;
        }

        let traf = &payload[fragment.body];

        for header in child_boxes(traf)? {
            // Version 0, flags `base-data-offset-present` = 0x1.
            let flags_at = header.body.start + 3;
            if header.kind == TFHD && header.body.len() >= 4 && traf[flags_at] & 1 != 0 {
                return Ok(true);
            }
        }
    }

    Ok(false)
}

/// Moves the `moof` offsets of the `tfra` boxes of an `mfra` payload.
fn shift_tfra(payload: &mut [u8], edits: &[Edit]) -> Result<()> {
    for child in child_boxes(payload)? {
        if child.kind != TFRA {
            continue;
        }

        let tfra = &mut payload[child.body];
        let malformed = || Error::Malformed("'tfra' box is cut short".into());

        if tfra.len() < 16 {
            return Err(malformed());
        }

        let wide = tfra[0] != 0;
        let lengths = be_u32(tfra, 8);
        let count = be_u32(tfra, 12) as usize;

        // Each entry: time and moof offset (both 32 or 64 bits), and three numbers of 1 to 4
        // bytes.
        let numbers = (((lengths >> 4) & 3) + ((lengths >> 2) & 3) + (lengths & 3) + 3) as usize;
        let field = if wide { 8 } else { 4 };
        let entry_len = 2 * field + numbers;

        match count.checked_mul(entry_len).and_then(|n| n.checked_add(16)) {
            Some(len) if len <= tfra.len() => (),
            _ => return Err(malformed()),
        }

        for i in 0..count {
            let at = 16 + i * entry_len + field;

            if wide {
                let new = map_offset(edits, be_u64(tfra, at))?;
                tfra[at..at + 8].copy_from_slice(&new.to_be_bytes());
            }
            else {
                let new = u32::try_from(map_offset(edits, u64::from(be_u32(tfra, at)))?)
                    .map_err(|_| Error::Unsupported("fragment offsets beyond 4 GiB".into()))?;
                tfra[at..at + 4].copy_from_slice(&new.to_be_bytes());
            }
        }
    }

    Ok(())
}

/// A part of the new file.
enum Piece {
    /// A range of the old file, as it is.
    Copy {
        start: u64,
        end: u64,
    },
    Bytes(Vec<u8>),
    /// A `free` box of this many bytes.
    Free(u64),
}

/// Adds a range of the old file, joining it with the range it directly follows.
fn keep_range(pieces: &mut Vec<Piece>, start: u64, end: u64) {
    match pieces.last_mut() {
        Some(Piece::Copy { end: last_end, .. }) if *last_end == start => *last_end = end,
        _ => pieces.push(Piece::Copy { start, end }),
    }
}

/// Writes a `free` box of `total` bytes (at least 8).
fn write_free(out: &mut impl Write, total: u64) -> Result<()> {
    let zeros = [0u8; 4096];

    let mut left = match u32::try_from(total) {
        Ok(size) => {
            out.write_all(&size.to_be_bytes())?;
            out.write_all(&FREE)?;
            total - 8
        }
        Err(_) => {
            out.write_all(&1u32.to_be_bytes())?;
            out.write_all(&FREE)?;
            out.write_all(&total.to_be_bytes())?;
            total - 16
        }
    };

    while left > 0 {
        let n = left.min(zeros.len() as u64) as usize;
        out.write_all(&zeros[..n])?;
        left -= n as u64;
    }

    Ok(())
}

fn rewrite(
    src: &mut File,
    out: &mut impl Write,
    tags: &[Tag],
    visuals: &[Visual],
    opts: &WriteOptions,
) -> Result<()> {
    let file_len = src.metadata()?.len();
    let (tops, tail_start) = scan_top_level(src, file_len)?;

    let moov_index = tops
        .iter()
        .position(|b| b.kind == MOOV)
        .ok_or_else(|| Error::Malformed("the file has no moov box".into()))?;
    let moov = tops[moov_index];

    // The region the new moov is written to: moov and the padding boxes behind it.
    let padding_boxes =
        tops[moov_index + 1..].iter().take_while(|b| b.kind == FREE || b.kind == SKIP).count();
    let region_end = tops[moov_index + padding_boxes].end;
    let region_len = region_end - moov.start;
    let data_follows = region_end < file_len;

    // Take the tags out of the movie and out of the top level.
    let mut moov_nodes = parse_children(&read_payload(src, &moov)?, &MOOV)?;
    let mut removed = Removed::default();
    let mut target = None;

    for (i, node) in moov_nodes.iter_mut().enumerate() {
        if let (UDTA, Body::Container { children, .. }) = (node.kind, &mut node.body) {
            let at = strip_udta(children, &mut removed)?;

            if target.is_none() {
                target = Some((i, at.unwrap_or(children.len())));
            }
        }
    }

    // A replacement (or removal, `None`) for each top-level `meta` box that holds tags.
    let mut top_meta: Vec<(usize, Option<Vec<u8>>)> = Vec::new();

    for (i, top) in tops.iter().enumerate().filter(|(_, b)| b.kind == META) {
        if let Some(replacement) = strip_top_level_meta(src, top, &mut removed)? {
            top_meta.push((i, replacement));
        }
    }

    // Build the new tags and put them in.
    let ilst = build_ilst(&removed.ilst, tags, visuals)?;

    if let Some(meta) = build_meta(&mut removed, ilst) {
        match target {
            Some((i, at)) => {
                if let Body::Container { children, .. } = &mut moov_nodes[i].body {
                    children.insert(at, meta);
                }
            }
            None => moov_nodes.push(Node::container(UDTA, Vec::new(), vec![meta])),
        }
    }

    moov_nodes.retain(|node| {
        !matches!((node.kind, &node.body), (UDTA, Body::Container { children, .. }) if children.is_empty())
    });

    // The size of the region: the new moov, and padding.
    let padding = match opts.padding {
        0 => 0,
        n => (n as u64).max(8),
    };

    let region_for = |moov_len: u64| -> u64 {
        if !data_follows {
            return moov_len;
        }

        match region_len.checked_sub(moov_len) {
            // It fits, and the room is a whole `free` box (or none) that is not a waste.
            Some(room) if (room == 0 || room >= 8) && room <= 2 * padding => region_len,
            _ => moov_len + padding,
        }
    };

    let top_meta_edits: Vec<Edit> = top_meta
        .iter()
        .map(|(i, replacement)| Edit {
            old_start: tops[*i].start,
            old_end: tops[*i].end,
            new_len: replacement.as_ref().map_or(0, |bytes| bytes.len() as u64),
        })
        .collect();

    let mut upgraded = false;

    let (moov_len, new_region_len, edits) = loop {
        let moov_len = box_len(moov_nodes.iter().map(node_len).sum::<u64>());
        let new_region_len = region_for(moov_len);

        let mut edits = top_meta_edits.clone();
        edits.push(Edit { old_start: moov.start, old_end: region_end, new_len: new_region_len });

        let changed = edits.iter().any(Edit::changes_size);

        // 32-bit chunk offsets that no longer fit need 64-bit tables, which are longer.
        if changed && !upgraded {
            let mut overflow = false;

            visit_offset_boxes(&mut moov_nodes, &mut |node| {
                overflow |= shift_offsets(node, &edits, true)?;
                Ok(())
            })?;

            if overflow {
                upgrade_stco(&mut moov_nodes)?;
                upgraded = true;
                continue;
            }
        }

        break (moov_len, new_region_len, edits);
    };

    let first_change = edits.iter().filter(|e| e.changes_size()).map(|e| e.old_start).min();

    // The patched payload of each `mfra` box; its header stays as it is.
    let mut mfra_patches: Vec<(usize, Vec<u8>)> = Vec::new();

    if let Some(first_change) = first_change {
        visit_offset_boxes(&mut moov_nodes, &mut |node| {
            shift_offsets(node, &edits, false).map(|_| ())
        })?;

        for (i, top) in tops.iter().enumerate() {
            match top.kind {
                // The segment index references the data behind it by distance.
                SIDX if top.start < first_change => {
                    return Err(Error::Unsupported(
                        "a segment index in front of the rewritten boxes".into(),
                    ));
                }
                MOOF if top.start >= first_change => {
                    if moof_has_explicit_base(&read_payload(src, top)?)? {
                        return Err(Error::Unsupported(
                            "fragments with an explicit base data offset".into(),
                        ));
                    }
                }
                MFRA => {
                    let mut payload = read_payload(src, top)?;
                    shift_tfra(&mut payload, &edits)?;
                    mfra_patches.push((i, payload));
                }
                _ => (),
            }
        }
    }

    let mut moov_bytes = Vec::with_capacity(moov_len as usize);
    let at = begin_box(&mut moov_bytes, &MOOV);
    for node in &moov_nodes {
        write_node(&mut moov_bytes, node);
    }
    end_box(&mut moov_bytes, at);

    debug_assert_eq!(moov_bytes.len() as u64, moov_len);

    // Lay the new file out.
    let mut pieces: Vec<Piece> = Vec::new();

    let mut i = 0;

    while i < tops.len() {
        let top = &tops[i];

        if i == moov_index {
            pieces.push(Piece::Bytes(std::mem::take(&mut moov_bytes)));

            if new_region_len > moov_len {
                pieces.push(Piece::Free(new_region_len - moov_len));
            }

            i += 1 + padding_boxes;
            continue;
        }

        if let Some((_, replacement)) = top_meta.iter().find(|(m, _)| *m == i) {
            if let Some(bytes) = replacement {
                pieces.push(Piece::Bytes(bytes.clone()));
            }
        }
        else if let Some((_, payload)) = mfra_patches.iter().find(|(m, _)| *m == i) {
            keep_range(&mut pieces, top.start, top.start + top.header_len);
            pieces.push(Piece::Bytes(payload.clone()));
        }
        else {
            keep_range(&mut pieces, top.start, top.end);
        }

        i += 1;
    }

    if tail_start < file_len {
        pieces.push(Piece::Copy { start: tail_start, end: file_len });
    }

    for piece in pieces {
        match piece {
            Piece::Copy { start, end } => copy_range(src, out, start, end - start)?,
            Piece::Bytes(bytes) => out.write_all(&bytes)?,
            Piece::Free(total) => write_free(out, total)?,
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stco(offsets: &[u32]) -> Node {
        let mut payload = vec![0; 4];
        payload.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
        for offset in offsets {
            payload.extend_from_slice(&offset.to_be_bytes());
        }
        Node::leaf(STCO, payload)
    }

    fn co64(offsets: &[u64]) -> Node {
        let mut payload = vec![0; 4];
        payload.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
        for offset in offsets {
            payload.extend_from_slice(&offset.to_be_bytes());
        }
        Node::leaf(CO64, payload)
    }

    /// `trak/mdia/minf/stbl/<table>`
    fn trak(table: Node) -> Node {
        let stbl = Node::container(STBL, Vec::new(), vec![table]);
        let minf = Node::container(MINF, Vec::new(), vec![stbl]);
        let mdia = Node::container(MDIA, Vec::new(), vec![minf]);
        Node::container(TRAK, Vec::new(), vec![mdia])
    }

    fn table_of(trak: &Node) -> &Node {
        let Body::Container { children: mdia, .. } = &trak.body
        else {
            panic!()
        };
        let Body::Container { children: minf, .. } = &mdia[0].body
        else {
            panic!()
        };
        let Body::Container { children: stbl, .. } = &minf[0].body
        else {
            panic!()
        };
        let Body::Container { children, .. } = &stbl[0].body
        else {
            panic!()
        };
        &children[0]
    }

    fn entries(node: &Node) -> Vec<u64> {
        let Body::Leaf(payload) = &node.body
        else {
            panic!()
        };
        match node.kind {
            STCO => (0..be_u32(payload, 4) as usize)
                .map(|i| u64::from(be_u32(payload, 8 + 4 * i)))
                .collect(),
            CO64 => (0..be_u32(payload, 4) as usize).map(|i| be_u64(payload, 8 + 8 * i)).collect(),
            _ => panic!(),
        }
    }

    fn shift_all(nodes: &mut [Node], edits: &[Edit], dry_run: bool) -> Result<bool> {
        let mut overflow = false;
        visit_offset_boxes(nodes, &mut |node| {
            overflow |= shift_offsets(node, edits, dry_run)?;
            Ok(())
        })?;
        Ok(overflow)
    }

    #[test]
    fn offsets_move_by_the_edits_in_front_of_them() {
        let edits = [
            Edit { old_start: 100, old_end: 200, new_len: 150 },
            Edit { old_start: 400, old_end: 500, new_len: 0 },
            // The same size: nothing moves.
            Edit { old_start: 600, old_end: 700, new_len: 100 },
        ];

        assert_eq!(map_offset(&edits, 50).unwrap(), 50);
        // Inside an edit: that edit does not count.
        assert_eq!(map_offset(&edits, 150).unwrap(), 150);
        assert_eq!(map_offset(&edits, 200).unwrap(), 250);
        assert_eq!(map_offset(&edits, 399).unwrap(), 449);
        assert_eq!(map_offset(&edits, 500).unwrap(), 450);
        assert_eq!(map_offset(&edits, 10_000).unwrap(), 9_950);

        // Before the start of the file.
        let edits = [Edit { old_start: 0, old_end: 100, new_len: 0 }];
        assert!(map_offset(&edits, 10).is_ok());
    }

    #[test]
    fn stco_overflow_is_detected_and_the_table_widens_to_co64() {
        let near_the_end = u32::MAX - 10;
        let edits = [Edit { old_start: 0, old_end: 50, new_len: 5000 }];

        let mut nodes = vec![trak(stco(&[100, near_the_end])), trak(co64(&[7_000_000_000]))];
        let old_len: u64 = nodes.iter().map(node_len).sum();

        // Nothing is changed by a dry run.
        assert!(shift_all(&mut nodes, &edits, true).unwrap());
        assert_eq!(entries(table_of(&nodes[0])), [100, u64::from(near_the_end)]);

        upgrade_stco(&mut nodes).unwrap();
        assert_eq!(table_of(&nodes[0]).kind, CO64);
        assert_eq!(nodes.iter().map(node_len).sum::<u64>(), old_len + 8);

        assert!(!shift_all(&mut nodes, &edits, true).unwrap());
        assert!(!shift_all(&mut nodes, &edits, false).unwrap());
        assert_eq!(entries(table_of(&nodes[0])), [5050, u64::from(near_the_end) + 4950]);
        assert_eq!(entries(table_of(&nodes[1])), [7_000_004_950]);
    }

    #[test]
    fn stco_that_fits_stays_32_bits_and_shrinking_moves_back() {
        let mut nodes = vec![trak(stco(&[1000, 2000, 3000]))];

        let grow = [Edit { old_start: 0, old_end: 10, new_len: 110 }];
        assert!(!shift_all(&mut nodes, &grow, false).unwrap());
        assert_eq!(table_of(&nodes[0]).kind, STCO);
        assert_eq!(entries(table_of(&nodes[0])), [1100, 2100, 3100]);

        let shrink = [Edit { old_start: 0, old_end: 210, new_len: 10 }];
        assert!(!shift_all(&mut nodes, &shrink, false).unwrap());
        assert_eq!(entries(table_of(&nodes[0])), [900, 1900, 2900]);
    }

    #[test]
    fn truncated_offset_tables_are_malformed() {
        let mut node = stco(&[1, 2, 3]);
        if let Body::Leaf(payload) = &mut node.body {
            payload.truncate(payload.len() - 1);
        }

        let edits = [Edit { old_start: 0, old_end: 1, new_len: 2 }];
        assert!(matches!(shift_offsets(&mut node, &edits, true), Err(Error::Malformed(_))));
    }

    #[test]
    fn saio_offsets_move_and_cannot_widen() {
        // Version 0, no auxiliary type: count, then 32-bit offsets.
        let mut payload = vec![0; 4];
        payload.extend_from_slice(&2u32.to_be_bytes());
        payload.extend_from_slice(&500u32.to_be_bytes());
        payload.extend_from_slice(&(u32::MAX - 5).to_be_bytes());
        let mut node = Node::leaf(SAIO, payload);

        let small = [Edit { old_start: 0, old_end: 10, new_len: 15 }];
        assert!(!shift_offsets(&mut node, &small, true).unwrap());

        let big = [Edit { old_start: 0, old_end: 10, new_len: 1000 }];
        assert!(matches!(shift_offsets(&mut node, &big, false), Err(Error::Unsupported(_))));

        // Version 1 has 64-bit offsets, behind a type and parameter when flag 1 is set.
        let mut payload = vec![1, 0, 0, 1];
        payload.extend_from_slice(b"cenc");
        payload.extend_from_slice(&[0; 4]);
        payload.extend_from_slice(&1u32.to_be_bytes());
        payload.extend_from_slice(&u64::from(u32::MAX).to_be_bytes());
        let mut node = Node::leaf(SAIO, payload);

        assert!(!shift_offsets(&mut node, &big, false).unwrap());
        let Body::Leaf(payload) = &node.body
        else {
            panic!()
        };
        assert_eq!(be_u64(payload, 16), u64::from(u32::MAX) + 990);
    }

    #[test]
    fn box_sizes_switch_to_64_bits_when_needed() {
        let max = u64::from(u32::MAX);
        assert_eq!(box_len(0), 8);
        assert_eq!(box_len(max - 8), max);
        assert_eq!(box_len(max - 7), max - 7 + 16);
    }

    #[test]
    fn headers_with_64_bit_and_open_sizes_are_parsed() {
        let mut buf = Vec::new();
        // A 64-bit `trak` holding a `free`, then a `udta` that extends to the end.
        let free = [0, 0, 0, 12, b'f', b'r', b'e', b'e', 1, 2, 3, 4];
        buf.extend_from_slice(&1u32.to_be_bytes());
        buf.extend_from_slice(b"trak");
        buf.extend_from_slice(&((16 + free.len()) as u64).to_be_bytes());
        buf.extend_from_slice(&free);
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(b"udta");
        buf.extend_from_slice(&free);

        let nodes = parse_children(&buf, &MOOV).unwrap();
        assert_eq!(nodes.len(), 2);

        let Body::Container { children, .. } = &nodes[0].body
        else {
            panic!()
        };
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].kind, FREE);

        let Body::Container { children, .. } = &nodes[1].body
        else {
            panic!()
        };
        assert_eq!(children[0].kind, FREE);

        // The sizes are written explicitly (and in 32 bits, as they fit).
        let mut out = Vec::new();
        for node in &nodes {
            write_node(&mut out, node);
        }
        assert_eq!(out.len() as u64, nodes.iter().map(node_len).sum::<u64>());
        assert_eq!(&out[..8], &[0, 0, 0, 20, b't', b'r', b'a', b'k']);

        // A box that does not fit its parent is malformed.
        let mut bad = buf.clone();
        bad[11] = 1;
        assert!(matches!(parse_children(&bad, &MOOV), Err(Error::Malformed(_))));
        assert!(parse_children(&[0, 0, 0, 4, b'f', b'r', b'e', b'e'], &MOOV).is_err());
    }

    #[test]
    fn bytes_too_few_for_a_box_at_the_end_of_a_parent_are_kept() {
        // QuickTime ends a `udta` with a 32-bit zero.
        let mut buf = vec![0, 0, 0, 8, b'f', b'r', b'e', b'e'];
        buf.extend_from_slice(&[0, 0, 0, 0]);

        let nodes = parse_children(&buf, &UDTA).unwrap();
        assert_eq!(nodes.len(), 2);

        let mut out = Vec::new();
        for node in &nodes {
            write_node(&mut out, node);
        }
        assert_eq!(out, buf);
    }

    fn box_of(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn explicit_base_data_offsets_are_found_in_the_track_fragment_headers() {
        let tfhd = |flags: u8| {
            let mut body = vec![0, 0, 0, flags];
            body.extend_from_slice(&1u32.to_be_bytes());
            box_of(b"tfhd", &body)
        };
        let moof = |tfhd: Vec<u8>| {
            let mut traf = tfhd;
            traf.extend_from_slice(&box_of(b"trun", &[0; 8]));
            let mut body = box_of(b"mfhd", &[0; 8]);
            body.extend_from_slice(&box_of(b"traf", &traf));
            body
        };

        assert!(!moof_has_explicit_base(&moof(tfhd(0x00))).unwrap());
        // default-sample-duration-present is another flag.
        assert!(!moof_has_explicit_base(&moof(tfhd(0x08))).unwrap());
        assert!(moof_has_explicit_base(&moof(tfhd(0x01))).unwrap());
        assert!(moof_has_explicit_base(&moof(tfhd(0x09))).unwrap());
    }

    #[test]
    fn tfra_offsets_move_in_both_versions() {
        // Entries of (time, moof offset, traf, trun and sample numbers of 1, 2 and 1 bytes).
        let lengths: u32 = 1 << 2;
        let entry_v0 = |time: u32, offset: u32| {
            let mut e = time.to_be_bytes().to_vec();
            e.extend_from_slice(&offset.to_be_bytes());
            e.extend_from_slice(&[1, 0, 1, 1]);
            e
        };
        let entry_v1 = |time: u64, offset: u64| {
            let mut e = time.to_be_bytes().to_vec();
            e.extend_from_slice(&offset.to_be_bytes());
            e.extend_from_slice(&[1, 0, 1, 1]);
            e
        };

        let mut v0 = vec![0, 0, 0, 0];
        v0.extend_from_slice(&1u32.to_be_bytes());
        v0.extend_from_slice(&lengths.to_be_bytes());
        v0.extend_from_slice(&2u32.to_be_bytes());
        v0.extend_from_slice(&entry_v0(0, 1000));
        v0.extend_from_slice(&entry_v0(90, 5000));

        let mut v1 = vec![1, 0, 0, 0];
        v1.extend_from_slice(&2u32.to_be_bytes());
        v1.extend_from_slice(&lengths.to_be_bytes());
        v1.extend_from_slice(&1u32.to_be_bytes());
        v1.extend_from_slice(&entry_v1(0, 7_000_000_000));

        let mut payload = box_of(b"tfra", &v0);
        payload.extend_from_slice(&box_of(b"tfra", &v1));
        payload.extend_from_slice(&box_of(b"mfro", &[0; 8]));
        let len = payload.len();

        // Everything from 1000 on moves 24 bytes forward.
        let edits = [Edit { old_start: 100, old_end: 200, new_len: 124 }];
        shift_tfra(&mut payload, &edits).unwrap();
        assert_eq!(payload.len(), len);

        let ranges = child_boxes(&payload).unwrap();
        let first = &payload[ranges[0].body.clone()];
        assert_eq!(be_u32(first, 16 + 4), 1024);
        assert_eq!(be_u32(first, 16 + 12 + 4), 5024);
        let second = &payload[ranges[1].body.clone()];
        assert_eq!(be_u64(second, 16 + 8), 7_000_000_024);

        // An offset that would not fit 32 bits is unsupported.
        let huge = [Edit { old_start: 0, old_end: 10, new_len: u64::from(u32::MAX) }];
        assert!(matches!(shift_tfra(&mut payload, &huge), Err(Error::Unsupported(_))));
    }

    /// A sink that remembers the first bytes it was given and counts all of them.
    #[derive(Default)]
    struct Sink {
        head: Vec<u8>,
        total: u64,
    }

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.head.len() < 16 {
                let room = 16 - self.head.len();
                self.head.extend_from_slice(&buf[..buf.len().min(room)]);
            }
            self.total += buf.len() as u64;
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn free_boxes_are_written_in_the_form_their_size_needs() {
        let mut sink = Sink::default();
        write_free(&mut sink, 8).unwrap();
        assert_eq!((sink.total, &sink.head[..]), (8, &[0, 0, 0, 8, b'f', b'r', b'e', b'e'][..]));

        let mut sink = Sink::default();
        write_free(&mut sink, 5000).unwrap();
        assert_eq!(sink.total, 5000);
        assert_eq!(&sink.head[..4], &5000u32.to_be_bytes());

        let big = u64::from(u32::MAX) + 100;
        let mut sink = Sink::default();
        write_free(&mut sink, big).unwrap();
        assert_eq!(sink.total, big);
        assert_eq!(&sink.head[..8], &[0, 0, 0, 1, b'f', b'r', b'e', b'e']);
        assert_eq!(&sink.head[8..16], &big.to_be_bytes());
    }

    fn leaf_kinds(nodes: &[Node]) -> Vec<[u8; 4]> {
        nodes.iter().map(|n| n.kind).collect()
    }

    #[test]
    fn the_user_data_loses_its_tags_and_keeps_everything_else() {
        let ilst = Node::leaf(ILST, vec![1, 2, 3]);
        let other = Node::leaf(*b"xml ", vec![9]);
        let meta = Node::container(
            META,
            vec![0; 4],
            vec![
                Node::leaf(HDLR, mdir_hdlr_box()[8..].to_vec()),
                ilst,
                Node::leaf(FREE, vec![0; 20]),
                other,
            ],
        );

        let mut children = vec![
            Node::leaf(*b"chpl", vec![5]),
            Node::leaf(ID32, vec![0; 8]),
            meta,
            Node::leaf(*b"name", vec![6]),
        ];
        let mut removed = Removed::default();

        let at = strip_udta(&mut children, &mut removed).unwrap();

        // The tags were between the two boxes that stay.
        assert_eq!(at, Some(1));
        assert_eq!(leaf_kinds(&children), [*b"chpl", *b"name"]);
        assert_eq!(removed.ilst, [1, 2, 3]);
        assert!(removed.hdlr.is_some());
        assert_eq!(leaf_kinds(&removed.kept), [*b"xml "]);

        // Nothing to store but a box that is kept: the meta box stays.
        let meta = build_meta(&mut removed, None).unwrap();
        let Body::Container { children, .. } = &meta.body
        else {
            panic!()
        };
        assert_eq!(leaf_kinds(children), [HDLR, *b"xml "]);

        // Nothing to store at all: no meta box.
        assert!(build_meta(&mut Removed::default(), None).is_none());

        // With a new ilst, a missing handler is added.
        let ilst = build_ilst(&[], &[], &[]).unwrap();
        assert!(ilst.is_none());
        let ilst = box_of(b"ilst", &[7, 7]);
        let meta = build_meta(&mut Removed::default(), Some(ilst)).unwrap();
        let Body::Container { children, .. } = &meta.body
        else {
            panic!()
        };
        assert_eq!(leaf_kinds(children), [HDLR, ILST]);
    }

    #[test]
    fn id32_handlers_are_dropped_and_foreign_ones_refused() {
        let hdlr = |handler: &[u8; 4]| {
            let mut body = vec![0; 8];
            body.extend_from_slice(handler);
            body.extend_from_slice(&[0; 13]);
            Node::leaf(HDLR, body)
        };

        let id32_meta =
            Node::container(META, vec![0; 4], vec![hdlr(b"ID32"), Node::leaf(ID32, vec![0; 8])]);
        let mut children = vec![id32_meta];
        let mut removed = Removed::default();
        assert_eq!(strip_udta(&mut children, &mut removed).unwrap(), Some(0));
        assert!(children.is_empty() && removed.ilst.is_empty() && removed.hdlr.is_none());

        let mdta =
            Node::container(META, vec![0; 4], vec![hdlr(b"mdta"), Node::leaf(ILST, vec![1])]);
        let mut children = vec![mdta];
        assert!(matches!(
            strip_udta(&mut children, &mut Removed::default()),
            Err(Error::Unsupported(_))
        ));
    }
}
