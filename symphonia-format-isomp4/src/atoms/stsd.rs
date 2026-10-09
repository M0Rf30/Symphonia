// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use core::str;

use log::debug;
use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::audio::well_known::CODEC_ID_MP3;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_PCM_F32BE, CODEC_ID_PCM_F32LE};
use symphonia_core::codecs::audio::well_known::{CODEC_ID_PCM_F64BE, CODEC_ID_PCM_F64LE};
use symphonia_core::codecs::audio::well_known::{CODEC_ID_PCM_S8, CODEC_ID_PCM_U8};
use symphonia_core::codecs::audio::well_known::{CODEC_ID_PCM_S16BE, CODEC_ID_PCM_S16LE};
use symphonia_core::codecs::audio::well_known::{CODEC_ID_PCM_S24BE, CODEC_ID_PCM_S24LE};
use symphonia_core::codecs::audio::well_known::{CODEC_ID_PCM_S32BE, CODEC_ID_PCM_S32LE};
use symphonia_core::codecs::audio::well_known::{CODEC_ID_PCM_U16BE, CODEC_ID_PCM_U16LE};
use symphonia_core::codecs::audio::well_known::{CODEC_ID_PCM_U24BE, CODEC_ID_PCM_U24LE};
use symphonia_core::codecs::audio::well_known::{CODEC_ID_PCM_U32BE, CODEC_ID_PCM_U32LE};
use symphonia_core::codecs::audio::{
    AudioCodecId, AudioCodecParameters, CODEC_ID_NULL_AUDIO, VerificationCheck,
};
use symphonia_core::codecs::subtitle::SubtitleCodecParameters;
use symphonia_core::codecs::subtitle::well_known::CODEC_ID_MOV_TEXT;
use symphonia_core::codecs::video::{VideoCodecId, VideoCodecParameters, VideoExtraData};
use symphonia_core::codecs::{CodecParameters, CodecProfile};

use crate::atoms::{
    AlacAtom, Atom, AtomHeader, AtomIterator, AtomType, AvcCAtom, Dac3Atom, Dec3Atom, DoviAtom,
    EsdsAtom, FlacAtom, HvcCAtom, OpusAtom, ReadAtom, Result, WaveAtom, decode_error,
    unsupported_error,
};
use crate::fp::FpU16;

/// Sample description atom.
#[allow(dead_code)]
#[derive(Debug)]
pub struct StsdAtom {
    /// Sample entry.
    sample_entry: SampleEntry,
}

impl Atom for StsdAtom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, _header: &AtomHeader) -> Result<Self> {
        let (_, _) = it.read_extended_header()?;

        let num_entries = it.read_u32()?;

        if num_entries == 0 {
            return decode_error("isomp4 (stsd): missing sample entry");
        }

        // A track may declare more than one sample entry — for example a chapter-image or
        // timed-text track in a podcast. Symphonia represents a track with a single codec, so use
        // the first sample entry and let the enclosing `read_atom` skip the remaining ones, rather
        // than failing to demux the whole file (including its audio track) over an auxiliary track.
        if num_entries > 1 {
            debug!("isomp4 (stsd): {num_entries} sample entries, using the first");
        }

        // Read the first sample entry atom.
        let header = match it.next_header()? {
            Some(header) => header,
            _ => return decode_error("isomp4 (stsd): missing expected sample entry"),
        };

        let sample_entry = match header.atom_type() {
            AtomType::AudioSampleEntryMp4a
            | AtomType::AudioSampleEntryAlac
            | AtomType::AudioSampleEntryAc3
            | AtomType::AudioSampleEntryEc3
            | AtomType::AudioSampleEntryFlac
            | AtomType::AudioSampleEntryOpus
            | AtomType::AudioSampleEntryMp3
            | AtomType::AudioSampleEntryLpcm
            | AtomType::AudioSampleEntryQtWave
            | AtomType::AudioSampleEntryALaw
            | AtomType::AudioSampleEntryMuLaw
            | AtomType::AudioSampleEntryU8
            | AtomType::AudioSampleEntryS16Le
            | AtomType::AudioSampleEntryS16Be
            | AtomType::AudioSampleEntryS24
            | AtomType::AudioSampleEntryS32
            | AtomType::AudioSampleEntryF32
            | AtomType::AudioSampleEntryF64 => {
                let entry = it.read_atom::<AudioSampleEntry>()?;
                SampleEntry::Audio(entry)
            }
            AtomType::VisualSampleEntryAv1
            | AtomType::VisualSampleEntryAvc1
            | AtomType::VisualSampleEntryDvh1
            | AtomType::VisualSampleEntryDvhe
            | AtomType::VisualSampleEntryHev1
            | AtomType::VisualSampleEntryHvc1
            | AtomType::VisualSampleEntryMp4v
            | AtomType::VisualSampleEntryVp8
            | AtomType::VisualSampleEntryVp9 => {
                let entry = it.read_atom::<VisualSampleEntry>()?;
                SampleEntry::Visual(entry)
            }
            AtomType::SubtitleSampleEntryText
            | AtomType::SubtitleSampleEntryTimedText
            | AtomType::SubtitleSampleEntryXml => {
                let entry = it.read_atom::<SubtitleSampleEntry>()?;
                SampleEntry::Subtitle(entry)
            }
            _ => {
                // Potentially subtitles, metadata, hints, etc.
                SampleEntry::Other
            }
        };

        Ok(StsdAtom { sample_entry })
    }
}

impl StsdAtom {
    /// Fill the provided `CodecParameters` using the sample entry.
    pub fn make_codec_params(&self) -> Option<CodecParameters> {
        // Audio sample entry.
        match &self.sample_entry {
            SampleEntry::Audio(entry) => Some(CodecParameters::Audio(entry.make_codec_params())),
            SampleEntry::Visual(entry) => Some(CodecParameters::Video(entry.make_codec_params())),
            SampleEntry::Subtitle(entry) => {
                Some(CodecParameters::Subtitle(entry.make_codec_params()))
            }
            _ => None,
        }
    }
}

