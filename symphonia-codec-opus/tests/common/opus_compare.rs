// Faithful Rust port of libopus `src/opus_compare.c`, restricted to 48 kHz (mono or stereo),
// which is all the RFC 8251 conformance harness needs.
// Ported from libopus (BSD-3-Clause), see ../../NOTICE.
#![allow(clippy::needless_range_loop)]

const NBANDS: usize = 21;
const NFREQS: usize = 240;
const BANDS: [usize; NBANDS + 1] =
    [0, 2, 4, 6, 8, 10, 12, 14, 16, 20, 24, 28, 32, 40, 48, 56, 68, 80, 96, 120, 156, 200];
const TEST_WIN_SIZE: usize = 480;
const TEST_WIN_STEP: usize = 120;

/// C: `band_energy`. `out` may be empty to skip the per-band-averaged output (mirrors passing
/// `NULL` for `_out` in the reference for the `Y` computation).
#[allow(clippy::too_many_arguments)]
fn band_energy(
    out: Option<&mut [f32]>,
    ps: &mut [f32],
    bands: &[usize],
    nbands: usize,
    input: &[f32],
    channels: usize,
    nframes: usize,
    window_sz: usize,
    step: usize,
    downsample: usize,
) {
    let mut window = vec![0f32; window_sz];
    let mut c = vec![0f32; window_sz];
    let mut s = vec![0f32; window_sz];
    let mut x = vec![0f32; channels * window_sz];
    let ps_sz = window_sz / 2;

    for xj in 0..window_sz {
        window[xj] = 0.5 - 0.5 * (2.0 * std::f32::consts::PI / (window_sz as f32 - 1.0) * xj as f32).cos();
    }
    for xj in 0..window_sz {
        c[xj] = (2.0 * std::f32::consts::PI / window_sz as f32 * xj as f32).cos();
    }
    for xj in 0..window_sz {
        s[xj] = (2.0 * std::f32::consts::PI / window_sz as f32 * xj as f32).sin();
    }

    let mut out = out;

    for xi in 0..nframes {
        for ci in 0..channels {
            for xk in 0..window_sz {
                x[ci * window_sz + xk] = window[xk] * input[(xi * step + xk) * channels + ci];
            }
        }
        let mut xj = 0usize;
        for bi in 0..nbands {
            let mut p = [0f32; 2];
            while xj < bands[bi + 1] {
                for ci in 0..channels {
                    let mut re = 0f32;
                    let mut im = 0f32;
                    let mut ti = 0usize;
                    for xk in 0..window_sz {
                        re += c[ti] * x[ci * window_sz + xk];
                        im -= s[ti] * x[ci * window_sz + xk];
                        ti += xj;
                        if ti >= window_sz {
                            ti -= window_sz;
                        }
                    }
                    re *= downsample as f32;
                    im *= downsample as f32;
                    let v = re * re + im * im + 100000.0;
                    ps[(xi * ps_sz + xj) * channels + ci] = v;
                    p[ci] += v;
                }
                xj += 1;
            }
            if let Some(out) = out.as_deref_mut() {
                out[(xi * nbands + bi) * channels] = p[0] / (bands[bi + 1] - bands[bi]) as f32;
                if channels == 2 {
                    out[(xi * nbands + bi) * channels + 1] = p[1] / (bands[bi + 1] - bands[bi]) as f32;
                }
            }
        }
    }
}

/// The result of [`compare`]: whether the vector passes and the internal weighted-error metric.
#[derive(Debug, Clone, Copy)]
pub struct CompareResult {
    pub pass: bool,
    pub quality_percent: f64,
    pub weighted_error: f64,
}

