// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Auxiliary WAVE chunks: Broadcast Wave `bext` (EBU Tech 3285), cue points (`cue ` and the `adtl`
//! list, Microsoft RIFF specification, "Multimedia Programmer's Reference"), and chunks that are
//! preserved as vendor data (`smpl`, `inst`, `iXML`, `axml`, `cart`, `acid`, `_PMX`).

use std::num::NonZero;
use std::sync::Arc;

use symphonia_core::errors::Result;
use symphonia_core::formats::prelude::*;
use symphonia_core::io::ReadBytes;
use symphonia_core::meta::{
    Chapter, ChapterGroup, ChapterGroupItem, MetadataBuilder, MetadataLog, MetadataRevision,
    StandardTag, Tag,
};
use symphonia_core::units::{TimeBase, Timestamp};
use symphonia_core::util::text;

use crate::common::{ByteOrder, ChunkParser, ChunksReader, ParseChunk, ParseChunkTag};

use super::WAVE_METADATA_INFO;

/// The maximum length of a chunk that will be buffered.
const MAX_EXTRA_CHUNK_LEN: u32 = 16 * 1024 * 1024;
/// The maximum length of an `adtl` sub-chunk that will be buffered.
const MAX_ADTL_CHUNK_LEN: u32 = 1024 * 1024;
/// The maximum number of cue points that will be read.
const MAX_CUE_POINTS: usize = 65_536;

/// Returns true if `tag` identifies a chunk handled by [`WaveExtras::add_chunk`] that is small
/// enough to be buffered.
pub fn is_extra_chunk(tag: &[u8; 4], len: u32) -> bool {
    len <= MAX_EXTRA_CHUNK_LEN
        && matches!(
            tag,
            b"bext" | b"cue " | b"smpl" | b"inst" | b"iXML" | b"axml" | b"cart" | b"acid" | b"_PMX"
        )
}

/// A whole chunk, buffered.
pub struct BufChunk {
    pub tag: [u8; 4],
    pub buf: Box<[u8]>,
}

impl ParseChunk for BufChunk {
    fn parse<B: ReadBytes>(reader: &mut B, tag: [u8; 4], len: u32) -> Result<Self> {
        Ok(BufChunk { tag, buf: reader.read_boxed_slice_exact(len as usize)? })
    }
}

/// The kind of an entry of an `adtl` list.
#[derive(Copy, Clone, PartialEq, Eq)]
enum AdtlKind {
    /// `labl`: the label (title) of a cue point.
    Label,
    /// `note`: a comment on a cue point.
    Note,
    /// `ltxt`: text associated with a section of the audio starting at a cue point.
    LabeledText,
}

struct AdtlEntry {
    kind: AdtlKind,
    /// The ID of the cue point the entry belongs to.
    cue_id: u32,
    /// The length of the section starting at the cue point, in frames. `ltxt` only.
    length: u32,
    text: String,
}

/// An `adtl` sub-chunk.
pub struct AdtlChunk {
    entry: Option<AdtlEntry>,
}

impl ParseChunk for AdtlChunk {
    fn parse<B: ReadBytes>(reader: &mut B, tag: [u8; 4], len: u32) -> Result<Self> {
        // Skip entries that are implausibly long.
        if len > MAX_ADTL_CHUNK_LEN {
            reader.ignore_bytes(u64::from(len))?;
            return Ok(AdtlChunk { entry: None });
        }

        let buf = reader.read_boxed_slice_exact(len as usize)?;

        let (kind, header_len) = match &tag {
            b"labl" => (AdtlKind::Label, 4),
            b"note" => (AdtlKind::Note, 4),
            // cue point ID, sample length, purpose ID, country, language, dialect, code page.
            _ => (AdtlKind::LabeledText, 20),
        };

        // Malformed entries are ignored.
        if buf.len() < header_len {
            return Ok(AdtlChunk { entry: None });
        }

        let cue_id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let length = match kind {
            AdtlKind::LabeledText => u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            _ => 0,
        };

        let text = decode_text(&buf[header_len..]);

        Ok(AdtlChunk { entry: Some(AdtlEntry { kind, cue_id, length, text }) })
    }
}

enum AdtlChunks {
    Entry(ChunkParser<AdtlChunk>),
}

impl ParseChunkTag for AdtlChunks {
    fn parse_tag(tag: [u8; 4], len: u32) -> Option<Self> {
        match &tag {
            b"labl" | b"note" | b"ltxt" => {
                Some(AdtlChunks::Entry(ChunkParser::<AdtlChunk>::new(tag, len)))
            }
            _ => None,
        }
    }
}

