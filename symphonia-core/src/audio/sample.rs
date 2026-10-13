// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `sample` module defines the audio sample trait and implements any non-primitive sample data
//! types.

use std::fmt;

use crate::util::clamp::{clamp_f32, clamp_f64, clamp_i24, clamp_u24};

/// An enumeration of standard sample formats.
#[derive(Copy, Clone, Debug)]
pub enum SampleFormat {
    /// Unsigned 8-bit integer.
    U8,
    /// Unsigned 16-bit integer.
    U16,
    /// Unsigned 24-bit integer.
    U24,
    /// Unsigned 32-bit integer.
    U32,
    /// Signed 8-bit integer.
    S8,
    /// Signed 16-bit integer.
    S16,
    /// Signed 24-bit integer.
    S24,
    /// Signed 32-bit integer.
    S32,
    /// Single precision (32-bit) floating point.
    F32,
    /// Double precision (64-bit) floating point.
    F64,
}

/// The sample trait defines the mandatory operations and attributes an audio sample data type must
/// implement and provide.
pub trait Sample:
    Copy
    + Clone
    + core::ops::Add<Output = Self>
    + core::ops::Sub<Output = Self>
    + Default
    + PartialOrd
    + PartialEq
    + Sized
{
    /// The effective number of bits of the valid (clamped) sample range. Quantifies the dynamic
    /// range of the sample format in bits.
    const EFF_BITS: u32;

    /// The mid-point value between the maximum and minimum sample value. If a sample is set to this
    /// value it is silent.
    const MID: Self;

    /// If the sample format does not use the full range of the underlying data type, returns the
    /// sample clamped to the valid range. Otherwise, returns the sample unchanged.
    fn clamped(self) -> Self;
}

/// An unsigned 24-bit integer sample with an internal unsigned 32-bit integer representation.
///
/// There are **no** guarantees the sample is within the valid range 24-bit range. Use the
/// [`Sample::clamped`] function to clamp the sample to the valid range.
#[allow(non_camel_case_types)]
#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct u24(pub u32);

/// A signed 24-bit integer sample with an internal signed 32-bit integer representation.
///
/// There are **no** guarantees the sample is within the valid range 24-bit range. Use the
/// [`Sample::clamped`] function to clamp the sample to the valid range.
#[allow(non_camel_case_types)]
#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct i24(pub i32);

impl Sample for u8 {
    const EFF_BITS: u32 = 8;
    const MID: u8 = 128;

    #[inline(always)]
    fn clamped(self) -> Self {
        self
    }
}

impl Sample for i8 {
    const EFF_BITS: u32 = 8;
    const MID: i8 = 0;

    #[inline(always)]
    fn clamped(self) -> Self {
        self
    }
}

impl Sample for u16 {
    const EFF_BITS: u32 = 16;
    const MID: u16 = 32_768;

    #[inline(always)]
    fn clamped(self) -> Self {
        self
    }
}

impl Sample for i16 {
    const EFF_BITS: u32 = 16;
    const MID: i16 = 0;

    #[inline(always)]
    fn clamped(self) -> Self {
        self
    }
}

impl Sample for u24 {
    const EFF_BITS: u32 = 24;
    const MID: u24 = u24(8_388_608);

    #[inline(always)]
    fn clamped(self) -> Self {
        u24(clamp_u24(self.0))
    }
}

impl Sample for i24 {
    const EFF_BITS: u32 = 24;
    const MID: i24 = i24(0);

    #[inline(always)]
    fn clamped(self) -> Self {
        i24(clamp_i24(self.0))
    }
}

impl Sample for u32 {
    const EFF_BITS: u32 = 32;
    const MID: u32 = 2_147_483_648;

    #[inline(always)]
    fn clamped(self) -> Self {
        self
    }
}

impl Sample for i32 {
    const EFF_BITS: u32 = 32;
    const MID: i32 = 0;

    #[inline(always)]
    fn clamped(self) -> Self {
        self
    }
}

