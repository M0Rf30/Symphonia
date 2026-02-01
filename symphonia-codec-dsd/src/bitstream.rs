// DSD Bitstream Unpacking Utilities
// Converts packed DSD bytes to individual bit values for filtering

use symphonia_core::codecs::BitOrder;

/// Unpack a DSD byte to 8 individual f32 samples for filtering
///
/// DSD uses 1-bit encoding where:
/// - 0 bit = -1.0 (or silence pattern)
/// - 1 bit = +1.0
///
/// # Arguments
/// * `byte` - The DSD byte to unpack
/// * `bit_order` - Bit order (LSB-first for DSF, MSB-first for DFF)
/// * `output` - Output buffer for 8 f32 values
pub fn unpack_dsd_byte_to_f32(byte: u8, bit_order: BitOrder, output: &mut [f32; 8]) {
    match bit_order {
        BitOrder::LsbFirst => {
            // DSF format: LSB first
            for i in 0..8 {
                let bit = (byte >> i) & 1;
                output[i] = if bit == 1 { 1.0 } else { -1.0 };
            }
        }
        BitOrder::MsbFirst => {
            // DFF format: MSB first
            for i in 0..8 {
                let bit = (byte >> (7 - i)) & 1;
                output[i] = if bit == 1 { 1.0 } else { -1.0 };
            }
        }
    }
}

/// Unpack a slice of DSD bytes to f32 samples
///
/// # Arguments
/// * `input` - Input DSD bytes
/// * `bit_order` - Bit order (LSB-first for DSF, MSB-first for DFF)
/// * `output` - Output buffer for f32 values (must be at least input.len() * 8)
pub fn unpack_dsd_bytes_to_f32(input: &[u8], bit_order: BitOrder, output: &mut [f32]) {
    assert!(output.len() >= input.len() * 8, "Output buffer too small");

    for (byte_idx, &byte) in input.iter().enumerate() {
        let out_slice = &mut output[byte_idx * 8..(byte_idx + 1) * 8];
        let mut temp: [f32; 8] = [0.0; 8];
        unpack_dsd_byte_to_f32(byte, bit_order, &mut temp);
        out_slice.copy_from_slice(&temp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unpack_dsd_byte_lsb_first() {
        let byte = 0b10101010; // 0xAA
        let mut output = [0.0f32; 8];
        unpack_dsd_byte_to_f32(byte, BitOrder::LsbFirst, &mut output);

        // LSB first: bit 0 is first
        assert_eq!(output, [-1.0, 1.0, -1.0, 1.0, -1.0, 1.0, -1.0, 1.0]);
    }

    #[test]
    fn test_unpack_dsd_byte_msb_first() {
        let byte = 0b10101010; // 0xAA
        let mut output = [0.0f32; 8];
        unpack_dsd_byte_to_f32(byte, BitOrder::MsbFirst, &mut output);

        // MSB first: bit 7 is first
        assert_eq!(output, [1.0, -1.0, 1.0, -1.0, 1.0, -1.0, 1.0, -1.0]);
    }

    #[test]
    fn test_unpack_dsd_silence_pattern() {
        // DSD silence is 0x55 = 0b01010101
        let byte = 0x55;
        let mut output = [0.0f32; 8];
        unpack_dsd_byte_to_f32(byte, BitOrder::LsbFirst, &mut output);

        // Should alternate -1, 1
        assert_eq!(output, [1.0, -1.0, 1.0, -1.0, 1.0, -1.0, 1.0, -1.0]);
    }

    #[test]
    fn test_unpack_dsd_bytes() {
        let input = [0xFF, 0x00, 0xAA];
        let mut output = vec![0.0f32; 24];
        unpack_dsd_bytes_to_f32(&input, BitOrder::LsbFirst, &mut output);

        // 0xFF = all 1s -> all 1.0
        for i in 0..8 {
            assert_eq!(output[i], 1.0);
        }

        // 0x00 = all 0s -> all -1.0
        for i in 8..16 {
            assert_eq!(output[i], -1.0);
        }

        // 0xAA = alternating
        assert_eq!(&output[16..24], &[-1.0, 1.0, -1.0, 1.0, -1.0, 1.0, -1.0, 1.0]);
    }

    #[test]
    #[should_panic(expected = "Output buffer too small")]
    fn test_unpack_dsd_bytes_buffer_too_small() {
        let input = [0xFF, 0x00];
        let mut output = vec![0.0f32; 8]; // Should be at least 16
        unpack_dsd_bytes_to_f32(&input, BitOrder::LsbFirst, &mut output);
    }
}
