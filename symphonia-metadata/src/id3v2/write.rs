// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An ID3v2 tag serializer.
//!
//! The serializer is the inverse of the ID3v2 reader: a [`Tag`] whose `std` is set is written to the
//! frame that the reader maps to that [`StandardTag`], following the conventions of MusicBrainz
//! Picard where the reader accepts several spellings (e.g. `TIT1` for the grouping, `UFID` for the
//! MusicBrainz recording ID, `TXXX` descriptions as Picard names them). A tag without a standard
//! tag is written as a free-form `TXXX` frame under its raw key.
//!
//! A tag that is rewritten replaces the *managed* frames of the old tag (every frame the reader
//! turns into a [`Tag`] or a [`Visual`]). All other frames (`PRIV`, `GEOB`, `POPM`, `SYLT`, `CHAP`,
//! `CTOC`, ...) are *unmanaged*: they are collected from the old tag(s) with [`Existing`] and
//! written back unchanged. Frames that are compressed or encrypted are only preserved when the
//! version of the new tag is the version of the old tag, as they cannot be converted.
//!
//! # Versions
//!
//! * ID3v2.4: text is UTF-8, multiple values are NUL-separated.
//! * ID3v2.3: text is ISO-8859-1, or UTF-16 with a byte-order-mark if it cannot be represented in
//!   ISO-8859-1. Multiple values are joined with [`WriteOptions::join_v23`] (`/`, as Picard does).
//!
//! The text encoding can be forced with [`WriteOptions::encoding`].
//!
//! # Grouping and work
//!
//! With [`WriteOptions::itunes_grouping`], [`StandardTag::Grouping`] is written to `GRP1` and
//! [`StandardTag::Work`] to `TIT1`. The reader maps both `TIT1` and `GRP1` to
//! [`StandardTag::Grouping`], so a consumer of a tag written this way must treat a tag with the
//! raw key `TIT1` as the work.
//!
//! # Ratings
//!
//! With [`WriteOptions::rating_email`], each [`StandardTag::Rating`] is written to a `POPM` frame
//! (the rating byte is the rating in PPM scaled to 0-255). The owner is the `EMAIL` sub-field of
//! the tag's raw tag if there is one, otherwise the option's email. The play counter of an
//! existing `POPM` frame of the same owner is kept. Existing `POPM` frames of an owner that is
//! written, or of the option's email, are replaced (or removed if there is no rating).
//!
//! # ID3v2.4-only frames
//!
//! Frames and tags that only exist in ID3v2.4 are written as their ID3v2.3 equivalent when there
//! is one (`TYER`/`TDAT`/`TIME`, `TORY`, `IPLS`, `TXXX` sort frames), otherwise, as the ID3v2.4
//! frame, which the reader accepts.
//!
//! Unsynchronisation is never applied to a tag that is written.

use symphonia_core::meta::{RawTag, RawValue, StandardTag, StandardVisualKey, Tag, Visual};

use crate::utils::images::try_get_image_info;

use super::frames::{LEGACY_FRAME_MAP, id3v2p4_frame_size, validate_frame_id};
use super::sub_fields::*;
use super::unsync::decode_unsynchronisation;

/// The owner identifier of the `UFID` frame containing the MusicBrainz recording ID.
const UFID_OWNER_MUSICBRAINZ: &str = "http://musicbrainz.org";

/// The separator that joins multiple values of a text frame in ID3v2.3, as Picard does.
const JOIN_V23: &str = "/";

/// The language written for comment and lyrics frames that do not specify one.
const DEFAULT_LANGUAGE: &str = "eng";

/// The largest frame size (header excluded) that ID3v2.4 can express.
const MAX_FRAME_SIZE: usize = 0x0fff_ffff;

/// The major version of the tag that is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    /// ID3v2.3.
    V3,
    /// ID3v2.4.
    V4,
}

impl Version {
    fn major(self) -> u8 {
        match self {
            Version::V3 => 3,
            Version::V4 => 4,
        }
    }
}

/// The text encoding the serializer uses for the text of the frames it writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextEncoding {
    /// ID3v2.4: UTF-8. ID3v2.3: ISO-8859-1, or UTF-16 for a frame with text that cannot be
    /// represented in ISO-8859-1.
    #[default]
    Auto,
    /// UTF-8 in ID3v2.4. ID3v2.3 has no UTF-8, so UTF-16 is written instead.
    Utf8,
    /// UTF-16 with a byte-order-mark, in both versions.
    Utf16,
    /// ISO-8859-1 in both versions, or UTF-16 for a frame with text that cannot be represented in
    /// ISO-8859-1.
    Latin1,
}

/// Options for [`serialize`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOptions {
    /// The version of the tag to write.
    pub version: Version,
    /// The number of zero bytes to append after the last frame, so that the tag can later grow
    /// without moving the data that follows it.
    pub padding: usize,
    /// The total size (header included) of the tag being replaced, if it is at the front of a
    /// stream. When the new tag fits in it (and does not leave more than twice `padding` bytes
    /// unused), the new tag is padded to exactly this size, so the data that follows it does not
    /// move.
    pub reuse_len: Option<usize>,
    /// The text encoding of the frames.
    pub encoding: TextEncoding,
    /// The string that joins the values of a multi-valued text frame in ID3v2.3 (which, unlike
    /// ID3v2.4, cannot separate values with NUL). Picard's `id3v23_join_with`.
    pub join_v23: String,
    /// Write the grouping to `GRP1` and the work to `TIT1` (the convention of iTunes) instead of
    /// the grouping to `TIT1` and the work to a `TXXX:WORK` frame. The reader maps both `TIT1`
    /// and `GRP1` to [`StandardTag::Grouping`], so when this is on, a consumer must treat the raw
    /// frame `TIT1` as the work. Picard's `itunes_compatible_grouping`.
    pub itunes_grouping: bool,
    /// The email of the `POPM` frame a [`StandardTag::Rating`] is written to (see the module
    /// documentation). When `None`, ratings are not written to ID3v2 tags and `POPM` frames are
    /// all preserved.
    pub rating_email: Option<String>,
}

impl Default for WriteOptions {
    fn default() -> Self {
        WriteOptions {
            version: Version::V4,
            padding: 0,
            reuse_len: None,
            encoding: TextEncoding::Auto,
            join_v23: JOIN_V23.to_string(),
            itunes_grouping: false,
            rating_email: None,
        }
    }
}

/// A frame of an existing tag that is preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawFrame {
    /// The 4 character frame ID (ID3v2.2 IDs are converted).
    id: [u8; 4],
    /// The frame flags in the format of `src_version`.
    flags: u16,
    /// The frame data, as stored in the frame (compression and group/encryption/length bytes
    /// included), after the removal of tag-wide unsynchronisation.
    data: Vec<u8>,
    /// The major version of the tag the frame was read from.
    src_version: u8,
}

/// The unmanaged frames of existing ID3v2 tags, which are preserved by [`serialize`].
#[derive(Debug, Default)]
pub struct Existing {
    version: Option<u8>,
    frames: Vec<RawFrame>,
}

impl Existing {
    /// Create an empty set of existing frames.
    pub fn new() -> Self {
        Self::default()
    }

    /// The major version (2, 3, or 4) of the first tag that was parsed.
    pub fn version(&self) -> Option<u8> {
        self.version
    }

    /// Returns `true` if there are no preserved frames.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Parse a complete existing tag: from the `ID3` header up to, optionally, the footer. The
    /// unmanaged frames of the tag are collected. Returns `false` if `tag` is not a valid ID3v2
    /// tag.
    pub fn parse_tag(&mut self, tag: &[u8]) -> bool {
        let Some(header) = parse_header(tag)
        else {
            return false;
        };

        if self.version.is_none() {
            self.version = Some(header.major);
        }

        // Tolerate a truncated tag: use the bytes that are available.
        let end = (10 + header.size).min(tag.len());
        let mut body = tag[10..end].to_vec();

        // Tag-wide unsynchronisation (ID3v2.4 only has per-frame unsynchronisation).
        if header.unsync && header.major < 4 {
            let len = decode_unsynchronisation(&mut body).len();
            body.truncate(len);
        }

        let mut pos = 0;

        // Skip the extended header.
        if header.ext_header {
            let skip = match header.major {
                3 => {
                    body.get(..4).map(|b| 4 + u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize)
                }
                _ => body.get(..4).map(|b| synchsafe(b) as usize),
            };

            match skip {
                Some(skip) if skip <= body.len() => pos = skip,
                _ => return true,
            }
        }

        while let Some(frame) = next_frame(&body, &mut pos, header.major) {
            if !is_managed(&frame) && !self.frames.contains(&frame) {
                self.frames.push(frame);
            }
        }

        true
    }
}

struct Header {
    major: u8,
    unsync: bool,
    ext_header: bool,
    size: usize,
}

fn synchsafe(b: &[u8]) -> u32 {
    b.iter().take(4).fold(0, |acc, &x| (acc << 7) | u32::from(x & 0x7f))
}

fn parse_header(tag: &[u8]) -> Option<Header> {
    let [b'I', b'D', b'3', major, minor, flags, size @ ..] = tag.get(..10)?
    else {
        return None;
    };

    if !(2..=4).contains(major) || *minor == 0xff || size.iter().any(|b| b & 0x80 != 0) {
        return None;
    }

    // ID3v2.2 compression is undefined.
    if *major == 2 && flags & 0x40 != 0 {
        return None;
    }

    Some(Header {
        major: *major,
        unsync: flags & 0x80 != 0,
        ext_header: *major >= 3 && flags & 0x40 != 0,
        size: synchsafe(size) as usize,
    })
}

/// Get the total length of an ID3v2 tag (header, body, and footer if the header says there is
/// one) from the first 10 bytes of the tag. Returns `None` if `header` does not start with a valid
/// ID3v2 header.
pub fn tag_len(header: &[u8]) -> Option<usize> {
    let hdr = parse_header(header)?;
    let footer = if hdr.major == 4 && header[5] & 0x10 != 0 { 10 } else { 0 };
    Some(10 + hdr.size + footer)
}

/// Read the next frame of the (unsynchronised) tag body `data`, from `pos`.
fn next_frame(data: &[u8], pos: &mut usize, major: u8) -> Option<RawFrame> {
    let id_len = if major == 2 { 3 } else { 4 };
    let hdr_len = if major == 2 { 6 } else { 10 };

    loop {
        let hdr = data.get(*pos..*pos + hdr_len)?;

        // Padding.
        if hdr[..id_len].iter().all(|&b| b == 0) {
            return None;
        }

        if !validate_frame_id(&hdr[..id_len]) {
            return None;
        }

        let (size, flags) = match major {
            2 => (u32::from_be_bytes([0, hdr[3], hdr[4], hdr[5]]) as usize, 0u16),
            3 => (
                u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize,
                u16::from_be_bytes([hdr[8], hdr[9]]),
            ),
            _ => (
                id3v2p4_frame_size([hdr[4], hdr[5], hdr[6], hdr[7]], &data[*pos + hdr_len..])
                    as usize,
                u16::from_be_bytes([hdr[8], hdr[9]]),
            ),
        };

        let start = *pos + hdr_len;
        let end = start.checked_add(size).filter(|&end| end <= data.len())?;
        *pos = end;

        let id = if major == 2 {
            match LEGACY_FRAME_MAP.get(&[hdr[0], hdr[1], hdr[2]]) {
                Some(id) => **id,
                // An unknown ID3v2.2 frame cannot be converted. Skip it.
                None => continue,
            }
        }
        else {
            [hdr[0], hdr[1], hdr[2], hdr[3]]
        };

        if size == 0 {
            continue;
        }

        return Some(RawFrame { id, flags, data: data[start..end].to_vec(), src_version: major });
    }
}