/// Polymorphic sample entry atom.
#[derive(Debug)]
pub enum SampleEntry {
    Audio(AudioSampleEntry),
    Visual(VisualSampleEntry),
    Subtitle(SubtitleSampleEntry),
    // Metadata,
    Other,
}

/// Audio sample entry.
#[derive(Debug, Default)]
pub struct AudioSampleEntry {
    pub num_channels: u32,
    pub sample_size: u16,
    pub sample_rate: f64,
    pub codec_id: AudioCodecId,
    pub profile: Option<CodecProfile>,
    pub bits_per_sample: Option<u32>,
    pub bits_per_coded_sample: Option<u32>,
    pub frames_per_packet: Option<u64>,
    pub channels: Option<Channels>,
    pub verification_check: Option<VerificationCheck>,
    pub extra_data: Option<Box<[u8]>>,
}

impl AudioSampleEntry {
    pub(crate) fn make_codec_params(&self) -> AudioCodecParameters {
        AudioCodecParameters {
            codec: self.codec_id,
            profile: self.profile,
            sample_rate: Some(self.sample_rate as u32),
            bits_per_sample: self.bits_per_sample,
            bits_per_coded_sample: self.bits_per_coded_sample,
            channels: self.channels.clone(),
            max_frames_per_packet: self.frames_per_packet,
            verification_check: self.verification_check,
            extra_data: self.extra_data.clone(),
            ..Default::default()
        }
    }
}

impl Atom for AudioSampleEntry {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, header: &AtomHeader) -> Result<Self> {
        // An audio sample entry atom is derived from a base sample entry atom. The audio sample
        // entry atom contains the fields of the base sample entry first, then the audio sample
        // entry fields next. After those fields, a number of other atoms are nested, including the
        // mandatory codec-specific atom. Though the codec-specific atom is nested within the
        // (audio) sample entry atom, the (audio) sample entry atom uses the atom type of the
        // codec-specific atom. This is odd in-that the final structure will appear to have the
        // codec-specific atom nested within itself, which is not actually the case.

        // SampleEntry portion

        // Reserved. All 0.
        it.ignore_bytes(6)?;

        // Sample entry data reference.
        let _ = it.read_u16()?;

        // AudioSampleEntry(V1) portion

        let mut entry = AudioSampleEntry::default();

        // The version of the audio sample entry.
        let version = it.read_u16()?;

        // Skip revision and vendor.
        it.ignore_bytes(6)?;

        entry.num_channels = u32::from(it.read_u16()?);
        entry.sample_size = it.read_u16()?;

        // Skip compression ID and packet size.
        it.ignore_bytes(4)?;

        entry.sample_rate = f64::from(FpU16::parse_raw(it.read_u32()?));

        let is_pcm_codec = is_pcm_codec(header.atom_type);

        match version {
            0 => {
                // Version 0.
                if is_pcm_codec {
                    entry.codec_id = pcm_codec_id(header.atom_type);

                    // The lpcm atom type carries no fixed PCM codec for version 0 and 1
                    // sample entries. Its PCM format is only described by the version 2
                    // extension fields, so it is invalid here.
                    if entry.codec_id == CODEC_ID_NULL_AUDIO {
                        return decode_error("isomp4: lpcm audio sample entry must be version 2");
                    }

                    let bits_per_sample = 8 * bytes_per_pcm_sample(entry.codec_id);

                    // Validate the codec-derived bytes-per-sample equals the declared
                    // bytes-per-sample.
                    if u32::from(entry.sample_size) != bits_per_sample {
                        return decode_error("isomp4: invalid pcm sample size");
                    }
                    entry.bits_per_sample = Some(bits_per_sample);
                    entry.bits_per_coded_sample = Some(bits_per_sample);
                    entry.frames_per_packet = Some(1);
                    entry.channels = Some(pcm_channels(entry.num_channels)?);
                }
            }
            1 => {
                // Version 1.

                // The number of frames (ISO/MP4 samples) per packet. For PCM codecs, this is
                // always 1.
                let _frames_per_packet = it.read_u32()?;

                // The number of bytes per PCM audio sample. This value supersedes sample_size. For
                // non-PCM codecs, this value is not useful.
                let bytes_per_audio_sample = it.read_u32()?;

                // The number of bytes per PCM audio frame (ISO/MP4 sample). For non-PCM codecs,
                // this value is not useful.
                let _bytes_per_frame = it.read_u32()?;

                // The next value, as defined, is seemingly non-sensical.
                let _ = it.read_u32()?;

                if is_pcm_codec {
                    entry.codec_id = pcm_codec_id(header.atom_type);

                    // Same as version 0: the lpcm atom type has no fixed PCM codec below
                    // version 2.
                    if entry.codec_id == CODEC_ID_NULL_AUDIO {
                        return decode_error("isomp4: lpcm audio sample entry must be version 2");
                    }

                    let codec_bytes_per_sample = bytes_per_pcm_sample(entry.codec_id);

                    // Validate the codec-derived bytes-per-sample equals the declared
                    // bytes-per-sample.
                    if bytes_per_audio_sample != codec_bytes_per_sample {
                        return decode_error("isomp4: invalid pcm bytes per sample");
                    }

                    // The new fields describe the PCM sample format and supersede the original
                    // version 0 fields.
                    entry.bits_per_sample = Some(8 * codec_bytes_per_sample);
                    entry.bits_per_coded_sample = Some(8 * codec_bytes_per_sample);
                    entry.frames_per_packet = Some(1);
                    entry.channels = Some(pcm_channels(entry.num_channels)?);
                }
            }
            2 => {
                // Version 2.
                it.ignore_bytes(4)?;

                entry.sample_rate = it.read_f64()?;
                entry.num_channels = it.read_u32()?;

                if it.read_u32()? != 0x7f00_0000 {
                    return decode_error(
                        "isomp4: audio sample entry v2 reserved must be 0x7f00_0000",
                    );
                }

                // The following fields are only useful for PCM codecs.
                let bits_per_sample = it.read_u32()?;
                let lpcm_flags = it.read_u32()?;
                let _bytes_per_packet = it.read_u32()?;
                let lpcm_frames_per_packet = it.read_u32()?;

                // This is only valid if this is a PCM codec.
                entry.codec_id = lpcm_codec_id(bits_per_sample, lpcm_flags);

                if is_pcm_codec && entry.codec_id != CODEC_ID_NULL_AUDIO {
                    // Like version 1, the new fields describe the PCM sample format and supersede
                    // the original version 0 fields.
                    entry.bits_per_sample = Some(bits_per_sample);
                    entry.bits_per_coded_sample = Some(bits_per_sample);
                    entry.frames_per_packet = Some(u64::from(lpcm_frames_per_packet));
                    entry.channels = Some(lpcm_channels(entry.num_channels)?);
                }
            }
            _ => {
                return unsupported_error("isomp4: unknown sample entry version");
            }
        };

