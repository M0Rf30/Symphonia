// Symphonia Musepack demuxer+decoder
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The 32-band polyphase synthesis filter (fast-MDCT form, after Byeong Gi Lee) and the
//! dithering pseudo-random generator.
//!
//! Ported from libmpcdec `synth_filter.c` (BSD-3-Clause), see `NOTICE`.
//!
//! Every arithmetic macro used by the reference (`MPC_SCALE_CONST`, `MPC_MULTIPLY_FRACT_CONST_*`,
//! `MPC_SHL`/`MPC_SHR`, ...) reduces to a plain floating-point multiply/add (or, for the shifts,
//! a no-op) in libmpcdec's non-fixed-point (float) build -- see `mpcdec_math.h`. This port targets
//! that float build exclusively, so every one of those macros below is simply `*`/`+`; none of the
//! shift amounts they carry in the C source apply here.

/// Number of interleaved history samples kept per channel (`MPC_V_MEM`).
pub const V_MEM: usize = 2304;
/// Total per-channel history buffer length (`MPC_V_MEM + 960`).
pub const V_BUF_LEN: usize = V_MEM + 960;

const fn d(x: i32) -> f32 {
    (x as f32) / 65536.0
}

/// `Di_opt`: the synthesis filter's coefficient table.
#[rustfmt::skip]
pub static DI_OPT: [[f32; 16]; 32] = [
    [d(0), d(-29), d(213), d(-459), d(2037), d(-5153), d(6574), d(-37489), d(75038), d(37489), d(6574), d(5153), d(2037), d(459), d(213), d(29)],
    [d(-1), d(-31), d(218), d(-519), d(2000), d(-5517), d(5959), d(-39336), d(74992), d(35640), d(7134), d(4788), d(2063), d(401), d(208), d(26)],
    [d(-1), d(-35), d(222), d(-581), d(1952), d(-5879), d(5288), d(-41176), d(74856), d(33791), d(7640), d(4425), d(2080), d(347), d(202), d(24)],
    [d(-1), d(-38), d(225), d(-645), d(1893), d(-6237), d(4561), d(-43006), d(74630), d(31947), d(8092), d(4063), d(2087), d(294), d(196), d(21)],
    [d(-1), d(-41), d(227), d(-711), d(1822), d(-6589), d(3776), d(-44821), d(74313), d(30112), d(8492), d(3705), d(2085), d(244), d(190), d(19)],
    [d(-1), d(-45), d(228), d(-779), d(1739), d(-6935), d(2935), d(-46617), d(73908), d(28289), d(8840), d(3351), d(2075), d(197), d(183), d(17)],
    [d(-1), d(-49), d(228), d(-848), d(1644), d(-7271), d(2037), d(-48390), d(73415), d(26482), d(9139), d(3004), d(2057), d(153), d(176), d(16)],
    [d(-2), d(-53), d(227), d(-919), d(1535), d(-7597), d(1082), d(-50137), d(72835), d(24694), d(9389), d(2663), d(2032), d(111), d(169), d(14)],
    [d(-2), d(-58), d(224), d(-991), d(1414), d(-7910), d(70), d(-51853), d(72169), d(22929), d(9592), d(2330), d(2001), d(72), d(161), d(13)],
    [d(-2), d(-63), d(221), d(-1064), d(1280), d(-8209), d(-998), d(-53534), d(71420), d(21189), d(9750), d(2006), d(1962), d(36), d(154), d(11)],
    [d(-2), d(-68), d(215), d(-1137), d(1131), d(-8491), d(-2122), d(-55178), d(70590), d(19478), d(9863), d(1692), d(1919), d(2), d(147), d(10)],
    [d(-3), d(-73), d(208), d(-1210), d(970), d(-8755), d(-3300), d(-56778), d(69679), d(17799), d(9935), d(1388), d(1870), d(-29), d(139), d(9)],
    [d(-3), d(-79), d(200), d(-1283), d(794), d(-8998), d(-4533), d(-58333), d(68692), d(16155), d(9966), d(1095), d(1817), d(-57), d(132), d(8)],
    [d(-4), d(-85), d(189), d(-1356), d(605), d(-9219), d(-5818), d(-59838), d(67629), d(14548), d(9959), d(814), d(1759), d(-83), d(125), d(7)],
    [d(-4), d(-91), d(177), d(-1428), d(402), d(-9416), d(-7154), d(-61289), d(66494), d(12980), d(9916), d(545), d(1698), d(-106), d(117), d(7)],
    [d(-5), d(-97), d(163), d(-1498), d(185), d(-9585), d(-8540), d(-62684), d(65290), d(11455), d(9838), d(288), d(1634), d(-127), d(111), d(6)],
    [d(-5), d(-104), d(146), d(-1567), d(-45), d(-9727), d(-9975), d(-64019), d(64019), d(9975), d(9727), d(45), d(1567), d(-146), d(104), d(5)],
    [d(-6), d(-111), d(127), d(-1634), d(-288), d(-9838), d(-11455), d(-65290), d(62684), d(8540), d(9585), d(-185), d(1498), d(-163), d(97), d(5)],
    [d(-7), d(-117), d(106), d(-1698), d(-545), d(-9916), d(-12980), d(-66494), d(61289), d(7154), d(9416), d(-402), d(1428), d(-177), d(91), d(4)],
    [d(-7), d(-125), d(83), d(-1759), d(-814), d(-9959), d(-14548), d(-67629), d(59838), d(5818), d(9219), d(-605), d(1356), d(-189), d(85), d(4)],
    [d(-8), d(-132), d(57), d(-1817), d(-1095), d(-9966), d(-16155), d(-68692), d(58333), d(4533), d(8998), d(-794), d(1283), d(-200), d(79), d(3)],
    [d(-9), d(-139), d(29), d(-1870), d(-1388), d(-9935), d(-17799), d(-69679), d(56778), d(3300), d(8755), d(-970), d(1210), d(-208), d(73), d(3)],
    [d(-10), d(-147), d(-2), d(-1919), d(-1692), d(-9863), d(-19478), d(-70590), d(55178), d(2122), d(8491), d(-1131), d(1137), d(-215), d(68), d(2)],
    [d(-11), d(-154), d(-36), d(-1962), d(-2006), d(-9750), d(-21189), d(-71420), d(53534), d(998), d(8209), d(-1280), d(1064), d(-221), d(63), d(2)],
    [d(-13), d(-161), d(-72), d(-2001), d(-2330), d(-9592), d(-22929), d(-72169), d(51853), d(-70), d(7910), d(-1414), d(991), d(-224), d(58), d(2)],
    [d(-14), d(-169), d(-111), d(-2032), d(-2663), d(-9389), d(-24694), d(-72835), d(50137), d(-1082), d(7597), d(-1535), d(919), d(-227), d(53), d(2)],
    [d(-16), d(-176), d(-153), d(-2057), d(-3004), d(-9139), d(-26482), d(-73415), d(48390), d(-2037), d(7271), d(-1644), d(848), d(-228), d(49), d(1)],
    [d(-17), d(-183), d(-197), d(-2075), d(-3351), d(-8840), d(-28289), d(-73908), d(46617), d(-2935), d(6935), d(-1739), d(779), d(-228), d(45), d(1)],
    [d(-19), d(-190), d(-244), d(-2085), d(-3705), d(-8492), d(-30112), d(-74313), d(44821), d(-3776), d(6589), d(-1822), d(711), d(-227), d(41), d(1)],
    [d(-21), d(-196), d(-294), d(-2087), d(-4063), d(-8092), d(-31947), d(-74630), d(43006), d(-4561), d(6237), d(-1893), d(645), d(-225), d(38), d(1)],
    [d(-24), d(-202), d(-347), d(-2080), d(-4425), d(-7640), d(-33791), d(-74856), d(41176), d(-5288), d(5879), d(-1952), d(581), d(-222), d(35), d(1)],
    [d(-26), d(-208), d(-401), d(-2063), d(-4788), d(-7134), d(-35640), d(-74992), d(39336), d(-5959), d(5517), d(-2000), d(519), d(-218), d(31), d(1)],
];