/// The frames that are rewritten from the tags (everything the reader maps to a [`Tag`] or a
/// [`Visual`]).
const MANAGED_FRAMES: &[&[u8; 4]] = &[
    b"APIC", b"COMM", b"GRP1", b"IPLS", b"MVIN", b"MVNM", b"PCNT", b"PCST", b"TALB", b"TBPM",
    b"TCAT", b"TCMP", b"TCOM", b"TCON", b"TCOP", b"TDAT", b"TDEN", b"TDES", b"TDOR", b"TDRC",
    b"TDRL", b"TDTG", b"TENC", b"TEXT", b"TGID", b"TIME", b"TIPL", b"TIT1", b"TIT2", b"TIT3",
    b"TKEY", b"TKWD", b"TLAN", b"TMCL", b"TMED", b"TMOO", b"TOAL", b"TOFN", b"TOLY", b"TOPE",
    b"TORY", b"TOWN", b"TPE1", b"TPE2", b"TPE3", b"TPE4", b"TPOS", b"TPRO", b"TPUB", b"TRCK",
    b"TRSN", b"TRSO", b"TSO2", b"TSOA", b"TSOC", b"TSOP", b"TSOT", b"TSRC", b"TSSE", b"TSST",
    b"TXXX", b"TYER", b"UFID", b"USER", b"USLT", b"WCOM", b"WCOP", b"WFED", b"WOAF", b"WOAR",
    b"WOAS", b"WORS", b"WPAY", b"WPUB", b"WXXX",
];

/// All other standard frames. Used to tell the frames of a tag read back from a file apart from
/// free-form tags.
const OTHER_FRAMES: &[&[u8; 4]] = &[
    b"AENC", b"ASPI", b"ATXT", b"CHAP", b"COMR", b"CTOC", b"ENCR", b"EQU2", b"EQUA", b"ETCO",
    b"GEOB", b"GRID", b"LINK", b"MCDI", b"MLLT", b"OWNE", b"POPM", b"POSS", b"PRIV", b"RBUF",
    b"RVA2", b"RVAD", b"RVRB", b"SEEK", b"SIGN", b"SYLT", b"SYTC", b"TDLY", b"TFLT", b"TLEN",
    b"TRDA", b"TSIZ",
];

/// Whether a frame of an existing tag is rewritten from the tags (and so is not preserved).
fn is_managed(frame: &RawFrame) -> bool {
    if !MANAGED_FRAMES.contains(&&frame.id) {
        return false;
    }

    // An encrypted frame cannot be read, and so it cannot have been turned into a tag. Keep it.
    let encrypted = match frame.src_version {
        3 => frame.flags & 0x0040 != 0,
        _ => frame.flags & 0x0004 != 0,
    };

    if encrypted {
        return false;
    }

    // Only the UFID frame that holds the MusicBrainz recording ID is managed.
    if &frame.id == b"UFID" {
        let skip = prefix_len(frame);
        let owner = frame.data.get(skip..).and_then(|d| d.split(|&b| b == 0).next());
        return owner.is_some_and(|o| o.eq_ignore_ascii_case(UFID_OWNER_MUSICBRAINZ.as_bytes()));
    }

    true
}

/// The number of bytes at the start of a frame's data that are not content (group identifier,
/// data length indicator).
fn prefix_len(frame: &RawFrame) -> usize {
    match frame.src_version {
        3 => usize::from(frame.flags & 0x0020 != 0),
        4 => usize::from(frame.flags & 0x0040 != 0) + if frame.flags & 0x0001 != 0 { 4 } else { 0 },
        _ => 0,
    }
}

// Output.

/// A frame to be written.
struct OutFrame {
    id: [u8; 4],
    flags: u16,
    data: Vec<u8>,
}

impl OutFrame {
    fn new(id: &str, data: Vec<u8>) -> Self {
        let id: [u8; 4] = id.as_bytes().try_into().expect("a frame id has 4 characters");
        OutFrame { id, flags: 0, data }
    }

    fn write(&self, version: Version, out: &mut Vec<u8>) -> bool {
        if self.data.len() > MAX_FRAME_SIZE || self.data.is_empty() {
            return false;
        }

        out.extend_from_slice(&self.id);

        match version {
            Version::V3 => out.extend_from_slice(&(self.data.len() as u32).to_be_bytes()),
            Version::V4 => out.extend_from_slice(&encode_synchsafe(self.data.len() as u32)),
        }

        out.extend_from_slice(&self.flags.to_be_bytes());
        out.extend_from_slice(&self.data);
        true
    }
}

fn encode_synchsafe(v: u32) -> [u8; 4] {
    [(v >> 21) as u8 & 0x7f, (v >> 14) as u8 & 0x7f, (v >> 7) as u8 & 0x7f, v as u8 & 0x7f]
}

// Text encoding.

/// Whether `s` can be written as ISO-8859-1 and be read back unchanged (the reader treats bytes
/// 0x80-0x9f as Windows-1252, and replaces control characters other than the line-feed).
fn is_latin1(s: &str) -> bool {
    s.chars().all(|c| {
        let u = u32::from(c);
        u <= 0xff && !(0x80..=0x9f).contains(&u) && (u >= 0x20 || c == '\n')
    })
}

/// Choose the text encoding identifier for `strings`, given the requested `encoding` and the
/// tag `version`.
fn choose_encoding(version: Version, encoding: TextEncoding, strings: &[&str]) -> u8 {
    let latin1_or_utf16 = || if strings.iter().all(|s| is_latin1(s)) { 0 } else { 1 };

    match (encoding, version) {
        (TextEncoding::Auto, Version::V4) | (TextEncoding::Utf8, Version::V4) => 3,
        (TextEncoding::Auto, Version::V3) | (TextEncoding::Latin1, _) => latin1_or_utf16(),
        // ID3v2.3 has no UTF-8.
        (TextEncoding::Utf8, Version::V3) | (TextEncoding::Utf16, _) => 1,
    }
}

/// Append `s` encoded as `encoding` (without a terminator).
fn push_text(out: &mut Vec<u8>, s: &str, encoding: u8) {
    match encoding {
        0 => out.extend(s.chars().map(|c| c as u8)),
        1 => {
            // UTF-16 with a little-endian byte-order-mark, even if the string is empty.
            out.extend_from_slice(&[0xff, 0xfe]);
            for unit in s.encode_utf16() {
                out.extend_from_slice(&unit.to_le_bytes());
            }
        }
        _ => out.extend_from_slice(s.as_bytes()),
    }
}

/// Append the terminator of a string in `encoding`.
fn push_terminator(out: &mut Vec<u8>, encoding: u8) {
    out.push(0);
    if encoding == 1 {
        out.push(0);
    }
}

/// Append `s` as ISO-8859-1, replacing characters that cannot be represented.
fn push_latin1(out: &mut Vec<u8>, s: &str) {
    out.extend(s.chars().map(|c| if u32::from(c) <= 0xff { c as u8 } else { b'?' }));
}

fn clean(s: &str) -> String {
    s.replace('\0', "")
}

/// Build the body of a text frame (`prefix` is the content that follows the encoding byte and
/// precedes the text, e.g. the description of a `TXXX` frame) holding `values`. In ID3v2.3, the
/// values are joined with `join_v23` unless `nul_separated` is set (frames that hold a list).
fn text_body(
    version: Version,
    enc: TextEncoding,
    join_v23: &str,
    prefix: &[&str],
    values: &[String],
    nul_separated: bool,
) -> Vec<u8> {
    let joined;
    let items: Vec<&str> = if version == Version::V3 && !nul_separated && values.len() > 1 {
        joined = values.iter().map(String::as_str).collect::<Vec<_>>().join(join_v23);
        vec![joined.as_str()]
    }
    else {
        values.iter().map(String::as_str).collect()
    };

    let all: Vec<&str> = prefix.iter().copied().chain(items.iter().copied()).collect();
    let encoding = choose_encoding(version, enc, &all);

    let mut out = vec![encoding];

    for p in prefix {
        push_text(&mut out, p, encoding);
        push_terminator(&mut out, encoding);
    }

    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            push_terminator(&mut out, encoding);
        }
        push_text(&mut out, item, encoding);
    }

    out
}

// Frame builders.

#[derive(Debug, Clone, PartialEq, Eq)]
enum Key {
    /// A text frame.
    Text([u8; 4]),
    /// A `TXXX` frame with a description.
    Txxx(String),
    /// A URL frame; one frame is written per value.
    Url([u8; 4]),
    /// A `WXXX` frame with a description; one frame is written per value.
    Wxxx(String),
    /// A `COMM` frame with a language and description; one frame is written per value.
    Comm(String, String),
    /// A `USLT` frame with a language and description; one frame is written per value.
    Uslt(String, String),
    /// A `USER` frame with a language.
    User(String),
    /// A `UFID` frame with an owner.
    Ufid(String),
    /// A `PCNT` frame.
    Pcnt,
}

struct Entry {
    key: Key,
    values: Vec<String>,
}

/// The sort rank of a frame, to get a conventional frame order.
fn rank(id: &[u8; 4]) -> u8 {
    match id {
        b"UFID" => 0,
        b"TIT2" => 1,
        b"TPE1" => 2,
        b"TALB" => 3,
        b"TPE2" => 4,
        b"TRCK" => 5,
        b"TPOS" => 6,
        b"TDRC" | b"TYER" => 7,
        b"TDAT" | b"TIME" => 8,
        b"TCON" => 9,
        b"COMM" => 10,
        b"USLT" => 11,
        b"TXXX" => 90,
        b"APIC" => 100,
        _ => 50,
    }
}

#[derive(Default)]
struct Pairs {
    list: Vec<(String, String)>,
}

struct Builder {
    version: Version,
    encoding: TextEncoding,
    join_v23: String,
    itunes_grouping: bool,
    entries: Vec<Entry>,
    /// The ratings (owner email if the tag has one, rating in PPM), in tag order.
    ratings: Vec<(Option<String>, u32)>,
    /// (number, total) pairs.
    track: (Option<u64>, Option<u64>),
    disc: (Option<u64>, Option<u64>),
    movement: (Option<u64>, Option<u64>),
    /// The involved people list (`TIPL`/`IPLS`) and the musician credits list (`TMCL`).
    involved: Pairs,
    musicians: Pairs,
    recording_date: Option<String>,
    recording_year: Option<u16>,
    recording_time: Option<String>,
    original_date: Option<String>,
    original_year: Option<u16>,
}

impl Builder {
    fn new(opts: &WriteOptions) -> Self {
        Builder {
            version: opts.version,
            encoding: opts.encoding,
            join_v23: opts.join_v23.clone(),
            itunes_grouping: opts.itunes_grouping,
            entries: Vec::new(),
            ratings: Vec::new(),
            track: (None, None),
            disc: (None, None),
            movement: (None, None),
            involved: Pairs::default(),
            musicians: Pairs::default(),
            recording_date: None,
            recording_year: None,
            recording_time: None,
            original_date: None,
            original_year: None,
        }
    }

    fn is_v4(&self) -> bool {
        self.version == Version::V4
    }

    fn add(&mut self, key: Key, value: String) {
        let value = clean(&value);
        if value.is_empty() && !matches!(key, Key::Pcnt) {
            return;
        }

        let merge = matches!(key, Key::Text(_) | Key::Txxx(_));

        if merge {
            let found = self.entries.iter_mut().find(|e| match (&e.key, &key) {
                (Key::Txxx(a), Key::Txxx(b)) => a.eq_ignore_ascii_case(b),
                (a, b) => a == b,
            });

            if let Some(entry) = found {
                entry.values.push(value);
                return;
            }
        }

        self.entries.push(Entry { key, values: vec![value] });
    }

    fn text(&mut self, id: &str, value: impl Into<String>) {
        self.add(
            Key::Text(id.as_bytes().try_into().expect("a frame id has 4 characters")),
            value.into(),
        );
    }

    fn txxx(&mut self, desc: &str, value: impl Into<String>) {
        self.add(Key::Txxx(clean(desc)), value.into());
    }

    fn url(&mut self, id: &str, value: impl Into<String>) {
        self.add(
            Key::Url(id.as_bytes().try_into().expect("a frame id has 4 characters")),
            value.into(),
        );
    }

    /// Add a text value of a frame that is ID3v2.4-only, as the TXXX frame the reader also
    /// understands in an ID3v2.3 tag.
    fn text_or_txxx(&mut self, id: &str, desc: &str, value: impl Into<String>) {
        if self.is_v4() {
            self.text(id, value);
        }
        else {
            self.txxx(desc, value);
        }
    }

