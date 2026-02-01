// SILK Decoder for Opus
// Ported from reference implementation
// SPDX-License-Identifier: MPL-2.0

use crate::entdec::RangeDecoder;
use std::cmp;

// Type alias for compatibility
type EntropyCoder<'a> = RangeDecoder<'a>;

// Constants from define.h
const MAX_FRAMES_PER_PACKET: usize = 3;
const MAX_NB_SUBFR: usize = 4;
const MAX_LPC_ORDER: usize = 16;
const MAX_FRAME_LENGTH: usize = 320; // 20ms at 16kHz
const MAX_SUB_FRAME_LENGTH: usize = 80; // 5ms at 16kHz
const LTP_ORDER: usize = 5;
const SHELL_CODEC_FRAME_LENGTH: usize = 16;
const LOG2_SHELL_CODEC_FRAME_LENGTH: usize = 4;
const N_RATE_LEVELS: usize = 10;
const SILK_MAX_PULSES: usize = 16;
const MAX_NB_SHELL_BLOCKS: usize = MAX_FRAME_LENGTH / SHELL_CODEC_FRAME_LENGTH;
const NLSF_QUANT_MAX_AMPLITUDE: i32 = 4;
const NLSF_QUANT_LEVEL_ADJ_Q10: i32 = 102; // SILK_FIX_CONST(0.1, 10) = 102
const QUANT_LEVEL_ADJUST_Q10: i32 = 80;
const BWE_AFTER_LOSS_Q16: i32 = 63570;
const LSF_COS_TAB_SZ: usize = 128;
const MAX_LPC_STABILIZE_ITERATIONS: usize = 16;
const MAX_PREDICTION_POWER_GAIN: f32 = 1e4;
const MAX_NLSF_STABILIZE_LOOPS: usize = 20;

// Signal types
const TYPE_NO_VOICE_ACTIVITY: i8 = 0;
const TYPE_UNVOICED: i8 = 1;
const TYPE_VOICED: i8 = 2;

// Conditional coding
const CODE_INDEPENDENTLY: i32 = 0;
const CODE_CONDITIONALLY: i32 = 2;

// Quantization offsets (Q10 format)
const OFFSET_VL_Q10: i32 = 32;
const OFFSET_VH_Q10: i32 = 100;
const OFFSET_UVL_Q10: i32 = 100;
const OFFSET_UVH_Q10: i32 = 240;

// NLSF Codebook structure
#[derive(Clone, Copy)]
pub struct NlsfCodebook {
    pub n_vectors: i16,
    pub order: i16,
    pub quant_step_size_q16: i32,
    pub inv_quant_step_size_q6: i16,
    pub cb1_nlsf_q8: &'static [u8],
    pub cb1_wght_q9: &'static [i16],
    pub cb1_icdf: &'static [u8],
    pub pred_q8: &'static [u8],
    pub ec_sel: &'static [u8],
    pub ec_icdf: &'static [u8],
    pub delta_min_q15: &'static [i16],
}

// ============================================================================
// NLSF Codebook Tables - Narrowband/Mediumband (8/12 kHz, order 10)
// ============================================================================

static SILK_NLSF_CB1_NB_MB_Q8: [u8; 320] = [
    12,     35,     60,     83,    108,    132,    157,    180,   206,    228,
    15,     32,     55,     77,    101,    125,    151,    175,   201,    225,
    19,     42,     66,     89,    114,    137,    162,    184,   209,    230,
    12,     25,     50,     72,     97,    120,    147,    172,   200,    223,
    26,     44,     69,     90,    114,    135,    159,    180,   205,    225,
    13,     22,     53,     80,    106,    130,    156,    180,   205,    228,
    15,     25,     44,     64,     90,    115,    142,    168,   196,    222,
    19,     24,     62,     82,    100,    120,    145,    168,   190,    214,
    22,     31,     50,     79,    103,    120,    151,    170,   203,    227,
    21,     29,     45,     65,    106,    124,    150,    171,   196,    224,
    30,     49,     75,     97,    121,    142,    165,    186,   209,    229,
    19,     25,     52,     70,     93,    116,    143,    166,   192,    219,
    26,     34,     62,     75,     97,    118,    145,    167,   194,    217,
    25,     33,     56,     70,     91,    113,    143,    165,   196,    223,
    21,     34,     51,     72,     97,    117,    145,    171,   196,    222,
    20,     29,     50,     67,     90,    117,    144,    168,   197,    221,
    22,     31,     48,     66,     95,    117,    146,    168,   196,    222,
    24,     33,     51,     77,    116,    134,    158,    180,   200,    224,
    21,     28,     70,     87,    106,    124,    149,    170,   194,    217,
    26,     33,     53,     64,     83,    117,    152,    173,   204,    225,
    27,     34,     65,     95,    108,    129,    155,    174,   210,    225,
    20,     26,     72,     99,    113,    131,    154,    176,   200,    219,
    34,     43,     61,     78,     93,    114,    155,    177,   205,    229,
    23,     29,     54,     97,    124,    138,    163,    179,   209,    229,
    30,     38,     56,     89,    118,    129,    158,    178,   200,    231,
    21,     29,     49,     63,     85,    111,    142,    163,   193,    222,
    27,     48,     77,    103,    133,    158,    179,    196,   215,    232,
    29,     47,     74,     99,    124,    151,    176,    198,   220,    237,
    33,     42,     61,     76,     93,    121,    155,    174,   207,    225,
    29,     53,     87,    112,    136,    154,    170,    188,   208,    227,
    24,     30,     52,     84,    131,    150,    166,    186,   203,    229,
    37,     48,     64,     84,    104,    118,    156,    177,   201,    230,
];

static SILK_NLSF_CB1_WGHT_Q9_NB_MB: [i16; 320] = [
    2897, 2314, 2314, 2314, 2287, 2287, 2314, 2300, 2327, 2287,
    2888, 2580, 2394, 2367, 2314, 2274, 2274, 2274, 2274, 2194,
    2487, 2340, 2340, 2314, 2314, 2314, 2340, 2340, 2367, 2354,
    3216, 2766, 2340, 2340, 2314, 2274, 2221, 2207, 2261, 2194,
    2460, 2474, 2367, 2394, 2394, 2394, 2394, 2367, 2407, 2314,
    3479, 3056, 2127, 2207, 2274, 2274, 2274, 2287, 2314, 2261,
    3282, 3141, 2580, 2394, 2247, 2221, 2207, 2194, 2194, 2114,
    4096, 3845, 2221, 2620, 2620, 2407, 2314, 2394, 2367, 2074,
    3178, 3244, 2367, 2221, 2553, 2434, 2340, 2314, 2167, 2221,
    3338, 3488, 2726, 2194, 2261, 2460, 2354, 2367, 2207, 2101,
    2354, 2420, 2327, 2367, 2394, 2420, 2420, 2420, 2460, 2367,
    3779, 3629, 2434, 2527, 2367, 2274, 2274, 2300, 2207, 2048,
    3254, 3225, 2713, 2846, 2447, 2327, 2300, 2300, 2274, 2127,
    3263, 3300, 2753, 2806, 2447, 2261, 2261, 2247, 2127, 2101,
    2873, 2981, 2633, 2367, 2407, 2354, 2194, 2247, 2247, 2114,
    3225, 3197, 2633, 2580, 2274, 2181, 2247, 2221, 2221, 2141,
    3178, 3310, 2740, 2407, 2274, 2274, 2274, 2287, 2194, 2114,
    3141, 3272, 2460, 2061, 2287, 2500, 2367, 2487, 2434, 2181,
    3507, 3282, 2314, 2700, 2647, 2474, 2367, 2394, 2340, 2127,
    3423, 3535, 3038, 3056, 2300, 1950, 2221, 2274, 2274, 2274,
    3404, 3366, 2087, 2687, 2873, 2354, 2420, 2274, 2474, 2540,
    3760, 3488, 1950, 2660, 2897, 2527, 2394, 2367, 2460, 2261,
    3028, 3272, 2740, 2888, 2740, 2154, 2127, 2287, 2234, 2247,
    3695, 3657, 2025, 1969, 2660, 2700, 2580, 2500, 2327, 2367,
    3207, 3413, 2354, 2074, 2888, 2888, 2340, 2487, 2247, 2167,
    3338, 3366, 2846, 2780, 2327, 2154, 2274, 2287, 2114, 2061,
    2327, 2300, 2181, 2167, 2181, 2367, 2633, 2700, 2700, 2553,
    2407, 2434, 2221, 2261, 2221, 2221, 2340, 2420, 2607, 2700,
    3038, 3244, 2806, 2888, 2474, 2074, 2300, 2314, 2354, 2380,
    2221, 2154, 2127, 2287, 2500, 2793, 2793, 2620, 2580, 2367,
    3676, 3713, 2234, 1838, 2181, 2753, 2726, 2673, 2513, 2207,
    2793, 3160, 2726, 2553, 2846, 2513, 2181, 2394, 2221, 2181,
];

static SILK_NLSF_CB1_ICDF_NB_MB: [u8; 64] = [
    212,    178,    148,    129,    108,     96,     85,     82,
     79,     77,     61,     59,     57,     56,     51,     49,
     48,     45,     42,     41,     40,     38,     36,     34,
     31,     30,     21,     12,     10,      3,      1,      0,
    255,    245,    244,    236,    233,    225,    217,    203,
    190,    176,    175,    161,    149,    136,    125,    114,
    102,     91,     81,     71,     60,     52,     43,     35,
     28,     20,     19,     18,     12,     11,      5,      0,
];

static SILK_NLSF_CB2_SELECT_NB_MB: [u8; 160] = [
     16,      0,      0,      0,      0,     99,     66,     36,
     36,     34,     36,     34,     34,     34,     34,     83,
     69,     36,     52,     34,    116,    102,     70,     68,
     68,    176,    102,     68,     68,     34,     65,     85,
     68,     84,     36,    116,    141,    152,    139,    170,
    132,    187,    184,    216,    137,    132,    249,    168,
    185,    139,    104,    102,    100,     68,     68,    178,
    218,    185,    185,    170,    244,    216,    187,    187,
    170,    244,    187,    187,    219,    138,    103,    155,
    184,    185,    137,    116,    183,    155,    152,    136,
    132,    217,    184,    184,    170,    164,    217,    171,
    155,    139,    244,    169,    184,    185,    170,    164,
    216,    223,    218,    138,    214,    143,    188,    218,
    168,    244,    141,    136,    155,    170,    168,    138,
    220,    219,    139,    164,    219,    202,    216,    137,
    168,    186,    246,    185,    139,    116,    185,    219,
    185,    138,    100,    100,    134,    100,    102,     34,
     68,     68,    100,     68,    168,    203,    221,    218,
    168,    167,    154,    136,    104,     70,    164,    246,
    171,    137,    139,    137,    155,    218,    219,    139,
];

static SILK_NLSF_CB2_ICDF_NB_MB: [u8; 72] = [
    255,    254,    253,    238,     14,      3,      2,      1,      0,
    255,    254,    252,    218,     35,      3,      2,      1,      0,
    255,    254,    250,    208,     59,      4,      2,      1,      0,
    255,    254,    246,    194,     71,     10,      2,      1,      0,
    255,    252,    236,    183,     82,      8,      2,      1,      0,
    255,    252,    235,    180,     90,     17,      2,      1,      0,
    255,    248,    224,    171,     97,     30,      4,      1,      0,
    255,    254,    236,    173,     95,     37,      7,      1,      0,
];

static SILK_NLSF_PRED_NB_MB_Q8: [u8; 18] = [
    179,    138,    140,    148,    151,    149,    153,    151,    163,
    116,     67,     82,     59,     92,     72,    100,     89,     92,
];

static SILK_NLSF_DELTA_MIN_NB_MB_Q15: [i16; 11] = [
    250,      3,      6,      3,      3,      3,      4,      3,      3,      3,    461,
];

// ============================================================================
// NLSF Codebook Tables - Wideband (16 kHz, order 16)
// ============================================================================

