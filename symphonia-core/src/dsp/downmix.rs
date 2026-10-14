// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Channel downmixing with the matrices of Recommendation ITU-R BS.775.
//!
//! The matrices are those of Table 2 of Annex 4 ("Downward mixing equations for 3/2 source
//! material") in Rec. ITU-R BS.775-4, <https://www.itu.int/rec/R-REC-BS.775>. The Recommendation
//! only defines the mixing of a 3/2 source (L, R, C, LS, RS) to the formats 1/0 (mono), 2/0
//! (stereo), 3/0, 2/1, 3/1 and 2/2. A source with fewer channels is mixed by treating the absent
//! channels as silent.
//!
//! Further points that are taken from the Recommendation:
//!
//!  * The low frequency effects channel is not part of the equations and is discarded. Annex 7
//!    notes that the LFE channel is "often not included in a 2-channel downmix" and Annex 8 §5 that
//!    it is typically not included in a downmix.
//!  * A mono surround signal (MS) is "fed to both LS and RS" (Recommends 3), which is how a
//!    [`REAR_CENTER`](Position::REAR_CENTER) source channel enters the equations.
//!  * The surround coefficient of the 2/0, 3/0, 2/1 and 3/1 formats is 0.7071 in Table 2; Annex 8
//!    allows a broadcaster to choose 0.7071, 0.5000 or 0.0000 instead (see [`SurroundLevel`]).
//!
//! The equations are not normalized, so the sum of several full-scale channels may exceed full
//! scale. Use [`DownmixOptions::normalize`] to prevent that.

use std::f32::consts::FRAC_1_SQRT_2;

use crate::audio::{Channels, Position};
use crate::errors::{Result, unsupported_error};

/// The columns of the Table 2 equations: L, R, C, LS, RS.
const NUM_ROLES: usize = 5;

const L: usize = 0;
const R: usize = 1;
const C: usize = 2;
const LS: usize = 3;
const RS: usize = 4;

/// The coefficient of -3 dB. Table 2 lists it as 0.7071.
const M3DB: f32 = FRAC_1_SQRT_2;

/// Table 2, Mono - 1/0 format.
const EQ_MONO: [[f32; NUM_ROLES]; 1] = [[M3DB, M3DB, 1.0, 0.5, 0.5]];
/// Table 2, Stereo - 2/0 format.
const EQ_STEREO: [[f32; NUM_ROLES]; 2] = [[1.0, 0.0, M3DB, M3DB, 0.0], [0.0, 1.0, M3DB, 0.0, M3DB]];
/// Table 2, Three channels - 3/0 format.
const EQ_3_0: [[f32; NUM_ROLES]; 3] =
    [[1.0, 0.0, 0.0, M3DB, 0.0], [0.0, 1.0, 0.0, 0.0, M3DB], [0.0, 0.0, 1.0, 0.0, 0.0]];
/// Table 2, Three channels - 2/1 format. The third output is the mono surround channel S.
const EQ_2_1: [[f32; NUM_ROLES]; 3] =
    [[1.0, 0.0, M3DB, 0.0, 0.0], [0.0, 1.0, M3DB, 0.0, 0.0], [0.0, 0.0, 0.0, M3DB, M3DB]];
/// Table 2, Four channels - 3/1 format. The fourth output is the mono surround channel S.
const EQ_3_1: [[f32; NUM_ROLES]; 4] = [
    [1.0, 0.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 0.0, M3DB, M3DB],
];
/// Table 2, Four channels - 2/2 format.
const EQ_2_2: [[f32; NUM_ROLES]; 4] = [
    [1.0, 0.0, M3DB, 0.0, 0.0],
    [0.0, 1.0, M3DB, 0.0, 0.0],
    [0.0, 0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 0.0, 1.0],
];