    fn add_std(&mut self, std: &StandardTag, raw: &RawTag) {
        use StandardTag::*;

        match std {
            Album(v) => self.text("TALB", v.as_str()),
            AlbumArtist(v) => self.text("TPE2", v.as_str()),
            Artist(v) => self.text("TPE1", v.as_str()),
            Bpm(v) => self.text("TBPM", v.to_string()),
            CompilationFlag(v) => self.text("TCMP", if *v { "1" } else { "0" }),
            Composer(v) => self.text("TCOM", v.as_str()),
            Conductor(v) => self.text("TPE3", v.as_str()),
            Copyright(v) => self.text("TCOP", v.as_str()),
            DiscSubtitle(v) => self.text("TSST", v.as_str()),
            EncodedBy(v) => self.text("TENC", v.as_str()),
            Encoder(v) => self.text("TSSE", v.as_str()),
            EncodingDate(v) => self.text("TDEN", v.as_str()),
            Genre(v) => self.text("TCON", v.as_str()),
            // Picard writes TIT1 unless it is told to be iTunes compatible.
            Grouping(v) => {
                self.text(if self.itunes_grouping { "GRP1" } else { "TIT1" }, v.as_str())
            }
            IdentIsrc(v) => self.text("TSRC", v.as_str()),
            IdentPodcast(v) => self.text("TGID", v.as_str()),
            InitialKey(v) => self.text("TKEY", v.as_str()),
            InternetRadioName(v) => self.text("TRSN", v.as_str()),
            InternetRadioOwner(v) => self.text("TRSO", v.as_str()),
            Label(v) => self.text("TPUB", v.as_str()),
            Language(v) => self.text("TLAN", v.as_str()),
            Lyricist(v) => self.text("TEXT", v.as_str()),
            MediaFormat(v) => self.text("TMED", v.as_str()),
            Mood(v) => self.text_or_txxx("TMOO", "MOOD", v.as_str()),
            MovementName(v) => self.text("MVNM", v.as_str()),
            OriginalAlbum(v) => self.text("TOAL", v.as_str()),
            OriginalArtist(v) => self.text("TOPE", v.as_str()),
            OriginalFile(v) => self.text("TOFN", v.as_str()),
            OriginalLyricist(v) => self.text("TOLY", v.as_str()),
            Owner(v) => self.text("TOWN", v.as_str()),
            PodcastCategory(v) => self.text("TCAT", v.as_str()),
            PodcastDescription(v) => self.text("TDES", v.as_str()),
            PodcastFlag(v) => self.text("PCST", if *v { "1" } else { "0" }),
            PodcastKeywords(v) => self.text("TKWD", v.as_str()),
            ProductionCopyright(v) => self.text("TPRO", v.as_str()),
            ReleaseTime(v) => self.text("TDRL", v.as_str()),
            Remixer(v) => self.text("TPE4", v.as_str()),
            SortAlbum(v) => self.text_or_txxx("TSOA", "ALBUMSORT", v.as_str()),
            SortAlbumArtist(v) => self.text("TSO2", v.as_str()),
            SortArtist(v) => self.text_or_txxx("TSOP", "ARTISTSORT", v.as_str()),
            SortComposer(v) => self.text("TSOC", v.as_str()),
            SortTrackTitle(v) => self.text_or_txxx("TSOT", "TITLESORT", v.as_str()),
            TaggingDate(v) => self.text("TDTG", v.as_str()),
            TrackSubtitle(v) => self.text("TIT3", v.as_str()),
            TrackTitle(v) => self.text("TIT2", v.as_str()),

            UrlArtist(v) => self.url("WOAR", v.as_str()),
            UrlCopyright(v) => self.url("WCOP", v.as_str()),
            UrlInternetRadio(v) => self.url("WORS", v.as_str()),
            UrlLabel(v) => self.url("WPUB", v.as_str()),
            UrlOfficial(v) => self.url("WOAF", v.as_str()),
            UrlPayment(v) => self.url("WPAY", v.as_str()),
            UrlPurchase(v) => self.url("WCOM", v.as_str()),
            UrlSource(v) => self.url("WOAS", v.as_str()),
            // The reader reads the podcast feed URL like a text frame.
            UrlPodcast(v) => self.text("WFED", v.as_str()),
            Url(v) => {
                let desc = sub_field(raw, WXXX_DESCRIPTION).unwrap_or_default();
                self.add(Key::Wxxx(clean(&desc)), v.to_string());
            }

            Comment(v) => {
                let (lang, desc) = lang_and_desc(raw, COMM_LANGUAGE, COMM_SHORT_DESCRIPTION);
                self.add(Key::Comm(lang, desc), v.to_string());
            }
            Lyrics(v) => {
                let (lang, desc) = lang_and_desc(raw, USLT_LANGUAGE, USLT_DESCRIPTION);
                self.add(Key::Uslt(lang, desc), v.to_string());
            }
            TermsOfUse(v) => {
                let lang = sub_field(raw, USER_LANGUAGE)
                    .filter(|l| is_language(l))
                    .unwrap_or_else(|| DEFAULT_LANGUAGE.to_string());
                self.add(Key::User(lang), v.to_string());
            }

            TrackNumber(v) => self.track.0 = self.track.0.or(Some(*v)),
            TrackTotal(v) => self.track.1 = self.track.1.or(Some(*v)),
            DiscNumber(v) => self.disc.0 = self.disc.0.or(Some(*v)),
            DiscTotal(v) => self.disc.1 = self.disc.1.or(Some(*v)),
            MovementNumber(v) => self.movement.0 = self.movement.0.or(Some(*v)),
            MovementTotal(v) => self.movement.1 = self.movement.1.or(Some(*v)),
            PlayCounter(v) => self.add(Key::Pcnt, v.to_string()),

            Arranger(v) => self.involved.list.push(("arranger".into(), v.to_string())),
            Engineer(v) => self.involved.list.push(("engineer".into(), v.to_string())),
            Producer(v) => self.involved.list.push(("producer".into(), v.to_string())),
            MixDj(v) => self.involved.list.push(("DJ-mix".into(), v.to_string())),
            MixEngineer(v) => self.involved.list.push(("mix".into(), v.to_string())),
            Performer(v) => {
                let role = performer_role(raw, v);

                if self.is_v4() {
                    self.musicians.list.push((role, v.to_string()));
                }
                else {
                    let role = if role.is_empty() { "performer".to_string() } else { role };
                    self.involved.list.push((role, v.to_string()));
                }
            }

            RecordingDate(v) => {
                self.recording_date.get_or_insert_with(|| v.to_string());
            }
            RecordingYear(v) => self.recording_year = self.recording_year.or(Some(*v)),
            RecordingTime(v) => {
                self.recording_time.get_or_insert_with(|| v.to_string());
            }
            OriginalReleaseDate(v) => {
                self.original_date.get_or_insert_with(|| v.to_string());
            }
            OriginalReleaseYear(v) => self.original_year = self.original_year.or(Some(*v)),

            MusicBrainzRecordingId(v) => {
                self.add(Key::Ufid(UFID_OWNER_MUSICBRAINZ.to_string()), v.to_string())
            }
            MusicBrainzAlbumArtistId(v) => self.txxx("MusicBrainz Album Artist Id", v.as_str()),
            MusicBrainzAlbumId(v) => self.txxx("MusicBrainz Album Id", v.as_str()),
            MusicBrainzArtistId(v) => self.txxx("MusicBrainz Artist Id", v.as_str()),
            MusicBrainzDiscId(v) => self.txxx("MusicBrainz Disc Id", v.as_str()),
            MusicBrainzOriginalAlbumId(v) => self.txxx("MusicBrainz Original Album Id", v.as_str()),
            MusicBrainzOriginalArtistId(v) => {
                self.txxx("MusicBrainz Original Artist Id", v.as_str())
            }
            MusicBrainzReleaseGroupId(v) => self.txxx("MusicBrainz Release Group Id", v.as_str()),
            MusicBrainzReleaseStatus(v) => self.txxx("MusicBrainz Album Status", v.as_str()),
            MusicBrainzReleaseTrackId(v) => self.txxx("MusicBrainz Release Track Id", v.as_str()),
            MusicBrainzReleaseType(v) => self.txxx("MusicBrainz Album Type", v.as_str()),
            MusicBrainzTrackId(v) => self.txxx("MusicBrainz Track Id", v.as_str()),
            MusicBrainzTrmId(v) => self.txxx("MusicBrainz TRM Id", v.as_str()),
            MusicBrainzWorkId(v) => self.txxx("MusicBrainz Work Id", v.as_str()),
            ReleaseCountry(v) => self.txxx("MusicBrainz Album Release Country", v.as_str()),
            AcoustIdFingerprint(v) => self.txxx("Acoustid Fingerprint", v.as_str()),
            AcoustIdId(v) => self.txxx("Acoustid Id", v.as_str()),

            IdentAsin(v) => self.txxx("ASIN", v.as_str()),
            IdentBarcode(v) => self.txxx("BARCODE", v.as_str()),
            IdentCatalogNumber(v) => self.txxx("CATALOGNUMBER", v.as_str()),
            License(v) => self.txxx("LICENSE", v.as_str()),
            ReleaseDate(v) => self.txxx("RELEASEDATE", v.as_str()),
            Script(v) => self.txxx("SCRIPT", v.as_str()),
            Work(v) => {
                if self.itunes_grouping {
                    self.text("TIT1", v.as_str())
                }
                else {
                    self.txxx("WORK", v.as_str())
                }
            }
            Writer(v) => self.txxx("WRITER", v.as_str()),
            ReplayGainAlbumGain(v) => self.txxx("REPLAYGAIN_ALBUM_GAIN", v.as_str()),
            ReplayGainAlbumPeak(v) => self.txxx("REPLAYGAIN_ALBUM_PEAK", v.as_str()),
            ReplayGainAlbumRange(v) => self.txxx("REPLAYGAIN_ALBUM_RANGE", v.as_str()),
            ReplayGainReferenceLoudness(v) => {
                self.txxx("REPLAYGAIN_REFERENCE_LOUDNESS", v.as_str())
            }
            ReplayGainTrackGain(v) => self.txxx("REPLAYGAIN_TRACK_GAIN", v.as_str()),
            ReplayGainTrackPeak(v) => self.txxx("REPLAYGAIN_TRACK_PEAK", v.as_str()),
            ReplayGainTrackRange(v) => self.txxx("REPLAYGAIN_TRACK_RANGE", v.as_str()),
            ContentAdvisory(v) => {
                use symphonia_core::meta::ContentAdvisory as Advisory;

                let value = match v {
                    Advisory::None => "0",
                    Advisory::Explicit => "1",
                    Advisory::Censored => "2",
                };
                self.txxx("ITUNESADVISORY", value)
            }

            // The rating is written to a popularimeter (POPM) by `serialize`, if it is told to.
            Rating(v) => self.ratings.push((sub_field(raw, POPM_EMAIL), *v)),

            // There is no frame that the reader maps to the standard tag. Keep the value as a
            // free-form TXXX frame.
            other => {
                let (ident, value) = debug_parts(other);
                if let Some(value) = value {
                    let name = if raw.key.is_empty() || is_frame_key(&raw.key) {
                        screaming_snake_case(&ident)
                    }
                    else {
                        free_form_key(raw)
                    };
                    self.txxx(&name, value);
                }
            }
        }
    }

    /// Add a tag that has no standard tag as a free-form `TXXX` frame.
    fn add_free_form(&mut self, raw: &RawTag) {
        // The frames of a tag that was read from a file but not mapped to a standard tag (e.g.
        // `POPM` and `TLEN`) are preserved in the file by the unmanaged frames, not by the tag.
        if is_frame_key(&raw.key) {
            return;
        }

        let desc = free_form_key(raw);

        if desc.is_empty() {
            return;
        }

        for value in raw_strings(&raw.value) {
            self.txxx(&desc, value);
        }
    }