        while let Some(entry_header) = it.next_header()? {
            match entry_header.atom_type {
                AtomType::Esds => {
                    let atom = it.read_atom::<EsdsAtom>()?;
                    atom.fill_audio_sample_entry(&mut entry)?;
                }
                AtomType::Ac3Config => {
                    let atom = it.read_atom::<Dac3Atom>()?;
                    atom.fill_audio_sample_entry(&mut entry);
                }
                AtomType::AudioSampleEntryAlac => {
                    let atom = it.read_atom::<AlacAtom>()?;
                    atom.fill_audio_sample_entry(&mut entry);
                }
                AtomType::Eac3Config => {
                    let atom = it.read_atom::<Dec3Atom>()?;
                    atom.fill_audio_sample_entry(&mut entry);
                }
                AtomType::FlacDsConfig => {
                    let atom = it.read_atom::<FlacAtom>()?;
                    atom.fill_audio_sample_entry(&mut entry);
                }
                AtomType::OpusDsConfig => {
                    let atom = it.read_atom::<OpusAtom>()?;
                    atom.fill_audio_sample_entry(&mut entry);
                }
                AtomType::AudioSampleEntryQtWave => {
                    // The QuickTime WAVE (aka. siDecompressionParam) atom may contain many
                    // different types of sub-atoms to store decoder parameters.
                    let atom = it.read_atom::<WaveAtom>()?;
                    atom.fill_audio_sample_entry(&mut entry)?;
                }
                _ => {
                    debug!("unknown audio sample entry sub-atom: {:?}.", entry_header.atom_type());
                }
            }
        }

        // A MP3 sample entry has no codec-specific atom.
        if header.atom_type == AtomType::AudioSampleEntryMp3 {
            entry.codec_id = CODEC_ID_MP3;
        }

        Ok(entry)
    }
}

/// Gets if the sample entry atom is for a PCM codec.
fn is_pcm_codec(atype: AtomType) -> bool {
    // PCM data in version 0 and 1 is signalled by the sample entry atom type. In version 2, the
    // atom type for PCM data is always LPCM.
    atype == AtomType::AudioSampleEntryLpcm || pcm_codec_id(atype) != CODEC_ID_NULL_AUDIO
}

/// Gets the PCM codec from the sample entry atom type for version 0 and 1 sample entries.
fn pcm_codec_id(atype: AtomType) -> AudioCodecId {
    match atype {
        AtomType::AudioSampleEntryU8 => CODEC_ID_PCM_U8,
        AtomType::AudioSampleEntryS16Le => CODEC_ID_PCM_S16LE,
        AtomType::AudioSampleEntryS16Be => CODEC_ID_PCM_S16BE,
        AtomType::AudioSampleEntryS24 => CODEC_ID_PCM_S24LE,
        AtomType::AudioSampleEntryS32 => CODEC_ID_PCM_S32LE,
        AtomType::AudioSampleEntryF32 => CODEC_ID_PCM_F32LE,
        AtomType::AudioSampleEntryF64 => CODEC_ID_PCM_F64LE,
        _ => CODEC_ID_NULL_AUDIO,
    }
}

/// Determines the number of bytes per PCM sample for a PCM codec ID.
fn bytes_per_pcm_sample(pcm_codec_id: AudioCodecId) -> u32 {
    match pcm_codec_id {
        CODEC_ID_PCM_S8 | CODEC_ID_PCM_U8 => 1,
        CODEC_ID_PCM_S16BE | CODEC_ID_PCM_S16LE => 2,
        CODEC_ID_PCM_U16BE | CODEC_ID_PCM_U16LE => 2,
        CODEC_ID_PCM_S24BE | CODEC_ID_PCM_S24LE => 3,
        CODEC_ID_PCM_U24BE | CODEC_ID_PCM_U24LE => 3,
        CODEC_ID_PCM_S32BE | CODEC_ID_PCM_S32LE => 4,
        CODEC_ID_PCM_U32BE | CODEC_ID_PCM_U32LE => 4,
        CODEC_ID_PCM_F32BE | CODEC_ID_PCM_F32LE => 4,
        CODEC_ID_PCM_F64BE | CODEC_ID_PCM_F64LE => 8,
        _ => unreachable!(),
    }
}