impl Sample for f32 {
    const EFF_BITS: u32 = 24;
    const MID: f32 = 0.0;

    #[inline(always)]
    fn clamped(self) -> Self {
        clamp_f32(self)
    }
}

impl Sample for f64 {
    const EFF_BITS: u32 = 53;
    const MID: f64 = 0.0;

    #[inline(always)]
    fn clamped(self) -> Self {
        clamp_f64(self)
    }
}

// Helper macros

macro_rules! shl_impl {
    ($t:ident, $f:ty) => {
        impl core::ops::Shl<$f> for $t {
            type Output = $t;

            #[inline]
            fn shl(self, other: $f) -> $t {
                $t(self.0 << other)
            }
        }
    };
}

macro_rules! shr_impl {
    ($t:ident, $f:ty) => {
        impl core::ops::Shr<$f> for $t {
            type Output = $t;

            #[inline]
            fn shr(self, other: $f) -> $t {
                $t(self.0 >> other)
            }
        }
    };
}

macro_rules! impl_shifts {
    ($t:ident, $f:ty) => {
        shl_impl! { $t, $f }
        shr_impl! { $t, $f }
    };
}

// Implementation for i24 and u24

macro_rules! impl_int24 {
    ($t:ident, $inner:ty, $max:expr, $min:expr, $clamp:ident, [$($narrow:ty),+]) => {
        impl $t {
            /// The largest value that can be represented by this integer type.
            pub const MAX: $t = $t($max);
            /// The smallest value that can be represented by this integer type.
            pub const MIN: $t = $t($min);

            /// Get the underlying integer backing this 24-bit integer.
            #[inline(always)]
            pub fn inner(self) -> $inner {
                self.0
            }

            /// Return the memory representation of this integer as a byte array in little-endian
            /// byte order.
            #[inline(always)]
            pub fn to_le_bytes(self) -> [u8; 3] {
                let b = self.0.to_le_bytes();

                // In little-endian the MSB is the last byte. Drop it.
                [b[0], b[1], b[2]]
            }

            /// Return the memory representation of this integer as a byte array in big-endian
            /// byte order.
            #[inline(always)]
            pub fn to_be_bytes(self) -> [u8; 3] {
                let b = self.0.to_be_bytes();

                // In big-endian the MSB is the first byte. Drop it.
                [b[1], b[2], b[3]]
            }

            /// Return the memory representation of this integer as a byte array in native byte
            /// order.
            #[inline(always)]
            pub fn to_ne_bytes(self) -> [u8; 3] {
                let b = self.0.to_ne_bytes();

                if cfg!(target_endian = "little") {
                    // In little-endian the MSB is the last byte. Drop it.
                    [b[0], b[1], b[2]]
                }
                else {
                    // In big-endian the MSB is the first byte. Drop it.
                    [b[1], b[2], b[3]]
                }
            }
        }

        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl From<$inner> for $t {
            fn from(val: $inner) -> Self {
                $t($clamp(val))
            }
        }

        $(
            impl From<$narrow> for $t {
                fn from(val: $narrow) -> Self {
                    $t(<$inner>::from(val))
                }
            }
        )+

        impl_binop_24! { $t, Add, add, + }
        impl_binop_24! { $t, Sub, sub, - }
        impl_binop_24! { $t, Mul, mul, * }
        impl_binop_24! { $t, Div, div, / }
        impl_binop_24! { $t, Rem, rem, % }
        impl_binop_24! { $t, Shl, shl, << }
        impl_binop_24! { $t, Shr, shr, >> }
        impl_binop_24! { $t, BitAnd, bitand, & }
        impl_binop_24! { $t, BitOr, bitor, | }
        impl_binop_24! { $t, BitXor, bitxor, ^ }

        impl core::ops::Not for $t {
            type Output = $t;

            #[inline]
            fn not(self) -> Self {
                $t(!self.0)
            }
        }

        impl_shifts! { $t, u8 }
        impl_shifts! { $t, u16 }
        impl_shifts! { $t, u32 }
        impl_shifts! { $t, u64 }
        impl_shifts! { $t, u128 }
        impl_shifts! { $t, usize }

        impl_shifts! { $t, i8 }
        impl_shifts! { $t, i16 }
        impl_shifts! { $t, i32 }
        impl_shifts! { $t, i64 }
        impl_shifts! { $t, i128 }
        impl_shifts! { $t, isize }
    };
}