/// The level at which the surround channels are mixed into the front channels.
///
/// Annex 8 of Rec. ITU-R BS.775-4 names these three coefficients as the alternatives a broadcaster
/// may signal. The setting replaces the 0.7071 surround coefficient of the 2/0, 3/0, 2/1 and 3/1
/// equations. The fixed coefficients of the mono (0.5000) and 2/2 (1.0000) equations are not
/// affected.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum SurroundLevel {
    /// 0.7071 (-3 dB), the coefficient of Table 2.
    #[default]
    Minus3Db,
    /// 0.5000 (-6 dB).
    Minus6Db,
    /// 0.0000, the surround channels are discarded.
    Off,
}

impl SurroundLevel {
    fn coefficient(self) -> f32 {
        match self {
            SurroundLevel::Minus3Db => M3DB,
            SurroundLevel::Minus6Db => 0.5,
            SurroundLevel::Off => 0.0,
        }
    }
}

/// Options for [`DownmixMatrix::bs775`].
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct DownmixOptions {
    /// The level at which the surround channels are mixed.
    pub surround_level: SurroundLevel,
    /// If `true`, all coefficients are divided by the largest sum of absolute coefficients of any
    /// output channel, if that sum exceeds 1. Full-scale input can then never exceed full scale
    /// at the output. The relative levels of the equations are preserved.
    pub normalize: bool,
}

/// A matrix of `dst_channels` x `src_channels` gains mixing a set of source channels into a set of
/// destination channels: `dst[d] = sum over s of coefficient(d, s) * src[s]`.
///
/// Channels are indexed in canonical order (see
/// [`Channels::get_canonical_index_for_positioned_channel`]).
#[derive(Clone, Debug, PartialEq)]
pub struct DownmixMatrix {
    src_channels: usize,
    dst_channels: usize,
    /// Row-major, one row per destination channel.
    coeffs: Vec<f32>,
}

/// How one source channel enters the 3/2 equations: the weights of the roles it feeds.
fn roles_of(pos: Position) -> &'static [usize] {
    match pos {
        Position::FRONT_LEFT => &[L],
        Position::FRONT_RIGHT => &[R],
        Position::FRONT_CENTER => &[C],
        Position::REAR_LEFT | Position::SIDE_LEFT => &[LS],
        Position::REAR_RIGHT | Position::SIDE_RIGHT => &[RS],
        // A mono surround signal is fed to both LS and RS.
        Position::REAR_CENTER => &[LS, RS],
        // The LFE channel is not part of the equations.
        _ => &[],
    }
}