struct CuePoint {
    id: u32,
    /// The position of the cue point, in frames from the start of the audio.
    frame: u64,
}

/// Cue points, and the chunks preserved as vendor data, collected while reading a WAVE file.
#[derive(Default)]
pub struct WaveExtras {
    cues: Vec<CuePoint>,
    adtl: Vec<AdtlEntry>,
    pub attachments: Vec<Attachment>,
}

impl WaveExtras {
    /// Processes a chunk identified by [`is_extra_chunk`].
    pub fn add_chunk(&mut self, chunk: BufChunk, metadata: &mut MetadataLog) {
        match &chunk.tag {
            b"bext" => metadata.push(parse_bext(&chunk.buf)),
            b"cue " => self.parse_cue(&chunk.buf),
            tag => self.attachments.push(Attachment::VendorData(VendorDataAttachment {
                ident: String::from_utf8_lossy(tag).into_owned(),
                data: chunk.buf,
            })),
        }
    }

    /// Reads the sub-chunks of an `adtl` list. `len` is the length of the list excluding the
    /// list form type.
    pub fn read_adtl_list<B: ReadBytes>(&mut self, reader: &mut B, len: u32) -> Result<()> {
        let mut list = ChunksReader::<AdtlChunks>::new(Some(len), ByteOrder::LittleEndian);

        while let Some(AdtlChunks::Entry(chunk)) = list.next(reader)? {
            if let Some(entry) = chunk.parse(reader)?.entry {
                self.adtl.push(entry);
            }
        }

        list.finish(reader)
    }

    fn parse_cue(&mut self, buf: &[u8]) {
        if buf.len() < 4 {
            return;
        }

        let count = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;

        // Each cue point is 24 bytes: ID, position, data chunk ID, chunk start, block start, and
        // sample offset.
        for point in buf[4..].chunks_exact(24).take(count.min(MAX_CUE_POINTS)) {
            let id = u32::from_le_bytes([point[0], point[1], point[2], point[3]]);
            let position = u32::from_le_bytes([point[4], point[5], point[6], point[7]]);
            let sample_offset = u32::from_le_bytes([point[20], point[21], point[22], point[23]]);

            // For a cue point in the "data" chunk, the sample offset is the offset of the frame
            // from the start of the chunk. Otherwise, only the playlist position is meaningful.
            let frame = if &point[8..12] == b"data" { sample_offset } else { position };

            self.cues.push(CuePoint { id, frame: u64::from(frame) });
        }
    }

    /// Builds the chapters from the cue points and the labels attached to them.
    pub fn chapters(&self, sample_rate: u32) -> Option<ChapterGroup> {
        let tb = TimeBase::from_recip(NonZero::new(sample_rate)?);

        let time_of = |frame: u64| tb.calc_time(Timestamp::try_from(frame).ok()?);

        let mut chapters = Vec::with_capacity(self.cues.len());

        for cue in &self.cues {
            let Some(start_time) = time_of(cue.frame)
            else {
                continue;
            };

            let mut end_time = None;
            let mut tags = Vec::new();

            for entry in self.adtl.iter().filter(|entry| entry.cue_id == cue.id) {
                let value = Arc::new(entry.text.clone());

                match entry.kind {
                    AdtlKind::Label => tags.push(Tag::new_from_parts(
                        "labl",
                        value.clone(),
                        Some(StandardTag::TrackTitle(value)),
                    )),
                    AdtlKind::Note => tags.push(Tag::new_from_parts(
                        "note",
                        value.clone(),
                        Some(StandardTag::Comment(value)),
                    )),
                    AdtlKind::LabeledText => {
                        if end_time.is_none() && entry.length > 0 {
                            end_time = time_of(cue.frame + u64::from(entry.length));
                        }
                        tags.push(Tag::new_from_parts("ltxt", value, None));
                    }
                }
            }

            chapters.push(ChapterGroupItem::Chapter(Chapter {
                start_time,
                end_time,
                start_byte: None,
                end_byte: None,
                tags,
                visuals: vec![],
            }));
        }

        if chapters.is_empty() {
            None
        }
        else {
            Some(ChapterGroup { items: chapters, tags: vec![], visuals: vec![] })
        }
    }
}

