// CIC (Cascaded Integrator-Comb) Decimation Filter
// Efficient multi-stage decimation for DSD-to-PCM conversion

use symphonia_core::codecs::audio::BitOrder;

/// Maximum number of CIC stages.
const MAX_STAGES: usize = 8;

/// 4-bit reversal.
const REVERSE_NIBBLE: [u8; 16] = [0, 8, 4, 12, 2, 10, 6, 14, 1, 9, 5, 13, 3, 11, 7, 15];

/// CIC filter for decimating a 1-bit DSD stream
///
/// A CIC filter consists of integrator stages followed by decimation and then comb stages. It
/// requires no multiplications.
///
/// # Theory
/// CIC filters accumulate (integrate) input samples, then decimate by R, then differentiate (comb)
/// the result. Multiple stages increase the filtering order, improving stopband attenuation.
///
/// # Gain Compensation
/// This implementation assumes a differential delay (M) of 1. The CIC filter gain is calculated as
/// `R^N` where:
/// - R = decimation ratio
/// - N = number of stages
///
/// For the general case with M ≠ 1, the gain would be `(RM)^N`.
/// See: [Intel AN455: Understanding CIC Compensation Filters](https://cdrdv2-public.intel.com/653906/an455.pdf)
///
/// The frequency response is: H(f) = [sin(πRMf)/sin(πf)]^N
///
/// # Implementation
/// The input of a DSD filter is a sequence of ±1 values, and a CIC filter with zero initial state
/// is exactly a FIR filter whose impulse response `h` is the N-fold convolution of a length-R
/// boxcar, evaluated every R input samples. All arithmetic is exact integer arithmetic, so the
/// FIR formulation is bit-identical to the classic integrator/comb cascade.
///
/// When R is a multiple of 4 the FIR is evaluated directly on the packed bits with a table of
/// partial sums: for every nibble (4 bits) position in the `N * R` bit window, the table holds
/// the sum of `±h[k]` over the four bits for each of the 16 possible nibble values. An output is
/// then `N * R / 4` table lookups and additions, with no per-bit work and no unpacking of the
/// bits to floating point. Other ratios fall back to a bit-serial integrator/comb cascade.
pub struct CicFilter {
    /// Decimation ratio
    decimation: usize,
    /// Number of stages (typically 3-5)
    stages: usize,
    /// Gain of the filter, `decimation^stages`.
    gain: f64,
    /// Nibble-lookup state, or `None` if the bit-serial fallback is used.
    poly: Option<Polyphase>,
    /// Bit-serial integrator state (one per stage), fallback only.
    integrators: [i64; MAX_STAGES],
    /// Bit-serial comb state (one per stage, each holds previous value), fallback only.
    combs: [i64; MAX_STAGES],
    /// Bit-serial sample counter for decimation, fallback only.
    sample_count: usize,
}

/// State of the table-driven polyphase evaluation (decimation a multiple of 4).
struct Polyphase {
    /// Nibbles per output block, `decimation / 4`.
    block: usize,
    /// Nibbles in the FIR window, `stages * block`.
    window: usize,
    /// Partial sums. Entry `[i][v]` is the contribution of nibble value `v` located at position
    /// `i` of the window, where position 0 is the oldest nibble and `window - 1` the newest.
    lut: Vec<[i64; 16]>,
    /// Impulse response of the filter (`stages * (decimation - 1) + 1` taps, newest sample first).
    taps: Vec<i64>,
    /// The most recent nibbles (at most `window`) in time order. Within a nibble, bit `j` is the
    /// sample at time `j` (earliest first), irrespective of the bit order of the input.
    history: Vec<u8>,
    /// Scratch buffer: `history` followed by the nibbles of the current call.
    scratch: Vec<u8>,
    /// Nibbles received since the last output, `< block`.
    phase: usize,
}

impl Polyphase {
    fn new(decimation: usize, stages: usize) -> Self {
        let block = decimation / 4;
        let mut poly = Polyphase {
            block,
            window: stages * block,
            lut: Vec::new(),
            taps: boxcar_power(decimation, stages),
            history: Vec::new(),
            scratch: Vec::new(),
            phase: 0,
        };
        poly.build_lut();
        poly
    }