/// C: `main` of `opus_compare.c`, minus file I/O (`x`/`y` are already-decoded interleaved
/// `i16`-range `f32` PCM at 48 kHz). `channels` is 1 or 2 for both `x` and `y` (unlike the C CLI,
/// which always reads `x` as stereo and optionally downmixes — callers here pass pre-downmixed
/// mono `x` samples when `channels == 1`, matching `.dec`/`m.dec` conventions).
pub fn compare(x: &[f32], y: &[f32], channels: usize) -> CompareResult {
    assert!(channels == 1 || channels == 2);
    let xlength = x.len() / channels;
    let ylength = y.len() / channels;
    assert_eq!(xlength, ylength, "sample counts do not match ({xlength} != {ylength})");
    assert!(xlength >= TEST_WIN_SIZE, "insufficient sample data ({xlength} < {TEST_WIN_SIZE})");

    let nframes = (xlength - TEST_WIN_SIZE + TEST_WIN_STEP) / TEST_WIN_STEP;
    let ybands = NBANDS;
    let yfreqs = NFREQS;

    let mut xb = vec![0f32; nframes * NBANDS * channels];
    let mut xfreq = vec![0f32; nframes * NFREQS * channels];
    let mut yfreq = vec![0f32; nframes * yfreqs * channels];

    band_energy(Some(&mut xb), &mut xfreq, &BANDS, NBANDS, x, channels, nframes, TEST_WIN_SIZE, TEST_WIN_STEP, 1);
    band_energy(None, &mut yfreq, &BANDS, ybands, y, channels, nframes, TEST_WIN_SIZE, TEST_WIN_STEP, 1);

    for xi in 0..nframes {
        for bi in 1..NBANDS {
            for ci in 0..channels {
                let add = 0.1 * xb[(xi * NBANDS + bi - 1) * channels + ci];
                xb[(xi * NBANDS + bi) * channels + ci] += add;
            }
        }
        for bi in (0..NBANDS - 1).rev() {
            for ci in 0..channels {
                let add = 0.03 * xb[(xi * NBANDS + bi + 1) * channels + ci];
                xb[(xi * NBANDS + bi) * channels + ci] += add;
            }
        }
        if xi > 0 {
            for bi in 0..NBANDS {
                for ci in 0..channels {
                    let add = 0.5 * xb[((xi - 1) * NBANDS + bi) * channels + ci];
                    xb[(xi * NBANDS + bi) * channels + ci] += add;
                }
            }
        }
        if channels == 2 {
            for bi in 0..NBANDS {
                let l = xb[(xi * NBANDS + bi) * channels];
                let r = xb[(xi * NBANDS + bi) * channels + 1];
                xb[(xi * NBANDS + bi) * channels] += 0.01 * r;
                xb[(xi * NBANDS + bi) * channels + 1] += 0.01 * l;
            }
        }

        for bi in 0..ybands {
            for xj in BANDS[bi]..BANDS[bi + 1] {
                for ci in 0..channels {
                    let m = 0.1 * xb[(xi * NBANDS + bi) * channels + ci];
                    xfreq[(xi * NFREQS + xj) * channels + ci] += m;
                    yfreq[(xi * yfreqs + xj) * channels + ci] += m;
                }
            }
        }
    }

    for bi in 0..ybands {
        for xj in BANDS[bi]..BANDS[bi + 1] {
            for ci in 0..channels {
                let mut xtmp = xfreq[xj * channels + ci];
                let mut ytmp = yfreq[xj * channels + ci];
                for xi in 1..nframes {
                    let xtmp2 = xfreq[(xi * NFREQS + xj) * channels + ci];
                    let ytmp2 = yfreq[(xi * yfreqs + xj) * channels + ci];
                    xfreq[(xi * NFREQS + xj) * channels + ci] += xtmp;
                    yfreq[(xi * yfreqs + xj) * channels + ci] += ytmp;
                    xtmp = xtmp2;
                    ytmp = ytmp2;
                }
            }
        }
    }

    // 48 kHz only: compare every band, no truncation.
    let max_compare = BANDS[NBANDS];

    let mut err = 0f64;
    for xi in 0..nframes {
        let mut ef = 0f64;
        for bi in 0..ybands {
            let mut eb = 0f64;
            for xj in BANDS[bi]..BANDS[bi + 1].min(max_compare) {
                for ci in 0..channels {
                    let re = yfreq[(xi * yfreqs + xj) * channels + ci] / xfreq[(xi * NFREQS + xj) * channels + ci];
                    let mut im = (re - re.ln() - 1.0) as f64;
                    if (79..=81).contains(&xj) {
                        im *= 0.1;
                    }
                    eb += im;
                }
            }
            eb /= ((BANDS[bi + 1] - BANDS[bi]) * channels) as f64;
            ef += eb * eb;
        }
        ef /= NBANDS as f64;
        ef *= ef;
        err += ef * ef;
    }

    err = (err / nframes as f64).powf(1.0 / 16.0);
    let q = 100.0 * (1.0 - 0.5 * (1.0 + err).ln() / 1.13f64.ln());

    CompareResult { pass: q >= 0.0, quality_percent: q, weighted_error: err }
}
