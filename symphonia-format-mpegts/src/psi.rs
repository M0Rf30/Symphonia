// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Program specific information (PSI): the program association table (PAT) and the program map
//! table (PMT) (ISO/IEC 13818-1 §2.4.4).

use symphonia_core::checksum::Crc32;
use symphonia_core::io::Monitor;

/// The PID of the program association table.
pub const PAT_PID: u16 = 0;

const TABLE_ID_PAT: u8 = 0x00;
const TABLE_ID_PMT: u8 = 0x02;

/// The CRC-32/MPEG-2 of `data`.
fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc = Crc32::new(0xffff_ffff);
    crc.process_buf_bytes(data);
    crc.crc()
}

/// Reassembles a PSI section carried in the payloads of transport stream packets.
#[derive(Default)]
pub struct SectionAssembler {
    buf: Vec<u8>,
    active: bool,
}

impl SectionAssembler {
    /// Add the payload of a transport stream packet. Returns the first complete section, if the
    /// packet completed one. Any further sections are returned by
    /// [`take_section`](Self::take_section).
    pub fn push(&mut self, pusi: bool, payload: &[u8]) -> Option<Vec<u8>> {
        if pusi {
            let pointer = usize::from(*payload.first()?);
            let rest = payload.get(1 + pointer..)?;

            // The end of a section started in a previous packet is discarded: it is only
            // complete if it was complete in the previous packet.
            self.buf.clear();
            self.buf.extend_from_slice(rest);
            self.active = true;
        }
        else if self.active {
            self.buf.extend_from_slice(payload);
        }
        else {
            return None;
        }

        self.take_section()
    }

    /// Take the next complete section from the data buffered. Call after [`push`](Self::push)
    /// returns a section, until it returns `None`, since a packet may contain several sections.
    pub fn take_section(&mut self) -> Option<Vec<u8>> {
        if !self.active || self.buf.len() < 3 {
            return None;
        }

        // Stuffing after the last section of a packet.
        if self.buf[0] == 0xff {
            self.active = false;
            self.buf.clear();
            return None;
        }

        let len = 3 + (usize::from(self.buf[1] & 0xf) << 8 | usize::from(self.buf[2]));

        if self.buf.len() < len {
            return None;
        }

        // Any data after the section is the start of the next.
        let rest = self.buf.split_off(len);
        let section = std::mem::replace(&mut self.buf, rest);

        self.active = !self.buf.is_empty();

        Some(section)
    }
}

/// An entry of the program association table.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PatEntry {
    pub program_number: u16,
    pub pmt_pid: u16,
}

/// Parse a program association table section. Returns `None` if the section is not valid.
pub fn parse_pat(section: &[u8]) -> Option<Vec<PatEntry>> {
    if section.len() < 12 || section[0] != TABLE_ID_PAT || crc32_mpeg2(section) != 0 {
        return None;
    }

    // Section syntax indicator must be set, and the section must be current.
    if section[1] & 0x80 == 0 || section[5] & 1 == 0 {
        return None;
    }

    let body = &section[8..section.len() - 4];

    Some(
        body.chunks_exact(4)
            .filter_map(|e| {
                let program_number = u16::from_be_bytes([e[0], e[1]]);
                let pid = u16::from_be_bytes([e[2] & 0x1f, e[3]]);
                // Program number 0 identifies the network PID.
                (program_number != 0).then_some(PatEntry { program_number, pmt_pid: pid })
            })
            .collect(),
    )
}

/// The descriptors of an elementary stream that are of interest.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamDescriptors {
    /// The format identifier of the registration descriptor, if present.
    pub registration: Option<[u8; 4]>,
    /// The channel configuration code of the Opus extension descriptor.
    pub opus_channel_config: Option<u8>,
    /// The ISO 639 language code.
    pub language: Option<String>,
    /// True if an AC-3 descriptor was present.
    pub has_ac3: bool,
    /// True if an enhanced AC-3 descriptor was present.
    pub has_eac3: bool,
    /// True if a DTS descriptor was present.
    pub has_dts: bool,
}

/// An elementary stream of a program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PmtStream {
    pub stream_type: u8,
    pub pid: u16,
    pub descriptors: StreamDescriptors,
    /// The length of the descriptors of the stream, in bytes.
    pub descriptor_len: usize,
}

/// A program map table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pmt {
    pub program_number: u16,
    pub pcr_pid: u16,
    /// The format identifier of the registration descriptor of the program, if present. `HDMV` or
    /// `HDPR` identify a Blu-ray stream.
    pub registration: Option<[u8; 4]>,
    pub streams: Vec<PmtStream>,
}