static SILK_NLSF_CB1_WB_Q8: [u8; 512] = [
      7,     23,     38,     54,     69,     85,    100,    116,   131,    147,    162,    178,    193,    208,    223,    239,
     13,     25,     41,     55,     69,     83,     98,    112,   127,    142,    157,    171,    187,    203,    220,    236,
     15,     21,     34,     51,     61,     78,     92,    106,   126,    136,    152,    167,    185,    205,    225,    240,
     10,     21,     36,     50,     63,     79,     95,    110,   126,    141,    157,    173,    189,    205,    221,    237,
     17,     20,     37,     51,     59,     78,     89,    107,   123,    134,    150,    164,    184,    205,    224,    240,
     10,     15,     32,     51,     67,     81,     96,    112,   129,    142,    158,    173,    189,    204,    220,    236,
      8,     21,     37,     51,     65,     79,     98,    113,   126,    138,    155,    168,    179,    192,    209,    218,
     12,     15,     34,     55,     63,     78,     87,    108,   118,    131,    148,    167,    185,    203,    219,    236,
     16,     19,     32,     36,     56,     79,     91,    108,   118,    136,    154,    171,    186,    204,    220,    237,
     11,     28,     43,     58,     74,     89,    105,    120,   135,    150,    165,    180,    196,    211,    226,    241,
      6,     16,     33,     46,     60,     75,     92,    107,   123,    137,    156,    169,    185,    199,    214,    225,
     11,     19,     30,     44,     57,     74,     89,    105,   121,    135,    152,    169,    186,    202,    218,    234,
     12,     19,     29,     46,     57,     71,     88,    100,   120,    132,    148,    165,    182,    199,    216,    233,
     17,     23,     35,     46,     56,     77,     92,    106,   123,    134,    152,    167,    185,    204,    222,    237,
     14,     17,     45,     53,     63,     75,     89,    107,   115,    132,    151,    171,    188,    206,    221,    240,
      9,     16,     29,     40,     56,     71,     88,    103,   119,    137,    154,    171,    189,    205,    222,    237,
     16,     19,     36,     48,     57,     76,     87,    105,   118,    132,    150,    167,    185,    202,    218,    236,
     12,     17,     29,     54,     71,     81,     94,    104,   126,    136,    149,    164,    182,    201,    221,    237,
     15,     28,     47,     62,     79,     97,    115,    129,   142,    155,    168,    180,    194,    208,    223,    238,
      8,     14,     30,     45,     62,     78,     94,    111,   127,    143,    159,    175,    192,    207,    223,    239,
     17,     30,     49,     62,     79,     92,    107,    119,   132,    145,    160,    174,    190,    204,    220,    235,
     14,     19,     36,     45,     61,     76,     91,    108,   121,    138,    154,    172,    189,    205,    222,    238,
     12,     18,     31,     45,     60,     76,     91,    107,   123,    138,    154,    171,    187,    204,    221,    236,
     13,     17,     31,     43,     53,     70,     83,    103,   114,    131,    149,    167,    185,    203,    220,    237,
     17,     22,     35,     42,     58,     78,     93,    110,   125,    139,    155,    170,    188,    206,    224,    240,
      8,     15,     34,     50,     67,     83,     99,    115,   131,    146,    162,    178,    193,    209,    224,    239,
     13,     16,     41,     66,     73,     86,     95,    111,   128,    137,    150,    163,    183,    206,    225,    241,
     17,     25,     37,     52,     63,     75,     92,    102,   119,    132,    144,    160,    175,    191,    212,    231,
     19,     31,     49,     65,     83,    100,    117,    133,   147,    161,    174,    187,    200,    213,    227,    242,
     18,     31,     52,     68,     88,    103,    117,    126,   138,    149,    163,    177,    192,    207,    223,    239,
     16,     29,     47,     61,     76,     90,    106,    119,   133,    147,    161,    176,    193,    209,    224,    240,
     15,     21,     35,     50,     61,     73,     86,     97,   110,    119,    129,    141,    175,    198,    218,    237,
];

static SILK_NLSF_CB1_WGHT_Q9_WB: [i16; 512] = [
    3657, 2925, 2925, 2925, 2925, 2925, 2925, 2925, 2925, 2925, 2925, 2925, 2963, 2963, 2925, 2846,
    3216, 3085, 2972, 3056, 3056, 3010, 3010, 3010, 2963, 2963, 3010, 2972, 2888, 2846, 2846, 2726,
    3920, 4014, 2981, 3207, 3207, 2934, 3056, 2846, 3122, 3244, 2925, 2846, 2620, 2553, 2780, 2925,
    3516, 3197, 3010, 3103, 3019, 2888, 2925, 2925, 2925, 2925, 2888, 2888, 2888, 2888, 2888, 2753,
    5054, 5054, 2934, 3573, 3385, 3056, 3085, 2793, 3160, 3160, 2972, 2846, 2513, 2540, 2753, 2888,
    4428, 4149, 2700, 2753, 2972, 3010, 2925, 2846, 2981, 3019, 2925, 2925, 2925, 2925, 2888, 2726,
    3620, 3019, 2972, 3056, 3056, 2873, 2806, 3056, 3216, 3047, 2981, 3291, 3291, 2981, 3310, 2991,
    5227, 5014, 2540, 3338, 3526, 3385, 3197, 3094, 3376, 2981, 2700, 2647, 2687, 2793, 2846, 2673,
    5081, 5174, 4615, 4428, 2460, 2897, 3047, 3207, 3169, 2687, 2740, 2888, 2846, 2793, 2846, 2700,
    3122, 2888, 2963, 2925, 2925, 2925, 2925, 2963, 2963, 2963, 2963, 2925, 2925, 2963, 2963, 2963,
    4202, 3207, 2981, 3103, 3010, 2888, 2888, 2925, 2972, 2873, 2916, 3019, 2972, 3010, 3197, 2873,
    3760, 3760, 3244, 3103, 2981, 2888, 2925, 2888, 2972, 2934, 2793, 2793, 2846, 2888, 2888, 2660,
    3854, 4014, 3207, 3122, 3244, 2934, 3047, 2963, 2963, 3085, 2846, 2793, 2793, 2793, 2793, 2580,
    3845, 4080, 3357, 3516, 3094, 2740, 3010, 2934, 3122, 3085, 2846, 2846, 2647, 2647, 2846, 2806,
    5147, 4894, 3225, 3845, 3441, 3169, 2897, 3413, 3451, 2700, 2580, 2673, 2740, 2846, 2806, 2753,
    4109, 3789, 3291, 3160, 2925, 2888, 2888, 2925, 2793, 2740, 2793, 2740, 2793, 2846, 2888, 2806,
    5081, 5054, 3047, 3545, 3244, 3056, 3085, 2944, 3103, 2897, 2740, 2740, 2740, 2846, 2793, 2620,
    4309, 4309, 2860, 2527, 3207, 3376, 3376, 3075, 3075, 3376, 3056, 2846, 2647, 2580, 2726, 2753,
    3056, 2916, 2806, 2888, 2740, 2687, 2897, 3103, 3150, 3150, 3216, 3169, 3056, 3010, 2963, 2846,
    4375, 3882, 2925, 2888, 2846, 2888, 2846, 2846, 2888, 2888, 2888, 2846, 2888, 2925, 2888, 2846,
    2981, 2916, 2916, 2981, 2981, 3056, 3122, 3216, 3150, 3056, 3010, 2972, 2972, 2972, 2925, 2740,
    4229, 4149, 3310, 3347, 2925, 2963, 2888, 2981, 2981, 2846, 2793, 2740, 2846, 2846, 2846, 2793,
    4080, 4014, 3103, 3010, 2925, 2925, 2925, 2888, 2925, 2925, 2846, 2846, 2846, 2793, 2888, 2780,
    4615, 4575, 3169, 3441, 3207, 2981, 2897, 3038, 3122, 2740, 2687, 2687, 2687, 2740, 2793, 2700,
    4149, 4269, 3789, 3657, 2726, 2780, 2888, 2888, 3010, 2972, 2925, 2846, 2687, 2687, 2793, 2888,
    4215, 3554, 2753, 2846, 2846, 2888, 2888, 2888, 2925, 2925, 2888, 2925, 2925, 2925, 2963, 2888,
    5174, 4921, 2261, 3432, 3789, 3479, 3347, 2846, 3310, 3479, 3150, 2897, 2460, 2487, 2753, 2925,
    3451, 3685, 3122, 3197, 3357, 3047, 3207, 3207, 2981, 3216, 3085, 2925, 2925, 2687, 2540, 2434,
    2981, 3010, 2793, 2793, 2740, 2793, 2846, 2972, 3056, 3103, 3150, 3150, 3150, 3103, 3010, 3010,
    2944, 2873, 2687, 2726, 2780, 3010, 3432, 3545, 3357, 3244, 3056, 3010, 2963, 2925, 2888, 2846,
    3019, 2944, 2897, 3010, 3010, 2972, 3019, 3103, 3056, 3056, 3010, 2888, 2846, 2925, 2925, 2888,
    3920, 3967, 3010, 3197, 3357, 3216, 3291, 3291, 3479, 3704, 3441, 2726, 2181, 2460, 2580, 2607,
];

static SILK_NLSF_CB1_ICDF_WB: [u8; 64] = [
    225,    204,    201,    184,    183,    175,    158,    154,
    153,    135,    119,    115,    113,    110,    109,     99,
     98,     95,     79,     68,     52,     50,     48,     45,
     43,     32,     31,     27,     18,     10,      3,      0,
    255,    251,    235,    230,    212,    201,    196,    182,
    167,    166,    163,    151,    138,    124,    110,    104,
     90,     78,     76,     70,     69,     57,     45,     34,
     24,     21,     11,      6,      5,      4,      3,      0,
];

static SILK_NLSF_CB2_SELECT_WB: [u8; 256] = [
      0,      0,      0,      0,      0,      0,      0,      1,
    100,    102,    102,     68,     68,     36,     34,     96,
    164,    107,    158,    185,    180,    185,    139,    102,
     64,     66,     36,     34,     34,      0,      1,     32,
    208,    139,    141,    191,    152,    185,    155,    104,
     96,    171,    104,    166,    102,    102,    102,    132,
      1,      0,      0,      0,      0,     16,     16,      0,
     80,    109,     78,    107,    185,    139,    103,    101,
    208,    212,    141,    139,    173,    153,    123,    103,
     36,      0,      0,      0,      0,      0,      0,      1,
     48,      0,      0,      0,      0,      0,      0,     32,
     68,    135,    123,    119,    119,    103,     69,     98,
     68,    103,    120,    118,    118,    102,     71,     98,
    134,    136,    157,    184,    182,    153,    139,    134,
    208,    168,    248,     75,    189,    143,    121,    107,
     32,     49,     34,     34,     34,      0,     17,      2,
    210,    235,    139,    123,    185,    137,    105,    134,
     98,    135,    104,    182,    100,    183,    171,    134,
    100,     70,     68,     70,     66,     66,     34,    131,
     64,    166,    102,     68,     36,      2,      1,      0,
    134,    166,    102,     68,     34,     34,     66,    132,
    212,    246,    158,    139,    107,    107,     87,    102,
    100,    219,    125,    122,    137,    118,    103,    132,
    114,    135,    137,    105,    171,    106,     50,     34,
    164,    214,    141,    143,    185,    151,    121,    103,
    192,     34,      0,      0,      0,      0,      0,      1,
    208,    109,     74,    187,    134,    249,    159,    137,
    102,    110,    154,    118,     87,    101,    119,    101,
      0,      2,      0,     36,     36,     66,     68,     35,
     96,    164,    102,    100,     36,      0,      2,     33,
    167,    138,    174,    102,    100,     84,      2,      2,
    100,    107,    120,    119,     36,    197,     24,      0,
];

static SILK_NLSF_CB2_ICDF_WB: [u8; 72] = [
    255,    254,    253,    244,     12,      3,      2,      1,      0,
    255,    254,    252,    224,     38,      3,      2,      1,      0,
    255,    254,    251,    209,     57,      4,      2,      1,      0,
    255,    254,    244,    195,     69,      4,      2,      1,      0,
    255,    251,    232,    184,     84,      7,      2,      1,      0,
    255,    254,    240,    186,     86,     14,      2,      1,      0,
    255,    254,    239,    178,     91,     30,      5,      1,      0,
    255,    248,    227,    177,    100,     19,      2,      1,      0,
];

static SILK_NLSF_PRED_WB_Q8: [u8; 30] = [
    175,    148,    160,    176,    178,    173,    174,    164,    177,    174,
    196,    182,    198,    192,    182,     68,     62,     66,     60,     72,
    117,     85,     90,    118,    136,    151,    142,    160,    142,    155,
];

static SILK_NLSF_DELTA_MIN_WB_Q15: [i16; 17] = [
    100,      3,     40,      3,      3,      3,      5,     14,
     14,     10,     11,      3,      8,      9,      7,      3,    347,
];

// ============================================================================
// NLSF Codebook structures
// ============================================================================

// SILK_FIX_CONST(0.18, 16) = 11796
// SILK_FIX_CONST(1.0/0.18, 6) = 356
pub static SILK_NLSF_CB_NB_MB: NlsfCodebook = NlsfCodebook {
    n_vectors: 32,
    order: 10,
    quant_step_size_q16: 11796,
    inv_quant_step_size_q6: 356,
    cb1_nlsf_q8: &SILK_NLSF_CB1_NB_MB_Q8,
    cb1_wght_q9: &SILK_NLSF_CB1_WGHT_Q9_NB_MB,
    cb1_icdf: &SILK_NLSF_CB1_ICDF_NB_MB,
    pred_q8: &SILK_NLSF_PRED_NB_MB_Q8,
    ec_sel: &SILK_NLSF_CB2_SELECT_NB_MB,
    ec_icdf: &SILK_NLSF_CB2_ICDF_NB_MB,
    delta_min_q15: &SILK_NLSF_DELTA_MIN_NB_MB_Q15,
};

