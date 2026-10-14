// DSDIFF metadata chunk parsers.
//
// Implements the metadata chunks of "Direct Stream Digital Interchange File Format" v1.5
// (Philips, 2004-04-27): COMT (3.6), DIIN with EMID/MARK/DIAR/DITI (3.7), MANF (3.8), and the
// ABSS (3.2.4) and LSCO (3.2.5) property chunks. All numeric fields are big-endian.
//
// The parsers operate on the in-memory body of a chunk, so they can never read past the end of the
// chunk they were given, whatever the sizes written in the file claim.

use std::sync::Arc;

use symphonia_core::common::FourCc;
use symphonia_core::formats::prelude::TimeBase;
use symphonia_core::meta::{
    Chapter, ChapterGroup, ChapterGroupItem, MetadataBuilder, MetadataId, MetadataInfo,
    MetadataRevision, RawTag, RawTagSubField, RawValue, StandardTag, Tag,
};
use symphonia_core::units::Timestamp;

/// Length in bytes of a DSDIFF chunk header (4-byte id and 8-byte size).
pub(crate) const DSDIFF_CHUNK_HEADER_LEN: usize = 12;

/// A cursor over a byte slice that reads big-endian integers without ever panicking.
pub(crate) struct Be<'a> {
    buf: &'a [u8],
}

impl<'a> Be<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Be { buf }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.buf.len()
    }

    pub(crate) fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.buf.len() < n {
            return None;
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Some(head)
    }

    pub(crate) fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.buf)
    }

    pub(crate) fn u8(&mut self) -> Option<u8> {
        self.bytes(1).map(|b| b[0])
    }

    pub(crate) fn u16(&mut self) -> Option<u16> {
        self.bytes(2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }

    pub(crate) fn u32(&mut self) -> Option<u32> {
        self.bytes(4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub(crate) fn i32(&mut self) -> Option<i32> {
        self.u32().map(|v| v as i32)
    }

    pub(crate) fn u64(&mut self) -> Option<u64> {
        self.bytes(8).map(|b| u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }

    pub(crate) fn id(&mut self) -> Option<[u8; 4]> {
        self.bytes(4).map(|b| [b[0], b[1], b[2], b[3]])
    }
}

/// An iterator over the chunks (4-byte id, 64-bit size, data, pad byte if the size is odd) in a
/// byte buffer. A chunk whose declared size exceeds the buffer yields the bytes that are present
/// and is the last chunk.
pub(crate) struct Chunks<'a> {
    buf: &'a [u8],
}

pub(crate) fn chunks(buf: &[u8]) -> Chunks<'_> {
    Chunks { buf }
}

impl<'a> Iterator for Chunks<'a> {
    type Item = ([u8; 4], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let mut r = Be::new(self.buf);
        let (Some(id), Some(size)) = (r.id(), r.u64())
        else {
            self.buf = &[];
            return None;
        };
        let body = r.rest();
        match usize::try_from(size) {
            Ok(len) if len <= body.len() => {
                self.buf = body.get(len + (len & 1)..).unwrap_or(&[]);
                Some((id, &body[..len]))
            }
            _ => {
                self.buf = &[];
                Some((id, body))
            }
        }
    }
}

/// Decode text of unspecified encoding. DSDIFF does not specify a character set: use UTF-8 when the
/// bytes are valid UTF-8 and ISO 8859-1 otherwise.
pub(crate) fn decode_text(bytes: &[u8]) -> String {
    let s = match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| char::from(b)).collect(),
    };
    s.trim_end_matches(['\0']).trim().to_string()
}

/// A time on the absolute time axis of the file (ABSS and MARK chunks), `hours:minutes:seconds`
/// plus a number of samples at the DSD sample rate.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AbsTime {
    pub hours: u16,
    pub minutes: u8,
    pub seconds: u8,
    pub samples: u32,
}

