// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The decoder of the frames of a channel: a port of the decoder of liblc3 `src/lc3.c`
//! (`lc3_decode`, `decode`, `synthesize`, `complete`) and of the packet loss concealment of
//! `src/plc.c`.

use crate::bits::Bits;
use crate::common;
use crate::ltpf::{self, LtpfData, LtpfSynthesis};
use crate::mdct::{self, MdctScratch};
use crate::sns::{self, SnsData};
use crate::spec;
use crate::tns::{self, TnsData};

/// The frame duration and the sample rate of a stream.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Mode {
    /// Frame duration identifier (`enum lc3_dt`).
    pub dt: usize,
    /// Sample rate identifier (`enum lc3_srate`).
    pub sr: usize,
}

impl Mode {
    /// The number of samples of a frame.
    pub fn ns(&self) -> usize {
        common::ns(self.dt, self.sr)
    }
}

/// State of the packet loss concealment (`struct lc3_plc_state`).
#[derive(Copy, Clone)]
struct Plc {
    seed: u32,
    count: u32,
    alpha: f32,
}

impl Plc {
    /// Resets the state (`lc3_plc_reset`).
    fn new() -> Self {
        Plc { seed: 24607, count: 1, alpha: 1.0 }
    }

    /// Suspends the concealment, a good frame was received (`lc3_plc_suspend`).
    fn suspend(&mut self) {
        self.count = 1;
        self.alpha = 1.0;
    }

    /// Synthesis of a concealed frame from the last good spectral coefficients `x`
    /// (`lc3_plc_synthesize`).
    fn synthesize(&mut self, ne: usize, x: &[f32], y: &mut [f32]) {
        let mut seed = self.seed;
        let mut alpha = self.alpha;

        alpha *= if self.count < 4 {
            1.0
        }
        else if self.count < 8 {
            0.9
        }
        else {
            0.85
        };

        for i in 0..ne {
            seed = (16831 + seed * 12821) & 0xffff;
            y[i] = alpha * (if seed & 0x8000 != 0 { -x[i] } else { x[i] });
        }

        self.seed = seed;
        self.alpha = alpha;
        self.count = self.count.saturating_add(1);
    }
}

/// Side data of a frame (`struct side_data`).
struct SideData {
    bw: usize,
    pitch_present: bool,
    ltpf: LtpfData,
    sns: SnsData,
    tns: TnsData,
}

/// The decoder state of a channel (`struct lc3_decoder`).
struct Channel {
    /// The history ring buffer, of `ns + nh` samples (`x` at `xh_off`).
    ring: Vec<f32>,
    /// The offset of the current frame in the ring (`xs_off`).
    xs_off: usize,
    /// The MDCT delayed samples (`x` at `xd_off`).
    xd: Vec<f32>,
    /// The last good spectral coefficients, after the noise shaping (`x` at `xg_off`).
    xg: Vec<f32>,
    plc: Plc,
    ltpf: LtpfSynthesis,
}

impl Channel {
    fn new(mode: Mode) -> Self {
        let ns = mode.ns();
        let nh = common::nh(mode.dt, mode.sr);
        let nd = common::nd(mode.dt, mode.sr);

        Channel {
            ring: vec![0.0; nh + ns],
            xs_off: nh,
            xd: vec![0.0; nd],
            xg: vec![0.0; ns],
            plc: Plc::new(),
            ltpf: LtpfSynthesis::new(),
        }
    }

    /// Decodes the bitstream of a frame, the spectral coefficients being left in the ring at
    /// `xs_off` (`decode`). Returns `None` if a bitstream error is detected.
    fn decode_bitstream(&mut self, mode: Mode, data: &[u8]) -> Option<SideData> {
        let (dt, sr) = (mode.dt, mode.sr);
        let nbytes = data.len();

        let ns = mode.ns();
        let ne = common::ne(dt, sr);

        let xf = &mut self.ring[self.xs_off..self.xs_off + ns];

        let mut bits = Bits::new(data);

        // Bandwidth indication (`lc3_bwdet_get_bw`).
        let nbits_bw = if common::is_hr(sr) {
            0
        }
        else {
            i32::from(sr > 0) + i32::from(sr > 1) + i32::from(sr > 3)
        };
        let bw = if nbits_bw == 0 { sr } else { bits.get_bits(nbits_bw) as usize };
        if bw > sr {
            return None;
        }

        let spec = spec::get_side(&mut bits, dt, sr)?;

        let tns = tns::get_data(&mut bits, dt, bw, nbytes)?;

        let pitch_present = bits.get_bit() != 0;

        let sns = sns::get_data(&mut bits)?;

        let ltpf = if pitch_present { ltpf::get_data(&mut bits) } else { LtpfData::default() };

        if !spec::decode(&mut bits, dt, sr, bw, nbytes, &spec, xf) {
            return None;
        }

        xf[ne..].fill(0.0);

        if !bits.check() {
            return None;
        }

        Some(SideData { bw, pitch_present, ltpf, sns, tns })
    }