impl DownmixMatrix {
    /// Creates the Rec. ITU-R BS.775 downmix matrix from the positioned channels `src` to the
    /// positioned channels `dst`.
    ///
    /// `dst` must be one of the formats of Table 2:
    ///
    ///  * mono: `FRONT_CENTER`,
    ///  * stereo: `FRONT_LEFT | FRONT_RIGHT`,
    ///  * 3/0: `FRONT_LEFT | FRONT_RIGHT | FRONT_CENTER`,
    ///  * 2/1: `FRONT_LEFT | FRONT_RIGHT | REAR_CENTER`,
    ///  * 3/1: `FRONT_LEFT | FRONT_RIGHT | FRONT_CENTER | REAR_CENTER`,
    ///  * 2/2: `FRONT_LEFT | FRONT_RIGHT` and either `REAR_LEFT | REAR_RIGHT` or
    ///    `SIDE_LEFT | SIDE_RIGHT`.
    ///
    /// `src` may be any subset of `FRONT_LEFT`, `FRONT_RIGHT`, `FRONT_CENTER`, `REAR_LEFT`,
    /// `REAR_RIGHT`, `SIDE_LEFT`, `SIDE_RIGHT` and `REAR_CENTER`, plus the LFE channels, which are
    /// discarded. The left/right surround pair (LS, RS) may be the rear or the side pair, but not
    /// both, and not together with `REAR_CENTER`, because Rec. ITU-R BS.775 does not define how
    /// to combine them. A source with more channels than that (e.g., 7.1) needs a matrix built
    /// with [`DownmixMatrix::from_coefficients`].
    ///
    /// # Errors
    ///
    /// Returns an unsupported error if `src` or `dst` are not positioned channel sets of the
    /// above kind, or if `dst` has more channels than `src` has non-LFE channels (a downmix cannot
    /// create channels).
    pub fn bs775(src: &Channels, dst: &Channels, opts: &DownmixOptions) -> Result<Self> {
        let (Channels::Positioned(src_mask), Channels::Positioned(dst_mask)) = (src, dst)
        else {
            return unsupported_error("core (downmix): channels must be positioned");
        };
        let (src_mask, dst_mask) = (*src_mask, *dst_mask);

        const FRONT: Position = Position::FRONT_LEFT.union(Position::FRONT_RIGHT);
        const REAR_PAIR: Position = Position::REAR_LEFT.union(Position::REAR_RIGHT);
        const SIDE_PAIR: Position = Position::SIDE_LEFT.union(Position::SIDE_RIGHT);
        const LFE: Position = Position::LFE1.union(Position::LFE2);

        // The source must be a subset of 3/2 (+ mono surround, + LFE).
        let known =
            FRONT | Position::FRONT_CENTER | REAR_PAIR | SIDE_PAIR | Position::REAR_CENTER | LFE;

        if !known.contains(src_mask) {
            return unsupported_error("core (downmix): unsupported source channel");
        }

        let has_rear = src_mask.intersects(REAR_PAIR);
        let has_side = src_mask.intersects(SIDE_PAIR);
        let has_mono_surround = src_mask.contains(Position::REAR_CENTER);

        if (has_rear && has_side) || (has_mono_surround && (has_rear || has_side)) {
            return unsupported_error("core (downmix): ambiguous surround channels in source");
        }

        // Select the equations for the destination format.
        let equations: &[[f32; NUM_ROLES]] = if dst_mask == Position::FRONT_CENTER {
            &EQ_MONO
        }
        else if dst_mask == FRONT {
            &EQ_STEREO
        }
        else if dst_mask == FRONT | Position::FRONT_CENTER {
            &EQ_3_0
        }
        else if dst_mask == FRONT | Position::REAR_CENTER {
            &EQ_2_1
        }
        else if dst_mask == FRONT | Position::FRONT_CENTER | Position::REAR_CENTER {
            &EQ_3_1
        }
        else if dst_mask == FRONT | REAR_PAIR || dst_mask == FRONT | SIDE_PAIR {
            &EQ_2_2
        }
        else {
            return unsupported_error("core (downmix): unsupported destination format");
        };

        let src_count = src_mask.bits().count_ones() as usize;
        let dst_count = equations.len();

        if dst_count > (src_mask & !LFE).bits().count_ones() as usize {
            return unsupported_error("core (downmix): destination has more channels than source");
        }

        let surround = opts.surround_level.coefficient();

        let mut coeffs = vec![0.0; dst_count * src_count];

        for (s, pos) in src_mask.iter().enumerate() {
            for &role in roles_of(pos) {
                for (d, row) in equations.iter().enumerate() {
                    let mut gain = row[role];

                    // Annex 8: the alternative surround coefficients replace 0.7071.
                    if (role == LS || role == RS) && gain == M3DB {
                        gain = surround;
                    }

                    coeffs[d * src_count + s] += gain;
                }
            }
        }

        let mut matrix = DownmixMatrix { src_channels: src_count, dst_channels: dst_count, coeffs };

        if opts.normalize {
            matrix.normalize();
        }

        Ok(matrix)
    }

    /// Creates a matrix from row-major coefficients, one row of `src_channels` gains per
    /// destination channel.
    ///
    /// Returns `None` if `coeffs.len() != dst_channels * src_channels`.
    pub fn from_coefficients(
        src_channels: usize,
        dst_channels: usize,
        coeffs: Vec<f32>,
    ) -> Option<Self> {
        (src_channels.checked_mul(dst_channels) == Some(coeffs.len())).then_some(DownmixMatrix {
            src_channels,
            dst_channels,
            coeffs,
        })
    }

