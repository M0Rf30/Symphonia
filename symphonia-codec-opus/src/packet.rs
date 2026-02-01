// Opus Packet Parser - Rewritten from xiph/opus src/opus.c
// Copyright (c) 2010 Xiph.Org Foundation, Skype Limited
// SPDX-License-Identifier: BSD-3-Clause

/// Opus mode (codec selection)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpusMode {
    /// SILK only (speech, low frequencies)
    SilkOnly,
    /// Hybrid (SILK + CELT)
    Hybrid,
    /// CELT only (music, full bandwidth)
    CeltOnly,
}

/// Opus bandwidth
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpusBandwidth {
    Narrowband,    // 4 kHz
    Mediumband,    // 6 kHz
    Wideband,      // 8 kHz
    SuperWideband, // 12 kHz
    Fullband,      // 20 kHz
}

/// Frame structure
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameCode {
    /// One frame
    SingleFrame,
    /// Two CBR (constant bitrate) frames
    TwoCbrFrames,
    /// Two VBR (variable bitrate) frames
    TwoVbrFrames,
    /// Multiple frames (CBR or VBR)
    MultipleFrames,
}

/// Parsed Opus packet information
#[derive(Debug)]
pub struct OpusPacket<'a> {
    /// Table of Contents byte
    pub toc: u8,
    /// Codec mode
    pub mode: OpusMode,
    /// Bandwidth
    pub bandwidth: OpusBandwidth,
    /// Frame size in samples (at 48kHz)
    pub frame_size: usize,
    /// Number of frames in packet
    pub frame_count: usize,
    /// Frame data slices
    pub frames: Vec<&'a [u8]>,
    /// Whether frames are constant bitrate
    pub is_cbr: bool,
}

impl<'a> OpusPacket<'a> {
    /// Parse an Opus packet
    ///
    /// Arguments:
    /// - data: Raw packet data
    ///
    /// Returns: Parsed packet information or error
    pub fn parse(data: &'a [u8]) -> Result<Self, &'static str> {
        if data.is_empty() {
            return Err("Empty packet");
        }

        let toc = data[0];

        // Extract mode from TOC byte
        let mode = if (toc & 0x80) != 0 {
            OpusMode::CeltOnly
        } else if (toc & 0x60) == 0x60 {
            OpusMode::Hybrid
        } else {
            OpusMode::SilkOnly
        };

        // Extract bandwidth and frame size
        let (bandwidth, frame_size) = Self::decode_toc(toc, mode)?;

        // Extract frame code (bits 0-1)
        let frame_code = match toc & 0x3 {
            0 => FrameCode::SingleFrame,
            1 => FrameCode::TwoCbrFrames,
            2 => FrameCode::TwoVbrFrames,
            3 => FrameCode::MultipleFrames,
            _ => unreachable!(),
        };

        // Parse frames based on frame code
        let (frame_count, frames, is_cbr) = Self::parse_frames(
            &data[1..],
            frame_code,
        )?;

        Ok(Self {
            toc,
            mode,
            bandwidth,
            frame_size,
            frame_count,
            frames,
            is_cbr,
        })
    }

    /// Decode TOC byte to get bandwidth and frame size
    fn decode_toc(toc: u8, mode: OpusMode) -> Result<(OpusBandwidth, usize), &'static str> {
        // Configuration is in bits 3-7
        let config = (toc >> 3) & 0x1F;

        match mode {
            OpusMode::CeltOnly => {
                // For CELT: bits 3-5 determine frame size
                let size_code = config & 0x3;
                let frame_size = 120 << size_code; // 120, 240, 480, or 960

                // Bit 6-7 determine bandwidth
                let bw_code = (config >> 2) & 0x3;
                let bandwidth = match bw_code {
                    0 => OpusBandwidth::Narrowband,
                    1 => OpusBandwidth::Wideband,
                    2 => OpusBandwidth::SuperWideband,
                    3 => OpusBandwidth::Fullband,
                    _ => unreachable!(),
                };

                Ok((bandwidth, frame_size))
            }
            OpusMode::Hybrid => {
                // Hybrid mode
                let frame_size = if (toc & 0x08) != 0 { 960 } else { 480 };
                Ok((OpusBandwidth::SuperWideband, frame_size))
            }
            OpusMode::SilkOnly => {
                // For SILK: extract frame duration
                let size_code = config & 0x3;
                let frame_size = match size_code {
                    0 => 480,  // 10ms at 48kHz
                    1 => 960,  // 20ms at 48kHz
                    2 => 1920, // 40ms at 48kHz
                    3 => 2880, // 60ms at 48kHz
                    _ => unreachable!(),
                };

                // Bandwidth from config
                let bw_code = (config >> 2) & 0x3;
                let bandwidth = match bw_code {
                    0 => OpusBandwidth::Narrowband,
                    1 => OpusBandwidth::Mediumband,
                    2 => OpusBandwidth::Wideband,
                    _ => OpusBandwidth::SuperWideband,
                };

                Ok((bandwidth, frame_size))
            }
        }
    }