fn parse_descriptors(mut d: &[u8]) -> StreamDescriptors {
    let mut out = StreamDescriptors::default();

    while d.len() >= 2 {
        let tag = d[0];
        let len = usize::from(d[1]);

        let Some(body) = d.get(2..2 + len)
        else {
            break;
        };

        match tag {
            // Registration descriptor.
            0x05 if len >= 4 => out.registration = Some([body[0], body[1], body[2], body[3]]),
            // ISO 639 language descriptor.
            0x0a if len >= 4 => {
                let lang: String = body[..3].iter().map(|&b| char::from(b)).collect();
                if lang.chars().all(|c| c.is_ascii_alphabetic()) {
                    out.language = Some(lang.to_ascii_lowercase());
                }
            }
            // AC-3, enhanced AC-3, and DTS descriptors (DVB).
            0x6a => out.has_ac3 = true,
            0x7a => out.has_eac3 = true,
            0x7b => out.has_dts = true,
            // Extension descriptor: user defined extension 0x80 is the Opus channel configuration.
            0x7f if len >= 2 && body[0] == 0x80 => out.opus_channel_config = Some(body[1]),
            _ => (),
        }

        d = &d[2 + len..];
    }

    out
}

/// Parse a program map table section. Returns `None` if the section is not valid.
pub fn parse_pmt(section: &[u8]) -> Option<Pmt> {
    if section.len() < 16 || section[0] != TABLE_ID_PMT || crc32_mpeg2(section) != 0 {
        return None;
    }

    if section[1] & 0x80 == 0 || section[5] & 1 == 0 {
        return None;
    }

    let program_number = u16::from_be_bytes([section[3], section[4]]);
    let pcr_pid = u16::from_be_bytes([section[8] & 0x1f, section[9]]);
    let program_info_len = usize::from(section[10] & 0xf) << 8 | usize::from(section[11]);

    let end = section.len() - 4;

    let program_descriptors = section.get(12..(12 + program_info_len).min(end))?;
    let registration = parse_descriptors(program_descriptors).registration;

    let mut i = 12 + program_info_len;
    let mut streams = vec![];

    while i + 5 <= end {
        let stream_type = section[i];
        let pid = u16::from_be_bytes([section[i + 1] & 0x1f, section[i + 2]]);
        let es_info_len = usize::from(section[i + 3] & 0xf) << 8 | usize::from(section[i + 4]);

        let descriptors = section.get(i + 5..(i + 5 + es_info_len).min(end))?;
        streams.push(PmtStream {
            stream_type,
            pid,
            descriptors: parse_descriptors(descriptors),
            descriptor_len: descriptors.len(),
        });

        i += 5 + es_info_len;
    }

    Some(Pmt { program_number, pcr_pid, registration, streams })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a PSI section (long form) with the CRC.
    fn build_section(table_id: u8, table_id_ext: u16, body: &[u8]) -> Vec<u8> {
        let len = 5 + body.len() + 4;
        let mut s = vec![table_id, 0xb0 | (len >> 8) as u8, len as u8];
        s.extend_from_slice(&table_id_ext.to_be_bytes());
        s.extend_from_slice(&[0xc1, 0, 0]);
        s.extend_from_slice(body);
        let crc = crc32_mpeg2(&s);
        s.extend_from_slice(&crc.to_be_bytes());
        s
    }

    #[test]
    fn verify_crc() {
        // The CRC of a section including its CRC is 0.
        let s = build_section(0, 1, &[0x00, 0x01, 0xf0, 0x00]);
        assert_eq!(crc32_mpeg2(&s), 0);
        // Known value: CRC-32/MPEG-2 of "123456789".
        assert_eq!(crc32_mpeg2(b"123456789"), 0x0376_e6e7);
    }

    #[test]
    fn verify_pat_and_pmt() {
        let pat = build_section(0, 1, &[0x00, 0x00, 0xe0, 0x10, 0x00, 0x01, 0xf0, 0x00]);
        assert_eq!(parse_pat(&pat), Some(vec![PatEntry { program_number: 1, pmt_pid: 0x1000 }]));

        let mut body = vec![0xe1, 0x00, 0xf0, 0x00];
        // AAC ADTS on PID 0x101 with a language, then Opus on PID 0x102.
        body.extend_from_slice(&[0x0f, 0xe1, 0x01, 0xf0, 0x06, 0x0a, 0x04, b'e', b'n', b'g', 0x00]);
        body.extend_from_slice(&[
            0x06, 0xe1, 0x02, 0xf0, 0x0d, 0x05, 0x04, b'O', b'p', b'u', b's', 0x7f, 0x02, 0x80,
            0x02, 0x59, 0x01, 0x00,
        ]);
        let pmt = parse_pmt(&build_section(2, 1, &body)).unwrap();
        assert_eq!(pmt.pcr_pid, 0x100);
        assert_eq!(pmt.streams.len(), 2);
        assert_eq!(pmt.streams[0].stream_type, 0x0f);
        assert_eq!(pmt.streams[0].descriptors.language.as_deref(), Some("eng"));
        assert_eq!(pmt.streams[1].descriptors.registration, Some(*b"Opus"));
        assert_eq!(pmt.streams[1].descriptors.opus_channel_config, Some(2));
    }

    #[test]
    fn verify_program_registration_and_ac3_dts_descriptors() {
        // The program has the HDMV registration descriptor.
        let mut body = vec![0xe1, 0x00, 0xf0, 0x06, 0x05, 0x04, b'H', b'D', b'M', b'V'];
        body.extend_from_slice(&[0x83, 0xe1, 0x00, 0xf0, 0x00]);
        // AC-3 (DVB), E-AC-3 (DVB), and DTS (DVB) descriptors, and a DTS1 registration.
        body.extend_from_slice(&[0x06, 0xe1, 0x01, 0xf0, 0x03, 0x6a, 0x01, 0x00]);
        body.extend_from_slice(&[0x06, 0xe1, 0x02, 0xf0, 0x03, 0x7a, 0x01, 0x00]);
        body.extend_from_slice(&[0x06, 0xe1, 0x03, 0xf0, 0x03, 0x7b, 0x01, 0x00]);
        body.extend_from_slice(&[0x06, 0xe1, 0x04, 0xf0, 0x06, 0x05, 0x04, b'D', b'T', b'S', b'1']);

        let pmt = parse_pmt(&build_section(2, 1, &body)).unwrap();
        assert_eq!(pmt.registration, Some(*b"HDMV"));
        assert_eq!(pmt.streams.len(), 5);
        assert_eq!(pmt.streams[0].descriptors, StreamDescriptors::default());
        assert!(pmt.streams[1].descriptors.has_ac3);
        assert!(pmt.streams[2].descriptors.has_eac3);
        assert!(pmt.streams[3].descriptors.has_dts);
        assert_eq!(pmt.streams[4].descriptors.registration, Some(*b"DTS1"));

        // A program without a registration descriptor.
        let pmt = parse_pmt(&build_section(2, 1, &[0xe1, 0x00, 0xf0, 0x00])).unwrap();
        assert_eq!(pmt.registration, None);
    }

    #[test]
    fn verify_section_assembler() {
        let sec = build_section(0, 1, &[0u8; 300]);
        let mut asm = SectionAssembler::default();

        let mut first = vec![0u8];
        first.extend_from_slice(&sec[..100]);
        assert!(asm.push(true, &first).is_none());
        assert!(asm.push(false, &sec[100..250]).is_none());
        assert_eq!(asm.push(false, &sec[250..]).as_deref(), Some(&sec[..]));
        assert!(asm.take_section().is_none());
    }

    #[test]
    fn verify_several_sections_in_a_packet() {
        let a = build_section(2, 1, &[1, 2, 3]);
        let b = build_section(2, 2, &[4, 5, 6, 7]);
        let mut payload = vec![0u8];
        payload.extend_from_slice(&a);
        payload.extend_from_slice(&b);
        payload.resize(184, 0xff);

        let mut asm = SectionAssembler::default();
        assert_eq!(asm.push(true, &payload).as_deref(), Some(&a[..]));
        assert_eq!(asm.take_section().as_deref(), Some(&b[..]));
        assert!(asm.take_section().is_none());

        // A section that begins in this packet and continues in the next.
        let mut payload = vec![0u8];
        payload.extend_from_slice(&a);
        payload.extend_from_slice(&b[..5]);
        let mut asm = SectionAssembler::default();
        assert_eq!(asm.push(true, &payload).as_deref(), Some(&a[..]));
        assert!(asm.take_section().is_none());
        assert_eq!(asm.push(false, &b[5..]).as_deref(), Some(&b[..]));
    }
}