    fn finish(mut self) -> Vec<OutFrame> {
        let v = self.version;

        // Number and total.
        for (id, desc, (num, total)) in [
            ("TRCK", "TRACKTOTAL", self.track),
            ("TPOS", "DISCTOTAL", self.disc),
            ("MVIN", "MOVEMENTTOTAL", self.movement),
        ] {
            match (num, total) {
                (Some(n), Some(t)) => self.text(id, format!("{n}/{t}")),
                (Some(n), None) => self.text(id, n.to_string()),
                // The reader cannot read a total without a number.
                (None, Some(t)) => self.txxx(desc, t.to_string()),
                (None, None) => (),
            }
        }

        self.finish_dates();

        let mut frames: Vec<OutFrame> = Vec::new();

        for entry in &self.entries {
            match &entry.key {
                Key::Text([a, b, c, d]) => {
                    let id = [*a, *b, *c, *d];
                    let body = text_body(v, self.encoding, &self.join_v23, &[], &entry.values, false);
                    frames.push(OutFrame { id, flags: 0, data: body });
                }
                Key::Txxx(desc) => {
                    let body =
                        text_body(v, self.encoding, &self.join_v23, &[desc], &entry.values, false);
                    frames.push(OutFrame::new("TXXX", body));
                }
                Key::Url(id) => {
                    for value in &entry.values {
                        let mut body = Vec::new();
                        push_latin1(&mut body, value);
                        frames.push(OutFrame { id: *id, flags: 0, data: body });
                    }
                }
                Key::Wxxx(desc) => {
                    for value in &entry.values {
                        let encoding = choose_encoding(v, self.encoding, &[desc]);
                        let mut body = vec![encoding];
                        push_text(&mut body, desc, encoding);
                        push_terminator(&mut body, encoding);
                        push_latin1(&mut body, value);
                        frames.push(OutFrame::new("WXXX", body));
                    }
                }
                Key::Comm(lang, desc) | Key::Uslt(lang, desc) => {
                    let id = if matches!(entry.key, Key::Comm(..)) { "COMM" } else { "USLT" };
                    for value in &entry.values {
                        let encoding = choose_encoding(v, self.encoding, &[desc, value]);
                        let mut body = vec![encoding];
                        body.extend_from_slice(lang.as_bytes());
                        push_text(&mut body, desc, encoding);
                        push_terminator(&mut body, encoding);
                        push_text(&mut body, value, encoding);
                        frames.push(OutFrame::new(id, body));
                    }
                }
                Key::User(lang) => {
                    for value in &entry.values {
                        let encoding = choose_encoding(v, self.encoding, &[value]);
                        let mut body = vec![encoding];
                        body.extend_from_slice(lang.as_bytes());
                        push_text(&mut body, value, encoding);
                        frames.push(OutFrame::new("USER", body));
                    }
                }
                Key::Ufid(owner) => {
                    for value in &entry.values {
                        let mut body = Vec::new();
                        push_latin1(&mut body, owner);
                        body.push(0);
                        body.extend_from_slice(value.as_bytes());
                        frames.push(OutFrame::new("UFID", body));
                    }
                }
                Key::Pcnt => {
                    if let Some(count) = entry.values.first().and_then(|c| c.parse::<u64>().ok()) {
                        let bytes = count.to_be_bytes();
                        // At least 4 bytes.
                        let skip = bytes.iter().take(4).take_while(|&&b| b == 0).count();
                        frames.push(OutFrame::new("PCNT", bytes[skip..].to_vec()));
                    }
                }
            }
        }

        // The involved people and musician credit lists.
        let involved_id = if self.is_v4() { "TIPL" } else { "IPLS" };

        for (id, pairs) in [(involved_id, &self.involved), ("TMCL", &self.musicians)] {
            let values: Vec<String> = pairs
                .list
                .iter()
                .filter(|(_, person)| !clean(person).is_empty())
                .flat_map(|(role, person)| [clean(role), clean(person)])
                .collect();

            if !values.is_empty() {
                frames.push(OutFrame::new(
                    id,
                    text_body(v, self.encoding, &self.join_v23, &[], &values, true),
                ));
            }
        }

        // A stable sort keeps the order of the tags within a rank.
        frames.sort_by_key(|f| rank(&f.id));

        frames
    }

    /// Turn the collected dates into frames.
    fn finish_dates(&mut self) {
        let date = self.recording_date.take().map(|d| d.trim().to_string());
        let year = self.recording_year.map(|y| format!("{y:04}"));

        if self.is_v4() {
            if let Some(date) = date.or(year) {
                self.text("TDRC", date);
            }

            if let Some(time) = self.recording_time.take() {
                self.text("TIME", time);
            }

            if let Some(date) = self.original_date.take() {
                self.text("TDOR", date);
            }

            if let Some(year) = self.original_year {
                self.txxx("ORIGINALYEAR", year.to_string());
            }

            return;
        }

        // ID3v2.3 splits the recording date over the year, day and month, and time frames.
        let time = self.recording_time.take();

        match date.or(year) {
            Some(date) => {
                let (year, ddmm, hhmm) = split_legacy_date(&date);

                match year {
                    Some(year) => {
                        self.text("TYER", year);

                        if let Some(ddmm) = ddmm {
                            self.text("TDAT", ddmm);

                            // The time is only meaningful in combination with a full date.
                            if let Some(hhmm) = hhmm.or(time) {
                                self.text("TIME", hhmm);
                            }
                        }
                    }
                    // A date that does not start with a year. Keep it as free-form text.
                    None => self.text("TRDA", date),
                }
            }
            None => {
                if let Some(time) = time {
                    self.text("TIME", time);
                }
            }
        }

        // The original release date has no frame in ID3v2.3: Picard writes the year as TORY and
        // the date as a TXXX frame.
        let original_year = self.original_year.map(|y| format!("{y:04}")).or_else(|| {
            self.original_date
                .as_deref()
                .and_then(|d| d.get(..4))
                .filter(|y| is_digits(y))
                .map(String::from)
        });

        if let Some(date) = self.original_date.take() {
            self.txxx("ORIGINALDATE", date);
        }

        if let Some(year) = original_year {
            self.text("TORY", year);
        }
    }
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Split an ISO 8601 style date (`YYYY[-MM[-DD[THH:MM[:SS]]]]`) into the contents of the `TYER`,
/// `TDAT` (DDMM), and `TIME` (HHMM) frames.
fn split_legacy_date(date: &str) -> (Option<String>, Option<String>, Option<String>) {
    let (day_part, time_part) = match date.split_once(['T', ' ']) {
        Some((d, t)) => (d, Some(t)),
        None => (date, None),
    };

    let mut fields = day_part.split('-');

    let Some(year) = fields.next().filter(|y| y.len() == 4 && is_digits(y))
    else {
        return (None, None, None);
    };

    let month = fields.next().filter(|m| is_digits(m) && m.len() <= 2);
    let day = fields.next().filter(|d| is_digits(d) && d.len() <= 2);

    let ddmm = match (month, day) {
        (Some(m), Some(d)) => Some(format!("{:0>2}{:0>2}", d, m)),
        _ => None,
    };

    let hhmm = match (&ddmm, time_part) {
        (Some(_), Some(t)) => {
            let mut parts = t.split(':');
            match (parts.next(), parts.next()) {
                (Some(h), Some(m)) if is_digits(h) && is_digits(m) && h.len() <= 2 => {
                    Some(format!("{:0>2}{:0>2}", h, &m[..m.len().min(2)]))
                }
                _ => None,
            }
        }
        _ => None,
    };

    (Some(year.to_string()), ddmm, hhmm)
}

fn sub_field(raw: &RawTag, name: &str) -> Option<String> {
    raw.sub_fields
        .as_deref()?
        .iter()
        .find(|f| f.field == name)
        .and_then(|f| raw_strings(&f.value).into_iter().next())
}

fn is_language(s: &str) -> bool {
    s.len() == 3 && s.bytes().all(|b| b.is_ascii_alphabetic())
}

fn lang_and_desc(raw: &RawTag, lang_field: &str, desc_field: &str) -> (String, String) {
    let lang = sub_field(raw, lang_field)
        .filter(|l| is_language(l))
        .map(|l| l.to_ascii_lowercase())
        .unwrap_or_else(|| DEFAULT_LANGUAGE.to_string());

    (lang, clean(&sub_field(raw, desc_field).unwrap_or_default()))
}

/// Find the role of `person` in a `TMCL`/`TIPL`/`IPLS` raw tag.
fn performer_role(raw: &RawTag, person: &str) -> String {
    if matches!(raw.key.as_str(), "TMCL" | "TIPL" | "IPLS" | "IPL") {
        if let RawValue::StringList(list) = &raw.value {
            for pair in list.chunks_exact(2) {
                if pair[1].trim() == person.trim() {
                    return pair[0].trim().to_string();
                }
            }
        }
    }

    String::new()
}

/// Get the strings of a raw value, if it is textual.
fn raw_strings(value: &RawValue) -> Vec<String> {
    match value {
        RawValue::String(s) => vec![s.to_string()],
        RawValue::StringList(list) => list.to_vec(),
        RawValue::Boolean(b) => vec![if *b { "1" } else { "0" }.to_string()],
        RawValue::Float(f) => vec![f.to_string()],
        RawValue::SignedInt(i) => vec![i.to_string()],
        RawValue::UnsignedInt(u) => vec![u.to_string()],
        _ => Vec::new(),
    }
}

/// Whether a raw key is the ID of a standard frame (as opposed to a free-form key).
fn is_frame_key(key: &str) -> bool {
    let bytes = key.as_bytes();

    match bytes.len() {
        3 => LEGACY_FRAME_MAP.contains_key(&[bytes[0], bytes[1], bytes[2]]),
        4 => {
            let id = [bytes[0], bytes[1], bytes[2], bytes[3]];
            // The TXXX frame is free-form.
            &id != b"TXXX" && (MANAGED_FRAMES.contains(&&id) || OTHER_FRAMES.contains(&&id))
        }
        _ => false,
    }
}

/// The description of the `TXXX` frame to write a tag without a standard tag as.
fn free_form_key(raw: &RawTag) -> String {
    let key = raw.key.as_str();

    // A `TXXX` frame read by the reader has its description in a sub-field.
    if matches!(key, "TXXX" | "TXX") {
        return clean(&sub_field(raw, TXXX_DESCRIPTION).unwrap_or_default());
    }

    clean(key.strip_prefix("TXXX:").unwrap_or(key))
}

/// Split the debug representation of a standard tag (`Name("value")` or `Name(12)`) into the name
/// of the variant and its value.
fn debug_parts(std: &StandardTag) -> (String, Option<String>) {
    let repr = format!("{std:?}");

    let Some((ident, rest)) = repr.split_once('(')
    else {
        return (repr, None);
    };

    let inner = rest.strip_suffix(')').unwrap_or(rest);

    let value = match inner.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        Some(quoted) => unescape_debug(quoted),
        None => inner.to_string(),
    };

    (ident.to_string(), Some(value))
}

/// Reverse the escaping of a debug-formatted string.
fn unescape_debug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();

    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }

        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('0') => out.push('\0'),
            Some('u') => {
                let hex: String = chars.by_ref().skip(1).take_while(|&c| c != '}').collect();
                if let Some(c) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    out.push(c);
                }
            }
            Some(other) => out.push(other),
            None => (),
        }
    }

    out
}

fn screaming_snake_case(ident: &str) -> String {
    let mut out = String::new();
    let mut prev_lower = false;

    for c in ident.chars() {
        if c.is_ascii_uppercase() && prev_lower {
            out.push('_');
        }
        out.push(c.to_ascii_uppercase());
        prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
    }

    out
}

// Pictures.

fn picture_type(usage: Option<StandardVisualKey>) -> u8 {
    match usage {
        Some(StandardVisualKey::FileIcon) => 0x01,
        Some(StandardVisualKey::OtherIcon) => 0x02,
        Some(StandardVisualKey::FrontCover) => 0x03,
        Some(StandardVisualKey::BackCover) => 0x04,
        Some(StandardVisualKey::Leaflet) => 0x05,
        Some(StandardVisualKey::Media) => 0x06,
        Some(StandardVisualKey::LeadArtistPerformerSoloist) => 0x07,
        Some(StandardVisualKey::ArtistPerformer) => 0x08,
        Some(StandardVisualKey::Conductor) => 0x09,
        Some(StandardVisualKey::BandOrchestra) => 0x0a,
        Some(StandardVisualKey::Composer) => 0x0b,
        Some(StandardVisualKey::Lyricist) => 0x0c,
        Some(StandardVisualKey::RecordingLocation) => 0x0d,
        Some(StandardVisualKey::RecordingSession) => 0x0e,
        Some(StandardVisualKey::Performance) => 0x0f,
        Some(StandardVisualKey::ScreenCapture) => 0x10,
        Some(StandardVisualKey::Illustration) => 0x12,
        Some(StandardVisualKey::BandArtistLogo) => 0x13,
        Some(StandardVisualKey::PublisherStudioLogo) => 0x14,
        _ => 0x00,
    }
}