    /// Parse frame data based on frame code
    fn parse_frames(
        data: &'a [u8],
        frame_code: FrameCode,
    ) -> Result<(usize, Vec<&'a [u8]>, bool), &'static str> {
        match frame_code {
            FrameCode::SingleFrame => {
                // One frame, entire data
                Ok((1, vec![data], true))
            }
            FrameCode::TwoCbrFrames => {
                // Two equal-sized frames
                if data.len() % 2 != 0 {
                    return Err("Invalid CBR frame length");
                }
                let size = data.len() / 2;
                Ok((2, vec![&data[0..size], &data[size..]], true))
            }
            FrameCode::TwoVbrFrames => {
                // Two VBR frames: first byte is size of first frame
                if data.is_empty() {
                    return Err("Empty VBR frame data");
                }
                let size1 = Self::parse_size(&data[0..])?;
                let offset = Self::size_bytes(size1);

                if offset + size1 > data.len() {
                    return Err("Invalid VBR frame size");
                }

                let frame1 = &data[offset..offset + size1];
                let frame2 = &data[offset + size1..];

                Ok((2, vec![frame1, frame2], false))
            }
            FrameCode::MultipleFrames => {
                // Multiple frames with count byte
                if data.is_empty() {
                    return Err("Empty multiple frames data");
                }

                let count_byte = data[0];
                let frame_count = (count_byte & 0x3F) as usize;

                if frame_count == 0 || frame_count > 48 {
                    return Err("Invalid frame count");
                }

                let is_cbr = (count_byte & 0x80) == 0;
                let has_padding = (count_byte & 0x40) != 0;

                let mut offset = 1;

                // Skip padding if present
                if has_padding {
                    loop {
                        if offset >= data.len() {
                            return Err("Invalid padding");
                        }
                        let pad = data[offset];
                        offset += 1;
                        if pad != 255 {
                            break;
                        }
                    }
                }

                let remaining = &data[offset..];

                if is_cbr {
                    // CBR: equal-sized frames
                    let frame_size = remaining.len() / frame_count;
                    if frame_size * frame_count != remaining.len() {
                        return Err("Invalid CBR frame alignment");
                    }

                    let mut frames = Vec::new();
                    for i in 0..frame_count {
                        let start = i * frame_size;
                        frames.push(&remaining[start..start + frame_size]);
                    }

                    Ok((frame_count, frames, true))
                } else {
                    // VBR: size for each frame
                    let mut frames = Vec::new();
                    let mut data_offset = 0;

                    for _i in 0..frame_count - 1 {
                        let size = Self::parse_size(&remaining[data_offset..])?;
                        let bytes = Self::size_bytes(size);
                        data_offset += bytes;

                        if data_offset + size > remaining.len() {
                            return Err("Invalid VBR frame size");
                        }

                        frames.push(&remaining[data_offset..data_offset + size]);
                        data_offset += size;
                    }

                    // Last frame is remaining data
                    frames.push(&remaining[data_offset..]);

                    Ok((frame_count, frames, false))
                }
            }
        }
    }

    /// Parse size field (1 or 2 bytes)
    fn parse_size(data: &[u8]) -> Result<usize, &'static str> {
        if data.is_empty() {
            return Err("Empty size field");
        }

        let first = data[0] as usize;
        if first < 252 {
            Ok(first)
        } else if data.len() < 2 {
            Err("Incomplete size field")
        } else {
            Ok(4 * first + (data[1] as usize))
        }
    }

    /// Get number of bytes used for size field
    fn size_bytes(size: usize) -> usize {
        if size < 252 {
            1
        } else {
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_single_frame() {
        // TOC: 0xFC = CELT, fullband, 960 samples, single frame
        let data = vec![0xFC, 0x01, 0x02, 0x03];
        let packet = OpusPacket::parse(&data).unwrap();

        assert_eq!(packet.mode, OpusMode::CeltOnly);
        assert_eq!(packet.frame_count, 1);
        assert_eq!(packet.frames.len(), 1);
        assert_eq!(packet.frames[0], &[0x01, 0x02, 0x03]);
    }

    #[test]
    fn test_parse_two_cbr_frames() {
        // TOC: 0xFD = CELT, fullband, 960 samples, two CBR frames (bit 0 set)
        let data = vec![0xFD, 0x01, 0x01, 0x02, 0x02];
        let packet = OpusPacket::parse(&data).unwrap();

        assert_eq!(packet.frame_count, 2);
        assert!(packet.is_cbr);
        assert_eq!(packet.frames.len(), 2);
    }

    #[test]
    fn test_mode_detection() {
        // CELT mode (bit 7 set)
        let data = vec![0x80, 0x00];
        let packet = OpusPacket::parse(&data).unwrap();
        assert_eq!(packet.mode, OpusMode::CeltOnly);

        // Hybrid mode (bits 6-5 = 0b11)
        let data = vec![0x60, 0x00];
        let packet = OpusPacket::parse(&data).unwrap();
        assert_eq!(packet.mode, OpusMode::Hybrid);

        // SILK mode (neither condition)
        let data = vec![0x00, 0x00];
        let packet = OpusPacket::parse(&data).unwrap();
        assert_eq!(packet.mode, OpusMode::SilkOnly);
    }

    #[test]
    fn test_empty_packet() {
        let data = vec![];
        assert!(OpusPacket::parse(&data).is_err());
    }
}