    /// Build the nibble tables.
    fn build_lut(&mut self) {
        let window = self.window;
        self.lut.reserve(window);

        for pos in 0..window {
            // Number of nibbles newer than this one.
            let age = window - 1 - pos;
            let mut entry = [0i64; 16];

            for (value, slot) in entry.iter_mut().enumerate() {
                let mut sum = 0i64;
                for bit in 0..4 {
                    // Number of samples newer than this bit (bit `bit` is at time `bit`).
                    let lag = age * 4 + (3 - bit);
                    let tap = self.taps.get(lag).copied().unwrap_or(0);
                    sum += if (value >> bit) & 1 == 1 { tap } else { -tap };
                }
                *slot = sum;
            }

            self.lut.push(entry);
        }
    }
}

/// Impulse response of `stages` cascaded length-`len` boxcar filters.
fn boxcar_power(len: usize, stages: usize) -> Vec<i64> {
    let mut taps = vec![1i64; len];

    for _ in 1..stages {
        // Convolve with a boxcar of length `len` using a running sum.
        let mut next = Vec::with_capacity(taps.len() + len - 1);
        let mut sum = 0i64;
        for i in 0..taps.len() + len - 1 {
            if let Some(&t) = taps.get(i) {
                sum += t;
            }
            if i >= len {
                sum -= taps[i - len];
            }
            next.push(sum);
        }
        taps = next;
    }

    taps
}

impl CicFilter {
    /// Create a new CIC filter
    ///
    /// # Arguments
    /// * `decimation` - Decimation ratio (must be > 0)
    /// * `stages` - Number of stages (typically 3-5)
    pub fn new(decimation: usize, stages: usize) -> Self {
        assert!(decimation > 0, "Decimation must be > 0");
        assert!(stages > 0, "Stages must be > 0");
        assert!(stages <= MAX_STAGES, "Too many stages");

        let poly = (decimation % 4 == 0).then(|| Polyphase::new(decimation, stages));

        CicFilter {
            decimation,
            stages,
            gain: (decimation as f64).powi(stages as i32),
            poly,
            integrators: [0; MAX_STAGES],
            combs: [0; MAX_STAGES],
            sample_count: 0,
        }
    }

    /// Convert the exact integer filter output to a sample.
    #[inline(always)]
    fn scale(&self, value: i64) -> f32 {
        (value as f64 / self.gain) as f32
    }

    /// Process packed DSD bytes, producing decimated samples.
    ///
    /// Each input byte holds 8 one-bit samples (`1` = +1, `0` = -1) in `bit_order`. The bits are
    /// consumed in time order, and the filter state is carried across calls, so the stream may be
    /// split into arbitrary chunks of bytes.
    ///
    /// # Arguments
    /// * `input` - Packed DSD bytes
    /// * `bit_order` - Order in which the bits of a byte occur in time
    /// * `output` - Output buffer, at least [`output_size_for_input`](Self::output_size_for_input)
    ///   samples long.
    ///
    /// # Returns
    /// Number of output samples produced
    pub fn process_bytes(
        &mut self,
        input: &[u8],
        bit_order: BitOrder,
        output: &mut [f32],
    ) -> usize {
        assert!(output.len() >= self.output_size_for_input(input.len()), "Output buffer too small");

        // Table-driven path.
        if let Some(poly) = self.poly.as_mut() {
            // Prepend the retained history and expand the bytes to nibbles in time order.
            poly.scratch.clear();
            poly.scratch.extend_from_slice(&poly.history);
            let base = poly.scratch.len();

            match bit_order {
                BitOrder::LsbFirst => {
                    for &byte in input {
                        poly.scratch.push(byte & 0xf);
                        poly.scratch.push(byte >> 4);
                    }
                }
                BitOrder::MsbFirst => {
                    // The earliest sample is the most significant bit: reverse the nibbles.
                    for &byte in input {
                        poly.scratch.push(REVERSE_NIBBLE[usize::from(byte >> 4)]);
                        poly.scratch.push(REVERSE_NIBBLE[usize::from(byte & 0xf)]);
                    }
                }
            }

            let (window, block) = (poly.window, poly.block);
            let gain = self.gain;
            let mut produced = 0;

            // A window ends after every `block`-th nibble of the stream.
            let mut end = base + (block - poly.phase);

            while end <= poly.scratch.len() {
                // Until the window is full, the missing nibbles precede the start of the stream and
                // contribute nothing.
                let len = end.min(window);
                let nibbles = &poly.scratch[end - len..end];
                let table = &poly.lut[window - len..];

                let sum = nibbles.iter().zip(table).fold(0i64, |acc, (&nibble, entry)| {
                    acc.wrapping_add(entry[usize::from(nibble & 0xf)])
                });

                output[produced] = (sum as f64 / gain) as f32;
                produced += 1;
                end += block;
            }

            // Retain the most recent `window` nibbles.
            poly.phase = (poly.phase + 2 * input.len()) % block;
            let keep_from = poly.scratch.len().saturating_sub(window);
            poly.history.clear();
            poly.history.extend_from_slice(&poly.scratch[keep_from..]);

            return produced;
        }

        // Bit-serial fallback for decimation ratios that are not a multiple of 4.
        let mut produced = 0;

        for &byte in input {
            for i in 0..8 {
                let bit = match bit_order {
                    BitOrder::LsbFirst => (byte >> i) & 1,
                    BitOrder::MsbFirst => (byte >> (7 - i)) & 1,
                };

                if let Some(sample) = self.process_bit(bit == 1) {
                    output[produced] = sample;
                    produced += 1;
                }
            }
        }

        produced
    }