macro_rules! impl_binop_24 {
    ($t:ident, $trait:ident, $method:ident, $op:tt) => {
        impl core::ops::$trait<$t> for $t {
            type Output = $t;

            #[inline]
            fn $method(self, other: Self) -> Self {
                $t(self.0 $op other.0)
            }
        }
    };
}

impl_int24! { i24, i32, 8_388_607, -8_388_608, clamp_i24, [i16, i8] }
impl_int24! { u24, u32, 16_777_215, 0, clamp_u24, [u16, u8] }

mod sealed {
    /// A marker trait for indicating 1-byte alignment. This trait is sealed to prevent external
    /// misuse.
    pub trait ByteAligned: bytemuck::Pod {}
}

impl sealed::ByteAligned for [u8; 1] {}
impl sealed::ByteAligned for [u8; 2] {}
impl sealed::ByteAligned for [u8; 3] {}
impl sealed::ByteAligned for [u8; 4] {}
impl sealed::ByteAligned for [u8; 5] {}
impl sealed::ByteAligned for [u8; 6] {}
impl sealed::ByteAligned for [u8; 7] {}
impl sealed::ByteAligned for [u8; 8] {}
impl sealed::ByteAligned for [u8; 9] {}
impl sealed::ByteAligned for [u8; 10] {}
impl sealed::ByteAligned for [u8; 11] {}
impl sealed::ByteAligned for [u8; 12] {}
impl sealed::ByteAligned for [u8; 13] {}
impl sealed::ByteAligned for [u8; 14] {}
impl sealed::ByteAligned for [u8; 15] {}
impl sealed::ByteAligned for [u8; 16] {}
// Note: DO NOT implement for any types other than [u8; N].

/// `SampleBytes` provides interfaces to get the packed byte representation of a `Sample`.
///
/// The packed byte representation is the byte representation of a sample in its native stream
/// format. This may differ from its in-memory working representation.
pub trait SampleBytes: Sample {
    /// The data type that stores the packed byte representation of the sample.
    ///
    /// The `sealed::ByteAligned` trait bound constrains the allowable types this may be. Only byte
    /// array types: `[u8; N]` for `N` between 1 and 16 inclusive are allowed.
    type RawType: sealed::ByteAligned;

    /// Return the sample in its packed byte representation in the native byte order.
    fn to_ne_sample_bytes(self) -> Self::RawType;

    /// Return the sample in its packed byte representation in the little-endian byte order.
    fn to_le_sample_bytes(self) -> Self::RawType;

    /// Return the sample in its packed byte representation in the big-endian byte order.
    fn to_be_sample_bytes(self) -> Self::RawType;
}

macro_rules! impl_sample_bytes {
    ($t:ty, $n:literal) => {
        impl SampleBytes for $t {
            type RawType = [u8; $n];

            #[inline(always)]
            fn to_ne_sample_bytes(self) -> Self::RawType {
                self.to_ne_bytes()
            }

            #[inline(always)]
            fn to_le_sample_bytes(self) -> Self::RawType {
                self.to_le_bytes()
            }

            #[inline(always)]
            fn to_be_sample_bytes(self) -> Self::RawType {
                self.to_be_bytes()
            }
        }
    };
}

impl_sample_bytes! { u8, 1 }
impl_sample_bytes! { u16, 2 }
impl_sample_bytes! { u24, 3 }
impl_sample_bytes! { u32, 4 }
impl_sample_bytes! { i8, 1 }
impl_sample_bytes! { i16, 2 }
impl_sample_bytes! { i24, 3 }
impl_sample_bytes! { i32, 4 }
impl_sample_bytes! { f32, 4 }
impl_sample_bytes! { f64, 8 }
