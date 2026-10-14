// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::common::SideData;

use symphonia_core::errors::Result;
use symphonia_core::formats::Track;
use symphonia_core::io::BufReader;
use symphonia_core::meta::MetadataBuilder;
use symphonia_core::units::{Duration, Timestamp};
use symphonia_metadata::embedded::vorbis::{VORBIS_COMMENT_METADATA_INFO, read_vorbis_comment};

mod celt;
mod flac;
mod opus;
mod pcm;
mod speex;
mod vorbis;

/// Detect a `Mapper` for a logical stream given the identification packet of the stream.
pub fn detect(serial: u32, buf: &[u8]) -> Result<Option<Box<dyn Mapper>>> {
    let mapper = flac::detect(serial, buf)?
        .or(vorbis::detect(serial, buf)?)
        .or(opus::detect(serial, buf)?)
        .or(speex::detect(serial, buf)?)
        .or(celt::detect(serial, buf)?)
        .or(pcm::detect(serial, buf)?)
        .or_else(make_null_mapper);

    Ok(mapper)
}

/// Result of a packet map operation.
pub enum MapResult {
    /// The packet contained side data.
    SideData { data: SideData },
    /// The packet contained setup data.
    Setup,
    /// The packet contained stream data.
    StreamData { dur: Duration, discard: Duration },
    /// The packet contained unknown data.
    Unknown,
}

/// A `PacketParser` implements a packet parser that decodes the timestamp and duration for a
/// packet.
pub trait PacketParser: Send + Sync {
    fn parse_next_packet_dur(&mut self, packet: &[u8]) -> (Duration, Duration);
}

/// A `Mapper` implements packet-handling for a specific `Codec`.
pub trait Mapper: Send + Sync {
    /// Gets the name of the mapper.
    fn name(&self) -> &'static str;

    /// Soft-reset the mapper after a discontinuity in packets.
    fn reset(&mut self);

    /// Gets an immutable reference to the `Track` for the stream belonging to this `Mapper`. If
    /// the stream is not ready then the track may be incomplete.
    fn track(&self) -> &Track;

    /// Gets a mutable reference to the `Track` for the stream belonging to this `Mapper`.
    /// If the stream is not ready then the track may be incomplete.
    fn track_mut(&mut self) -> &mut Track;

    /// Soft-reset the mapper after seeking to the very first bitstream page of the logical
    /// stream. Codecs that signal a start-of-stream discard (e.g. Opus pre-skip) must re-arm it
    /// here since, unlike for any other seek, the stream really does start over.
    fn reset_at_start(&mut self) {
        self.reset();
    }

    /// Convert the absolute granular position of a page to a timestamp. `is_last_page` is true
    /// if the page ends the logical stream (some codecs define the granule position of the last
    /// page differently from all others).
    fn absgp_to_ts(&self, absgp: u64, _is_last_page: bool) -> Timestamp {
        Timestamp::from(absgp as i64)
    }

    /// Get the maximum duration between two random access points.
    fn max_rap_period(&self) -> Duration {
        Duration::new(0)
    }

    /// Make a packet parser for parsing packet timing.
    fn make_parser(&self) -> Option<Box<dyn PacketParser>>;

    /// Map a packet.
    fn map_packet(&mut self, packet: &[u8]) -> Result<MapResult>;

    /// Returns `true` if the stream can is ready for usage. If the stream is not ready then the
    /// mapper needs to consume more setup packets.
    fn is_ready(&self) -> bool {
        true
    }
}

/// Map the comment packet of a stream whose comment is a bare Vorbis comment: no signature, no
/// framing bit (Speex, CELT, OggPCM).
fn read_bare_comment(packet: &[u8]) -> Result<MapResult> {
    let mut builder = MetadataBuilder::new(VORBIS_COMMENT_METADATA_INFO);
    let mut side_data = Default::default();

    read_vorbis_comment(&mut BufReader::new(packet), &mut builder, &mut side_data)?;

    let rev = builder.build();

    Ok(MapResult::SideData { data: SideData::Metadata { rev, side_data } })
}

fn make_null_mapper() -> Option<Box<dyn Mapper>> {
    Some(Box::new(NullMapper::new()))
}

struct NullMapper {
    track: Track,
}

impl NullMapper {
    fn new() -> Self {
        NullMapper { track: Track::new(0) }
    }
}

impl Mapper for NullMapper {
    fn name(&self) -> &'static str {
        "null"
    }

    fn track(&self) -> &Track {
        &self.track
    }

    fn track_mut(&mut self) -> &mut Track {
        &mut self.track
    }

    fn reset(&mut self) {
        // Nothing to do!
    }

    fn make_parser(&self) -> Option<Box<dyn PacketParser>> {
        Some(Box::new(NullPacketParser {}))
    }

    fn map_packet(&mut self, _: &[u8]) -> Result<MapResult> {
        Ok(MapResult::Unknown)
    }
}

struct NullPacketParser {}

impl PacketParser for NullPacketParser {
    fn parse_next_packet_dur(&mut self, _: &[u8]) -> (Duration, Duration) {
        (Duration::ZERO, Duration::ZERO)
    }
}
