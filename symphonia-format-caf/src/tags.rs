// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Mapping of CAF `info` chunk key/value pairs to [`StandardTag`]s.
//!
//! Apple's CAF specification ("Information Chunk") documents a set of commonly-used, free-form
//! key strings, e.g. `"artist"`, `"album"`, `"title"`, `"track number"`. Keys are conventionally
//! lowercase, but some encoders capitalize them, so matching here is case-insensitive. Notably,
//! ffmpeg's CAF muxer writes lowercase keys taken directly from its own metadata dictionary:
//! `artist`, `album`, `title`, `track`, `genre`, `composer`, `comment`, `copyright`, `encoder`.

use std::sync::Arc;

use symphonia_core::meta::{StandardTag, Tag};

/// Convert a CAF `info` chunk key/value pair into one or more [`Tag`]s, pushed onto `out`. Most
/// keys produce exactly one [`Tag`] (with a [`StandardTag`] mapping if the key is recognized),
/// but `"N/M"`-formatted track/disc numbers additionally produce a total tag.
pub fn push_tags(key: &str, value: &str, out: &mut Vec<Tag>) {
    // Normalize the key: lowercase, and treat spaces/underscores/hyphens equivalently so both
    // Apple's documented "nominal bit rate"-style keys and common underscore/hyphen variants used
    // by other encoders match.
    let norm_key = key.to_ascii_lowercase().replace(['_', '-'], " ");

    let arc_value = Arc::new(value.to_string());

    let std = match norm_key.as_str() {
        "title" | "name" => Some(StandardTag::TrackTitle(arc_value)),
        "subtitle" => Some(StandardTag::TrackSubtitle(arc_value)),
        "artist" => Some(StandardTag::Artist(arc_value)),
        "album" => Some(StandardTag::Album(arc_value)),
        "album artist" => Some(StandardTag::AlbumArtist(arc_value)),
        "composer" => Some(StandardTag::Composer(arc_value)),
        "lyricist" => Some(StandardTag::Lyricist(arc_value)),
        "arranger" => Some(StandardTag::Arranger(arc_value)),
        "genre" => Some(StandardTag::Genre(arc_value)),
        "comments" | "comment" => Some(StandardTag::Comment(arc_value)),
        "copyright" => Some(StandardTag::Copyright(arc_value)),
        "publisher" => Some(StandardTag::Label(arc_value)),
        "key signature" => Some(StandardTag::InitialKey(arc_value)),
        "recorded date" | "date" | "year" => Some(StandardTag::RecordingDate(arc_value)),
        "source encoder" | "encoder" | "encoding application" | "tool" => {
            Some(StandardTag::Encoder(arc_value))
        }
        "isrc" => Some(StandardTag::IdentIsrc(arc_value)),
        "keywords" => Some(StandardTag::Keywords(arc_value)),
        "tempo" => parse_bpm(value),
        "track number" | "track" => {
            let (number, total) = parse_number_pair(value);
            if let Some(total) = total {
                out.push(Tag::new_from_parts("TRACK_TOTAL", total, Some(StandardTag::TrackTotal(total))));
            }
            number.map(StandardTag::TrackNumber)
        }
        "disc number" | "disc" => {
            let (number, total) = parse_number_pair(value);
            if let Some(total) = total {
                out.push(Tag::new_from_parts("DISC_TOTAL", total, Some(StandardTag::DiscTotal(total))));
            }
            number.map(StandardTag::DiscNumber)
        }
        // Known keys with no corresponding standard tag (kept as raw tags only).
        "time signature" | "nominal bit rate" | "channel layout" | "client" => None,
        _ => None,
    };

    out.push(Tag::new_from_parts(key, value, std));
}

/// Parses a `"N"` or `"N/M"` formatted track/disc number string, returning the leading number
/// and, if present, the total count following a `/`.
fn parse_number_pair(value: &str) -> (Option<u64>, Option<u64>) {
    let mut parts = value.splitn(2, '/');
    let number = parts.next().and_then(|s| s.trim().parse::<u64>().ok());
    let total = parts.next().and_then(|s| s.trim().parse::<u64>().ok());
    (number, total)
}

fn parse_bpm(value: &str) -> Option<StandardTag> {
    value.trim().parse::<f64>().ok().map(|bpm| StandardTag::Bpm(bpm.round() as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags_for(key: &str, value: &str) -> Vec<Tag> {
        let mut out = Vec::new();
        push_tags(key, value, &mut out);
        out
    }

    #[test]
    fn maps_ffmpeg_keys() {
        assert!(matches!(
            &tags_for("artist", "rmpd test")[0].std,
            Some(StandardTag::Artist(v)) if v.as_str() == "rmpd test"
        ));
        assert!(matches!(
            &tags_for("album", "Gapless")[0].std,
            Some(StandardTag::Album(v)) if v.as_str() == "Gapless"
        ));
        assert!(matches!(
            &tags_for("title", "ALAC caf")[0].std,
            Some(StandardTag::TrackTitle(v)) if v.as_str() == "ALAC caf"
        ));
        assert!(matches!(tags_for("track", "3")[0].std, Some(StandardTag::TrackNumber(3))));
        assert!(matches!(
            &tags_for("encoder", "Lavf63.1.102")[0].std,
            Some(StandardTag::Encoder(v)) if v.as_str() == "Lavf63.1.102"
        ));
    }

    #[test]
    fn is_case_insensitive() {
        assert!(matches!(tags_for("ARTIST", "x")[0].std, Some(StandardTag::Artist(_))));
        assert!(matches!(tags_for("Track Number", "5")[0].std, Some(StandardTag::TrackNumber(5))));
    }

    #[test]
    fn splits_track_total() {
        let tags = tags_for("track", "2/10");
        assert!(tags.iter().any(|t| matches!(t.std, Some(StandardTag::TrackNumber(2)))));
        assert!(tags.iter().any(|t| matches!(t.std, Some(StandardTag::TrackTotal(10)))));
    }

    #[test]
    fn unknown_key_has_no_std_tag() {
        let tags = tags_for("some_unknown_key", "value");
        assert_eq!(tags.len(), 1);
        assert!(tags[0].std.is_none());
    }
}