/// Ported from libmpcdec `synth_filter.c` (`mpc_compute_new_V`).
///
/// Fills the 64-element window `p_v` from the 32 subband samples `p_sample` using the
/// fast-MDCT-based butterfly network (ISO 11172-3 p.39 / Byeong Gi Lee's algorithm).
#[allow(clippy::many_single_char_names)]
fn compute_new_v(p_sample: &[f32; 32], p_v: &mut [f32]) {
    debug_assert!(p_v.len() >= 64);
    let p = p_sample;

    let a00 = p[0] + p[31];
    let a01 = p[1] + p[30];
    let a02 = p[2] + p[29];
    let a03 = p[3] + p[28];
    let a04 = p[4] + p[27];
    let a05 = p[5] + p[26];
    let a06 = p[6] + p[25];
    let a07 = p[7] + p[24];
    let a08 = p[8] + p[23];
    let a09 = p[9] + p[22];
    let a10 = p[10] + p[21];
    let a11 = p[11] + p[20];
    let a12 = p[12] + p[19];
    let a13 = p[13] + p[18];
    let a14 = p[14] + p[17];
    let a15 = p[15] + p[16];

    let b00 = a00 + a15;
    let b01 = a01 + a14;
    let b02 = a02 + a13;
    let b03 = a03 + a12;
    let b04 = a04 + a11;
    let b05 = a05 + a10;
    let b06 = a06 + a09;
    let b07 = a07 + a08;
    let b08 = (a00 - a15) * 0.5024192929;
    let b09 = (a01 - a14) * 0.5224986076;
    let b10 = (a02 - a13) * 0.5669440627;
    let b11 = (a03 - a12) * 0.6468217969;
    let b12 = (a04 - a11) * 0.7881546021;
    let b13 = (a05 - a10) * 1.0606776476;
    let b14 = (a06 - a09) * 1.7224471569;
    let b15 = (a07 - a08) * 5.1011486053;

    let a00 = b00 + b07;
    let a01 = b01 + b06;
    let a02 = b02 + b05;
    let a03 = b03 + b04;
    let a04 = (b00 - b07) * 0.5097956061;
    let a05 = (b01 - b06) * 0.6013448834;
    let a06 = (b02 - b05) * 0.8999761939;
    let a07 = (b03 - b04) * 2.5629155636;
    let a08 = b08 + b15;
    let a09 = b09 + b14;
    let a10 = b10 + b13;
    let a11 = b11 + b12;
    let a12 = (b08 - b15) * 0.5097956061;
    let a13 = (b09 - b14) * 0.6013448834;
    let a14 = (b10 - b13) * 0.8999761939;
    let a15 = (b11 - b12) * 2.5629155636;

    let b00 = a00 + a03;
    let b01 = a01 + a02;
    let b02 = (a00 - a03) * 0.5411961079;
    let b03 = (a01 - a02) * 1.3065630198;
    let b04 = a04 + a07;
    let b05 = a05 + a06;
    let b06 = (a04 - a07) * 0.5411961079;
    let b07 = (a05 - a06) * 1.3065630198;
    let b08 = a08 + a11;
    let b09 = a09 + a10;
    let b10 = (a08 - a11) * 0.5411961079;
    let b11 = (a09 - a10) * 1.3065630198;
    let b12 = a12 + a15;
    let b13 = a13 + a14;
    let b14 = (a12 - a15) * 0.5411961079;
    let b15 = (a13 - a14) * 1.3065630198;

    let a00 = b00 + b01;
    let a01 = (b00 - b01) * 0.7071067691;
    let a02 = b02 + b03;
    let a03 = (b02 - b03) * 0.7071067691;
    let a04 = b04 + b05;
    let a05 = (b04 - b05) * 0.7071067691;
    let a06 = b06 + b07;
    let a07 = (b06 - b07) * 0.7071067691;
    let a08 = b08 + b09;
    let a09 = (b08 - b09) * 0.7071067691;
    let a10 = b10 + b11;
    let a11 = (b10 - b11) * 0.7071067691;
    let a12 = b12 + b13;
    let a13 = (b12 - b13) * 0.7071067691;
    let a14 = b14 + b15;
    let a15 = (b14 - b15) * 0.7071067691;

    p_v[48] = -a00;
    p_v[0] = a01;
    p_v[8] = a03;
    p_v[40] = -a02 - p_v[8];
    p_v[12] = a07;
    p_v[4] = a05 + p_v[12];
    p_v[36] = -(p_v[4] + a06);
    p_v[44] = -a04 - a06 - a07;
    p_v[14] = a15;
    p_v[10] = a11 + p_v[14];
    p_v[6] = p_v[10] + a13;
    p_v[2] = a09 + a13 + a15;
    p_v[34] = -p_v[2] - a14;
    p_v[38] = p_v[34] + a09 - a10 - a11;
    let mut tmp = -(a12 + a14 + a15);
    p_v[46] = tmp - a08;
    p_v[42] = tmp - a10 - a11;

    let a00 = (p[0] - p[31]) * 0.5006030202;
    let a01 = (p[1] - p[30]) * 0.5054709315;
    let a02 = (p[2] - p[29]) * 0.5154473186;
    let a03 = (p[3] - p[28]) * 0.5310425758;
    let a04 = (p[4] - p[27]) * 0.5531039238;
    let a05 = (p[5] - p[26]) * 0.5829349756;
    let a06 = (p[6] - p[25]) * 0.6225041151;
    let a07 = (p[7] - p[24]) * 0.6748083234;
    let a08 = (p[8] - p[23]) * 0.7445362806;
    let a09 = (p[9] - p[22]) * 0.8393496275;
    let a10 = (p[10] - p[21]) * 0.9725682139;
    let a11 = (p[11] - p[20]) * 1.1694399118;
    let a12 = (p[12] - p[19]) * 1.4841645956;
    let a13 = (p[13] - p[18]) * 2.0577809811;
    let a14 = (p[14] - p[17]) * 3.4076085091;
    let a15 = (p[15] - p[16]) * 10.1900081635;

    let b00 = a00 + a15;
    let b01 = a01 + a14;
    let b02 = a02 + a13;
    let b03 = a03 + a12;
    let b04 = a04 + a11;
    let b05 = a05 + a10;
    let b06 = a06 + a09;
    let b07 = a07 + a08;
    let b08 = (a00 - a15) * 0.5024192929;
    let b09 = (a01 - a14) * 0.5224986076;
    let b10 = (a02 - a13) * 0.5669440627;
    let b11 = (a03 - a12) * 0.6468217969;
    let b12 = (a04 - a11) * 0.7881546021;
    let b13 = (a05 - a10) * 1.0606776476;
    let b14 = (a06 - a09) * 1.7224471569;
    let b15 = (a07 - a08) * 5.1011486053;

    let a00 = b00 + b07;
    let a01 = b01 + b06;
    let a02 = b02 + b05;
    let a03 = b03 + b04;
    let a04 = (b00 - b07) * 0.5097956061;
    let a05 = (b01 - b06) * 0.6013448834;
    let a06 = (b02 - b05) * 0.8999761939;
    let a07 = (b03 - b04) * 2.5629155636;
    let a08 = b08 + b15;
    let a09 = b09 + b14;
    let a10 = b10 + b13;
    let a11 = b11 + b12;
    let a12 = (b08 - b15) * 0.5097956061;
    let a13 = (b09 - b14) * 0.6013448834;
    let a14 = (b10 - b13) * 0.8999761939;
    let a15 = (b11 - b12) * 2.5629155636;

    let b00 = a00 + a03;
    let b01 = a01 + a02;
    let b02 = (a00 - a03) * 0.5411961079;
    let b03 = (a01 - a02) * 1.3065630198;
    let b04 = a04 + a07;
    let b05 = a05 + a06;
    let b06 = (a04 - a07) * 0.5411961079;
    let b07 = (a05 - a06) * 1.3065630198;
    let b08 = a08 + a11;
    let b09 = a09 + a10;
    let b10 = (a08 - a11) * 0.5411961079;
    let b11 = (a09 - a10) * 1.3065630198;
    let b12 = a12 + a15;
    let b13 = a13 + a14;
    let b14 = (a12 - a15) * 0.5411961079;
    let b15 = (a13 - a14) * 1.3065630198;

    let a00 = b00 + b01;
    let a01 = (b00 - b01) * 0.7071067691;
    let a02 = b02 + b03;
    let a03 = (b02 - b03) * 0.7071067691;
    let a04 = b04 + b05;
    let a05 = (b04 - b05) * 0.7071067691;
    let a06 = b06 + b07;
    let a07 = (b06 - b07) * 0.7071067691;
    let a08 = b08 + b09;
    let a09 = (b08 - b09) * 0.7071067691;
    let a10 = b10 + b11;
    let a11 = (b10 - b11) * 0.7071067691;
    let a12 = b12 + b13;
    let a13 = (b12 - b13) * 0.7071067691;
    let a14 = b14 + b15;
    let a15 = (b14 - b15) * 0.7071067691;

    p_v[15] = a15;
    p_v[13] = a07 + p_v[15];
    p_v[11] = p_v[13] + a11;
    p_v[5] = p_v[11] + a05 + a13;
    p_v[9] = a03 + a11 + a15;
    p_v[7] = p_v[9] + a13;
    p_v[1] = a01 + a09 + a13 + a15;
    p_v[33] = -p_v[1] - a14;
    p_v[3] = a05 + a07 + a09 + a13 + a15;
    p_v[35] = -p_v[3] - a06 - a14;
    tmp = -(a10 + a11 + a13 + a14 + a15);
    p_v[37] = tmp - a05 - a06 - a07;
    p_v[39] = tmp - a02 - a03;
    tmp += a13 - a12;
    p_v[41] = tmp - a02 - a03;
    p_v[43] = tmp - a04 - a06 - a07;
    tmp = -(a08 + a12 + a14 + a15);
    p_v[47] = tmp - a00;
    p_v[45] = tmp - a04 - a06 - a07;

    for i in 0..16 {
        p_v[32 - i] = -p_v[i];
    }
    for i in 0..15 {
        p_v[63 - i] = p_v[33 + i];
    }
}