/// Gets the PCM codec from the LPCM parameters in the version 2 sample entry atom.
fn lpcm_codec_id(bits_per_sample: u32, lpcm_flags: u32) -> AudioCodecId {
    let is_floating_point = lpcm_flags & 0x1 != 0;
    let is_big_endian = lpcm_flags & 0x2 != 0;
    let is_signed = lpcm_flags & 0x4 != 0;

    if is_floating_point {
        // Floating-point sample format.
        match bits_per_sample {
            32 if is_big_endian => CODEC_ID_PCM_F32BE,
            64 if is_big_endian => CODEC_ID_PCM_F64BE,
            32 => CODEC_ID_PCM_F32LE,
            64 => CODEC_ID_PCM_F64LE,
            _ => CODEC_ID_NULL_AUDIO,
        }
    }
    else {
        // Integer sample format.
        if is_signed {
            // Signed-integer sample format.
            match bits_per_sample {
                8 => CODEC_ID_PCM_S8,
                16 if is_big_endian => CODEC_ID_PCM_S16BE,
                24 if is_big_endian => CODEC_ID_PCM_S24BE,
                32 if is_big_endian => CODEC_ID_PCM_S32BE,
                16 => CODEC_ID_PCM_S16LE,
                24 => CODEC_ID_PCM_S24LE,
                32 => CODEC_ID_PCM_S32LE,
                _ => CODEC_ID_NULL_AUDIO,
            }
        }
        else {
            // Unsigned-integer sample format.
            match bits_per_sample {
                8 => CODEC_ID_PCM_U8,
                16 if is_big_endian => CODEC_ID_PCM_U16BE,
                24 if is_big_endian => CODEC_ID_PCM_U24BE,
                32 if is_big_endian => CODEC_ID_PCM_U32BE,
                16 => CODEC_ID_PCM_U16LE,
                24 => CODEC_ID_PCM_U24LE,
                32 => CODEC_ID_PCM_U32LE,
                _ => CODEC_ID_NULL_AUDIO,
            }
        }
    }
}

/// Gets the audio channels for a version 0 or 1 sample entry.
fn pcm_channels(num_channels: u32) -> Result<Channels> {
    match num_channels {
        1 => Ok(Channels::Positioned(Position::FRONT_LEFT)),
        2 => Ok(Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT)),
        _ => decode_error("isomp4: invalid number of channels"),
    }
}

/// Gets the audio channels for a version 2 LPCM sample entry.
fn lpcm_channels(num_channels: u32) -> Result<Channels> {
    if num_channels < 1 {
        return decode_error("isomp4: invalid number of channels");
    }

    if num_channels > 32 {
        return unsupported_error("isomp4: maximum 32 channels");
    }

    // TODO: For LPCM, the channels are "auxilary". They do not have a speaker assignment. Symphonia
    // does not have a way to represent this yet.
    let channel_mask = !((!0 << 1) << (num_channels - 1));

    match Position::from_bits(channel_mask) {
        Some(positions) => Ok(Channels::Positioned(positions)),
        _ => unsupported_error("isomp4: unsupported number of channels"),
    }
}

/// Visual sample entry.
#[allow(dead_code)]
#[derive(Debug, Default)]
pub struct VisualSampleEntry {
    pub width: u16,
    pub height: u16,
    pub horiz_res: f64,
    pub vert_res: f64,
    /// Frame count per sample.
    pub frame_count: u16,
    pub compressor: Option<String>,
    pub codec_id: VideoCodecId,
    pub profile: Option<CodecProfile>,
    pub level: Option<u32>,
    pub extra_data: Vec<VideoExtraData>,
}

impl VisualSampleEntry {
    pub(crate) fn make_codec_params(&self) -> VideoCodecParameters {
        let mut codec_params = VideoCodecParameters {
            width: Some(self.width),
            height: Some(self.height),
            codec: self.codec_id,
            extra_data: self.extra_data.clone(),
            ..Default::default()
        };

        if let Some(profile) = self.profile {
            codec_params.with_profile(profile);
        }
        if let Some(level) = self.level {
            codec_params.with_level(level);
        }

        codec_params
    }
}

impl Atom for VisualSampleEntry {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, _header: &AtomHeader) -> Result<Self> {
        // SampleEntry portion

        // Reserved. All 0.
        it.ignore_bytes(6)?;

        // Sample entry data reference.
        let _ = it.read_u16()?;

        // VisualSampleEntry portion

        // Reserved.
        it.ignore_bytes(16)?;

        let mut entry = VisualSampleEntry {
            width: it.read_u16()?,
            height: it.read_u16()?,
            horiz_res: f64::from(FpU16::parse_raw(it.read_u32()?)),
            vert_res: f64::from(FpU16::parse_raw(it.read_u32()?)),
            ..Default::default()
        };

        // Reserved.
        let _ = it.read_u32()?;

        entry.frame_count = it.read_u16()?;

        entry.compressor = {
            let len = usize::from(it.read_u8()?);

            let mut name = [0u8; 31];
            it.read_buf_exact(&mut name)?;

            if len > 31 {
                return decode_error("isomp4 (stsd): compressor name length exceeds 31 bytes");
            }

            match str::from_utf8(&name[..len]) {
                Ok(name) => Some(name.to_string()),
                _ => None,
            }
        };

        let _depth = it.read_u16()?;

        // Reserved.
        it.read_u16()?;

        while let Some(entry_header) = it.next_header()? {
            match entry_header.atom_type {
                AtomType::Esds => {
                    let atom = it.read_atom::<EsdsAtom>()?;
                    atom.fill_video_sample_entry(&mut entry)?;
                }
                AtomType::AvcConfiguration => {
                    let atom = it.read_atom::<AvcCAtom>()?;
                    atom.fill_video_sample_entry(&mut entry);
                }
                AtomType::HevcConfiguration => {
                    let atom = it.read_atom::<HvcCAtom>()?;
                    atom.fill_video_sample_entry(&mut entry);
                }
                AtomType::DolbyVisionConfiguration => {
                    let atom = it.read_atom::<DoviAtom>()?;
                    atom.fill_video_sample_entry(&mut entry);
                }
                _ => {
                    debug!("unknown visual sample entry sub-atom: {:?}.", entry_header.atom_type());
                }
            }
        }

        Ok(entry)
    }
}

#[derive(Debug)]
pub enum SubtitleCodecSpecific {
    /// MOV_TEXT
    TimedText,
}

/// Subtitle sample entry type.
#[allow(dead_code)]
#[derive(Debug)]
pub struct SubtitleSampleEntry {
    btrt: Option<BtrtAtom>,
    txtc: Option<TxtcAtom>,
    codec_specific: Option<SubtitleCodecSpecific>,
}

