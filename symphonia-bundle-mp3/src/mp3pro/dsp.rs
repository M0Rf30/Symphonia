// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The mp3PRO SBR signal path (spec section 11).
//!
//! Per channel and 576-sample core frame:
//!
//! 1. 32-band complex QMF analysis of `[288 history | 576 new]` core samples gives 18 columns, written
//!    to rows 8..26 of the 26 x 64 row array (rows 0..8 carry the previous frame's rows 18..26).
//! 2. The HF generator ([`hfgen`]) and the envelope adjuster ([`envelope`]) act on rows 2..26: rows
//!    2..20 are the frame and rows 20..26 the 6-column look-ahead.
//! 3. 64-band synthesis of rows 2..20 gives 1152 output samples, then the rows are swapped for the
//!    next frame.
//!
//! Frames without usable SBR data take the core-only path: bands `>= kx` are zeroed.

// The kernels below are index-heavy numerical code that mirrors the formulas of the spec.
#![allow(clippy::needless_range_loop)]

mod envelope;
mod hfgen;
mod qmf;
mod tables;

use qmf::{
    ANALYSIS_BUF, COLUMNS, FIRST_ROW, FRAME_LEN, HISTORY, OUT_LEN, QmfTables, ROWS, Rows, Synth,
};

use super::{FreqTables, SbrFrame, SbrHeader};

/// Scale between the full-scale `[-1, 1]` PCM of the decoder and the int16 units of the SBR model.
const PCM_SCALE: f64 = 32768.0;

/// Per-channel decoder state.
struct Channel {
    /// The analysis input: 288 samples of history followed by the current 576-sample frame.
    tbuf: [f64; ANALYSIS_BUF],
    rows: Rows,
    synth: Synth,
    lpp: hfgen::State,
    env: envelope::State,
}

impl Channel {
    fn new() -> Self {
        Channel {
            tbuf: [0.0; ANALYSIS_BUF],
            rows: Rows::zeroed(),
            synth: Synth::new(),
            lpp: hfgen::State::default(),
            env: envelope::State::new(),
        }
    }
}

/// Reconstructs the high band of the core PCM from the parsed SBR data.
pub(crate) struct SbrDsp {
    qmf: Box<QmfTables>,
    tables: FreqTables,
    /// Output sample rate in Hz (twice the core rate).
    out_rate: u32,
    /// The patch table of the current header, `None` if the header yields no usable patch (the
    /// core-only path is taken then).
    patches: Option<hfgen::PatchTable>,
    channels: Vec<Channel>,
    /// Snapshot of the rows used by the HF generator.
    snapshot: Box<Rows>,
    /// Synthesis output of the current channel.
    out: [f64; OUT_LEN],
}

impl SbrDsp {
    /// Creates a decoder for `n_channels` channels at output rate `out_rate` (twice the core
    /// rate) using the frequency tables of the initial
    /// (default) header.
    pub fn new(n_channels: usize, out_rate: u32, _header: &SbrHeader, tables: &FreqTables) -> Self {
        SbrDsp {
            qmf: QmfTables::new(),
            tables: *tables,
            out_rate,
            patches: hfgen::PatchTable::new(tables, 0, out_rate),
            channels: (0..n_channels).map(|_| Channel::new()).collect(),
            snapshot: Box::new(Rows::zeroed()),
            out: [0.0; OUT_LEN],
        }
    }

    /// Applies a header change (spec section 11.11): the patch table is rebuilt, the QMF rows
    /// between the old and the new `kx` are cleared, and the envelope state is reset. The LPC
    /// bandwidth history is kept. `data_extra` is the `data_extra` field of channel 0.
    pub fn reset(&mut self, _header: &SbrHeader, tables: &FreqTables, data_extra: u8) {
        let (lo, hi) = if tables.kx < self.tables.kx {
            (tables.kx, self.tables.kx)
        }
        else {
            (self.tables.kx, tables.kx)
        };

        for ch in self.channels.iter_mut() {
            for row in 0..ROWS {
                ch.rows.re[row][lo..hi].fill(0.0);
                ch.rows.im[row][lo..hi].fill(0.0);
            }
            ch.env.reset();
        }

        self.tables = *tables;
        self.patches = hfgen::PatchTable::new(tables, data_extra, self.out_rate);
    }

    /// Processes one channel of one 576-sample core frame into 1152 output samples.
    ///
    /// `core` and `out` are full-scale (`[-1, 1]`). `frame` is `Some` on the SBR path and `None`
    /// on the core-only path.
    pub fn process(
        &mut self,
        ch: usize,
        core: &[f32],
        frame: Option<&SbrFrame>,
        header: &SbrHeader,
        out: &mut [f32],
    ) {
        debug_assert_eq!(core.len(), FRAME_LEN);
        debug_assert_eq!(out.len(), OUT_LEN);

        let kx = self.tables.kx;
        let state = &mut self.channels[ch];

        // 1. Analysis.
        state.tbuf.copy_within(FRAME_LEN.., 0);
        for (dst, &src) in state.tbuf[HISTORY..].iter_mut().zip(core) {
            *dst = f64::from(src) * PCM_SCALE;
        }

        let rows = &mut state.rows;
        for row in FIRST_ROW..ROWS {
            rows.re[row] = [0.0; 64];
            rows.im[row] = [0.0; 64];
        }
        self.qmf.analyse(&state.tbuf, kx, rows);

        // 2. HF generation and envelope adjustment, or zeroing of the HF bands.
        let sbr = match (frame, self.patches.as_ref()) {
            (Some(frame), Some(patches)) => Some((frame, patches)),
            _ => None,
        };

        match sbr {
            Some((frame, patches)) => {
                patches.process(&mut state.lpp, rows, &mut self.snapshot, frame);
                state.env.process(rows, frame, header, &self.tables);
            }
            None => {
                for row in 2..ROWS {
                    rows.re[row][kx..].fill(0.0);
                    rows.im[row][kx..].fill(0.0);
                }
            }
        }

        // 3. Synthesis.
        state.synth.run(&self.qmf, rows, &mut self.out);
        for (dst, &src) in out.iter_mut().zip(self.out.iter()) {
            *dst = (src / PCM_SCALE) as f32;
        }

        // Row bookkeeping for the next frame (spec section 11.5).
        for i in 0..ROWS - COLUMNS {
            rows.re.swap(i, i + COLUMNS);
            rows.im.swap(i, i + COLUMNS);
        }
        if sbr.is_some() {
            if state.lpp.extended {
                for i in 0..2 {
                    let (src_re, src_im) = (rows.re[i + COLUMNS], rows.im[i + COLUMNS]);
                    rows.re[i][..kx].copy_from_slice(&src_re[..kx]);
                    rows.im[i][..kx].copy_from_slice(&src_im[..kx]);
                }
            }
        }
        else {
            for i in 0..2 {
                rows.re[i] = [0.0; 64];
                rows.im[i] = [0.0; 64];
            }
        }
    }
}