// SILK_FIX_CONST(0.15, 16) = 9830
// SILK_FIX_CONST(1.0/0.15, 6) = 427
pub static SILK_NLSF_CB_WB: NlsfCodebook = NlsfCodebook {
    n_vectors: 32,
    order: 16,
    quant_step_size_q16: 9830,
    inv_quant_step_size_q6: 427,
    cb1_nlsf_q8: &SILK_NLSF_CB1_WB_Q8,
    cb1_wght_q9: &SILK_NLSF_CB1_WGHT_Q9_WB,
    cb1_icdf: &SILK_NLSF_CB1_ICDF_WB,
    pred_q8: &SILK_NLSF_PRED_WB_Q8,
    ec_sel: &SILK_NLSF_CB2_SELECT_WB,
    ec_icdf: &SILK_NLSF_CB2_ICDF_WB,
    delta_min_q15: &SILK_NLSF_DELTA_MIN_WB_Q15,
};

// ============================================================================
// LSF Cosine Table for NLSF to LPC conversion
// ============================================================================

static SILK_LSF_COS_TAB_FIX_Q12: [i16; LSF_COS_TAB_SZ + 1] = [
     8192,  8190,  8182,  8170,  8152,  8130,  8104,  8072,
     8034,  7994,  7946,  7896,  7840,  7778,  7714,  7644,
     7568,  7490,  7406,  7318,  7226,  7128,  7026,  6922,
     6812,  6698,  6580,  6458,  6332,  6204,  6070,  5934,
     5792,  5648,  5502,  5352,  5198,  5040,  4880,  4718,
     4552,  4382,  4212,  4038,  3862,  3684,  3502,  3320,
     3136,  2948,  2760,  2570,  2378,  2186,  1990,  1794,
     1598,  1400,  1202,  1002,   802,   602,   402,   202,
        0,  -202,  -402,  -602,  -802, -1002, -1202, -1400,
    -1598, -1794, -1990, -2186, -2378, -2570, -2760, -2948,
    -3136, -3320, -3502, -3684, -3862, -4038, -4212, -4382,
    -4552, -4718, -4880, -5040, -5198, -5352, -5502, -5648,
    -5792, -5934, -6070, -6204, -6332, -6458, -6580, -6698,
    -6812, -6922, -7026, -7128, -7226, -7318, -7406, -7490,
    -7568, -7644, -7714, -7778, -7840, -7896, -7946, -7994,
    -8034, -8072, -8104, -8130, -8152, -8170, -8182, -8190,
    -8192,
];

// NLSF extension iCDF for out-of-range values
static SILK_NLSF_EXT_ICDF: [u8; 7] = [100, 40, 16, 7, 3, 1, 0];

// Side information indices
#[derive(Default, Clone)]
struct SideInfoIndices {
    gains_indices: [i8; MAX_NB_SUBFR],
    ltp_index: [i8; MAX_NB_SUBFR],
    nlsf_indices: [i8; MAX_LPC_ORDER + 1],
    lag_index: i16,
    contour_index: i8,
    signal_type: i8,
    quant_offset_type: i8,
    nlsf_interp_coef_q2: i8,
    per_index: i8,
    ltp_scale_index: i8,
    seed: i8,
}

// Decoder control structure
struct SilkDecoderControl {
    pitch_l: [i32; MAX_NB_SUBFR],
    gains_q16: [i32; MAX_NB_SUBFR],
    pred_coef_q12: [[i16; MAX_LPC_ORDER]; 2],
    ltp_coef_q14: [i16; LTP_ORDER * MAX_NB_SUBFR],
    ltp_scale_q14: i32,
}

impl Default for SilkDecoderControl {
    fn default() -> Self {
        Self {
            pitch_l: [0; MAX_NB_SUBFR],
            gains_q16: [0; MAX_NB_SUBFR],
            pred_coef_q12: [[0; MAX_LPC_ORDER]; 2],
            ltp_coef_q14: [0; LTP_ORDER * MAX_NB_SUBFR],
            ltp_scale_q14: 0,
        }
    }
}

// LTP memory length constant (samples needed for LTP history)
const LTP_MEM_LENGTH_MS: usize = 20;
// Maximum LTP buffer size: ltp_mem_length + frame_length
const MAX_LTP_BUF_LENGTH: usize = MAX_FRAME_LENGTH * 2 + MAX_FRAME_LENGTH;

// Main SILK decoder state
pub struct SilkDecoder {
    // Decoder state
    prev_gain_q16: i32,
    exc_q14: [i32; MAX_FRAME_LENGTH],
    slpc_q14_buf: [i32; MAX_LPC_ORDER],
    out_buf: [i16; MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH],
    lag_prev: i32,
    last_gain_index: i8,

    // LTP state buffer (Q15 format for long-term prediction signal)
    s_ltp_q15: Vec<i32>,
    s_ltp_buf_idx: usize,

    // Configuration
    fs_khz: i32,
    fs_api_hz: i32,
    nb_subfr: usize,
    frame_length: usize,
    subfr_length: usize,
    ltp_mem_length: usize,
    lpc_order: usize,

    // NLSF state
    prev_nlsf_q15: [i16; MAX_LPC_ORDER],
    first_frame_after_reset: bool,

    // Frame tracking
    n_frames_decoded: i32,
    n_frames_per_packet: i32,

    // Entropy coding state
    ec_prev_signal_type: i32,
    ec_prev_lag_index: i16,

    // Loss concealment
    loss_cnt: i32,
    prev_signal_type: i8,

    // Quantization indices
    indices: SideInfoIndices,

    // NLSF codebook reference
    nlsf_cb: &'static NlsfCodebook,
}

// ============================================================================
// NLSF Decoding Helper Functions
// ============================================================================

/// Unpack entropy table indices and predictor coefficients from the codebook
fn silk_nlsf_unpack(
    ec_ix: &mut [i16],
    pred_q8: &mut [u8],
    nlsf_cb: &NlsfCodebook,
    cb1_index: usize,
) {
    let order = nlsf_cb.order as usize;
    let ec_sel_ptr = &nlsf_cb.ec_sel[cb1_index * order / 2..];

    for i in (0..order).step_by(2) {
        let entry = ec_sel_ptr[i / 2];

        // Extract entropy table indices for even and odd coefficients
        // ec_ix[i] = ((entry >> 1) & 7) * (2 * NLSF_QUANT_MAX_AMPLITUDE + 1)
        ec_ix[i] = (((entry >> 1) & 7) as i16) * (2 * NLSF_QUANT_MAX_AMPLITUDE as i16 + 1);
        // pred_q8[i] from pred table based on (entry & 1)
        let pred_idx_even = i + ((entry & 1) as usize) * (order - 1);
        pred_q8[i] = nlsf_cb.pred_q8[pred_idx_even];

        // ec_ix[i+1] = ((entry >> 5) & 7) * (2 * NLSF_QUANT_MAX_AMPLITUDE + 1)
        ec_ix[i + 1] = (((entry >> 5) & 7) as i16) * (2 * NLSF_QUANT_MAX_AMPLITUDE as i16 + 1);
        // pred_q8[i+1] from pred table based on ((entry >> 4) & 1)
        let pred_idx_odd = i + (((entry >> 4) & 1) as usize) * (order - 1) + 1;
        pred_q8[i + 1] = nlsf_cb.pred_q8[pred_idx_odd];
    }
}

/// Predictive dequantizer for NLSF residuals
fn silk_nlsf_residual_dequant(
    x_q10: &mut [i16],
    indices: &[i8],
    pred_coef_q8: &[u8],
    quant_step_size_q16: i32,
    order: usize,
) {
    let mut out_q10: i32 = 0;

    for i in (0..order).rev() {
        // Prediction: pred_Q10 = (out_Q10 * pred_coef_Q8[i]) >> 8
        let pred_q10 = (out_q10 * (pred_coef_q8[i] as i32)) >> 8;

        // Dequantize: out_Q10 = indices[i] << 10
        out_q10 = (indices[i] as i32) << 10;

        // Apply level adjustment
        if out_q10 > 0 {
            out_q10 -= NLSF_QUANT_LEVEL_ADJ_Q10;
        } else if out_q10 < 0 {
            out_q10 += NLSF_QUANT_LEVEL_ADJ_Q10;
        }

        // Scale by quantization step size and add prediction
        // out_Q10 = pred_Q10 + ((out_Q10 * quant_step_size_Q16) >> 16)
        out_q10 = pred_q10 + ((out_q10 * quant_step_size_q16) >> 16);

        x_q10[i] = out_q10 as i16;
    }
}

/// Main NLSF decoding function - converts indices to NLSF Q15 values
fn silk_nlsf_decode(
    nlsf_q15: &mut [i16],
    nlsf_indices: &[i8],
    nlsf_cb: &NlsfCodebook,
) {
    let order = nlsf_cb.order as usize;
    let mut pred_q8 = [0u8; MAX_LPC_ORDER];
    let mut ec_ix = [0i16; MAX_LPC_ORDER];
    let mut res_q10 = [0i16; MAX_LPC_ORDER];

    // Unpack entropy table indices and predictor for current CB1 index
    let cb1_index = nlsf_indices[0] as usize;
    silk_nlsf_unpack(&mut ec_ix[..order], &mut pred_q8[..order], nlsf_cb, cb1_index);

    // Predictive residual dequantizer
    silk_nlsf_residual_dequant(
        &mut res_q10[..order],
        &nlsf_indices[1..=order],
        &pred_q8[..order],
        nlsf_cb.quant_step_size_q16,
        order,
    );

    // Apply inverse square-rooted weights to first stage and add to output
    let cb_element_start = cb1_index * order;
    let cb_element = &nlsf_cb.cb1_nlsf_q8[cb_element_start..cb_element_start + order];
    let cb_wght = &nlsf_cb.cb1_wght_q9[cb_element_start..cb_element_start + order];

    for i in 0..order {
        // NLSF_Q15 = ((res_Q10[i] << 14) / cb_wght_Q9[i]) + (cb_element[i] << 7)
        let res_scaled = (res_q10[i] as i32) << 14;
        let wght = cb_wght[i] as i32;
        let nlsf_tmp = if wght != 0 {
            (res_scaled / wght) + ((cb_element[i] as i32) << 7)
        } else {
            (cb_element[i] as i32) << 7
        };

        // Clamp to valid range [0, 32767]
        nlsf_q15[i] = nlsf_tmp.clamp(0, 32767) as i16;
    }

    // NLSF stabilization
    silk_nlsf_stabilize(nlsf_q15, nlsf_cb.delta_min_q15, order);
}

/// NLSF stabilizer - ensures minimum distance between NLSFs and bounds
fn silk_nlsf_stabilize(nlsf_q15: &mut [i16], delta_min_q15: &[i16], order: usize) {
    for _loop in 0..MAX_NLSF_STABILIZE_LOOPS {
        // Find smallest distance
        let mut min_diff_q15 = (nlsf_q15[0] as i32) - (delta_min_q15[0] as i32);
        let mut min_idx = 0;

        // Middle elements
        for i in 1..order {
            let diff = (nlsf_q15[i] as i32) - (nlsf_q15[i - 1] as i32) - (delta_min_q15[i] as i32);
            if diff < min_diff_q15 {
                min_diff_q15 = diff;
                min_idx = i;
            }
        }

        // Last element
        let last_diff = (1 << 15) - (nlsf_q15[order - 1] as i32) - (delta_min_q15[order] as i32);
        if last_diff < min_diff_q15 {
            min_diff_q15 = last_diff;
            min_idx = order;
        }

        // Check if smallest distance is non-negative
        if min_diff_q15 >= 0 {
            return;
        }

        // Fix the violation
        if min_idx == 0 {
            // Move away from lower limit
            nlsf_q15[0] = delta_min_q15[0];
        } else if min_idx == order {
            // Move away from upper limit
            nlsf_q15[order - 1] = ((1 << 15) - (delta_min_q15[order] as i32)) as i16;
        } else {
            // Find center frequency bounds
            let mut min_center_q15: i32 = 0;
            for k in 0..min_idx {
                min_center_q15 += delta_min_q15[k] as i32;
            }
            min_center_q15 += (delta_min_q15[min_idx] as i32) >> 1;

            let mut max_center_q15: i32 = 1 << 15;
            for k in (min_idx + 1..=order).rev() {
                max_center_q15 -= delta_min_q15[k] as i32;
            }
            max_center_q15 -= (delta_min_q15[min_idx] as i32) >> 1;

            // Calculate new center frequency
            let center_sum = (nlsf_q15[min_idx - 1] as i32) + (nlsf_q15[min_idx] as i32);
            let center_freq_q15 = ((center_sum + 1) >> 1).clamp(min_center_q15, max_center_q15);

            // Move apart keeping same center
            nlsf_q15[min_idx - 1] = (center_freq_q15 - ((delta_min_q15[min_idx] as i32) >> 1)) as i16;
            nlsf_q15[min_idx] = (nlsf_q15[min_idx - 1] as i32 + delta_min_q15[min_idx] as i32) as i16;
        }
    }

    // Fallback: sort and enforce constraints
    // Sort using insertion sort (efficient for nearly sorted arrays)
    for i in 1..order {
        let key = nlsf_q15[i];
        let mut j = i;
        while j > 0 && nlsf_q15[j - 1] > key {
            nlsf_q15[j] = nlsf_q15[j - 1];
            j -= 1;
        }
        nlsf_q15[j] = key;
    }

    // First NLSF should be no less than delta_min[0]
    nlsf_q15[0] = nlsf_q15[0].max(delta_min_q15[0]);

    // Keep delta_min distance between NLSFs
    for i in 1..order {
        let min_val = nlsf_q15[i - 1].saturating_add(delta_min_q15[i]);
        nlsf_q15[i] = nlsf_q15[i].max(min_val);
    }

    // Last NLSF should be no higher than 1 - delta_min[order]
    let max_last = ((1 << 15) - (delta_min_q15[order] as i32)) as i16;
    nlsf_q15[order - 1] = nlsf_q15[order - 1].min(max_last);

    // Enforce delta_min from the end
    for i in (0..order - 1).rev() {
        let max_val = nlsf_q15[i + 1] - delta_min_q15[i + 1];
        nlsf_q15[i] = nlsf_q15[i].min(max_val);
    }
}

