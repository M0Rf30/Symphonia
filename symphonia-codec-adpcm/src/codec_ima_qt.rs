// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::errors::{Result, decode_error};
use symphonia_core::io::ReadBytes;

use crate::common::Nibble;
use crate::common_ima::AdpcmImaBlockStatus;

/// Sentinel step index marking a channel state that carries no history (stream start, after a
/// reset). It can never equal a step index read from a packet header.
const NO_HISTORY: i32 = -1;

/// Per-channel decoder state carried across packets.
///
/// The 16-bit QuickTime packet header only holds the top 9 bits of the predictor, so the low 7
/// bits are lost. When a packet continues directly from the previous one (same step index and a
/// predictor within 0x7f of the running one), Apple's decoder, and FFmpeg's `adpcm_ima_qt`
/// decoder, keep the running predictor rather than the truncated header value.
pub(crate) type QtChannelState = AdpcmImaBlockStatus;

pub(crate) fn new_state() -> QtChannelState {
    AdpcmImaBlockStatus { predictor: 0, step_index: NO_HISTORY }
}

fn read_preamble<B: ReadBytes>(
    stream: &mut B,
    state: &QtChannelState,
) -> Result<AdpcmImaBlockStatus> {
    let header = stream.read_be_u16()?;
    let header_predictor = (header & 0xFF80) as i16 as i32;
    let step_index = (header & 0x7F) as i32;
    if step_index > 88 {
        return decode_error("adpcm (ima-qt): invalid step index");
    }

    let continues =
        state.step_index == step_index && (header_predictor - state.predictor).abs() <= 0x7f;
    let predictor = if continues { state.predictor } else { header_predictor };

    Ok(AdpcmImaBlockStatus { predictor, step_index })
}

pub(crate) fn decode_mono<B: ReadBytes>(
    stream: &mut B,
    buffer: &mut [i32],
    state: &mut QtChannelState,
) -> Result<()> {
    // IMA4 always uses 34 byte packets
    // https://wiki.multimedia.cx/index.php/Apple_QuickTime_IMA_ADPCM
    let mut status = read_preamble(stream, state)?;
    for byte in 0..32 {
        let nibbles = stream.read_u8()?;
        buffer[byte * 2] = status.expand_nibble(nibbles, Nibble::Lower);
        buffer[byte * 2 + 1] = status.expand_nibble(nibbles, Nibble::Upper);
    }
    *state = status;
    Ok(())
}

pub(crate) fn decode_stereo<B: ReadBytes>(
    stream: &mut B,
    buffers: [&mut [i32]; 2],
    state: &mut [QtChannelState; 2],
) -> Result<()> {
    let [left, right] = buffers;
    decode_mono(stream, left, &mut state[0])?;
    decode_mono(stream, right, &mut state[1])?;
    Ok(())
}
