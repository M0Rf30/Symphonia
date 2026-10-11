// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Combination of the ID3v2.2/v2.3 split date frames into a single recording date.
//!
//! ID3v2.2 and ID3v2.3 store the recording date across multiple frames: the year (`TYER`/`TYE`),
//! the day and month as `DDMM` (`TDAT`/`TDA`), and the time as `HHMM` (`TIME`/`TIM`). None of the
//! day/month/time frames are meaningful on their own, so only the combination of them with a year
//! yields a recording date.

use symphonia_core::meta::{RawTag, RawValue, StandardTag, Tag};

use std::sync::Arc;

/// Collects the year, date, and time frames of a tag and combines them into a recording date.
#[derive(Default)]
pub struct LegacyDates {
    year: Option<String>,
    date: Option<String>,
    time: Option<String>,
    has_recording_date: bool,
}

impl LegacyDates {
    /// Observe a tag read from the ID3v2 tag.
    pub fn observe(&mut self, tag: &Tag) {
        // A recording date was provided directly (e.g., `TDRC`), nothing to combine.
        if matches!(tag.std, Some(StandardTag::RecordingDate(_))) {
            self.has_recording_date = true;
            return;
        }

        let slot = match tag.raw.key.as_str() {
            "TYER" | "TYE" => &mut self.year,
            "TDAT" | "TDA" => &mut self.date,
            "TIME" | "TIM" => &mut self.time,
            _ => return,
        };

        let text = match &tag.raw.value {
            RawValue::String(s) => Some(s.as_str()),
            RawValue::StringList(list) => list.first().map(String::as_str),
            _ => None,
        };

        // Only the first occurrence of each frame is used.
        if let Some(text) = text {
            if slot.is_none() {
                *slot = Some(text.trim().to_string());
            }
        }
    }

    /// Combine the observed frames into a recording date tag, if possible.
    pub fn finish(self) -> Option<Tag> {
        if self.has_recording_date {
            return None;
        }

        let year = self.year.as_deref().filter(|y| is_digits(y, 4))?;

        let mut date = year.to_string();
        let mut key = "TYER".to_string();

        if let Some((day, month)) = self.date.as_deref().and_then(parse_ddmm) {
            date = format!("{year}-{month:02}-{day:02}");
            key.push_str("+TDAT");

            // The time is only meaningful in combination with a full date.
            if let Some((hour, minute)) = self.time.as_deref().and_then(parse_hhmm) {
                date = format!("{date}T{hour:02}:{minute:02}");
                key.push_str("+TIME");
            }
        }

        Some(Tag::new_std(
            RawTag::new(key, date.clone()),
            StandardTag::RecordingDate(Arc::new(date)),
        ))
    }
}

fn is_digits(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_digit())
}

/// Parse a `DDMM` string into a valid (day, month) pair.
fn parse_ddmm(s: &str) -> Option<(u8, u8)> {
    if !is_digits(s, 4) {
        return None;
    }
    let day: u8 = s[..2].parse().ok()?;
    let month: u8 = s[2..].parse().ok()?;
    ((1..=31).contains(&day) && (1..=12).contains(&month)).then_some((day, month))
}

/// Parse a `HHMM` string into a valid (hour, minute) pair.
fn parse_hhmm(s: &str) -> Option<(u8, u8)> {
    if !is_digits(s, 4) {
        return None;
    }
    let hour: u8 = s[..2].parse().ok()?;
    let minute: u8 = s[2..].parse().ok()?;
    (hour < 24 && minute < 60).then_some((hour, minute))
}