fn apic_frame(version: Version, encoding: TextEncoding, visual: &Visual) -> OutFrame {
    let media_type = visual
        .media_type
        .clone()
        .filter(|m| !m.is_empty())
        .or_else(|| try_get_image_info(&visual.data).map(|info| info.media_type))
        .unwrap_or_else(|| "application/octet-stream".to_string());

    let desc = visual
        .tags
        .iter()
        .find_map(|t| match &t.std {
            Some(StandardTag::Description(d)) => Some(clean(d)),
            _ => None,
        })
        .unwrap_or_default();

    let encoding = choose_encoding(version, encoding, &[&desc]);

    let mut body = vec![encoding];
    push_latin1(&mut body, &media_type);
    body.push(0);
    body.push(picture_type(visual.usage));
    push_text(&mut body, &desc, encoding);
    push_terminator(&mut body, encoding);
    body.extend_from_slice(&visual.data);

    OutFrame::new("APIC", body)
}

// Preserved frames.

/// The frames that only exist in ID3v2.3.
const V3_ONLY_FRAMES: &[&[u8; 4]] = &[b"EQUA", b"RVAD", b"TRDA", b"TSIZ"];

/// The frames that only exist in ID3v2.4.
const V4_ONLY_FRAMES: &[&[u8; 4]] = &[b"ASPI", b"EQU2", b"RVA2", b"SEEK", b"SIGN"];

/// Convert a preserved frame to the output version. Returns `None` if the frame cannot be
/// converted, and so is dropped.
fn convert_frame(frame: &RawFrame, to: Version) -> Option<OutFrame> {
    let to_major = to.major();

    // A frame in the output version is copied unchanged, whatever its flags.
    if frame.src_version == to_major {
        return Some(OutFrame { id: frame.id, flags: frame.flags, data: frame.data.clone() });
    }

    let (flags, prefix, mut data): (u16, usize, Vec<u8>) = match (frame.src_version, to) {
        // ID3v2.2 frames have no flags and are, but for the few that are rewritten, the same as
        // ID3v2.3 frames.
        (2, _) => {
            if matches!(&frame.id, b"LINK" | b"CRM_") {
                return None;
            }

            (0, 0, frame.data.clone())
        }
        (3, Version::V4) => {
            // Compression and encryption cannot be converted.
            if frame.flags & 0x00c0 != 0 {
                return None;
            }

            let flags =
                ((frame.flags & 0xe000) >> 1) | if frame.flags & 0x0020 != 0 { 0x0040 } else { 0 };
            (flags, usize::from(frame.flags & 0x0020 != 0), frame.data.clone())
        }
        (4, Version::V3) => {
            if frame.flags & 0x000c != 0 {
                return None;
            }

            let grouped = frame.flags & 0x0040 != 0;
            let flags = ((frame.flags & 0x7000) << 1) | if grouped { 0x0020 } else { 0 };

            let group_len = usize::from(grouped);
            let dli_len = if frame.flags & 0x0001 != 0 { 4 } else { 0 };

            let mut data = frame.data.get(..group_len)?.to_vec();
            let mut content = frame.data.get(group_len + dli_len..)?.to_vec();

            if frame.flags & 0x0002 != 0 {
                let len = decode_unsynchronisation(&mut content).len();
                content.truncate(len);
            }

            data.extend_from_slice(&content);
            (flags, group_len, data)
        }
        _ => return None,
    };

    if V3_ONLY_FRAMES.contains(&&frame.id) && to == Version::V4 {
        return None;
    }

    if V4_ONLY_FRAMES.contains(&&frame.id) && to == Version::V3 {
        return None;
    }

    // Embedded frames have a header that differs between the versions.
    if matches!(&frame.id, b"CHAP" | b"CTOC") {
        let content = data.get(prefix..)?;
        let converted = convert_embedded(&frame.id, content, frame.src_version, to)?;
        data.truncate(prefix);
        data.extend_from_slice(&converted);
    }
    else if to == Version::V3 && frame.id[0] == b'T' && data.get(prefix) == Some(&3) {
        // UTF-8 text does not exist in ID3v2.3.
        let converted = utf8_text_to_utf16(&data[prefix + 1..]);
        data.truncate(prefix);
        data.extend_from_slice(&converted);
    }

    Some(OutFrame { id: frame.id, flags, data })
}

/// Convert NUL-separated UTF-8 strings to a UTF-16 text frame body (with the encoding byte).
fn utf8_text_to_utf16(utf8: &[u8]) -> Vec<u8> {
    // A terminator after the last string does not start another string.
    let utf8 = utf8.strip_suffix(&[0]).unwrap_or(utf8);
    let mut out = vec![1];

    for (i, part) in utf8.split(|&b| b == 0).enumerate() {
        if i > 0 {
            push_terminator(&mut out, 1);
        }
        push_text(&mut out, &String::from_utf8_lossy(part), 1);
    }

    out
}

/// Convert the data of a `CHAP` or `CTOC` frame: the sub-frames that follow the frame's own
/// fields have the header of the tag's version.
fn convert_embedded(id: &[u8; 4], data: &[u8], from: u8, to: Version) -> Option<Vec<u8>> {
    // The element ID.
    let mut pos = data.iter().position(|&b| b == 0)? + 1;

    if id == b"CHAP" {
        // Start and end times, and start and end offsets.
        pos += 16;
    }
    else {
        // The flags, and the number of child element IDs, followed by the child element IDs.
        let count = *data.get(pos + 1)?;
        pos += 2;

        for _ in 0..count {
            pos += data.get(pos..)?.iter().position(|&b| b == 0)? + 1;
        }
    }

    let mut out = data.get(..pos)?.to_vec();
    let sub_frames = data.get(pos..)?;
    let mut sub_pos = 0;

    while let Some(frame) = next_frame(sub_frames, &mut sub_pos, from) {
        if let Some(converted) = convert_frame(&frame, to) {
            converted.write(to, &mut out);
        }
    }

    Some(out)
}

/// Parse a preserved `POPM` frame into its owner email and the bytes of its play counter. Returns
/// `None` if the frame is not a readable `POPM` frame.
fn parse_popm(frame: &RawFrame) -> Option<(String, Vec<u8>)> {
    if &frame.id != b"POPM" {
        return None;
    }

    // Compressed and encrypted frames cannot be parsed.
    let opaque = match frame.src_version {
        3 => frame.flags & 0x00c0,
        4 => frame.flags & 0x000c,
        _ => 0,
    };

    if opaque != 0 {
        return None;
    }

    let mut content = frame.data.get(prefix_len(frame)..)?.to_vec();

    if frame.src_version == 4 && frame.flags & 0x0002 != 0 {
        let len = decode_unsynchronisation(&mut content).len();
        content.truncate(len);
    }

    let end = content.iter().position(|&b| b == 0)?;
    content.get(end + 1)?;

    let email = content[..end].iter().map(|&b| char::from(b)).collect();
    Some((email, content[end + 2..].to_vec()))
}