/// Ported from libmpcdec `synth_filter.c` (`mpc_synthese_filter_float_internal` +
/// `mpc_synthese_filter_float_scalar`), specialized to run on a single channel's history buffer.
///
/// `history` must have length [`V_BUF_LEN`]. `y` holds the 36x32 subband samples for this frame
/// (`p_dec->Y_L`/`Y_R`). Writes `36 * 32` interleaved samples into `out`, starting at
/// `out[channel_offset]` with a stride of `channels`.
pub fn synth_channel(
    history: &mut [f32],
    y: &[[f32; 32]; 36],
    out: &mut [f32],
    channels: usize,
    channel_offset: usize,
) {
    debug_assert!(history.len() >= V_BUF_LEN);

    // `memmove(&V[MPC_V_MEM], V, 960 * sizeof *V)`: carry the tail of the previous call's fresh
    // history forward so this call's backward-walking windows can read it as "old" data.
    history.copy_within(0..960, V_MEM);

    const OFFSETS: [usize; 16] =
        [0, 96, 128, 224, 256, 352, 384, 480, 512, 608, 640, 736, 768, 864, 896, 992];

    let mut pv_base = V_MEM;
    for (n, samples) in y.iter().enumerate() {
        pv_base -= 64;
        compute_new_v(samples, &mut history[pv_base..pv_base + 64]);

        for k in 0..32 {
            let idx = pv_base + k;
            let coeffs = &DI_OPT[k];
            let mut acc = 0.0f32;
            for (j, &off) in OFFSETS.iter().enumerate() {
                acc += history[idx + off] * coeffs[j];
            }
            out[(n * 32 + k) * channels + channel_offset] = acc;
        }
    }
    debug_assert_eq!(pv_base, 0);
}