impl SubtitleSampleEntry {
    pub(crate) fn make_codec_params(&self) -> SubtitleCodecParameters {
        let mut codec_params = SubtitleCodecParameters::new();

        if let Some(SubtitleCodecSpecific::TimedText) = self.codec_specific {
            codec_params.for_codec(CODEC_ID_MOV_TEXT);
        }

        codec_params
    }
}

impl Atom for SubtitleSampleEntry {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, header: &AtomHeader) -> Result<Self> {
        // SampleEntry portion

        // Reserved. All 0.
        it.ignore_bytes(6)?;

        // Sample entry data reference.
        let _ = it.read_u16()?;

        let mut codec_specific = None;
        // SubtitleSampleEntry portion

        match header.atom_type {
            AtomType::SubtitleSampleEntryText => {
                let _encoding = it.read_null_terminated_utf8()?;
                let _mime_type = it.read_null_terminated_utf8()?;
            }
            AtomType::SubtitleSampleEntryTimedText => {
                // Standard - 3GPP TS 26.245 - TextSampleEntry
                // display flags - 4 bytes
                // horizontal justification - 1 bytes
                // vertical justification - 1 bytes
                // background color rgba - 4 bytes
                // box record - 8 bytes
                // style record - 12 bytes
                it.ignore_bytes(30)?;

                codec_specific = Some(SubtitleCodecSpecific::TimedText);
            }
            AtomType::SubtitleSampleEntryXml => {
                let _namespace = it.read_null_terminated_utf8()?;
                let _schema_location = it.read_null_terminated_utf8()?;
                let _auxiliary_mime_types = it.read_null_terminated_utf8()?;
            }
            _ => {}
        }

        let mut btrt = None;
        let mut txtc = None;

        while let Some(entry_header) = it.next_header()? {
            match entry_header.atom_type {
                AtomType::BitRate => {
                    btrt = Some(it.read_atom::<BtrtAtom>()?);
                }
                AtomType::TextConfig => {
                    txtc = Some(it.read_atom::<TxtcAtom>()?);
                }
                _ => {
                    debug!(
                        "unknown subtitle sample entry sub-atom: {:?}.",
                        entry_header.atom_type()
                    );
                }
            }
        }

        Ok(SubtitleSampleEntry { btrt, txtc, codec_specific })
    }
}

/// Bitrate atom.
#[allow(dead_code)]
#[derive(Debug)]
pub struct BtrtAtom {
    /// Size of the decoding buffer for an elementary stream in bytes.
    pub buf_size_db: u32,
    /// Maximum bitrate in bits/second over a window of 1 second.
    pub max_bitrate: u32,
    /// Average bitrate in bits/second.
    pub avg_bitrate: u32,
}

impl Atom for BtrtAtom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, _header: &AtomHeader) -> Result<Self> {
        Ok(BtrtAtom {
            buf_size_db: it.read_u32()?,
            max_bitrate: it.read_u32()?,
            avg_bitrate: it.read_u32()?,
        })
    }
}

/// Text config atom.
#[allow(dead_code)]
#[derive(Debug)]
pub struct TxtcAtom {
    /// Initial text to be prepended before the contents of each sync sample.
    pub text_config: String,
}

impl Atom for TxtcAtom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, _header: &AtomHeader) -> Result<Self> {
        let (_, _) = it.read_extended_header()?;
        let text_config = it.read_null_terminated_utf8()?;
        Ok(TxtcAtom { text_config })
    }
}

/// Clean aperture atom.
#[allow(dead_code)]
#[derive(Debug)]
pub struct ClapAtom {
    pub h_spacing: u32,
    pub v_spacing: u32,
}

impl Atom for ClapAtom {
    fn read<R: ReadAtom>(reader: &mut AtomIterator<R>, _header: &AtomHeader) -> Result<Self> {
        Ok(ClapAtom { h_spacing: reader.read_u32()?, v_spacing: reader.read_u32()? })
    }
}

/// Pixel aspect ratio atom.
#[allow(dead_code)]
#[derive(Debug)]
pub struct PaspAtom {
    clean_aperture_width_n: u32,
    clean_aperture_width_d: u32,
    clean_aperture_height_n: u32,
    clean_aperture_height_d: u32,
    horiz_off_n: u32,
    horiz_off_d: u32,
    vert_off_n: u32,
    vert_off_d: u32,
}

