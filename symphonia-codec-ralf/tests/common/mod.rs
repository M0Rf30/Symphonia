// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(dead_code)]
#![allow(clippy::unwrap_used)]

//! Helpers shared by the integration tests: a minimal RealMedia (`.rm`) reader and writer, and a
//! driver that decodes a RALF stream.

use symphonia_codec_ralf::RalfDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::well_known::CODEC_ID_RALF;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

/// The MIME type of RALF streams in RealMedia.
const RALF_MIME: &[u8] = b"audio/x-ralf-mpeg4-generic";

/// A RALF stream: the `LSD:` header and the packets.
pub struct RalfStream {
    pub extra_data: Vec<u8>,
    /// The timestamp in milliseconds and the payload of each data packet.
    pub packets: Vec<(u32, Vec<u8>)>,
}

fn be16(d: &[u8], at: usize) -> usize {
    usize::from(u16::from_be_bytes([d[at], d[at + 1]]))
}

fn be32(d: &[u8], at: usize) -> usize {
    u32::from_be_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]]) as usize
}

/// Extracts the RALF stream of a RealMedia file: the `MDPR` header whose MIME type is RALF
/// supplies the extra data (all of its type-specific data), and the `DATA` packets of that stream
/// the packets.
pub fn read_rm(d: &[u8]) -> RalfStream {
    assert_eq!(&d[..4], b".RMF");

    let mut pos = be32(d, 4);
    let mut stream = None;
    let mut extra_data = Vec::new();
    let mut packets = Vec::new();

    while pos + 10 <= d.len() {
        let tag = &d[pos..pos + 4];
        let size = be32(d, pos + 4);

        match tag {
            b"MDPR" => {
                let mut p = pos + 10;
                let id = be16(d, p);
                // Skip the rates, packet sizes and times.
                p += 2 + 28;
                // The stream name, then the MIME type.
                p += 1 + usize::from(d[p]);
                let mime_len = usize::from(d[p]);
                let mime = &d[p + 1..p + 1 + mime_len];
                p += 1 + mime_len;
                let len = be32(d, p);
                p += 4;

                if mime == RALF_MIME && stream.is_none() {
                    stream = Some(id);
                    extra_data = d[p..p + len].to_vec();
                }
            }
            b"DATA" => {
                let end = (pos + size).min(d.len());
                // Skip the packet count and the offset of the next DATA chunk.
                let mut p = pos + 10 + 8;

                while p + 12 <= end {
                    let len = be16(d, p + 2);
                    assert!(len >= 12, "bad RealMedia packet");

                    if Some(be16(d, p + 4)) == stream {
                        packets.push((be32(d, p + 6) as u32, d[p + 12..p + len].to_vec()));
                    }
                    p += len;
                }
                break;
            }
            _ => (),
        }

        assert!(size >= 10, "bad RealMedia chunk");
        pos += size;
    }

    assert!(stream.is_some(), "no RALF stream");
    RalfStream { extra_data, packets }
}

/// Writes a RealMedia file with a single RALF stream, which FFmpeg can demux.
pub fn write_rm(stream: &RalfStream) -> Vec<u8> {
    fn chunk(tag: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut c = tag.to_vec();
        c.extend_from_slice(&(10 + body.len() as u32).to_be_bytes());
        c.extend_from_slice(&0u16.to_be_bytes());
        c.extend_from_slice(body);
        c
    }

    let max_packet = stream.packets.iter().map(|p| p.1.len()).max().unwrap_or(0) as u32;
    let num_packets = stream.packets.len() as u32;

    let mut out = b".RMF".to_vec();
    // Header size, version, file version and number of headers.
    out.extend_from_slice(&18u32.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&3u32.to_be_bytes());

    // PROP: rates, packet sizes, packet count, duration, preroll, index and data offsets (0: not
    // present, computed by the reader), streams and flags.
    let mut prop = Vec::new();
    for v in [0, 0, max_packet, max_packet, num_packets, 0, 0, 0, 0] {
        prop.extend_from_slice(&u32::to_be_bytes(v));
    }
    prop.extend_from_slice(&1u16.to_be_bytes());
    prop.extend_from_slice(&0u16.to_be_bytes());
    out.extend(chunk(b"PROP", &prop));

    // MDPR: stream number, rates, packet sizes, times, name, MIME type and type-specific data.
    let mut mdpr = 0u16.to_be_bytes().to_vec();
    for v in [0, 0, max_packet, max_packet, 0, 0, 0] {
        mdpr.extend_from_slice(&u32::to_be_bytes(v));
    }
    mdpr.push(0);
    mdpr.push(RALF_MIME.len() as u8);
    mdpr.extend_from_slice(RALF_MIME);
    mdpr.extend_from_slice(&(stream.extra_data.len() as u32).to_be_bytes());
    mdpr.extend_from_slice(&stream.extra_data);
    out.extend(chunk(b"MDPR", &mdpr));

    let mut data = Vec::new();
    data.extend_from_slice(&num_packets.to_be_bytes());
    data.extend_from_slice(&0u32.to_be_bytes());
    for (ts, payload) in &stream.packets {
        // Version, length, stream, timestamp, group and flags (2: key frame).
        data.extend_from_slice(&0u16.to_be_bytes());
        data.extend_from_slice(&(12 + payload.len() as u16).to_be_bytes());
        data.extend_from_slice(&0u16.to_be_bytes());
        data.extend_from_slice(&ts.to_be_bytes());
        data.extend_from_slice(&[0, 2]);
        data.extend_from_slice(payload);
    }
    out.extend(chunk(b"DATA", &data));

    out
}

pub fn decoder(extra_data: &[u8]) -> RalfDecoder {
    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_RALF).with_extra_data(extra_data.to_vec().into_boxed_slice());
    RalfDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap()
}

/// The result of decoding one packet.
pub struct Decoded {
    /// The interleaved 16-bit samples, empty if the packet produced none.
    pub samples: Vec<i16>,
    /// The number of audio frames.
    pub frames: usize,
}

/// Decodes one packet. A packet that fails to decode yields `None`.
pub fn decode_one(dec: &mut RalfDecoder, ts: u32, payload: &[u8]) -> Option<Decoded> {
    let pkt = PacketRef::new(0, Timestamp::new(i64::from(ts)), Duration::new(0), payload);

    match dec.decode_ref(&pkt) {
        Ok(GenericAudioBufferRef::S16(buf)) => {
            let mut samples = Vec::new();
            buf.copy_to_vec_interleaved(&mut samples);
            Some(Decoded { samples, frames: buf.frames() })
        }
        Ok(_) => panic!("the decoder must output 16-bit audio"),
        Err(_) => None,
    }
}

/// Decodes `packets` in order with a single decoder.
pub fn decode_packets(extra_data: &[u8], packets: &[(u32, Vec<u8>)]) -> Vec<Option<Decoded>> {
    let mut dec = decoder(extra_data);
    packets.iter().map(|(ts, payload)| decode_one(&mut dec, *ts, payload)).collect()
}

/// Byte-wise FNV-1a of the little-endian bytes of `samples`, as `ffmpeg -f s16le` writes them.
pub fn fnv_s16le(samples: &[i16]) -> u64 {
    let mut fnv = symphonia_testutil::Fnv::new();
    for s in samples {
        fnv.write(&s.to_le_bytes());
    }
    fnv.finish()
}