// ============================================================================
// NLSF to LPC Conversion
// ============================================================================

/// Helper function for NLSF2A - computes intermediate polynomial
fn silk_nlsf2a_find_poly(out: &mut [i32], c_lsf: &[i32], dd: usize) {
    const QA: i32 = 16;

    out[0] = 1 << QA;
    out[1] = -c_lsf[0];

    for k in 1..dd {
        let ftmp = c_lsf[2 * k];
        // out[k+1] = (out[k-1] << 1) - ((ftmp * out[k]) >> QA)
        out[k + 1] = (out[k - 1] << 1) - rshift_round64(ftmp as i64 * out[k] as i64, QA);

        for n in (2..=k).rev() {
            out[n] = out[n] + out[n - 2] - rshift_round64(ftmp as i64 * out[n - 1] as i64, QA);
        }
        out[1] -= ftmp;
    }
}

/// Convert NLSF to LPC coefficients
fn silk_nlsf2a(a_q12: &mut [i16], nlsf: &[i16], order: usize) {
    const QA: i32 = 16;

    // Ordering for better numerical accuracy
    static ORDERING_16: [usize; 16] = [0, 15, 8, 7, 4, 11, 12, 3, 2, 13, 10, 5, 6, 9, 14, 1];
    static ORDERING_10: [usize; 10] = [0, 9, 6, 3, 4, 5, 8, 1, 2, 7];

    let ordering: &[usize] = if order == 16 { &ORDERING_16 } else { &ORDERING_10 };

    let mut cos_lsf_qa = [0i32; MAX_LPC_ORDER];
    let dd = order >> 1;

    // Convert NLSF to 2*cos(LSF) using piecewise linear interpolation
    for k in 0..order {
        // f_int on scale 0-127 (rounded down)
        let f_int = (nlsf[k] as i32) >> (15 - 7);

        // f_frac range: 0..255
        let f_frac = (nlsf[k] as i32) - (f_int << (15 - 7));

        // Read start and end value from table
        let f_int_clamped = (f_int as usize).min(LSF_COS_TAB_SZ - 1);
        let cos_val = SILK_LSF_COS_TAB_FIX_Q12[f_int_clamped] as i32;
        let delta = (SILK_LSF_COS_TAB_FIX_Q12[f_int_clamped + 1] as i32) - cos_val;

        // Linear interpolation: ((cos_val << 8) + delta * f_frac) >> (20 - QA)
        let cos_interp = (cos_val << 8) + delta * f_frac;
        cos_lsf_qa[ordering[k]] = rshift_round(cos_interp, 20 - QA as i32);
    }

    // Generate even and odd polynomials using convolution
    let mut p = [0i32; MAX_LPC_ORDER / 2 + 1];
    let mut q = [0i32; MAX_LPC_ORDER / 2 + 1];

    silk_nlsf2a_find_poly(&mut p[..dd + 1], &cos_lsf_qa[0..], dd);
    silk_nlsf2a_find_poly(&mut q[..dd + 1], &cos_lsf_qa[1..], dd);

    // Convert even and odd polynomials to Q12 filter coefficients
    let mut a32_qa1 = [0i32; MAX_LPC_ORDER];

    for k in 0..dd {
        let p_tmp = p[k + 1] + p[k];
        let q_tmp = q[k + 1] - q[k];

        // a32_QA1[k] = -Q_tmp - P_tmp (QA+1)
        a32_qa1[k] = -q_tmp - p_tmp;
        // a32_QA1[d-k-1] = Q_tmp - P_tmp (QA+1)
        a32_qa1[order - k - 1] = q_tmp - p_tmp;
    }

    // Convert int32 coefficients to Q12 int16 coefficients with limiting
    silk_lpc_fit(a_q12, &mut a32_qa1, 12, QA as i32 + 1, order);

    // Check stability and apply bandwidth expansion if needed
    for i in 0..MAX_LPC_STABILIZE_ITERATIONS {
        if silk_lpc_inverse_pred_gain(a_q12, order) != 0 {
            break;
        }

        // Apply bandwidth expansion
        silk_bwexpander_32(&mut a32_qa1, order, 65536 - (2 << i));

        for k in 0..order {
            a_q12[k] = rshift_round(a32_qa1[k], QA as i32 + 1 - 12) as i16;
        }
    }
}

/// Convert int32 coefficients to int16 with limiting
fn silk_lpc_fit(
    a_qout: &mut [i16],
    a_qin: &mut [i32],
    qout: i32,
    qin: i32,
    order: usize,
) {
    const MAX_ITER: usize = 10;

    for _iter in 0..MAX_ITER {
        // Find maximum absolute value and its index
        let mut maxabs = 0i32;
        let mut idx = 0;

        for k in 0..order {
            let absval = a_qin[k].abs();
            if absval > maxabs {
                maxabs = absval;
                idx = k;
            }
        }

        maxabs = rshift_round(maxabs, qin - qout);

        if maxabs > i16::MAX as i32 {
            // Reduce magnitude of prediction coefficients
            maxabs = maxabs.min(163838); // (i32::MAX >> 14) + i16::MAX

            let chirp_q16 = 65536 - 1 - ((maxabs - i16::MAX as i32) << 14) / ((maxabs * (idx as i32 + 1)) >> 2);
            silk_bwexpander_32(a_qin, order, chirp_q16);
        } else {
            break;
        }
    }

    // Final conversion
    for k in 0..order {
        let val = rshift_round(a_qin[k], qin - qout);
        a_qout[k] = val.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    }
}

/// Bandwidth expander for int32 coefficients
fn silk_bwexpander_32(ar: &mut [i32], order: usize, mut chirp_q16: i32) {
    let chirp_minus_one_q16 = chirp_q16 - 65536;

    for i in 0..order - 1 {
        ar[i] = smulww(chirp_q16, ar[i]);
        chirp_q16 += rshift_round(chirp_q16 * chirp_minus_one_q16, 16);
    }
    ar[order - 1] = smulww(chirp_q16, ar[order - 1]);
}

/// Compute inverse of LPC prediction gain and test stability
fn silk_lpc_inverse_pred_gain(a_q12: &[i16], order: usize) -> i32 {
    const QA: i32 = 24;

    let mut atmp_qa = [0i32; MAX_LPC_ORDER];
    let mut dc_resp: i32 = 0;

    // Increase Q domain of the AR coefficients
    for k in 0..order {
        dc_resp += a_q12[k] as i32;
        atmp_qa[k] = (a_q12[k] as i32) << (QA - 12);
    }

    // If DC is unstable, no need to continue
    if dc_resp >= 4096 {
        return 0;
    }

    lpc_inverse_pred_gain_qa(&mut atmp_qa, order)
}

fn lpc_inverse_pred_gain_qa(a_qa: &mut [i32], order: usize) -> i32 {
    const QA: i32 = 24;
    const A_LIMIT: i32 = (0.99975 * (1 << QA) as f64) as i32;

    let mut inv_gain_q30: i32 = 1 << 30;

    for k in (1..order).rev() {
        // Check for stability
        if a_qa[k] > A_LIMIT || a_qa[k] < -A_LIMIT {
            return 0;
        }

        // Set RC equal to negated AR coef
        let rc_q31 = -(a_qa[k] << (31 - QA));

        // rc_mult1_Q30 = 1 - rc^2 in Q30
        let rc_mult1_q30 = (1 << 30) - smmul(rc_q31, rc_q31);

        if rc_mult1_q30 <= (1 << 15) {
            return 0;
        }

        // Update inverse gain
        inv_gain_q30 = smmul(inv_gain_q30, rc_mult1_q30) << 2;

        if inv_gain_q30 < ((1.0 / MAX_PREDICTION_POWER_GAIN) * (1 << 30) as f32) as i32 {
            return 0;
        }

        // Compute rc_mult2 = 1 / rc_mult1
        let mult2q = 32 - clz32(rc_mult1_q30.abs());
        let rc_mult2 = inverse32_varq(rc_mult1_q30, mult2q + 30);

        // Update AR coefficients
        for n in 0..((k + 1) >> 1) {
            let tmp1 = a_qa[n];
            let tmp2 = a_qa[k - n - 1];

            let val1 = tmp1 - mul32_frac_q(tmp2, rc_q31, 31);
            let tmp64_1_raw = val1 as i64 * rc_mult2 as i64;
            let tmp64_1_shifted = rshift_round64_raw(tmp64_1_raw, mult2q);

            if tmp64_1_shifted > i32::MAX as i64 || tmp64_1_shifted < i32::MIN as i64 {
                return 0;
            }
            a_qa[n] = tmp64_1_shifted as i32;

            if n != k - n - 1 {
                let val2 = tmp2 - mul32_frac_q(tmp1, rc_q31, 31);
                let tmp64_2_raw = val2 as i64 * rc_mult2 as i64;
                let tmp64_2_shifted = rshift_round64_raw(tmp64_2_raw, mult2q);

                if tmp64_2_shifted > i32::MAX as i64 || tmp64_2_shifted < i32::MIN as i64 {
                    return 0;
                }
                a_qa[k - n - 1] = tmp64_2_shifted as i32;
            }
        }
    }

    // Check stability of last coefficient
    if a_qa[0] > A_LIMIT || a_qa[0] < -A_LIMIT {
        return 0;
    }

    let rc_q31 = -(a_qa[0] << (31 - QA));
    let rc_mult1_q30 = (1 << 30) - smmul(rc_q31, rc_q31);

    inv_gain_q30 = smmul(inv_gain_q30, rc_mult1_q30) << 2;

    if inv_gain_q30 < ((1.0 / MAX_PREDICTION_POWER_GAIN) * (1 << 30) as f32) as i32 {
        return 0;
    }

    inv_gain_q30
}

// ============================================================================
// Fixed-Point Math Helpers
// ============================================================================

#[inline]
fn rshift_round(val: i32, shift: i32) -> i32 {
    if shift <= 0 {
        val << -shift
    } else if shift >= 32 {
        0
    } else {
        (val + (1 << (shift - 1))) >> shift
    }
}

#[inline]
fn rshift_round64(val: i64, shift: i32) -> i32 {
    rshift_round64_raw(val, shift) as i32
}

#[inline]
fn rshift_round64_raw(val: i64, shift: i32) -> i64 {
    if shift <= 0 {
        val << -shift
    } else if shift >= 64 {
        0
    } else {
        (val + (1i64 << (shift - 1))) >> shift
    }
}

#[inline]
fn smulww(a: i32, b: i32) -> i32 {
    ((a as i64 * b as i64) >> 16) as i32
}

#[inline]
fn smmul(a: i32, b: i32) -> i32 {
    ((a as i64 * b as i64) >> 32) as i32
}

#[inline]
fn mul32_frac_q(a: i32, b: i32, q: i32) -> i32 {
    rshift_round64(a as i64 * b as i64, q)
}

#[inline]
fn clz32(x: i32) -> i32 {
    if x == 0 {
        32
    } else {
        x.abs().leading_zeros() as i32
    }
}

/// Compute 1/x in variable Q format
fn inverse32_varq(b: i32, q: i32) -> i32 {
    if b == 0 {
        return i32::MAX;
    }

    // Normalize b
    let b_norm = b.abs();
    let lshift = clz32(b_norm) - 1;
    let b_shifted = if lshift >= 0 {
        b_norm << lshift
    } else {
        b_norm >> (-lshift)
    };

    // Calculate approximation
    let b_headroom = 30 - lshift;

    if q > b_headroom {
        let result = (1i64 << (q - b_headroom + 30)) / (b_shifted as i64);
        if b < 0 {
            -(result as i32)
        } else {
            result as i32
        }
    } else {
        let result = (1 << 30) / b_shifted;
        let shift = b_headroom - q;
        let final_result = if shift >= 32 { 0 } else { result >> shift };
        if b < 0 {
            -final_result
        } else {
            final_result
        }
    }
}