impl Atom for PaspAtom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, _header: &AtomHeader) -> Result<Self> {
        Ok(PaspAtom {
            clean_aperture_width_n: it.read_u32()?,
            clean_aperture_width_d: it.read_u32()?,
            clean_aperture_height_n: it.read_u32()?,
            clean_aperture_height_d: it.read_u32()?,
            horiz_off_n: it.read_u32()?,
            horiz_off_d: it.read_u32()?,
            vert_off_n: it.read_u32()?,
            vert_off_d: it.read_u32()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use symphonia_core::codecs::CodecParameters;
    use symphonia_core::errors::Error;
    use symphonia_core::formats::FormatOptions;
    use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions};

    use super::StsdAtom;
    use crate::IsoMp4Reader;
    use crate::atoms::AtomIterator;

    /// Build a minimal `stsd` atom declaring `entry_count` sample entries: a first `mp4a` audio
    /// entry (44.1 kHz, stereo) followed by a throwaway `jpeg` entry that must be skipped.
    fn stsd_with_two_entries() -> Vec<u8> {
        // First entry: an mp4a audio sample entry (version 0, no codec-specific sub-atoms).
        let mut mp4a_body = Vec::new();
        mp4a_body.extend_from_slice(&[0u8; 6]); // reserved
        mp4a_body.extend_from_slice(&[0, 0]); // data reference index
        mp4a_body.extend_from_slice(&[0, 0]); // version 0
        mp4a_body.extend_from_slice(&[0u8; 6]); // revision + vendor
        mp4a_body.extend_from_slice(&2u16.to_be_bytes()); // channels
        mp4a_body.extend_from_slice(&16u16.to_be_bytes()); // sample size
        mp4a_body.extend_from_slice(&[0u8; 4]); // compression id + packet size
        mp4a_body.extend_from_slice(&(44_100u32 << 16).to_be_bytes()); // sample rate (16.16)
        let mut mp4a = Vec::new();
        mp4a.extend_from_slice(&((8 + mp4a_body.len()) as u32).to_be_bytes());
        mp4a.extend_from_slice(b"mp4a");
        mp4a.extend_from_slice(&mp4a_body);

        // Second entry: a throwaway image entry (as a podcast chapter-image track carries).
        let mut jpeg = Vec::new();
        jpeg.extend_from_slice(&12u32.to_be_bytes());
        jpeg.extend_from_slice(b"jpeg");
        jpeg.extend_from_slice(&[0u8; 4]);

        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // version + flags
        body.extend_from_slice(&2u32.to_be_bytes()); // entry count
        body.extend_from_slice(&mp4a);
        body.extend_from_slice(&jpeg);

        let mut atom = Vec::new();
        atom.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
        atom.extend_from_slice(b"stsd");
        atom.extend_from_slice(&body);
        atom
    }

    #[test]
    fn reads_first_entry_when_multiple_present() {
        let bytes = stsd_with_two_entries();
        let len = bytes.len() as u64;
        let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
        let mut it = AtomIterator::new(mss, Some(len));

        assert!(it.next_header().ok().flatten().is_some(), "stsd header should be read");
        // A second sample entry must not fail the whole atom (previously an unsupported error).
        let stsd = match it.read_atom::<StsdAtom>() {
            Ok(stsd) => stsd,
            Err(_) => panic!("stsd with two entries should parse"),
        };

        // The first (audio) entry is the one used.
        match stsd.make_codec_params() {
            Some(CodecParameters::Audio(params)) => {
                assert_eq!(params.sample_rate, Some(44_100));
            }
            _ => panic!("expected the first entry's audio codec parameters"),
        }
    }

    /// Wrap `body` in an ISO-BMFF atom with the given four-cc.
    fn atom(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8 + body.len());
        buf.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
        buf.extend_from_slice(fourcc);
        buf.extend_from_slice(body);
        buf
    }

    /// Build a minimal MP4 containing a single audio track whose sample description
    /// entry uses the given atom type and sample entry version.
    fn mp4_with_audio_sample_entry(entry_type: &[u8; 4], version: u16) -> Vec<u8> {
        // Audio sample entry body.
        let mut entry = Vec::new();
        entry.extend_from_slice(&[0; 6]); // Reserved.
        entry.extend_from_slice(&1u16.to_be_bytes()); // Data reference index.
        entry.extend_from_slice(&version.to_be_bytes());
        entry.extend_from_slice(&[0; 6]); // Revision level + vendor.
        entry.extend_from_slice(&2u16.to_be_bytes()); // Channel count.
        entry.extend_from_slice(&16u16.to_be_bytes()); // Sample size.
        entry.extend_from_slice(&[0; 4]); // Compression id + packet size.
        entry.extend_from_slice(&(44100u32 << 16).to_be_bytes()); // Sample rate (16.16).

        match version {
            1 => {
                entry.extend_from_slice(&1u32.to_be_bytes()); // Frames per packet.
                entry.extend_from_slice(&2u32.to_be_bytes()); // Bytes per PCM sample.
                entry.extend_from_slice(&[0; 8]); // Bytes per frame + unused.
            }
            2 => {
                entry.extend_from_slice(&[0; 4]); // Reserved.
                entry.extend_from_slice(&44100f64.to_be_bytes()); // Sample rate.
                entry.extend_from_slice(&2u32.to_be_bytes()); // Channel count.
                entry.extend_from_slice(&0x7f00_0000u32.to_be_bytes()); // Constant.
                entry.extend_from_slice(&16u32.to_be_bytes()); // Bits per sample.
                entry.extend_from_slice(&4u32.to_be_bytes()); // LPCM flags (signed int).
                entry.extend_from_slice(&0u32.to_be_bytes()); // Bytes per packet.
                entry.extend_from_slice(&1u32.to_be_bytes()); // LPCM frames per packet.
            }
            _ => (),
        }

        // stsd atom with the sample entry as its single entry.
        let mut stsd_body = vec![0; 4]; // Version + flags.
        stsd_body.extend_from_slice(&1u32.to_be_bytes()); // Entry count.
        stsd_body.extend_from_slice(&atom(entry_type, &entry));

        // Sample table atom with the remaining (empty) mandatory tables.
        let stbl = atom(
            b"stbl",
            &[
                atom(b"stsd", &stsd_body),
                atom(b"stts", &[0; 8]),
                atom(b"stsc", &[0; 8]),
                atom(b"stsz", &[0; 12]),
            ]
            .concat(),
        );

        let minf = atom(b"minf", &[atom(b"smhd", &[0; 8]), stbl].concat());

        // mdhd atom (version 0).
        let mut mdhd_body = vec![0; 4]; // Version + flags.
        mdhd_body.extend_from_slice(&[0; 8]); // ctime + mtime.
        mdhd_body.extend_from_slice(&44100u32.to_be_bytes()); // Timescale.
        mdhd_body.extend_from_slice(&[0; 8]); // Duration + language + quality.
        let mdhd = atom(b"mdhd", &mdhd_body);

        // hdlr atom ("soun" handler).
        let mut hdlr_body = vec![0; 8]; // Version + flags + component type.
        hdlr_body.extend_from_slice(b"soun");
        hdlr_body.extend_from_slice(&[0; 12]); // Component flags + flags mask.
        let hdlr = atom(b"hdlr", &hdlr_body);

        let mdia = atom(b"mdia", &[mdhd, hdlr, minf].concat());

        // tkhd atom (version 0).
        let mut tkhd_body = vec![0; 4]; // Version + flags.
        tkhd_body.extend_from_slice(&[0; 8]); // ctime + mtime.
        tkhd_body.extend_from_slice(&1u32.to_be_bytes()); // Track id.
        tkhd_body.extend_from_slice(&[0; 8]); // Reserved + duration.
        tkhd_body.extend_from_slice(&[0; 14]); // Reserved + layer + alt group + volume.
        let tkhd = atom(b"tkhd", &tkhd_body);

        let trak = atom(b"trak", &[tkhd, mdia].concat());

        // mvhd atom (version 0).
        let mut mvhd_body = vec![0; 4]; // Version + flags.
        mvhd_body.extend_from_slice(&[0; 8]); // ctime + mtime.
        mvhd_body.extend_from_slice(&1000u32.to_be_bytes()); // Timescale.
        mvhd_body.extend_from_slice(&[0; 4]); // Duration.
        mvhd_body.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // Preferred rate 1.0.
        mvhd_body.extend_from_slice(&0x0100u16.to_be_bytes()); // Preferred volume 1.0.
        let mvhd = atom(b"mvhd", &mvhd_body);

        let moov = atom(b"moov", &[mvhd, trak].concat());

        // ftyp atom.
        let mut ftyp_body = Vec::new();
        ftyp_body.extend_from_slice(b"M4A ");
        ftyp_body.extend_from_slice(&[0; 4]); // Minor version.
        ftyp_body.extend_from_slice(b"M4A mp42"); // Compatible brands.

        [atom(b"ftyp", &ftyp_body), moov].concat()
    }