    /// Divides every coefficient by the largest sum of absolute coefficients over the rows, if
    /// that sum exceeds 1.
    fn normalize(&mut self) {
        let peak = self
            .coeffs
            .chunks_exact(self.src_channels.max(1))
            .map(|row| row.iter().map(|c| c.abs()).sum::<f32>())
            .fold(0.0, f32::max);

        if peak > 1.0 {
            self.coeffs.iter_mut().for_each(|c| *c /= peak);
        }
    }

    /// The number of source channels.
    pub fn src_channels(&self) -> usize {
        self.src_channels
    }

    /// The number of destination channels.
    pub fn dst_channels(&self) -> usize {
        self.dst_channels
    }

    /// The gain of source channel `src` in destination channel `dst`.
    ///
    /// # Panics
    ///
    /// Panics if either index is out of range.
    pub fn coefficient(&self, dst: usize, src: usize) -> f32 {
        assert!(dst < self.dst_channels && src < self.src_channels, "channel out of range");
        self.coeffs[dst * self.src_channels + src]
    }

    /// The coefficients, row-major with one row per destination channel.
    pub fn coefficients(&self) -> &[f32] {
        &self.coeffs
    }

    /// Mixes planar audio: `src` holds one slice per source channel, and `dst` one slice per
    /// destination channel. `dst` is overwritten.
    ///
    /// # Panics
    ///
    /// Panics if the number of slices does not match the matrix, or if the slices do not all
    /// have the same length.
    pub fn apply(&self, src: &[&[f32]], dst: &mut [&mut [f32]]) {
        assert_eq!(src.len(), self.src_channels, "source channel count mismatch");
        assert_eq!(dst.len(), self.dst_channels, "destination channel count mismatch");

        let frames = src.first().map_or_else(|| dst.first().map_or(0, |d| d.len()), |s| s.len());
        assert!(src.iter().all(|s| s.len() == frames), "source length mismatch");
        assert!(dst.iter().all(|d| d.len() == frames), "destination length mismatch");

        for (row, out) in self.coeffs.chunks_exact(self.src_channels.max(1)).zip(dst.iter_mut()) {
            out.fill(0.0);

            for (&gain, input) in row.iter().zip(src) {
                // A zero gain (e.g., a discarded channel) contributes nothing.
                if gain != 0.0 {
                    for (o, &i) in out.iter_mut().zip(*input) {
                        *o += gain * i;
                    }
                }
            }
        }
    }