impl SilkDecoder {
    /// Create a new SILK decoder
    pub fn new(sample_rate: u32) -> Self {
        let fs_khz = (sample_rate / 1000) as i32;
        let nb_subfr = if fs_khz == 8 { 2 } else { 4 };
        let frame_length = (fs_khz as usize) * 20; // 20ms frame
        let subfr_length = frame_length / nb_subfr;
        let ltp_mem_length = (fs_khz as usize) * 20; // LTP_MEM_LENGTH_MS = 20
        let lpc_order = if fs_khz <= 12 { 10 } else { 16 };

        // Select NLSF codebook based on sample rate
        // NB (8kHz) and MB (12kHz) use the same codebook with order 10
        // WB (16kHz) uses separate codebook with order 16
        let nlsf_cb: &'static NlsfCodebook = if fs_khz <= 12 {
            &SILK_NLSF_CB_NB_MB
        } else {
            &SILK_NLSF_CB_WB
        };

        // Initialize prev_nlsf_q15 to linear spacing
        let mut prev_nlsf_q15 = [0i16; MAX_LPC_ORDER];
        for i in 0..lpc_order {
            // Linear spacing: (i + 0.5) / order * 2^15
            prev_nlsf_q15[i] = (((i as i32 * 2 + 1) << 14) / (lpc_order as i32)) as i16;
        }

        // Allocate LTP buffer: ltp_mem_length + frame_length
        let ltp_buf_size = ltp_mem_length + frame_length;
        let s_ltp_q15 = vec![0i32; ltp_buf_size];