/// Decodes a NUL-terminated or NUL-padded string. Text is ASCII per the specifications, but
/// taggers write UTF-8 and legacy software Latin-1.
fn decode_text(buf: &[u8]) -> String {
    let buf = buf.split(|&b| b == 0).next().unwrap_or_default();

    match std::str::from_utf8(buf) {
        Ok(text) => text.to_string(),
        Err(_) => text::decode_iso8859_1_lossy(buf).collect(),
    }
}

/// Parses the Broadcast Wave Format extension chunk, `bext` (EBU Tech 3285 v1 and v2).
///
/// Layout: Description (256), Originator (32), OriginatorReference (32), OriginationDate (10),
/// OriginationTime (8), TimeReference (u64), Version (u16), UMID (64), LoudnessValue (i16),
/// LoudnessRange (i16), MaxTruePeakLevel (i16), MaxMomentaryLoudness (i16),
/// MaxShortTermLoudness (i16), 180 reserved bytes, and CodingHistory (the remainder).
fn parse_bext(buf: &[u8]) -> MetadataRevision {
    const DESCRIPTION: usize = 0;
    const ORIGINATOR: usize = DESCRIPTION + 256;
    const ORIGINATOR_REFERENCE: usize = ORIGINATOR + 32;
    const ORIGINATION_DATE: usize = ORIGINATOR_REFERENCE + 32;
    const ORIGINATION_TIME: usize = ORIGINATION_DATE + 10;
    const TIME_REFERENCE: usize = ORIGINATION_TIME + 8;
    const VERSION: usize = TIME_REFERENCE + 8;
    const UMID: usize = VERSION + 2;
    const LOUDNESS: usize = UMID + 64;
    const CODING_HISTORY: usize = LOUDNESS + 10 + 180;

    /// The value of a loudness field that has not been set.
    const LOUDNESS_UNSET: i16 = 0x7fff;

    let mut builder = MetadataBuilder::new(WAVE_METADATA_INFO);

    let field = |start: usize, len: usize| -> Option<String> {
        let text = decode_text(buf.get(start..start + len)?);
        let text = text.trim_end();
        (!text.is_empty()).then(|| text.to_string())
    };

    let mut add_text = |key: &str, value: Option<String>, std: fn(Arc<String>) -> StandardTag| {
        if let Some(value) = value {
            let value = Arc::new(value);
            builder.add_tag(Tag::new_from_parts(key, value.clone(), Some(std(value))));
        }
    };

    add_text("description", field(DESCRIPTION, 256), StandardTag::Description);

    let mut add_raw = |key: &str, value: Option<String>| {
        if let Some(value) = value {
            builder.add_tag(Tag::new_from_parts(key, value, None));
        }
    };

    add_raw("originator", field(ORIGINATOR, 32));
    add_raw("originator_reference", field(ORIGINATOR_REFERENCE, 32));
    add_raw("origination_date", field(ORIGINATION_DATE, 10));
    add_raw("origination_time", field(ORIGINATION_TIME, 8));

    if let Some(time_reference) = buf.get(TIME_REFERENCE..TIME_REFERENCE + 8) {
        let time_reference = u64::from_le_bytes(time_reference.try_into().expect("8 bytes"));
        add_raw("time_reference", Some(time_reference.to_string()));
    }

    let version = buf
        .get(VERSION..VERSION + 2)
        .map(|version| u16::from_le_bytes([version[0], version[1]]))
        .unwrap_or(0);

    // The UMID was introduced in version 1.
    if version >= 1 {
        if let Some(umid) = buf.get(UMID..UMID + 64).filter(|umid| umid.iter().any(|&b| b != 0)) {
            add_raw("umid", Some(umid.iter().map(|b| format!("{b:02x}")).collect()));
        }
    }

    // The loudness fields were introduced in version 2, and are in units of 0.01 LU (LUFS or dB).
    if version >= 2 {
        for (i, key) in [
            "loudness_value",
            "loudness_range",
            "max_true_peak_level",
            "max_momentary_loudness",
            "max_short_term_loudness",
        ]
        .into_iter()
        .enumerate()
        {
            let start = LOUDNESS + i * 2;

            if let Some(raw) = buf.get(start..start + 2) {
                let raw = i16::from_le_bytes([raw[0], raw[1]]);

                if raw != LOUDNESS_UNSET {
                    add_raw(key, Some(format!("{:.2}", f64::from(raw) / 100.0)));
                }
            }
        }
    }

    if let Some(history) = buf.get(CODING_HISTORY..) {
        let history = decode_text(history);
        let history = history.trim_end();
        add_raw("coding_history", (!history.is_empty()).then(|| history.to_string()));
    }

    builder.build()
}
