//! F16-FMA-CONTEXT: x86-64-v3 has FMA but no AVX512-FP16. Relaxed scalar
//! half FMA must use the existing f64 expansion, preserving one final half rounding.
//! Run with BLAZE_FMA_RELAXED=1 through blaze test.
#![feature(f16, core_intrinsics)]
#![allow(internal_features)]

#[cfg(not(all(
    target_arch = "x86_64",
    target_feature = "fma",
    not(target_feature = "avx512fp16")
)))]
compile_error!("this regression requires x86-64 with FMA and without AVX512-FP16");

#[inline(never)]
pub fn public_fma(a: u16, b: u16, c: u16) -> u16 {
    f16::from_bits(a).mul_add(f16::from_bits(b), f16::from_bits(c)).to_bits()
}

#[inline(never)]
pub fn intrinsic_fma(a: u16, b: u16, c: u16) -> u16 {
    core::intrinsics::fma(f16::from_bits(a), f16::from_bits(b), f16::from_bits(c)).to_bits()
}

#[inline]
pub fn negated_fma(a: f16, b: f16, c: f16) -> f16 {
    core::intrinsics::fma(-a, b, -c)
}

#[test]
fn fused_half_rounding_and_negations() {
    for (a, b, c, expected) in [
        // An f32 intermediate double-rounds this result to 0x3000.
        (0x520b, 0x00e9, 0x2ff6, 0x3001),
        (0x3c01, 0x3bfe, 0xbc00, 0x8010),
        (0x7bff, 0x4000, 0xfbff, 0x7bff),
    ] {
        let (a, b, c) = std::hint::black_box((a, b, c));
        assert_eq!(public_fma(a, b, c), expected);
        assert_eq!(intrinsic_fma(a, b, c), expected);
        assert_eq!(
            negated_fma(f16::from_bits(a), f16::from_bits(b), f16::from_bits(c)).to_bits(),
            expected ^ 0x8000,
        );
    }
}

#[test]
fn signed_zero_and_nan_classification() {
    for (a, b, c, expected) in [
        (0x8000, 0x3c00, 0x8000, 0x8000),
        (0x8000, 0x3c00, 0x0000, 0x0000),
        (0x0000, 0x3c00, 0x8000, 0x0000),
    ] {
        let (a, b, c) = std::hint::black_box((a, b, c));
        assert_eq!(public_fma(a, b, c), expected);
        assert_eq!(intrinsic_fma(a, b, c), expected);
    }
    for (a, b, c) in [
        (0x7e01, 0x3c00, 0x0000),
        (0x3c00, 0xfc01, 0x0000),
        (0x3c00, 0x3c00, 0xfe02),
        (0x0000, 0x7c00, 0x3c00),
    ] {
        let (a, b, c) = std::hint::black_box((a, b, c));
        for bits in [public_fma(a, b, c), intrinsic_fma(a, b, c)] {
            assert_eq!(bits & 0x7c00, 0x7c00);
            assert_ne!(bits & 0x03ff, 0);
        }
    }
}