        Self {
            prev_gain_q16: 65536,
            exc_q14: [0; MAX_FRAME_LENGTH],
            slpc_q14_buf: [0; MAX_LPC_ORDER],
            out_buf: [0; MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH],
            lag_prev: 0,
            last_gain_index: 0,
            s_ltp_q15,
            s_ltp_buf_idx: ltp_mem_length,
            fs_khz,
            fs_api_hz: sample_rate as i32,
            nb_subfr,
            frame_length,
            subfr_length,
            ltp_mem_length,
            lpc_order,
            prev_nlsf_q15,
            first_frame_after_reset: true,
            n_frames_decoded: 0,
            n_frames_per_packet: 1,
            ec_prev_signal_type: 0,
            ec_prev_lag_index: 0,
            loss_cnt: 0,
            prev_signal_type: TYPE_NO_VOICE_ACTIVITY,
            indices: SideInfoIndices::default(),
            nlsf_cb,
        }
    }

    /// Reset decoder state
    pub fn reset(&mut self) {
        self.prev_gain_q16 = 65536;
        self.exc_q14 = [0; MAX_FRAME_LENGTH];
        self.slpc_q14_buf = [0; MAX_LPC_ORDER];
        self.out_buf = [0; MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH];
        self.lag_prev = 0;
        self.first_frame_after_reset = true;
        self.loss_cnt = 0;

        // Reset LTP state
        self.s_ltp_q15.fill(0);
        self.s_ltp_buf_idx = self.ltp_mem_length;

        // Reset NLSF state to linear spacing
        for i in 0..self.lpc_order {
            self.prev_nlsf_q15[i] = (((i as i32 * 2 + 1) << 14) / (self.lpc_order as i32)) as i16;
        }
    }

    /// Decode a SILK frame
    pub fn decode_frame(
        &mut self,
        ec: &mut EntropyCoder,
        output: &mut [i16],
        lost_flag: bool,
        cond_coding: i32,
    ) -> Result<usize, &'static str> {
        let mut dec_ctrl = SilkDecoderControl::default();

        if !lost_flag {
            // Allocate pulse buffer
            let mut pulses = vec![0i16; self.frame_length];

            // Decode quantization indices
            self.decode_indices(ec, 0, false, cond_coding)?;

            // Decode pulse signal
            self.decode_pulses(ec, &mut pulses)?;

            // Decode parameters
            self.decode_parameters(&mut dec_ctrl, cond_coding)?;

            // Run inverse NSQ (synthesis)
            self.decode_core(&dec_ctrl, output, &pulses)?;

            // Update output buffer
            let mv_len = self.ltp_mem_length - self.frame_length;
            self.out_buf.copy_within(self.frame_length..self.frame_length + mv_len, 0);
            self.out_buf[mv_len..mv_len + self.frame_length].copy_from_slice(&output[..self.frame_length]);

            self.loss_cnt = 0;
            self.prev_signal_type = self.indices.signal_type;
            self.first_frame_after_reset = false;
        } else {
            // Packet loss concealment (basic)
            output[..self.frame_length].fill(0);
            self.loss_cnt += 1;
        }

        self.lag_prev = dec_ctrl.pitch_l[self.nb_subfr - 1];

        Ok(self.frame_length)
    }

    /// Decode side information indices
    fn decode_indices(
        &mut self,
        ec: &mut EntropyCoder,
        _frame_index: i32,
        _decode_lbrr: bool,
        cond_coding: i32,
    ) -> Result<(), &'static str> {
        // Decode signal type and quantizer offset
        let ix = ec.decode_icdf(&SILK_TYPE_OFFSET_NO_VAD_ICDF, 8);
        self.indices.signal_type = (ix >> 1) as i8;
        self.indices.quant_offset_type = (ix & 1) as i8;

        // Decode gains - first subframe
        if cond_coding == CODE_CONDITIONALLY {
            self.indices.gains_indices[0] = ec.decode_icdf(&SILK_DELTA_GAIN_ICDF, 8) as i8;
        } else {
            let msb = ec.decode_icdf(&SILK_GAIN_ICDF[self.indices.signal_type as usize], 8);
            let lsb = ec.decode_icdf(&SILK_UNIFORM8_ICDF, 8);
            self.indices.gains_indices[0] = ((msb << 3) + lsb) as i8;
        }

        // Remaining subframes
        for i in 1..self.nb_subfr {
            self.indices.gains_indices[i] = ec.decode_icdf(&SILK_DELTA_GAIN_ICDF, 8) as i8;
        }

        // Decode NLSF indices
        self.decode_nlsf_indices(ec)?;

        // NLSF interpolation factor
        if self.nb_subfr == MAX_NB_SUBFR {
            self.indices.nlsf_interp_coef_q2 = ec.decode_icdf(&SILK_NLSF_INTERPOLATION_FACTOR_ICDF, 8) as i8;
        } else {
            self.indices.nlsf_interp_coef_q2 = 4;
        }

        // Decode pitch parameters for voiced frames
        if self.indices.signal_type == TYPE_VOICED {
            // Decode lag index (simplified)
            let lag_base = ec.decode_icdf(&SILK_PITCH_LAG_ICDF, 8) as i32;
            self.indices.lag_index = (lag_base * (self.fs_khz / 2)) as i16;
            self.ec_prev_lag_index = self.indices.lag_index;

            // Contour index
            self.indices.contour_index = ec.decode_icdf(&SILK_PITCH_CONTOUR_ICDF, 8) as i8;

            // PER index (periodicity index - selects LTP codebook)
            self.indices.per_index = ec.decode_icdf(&SILK_LTP_PER_INDEX_ICDF, 8) as i8;

            // LTP indices - decode using appropriate codebook based on PER index
            for k in 0..self.nb_subfr {
                let ltp_idx = match self.indices.per_index {
                    0 => ec.decode_icdf(&SILK_LTP_GAIN_ICDF_0, 8),
                    1 => ec.decode_icdf(&SILK_LTP_GAIN_ICDF_1, 8),
                    _ => ec.decode_icdf(&SILK_LTP_GAIN_ICDF_2, 8),
                };
                self.indices.ltp_index[k] = ltp_idx as i8;
            }

            // LTP scaling
            if cond_coding == CODE_INDEPENDENTLY {
                self.indices.ltp_scale_index = ec.decode_icdf(&SILK_LTPSCALE_ICDF, 8) as i8;
            } else {
                self.indices.ltp_scale_index = 0;
            }
        }

        self.ec_prev_signal_type = self.indices.signal_type as i32;

        // Decode seed
        self.indices.seed = ec.decode_icdf(&SILK_UNIFORM4_ICDF, 8) as i8;

        Ok(())
    }

    /// Decode NLSF indices from the bitstream
    fn decode_nlsf_indices(&mut self, ec: &mut EntropyCoder) -> Result<(), &'static str> {
        let nlsf_cb = self.nlsf_cb;
        let order = nlsf_cb.order as usize;
        let n_vectors = nlsf_cb.n_vectors as usize;

        // Decode CB1 index (primary codebook index)
        // The iCDF is split into two halves for voiced/unvoiced
        let signal_type_offset = (self.indices.signal_type as usize >> 1) * n_vectors;
        let cb1_icdf_slice = &nlsf_cb.cb1_icdf[signal_type_offset..signal_type_offset + n_vectors];
        self.indices.nlsf_indices[0] = ec.decode_icdf(cb1_icdf_slice, 8) as i8;

        // Unpack entropy table indices for residual decoding
        let cb1_index = self.indices.nlsf_indices[0] as usize;
        let mut ec_ix = [0i16; MAX_LPC_ORDER];
        let mut pred_q8 = [0u8; MAX_LPC_ORDER];
        silk_nlsf_unpack(&mut ec_ix[..order], &mut pred_q8[..order], nlsf_cb, cb1_index);

        // Decode residual indices for each coefficient
        for i in 0..order {
            // Get the iCDF slice for this coefficient's entropy table
            let icdf_start = ec_ix[i] as usize;
            let icdf_len = 2 * NLSF_QUANT_MAX_AMPLITUDE as usize + 1;
            let ec_icdf_slice = &nlsf_cb.ec_icdf[icdf_start..icdf_start + icdf_len];

            let mut ix = ec.decode_icdf(ec_icdf_slice, 8) as i32;

            // Handle out-of-range values with extension table
            if ix == 0 {
                // Negative extension
                ix -= ec.decode_icdf(&SILK_NLSF_EXT_ICDF, 8) as i32;
            } else if ix == 2 * NLSF_QUANT_MAX_AMPLITUDE {
                // Positive extension
                ix += ec.decode_icdf(&SILK_NLSF_EXT_ICDF, 8) as i32;
            }

            // Convert to signed index centered at 0
            self.indices.nlsf_indices[i + 1] = (ix - NLSF_QUANT_MAX_AMPLITUDE) as i8;
        }

        Ok(())
    }

    /// Decode pulse/excitation signal
    fn decode_pulses(&mut self, ec: &mut EntropyCoder, pulses: &mut [i16]) -> Result<(), &'static str> {
        let _signal_type = self.indices.signal_type;

        // Decode rate level
        let rate_level = ec.decode_icdf(&SILK_RATE_LEVELS_ICDF[0], 8) as usize;

        // Calculate number of shell blocks
        let mut iter = self.frame_length >> LOG2_SHELL_CODEC_FRAME_LENGTH;
        if iter * SHELL_CODEC_FRAME_LENGTH < self.frame_length {
            iter += 1;
        }

        let mut sum_pulses = vec![0usize; iter];
        let mut n_lshifts = vec![0usize; iter];

        // Decode sum of pulses per block
        let cdf_ptr = &SILK_PULSES_PER_BLOCK_ICDF[rate_level.min(N_RATE_LEVELS - 1)];
        for i in 0..iter {
            sum_pulses[i] = ec.decode_icdf(cdf_ptr, 8) as usize;

            // LSB indication
            while sum_pulses[i] == SILK_MAX_PULSES + 1 {
                n_lshifts[i] += 1;
                sum_pulses[i] = ec.decode_icdf(cdf_ptr, 8) as usize;
            }
        }

        // Shell decoding (simplified - just zero out for now)
        for i in 0..iter {
            let start = i * SHELL_CODEC_FRAME_LENGTH;
            let end = cmp::min(start + SHELL_CODEC_FRAME_LENGTH, pulses.len());
            pulses[start..end].fill(0);
        }

        // Decode signs (simplified)
        Ok(())
    }

    /// Decode parameters from indices
    fn decode_parameters(
        &mut self,
        dec_ctrl: &mut SilkDecoderControl,
        _cond_coding: i32,
    ) -> Result<(), &'static str> {
        // Dequantize gains (simplified linear mapping)
        for k in 0..self.nb_subfr {
            let gain_idx = self.indices.gains_indices[k] as i32;
            // Simple gain mapping: 2 dB to 88 dB range
            let gain_db = 2.0 + (gain_idx as f32 * 86.0 / 63.0);
            dec_ctrl.gains_q16[k] = ((10.0_f32.powf(gain_db / 20.0) * 65536.0) as i32).max(65536);
        }

        // Decode NLSFs and convert to LPC
        let mut nlsf_q15 = [0i16; MAX_LPC_ORDER];

        // Decode current frame NLSFs from indices
        silk_nlsf_decode(&mut nlsf_q15[..self.lpc_order], &self.indices.nlsf_indices, self.nlsf_cb);

        // Handle interpolation between frames
        if self.indices.nlsf_interp_coef_q2 < 4 {
            // Interpolate NLSF values for first half
            let mut nlsf_interp_q15 = [0i16; MAX_LPC_ORDER];
            let interp_factor = self.indices.nlsf_interp_coef_q2 as i32;

            for i in 0..self.lpc_order {
                // nlsf_interp = prev + (interp_factor * (current - prev)) / 4
                let prev = self.prev_nlsf_q15[i] as i32;
                let curr = nlsf_q15[i] as i32;
                nlsf_interp_q15[i] = (prev + ((interp_factor * (curr - prev)) >> 2)) as i16;
            }

            // Convert interpolated NLSF to LPC for first half of frame
            silk_nlsf2a(
                &mut dec_ctrl.pred_coef_q12[0][..self.lpc_order],
                &nlsf_interp_q15[..self.lpc_order],
                self.lpc_order,
            );
        }

        // Convert current frame NLSF to LPC for second half (or entire frame if no interpolation)
        silk_nlsf2a(
            &mut dec_ctrl.pred_coef_q12[1][..self.lpc_order],
            &nlsf_q15[..self.lpc_order],
            self.lpc_order,
        );

        // If no interpolation, copy to first half
        if self.indices.nlsf_interp_coef_q2 >= 4 {
            dec_ctrl.pred_coef_q12[0] = dec_ctrl.pred_coef_q12[1];
        }

        // Save current NLSFs for next frame interpolation
        self.prev_nlsf_q15[..self.lpc_order].copy_from_slice(&nlsf_q15[..self.lpc_order]);

        // Decode pitch parameters for voiced frames
        if self.indices.signal_type == TYPE_VOICED {
            // Decode pitch lags using contour index for per-subframe refinement
            decode_pitch(
                self.indices.lag_index,
                self.indices.contour_index,
                &mut dec_ctrl.pitch_l,
                self.fs_khz,
                self.nb_subfr,
            );

            // Decode LTP coefficients from VQ codebook
            // Select codebook based on PER index
            let per_idx = self.indices.per_index.clamp(0, 2) as usize;
            for k in 0..self.nb_subfr {
                let ltp_idx = self.indices.ltp_index[k] as usize;

                // Look up coefficients in the appropriate codebook and convert from Q7 to Q14
                let cbk_row: &[i8; 5] = match per_idx {
                    0 => &SILK_LTP_VQ_Q7_0[ltp_idx.min(SILK_LTP_VQ_SIZES[0] - 1)],
                    1 => &SILK_LTP_VQ_Q7_1[ltp_idx.min(SILK_LTP_VQ_SIZES[1] - 1)],
                    _ => &SILK_LTP_VQ_Q7_2[ltp_idx.min(SILK_LTP_VQ_SIZES[2] - 1)],
                };

                // Convert from Q7 to Q14 (shift left by 7)
                for i in 0..LTP_ORDER {
                    dec_ctrl.ltp_coef_q14[k * LTP_ORDER + i] = (cbk_row[i] as i16) << 7;
                }
            }

            // LTP scaling
            dec_ctrl.ltp_scale_q14 = SILK_LTPSCALES_TABLE_Q14[self.indices.ltp_scale_index.clamp(0, 2) as usize] as i32;
        } else {
            dec_ctrl.pitch_l.fill(0);
            dec_ctrl.ltp_coef_q14.fill(0);
            dec_ctrl.ltp_scale_q14 = 0;
        }

        Ok(())
    }

    /// Core synthesis: LTP + LPC synthesis with complete long-term prediction
    fn decode_core(
        &mut self,
        dec_ctrl: &SilkDecoderControl,
        output: &mut [i16],
        pulses: &[i16],
    ) -> Result<(), &'static str> {
        let offset_q10 = SILK_QUANTIZATION_OFFSETS_Q10[self.indices.signal_type as usize >> 1]
            [self.indices.quant_offset_type as usize];

        let signal_type = self.indices.signal_type;
        let is_voiced = signal_type == TYPE_VOICED;

        // Decode excitation from pulses
        let mut rand_seed = self.indices.seed as i32;
        for i in 0..self.frame_length {
            rand_seed = silk_rand(rand_seed);
            let mut exc = (pulses[i] as i32) << 14;

            if exc > 0 {
                exc -= QUANT_LEVEL_ADJUST_Q10 << 4;
            } else if exc < 0 {
                exc += QUANT_LEVEL_ADJUST_Q10 << 4;
            }
            exc += (offset_q10 as i32) << 4;

            if rand_seed < 0 {
                exc = -exc;
            }

            self.exc_q14[i] = exc;
            rand_seed = rand_seed.wrapping_add(pulses[i] as i32);
        }

        // Ensure LTP buffer is large enough
        let required_ltp_len = self.ltp_mem_length + self.frame_length;
        if self.s_ltp_q15.len() < required_ltp_len {
            self.s_ltp_q15.resize(required_ltp_len, 0);
        }

        // Reset sLTP buffer index at start of frame
        self.s_ltp_buf_idx = self.ltp_mem_length;

        // Re-whitening: Apply LPC analysis filter to output buffer to initialize sLTP
        // This decorrelates the signal using the current LPC coefficients
        if is_voiced && self.prev_signal_type == TYPE_VOICED {
            let a_q12 = &dec_ctrl.pred_coef_q12[self.nb_subfr >> 1];
            let lag = dec_ctrl.pitch_l[0].max(1) as usize;
            let start_idx = self.ltp_mem_length.saturating_sub(lag + self.lpc_order + LTP_ORDER / 2);

            // Apply LPC analysis filter to historical output to get whitened residual
            for i in start_idx..self.ltp_mem_length {
                // LPC analysis: residual = sample - predicted
                let out_idx = i.saturating_sub(self.ltp_mem_length - self.frame_length);
                let out_idx = out_idx.min(self.out_buf.len().saturating_sub(1));
                let sample_q15 = (self.out_buf[out_idx] as i32) << 15;

                let mut lpc_pred_q15: i64 = 0;
                for j in 0..self.lpc_order {
                    let hist_idx = i.saturating_sub(j + 1);
                    if hist_idx < self.s_ltp_q15.len() {
                        lpc_pred_q15 += (self.s_ltp_q15[hist_idx] as i64) * (a_q12[j] as i64);
                    }
                }
                let lpc_pred_q15 = (lpc_pred_q15 >> 12) as i32;

                // Store whitened (residual) signal
                if i < self.s_ltp_q15.len() {
                    self.s_ltp_q15[i] = sample_q15 - lpc_pred_q15;
                }
            }
        }

        // Copy LPC state
        let mut slpc_q14 = [0i32; MAX_LPC_ORDER + MAX_SUB_FRAME_LENGTH];
        slpc_q14[..MAX_LPC_ORDER].copy_from_slice(&self.slpc_q14_buf);

        // Residual buffer for LTP output (Q14)
        let mut pres_q14 = vec![0i32; self.subfr_length];

        // Process subframes
        let mut pexc_idx = 0;
        let mut pxq_idx = 0;

        for k in 0..self.nb_subfr {
            let a_q12 = &dec_ctrl.pred_coef_q12[k >> 1];
            let b_q14 = &dec_ctrl.ltp_coef_q14[k * LTP_ORDER..(k + 1) * LTP_ORDER];
            let gain_q10 = dec_ctrl.gains_q16[k] >> 6;
            let lag = dec_ctrl.pitch_l[k] as usize;

            // LTP synthesis for voiced frames
            if is_voiced && lag > 0 {
                // LTP prediction using 5-tap filter
                // pred_lag_ptr points to: sLTP_Q15[sLTP_buf_idx - lag + LTP_ORDER/2]
                for i in 0..self.subfr_length {
                    // LTP prediction: weighted sum of past samples
                    // LTP_pred_Q13 = 2 + sum(B_Q14[j] * sLTP_Q15[idx - j]) >> 16 for j=0..4
                    let mut ltp_pred_q13: i64 = 2; // Rounding bias

                    // The prediction uses samples at: [idx, idx-1, idx-2, idx-3, idx-4]
                    // where idx = sLTP_buf_idx + i - lag + LTP_ORDER/2
                    let base_idx = self.s_ltp_buf_idx + i;
                    let pred_idx = if base_idx >= lag { base_idx - lag + LTP_ORDER / 2 } else { 0 };

                    for j in 0..LTP_ORDER {
                        let tap_idx = pred_idx.saturating_sub(j);
                        if tap_idx < self.s_ltp_q15.len() {
                            // silk_SMLAWB: (a + ((b * c) >> 16))
                            ltp_pred_q13 += ((self.s_ltp_q15[tap_idx] as i64) * (b_q14[j] as i64)) >> 16;
                        }
                    }

                    // Generate LPC excitation: pres_Q14 = pexc_Q14 + (LTP_pred_Q13 << 1)
                    pres_q14[i] = self.exc_q14[pexc_idx + i] + ((ltp_pred_q13 as i32) << 1);

                    // Update sLTP buffer with new residual (Q15 = pres_Q14 << 1)
                    let stp_idx = self.s_ltp_buf_idx + i;
                    if stp_idx < self.s_ltp_q15.len() {
                        self.s_ltp_q15[stp_idx] = pres_q14[i] << 1;
                    }
                }
            } else {
                // Unvoiced: excitation passes through directly
                for i in 0..self.subfr_length {
                    pres_q14[i] = self.exc_q14[pexc_idx + i];

                    // Still update sLTP buffer for continuity
                    let stp_idx = self.s_ltp_buf_idx + i;
                    if stp_idx < self.s_ltp_q15.len() {
                        self.s_ltp_q15[stp_idx] = pres_q14[i] << 1;
                    }
                }
            }

            // Update sLTP buffer index
            self.s_ltp_buf_idx += self.subfr_length;

            // LPC synthesis for this subframe
            for i in 0..self.subfr_length {
                // LPC prediction (all coefficients, order 10 or 16)
                // lpc_pred_Q10 = round(sum(a_Q12[j] * sLPC_Q14[idx-j-1]) >> 16)
                let mut lpc_pred_q10: i64 = (self.lpc_order as i64) >> 1; // Rounding

                for j in 0..self.lpc_order {
                    let idx = MAX_LPC_ORDER + i;
                    if idx > j {
                        lpc_pred_q10 += (slpc_q14[idx - j - 1] as i64 * a_q12[j] as i64) >> 16;
                    }
                }

                // Add LPC prediction to residual: sLPC_Q14 = pres_Q14 + (lpc_pred_Q10 << 4)
                slpc_q14[MAX_LPC_ORDER + i] = pres_q14[i].saturating_add((lpc_pred_q10 as i32) << 4);

                // Apply gain and convert to output
                // output = (sLPC_Q14 * gain_Q10) >> 16, then scale to Q0
                let sample_q24 = (slpc_q14[MAX_LPC_ORDER + i] as i64) * (gain_q10 as i64);
                let sample = (sample_q24 >> 16) as i32;
                output[pxq_idx + i] = sample.clamp(-32768, 32767) as i16;
            }

            // Update LPC state for next subframe
            slpc_q14.copy_within(self.subfr_length..self.subfr_length + MAX_LPC_ORDER, 0);

            pexc_idx += self.subfr_length;
            pxq_idx += self.subfr_length;
        }

        // Shift sLTP buffer: move recent samples to beginning for next frame
        if self.s_ltp_buf_idx > self.ltp_mem_length {
            let shift_amount = self.s_ltp_buf_idx - self.ltp_mem_length;
            if shift_amount < self.s_ltp_q15.len() {
                self.s_ltp_q15.copy_within(shift_amount..self.s_ltp_buf_idx, 0);
            }
        }

        // Save LPC state
        self.slpc_q14_buf.copy_from_slice(&slpc_q14[..MAX_LPC_ORDER]);

        Ok(())
    }

    /// Apply LTP analysis filter (re-whitening) for inverse filtering
    /// This converts synthesized output back to a residual signal
    #[allow(dead_code)]
    fn ltp_analysis_filter(
        &self,
        residual: &mut [i32],
        output: &[i16],
        ltp_coef_q14: &[i16],
        pitch_lag: usize,
        length: usize,
    ) {
        // Apply inverse LTP filter: residual[i] = output[i] - LTP_pred[i]
        // where LTP_pred[i] = sum(b[j] * output[i - lag + j - LTP_ORDER/2]) for j=0..4
        for i in 0..length {
            let mut ltp_pred_q13: i64 = 2; // Rounding bias

            for j in 0..LTP_ORDER {
                let tap_offset = (LTP_ORDER / 2) as i32 - (j as i32);
                let idx = (i as i32) - (pitch_lag as i32) + tap_offset;

                if idx >= 0 && (idx as usize) < output.len() {
                    let sample_q15 = (output[idx as usize] as i64) << 15;
                    ltp_pred_q13 += (sample_q15 * (ltp_coef_q14[j] as i64)) >> 16;
                }
            }

            // residual = output - LTP_prediction
            let output_q14 = (output[i] as i32) << 14;
            residual[i] = output_q14 - ((ltp_pred_q13 as i32) << 1);
        }
    }
}