impl AbsTime {
    /// Parse the ABSS chunk body.
    pub(crate) fn parse(body: &[u8]) -> Option<AbsTime> {
        let mut r = Be::new(body);
        Some(AbsTime { hours: r.u16()?, minutes: r.u8()?, seconds: r.u8()?, samples: r.u32()? })
    }

    /// The time in DSD samples (per channel) at `rate` Hz.
    pub(crate) fn to_samples(self, rate: u32) -> i128 {
        let secs = (i128::from(self.hours) * 60 + i128::from(self.minutes)) * 60
            + i128::from(self.seconds);
        secs * i128::from(rate) + i128::from(self.samples)
    }

    pub(crate) fn describe(self) -> String {
        format!("{:02}:{:02}:{:02}:{}", self.hours, self.minutes, self.seconds, self.samples)
    }
}

/// A marker (MARK chunk, DSDIFF 3.7.2).
#[derive(Clone, Debug)]
pub(crate) struct Marker {
    pub time: AbsTime,
    /// Offset in samples with respect to `time`.
    pub offset: i32,
    pub mark_type: u16,
    pub channel: u16,
    pub flags: u16,
    pub text: String,
}

impl Marker {
    fn mark_type_name(&self) -> String {
        match self.mark_type {
            0 => "TrackStart".to_string(),
            1 => "TrackStop".to_string(),
            2 => "ProgramStart".to_string(),
            4 => "Index".to_string(),
            n => format!("Type{n}"),
        }
    }
}

/// Everything read from the metadata chunks of a DSDIFF file.
#[derive(Default)]
pub(crate) struct DffMeta {
    pub tags: Vec<Tag>,
    pub markers: Vec<Marker>,
}

fn str_tag(key: &str, value: &str, std: Option<StandardTag>) -> Tag {
    Tag::new_from_parts(key, value, std)
}

/// Describe a COMT comment reference for the given comment type (DSDIFF 3.6).
fn comment_kind(cmt_type: u16, cmt_ref: u16) -> (&'static str, String) {
    match cmt_type {
        0 => ("COMMENT", String::new()),
        1 => {
            ("CHANNEL_COMMENT", if cmt_ref == 0 { "all".to_string() } else { cmt_ref.to_string() })
        }
        2 => (
            "SOUND_SOURCE",
            match cmt_ref {
                0 => "DSD recording".to_string(),
                1 => "Analogue recording".to_string(),
                2 => "PCM recording".to_string(),
                n => n.to_string(),
            },
        ),
        3 => (
            "FILE_HISTORY",
            match cmt_ref {
                0 => "General remark".to_string(),
                1 => "Operator".to_string(),
                2 => "Machine".to_string(),
                3 => "Time zone".to_string(),
                4 => "Revision".to_string(),
                n => n.to_string(),
            },
        ),
        _ => ("COMMENT", format!("type {cmt_type} ref {cmt_ref}")),
    }
}

impl DffMeta {
    /// Parse a DIIN (Edited Master Information) chunk body: EMID, MARK, DIAR and DITI chunks.
    pub(crate) fn parse_diin(&mut self, body: &[u8]) {
        for (id, data) in chunks(body) {
            match &id {
                b"DITI" | b"DIAR" => {
                    let mut r = Be::new(data);
                    let Some(count) = r.u32()
                    else {
                        continue;
                    };
                    // The count is the length of the text, which excludes a pad byte. Be lenient
                    // with writers that wrote a count larger than the chunk.
                    let text = decode_text(&r.rest()[..(count as usize).min(data.len() - 4)]);
                    if !text.is_empty() {
                        let tag = if &id == b"DITI" {
                            str_tag(
                                "TITLE",
                                &text,
                                Some(StandardTag::TrackTitle(Arc::new(text.clone()))),
                            )
                        }
                        else {
                            str_tag(
                                "ARTIST",
                                &text,
                                Some(StandardTag::Artist(Arc::new(text.clone()))),
                            )
                        };
                        self.tags.push(tag);
                    }
                }
                b"EMID" => {
                    let value = match std::str::from_utf8(data) {
                        Ok(s) => RawValue::from(s.trim_end_matches('\0')),
                        Err(_) => RawValue::from(data),
                    };
                    self.tags.push(Tag::new(RawTag::new("EDITED_MASTER_ID", value)));
                }
                b"MARK" => self.parse_mark(data),
                _ => (),
            }
        }
    }