    /// Mixes interleaved audio. `dst` is overwritten.
    ///
    /// # Panics
    ///
    /// Panics if `src.len()` is not a multiple of the source channel count, or if `dst` is not
    /// the same number of frames.
    pub fn apply_interleaved(&self, src: &[f32], dst: &mut [f32]) {
        let (sc, dc) = (self.src_channels, self.dst_channels);

        assert!(sc > 0 && dc > 0, "matrix has no channels");
        assert!(src.len() % sc == 0, "source is not a whole number of frames");
        assert_eq!(src.len() / sc * dc, dst.len(), "destination length mismatch");

        for (frame, out) in src.chunks_exact(sc).zip(dst.chunks_exact_mut(dc)) {
            for (row, o) in self.coeffs.chunks_exact(sc).zip(out) {
                *o = row.iter().zip(frame).map(|(g, s)| g * s).sum();
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
// Table 2 of the Recommendation lists the coefficient -3 dB as the literal 0.7071.
#[allow(clippy::approx_constant)]
mod tests {
    use super::*;

    const EPS: f32 = 1e-6;

    fn pos(p: Position) -> Channels {
        Channels::Positioned(p)
    }

    fn five_one() -> Channels {
        pos(Position::FRONT_LEFT
            | Position::FRONT_RIGHT
            | Position::FRONT_CENTER
            | Position::LFE1
            | Position::REAR_LEFT
            | Position::REAR_RIGHT)
    }

    fn matrix(src: &Channels, dst: Position) -> DownmixMatrix {
        DownmixMatrix::bs775(src, &pos(dst), &Default::default()).unwrap()
    }

    fn assert_row(m: &DownmixMatrix, d: usize, expected: &[f32]) {
        assert_eq!(expected.len(), m.src_channels());
        for (s, &e) in expected.iter().enumerate() {
            let got = m.coefficient(d, s);
            // Table 2 lists four decimals.
            assert!((got - e).abs() < 5e-5, "dst {d} src {s}: {got} != {e}");
        }
    }

    // Source order for 5.1 (FL FR FC LFE1 RL RR): L, R, C, LFE, LS, RS.

    #[test]
    fn table2_stereo() {
        let m = matrix(&five_one(), Position::FRONT_LEFT | Position::FRONT_RIGHT);
        // L = 1.0000 L + 0.7071 C + 0.7071 LS; R = 1.0000 R + 0.7071 C + 0.7071 RS.
        assert_row(&m, 0, &[1.0, 0.0, 0.7071, 0.0, 0.7071, 0.0]);
        assert_row(&m, 1, &[0.0, 1.0, 0.7071, 0.0, 0.0, 0.7071]);
    }

    #[test]
    fn table2_mono() {
        let m = matrix(&five_one(), Position::FRONT_CENTER);
        // C = 0.7071 L + 0.7071 R + 1.0000 C + 0.5000 LS + 0.5000 RS.
        assert_row(&m, 0, &[0.7071, 0.7071, 1.0, 0.0, 0.5, 0.5]);
    }

    #[test]
    fn table2_other_formats() {
        let src = five_one();
        let fl_fr = Position::FRONT_LEFT | Position::FRONT_RIGHT;

        let m = matrix(&src, fl_fr | Position::FRONT_CENTER);
        assert_row(&m, 0, &[1.0, 0.0, 0.0, 0.0, 0.7071, 0.0]);
        assert_row(&m, 1, &[0.0, 1.0, 0.0, 0.0, 0.0, 0.7071]);
        assert_row(&m, 2, &[0.0, 0.0, 1.0, 0.0, 0.0, 0.0]);

        let m = matrix(&src, fl_fr | Position::REAR_CENTER);
        assert_row(&m, 0, &[1.0, 0.0, 0.7071, 0.0, 0.0, 0.0]);
        assert_row(&m, 1, &[0.0, 1.0, 0.7071, 0.0, 0.0, 0.0]);
        assert_row(&m, 2, &[0.0, 0.0, 0.0, 0.0, 0.7071, 0.7071]);

        let m = matrix(&src, fl_fr | Position::FRONT_CENTER | Position::REAR_CENTER);
        assert_row(&m, 0, &[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        assert_row(&m, 2, &[0.0, 0.0, 1.0, 0.0, 0.0, 0.0]);
        assert_row(&m, 3, &[0.0, 0.0, 0.0, 0.0, 0.7071, 0.7071]);

        let m = matrix(&src, fl_fr | Position::REAR_LEFT | Position::REAR_RIGHT);
        assert_row(&m, 0, &[1.0, 0.0, 0.7071, 0.0, 0.0, 0.0]);
        assert_row(&m, 1, &[0.0, 1.0, 0.7071, 0.0, 0.0, 0.0]);
        assert_row(&m, 2, &[0.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
        assert_row(&m, 3, &[0.0, 0.0, 0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn lfe_is_discarded() {
        let m = matrix(&five_one(), Position::FRONT_LEFT | Position::FRONT_RIGHT);
        // The LFE channel (source index 3) is in no output.
        assert_eq!(m.coefficient(0, 3), 0.0);
        assert_eq!(m.coefficient(1, 3), 0.0);
    }

    #[test]
    fn side_surrounds_are_equivalent_to_rear_surrounds() {
        let side = pos(Position::FRONT_LEFT
            | Position::FRONT_RIGHT
            | Position::FRONT_CENTER
            | Position::SIDE_LEFT
            | Position::SIDE_RIGHT);
        let rear = pos(Position::FRONT_LEFT
            | Position::FRONT_RIGHT
            | Position::FRONT_CENTER
            | Position::REAR_LEFT
            | Position::REAR_RIGHT);
        let dst = Position::FRONT_LEFT | Position::FRONT_RIGHT;
        assert_eq!(matrix(&side, dst), matrix(&rear, dst));
    }

    #[test]
    fn mono_surround_feeds_both_surround_roles() {
        // 3/1: L R C S (canonical order FL FR FC RC).
        let src = pos(Position::FRONT_LEFT
            | Position::FRONT_RIGHT
            | Position::FRONT_CENTER
            | Position::REAR_CENTER);

        // Stereo: S is fed to LS and RS, each mixed in at 0.7071.
        let m = matrix(&src, Position::FRONT_LEFT | Position::FRONT_RIGHT);
        assert_row(&m, 0, &[1.0, 0.0, 0.7071, 0.7071]);
        assert_row(&m, 1, &[0.0, 1.0, 0.7071, 0.7071]);

        // Mono: 0.5000 LS + 0.5000 RS = 1.0000 S.
        let m = matrix(&src, Position::FRONT_CENTER);
        assert_row(&m, 0, &[0.7071, 0.7071, 1.0, 1.0]);
    }

    #[test]
    fn subsets_treat_absent_channels_as_silent() {
        // Stereo to mono.
        let m = matrix(&pos(Position::FRONT_LEFT | Position::FRONT_RIGHT), Position::FRONT_CENTER);
        assert_row(&m, 0, &[0.7071, 0.7071]);

        // Stereo to stereo is the identity.
        let m = matrix(
            &pos(Position::FRONT_LEFT | Position::FRONT_RIGHT),
            Position::FRONT_LEFT | Position::FRONT_RIGHT,
        );
        assert_row(&m, 0, &[1.0, 0.0]);
        assert_row(&m, 1, &[0.0, 1.0]);
    }

    #[test]
    fn surround_level_replaces_the_surround_coefficient() {
        let dst = pos(Position::FRONT_LEFT | Position::FRONT_RIGHT);
        let opts = |surround_level| DownmixOptions { surround_level, normalize: false };

        let m = DownmixMatrix::bs775(&five_one(), &dst, &opts(SurroundLevel::Minus6Db)).unwrap();
        assert_row(&m, 0, &[1.0, 0.0, 0.7071, 0.0, 0.5, 0.0]);

        let m = DownmixMatrix::bs775(&five_one(), &dst, &opts(SurroundLevel::Off)).unwrap();
        assert_row(&m, 0, &[1.0, 0.0, 0.7071, 0.0, 0.0, 0.0]);

        // The mono equation has a fixed surround coefficient.
        let mono = pos(Position::FRONT_CENTER);
        let m = DownmixMatrix::bs775(&five_one(), &mono, &opts(SurroundLevel::Off)).unwrap();
        assert_row(&m, 0, &[0.7071, 0.7071, 1.0, 0.0, 0.5, 0.5]);
    }

    #[test]
    fn normalize_bounds_the_gain() {
        let dst = pos(Position::FRONT_LEFT | Position::FRONT_RIGHT);
        let opts = DownmixOptions { normalize: true, ..Default::default() };
        let m = DownmixMatrix::bs775(&five_one(), &dst, &opts).unwrap();

        let peak = (0..2)
            .map(|d| (0..6).map(|s| m.coefficient(d, s).abs()).sum::<f32>())
            .fold(0.0, f32::max);
        assert!((peak - 1.0).abs() < EPS);

        // The ratios of Table 2 are kept: L:C = 1 : 0.7071.
        assert!((m.coefficient(0, 2) / m.coefficient(0, 0) - M3DB).abs() < EPS);
    }

    #[test]
    fn rejects_unsupported_layouts() {
        let stereo = pos(Position::FRONT_LEFT | Position::FRONT_RIGHT);
        let opts = DownmixOptions::default();

        // Upmix.
        assert!(
            DownmixMatrix::bs775(&pos(Position::FRONT_CENTER), &stereo, &opts).is_err(),
            "upmix"
        );
        // 7.1: both a rear and a side pair.
        let seven_one = pos(Position::FRONT_LEFT
            | Position::FRONT_RIGHT
            | Position::FRONT_CENTER
            | Position::LFE1
            | Position::REAR_LEFT
            | Position::REAR_RIGHT
            | Position::SIDE_LEFT
            | Position::SIDE_RIGHT);
        assert!(DownmixMatrix::bs775(&seven_one, &stereo, &opts).is_err(), "7.1");
        // Channels BS.775 does not know.
        let top = pos(Position::FRONT_LEFT | Position::FRONT_RIGHT | Position::TOP_CENTER);
        assert!(DownmixMatrix::bs775(&top, &stereo, &opts).is_err(), "top");
        // Non-positioned channels and unknown destinations.
        assert!(DownmixMatrix::bs775(&Channels::Discrete(6), &stereo, &opts).is_err());
        assert!(DownmixMatrix::bs775(&five_one(), &Channels::Discrete(2), &opts).is_err());
        assert!(DownmixMatrix::bs775(&five_one(), &pos(Position::FRONT_LEFT), &opts).is_err());
    }

    #[test]
    fn from_coefficients_validates_length() {
        assert!(DownmixMatrix::from_coefficients(2, 1, vec![0.5, 0.5]).is_some());
        assert!(DownmixMatrix::from_coefficients(2, 1, vec![0.5]).is_none());
        assert!(DownmixMatrix::from_coefficients(usize::MAX, 2, vec![]).is_none());
    }

    #[test]
    fn apply_planar_and_interleaved_agree_with_the_equations() {
        let m = matrix(&five_one(), Position::FRONT_LEFT | Position::FRONT_RIGHT);

        // Two frames; channels L R C LFE LS RS.
        let planar: [&[f32]; 6] =
            [&[1.0, 0.0], &[0.5, 0.0], &[0.0, 1.0], &[9.0, 9.0], &[0.0, 0.5], &[0.25, 0.0]];

        let mut left = [0.0; 2];
        let mut right = [0.0; 2];
        m.apply(&planar, &mut [&mut left, &mut right]);

        let c = FRAC_1_SQRT_2;
        let expect_left = [1.0, c * 1.0 + c * 0.5];
        let expect_right = [0.5 + c * 0.25, c * 1.0];
        for i in 0..2 {
            assert!((left[i] - expect_left[i]).abs() < EPS, "L[{i}]");
            assert!((right[i] - expect_right[i]).abs() < EPS, "R[{i}]");
        }

        let mut interleaved = Vec::new();
        for f in 0..2 {
            interleaved.extend(planar.iter().map(|ch| ch[f]));
        }
        let mut out = [0.0; 4];
        m.apply_interleaved(&interleaved, &mut out);
        for i in 0..2 {
            assert!((out[2 * i] - left[i]).abs() < EPS);
            assert!((out[2 * i + 1] - right[i]).abs() < EPS);
        }
    }

    #[test]
    fn apply_mono_of_stereo_is_the_average_scaled_by_3db() {
        let m = matrix(&pos(Position::FRONT_LEFT | Position::FRONT_RIGHT), Position::FRONT_CENTER);
        let mut out = [0.0; 3];
        m.apply(&[&[1.0, -1.0, 0.5], &[1.0, 1.0, 0.0]], &mut [&mut out]);
        let c = FRAC_1_SQRT_2;
        for (o, e) in out.iter().zip([2.0 * c, 0.0, 0.5 * c]) {
            assert!((o - e).abs() < EPS);
        }
    }
}