// Helper: SILK pseudo-random number generator
fn silk_rand(seed: i32) -> i32 {
    seed.wrapping_mul(196314165).wrapping_add(907633515)
}

// Entropy coding tables (simplified versions)
static SILK_TYPE_OFFSET_NO_VAD_ICDF: [u8; 2] = [184, 0];
static SILK_DELTA_GAIN_ICDF: [u8; 41] = [
    250, 245, 234, 203, 71, 50, 42, 38, 35, 33, 31, 29, 28, 27, 26, 25, 24, 23, 22, 21,
    20, 19, 18, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0,
];
static SILK_GAIN_ICDF: [[u8; 8]; 3] = [
    [224, 112, 44, 15, 3, 2, 1, 0],
    [224, 112, 44, 15, 3, 2, 1, 0],
    [224, 112, 44, 15, 3, 2, 1, 0],
];
static SILK_UNIFORM4_ICDF: [u8; 4] = [192, 128, 64, 0];
static SILK_UNIFORM8_ICDF: [u8; 8] = [224, 192, 160, 128, 96, 64, 32, 0];
static SILK_NLSF_INTERPOLATION_FACTOR_ICDF: [u8; 5] = [243, 221, 192, 181, 0];
static SILK_PITCH_LAG_ICDF: [u8; 32] = [
    252, 250, 244, 237, 229, 219, 207, 194, 182, 168, 154, 140, 126, 113, 100, 88,
    76, 65, 55, 46, 38, 31, 25, 20, 15, 11, 8, 5, 3, 2, 1, 0,
];
static SILK_PITCH_CONTOUR_ICDF: [u8; 34] = [
    252, 250, 244, 237, 229, 221, 212, 202, 192, 182, 170, 158, 146, 134, 122, 110,
    98, 87, 77, 67, 58, 49, 41, 34, 28, 22, 17, 13, 9, 6, 4, 2, 1, 0,
];
static SILK_LTP_PER_INDEX_ICDF: [u8; 3] = [179, 99, 0];

// LTP gain iCDF tables for each periodicity index (3 codebooks)
static SILK_LTP_GAIN_ICDF_0: [u8; 8] = [71, 56, 43, 30, 21, 12, 6, 0];
static SILK_LTP_GAIN_ICDF_1: [u8; 16] = [
    199, 165, 144, 124, 109, 96, 84, 71, 61, 51, 42, 32, 23, 15, 8, 0,
];
static SILK_LTP_GAIN_ICDF_2: [u8; 32] = [
    241, 225, 211, 199, 187, 175, 164, 153, 142, 132, 123, 114, 105, 96, 88, 80, 72, 64, 57, 50,
    44, 38, 33, 29, 24, 20, 16, 12, 9, 5, 2, 0,
];

// LTP VQ codebooks (Q7 format) - 5 coefficients per entry
// Codebook 0: 8 entries
static SILK_LTP_VQ_Q7_0: [[i8; 5]; 8] = [
    [4, 6, 24, 7, 5],
    [0, 0, 2, 0, 0],
    [12, 28, 41, 13, -4],
    [-9, 15, 42, 25, 14],
    [1, -2, 62, 41, -9],
    [-10, 37, 65, -4, 3],
    [-6, 4, 66, 7, -8],
    [16, 14, 38, -3, 33],
];

// Codebook 1: 16 entries
static SILK_LTP_VQ_Q7_1: [[i8; 5]; 16] = [
    [13, 22, 39, 23, 12],
    [-1, 36, 64, 27, -6],
    [-7, 10, 55, 43, 17],
    [1, 1, 8, 1, 1],
    [6, -11, 74, 53, -9],
    [-12, 55, 76, -12, 8],
    [-3, 3, 93, 27, -4],
    [26, 39, 59, 3, -8],
    [2, 0, 77, 11, 9],
    [-8, 22, 44, -6, 7],
    [40, 9, 26, 3, 9],
    [-7, 20, 101, -7, 4],
    [3, -8, 42, 26, 0],
    [-15, 33, 68, 2, 23],
    [-2, 55, 46, -2, 15],
    [3, -1, 21, 16, 41],
];

// Codebook 2: 32 entries
static SILK_LTP_VQ_Q7_2: [[i8; 5]; 32] = [
    [-6, 27, 61, 39, 5],
    [-11, 42, 88, 4, 1],
    [-2, 60, 65, 6, -4],
    [-1, -5, 73, 56, 1],
    [-9, 19, 94, 29, -9],
    [0, 12, 99, 6, 4],
    [8, -19, 102, 46, -13],
    [3, 2, 13, 3, 2],
    [9, -21, 84, 72, -18],
    [-11, 46, 104, -22, 8],
    [18, 38, 48, 23, 0],
    [-16, 70, 83, -21, 11],
    [5, -11, 117, 22, -8],
    [-6, 23, 117, -12, 3],
    [3, -8, 95, 28, 4],
    [-10, 15, 77, 60, -15],
    [-1, 4, 124, 2, -4],
    [3, 38, 84, 24, -25],
    [2, 13, 42, 13, 31],
    [21, -4, 56, 46, -1],
    [-1, 35, 79, -13, 19],
    [-7, 65, 88, -9, -14],
    [20, 4, 81, 49, -29],
    [20, 0, 75, 3, -17],
    [5, -9, 44, 92, -8],
    [1, -3, 22, 69, 31],
    [-6, 95, 41, -12, 5],
    [39, 67, 16, -4, 1],
    [0, -6, 120, 55, -36],
    [-13, 44, 122, 4, -24],
    [81, 5, 11, 3, 7],
    [2, 0, 9, 10, 88],
];

// Codebook sizes for each periodicity index
static SILK_LTP_VQ_SIZES: [usize; 3] = [8, 16, 32];

// Pitch lag contour tables for 4 subframes (20ms frame)
// silk_CB_lags_stage2 - 4 x 11 (contour refinement)
static SILK_CB_LAGS_STAGE2: [[i8; 11]; 4] = [
    [0, 2, -1, -1, -1, 0, 0, 1, 1, 0, 1],
    [0, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0],
    [0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0],
    [0, -1, 2, 1, 0, 1, 1, 0, 0, -1, -1],
];

// silk_CB_lags_stage3 - 4 x 34 (fine contour)
static SILK_CB_LAGS_STAGE3: [[i8; 34]; 4] = [
    [
        0, 0, 1, -1, 0, 1, -1, 0, -1, 1, -2, 2, -2, -2, 2, -3, 2, 3, -3, -4, 3, -4, 4, 4, -5, 5,
        -6, -5, 6, -7, 6, 5, 8, -9,
    ],
    [
        0, 0, 1, 0, 0, 0, 0, 0, 0, 0, -1, 1, 0, 0, 1, -1, 0, 1, -1, -1, 1, -1, 2, 1, -1, 2, -2,
        -2, 2, -2, 2, 2, 3, -3,
    ],
    [
        0, 1, 0, 0, 0, 0, 0, 0, 1, 0, 1, 0, 0, 1, -1, 1, 0, 0, 2, 1, -1, 2, -1, -1, 2, -1, 2, 2,
        -1, 3, -2, -2, -2, 3,
    ],
    [
        0, 1, 0, 0, 1, 0, 1, -1, 2, -1, 2, -1, 2, 3, -2, 3, -2, -2, 4, 4, -3, 5, -3, -4, 6, -4, 6,
        5, -5, 8, -6, -5, -7, 9,
    ],
];

// Pitch contour tables for 2 subframes (10ms frame / NB)
static SILK_CB_LAGS_STAGE2_10_MS: [[i8; 3]; 2] = [[0, 1, 0], [0, 0, 1]];

static SILK_CB_LAGS_STAGE3_10_MS: [[i8; 12]; 2] = [
    [0, 0, 1, -1, 1, -1, 2, -2, 2, -2, 3, -3],
    [0, 1, 0, 1, -1, 2, -1, 2, -2, 3, -2, 3],
];

// Pitch lag limits per sample rate
const PITCH_LAG_MIN_NB: i32 = 16; // 8 kHz
const PITCH_LAG_MAX_NB: i32 = 144;
const PITCH_LAG_MIN_MB: i32 = 24; // 12 kHz
const PITCH_LAG_MAX_MB: i32 = 216;
const PITCH_LAG_MIN_WB: i32 = 32; // 16 kHz
const PITCH_LAG_MAX_WB: i32 = 288;

static SILK_LTPSCALE_ICDF: [u8; 3] = [128, 64, 0];

/// Decode pitch lags from indices using contour tables
fn decode_pitch(
    lag_index: i16,
    contour_index: i8,
    pitch_lags: &mut [i32; MAX_NB_SUBFR],
    fs_khz: i32,
    nb_subfr: usize,
) {
    // Get min lag based on sample rate
    let min_lag = match fs_khz {
        8 => PITCH_LAG_MIN_NB,
        12 => PITCH_LAG_MIN_MB,
        _ => PITCH_LAG_MIN_WB,
    };

    // Base lag from index
    let lag = min_lag + (lag_index as i32);
    let contour_idx = contour_index.max(0) as usize;

    if nb_subfr == 2 {
        // 10ms frame (NB) - use 2-subframe tables
        let contour_idx_clamped = contour_idx.min(SILK_CB_LAGS_STAGE3_10_MS[0].len() - 1);
        for k in 0..2 {
            pitch_lags[k] = lag + (SILK_CB_LAGS_STAGE3_10_MS[k][contour_idx_clamped] as i32);
        }
    } else {
        // 20ms frame - use 4-subframe tables
        let contour_idx_clamped = contour_idx.min(SILK_CB_LAGS_STAGE3[0].len() - 1);
        for k in 0..4 {
            pitch_lags[k] = lag + (SILK_CB_LAGS_STAGE3[k][contour_idx_clamped] as i32);
        }
    }

    // Clamp pitch lags to valid range
    let max_lag = match fs_khz {
        8 => PITCH_LAG_MAX_NB,
        12 => PITCH_LAG_MAX_MB,
        _ => PITCH_LAG_MAX_WB,
    };

    for k in 0..nb_subfr {
        pitch_lags[k] = pitch_lags[k].clamp(min_lag, max_lag);
    }
}
static SILK_RATE_LEVELS_ICDF: [[u8; 9]; 2] = [
    [250, 245, 234, 203, 71, 50, 42, 38, 0],
    [250, 245, 234, 203, 71, 50, 42, 38, 0],
];
static SILK_PULSES_PER_BLOCK_ICDF: [[u8; 18]; N_RATE_LEVELS] = [
    [125, 51, 26, 18, 15, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0],
    [198, 105, 45, 22, 15, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0],
    [213, 162, 116, 83, 59, 43, 32, 24, 18, 15, 12, 9, 7, 6, 5, 3, 2, 0],
    [239, 187, 116, 59, 28, 16, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0],
    [250, 229, 188, 135, 86, 51, 30, 19, 13, 10, 8, 6, 5, 4, 3, 2, 1, 0],
    [249, 235, 213, 185, 156, 128, 103, 83, 66, 53, 42, 33, 26, 21, 17, 13, 10, 0],
    [254, 249, 235, 206, 164, 118, 77, 46, 27, 16, 10, 7, 5, 4, 3, 2, 1, 0],
    [255, 253, 249, 239, 220, 191, 156, 119, 85, 57, 37, 23, 15, 10, 6, 4, 2, 0],
    [255, 253, 251, 246, 237, 223, 203, 179, 152, 124, 98, 75, 55, 40, 29, 21, 15, 0],
    [255, 254, 253, 247, 220, 162, 106, 67, 42, 28, 18, 12, 9, 6, 4, 3, 2, 0],
];

// Quantization offset table
static SILK_QUANTIZATION_OFFSETS_Q10: [[i16; 2]; 2] = [
    [OFFSET_UVL_Q10 as i16, OFFSET_UVH_Q10 as i16], // Unvoiced
    [OFFSET_VL_Q10 as i16, OFFSET_VH_Q10 as i16],   // Voiced
];

// LTP scale table
static SILK_LTPSCALES_TABLE_Q14: [i16; 3] = [15565, 12288, 8192];