    /// Process a single bit with the integrator/comb cascade.
    fn process_bit(&mut self, bit: bool) -> Option<f32> {
        let mut value = if bit { 1i64 } else { -1i64 };

        // Integrator stages
        for integrator in &mut self.integrators[..self.stages] {
            value = value.wrapping_add(*integrator);
            *integrator = value;
        }

        // Decimation: only output every Rth sample
        self.sample_count += 1;
        if self.sample_count < self.decimation {
            return None;
        }
        self.sample_count = 0;

        // Comb stages (differentiate)
        for comb_state in &mut self.combs[..self.stages] {
            let prev = *comb_state;
            *comb_state = value;
            value = value.wrapping_sub(prev);
        }

        Some(self.scale(value))
    }

    /// Reset filter state
    pub fn reset(&mut self) {
        self.integrators = [0; MAX_STAGES];
        self.combs = [0; MAX_STAGES];
        self.sample_count = 0;

        if let Some(poly) = self.poly.as_mut() {
            poly.history.clear();
            poly.phase = 0;
        }
    }

    /// Get the number of output samples produced by processing `input_len` bytes of input
    pub fn output_size_for_input(&self, input_len: usize) -> usize {
        // Account for partial decimation from previous calls. Everything is counted in bits for
        // the bit-serial path, and in nibbles for the table-driven one.
        match &self.poly {
            Some(poly) => (poly.phase + input_len * 2) / poly.block,
            None => (self.sample_count + input_len * 8) / self.decimation,
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// The classic integrator/comb cascade operating on `f32` samples with a scale of 2^15, as it
    /// was implemented before the table-driven filter. Used as a bit-exactness oracle.
    pub(crate) struct ReferenceCic {
        decimation: usize,
        stages: usize,
        integrators: Vec<i64>,
        combs: Vec<i64>,
        sample_count: usize,
    }

    impl ReferenceCic {
        pub(crate) fn new(decimation: usize, stages: usize) -> Self {
            ReferenceCic {
                decimation,
                stages,
                integrators: vec![0; stages],
                combs: vec![0; stages],
                sample_count: 0,
            }
        }

        pub(crate) fn process(&mut self, input: f32) -> Option<f32> {
            let mut value = (input * 32768.0) as i64;

            for integrator in &mut self.integrators {
                value = value.wrapping_add(*integrator);
                *integrator = value;
            }

            self.sample_count += 1;
            if self.sample_count < self.decimation {
                return None;
            }
            self.sample_count = 0;

            for comb_state in &mut self.combs {
                let prev = *comb_state;
                *comb_state = value;
                value = value.wrapping_sub(prev);
            }

            let gain = (self.decimation as f64).powi(self.stages as i32);
            Some((value as f64 / gain / 32768.0) as f32)
        }

        pub(crate) fn process_bytes(
            &mut self,
            input: &[u8],
            bit_order: BitOrder,
            output: &mut Vec<f32>,
        ) {
            let mut bits = vec![0.0f32; input.len() * 8];
            crate::bitstream::unpack_dsd_bytes_to_f32(input, bit_order, &mut bits);
            output.extend(bits.into_iter().filter_map(|b| self.process(b)));
        }
    }

    /// Deterministic xorshift generator.
    pub(crate) struct Rng(pub(crate) u64);

    impl Rng {
        pub(crate) fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{ReferenceCic, Rng};
    use super::*;

    /// Run the table-driven and reference filters over the same random data, split into random
    /// chunks, and require bit-identical output.
    fn check_exact(decimation: usize, stages: usize, bit_order: BitOrder, seed: u64) {
        let mut rng = Rng(seed);
        let mut fast = CicFilter::new(decimation, stages);
        let mut reference = ReferenceCic::new(decimation, stages);

        let total = 4096 + (rng.next() % 512) as usize;
        let data: Vec<u8> = (0..total)
            .map(|i| match (i / 700) % 4 {
                // Mix of noise, DC, the silence pattern and sparse bits.
                0 => rng.next() as u8,
                1 => 0xff,
                2 => 0x69,
                _ => (rng.next() & rng.next() & rng.next()) as u8,
            })
            .collect();

        let mut pos = 0;
        let mut got = Vec::new();
        let mut expected = Vec::new();

        while pos < data.len() {
            let len = 1 + (rng.next() as usize % 300).min(data.len() - pos - 1);
            let chunk = &data[pos..pos + len];
            pos += len;

            let mut out = vec![f32::NAN; fast.output_size_for_input(len)];
            let n = fast.process_bytes(chunk, bit_order, &mut out);
            assert_eq!(n, out.len());
            got.extend_from_slice(&out);

            reference.process_bytes(chunk, bit_order, &mut expected);
        }

        assert_eq!(got.len(), expected.len(), "R={decimation} N={stages}");
        for (i, (a, b)) in got.iter().zip(&expected).enumerate() {
            assert_eq!(
                a.to_bits(),
                b.to_bits(),
                "R={decimation} N={stages} {bit_order:?} output {i}: {a} != {b}"
            );
        }
    }

    #[test]
    fn test_polyphase_bit_exact_with_integrator_comb() {
        for &decimation in &[4, 8, 12, 16, 32, 64, 128, 256] {
            for &stages in &[1, 3, 4, 5] {
                for &order in &[BitOrder::LsbFirst, BitOrder::MsbFirst] {
                    check_exact(
                        decimation,
                        stages,
                        order,
                        0x9e3779b97f4a7c15 ^ (decimation * 31 + stages) as u64,
                    );
                }
            }
        }
    }

    #[test]
    fn test_serial_fallback_bit_exact_with_integrator_comb() {
        for &decimation in &[1, 2, 3, 5, 6, 7, 9, 10, 14, 63] {
            for &stages in &[1, 4] {
                for &order in &[BitOrder::LsbFirst, BitOrder::MsbFirst] {
                    check_exact(
                        decimation,
                        stages,
                        order,
                        0x1234567 ^ (decimation * 17 + stages) as u64,
                    );
                }
            }
        }
    }

    #[test]
    fn test_bit_order_change_between_calls() {
        // The state is kept in time order, so the order of a later call may differ.
        let mut a = CicFilter::new(16, 4);
        let mut b = CicFilter::new(16, 4);
        let first = [0x12u8, 0x34, 0x56, 0x78, 0x9a];
        let second = [0xdeu8, 0xf0, 0x0f, 0xa5, 0x5a, 0xc3];
        let mut flipped = second;
        for byte in &mut flipped {
            *byte = byte.reverse_bits();
        }

        let mut out_a = vec![0.0; a.output_size_for_input(11)];
        let mut out_b = out_a.clone();
        let mut n = a.process_bytes(&first, BitOrder::LsbFirst, &mut out_a);
        n += a.process_bytes(&second, BitOrder::LsbFirst, &mut out_a[n..]);
        let mut m = b.process_bytes(&first, BitOrder::LsbFirst, &mut out_b);
        m += b.process_bytes(&flipped, BitOrder::MsbFirst, &mut out_b[m..]);
        assert_eq!(n, m);
        assert_eq!(out_a[..n], out_b[..m]);
    }

    #[test]
    fn test_impulse_response_gain() {
        for &(r, n) in &[(4usize, 4usize), (8, 3), (16, 4), (128, 4)] {
            let taps = boxcar_power(r, n);
            assert_eq!(taps.len(), n * (r - 1) + 1);
            assert_eq!(taps.iter().sum::<i64>(), (r as i64).pow(n as u32));
            // Symmetric.
            assert!(taps.iter().zip(taps.iter().rev()).all(|(a, b)| a == b));
        }
    }

    #[test]
    fn test_cic_filter_creation() {
        let filter = CicFilter::new(8, 3);
        assert_eq!(filter.decimation, 8);
        assert_eq!(filter.stages, 3);
    }

    #[test]
    #[should_panic(expected = "Decimation must be > 0")]
    fn test_cic_filter_zero_decimation() {
        CicFilter::new(0, 3);
    }

    #[test]
    #[should_panic(expected = "Stages must be > 0")]
    fn test_cic_filter_zero_stages() {
        CicFilter::new(8, 0);
    }

    #[test]
    #[should_panic(expected = "Output buffer too small")]
    fn test_cic_filter_output_too_small() {
        CicFilter::new(8, 3).process_bytes(&[0; 4], BitOrder::LsbFirst, &mut [0.0; 3]);
    }

    #[test]
    fn test_cic_filter_decimation() {
        let mut out = [0.0f32; 4];

        // One byte completes a block of 8 samples.
        let mut filter = CicFilter::new(8, 3);
        assert_eq!(filter.process_bytes(&[0xff], BitOrder::LsbFirst, &mut out), 1);

        // A block of 12 samples takes a byte and a half.
        let mut filter = CicFilter::new(12, 3);
        assert_eq!(filter.process_bytes(&[0xff], BitOrder::LsbFirst, &mut out), 0);
        assert_eq!(filter.process_bytes(&[0xff], BitOrder::LsbFirst, &mut out), 1);
        assert_eq!(filter.process_bytes(&[0xff], BitOrder::LsbFirst, &mut out), 1);

        // Same for the bit-serial path.
        let mut filter = CicFilter::new(12 + 1, 3);
        assert_eq!(filter.process_bytes(&[0xff], BitOrder::LsbFirst, &mut out), 0);
        assert_eq!(filter.process_bytes(&[0xff], BitOrder::LsbFirst, &mut out), 1);
    }

    #[test]
    fn test_cic_filter_dc_response() {
        let mut filter = CicFilter::new(8, 3);

        // Feed DC signal (all ones = +1.0)
        let input = vec![0xffu8; 80];
        let mut output = vec![0.0f32; 80];

        let out_count = filter.process_bytes(&input, BitOrder::LsbFirst, &mut output);
        assert_eq!(out_count, 80);

        // Once the window is full the output is exactly the input level.
        for &sample in &output[3..out_count] {
            assert_eq!(sample, 1.0);
        }
    }

    #[test]
    fn test_cic_filter_reset() {
        let mut filter = CicFilter::new(8, 3);
        let mut out = [0.0f32; 8];

        let first = filter.process_bytes(&[0x12, 0x34, 0x56], BitOrder::LsbFirst, &mut out);
        filter.reset();
        let second =
            filter.process_bytes(&[0x12, 0x34, 0x56], BitOrder::LsbFirst, &mut out[first..]);
        assert_eq!(out[..first], out[first..first + second]);

        // The bit-serial state is cleared as well.
        let mut serial = CicFilter::new(6, 3);
        serial.process_bytes(&[0x12, 0x34, 0x56], BitOrder::LsbFirst, &mut [0.0; 8]);
        serial.reset();
        assert_eq!(serial.sample_count, 0);
        assert!(serial.integrators.iter().all(|&v| v == 0));
        assert!(serial.combs.iter().all(|&v| v == 0));
    }

    #[test]
    fn test_output_size_calculation() {
        let filter = CicFilter::new(8, 3);
        assert_eq!(filter.output_size_for_input(10), 10);
        assert_eq!(filter.output_size_for_input(2), 2);
        assert_eq!(filter.output_size_for_input(0), 0);

        let filter = CicFilter::new(32, 4);
        assert_eq!(filter.output_size_for_input(8), 2);
        assert_eq!(filter.output_size_for_input(3), 0);

        let filter = CicFilter::new(12, 4);
        assert_eq!(filter.output_size_for_input(3), 2);
        assert_eq!(filter.output_size_for_input(1), 0);
    }

    #[test]
    fn test_cic_filter_alternating_signal() {
        let mut filter = CicFilter::new(8, 3);

        // The DSD silence pattern (alternating +1/-1).
        let input = vec![0x55u8; 20];
        let mut output = vec![0.0f32; 20];
        let out_count = filter.process_bytes(&input, BitOrder::LsbFirst, &mut output);
        assert_eq!(out_count, 20);

        // Alternating signal at Nyquist is completely removed by the CIC zeros.
        for &sample in &output[5..] {
            assert!(sample.abs() < 1e-6, "High frequency not attenuated: {}", sample);
        }
    }
}