/// Serialize a tag.
///
/// The new tag has the frames that [`Tag`]s and [`Visual`]s map to, followed by the unmanaged
/// frames of `existing`. The result is a complete tag (header, frames, padding) and is empty if
/// there is nothing to write.
pub fn serialize(
    tags: &[Tag],
    visuals: &[Visual],
    existing: &Existing,
    opts: &WriteOptions,
) -> Vec<u8> {
    let mut builder = Builder::new(opts);

    for tag in tags {
        match &tag.std {
            Some(std) => builder.add_std(std, &tag.raw),
            None => builder.add_free_form(&tag.raw),
        }
    }

    let ratings = std::mem::take(&mut builder.ratings);
    let mut frames = builder.finish();

    for visual in visuals {
        frames.push(apic_frame(opts.version, opts.encoding, visual));
    }

    // The `POPM` frames of the ratings, and the owners whose existing `POPM` frame they replace.
    let mut replaced: Vec<String> = Vec::new();

    if let Some(default_email) = &opts.rating_email {
        replaced.push(default_email.clone());
        let mut seen: Vec<String> = Vec::new();

        for (email, ppm) in ratings {
            let email = email.unwrap_or_else(|| default_email.clone());

            if seen.contains(&email) {
                continue;
            }

            // The play counter of the frame that is replaced is carried over.
            let counter =
                existing.frames.iter().filter_map(parse_popm).find(|(e, _)| *e == email);

            let rating = ((u64::from(ppm) * 255 + 500_000) / 1_000_000).min(255) as u8;

            let mut body = Vec::new();
            push_latin1(&mut body, &email);
            body.push(0);
            body.push(rating);

            if let Some((_, counter)) = counter {
                body.extend_from_slice(&counter);
            }

            frames.push(OutFrame::new("POPM", body));
            seen.push(email);
        }

        replaced.extend(seen);
    }

    for frame in &existing.frames {
        if !replaced.is_empty() && parse_popm(frame).is_some_and(|(e, _)| replaced.contains(&e)) {
            continue;
        }

        if let Some(frame) = convert_frame(frame, opts.version) {
            frames.push(frame);
        }
    }

    let mut body = Vec::new();

    for frame in &frames {
        frame.write(opts.version, &mut body);
    }

    if body.is_empty() {
        return Vec::new();
    }

    // Decide how much padding to write.
    let needed = 10 + body.len();

    let padding = match opts.reuse_len {
        Some(len) if len >= needed && len - needed <= opts.padding.saturating_mul(2) => {
            len - needed
        }
        _ => opts.padding,
    };

    let size = (body.len() + padding) as u32;

    let mut tag = Vec::with_capacity(10 + body.len() + padding);
    tag.extend_from_slice(b"ID3");
    tag.push(opts.version.major());
    tag.push(0);
    tag.push(0);
    tag.extend_from_slice(&encode_synchsafe(size));
    tag.extend_from_slice(&body);
    tag.resize(10 + body.len() + padding, 0);
    tag
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use symphonia_core::io::BufReader;
    use symphonia_core::meta::{MetadataBuilder, RawTag, RawTagSubField, StandardTag, Tag, Visual};

    use super::*;
    use crate::id3v2::{ID3V2_METADATA_INFO, read_id3v2};

    fn s(v: &str) -> Arc<String> {
        Arc::new(v.to_string())
    }

    fn std(tag: StandardTag) -> Tag {
        Tag::new_std(RawTag::new("", ""), tag)
    }

    fn opts(version: Version) -> WriteOptions {
        WriteOptions { version, ..Default::default() }
    }

    fn read_back(bytes: &[u8]) -> (Vec<Tag>, Vec<Visual>) {
        let mut builder = MetadataBuilder::new(ID3V2_METADATA_INFO);
        read_id3v2(&mut BufReader::new(bytes), &mut builder, &mut Vec::new()).unwrap();
        let revision = builder.build();
        (revision.media.tags, revision.media.visuals)
    }

    fn std_tags(tags: &[Tag]) -> Vec<String> {
        let mut v: Vec<String> =
            tags.iter().filter_map(|t| t.std.as_ref().map(|s| format!("{s:?}"))).collect();
        v.sort();
        v
    }

    fn expected(tags: &[Tag]) -> Vec<String> {
        std_tags(tags)
    }

    /// A set of tags that is read back unchanged from an ID3v2.4 tag.
    fn sample_tags() -> Vec<Tag> {
        let comment = Tag::new_std(
            RawTag::new_with_sub_fields(
                "COMM",
                "A comment",
                vec![
                    RawTagSubField::new(COMM_LANGUAGE, "fra"),
                    RawTagSubField::new(COMM_SHORT_DESCRIPTION, "d"),
                ]
                .into_boxed_slice(),
            ),
            StandardTag::Comment(s("A comment")),
        );

        vec![
            std(StandardTag::TrackTitle(s("Title \u{e9}"))),
            std(StandardTag::Artist(s("Artist 1"))),
            std(StandardTag::Artist(s("Artist 2"))),
            std(StandardTag::Album(s("Album \u{65e5}\u{672c}"))),
            std(StandardTag::AlbumArtist(s("Album Artist"))),
            std(StandardTag::TrackNumber(3)),
            std(StandardTag::TrackTotal(12)),
            std(StandardTag::DiscNumber(1)),
            std(StandardTag::DiscTotal(2)),
            std(StandardTag::RecordingDate(s("2004-05-06"))),
            std(StandardTag::OriginalReleaseDate(s("1999-01-02"))),
            std(StandardTag::Genre(s("Rock"))),
            std(StandardTag::Genre(s("Pop"))),
            comment,
            std(StandardTag::Lyrics(s("la la\nla"))),
            std(StandardTag::Composer(s("Composer"))),
            std(StandardTag::Bpm(120)),
            std(StandardTag::CompilationFlag(true)),
            std(StandardTag::MusicBrainzRecordingId(s("11111111-2222-3333-4444-555555555555"))),
            std(StandardTag::MusicBrainzAlbumId(s("aaaaaaaa-2222-3333-4444-555555555555"))),
            std(StandardTag::MusicBrainzReleaseTrackId(s("bbbbbbbb-2222-3333-4444-555555555555"))),
            std(StandardTag::Arranger(s("Arranger"))),
            std(StandardTag::Producer(s("Producer"))),
            std(StandardTag::Engineer(s("Engineer"))),
            std(StandardTag::MixDj(s("DJ"))),
            std(StandardTag::MixEngineer(s("Mixer"))),
            std(StandardTag::Performer(s("Performer"))),
            std(StandardTag::Grouping(s("Grouping"))),
            std(StandardTag::SortArtist(s("Artist, The"))),
            std(StandardTag::SortAlbumArtist(s("Album Artist, The"))),
            std(StandardTag::SortAlbum(s("Album, The"))),
            std(StandardTag::SortTrackTitle(s("Title, The"))),
            std(StandardTag::SortComposer(s("Composer, The"))),
            std(StandardTag::ReplayGainTrackGain(s("-6.50 dB"))),
            std(StandardTag::ReplayGainTrackPeak(s("0.988"))),
            std(StandardTag::IdentIsrc(s("USRC17607839"))),
            std(StandardTag::IdentBarcode(s("0123456789012"))),
            std(StandardTag::IdentCatalogNumber(s("CAT-1"))),
            std(StandardTag::Label(s("Label"))),
            std(StandardTag::Mood(s("Calm"))),
            std(StandardTag::Conductor(s("Conductor"))),
            std(StandardTag::Remixer(s("Remixer"))),
            std(StandardTag::Lyricist(s("Lyricist"))),
            std(StandardTag::Writer(s("Writer"))),
            std(StandardTag::Work(s("Work"))),
            std(StandardTag::InitialKey(s("Am"))),
            std(StandardTag::MediaFormat(s("CD"))),
            std(StandardTag::Language(s("eng"))),
            std(StandardTag::Copyright(s("(c) Me"))),
            std(StandardTag::EncodedBy(s("Me"))),
            std(StandardTag::Encoder(s("Encoder 1.0"))),
            std(StandardTag::TrackSubtitle(s("Sub"))),
            std(StandardTag::DiscSubtitle(s("Disc Sub"))),
            std(StandardTag::UrlArtist(s("https://example.org/artist"))),
            std(StandardTag::Url(s("https://example.org/"))),
            std(StandardTag::ReleaseCountry(s("XW"))),
            std(StandardTag::MovementName(s("Movement"))),
            std(StandardTag::MovementNumber(2)),
            std(StandardTag::MovementTotal(4)),
            std(StandardTag::PlayCounter(42)),
        ]
    }

    #[test]
    fn verify_v4_round_trip() {
        let tags = sample_tags();
        let bytes = serialize(&tags, &[], &Existing::new(), &opts(Version::V4));

        assert_eq!(&bytes[..4], b"ID3\x04");
        assert_eq!(tag_len(&bytes), Some(bytes.len()));

        let (read, visuals) = read_back(&bytes);
        assert!(visuals.is_empty());
        assert_eq!(std_tags(&read), expected(&tags));

        // The comment keeps its language and description.
        let comment = read.iter().find(|t| matches!(t.std, Some(StandardTag::Comment(_)))).unwrap();
        let sub = comment.raw.sub_fields.as_ref().unwrap();
        assert!(sub.iter().any(|f| f.field == COMM_LANGUAGE && f.value == RawValue::from("fra")));
        assert!(
            sub.iter().any(|f| f.field == COMM_SHORT_DESCRIPTION && f.value == RawValue::from("d"))
        );
    }

    #[test]
    fn verify_v3_round_trip() {
        let mut tags = sample_tags();
        // ID3v2.3 joins multiple values into one.
        tags.retain(|t| {
            !matches!(&t.std, Some(StandardTag::Artist(a)) if a.as_str() == "Artist 2")
                && !matches!(&t.std, Some(StandardTag::Genre(a)) if a.as_str() == "Pop")
        });
        // The ID3v2.3 date frames hold a date to the minute.
        tags.push(std(StandardTag::RecordingYear(2004)));

        let bytes = serialize(&tags, &[], &Existing::new(), &opts(Version::V3));
        assert_eq!(&bytes[..4], b"ID3\x03");

        let (read, _) = read_back(&bytes);
        let read = std_tags(&read);

        // The year, and the original year (TORY) that is derived from the original date, are
        // also read.
        for tag in expected(&tags) {
            assert!(read.contains(&tag), "{tag} is missing from {read:#?}");
        }

        assert!(read.contains(&format!("{:?}", StandardTag::OriginalReleaseYear(1999))));
        assert!(read.contains(&format!("{:?}", StandardTag::RecordingDate(s("2004-05-06")))));
    }

    #[test]
    fn verify_v3_text_encodings_and_joined_values() {
        let tags = [
            std(StandardTag::TrackTitle(s("Caf\u{e9}"))),
            std(StandardTag::Album(s("\u{65e5}\u{672c}\u{8a9e}"))),
            std(StandardTag::Artist(s("A"))),
            std(StandardTag::Artist(s("B"))),
        ];

        let bytes = serialize(&tags, &[], &Existing::new(), &opts(Version::V3));

        // TIT2: ISO-8859-1.
        let tit2 = bytes.windows(4).position(|w| w == b"TIT2").unwrap();
        assert_eq!(bytes[tit2 + 10], 0);
        assert_eq!(&bytes[tit2 + 11..tit2 + 15], b"Caf\xe9");

        // TALB: UTF-16 with a byte-order-mark.
        let talb = bytes.windows(4).position(|w| w == b"TALB").unwrap();
        assert_eq!(&bytes[talb + 10..talb + 13], &[1, 0xff, 0xfe]);

        let (read, _) = read_back(&bytes);
        assert!(std_tags(&read).contains(&format!("{:?}", StandardTag::Artist(s("A/B")))));
        assert!(
            std_tags(&read)
                .contains(&format!("{:?}", StandardTag::Album(s("\u{65e5}\u{672c}\u{8a9e}"))))
        );
    }

    #[test]
    fn verify_v4_multiple_values_are_nul_separated_utf8() {
        let tags = [std(StandardTag::Artist(s("A"))), std(StandardTag::Artist(s("B")))];
        let bytes = serialize(&tags, &[], &Existing::new(), &opts(Version::V4));
        assert_eq!(&bytes[10..14], b"TPE1");
        assert_eq!(&bytes[20..24], b"\x03A\0B");
    }

    #[test]
    fn verify_free_form_tags() {
        let reader_style = Tag::new(RawTag::new_with_sub_fields(
            "TXXX",
            "value 1",
            vec![RawTagSubField::new(TXXX_DESCRIPTION, "MY KEY")].into_boxed_slice(),
        ));
        let prefixed = Tag::new(RawTag::new("TXXX:Other", vec!["a".to_string(), "b".to_string()]));
        let plain = Tag::new(RawTag::new("PLAIN", "x"));
        // A frame that was read but not mapped to a standard tag is not a free-form tag.
        let frame = Tag::new(RawTag::new("TLEN", "123"));
        let popm = Tag::new(RawTag::new("POPM", 255u8));

        let tags = [reader_style, prefixed, plain, frame, popm];

        for version in [Version::V3, Version::V4] {
            let bytes = serialize(&tags, &[], &Existing::new(), &opts(version));
            let (read, _) = read_back(&bytes);

            let txxx: Vec<(String, String)> = read
                .iter()
                .map(|t| {
                    let desc = t
                        .raw
                        .sub_fields
                        .as_ref()
                        .and_then(|f| f.iter().find(|f| f.field == TXXX_DESCRIPTION))
                        .map(|f| f.value.to_string())
                        .unwrap_or_default();
                    (desc, t.raw.value.to_string())
                })
                .collect();

            assert_eq!(txxx.len(), 3, "{txxx:?}");
            assert!(txxx.iter().any(|(d, v)| d == "MY KEY" && v == "value 1"));
            assert!(txxx.iter().any(|(d, _)| d == "Other"));
            assert!(txxx.iter().any(|(d, v)| d == "PLAIN" && v == "x"));
        }
    }

    #[test]
    fn verify_visuals() {
        let visual = Visual {
            media_type: Some("image/png".to_string()),
            dimensions: None,
            color_mode: None,
            usage: Some(StandardVisualKey::FrontCover),
            tags: vec![std(StandardTag::Description(s("Cover \u{e9}")))],
            data: Box::from(&b"\x89PNG not really"[..]),
        };

        for version in [Version::V3, Version::V4] {
            let bytes =
                serialize(&[], std::slice::from_ref(&visual), &Existing::new(), &opts(version));
            let (tags, visuals) = read_back(&bytes);
            assert!(tags.is_empty());
            assert_eq!(visuals.len(), 1);
            assert_eq!(visuals[0].usage, Some(StandardVisualKey::FrontCover));
            assert_eq!(visuals[0].media_type.as_deref(), Some("image/png"));
            assert_eq!(visuals[0].data, visual.data);
            assert!(matches!(
                &visuals[0].tags[0].std,
                Some(StandardTag::Description(d)) if d.as_str() == "Cover \u{e9}"
            ));
        }
    }

    #[test]
    fn verify_empty_output() {
        assert!(serialize(&[], &[], &Existing::new(), &opts(Version::V4)).is_empty());
        let padded = WriteOptions { padding: 1024, reuse_len: Some(4096), ..Default::default() };
        assert!(serialize(&[], &[], &Existing::new(), &padded).is_empty());
    }

    #[test]
    fn verify_padding_and_reuse() {
        let tags = [std(StandardTag::TrackTitle(s("T")))];
        let plain = serialize(&tags, &[], &Existing::new(), &opts(Version::V4));

        let padded = WriteOptions { padding: 100, ..Default::default() };
        let bytes = serialize(&tags, &[], &Existing::new(), &padded);
        assert_eq!(bytes.len(), plain.len() + 100);
        assert_eq!(tag_len(&bytes), Some(bytes.len()));
        assert!(bytes[plain.len()..].iter().all(|&b| b == 0));
        read_back(&bytes);

        // The old size is reused when the new tag fits in it.
        let reuse = WriteOptions { reuse_len: Some(plain.len() + 150), ..padded.clone() };
        assert_eq!(serialize(&tags, &[], &Existing::new(), &reuse).len(), plain.len() + 150);

        // It is not reused when it does not fit, or when it wastes too much space.
        let reuse = WriteOptions { reuse_len: Some(plain.len() - 1), ..padded.clone() };
        assert_eq!(serialize(&tags, &[], &Existing::new(), &reuse).len(), plain.len() + 100);
        let reuse = WriteOptions { reuse_len: Some(plain.len() + 5000), ..padded.clone() };
        assert_eq!(serialize(&tags, &[], &Existing::new(), &reuse).len(), plain.len() + 100);
    }

    fn frame_v3(id: &[u8; 4], flags: u16, data: &[u8]) -> Vec<u8> {
        let mut f = id.to_vec();
        f.extend_from_slice(&(data.len() as u32).to_be_bytes());
        f.extend_from_slice(&flags.to_be_bytes());
        f.extend_from_slice(data);
        f
    }

    fn frame_v4(id: &[u8; 4], flags: u16, data: &[u8]) -> Vec<u8> {
        let mut f = id.to_vec();
        f.extend_from_slice(&encode_synchsafe(data.len() as u32));
        f.extend_from_slice(&flags.to_be_bytes());
        f.extend_from_slice(data);
        f
    }

    fn tag_bytes(major: u8, flags: u8, body: &[u8]) -> Vec<u8> {
        let mut t = b"ID3".to_vec();
        t.extend_from_slice(&[major, 0, flags]);
        t.extend_from_slice(&encode_synchsafe(body.len() as u32));
        t.extend_from_slice(body);
        t
    }

    /// Find the data of the first frame called `id` in a tag that was written by the serializer.
    fn find_frame(tag: &[u8], id: &[u8; 4]) -> Option<(u16, Vec<u8>)> {
        let header = parse_header(tag)?;
        let mut pos = 0;
        let body = &tag[10..10 + header.size];
        while let Some(frame) = next_frame(body, &mut pos, header.major) {
            if &frame.id == id {
                return Some((frame.flags, frame.data));
            }
        }
        None
    }

    #[test]
    fn verify_unmanaged_frames_are_preserved_across_versions() {
        let priv_data = b"owner\0\x01\x02\x03";
        let popm_data = b"me@example.org\0\xc4\0\0\0\x07";

        let mut body = Vec::new();
        body.extend(frame_v3(b"TPE1", 0, b"\0Old Artist"));
        body.extend(frame_v3(b"PRIV", 0, priv_data));
        body.extend(frame_v3(b"POPM", 0x0020, &[&[9u8][..], popm_data].concat()));
        body.extend(frame_v3(b"TLEN", 0, b"\x00123456"));
        body.extend(frame_v3(b"EQUA", 0, b"\x10abc"));
        body.extend(frame_v3(b"UFID", 0, b"http://musicbrainz.org\0abc"));
        body.extend(frame_v3(b"UFID", 0, b"other\0xyz"));
        body.extend(frame_v3(b"APIC", 0, b"\0image/png\0\x03\0data"));
        // A compressed frame.
        body.extend(frame_v3(b"GEOB", 0x0080, b"\0\0\0\x05zzzzz"));
        body.resize(body.len() + 64, 0);
        let old = tag_bytes(3, 0, &body);

        let mut existing = Existing::new();
        assert!(existing.parse_tag(&old));
        assert_eq!(existing.version(), Some(3));

        let tags = [std(StandardTag::Artist(s("New Artist")))];

        // The same version: everything unmanaged is kept as it is.
        let v3 = serialize(&tags, &[], &existing, &opts(Version::V3));
        assert_eq!(find_frame(&v3, b"PRIV"), Some((0, priv_data.to_vec())));
        assert_eq!(find_frame(&v3, b"POPM").unwrap().0, 0x0020);
        assert!(find_frame(&v3, b"TLEN").is_some());
        assert!(find_frame(&v3, b"EQUA").is_some());
        assert_eq!(find_frame(&v3, b"UFID").unwrap().1, b"other\0xyz");
        assert_eq!(find_frame(&v3, b"GEOB").unwrap().0, 0x0080);
        // The managed frames were replaced.
        assert!(find_frame(&v3, b"APIC").is_none());
        let (read, visuals) = read_back(&v3);
        assert!(visuals.is_empty());
        // The preserved POPM frame is read as a rating.
        assert_eq!(
            std_tags(&read),
            vec![
                format!("{:?}", StandardTag::Artist(s("New Artist"))),
                format!("{:?}", StandardTag::Rating(768_627)),
            ]
        );

        // A different version: the frames are converted, or dropped when they cannot be.
        let v4 = serialize(&tags, &[], &existing, &opts(Version::V4));
        assert_eq!(find_frame(&v4, b"PRIV"), Some((0, priv_data.to_vec())));
        // The grouping flag moved.
        assert_eq!(find_frame(&v4, b"POPM").unwrap().0, 0x0040);
        assert!(find_frame(&v4, b"TLEN").is_some());
        assert!(find_frame(&v4, b"EQUA").is_none());
        assert!(find_frame(&v4, b"GEOB").is_none());
        assert_eq!(find_frame(&v4, b"UFID").unwrap().1, b"other\0xyz");
    }

    #[test]
    fn verify_v4_frames_convert_to_v3() {
        let mut body = Vec::new();
        // Unsynchronised, with a data length indicator.
        body.extend(frame_v4(
            b"PRIV",
            0x0003,
            &[0, 0, 0, 6, b'o', 0, 0xff, 0x00, 0x01, 0xff, 0x00],
        ));
        body.extend(frame_v4(b"SEEK", 0, &[0, 0, 0, 1]));
        body.extend(frame_v4(b"TLEN", 0, b"\x031000"));
        body.extend(frame_v4(b"GEOB", 0x0008, b"\0\0\0\x05zzzzz"));
        let old = tag_bytes(4, 0, &body);

        let mut existing = Existing::new();
        assert!(existing.parse_tag(&old));
        assert_eq!(existing.version(), Some(4));

        let tags = [std(StandardTag::Artist(s("A")))];
        let v3 = serialize(&tags, &[], &existing, &opts(Version::V3));

        assert_eq!(find_frame(&v3, b"PRIV"), Some((0, b"o\0\xff\x01\xff".to_vec())));
        assert!(find_frame(&v3, b"SEEK").is_none());
        assert!(find_frame(&v3, b"GEOB").is_none());
        // UTF-8 text is converted to UTF-16.
        assert_eq!(
            find_frame(&v3, b"TLEN"),
            Some((0, [&[1u8, 0xff, 0xfe][..], b"1\x000\x000\x000\x00"].concat()))
        );

        // The same version: copied as they are.
        let v4 = serialize(&tags, &[], &existing, &opts(Version::V4));
        assert_eq!(find_frame(&v4, b"GEOB"), Some((0x0008, b"\0\0\0\x05zzzzz".to_vec())));
        assert_eq!(
            find_frame(&v4, b"PRIV"),
            Some((0x0003, vec![0, 0, 0, 6, b'o', 0, 0xff, 0x00, 0x01, 0xff, 0x00]))
        );
    }

    #[test]
    fn verify_tag_wide_unsynchronisation_and_extended_header_are_handled() {
        let priv_frame = frame_v3(b"PRIV", 0, b"o\0\xff\xe0\xff");
        let mut body = Vec::new();
        // ID3v2.3 extended header (6 bytes of data, no CRC).
        body.extend_from_slice(&[0, 0, 0, 6, 0, 0, 0, 0, 0, 0]);
        body.extend(&priv_frame);

        // Unsynchronise the tag.
        let mut unsync = Vec::new();
        for (i, &b) in body.iter().enumerate() {
            unsync.push(b);
            if b == 0xff && body.get(i + 1).is_some_and(|&n| n >= 0xe0 || n == 0) {
                unsync.push(0);
            }
        }
        let old = tag_bytes(3, 0xc0, &unsync);

        let mut existing = Existing::new();
        assert!(existing.parse_tag(&old));

        let v3 = serialize(&[], &[], &existing, &opts(Version::V3));
        assert_eq!(find_frame(&v3, b"PRIV"), Some((0, b"o\0\xff\xe0\xff".to_vec())));
        // The tag that is written has neither.
        assert_eq!(v3[5], 0);
    }

    #[test]
    fn verify_v22_frames_are_preserved() {
        let mut body = Vec::new();
        // TT2 (managed), CNT-like unmanaged frame: POP (popularimeter) and an unknown frame.
        body.extend_from_slice(b"TT2\0\0\x03\0Hi");
        body.extend_from_slice(b"POP\0\0\x07me\0\x80\0\0\0");
        body.extend_from_slice(b"ZZZ\0\0\x01x");
        let old = tag_bytes(2, 0, &body);

        let mut existing = Existing::new();
        assert!(existing.parse_tag(&old));
        assert_eq!(existing.version(), Some(2));

        let v4 = serialize(&[], &[], &existing, &opts(Version::V4));
        assert_eq!(find_frame(&v4, b"POPM").unwrap().1, b"me\0\x80\0\0\0");
        assert!(find_frame(&v4, b"TIT2").is_none());
    }

    #[test]
    fn verify_chapter_frames_are_converted() {
        // A CHAP frame with a TIT2 sub-frame, in ID3v2.3.
        let mut chap = b"ch1\0".to_vec();
        chap.extend_from_slice(&[
            0, 0, 0, 0, 0, 0, 0x03, 0xe8, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ]);
        chap.extend(frame_v3(b"TIT2", 0, b"\0Chapter"));
        let mut body = frame_v3(b"CHAP", 0, &chap);
        // A CTOC with a child.
        let mut ctoc = b"toc\0\x03\x01ch1\0".to_vec();
        ctoc.extend(frame_v3(b"TIT2", 0, b"\0Toc"));
        body.extend(frame_v3(b"CTOC", 0, &ctoc));
        let old = tag_bytes(3, 0, &body);

        let mut existing = Existing::new();
        assert!(existing.parse_tag(&old));

        let v4 = serialize(&[], &[], &existing, &opts(Version::V4));
        let (_, chap_data) = find_frame(&v4, b"CHAP").unwrap();
        let (_, ctoc_data) = find_frame(&v4, b"CTOC").unwrap();

        let mut expected_chap = b"ch1\0".to_vec();
        expected_chap.extend_from_slice(&chap[4..20]);
        expected_chap.extend(frame_v4(b"TIT2", 0, b"\0Chapter"));
        assert_eq!(chap_data, expected_chap);

        let mut expected_ctoc = b"toc\0\x03\x01ch1\0".to_vec();
        expected_ctoc.extend(frame_v4(b"TIT2", 0, b"\0Toc"));
        assert_eq!(ctoc_data, expected_ctoc);

        // And back: the sub-frame sizes become plain integers again.
        let mut existing = Existing::new();
        assert!(existing.parse_tag(&v4));
        let v3 = serialize(&[], &[], &existing, &opts(Version::V3));
        assert_eq!(find_frame(&v3, b"CHAP").unwrap().1, chap);
    }

    #[test]
    fn verify_trailing_terminator_is_not_a_string() {
        assert_eq!(utf8_text_to_utf16(b"A\0"), utf8_text_to_utf16(b"A"));
        assert_eq!(
            utf8_text_to_utf16(b"A\0B\0"),
            [&[1u8, 0xff, 0xfe][..], b"A\0", b"\0\0", &[0xff, 0xfe], b"B\0"].concat()
        );
    }

    #[test]
    fn verify_tag_len() {
        let tag = tag_bytes(4, 0x10, &[0u8; 20]);
        assert_eq!(tag_len(&tag), Some(10 + 20 + 10));
        let tag = tag_bytes(3, 0x10, &[0u8; 20]);
        assert_eq!(tag_len(&tag), Some(10 + 20));
        assert_eq!(tag_len(b"ID3\x04\0\0\x80\0\0\0"), None);
        assert_eq!(tag_len(b"MP3\x04\0\0\0\0\0\0"), None);
        assert_eq!(tag_len(b"ID3"), None);
    }

    #[test]
    fn verify_dates_in_v3() {
        let tags = [
            std(StandardTag::RecordingDate(s("2004-05-06T07:08"))),
            std(StandardTag::OriginalReleaseDate(s("1999"))),
        ];
        let bytes = serialize(&tags, &[], &Existing::new(), &opts(Version::V3));

        assert_eq!(find_frame(&bytes, b"TYER").unwrap().1, b"\x002004");
        assert_eq!(find_frame(&bytes, b"TDAT").unwrap().1, b"\x000605");
        assert_eq!(find_frame(&bytes, b"TIME").unwrap().1, b"\x000708");
        assert_eq!(find_frame(&bytes, b"TORY").unwrap().1, b"\x001999");
        assert!(find_frame(&bytes, b"TDRC").is_none());

        let (read, _) = read_back(&bytes);
        assert!(
            std_tags(&read)
                .contains(&format!("{:?}", StandardTag::RecordingDate(s("2004-05-06T07:08"))))
        );

        let bytes = serialize(&tags, &[], &Existing::new(), &opts(Version::V4));
        assert_eq!(find_frame(&bytes, b"TDRC").unwrap().1, b"\x032004-05-06T07:08");
        assert_eq!(find_frame(&bytes, b"TDOR").unwrap().1, b"\x031999");
        assert!(find_frame(&bytes, b"TYER").is_none());
    }

    #[test]
    fn verify_unmapped_standard_tags_are_free_form() {
        let tags = [std(StandardTag::TvEpisodeNumber(5)), std(StandardTag::Opus(s("Op. 1")))];
        let bytes = serialize(&tags, &[], &Existing::new(), &opts(Version::V4));
        let (read, _) = read_back(&bytes);
        let mut found: Vec<(String, String)> = read
            .iter()
            .map(|t| {
                let desc = t.raw.sub_fields.as_ref().unwrap()[0].value.to_string();
                (desc, t.raw.value.to_string())
            })
            .collect();
        found.sort();
        assert_eq!(
            found,
            vec![
                ("OPUS".to_string(), "Op. 1".to_string()),
                ("TV_EPISODE_NUMBER".to_string(), "5".to_string())
            ]
        );
    }

    fn encoding_of(tag: &[u8], id: &[u8; 4]) -> u8 {
        find_frame(tag, id).unwrap().1[0]
    }

    fn encoded(version: Version, encoding: TextEncoding) -> WriteOptions {
        WriteOptions { version, encoding, ..Default::default() }
    }

    #[test]
    fn verify_text_encoding_option() {
        let ascii = [std(StandardTag::TrackTitle(s("Plain")))];
        let cyrillic = [std(StandardTag::TrackTitle(s("\u{41a}\u{438}\u{43d}\u{43e} \u{20ac}")))];

        // (version, encoding, expected for ASCII, expected for non-Latin-1).
        let cases = [
            (Version::V4, TextEncoding::Auto, 3, 3),
            (Version::V3, TextEncoding::Auto, 0, 1),
            (Version::V4, TextEncoding::Utf8, 3, 3),
            (Version::V3, TextEncoding::Utf8, 1, 1),
            (Version::V4, TextEncoding::Utf16, 1, 1),
            (Version::V3, TextEncoding::Utf16, 1, 1),
            (Version::V4, TextEncoding::Latin1, 0, 1),
            (Version::V3, TextEncoding::Latin1, 0, 1),
        ];

        for (version, encoding, ascii_enc, wide_enc) in cases {
            let o = encoded(version, encoding);

            let bytes = serialize(&ascii, &[], &Existing::new(), &o);
            assert_eq!(encoding_of(&bytes, b"TIT2"), ascii_enc, "{version:?} {encoding:?}");
            let (read, _) = read_back(&bytes);
            assert_eq!(std_tags(&read), expected(&ascii));

            let bytes = serialize(&cyrillic, &[], &Existing::new(), &o);
            assert_eq!(encoding_of(&bytes, b"TIT2"), wide_enc, "{version:?} {encoding:?}");
            let (read, _) = read_back(&bytes);
            assert_eq!(std_tags(&read), expected(&cyrillic));
        }
    }

    #[test]
    fn verify_text_encoding_applies_to_every_text_frame() {
        let comment = Tag::new_std(
            RawTag::new_with_sub_fields(
                "COMM",
                "\u{43a}\u{43e}\u{43c}",
                vec![
                    RawTagSubField::new(COMM_LANGUAGE, "eng"),
                    RawTagSubField::new(COMM_SHORT_DESCRIPTION, "d"),
                ]
                .into_boxed_slice(),
            ),
            StandardTag::Comment(s("\u{43a}\u{43e}\u{43c}")),
        );

        let tags = [
            std(StandardTag::Artist(s("A"))),
            std(StandardTag::Artist(s("B"))),
            std(StandardTag::MusicBrainzAlbumId(s("id"))),
            std(StandardTag::Lyrics(s("la"))),
            std(StandardTag::Url(s("https://example.org/"))),
            comment,
        ];

        for version in [Version::V3, Version::V4] {
            for (encoding, expected_enc) in [(TextEncoding::Utf16, 1), (TextEncoding::Latin1, 0)] {
                let bytes = serialize(&tags, &[], &Existing::new(), &encoded(version, encoding));

                for id in [b"TPE1", b"TXXX", b"USLT", b"WXXX"] {
                    assert_eq!(encoding_of(&bytes, id), expected_enc, "{version:?} {encoding:?}");
                }

                // The comment has text that is not representable in ISO-8859-1.
                assert_eq!(encoding_of(&bytes, b"COMM"), 1);

                let (read, _) = read_back(&bytes);
                assert_eq!(std_tags(&read), {
                    let mut e = expected(&tags);
                    if version == Version::V3 {
                        // The artists are joined.
                        e.retain(|t| !t.starts_with("Artist"));
                        e.push(format!("{:?}", StandardTag::Artist(s("A/B"))));
                        e.sort();
                    }
                    e
                });
            }
        }
    }

    #[test]
    fn verify_utf16_v4_multiple_values_have_wide_separators() {
        let tags = [std(StandardTag::Artist(s("A"))), std(StandardTag::Artist(s("B")))];
        let bytes = serialize(&tags, &[], &Existing::new(), &encoded(Version::V4, TextEncoding::Utf16));
        let data = find_frame(&bytes, b"TPE1").unwrap().1;
        assert_eq!(data, b"\x01\xff\xfeA\0\0\0\xff\xfeB\0");
        let (read, _) = read_back(&bytes);
        assert_eq!(std_tags(&read), expected(&tags));
    }

    #[test]
    fn verify_v23_join_separator() {
        let tags = [std(StandardTag::Artist(s("A"))), std(StandardTag::Artist(s("B")))];

        for (join, joined) in [("; ", "A; B"), ("", "AB"), ("/", "A/B")] {
            let o = WriteOptions {
                version: Version::V3,
                join_v23: join.to_string(),
                ..Default::default()
            };
            let bytes = serialize(&tags, &[], &Existing::new(), &o);
            assert_eq!(find_frame(&bytes, b"TPE1").unwrap().1, [b"\0", joined.as_bytes()].concat());
        }

        // ID3v2.4 is unaffected by the separator.
        let o = WriteOptions { join_v23: "; ".to_string(), ..Default::default() };
        let bytes = serialize(&tags, &[], &Existing::new(), &o);
        assert_eq!(find_frame(&bytes, b"TPE1").unwrap().1, b"\x03A\0B");
    }

    #[test]
    fn verify_itunes_grouping() {
        let tags = [
            std(StandardTag::Grouping(s("Group"))),
            std(StandardTag::Work(s("Work"))),
        ];

        for version in [Version::V3, Version::V4] {
            let o = WriteOptions { version, itunes_grouping: true, ..Default::default() };
            let bytes = serialize(&tags, &[], &Existing::new(), &o);
            assert_eq!(find_frame(&bytes, b"GRP1").unwrap().1[1..], *b"Group");
            assert_eq!(find_frame(&bytes, b"TIT1").unwrap().1[1..], *b"Work");
            assert!(find_frame(&bytes, b"TXXX").is_none());

            let (read, _) = read_back(&bytes);
            let raw: Vec<_> = read.iter().map(|t| (t.raw.key.clone(), t.raw.value.to_string())).collect();
            assert!(raw.contains(&("GRP1".to_string(), "Group".to_string())));
            assert!(raw.contains(&("TIT1".to_string(), "Work".to_string())));

            // Without the option: TIT1 is the grouping, and the work is a TXXX frame.
            let bytes = serialize(&tags, &[], &Existing::new(), &opts(version));
            assert_eq!(find_frame(&bytes, b"TIT1").unwrap().1[1..], *b"Group");
            assert!(find_frame(&bytes, b"GRP1").is_none());
            assert!(find_frame(&bytes, b"TXXX").is_some());
        }
    }

    fn popm(email: &str, rating: u8, counter: &[u8]) -> Vec<u8> {
        [email.as_bytes(), &[0, rating], counter].concat()
    }

    fn popm_frames(tag: &[u8]) -> Vec<Vec<u8>> {
        let header = parse_header(tag).unwrap();
        let mut pos = 0;
        let body = &tag[10..10 + header.size];
        let mut out = Vec::new();
        while let Some(frame) = next_frame(body, &mut pos, header.major) {
            if &frame.id == b"POPM" {
                out.push(frame.data);
            }
        }
        out
    }

    fn rating_opts(version: Version, email: Option<&str>) -> WriteOptions {
        WriteOptions { version, rating_email: email.map(String::from), ..Default::default() }
    }

    #[test]
    fn verify_popm_reader_maps_rating() {
        let bytes = serialize(
            &[],
            &[],
            &{
                let mut e = Existing::new();
                e.parse_tag(&tag_bytes(
                    4,
                    0,
                    &frame_v4(b"POPM", 0, &popm("a@b.c", 128, &[0, 0, 0, 5])),
                ));
                e
            },
            &opts(Version::V4),
        );
        let (read, _) = read_back(&bytes);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].raw.key, "POPM");
        assert_eq!(read[0].std, Some(StandardTag::Rating(501_961)));
    }

    #[test]
    fn verify_rating_is_ignored_without_email() {
        let mut old = Vec::new();
        old.extend(frame_v4(b"POPM", 0, &popm("a@b.c", 128, &[3])));
        let mut existing = Existing::new();
        existing.parse_tag(&tag_bytes(4, 0, &old));

        let tags = [std(StandardTag::Rating(1_000_000))];
        let bytes = serialize(&tags, &[], &existing, &rating_opts(Version::V4, None));
        assert_eq!(popm_frames(&bytes), vec![popm("a@b.c", 128, &[3])]);
    }

    #[test]
    fn verify_rating_write_replace_remove() {
        let mut old = Vec::new();
        old.extend(frame_v3(b"POPM", 0, &popm("me@x.org", 64, &[0, 0, 0, 9])));
        old.extend(frame_v3(b"POPM", 0, &popm("other@x.org", 255, &[1])));
        let mut existing = Existing::new();
        existing.parse_tag(&tag_bytes(3, 0, &old));

        for version in [Version::V3, Version::V4] {
            let o = rating_opts(version, Some("me@x.org"));

            // Replaced: the counter is carried over, the other user's frame stays.
            let tags = [std(StandardTag::Rating(1_000_000))];
            let bytes = serialize(&tags, &[], &existing, &o);
            assert_eq!(
                popm_frames(&bytes),
                vec![popm("me@x.org", 255, &[0, 0, 0, 9]), popm("other@x.org", 255, &[1])]
            );
            let (read, _) = read_back(&bytes);
            assert!(read.iter().any(|t| t.std == Some(StandardTag::Rating(1_000_000))));

            // Rounded.
            let tags = [std(StandardTag::Rating(500_000))];
            let bytes = serialize(&tags, &[], &existing, &o);
            assert_eq!(popm_frames(&bytes)[0], popm("me@x.org", 128, &[0, 0, 0, 9]));

            // Removed by omitting the rating.
            let bytes = serialize(&[], &[], &existing, &o);
            assert_eq!(popm_frames(&bytes), vec![popm("other@x.org", 255, &[1])]);

            // A new owner has no counter.
            let o = rating_opts(version, Some("new@x.org"));
            let tags = [std(StandardTag::Rating(0))];
            let bytes = serialize(&tags, &[], &existing, &o);
            let frames = popm_frames(&bytes);
            assert_eq!(frames[0], popm("new@x.org", 0, &[]));
            assert_eq!(frames.len(), 3);
        }
    }

    #[test]
    fn verify_rating_with_raw_email_and_first_wins() {
        let mut old = Vec::new();
        old.extend(frame_v4(b"POPM", 0, &popm("other@x.org", 10, &[7])));
        let mut existing = Existing::new();
        existing.parse_tag(&tag_bytes(4, 0, &old));

        let other = Tag::new_std(
            RawTag::new_with_sub_fields(
                "POPM",
                255u8,
                vec![RawTagSubField::new(POPM_EMAIL, "other@x.org")].into_boxed_slice(),
            ),
            StandardTag::Rating(1_000_000),
        );
        let tags = [
            std(StandardTag::Rating(200_000)),
            std(StandardTag::Rating(900_000)),
            other,
        ];

        let bytes = serialize(&tags, &[], &existing, &rating_opts(Version::V4, Some("me@x.org")));
        assert_eq!(
            popm_frames(&bytes),
            vec![popm("me@x.org", 51, &[]), popm("other@x.org", 255, &[7])]
        );
    }
}