/// Ported from libmpcdec `synth_filter.c` (`mpc_random_int`).
///
/// A dual-polycounter PRNG used to synthesize "noise" subband samples for `Res == -1`.
pub fn random_int(r1: &mut u32, r2: &mut u32) -> u32 {
    #[rustfmt::skip]
    const PARITY: [u8; 256] = [
        0,1,1,0,1,0,0,1,1,0,0,1,0,1,1,0,1,0,0,1,0,1,1,0,0,1,1,0,1,0,0,1,
        1,0,0,1,0,1,1,0,0,1,1,0,1,0,0,1,0,1,1,0,1,0,0,1,1,0,0,1,0,1,1,0,
        1,0,0,1,0,1,1,0,0,1,1,0,1,0,0,1,0,1,1,0,1,0,0,1,1,0,0,1,0,1,1,0,
        0,1,1,0,1,0,0,1,1,0,0,1,0,1,1,0,1,0,0,1,0,1,1,0,0,1,1,0,1,0,0,1,
        1,0,0,1,0,1,1,0,0,1,1,0,1,0,0,1,0,1,1,0,1,0,0,1,1,0,0,1,0,1,1,0,
        0,1,1,0,1,0,0,1,1,0,0,1,0,1,1,0,1,0,0,1,0,1,1,0,0,1,1,0,1,0,0,1,
        0,1,1,0,1,0,0,1,1,0,0,1,0,1,1,0,1,0,0,1,0,1,1,0,0,1,1,0,1,0,0,1,
        1,0,0,1,0,1,1,0,0,1,1,0,1,0,0,1,0,1,1,0,1,0,0,1,1,0,0,1,0,1,1,0,
    ];

    let t3 = *r1;
    let t1 = *r1;
    let t4 = *r2;
    let t2 = *r2;
    let t1 = t1 & 0xF5;
    let t2 = t2 >> 25;
    let t1 = u32::from(PARITY[t1 as usize]);
    let t2 = t2 & 0x63;
    let t1 = t1 << 31;
    let t2 = u32::from(PARITY[t2 as usize]);

    *r1 = (t3 >> 1) | t1;
    *r2 = (t4 << 1) | t2;
    *r1 ^ *r2
}