// ============================================================================
// Unit Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nlsf_codebook_nb_mb_structure() {
        // Test that the NB/MB codebook has correct dimensions
        let cb = &SILK_NLSF_CB_NB_MB;
        assert_eq!(cb.n_vectors, 32);
        assert_eq!(cb.order, 10);
        assert_eq!(cb.cb1_nlsf_q8.len(), 320); // 32 * 10
        assert_eq!(cb.cb1_wght_q9.len(), 320);
        assert_eq!(cb.cb1_icdf.len(), 64); // 2 * 32 (voiced/unvoiced)
        assert_eq!(cb.pred_q8.len(), 18); // 2 * (order - 1)
        assert_eq!(cb.ec_sel.len(), 160); // 32 * 10 / 2
        assert_eq!(cb.ec_icdf.len(), 72); // 8 * 9 (8 entropy tables)
        assert_eq!(cb.delta_min_q15.len(), 11); // order + 1
    }

    #[test]
    fn test_nlsf_codebook_wb_structure() {
        // Test that the WB codebook has correct dimensions
        let cb = &SILK_NLSF_CB_WB;
        assert_eq!(cb.n_vectors, 32);
        assert_eq!(cb.order, 16);
        assert_eq!(cb.cb1_nlsf_q8.len(), 512); // 32 * 16
        assert_eq!(cb.cb1_wght_q9.len(), 512);
        assert_eq!(cb.cb1_icdf.len(), 64); // 2 * 32
        assert_eq!(cb.pred_q8.len(), 30); // 2 * (order - 1)
        assert_eq!(cb.ec_sel.len(), 256); // 32 * 16 / 2
        assert_eq!(cb.ec_icdf.len(), 72);
        assert_eq!(cb.delta_min_q15.len(), 17); // order + 1
    }

    #[test]
    fn test_nlsf_unpack() {
        let cb = &SILK_NLSF_CB_NB_MB;
        let mut ec_ix = [0i16; 10];
        let mut pred_q8 = [0u8; 10];

        // Test unpacking for CB1 index 0
        silk_nlsf_unpack(&mut ec_ix, &mut pred_q8, cb, 0);

        // Verify entropy indices are within valid range
        for i in 0..10 {
            assert!(ec_ix[i] >= 0);
            assert!(ec_ix[i] < 72); // Should be < ec_icdf.len()
        }

        // Verify predictor values are non-zero (from pred_q8 table)
        let mut has_nonzero = false;
        for i in 0..10 {
            if pred_q8[i] != 0 {
                has_nonzero = true;
            }
        }
        assert!(has_nonzero, "Predictor should have some non-zero values");
    }

    #[test]
    fn test_nlsf_decode_simple() {
        let cb = &SILK_NLSF_CB_NB_MB;
        let mut nlsf_q15 = [0i16; 10];

        // Simple test with all-zero indices (CB1=0, residuals=0)
        let indices: [i8; 11] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

        silk_nlsf_decode(&mut nlsf_q15, &indices, cb);

        // Verify NLSFs are in ascending order
        for i in 1..10 {
            assert!(nlsf_q15[i] >= nlsf_q15[i - 1], "NLSFs should be in ascending order");
        }

        // Verify NLSFs are within valid range [0, 32767]
        for i in 0..10 {
            assert!(nlsf_q15[i] >= 0, "NLSF should be >= 0");
            assert!(nlsf_q15[i] <= 32767, "NLSF should be <= 32767");
        }
    }

    #[test]
    fn test_nlsf_stabilize() {
        let mut nlsf_q15: [i16; 10] = [1000, 2000, 3000, 4000, 5000, 6000, 7000, 8000, 9000, 10000];
        let delta_min = &SILK_NLSF_DELTA_MIN_NB_MB_Q15;

        silk_nlsf_stabilize(&mut nlsf_q15, delta_min, 10);

        // Check that minimum distances are maintained
        assert!(nlsf_q15[0] >= delta_min[0], "First NLSF should be >= delta_min[0]");

        for i in 1..10 {
            let diff = nlsf_q15[i] - nlsf_q15[i - 1];
            assert!(diff >= delta_min[i], "NLSF difference should be >= delta_min");
        }

        let last_gap = 32768 - nlsf_q15[9] as i32;
        assert!(last_gap >= delta_min[10] as i32, "Last gap should be >= delta_min[10]");
    }

    #[test]
    fn test_nlsf2a_order10() {
        // Test NLSF to LPC conversion for order 10
        // Use some known NLSF values (linearly spaced)
        let nlsf_q15: [i16; 10] = [3277, 6554, 9830, 13107, 16384, 19661, 22938, 26214, 29491, 31768];
        let mut a_q12 = [0i16; 10];

        silk_nlsf2a(&mut a_q12, &nlsf_q15, 10);

        // The LPC coefficients should not all be zero
        let mut has_nonzero = false;
        for i in 0..10 {
            if a_q12[i] != 0 {
                has_nonzero = true;
            }
        }
        assert!(has_nonzero, "LPC coefficients should have some non-zero values");

        // LPC coefficients should be within reasonable bounds for Q12
        for i in 0..10 {
            assert!(a_q12[i] > -8192 && a_q12[i] < 8192, "LPC coef {} out of range: {}", i, a_q12[i]);
        }
    }

    #[test]
    fn test_nlsf2a_order16() {
        // Test NLSF to LPC conversion for order 16
        let nlsf_q15: [i16; 16] = [
            2048, 4096, 6144, 8192, 10240, 12288, 14336, 16384,
            18432, 20480, 22528, 24576, 26624, 28672, 30720, 31744
        ];
        let mut a_q12 = [0i16; 16];

        silk_nlsf2a(&mut a_q12, &nlsf_q15, 16);

        // The LPC coefficients should not all be zero
        let mut has_nonzero = false;
        for i in 0..16 {
            if a_q12[i] != 0 {
                has_nonzero = true;
            }
        }
        assert!(has_nonzero, "LPC coefficients should have some non-zero values");
    }

    #[test]
    fn test_lsf_cos_table() {
        // Test the cosine table values
        assert_eq!(SILK_LSF_COS_TAB_FIX_Q12[0], 8192); // cos(0) * 4096 * 2
        assert_eq!(SILK_LSF_COS_TAB_FIX_Q12[64], 0);   // cos(pi/2) * 4096 * 2 = 0
        assert_eq!(SILK_LSF_COS_TAB_FIX_Q12[128], -8192); // cos(pi) * 4096 * 2 = -8192
    }

    #[test]
    fn test_silk_decoder_creation() {
        // Test decoder creation for different sample rates
        let decoder_8k = SilkDecoder::new(8000);
        assert_eq!(decoder_8k.fs_khz, 8);
        assert_eq!(decoder_8k.lpc_order, 10);
        assert_eq!(decoder_8k.nb_subfr, 2);
        assert_eq!(decoder_8k.nlsf_cb.order, 10);

        let decoder_12k = SilkDecoder::new(12000);
        assert_eq!(decoder_12k.fs_khz, 12);
        assert_eq!(decoder_12k.lpc_order, 10);
        assert_eq!(decoder_12k.nb_subfr, 4);
        assert_eq!(decoder_12k.nlsf_cb.order, 10);

        let decoder_16k = SilkDecoder::new(16000);
        assert_eq!(decoder_16k.fs_khz, 16);
        assert_eq!(decoder_16k.lpc_order, 16);
        assert_eq!(decoder_16k.nb_subfr, 4);
        assert_eq!(decoder_16k.nlsf_cb.order, 16);
    }

    #[test]
    fn test_fixed_point_helpers() {
        // Test rshift_round
        assert_eq!(rshift_round(100, 0), 100);
        assert_eq!(rshift_round(100, 1), 50);
        assert_eq!(rshift_round(101, 1), 51); // rounds up
        assert_eq!(rshift_round(-100, 1), -50);

        // Test rshift_round64
        assert_eq!(rshift_round64(100, 0), 100);
        assert_eq!(rshift_round64(100, 1), 50);
        assert_eq!(rshift_round64(0x100000000i64, 32), 1);

        // Test smulww
        assert_eq!(smulww(65536, 65536), 65536); // 1.0 * 1.0 = 1.0 in Q16

        // Test smmul
        assert_eq!(smmul(0x40000000, 0x40000000), 0x10000000); // 0.5 * 0.5 = 0.25 in Q30
    }

    #[test]
    fn test_ltp_vq_codebooks() {
        // Test that LTP VQ codebooks have correct dimensions
        assert_eq!(SILK_LTP_VQ_Q7_0.len(), 8);
        assert_eq!(SILK_LTP_VQ_Q7_1.len(), 16);
        assert_eq!(SILK_LTP_VQ_Q7_2.len(), 32);

        // Each entry has 5 coefficients
        for entry in &SILK_LTP_VQ_Q7_0 {
            assert_eq!(entry.len(), LTP_ORDER);
        }
        for entry in &SILK_LTP_VQ_Q7_1 {
            assert_eq!(entry.len(), LTP_ORDER);
        }
        for entry in &SILK_LTP_VQ_Q7_2 {
            assert_eq!(entry.len(), LTP_ORDER);
        }

        // Verify codebook sizes match
        assert_eq!(SILK_LTP_VQ_SIZES[0], SILK_LTP_VQ_Q7_0.len());
        assert_eq!(SILK_LTP_VQ_SIZES[1], SILK_LTP_VQ_Q7_1.len());
        assert_eq!(SILK_LTP_VQ_SIZES[2], SILK_LTP_VQ_Q7_2.len());
    }

    #[test]
    fn test_ltp_gain_icdf() {
        // Test that LTP gain iCDF tables have correct sizes
        assert_eq!(SILK_LTP_GAIN_ICDF_0.len(), 8);
        assert_eq!(SILK_LTP_GAIN_ICDF_1.len(), 16);
        assert_eq!(SILK_LTP_GAIN_ICDF_2.len(), 32);

        // iCDF tables should end with 0
        assert_eq!(SILK_LTP_GAIN_ICDF_0[7], 0);
        assert_eq!(SILK_LTP_GAIN_ICDF_1[15], 0);
        assert_eq!(SILK_LTP_GAIN_ICDF_2[31], 0);
    }

    #[test]
    fn test_decode_pitch() {
        let mut pitch_lags = [0i32; MAX_NB_SUBFR];

        // Test 8kHz (NB) - 2 subframes
        decode_pitch(0, 0, &mut pitch_lags, 8, 2);
        assert!(pitch_lags[0] >= PITCH_LAG_MIN_NB);
        assert!(pitch_lags[0] <= PITCH_LAG_MAX_NB);
        assert!(pitch_lags[1] >= PITCH_LAG_MIN_NB);
        assert!(pitch_lags[1] <= PITCH_LAG_MAX_NB);

        // Test 16kHz (WB) - 4 subframes
        decode_pitch(50, 5, &mut pitch_lags, 16, 4);
        for k in 0..4 {
            assert!(pitch_lags[k] >= PITCH_LAG_MIN_WB);
            assert!(pitch_lags[k] <= PITCH_LAG_MAX_WB);
        }
    }

    #[test]
    fn test_pitch_contour_tables() {
        // Test that pitch contour tables have correct dimensions
        assert_eq!(SILK_CB_LAGS_STAGE3.len(), 4);
        assert_eq!(SILK_CB_LAGS_STAGE3[0].len(), 34);

        assert_eq!(SILK_CB_LAGS_STAGE3_10_MS.len(), 2);
        assert_eq!(SILK_CB_LAGS_STAGE3_10_MS[0].len(), 12);

        // First entries should be 0 (base lag unchanged)
        assert_eq!(SILK_CB_LAGS_STAGE3[0][0], 0);
        assert_eq!(SILK_CB_LAGS_STAGE3[1][0], 0);
        assert_eq!(SILK_CB_LAGS_STAGE3_10_MS[0][0], 0);
        assert_eq!(SILK_CB_LAGS_STAGE3_10_MS[1][0], 0);
    }

    #[test]
    fn test_ltp_coef_q7_to_q14_conversion() {
        // Test that Q7 to Q14 conversion works correctly
        // Q7 value of 64 should become 64 << 7 = 8192 in Q14
        let coef_q7: i8 = 64;
        let coef_q14 = (coef_q7 as i16) << 7;
        assert_eq!(coef_q14, 8192);

        // Q7 value of -64 should become -64 << 7 = -8192 in Q14
        let coef_q7_neg: i8 = -64;
        let coef_q14_neg = (coef_q7_neg as i16) << 7;
        assert_eq!(coef_q14_neg, -8192);
    }

    #[test]
    fn test_silk_decoder_ltp_buffer() {
        // Test that decoder has properly sized LTP buffer
        let decoder = SilkDecoder::new(16000);
        let expected_ltp_len = decoder.ltp_mem_length + decoder.frame_length;
        assert_eq!(decoder.s_ltp_q15.len(), expected_ltp_len);
        assert_eq!(decoder.s_ltp_buf_idx, decoder.ltp_mem_length);
    }

    #[test]
    fn test_silk_decoder_reset_clears_ltp() {
        let mut decoder = SilkDecoder::new(16000);

        // Modify LTP buffer
        decoder.s_ltp_q15[0] = 12345;
        decoder.s_ltp_buf_idx = 100;

        // Reset should clear buffer
        decoder.reset();
        assert_eq!(decoder.s_ltp_q15[0], 0);
        assert_eq!(decoder.s_ltp_buf_idx, decoder.ltp_mem_length);
    }
}