    fn parse_mark(&mut self, data: &[u8]) {
        let mut r = Be::new(data);
        let (
            Some(hours),
            Some(minutes),
            Some(seconds),
            Some(samples),
            Some(offset),
            Some(mark_type),
            Some(channel),
            Some(flags),
            Some(count),
        ) = (r.u16(), r.u8(), r.u8(), r.u32(), r.i32(), r.u16(), r.u16(), r.u16(), r.u32())
        else {
            return;
        };
        let text_len = (count as usize).min(r.remaining());
        let text = decode_text(&r.rest()[..text_len]);
        self.markers.push(Marker {
            time: AbsTime { hours, minutes, seconds, samples },
            offset,
            mark_type,
            channel,
            flags,
            text,
        });
    }

    /// Parse a COMT (Comments) chunk body.
    pub(crate) fn parse_comt(&mut self, body: &[u8]) {
        let mut r = Be::new(body);
        let Some(num) = r.u16()
        else {
            return;
        };
        for _ in 0..num {
            let (
                Some(year),
                Some(month),
                Some(day),
                Some(hour),
                Some(minutes),
                Some(cmt_type),
                Some(cmt_ref),
                Some(count),
            ) = (r.u16(), r.u8(), r.u8(), r.u8(), r.u8(), r.u16(), r.u16(), r.u32())
            else {
                return;
            };
            let len = (count as usize).min(r.remaining());
            let Some(text_bytes) = r.bytes(len)
            else {
                return;
            };
            // Each comment is padded to an even length.
            if len & 1 == 1 {
                let _ = r.u8();
            }
            let text = decode_text(text_bytes);
            if text.is_empty() {
                continue;
            }
            let (key, reference) = comment_kind(cmt_type, cmt_ref);
            let mut sub_fields = Vec::new();
            if !reference.is_empty() {
                sub_fields.push(RawTagSubField::new("REFERENCE", reference));
            }
            // Writers without a real-time clock count comments in the year field with the other
            // fields zero (DSDIFF 3.6); only a real date is worth reporting.
            if month != 0 || day != 0 || year >= 2000 {
                sub_fields.push(RawTagSubField::new(
                    "DATE",
                    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minutes:02}"),
                ));
            }
            let std = (cmt_type == 0).then(|| StandardTag::Comment(Arc::new(text.clone())));
            let raw = if sub_fields.is_empty() {
                RawTag::new(key, text)
            }
            else {
                RawTag::new_with_sub_fields(key, text, sub_fields.into_boxed_slice())
            };
            self.tags.push(Tag { raw, std });
        }
    }

    /// Parse a MANF (Manufacturer Specific) chunk body: a manufacturer ID and opaque data.
    pub(crate) fn parse_manf(&mut self, body: &[u8]) {
        let mut r = Be::new(body);
        let Some(man_id) = r.id()
        else {
            return;
        };
        let id_text = decode_text(&man_id);
        let data: Box<[u8]> = Box::from(r.rest());
        let sub_fields = vec![RawTagSubField::new("DATA", data)].into_boxed_slice();
        self.tags.push(Tag::new(RawTag::new_with_sub_fields("MANUFACTURER", id_text, sub_fields)));
    }

    /// Record the absolute start time (ABSS) as a tag.
    pub(crate) fn add_abss(&mut self, abss: AbsTime) {
        self.tags.push(str_tag("ABSOLUTE_START_TIME", &abss.describe(), None));
    }

    /// Record the loudspeaker configuration (LSCO) as a tag.
    pub(crate) fn add_lsco(&mut self, lsco: u16) {
        let text = match lsco {
            0 => "2-channel stereo",
            3 => "5-channel (ITU-R BS.775-1)",
            4 => "5.1 (ITU-R BS.775-1 plus LFE)",
            65535 => "undefined",
            _ => "reserved",
        };
        self.tags.push(str_tag("LOUDSPEAKER_CONFIGURATION", text, None));
    }

    /// Build the metadata revision, if there are any tags.
    pub(crate) fn revision(&mut self) -> Option<MetadataRevision> {
        if self.tags.is_empty() {
            return None;
        }
        let mut builder = MetadataBuilder::new(MetadataInfo {
            metadata: MetadataId::new(FourCc::new(*b"DFFI")),
            short_name: "dff_info",
            long_name: "DSDIFF INFO",
        });
        for tag in self.tags.drain(..) {
            builder.add_tag(tag);
        }
        Some(builder.build())
    }

    /// Convert the markers into chapters.
    ///
    /// The position of a marker is its time plus its offset, on the absolute time axis (DSDIFF
    /// 3.7.2), so the start time of the sound data (ABSS) is subtracted. A TrackStart marker ends at
    /// the next TrackStart or TrackStop marker (DSDIFF, "Creating Tracks from Markers").
    pub(crate) fn chapters(
        &self,
        rate: u32,
        abs_start: AbsTime,
        time_base: TimeBase,
    ) -> Option<ChapterGroup> {
        if self.markers.is_empty() {
            return None;
        }
        let origin = abs_start.to_samples(rate);
        let mut positions: Vec<(i128, &Marker)> = self
            .markers
            .iter()
            .map(|m| (m.time.to_samples(rate) + i128::from(m.offset) - origin, m))
            .collect();
        positions.sort_by_key(|(pos, _)| *pos);

        let to_time = |pos: i128| {
            let ts = i64::try_from(pos).ok()?;
            time_base.calc_time(Timestamp::new(ts))
        };

        let mut items = Vec::with_capacity(positions.len());
        for (i, (pos, marker)) in positions.iter().enumerate() {
            let Some(start_time) = to_time(*pos)
            else {
                continue;
            };
            // Only a Track has a defined end.
            let end_time = (marker.mark_type == 0)
                .then(|| {
                    positions[i + 1..]
                        .iter()
                        .find(|(_, next)| next.mark_type <= 1)
                        .and_then(|(next_pos, _)| to_time(*next_pos))
                })
                .flatten();

            let mut tags = Vec::new();
            if !marker.text.is_empty() {
                tags.push(Tag::new_std(
                    RawTag::new("TITLE", marker.text.as_str()),
                    StandardTag::ChapterTitle(Arc::new(marker.text.clone())),
                ));
            }
            tags.push(str_tag("MARKER_TYPE", &marker.mark_type_name(), None));
            if marker.channel != 0 {
                tags.push(Tag::new(RawTag::new("MARKER_CHANNEL", marker.channel)));
            }
            if marker.flags != 0 {
                tags.push(Tag::new(RawTag::new("TRACK_FLAGS", marker.flags)));
            }
            items.push(ChapterGroupItem::Chapter(Chapter {
                start_time,
                end_time,
                start_byte: None,
                end_byte: None,
                tags,
                visuals: Vec::new(),
            }));
        }
        if items.is_empty() {
            return None;
        }
        Some(ChapterGroup { items, tags: Vec::new(), visuals: Vec::new() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        v.extend_from_slice(&(body.len() as u64).to_be_bytes());
        v.extend_from_slice(body);
        if body.len() % 2 == 1 {
            v.push(0);
        }
        v
    }

    #[test]
    fn verify_chunks_iterator_clamps_oversized_chunk() {
        let mut buf = b"ABCD".to_vec();
        buf.extend_from_slice(&1000u64.to_be_bytes());
        buf.extend_from_slice(b"xyz");
        let items: Vec<_> = chunks(&buf).collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].1, b"xyz");
    }

    #[test]
    fn verify_diin_text_count_exceeding_subchunk_is_clamped() {
        let mut body = 100u32.to_be_bytes().to_vec();
        body.extend_from_slice(b"abcd");
        let mut meta = DffMeta::default();
        meta.parse_diin(&chunk(b"DITI", &body));
        assert_eq!(meta.tags.len(), 1);
        assert_eq!(meta.tags[0].raw.key, "TITLE");
    }

    #[test]
    fn verify_comt_types_are_mapped() {
        let mut body = 2u16.to_be_bytes().to_vec();
        for (ty, rf, text) in [(0u16, 0u16, "general"), (2, 1, "tape")] {
            body.extend_from_slice(&[0x07, 0xE8, 3, 4, 5, 6]);
            body.extend_from_slice(&ty.to_be_bytes());
            body.extend_from_slice(&rf.to_be_bytes());
            body.extend_from_slice(&(text.len() as u32).to_be_bytes());
            body.extend_from_slice(text.as_bytes());
            if text.len() % 2 == 1 {
                body.push(0);
            }
        }
        let mut meta = DffMeta::default();
        meta.parse_comt(&body);
        assert_eq!(meta.tags.len(), 2);
        assert_eq!(meta.tags[0].raw.key, "COMMENT");
        assert!(matches!(meta.tags[0].std, Some(StandardTag::Comment(_))));
        assert_eq!(meta.tags[1].raw.key, "SOUND_SOURCE");
        let subs = meta.tags[1].raw.sub_fields.as_ref().unwrap();
        assert_eq!(subs[0].field, "REFERENCE");
        assert_eq!(subs[1].field, "DATE");
    }

    #[test]
    fn verify_marker_chapters_use_abss_and_track_ends() {
        fn mark(h: u16, m: u8, s: u8, samples: u32, offset: i32, ty: u16, text: &str) -> Vec<u8> {
            let mut b = h.to_be_bytes().to_vec();
            b.extend_from_slice(&[m, s]);
            b.extend_from_slice(&samples.to_be_bytes());
            b.extend_from_slice(&offset.to_be_bytes());
            b.extend_from_slice(&ty.to_be_bytes());
            b.extend_from_slice(&0u16.to_be_bytes());
            b.extend_from_slice(&0u16.to_be_bytes());
            b.extend_from_slice(&(text.len() as u32).to_be_bytes());
            b.extend_from_slice(text.as_bytes());
            chunk(b"MARK", &b)
        }
        let mut diin = Vec::new();
        // ABSS is 10:00:00; the markers are absolute times.
        diin.extend(mark(10, 0, 1, 0, 0, 0, "One"));
        diin.extend(mark(10, 0, 3, 0, -150_528, 0, "Two"));
        diin.extend(mark(10, 3, 33, 0, 0, 1, ""));
        let mut meta = DffMeta::default();
        meta.parse_diin(&diin);
        assert_eq!(meta.markers.len(), 3);

        let abss = AbsTime { hours: 10, ..Default::default() };
        let tb = TimeBase::try_from_recip(2_822_400).unwrap();
        let group = meta.chapters(2_822_400, abss, tb).unwrap();
        let chapters: Vec<_> = group
            .items
            .iter()
            .map(|i| match i {
                ChapterGroupItem::Chapter(c) => c,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(chapters.len(), 3);
        // Chapter "One" starts 1 s after the start of the data and ends where "Two" starts:
        // 3 s minus 150528 samples (53.33 ms) = 2.94667 s.
        assert_eq!(chapters[0].start_time.as_secs(), 1);
        let end = chapters[0].end_time.unwrap();
        assert_eq!(end.as_millis(), 2946);
        assert_eq!(chapters[1].start_time.as_millis(), 2946);
        assert_eq!(chapters[1].end_time.unwrap().as_secs(), 213);
    }
}