    /// Probe a complete MP4 through the format reader.
    fn probe(data: Vec<u8>) -> symphonia_core::errors::Result<IsoMp4Reader<'static>> {
        let mss = MediaSourceStream::new(
            Box::new(Cursor::new(data)),
            MediaSourceStreamOptions::default(),
        );
        IsoMp4Reader::try_new(mss, FormatOptions::default())
    }

    #[test]
    fn audio_sample_entry_lpcm_v0_is_rejected() {
        // An lpcm atom type on a version 0 sample entry has no derivable PCM codec.
        // Previously this reached an unreachable!() in bytes_per_pcm_sample and
        // aborted the process (issue #560).
        let result = probe(mp4_with_audio_sample_entry(b"lpcm", 0));
        assert!(matches!(result, Err(Error::DecodeError(_))));
    }

    #[test]
    fn audio_sample_entry_lpcm_v1_is_rejected() {
        let result = probe(mp4_with_audio_sample_entry(b"lpcm", 1));
        assert!(matches!(result, Err(Error::DecodeError(_))));
    }

    #[test]
    fn audio_sample_entry_lpcm_v2_is_accepted() {
        // Version 2 is the valid form of an lpcm sample entry.
        let result = probe(mp4_with_audio_sample_entry(b"lpcm", 2));
        assert!(result.is_ok(), "probe failed: {:?}", result.err());
    }

    #[test]
    fn audio_sample_entry_pcm_v0_is_accepted() {
        // A PCM atom type with a fixed codec mapping remains valid at version 0.
        let result = probe(mp4_with_audio_sample_entry(b"sowt", 0));
        assert!(result.is_ok(), "probe failed: {:?}", result.err());
    }

    /// Build an `stsd` atom with one audio sample entry of type `entry_type` and `version`,
    /// declaring `rate` (16.16 fixed-point, as truncated by writers) and `channels`, followed by
    /// the given extra bytes (version 1 fields and/or sub-atoms).
    fn stsd_audio_entry(
        entry_type: &[u8; 4],
        version: u16,
        channels: u16,
        rate: u32,
        extra: &[u8],
    ) -> Vec<u8> {
        let mut entry = Vec::new();
        entry.extend_from_slice(&[0; 6]); // Reserved.
        entry.extend_from_slice(&1u16.to_be_bytes()); // Data reference index.
        entry.extend_from_slice(&version.to_be_bytes());
        entry.extend_from_slice(&[0; 6]); // Revision level + vendor.
        entry.extend_from_slice(&channels.to_be_bytes());
        entry.extend_from_slice(&16u16.to_be_bytes()); // Sample size.
        entry.extend_from_slice(&[0; 4]); // Compression id + packet size.
        entry.extend_from_slice(&rate.to_be_bytes()); // Sample rate (16.16).
        entry.extend_from_slice(extra);

        let mut stsd_body = vec![0; 4]; // Version + flags.
        stsd_body.extend_from_slice(&1u32.to_be_bytes()); // Entry count.
        stsd_body.extend_from_slice(&atom(entry_type, &entry));
        atom(b"stsd", &stsd_body)
    }

    /// Parse an `stsd` atom and return the audio codec parameters of its first entry.
    fn audio_params(stsd: Vec<u8>) -> symphonia_core::codecs::audio::AudioCodecParameters {
        let len = stsd.len() as u64;
        let mss = MediaSourceStream::new(Box::new(Cursor::new(stsd)), Default::default());
        let mut it = AtomIterator::new(mss, Some(len));

        assert!(it.next_header().ok().flatten().is_some(), "stsd header should be read");

        let stsd = match it.read_atom::<StsdAtom>() {
            Ok(stsd) => stsd,
            Err(_) => panic!("stsd should parse"),
        };

        match stsd.make_codec_params() {
            Some(CodecParameters::Audio(params)) => params,
            _ => panic!("expected audio codec parameters"),
        }
    }

    /// Build an `esds` atom for AAC (object type 0x40) with the given audio specific config.
    fn esds_aac(asc: &[u8]) -> Vec<u8> {
        // Object type, stream type, buffer size (3), max bitrate (4), average bitrate (4).
        let mut dec_config = vec![0x40, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        dec_config.extend_from_slice(&[0x05, asc.len() as u8]);
        dec_config.extend_from_slice(asc);

        let mut es = vec![0, 1, 0]; // ES id + flags.
        es.extend_from_slice(&[0x04, dec_config.len() as u8]);
        es.extend_from_slice(&dec_config);
        es.extend_from_slice(&[0x06, 0x01, 0x02]); // SL config (MP4).

        let mut body = vec![0; 4]; // Version + flags.
        body.extend_from_slice(&[0x03, es.len() as u8]);
        body.extend_from_slice(&es);
        atom(b"esds", &body)
    }

    #[test]
    fn aac_sample_rate_above_65535_comes_from_audio_specific_config() {
        // AAC-LC, 96 kHz, stereo. The 16.16 sample rate in the sample entry is truncated.
        let truncated = (96_000u32 & 0xffff) << 16;
        let stsd = stsd_audio_entry(b"mp4a", 0, 2, truncated, &esds_aac(&[0x10, 0x10]));

        let params = audio_params(stsd);
        assert_eq!(params.sample_rate, Some(96_000));
    }

    #[test]
    fn he_aac_params_describe_decoded_output() {
        use symphonia_core::audio::layouts;

        // HE-AAC v1: 22.05 kHz stereo core, explicit backwards compatible SBR at 44.1 kHz.
        let stsd = stsd_audio_entry(
            b"mp4a",
            0,
            2,
            22_050 << 16,
            &esds_aac(&[0x13, 0x90, 0x56, 0xe5, 0xa0]),
        );
        let params = audio_params(stsd);
        assert_eq!(params.sample_rate, Some(44_100));
        assert_eq!(params.channels, Some(layouts::CHANNEL_LAYOUT_STEREO));

        // HE-AAC v2: 22.05 kHz mono core, explicit SBR and PS signalling, stereo output.
        let stsd = stsd_audio_entry(
            b"mp4a",
            0,
            2,
            22_050 << 16,
            &esds_aac(&[0x13, 0x88, 0x56, 0xe5, 0xa5, 0x48, 0x80]),
        );
        let params = audio_params(stsd);
        assert_eq!(params.sample_rate, Some(44_100));
        assert_eq!(params.channels, Some(layouts::CHANNEL_LAYOUT_STEREO));
    }

    #[test]
    fn quicktime_alac_cookie_in_wave_atom() {
        use symphonia_core::audio::layouts;
        use symphonia_core::codecs::audio::well_known::CODEC_ID_ALAC;

        // The ALAC magic cookie.
        let mut cookie = vec![0; 4]; // Version + flags.
        cookie.extend_from_slice(&4096u32.to_be_bytes()); // Frame length.
        cookie.extend_from_slice(&[0, 16, 40, 10, 14, 2]); // Compat version, depth, pb, mb, kb, ch.
        cookie.extend_from_slice(&255u16.to_be_bytes()); // Max run.
        cookie.extend_from_slice(&[0; 8]); // Max frame bytes + average bit rate.
        cookie.extend_from_slice(&44_100u32.to_be_bytes()); // Sample rate.

        // The QuickTime version 1 sample entry stores it in a `wave` atom.
        let mut wave = atom(b"frma", b"alac");
        wave.extend_from_slice(&atom(b"alac", &cookie));
        wave.extend_from_slice(&atom(b"\0\0\0\0", &[]));

        let mut extra = Vec::new();
        extra.extend_from_slice(&[0; 16]); // Version 1 sample entry fields.
        extra.extend_from_slice(&atom(b"wave", &wave));

        let params = audio_params(stsd_audio_entry(b"alac", 1, 2, 44_100 << 16, &extra));
        assert_eq!(params.codec, CODEC_ID_ALAC);
        assert_eq!(params.sample_rate, Some(44_100));
        assert_eq!(params.channels, Some(layouts::CHANNEL_LAYOUT_STEREO));
        assert!(params.extra_data.is_some());
    }

    #[test]
    fn opus_dops_is_converted_to_opus_head() {
        use symphonia_core::codecs::audio::well_known::CODEC_ID_OPUS;

        // dOps: version 0, 2 channels, big-endian pre-skip (312), input rate (44100), and
        // output gain (-256), mapping family 0.
        let mut dops = vec![0, 2];
        dops.extend_from_slice(&312u16.to_be_bytes());
        dops.extend_from_slice(&44_100u32.to_be_bytes());
        dops.extend_from_slice(&(-256i16).to_be_bytes());
        dops.push(0);

        let params =
            audio_params(stsd_audio_entry(b"Opus", 0, 2, 48_000 << 16, &atom(b"dOps", &dops)));

        assert_eq!(params.codec, CODEC_ID_OPUS);
        assert_eq!(params.sample_rate, Some(48_000));

        // The extra data is a (little-endian, version 1) OpusHead.
        let mut head = b"OpusHead".to_vec();
        head.push(1);
        head.push(2);
        head.extend_from_slice(&312u16.to_le_bytes());
        head.extend_from_slice(&44_100u32.to_le_bytes());
        head.extend_from_slice(&(-256i16).to_le_bytes());
        head.push(0);
        assert_eq!(params.extra_data.as_deref(), Some(head.as_slice()));
    }

    #[test]
    fn opus_dops_with_channel_mapping_table() {
        // 6 channels, family 1: stream count, coupled count, then a table with 6 entries.
        let mut dops = vec![0, 6];
        dops.extend_from_slice(&312u16.to_be_bytes());
        dops.extend_from_slice(&48_000u32.to_be_bytes());
        dops.extend_from_slice(&0i16.to_be_bytes());
        dops.push(1);
        dops.extend_from_slice(&[4, 2, 0, 4, 1, 2, 3, 5]);

        let params =
            audio_params(stsd_audio_entry(b"Opus", 0, 6, 48_000 << 16, &atom(b"dOps", &dops)));

        let extra_data = params.extra_data.expect("opus extra data");
        assert_eq!(&extra_data[..9], b"OpusHead\x01");
        assert_eq!(&extra_data[19..], &[4, 2, 0, 4, 1, 2, 3, 5]);
        assert_eq!(params.channels.map(|c| c.count()), Some(6));
    }
}