    /// Frame synthesis (`synthesize`), from the spectral coefficients in the ring, or by packet
    /// loss concealment when `side` is `None`. Returns `false`, having done nothing, if the
    /// synthesis was requested of invalid side data.
    fn synthesize(
        &mut self,
        mode: Mode,
        side: Option<&SideData>,
        nbytes: usize,
        mdct_scratch: &mut MdctScratch,
        tmp: &mut [f32],
    ) -> bool {
        let (dt, sr) = (mode.dt, mode.sr);
        let ns = mode.ns();
        let ne = common::ne(dt, sr);
        let xo = self.xs_off;

        let mut done = false;

        if let Some(side) = side {
            let xf = &mut self.ring[xo..xo + ns];

            tns::synthesize(dt, side.bw, &side.tns, xf);

            if sns::synthesize(dt, sr, &side.sns, xf, &mut self.xg) {
                self.plc.suspend();

                let xs = &mut self.ring[xo..xo + ns];
                done = mdct::inverse(dt, sr, &self.xg, &mut self.xd, xs, mdct_scratch);
            }
            else {
                return false;
            }
        }

        if !done {
            self.plc.synthesize(ne, &self.xg, tmp);
            tmp[ne..ns].fill(0.0);

            let xs = &mut self.ring[xo..xo + ns];
            mdct::inverse(dt, sr, tmp, &mut self.xd, xs, mdct_scratch);
        }

        if !common::is_hr(sr) {
            let data = side.filter(|s| s.pitch_present).map(|s| &s.ltpf);
            self.ltpf.synthesize(dt, sr, nbytes, data, &mut self.ring, xo);
        }

        true
    }

    /// Updates the state on the completion of a frame (`complete`).
    fn complete(&mut self, mode: Mode) {
        let nh = common::nh(mode.dt, mode.sr);

        self.xs_off = if self.xs_off < nh { self.xs_off + mode.ns() } else { 0 };
    }
}

/// The decoder of the frames of all the channels of a stream.
pub struct FrameDecoder {
    mode: Mode,
    channels: Vec<Channel>,
    mdct: MdctScratch,
    tmp: Vec<f32>,
}

impl FrameDecoder {
    /// Creates the decoder of `num_channels` channels.
    pub fn new(mode: Mode, num_channels: usize) -> Self {
        FrameDecoder {
            mode,
            channels: (0..num_channels).map(|_| Channel::new(mode)).collect(),
            mdct: MdctScratch::new(common::MAX_NS),
            tmp: vec![0.0; mode.ns()],
        }
    }

    /// Resets the state of the decoder.
    pub fn reset(&mut self) {
        for ch in self.channels.iter_mut() {
            *ch = Channel::new(self.mode);
        }
    }

    /// Returns the largest size of the data of the frame of a channel (`lc3_max_frame_bytes`).
    pub fn max_frame_bytes(&self) -> usize {
        common::max_frame_bytes(self.mode.dt, self.mode.sr)
    }

    /// Decodes the frame of a channel (`lc3_decode`, with floating-point output).
    ///
    /// `data` is the frame of the channel, or `None` to conceal a lost frame. `out` receives the
    /// `ns` samples. The caller checks the size of the frame. Returns `true` if the frame was
    /// concealed because the bitstream is invalid.
    pub fn decode(&mut self, ch: usize, data: Option<&[u8]>, out: &mut [f32]) -> bool {
        let mode = self.mode;
        let ns = mode.ns();

        let Some(chan) = self.channels.get_mut(ch)
        else {
            return false;
        };

        let side = data.and_then(|d| chan.decode_bitstream(mode, d));
        let nbytes = data.map_or(0, <[u8]>::len);

        let mut concealed = data.is_some() && side.is_none();

        if !chan.synthesize(mode, side.as_ref(), nbytes, &mut self.mdct, &mut self.tmp) {
            // The noise shaping data is invalid: conceal the frame.
            concealed = true;
            chan.synthesize(mode, None, nbytes, &mut self.mdct, &mut self.tmp);
        }

        // Store the samples as floating-point values (`store_float`): the decoder works with
        // 16-bit sample values, which are scaled to the [-1, 1] range.
        const SCALE: f32 = 1.0 / 32768.0;

        let xs = &chan.ring[chan.xs_off..chan.xs_off + ns];
        for (o, &s) in out.iter_mut().zip(xs) {
            let s = s * SCALE;
            *o = if s.is_finite() { s.clamp(-1.0, 1.0) } else { 0.0 };
        }

        chan.complete(mode);

        concealed
    }
}
