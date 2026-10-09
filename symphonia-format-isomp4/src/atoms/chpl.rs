// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;

use symphonia_core::meta::{Chapter, ChapterGroup, ChapterGroupItem, RawTag, StandardTag, Tag};
use symphonia_core::units::Time;

use crate::atoms::{Atom, AtomHeader, AtomIterator, ReadAtom, Result};

/// A single chapter in a Nero chapter list.
#[derive(Debug)]
pub struct ChplEntry {
    /// The start of the chapter in units of 100 nanoseconds.
    pub start: u64,
    /// The title of the chapter.
    pub title: String,
}

/// Nero chapter list atom (`chpl`), a child of `udta`.
#[derive(Debug)]
pub struct ChplAtom {
    pub entries: Vec<ChplEntry>,
}

impl Atom for ChplAtom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, _header: &AtomHeader) -> Result<Self> {
        let (version, _) = it.read_extended_header()?;

        // Version 1 has 4 reserved bytes preceeding the chapter count.
        if version == 1 {
            it.ignore_bytes(4)?;
        }

        // The chapter count is a single byte, so at most 255 entries are read.
        let count = usize::from(it.read_u8()?);

        let mut entries = Vec::with_capacity(count);

        for _ in 0..count {
            let start = it.read_u64()?;
            let len = usize::from(it.read_u8()?);

            let mut title = vec![0; len];
            it.read_buf_exact(&mut title)?;

            entries.push(ChplEntry { start, title: String::from_utf8_lossy(&title).into_owned() });
        }

        Ok(ChplAtom { entries })
    }
}

impl ChplAtom {
    /// Convert the chapter list into a chapter group. `end` is the end time of the last chapter,
    /// if known.
    pub fn make_chapters(&self, end: Option<Time>) -> Option<ChapterGroup> {
        if self.entries.is_empty() {
            return None;
        }

        let start_time = |entry: &ChplEntry| Time::from_nanos_u64(entry.start.saturating_mul(100));

        let items = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let end_time = match self.entries.get(i + 1) {
                    Some(next) => Some(start_time(next)),
                    None => end,
                };

                let title = Arc::new(entry.title.clone());
                let raw = RawTag::new("chpl", title.clone());
                let tag = Tag::new_std(raw, StandardTag::ChapterTitle(title));

                ChapterGroupItem::Chapter(Chapter {
                    start_time: start_time(entry),
                    end_time,
                    start_byte: None,
                    end_byte: None,
                    tags: vec![tag],
                    visuals: vec![],
                })
            })
            .collect();

        Some(ChapterGroup { items, tags: vec![], visuals: vec![] })
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use symphonia_core::io::MediaSourceStream;
    use symphonia_core::meta::{ChapterGroupItem, StandardTag};

    use super::ChplAtom;
    use crate::atoms::AtomIterator;

    fn read_chpl(body: &[u8]) -> ChplAtom {
        let mut buf = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        buf.extend_from_slice(b"chpl");
        buf.extend_from_slice(body);

        let len = buf.len() as u64;
        let mss = MediaSourceStream::new(Box::new(Cursor::new(buf)), Default::default());
        let mut it = AtomIterator::new(mss, Some(len));

        assert!(it.next_header().ok().flatten().is_some(), "chpl header should be read");
        match it.read_atom::<ChplAtom>() {
            Ok(chpl) => chpl,
            Err(_) => panic!("chpl should parse"),
        }
    }

    #[test]
    fn reads_nero_chapter_list() {
        // Version 1, flags 0, reserved, 2 chapters at 0 s and 2 s (100 ns units).
        let mut body = vec![1, 0, 0, 0, 0, 0, 0, 0, 2];
        body.extend_from_slice(&0u64.to_be_bytes());
        body.push(5);
        body.extend_from_slice(b"Intro");
        body.extend_from_slice(&20_000_000u64.to_be_bytes());
        body.push(6);
        body.extend_from_slice(b"Middle");

        let chpl = read_chpl(&body);
        assert_eq!(chpl.entries.len(), 2);

        let group = chpl.make_chapters(None).expect("chapters");
        assert_eq!(group.items.len(), 2);

        let ChapterGroupItem::Chapter(first) = &group.items[0]
        else {
            panic!("expected a chapter");
        };
        assert_eq!(first.start_time.as_millis(), 0);
        assert_eq!(first.end_time.map(|t| t.as_millis()), Some(2000));
        assert!(matches!(
            first.tags[0].std.as_ref(),
            Some(StandardTag::ChapterTitle(title)) if title.as_str() == "Intro"
        ));

        let ChapterGroupItem::Chapter(second) = &group.items[1]
        else {
            panic!("expected a chapter");
        };
        assert_eq!(second.start_time.as_millis(), 2000);
        assert!(second.end_time.is_none());
    }

    #[test]
    fn truncated_chapter_list_is_an_error() {
        // Declares 3 chapters, but has none.
        let body = vec![1, 0, 0, 0, 0, 0, 0, 0, 3];

        let mut buf = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        buf.extend_from_slice(b"chpl");
        buf.extend_from_slice(&body);

        let len = buf.len() as u64;
        let mss = MediaSourceStream::new(Box::new(Cursor::new(buf)), Default::default());
        let mut it = AtomIterator::new(mss, Some(len));

        assert!(it.next_header().ok().flatten().is_some());
        assert!(it.read_atom::<ChplAtom>().is_err());
    }
}
