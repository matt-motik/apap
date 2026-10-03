//! Запись сэмплов в байты выходного формата и квантование (§6.14, ТЗ-4…ТЗ-7, ТЗ-17).
//!
//! Целые форматы пишут младшие `BYTES` байт значения `N`-битного сэмпла в `i32`
//! (little-endian): для `S24_LE` это 4 байта значения `>> 8`, старший байт —
//! знаковое расширение (ОВ-32). Индексации нет: только итераторы по `dst` (ТЗ-100).

/// Формат выходного сэмпла колбэка. `dst` в методах — ровно `BYTES` байт.
pub trait OutFormat: Send + 'static {
    /// Байт на сэмпл.
    const BYTES: usize;
    /// Значащих бит целого формата `N`; `0` для `F32`.
    const BITS: u32;
    /// Целый формат: квантование и TPDF применимы.
    const IS_INT: bool = Self::BITS != 0;

    /// Ветвь `Unity` для `ExactI32` при `vb ≤ N` (§6.14): сдвиг `>> (32 − N)`;
    /// для `F32` — `f64(s) · 2⁻³¹`.
    fn write_exact(dst: &mut [u8], s: i32);

    /// Значение в единицах full scale `[-1, 1)` с шумом `d` в LSB (0 — без дизеринга).
    /// Целые форматы квантуют в `N` бит; `F32` пишет `x` и игнорирует `d`.
    fn write_float(dst: &mut [u8], x: f64, d: f64);

    /// Тишина формата.
    fn write_silence(dst: &mut [u8]) {
        dst.fill(0);
    }
}

/// 2⁻³¹: перевод выровненного влево `ExactI32` в full scale.
pub const EXACT_TO_UNIT: f64 = 1.0 / 2_147_483_648.0;

/// Квантование в `bits` бит (ТЗ-7, ТЗ-17): `q = (x · 2^(N−1) + d).round()`,
/// `clamp(−2^(N−1), 2^(N−1) − 1)` в `f64`, затем приведение к `i32`, точное после `clamp`.
/// `bits` — от 1 до 32.
#[inline]
pub fn quantize(x: f64, d: f64, bits: u32) -> i32 {
    let scale = f64::from(1u32 << bits.clamp(1, 32).saturating_sub(1));
    let q = (x * scale + d).round();
    // NaN → 0: `clamp` пропускает NaN, а `as` даёт 0 — явно, без зависимости от этого.
    if q.is_nan() {
        return 0;
    }
    q.clamp(-scale, scale - 1.0) as i32
}

/// Младшие байты `v` в `dst` (little-endian).
#[inline]
fn put_low_bytes(dst: &mut [u8], v: i32) {
    for (d, b) in dst.iter_mut().zip(v.to_le_bytes()) {
        *d = b;
    }
}

macro_rules! int_format {
    ($(#[$m:meta])* $name:ident, $bytes:expr, $bits:expr) => {
        $(#[$m])*
        pub struct $name;

        impl OutFormat for $name {
            const BYTES: usize = $bytes;
            const BITS: u32 = $bits;

            #[inline]
            fn write_exact(dst: &mut [u8], s: i32) {
                put_low_bytes(dst, s >> (32 - $bits));
            }

            #[inline]
            fn write_float(dst: &mut [u8], x: f64, d: f64) {
                put_low_bytes(dst, quantize(x, d, $bits));
            }
        }
    };
}

int_format!(
    /// `S16_LE`: 2 байта.
    S16Le, 2, 16
);
int_format!(
    /// `S24_3LE`: младшие 3 байта значения `>> 8`.
    S24_3Le, 3, 24
);
int_format!(
    /// `S24_LE`: 4 байта значения `>> 8`, старший байт — знак (ОВ-32).
    S24Le, 4, 24
);
int_format!(
    /// `S32_LE`: 4 байта.
    S32Le, 4, 32
);

/// `F32_LE`: `to_le_bytes`.
pub struct F32Le;

impl OutFormat for F32Le {
    const BYTES: usize = 4;
    const BITS: u32 = 0;

    /// Точно для `vb ≤ 24`; для 32 бит — округление `f32` (Int32ToFloat, §6.14).
    #[inline]
    fn write_exact(dst: &mut [u8], s: i32) {
        let x = (f64::from(s) * EXACT_TO_UNIT) as f32;
        for (d, b) in dst.iter_mut().zip(x.to_le_bytes()) {
            *d = b;
        }
    }

    #[inline]
    fn write_float(dst: &mut [u8], x: f64, _d: f64) {
        for (d, b) in dst.iter_mut().zip((x as f32).to_le_bytes()) {
            *d = b;
        }
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    /// Обратное извлечение `N`-битного значения из байт формата.
    fn read_int<O: OutFormat>(b: &[u8]) -> i32 {
        let mut w = [0u8; 4];
        w[..O::BYTES].copy_from_slice(b);
        let raw = i32::from_le_bytes(w);
        // Знаковое расширение 2/3-байтовых форматов.
        let unused = 32 - 8 * O::BYTES as u32;
        (raw << unused) >> unused
    }

    fn exact<O: OutFormat>(s: i32) -> Vec<u8> {
        let mut b = vec![0u8; O::BYTES];
        O::write_exact(&mut b, s);
        b
    }

    fn roundtrip<O: OutFormat>(vals: &[i32], src_bits: u32) {
        let mut mismatches = 0;
        for &v in vals {
            let s = v << (32 - src_bits);
            let back = read_int::<O>(&exact::<O>(s)) << (32 - O::BITS);
            if back != s {
                mismatches += 1;
            }
        }
        assert_eq!(mismatches, 0);
    }

    /// Вектор 24 бит (ТЗ-4): границы, около нуля, все разряды.
    const I24_VECTOR: [i32; 8] = [-8_388_608, 8_388_607, 0, -1, 1, 0x12_3456, -0x12_3456, 0x55_AAAA];
    /// Вектор 32 бит (ТЗ-5): `MIN`, `MAX`, ненулевые младшие 8 бит.
    const I32_VECTOR: [i32; 6] = [i32::MIN, i32::MAX, 0x0000_00FF, -0x0000_0081, 0x1234_5678, -1];

    #[test]
    fn passthrough_i16_roundtrip_is_bit_exact() {
        let all: Vec<i32> = (i16::MIN..=i16::MAX).map(i32::from).collect();
        roundtrip::<S16Le>(&all, 16);
        roundtrip::<S24_3Le>(&all, 16);
        roundtrip::<S24Le>(&all, 16);
        roundtrip::<S32Le>(&all, 16);
    }

    #[test]
    fn passthrough_i24_vector_is_bit_exact() {
        roundtrip::<S24_3Le>(&I24_VECTOR, 24);
        roundtrip::<S24Le>(&I24_VECTOR, 24);
        roundtrip::<S32Le>(&I24_VECTOR, 24);
    }

    #[test]
    fn passthrough_i32_vector_is_bit_exact() {
        roundtrip::<S32Le>(&I32_VECTOR, 32);
    }

    #[test]
    fn zero_pad_layout_per_format() {
        let s = 20000i32 << 16;
        assert_eq!(exact::<S32Le>(s), 0x4E20_0000u32.to_le_bytes());
        assert_eq!(exact::<S24_3Le>(s), [0x00, 0x20, 0x4E]);
        assert_eq!(exact::<S24Le>(s), 0x004E_2000u32.to_le_bytes());
        // 24 бит → S32_LE: младшие 8 бит нулевые.
        for v in I24_VECTOR {
            assert_eq!(exact::<S32Le>(v << 8)[0], 0);
        }
    }

    #[test]
    fn s24le_is_right_aligned_with_sign_extension() {
        for v in I24_VECTOR {
            let b = exact::<S24Le>(v << 8);
            let raw = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            assert_eq!(raw, v, "младшие 24 бита — сэмпл");
            assert_eq!(b[3], if v < 0 { 0xFF } else { 0x00 });
        }
    }

    #[test]
    fn quantize_f32_symmetric_roundtrip() {
        for s in i16::MIN..=i16::MAX {
            let x = f64::from(s) / 32768.0;
            assert_eq!(quantize(x, 0.0, 16), i32::from(s));
            // Через f32, как приходит от SRC/float-источника.
            assert_eq!(quantize(f64::from(x as f32), 0.0, 16), i32::from(s));
        }
        for v in I24_VECTOR {
            assert_eq!(quantize(f64::from(v) / 8_388_608.0, 0.0, 24), v);
        }
        for bits in [16, 24, 32] {
            let max = ((1i64 << (bits - 1)) - 1) as i32;
            let min = (-(1i64 << (bits - 1))) as i32;
            assert_eq!(quantize(-1.0, 0.0, bits), min);
            assert_eq!(quantize(1.0, 0.0, bits), max);
            assert_eq!(quantize(4.0, 0.9, bits), max, "без переполнения при перегрузке");
        }
        assert_eq!(quantize(f64::NAN, 0.0, 16), 0);
    }

    #[test]
    fn f32_out_exact_for_24_bits() {
        for v in I24_VECTOR {
            let b = exact::<F32Le>(v << 8);
            let x = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            assert_eq!(f64::from(x), f64::from(v) / 8_388_608.0);
        }
    }

    #[test]
    fn silence_is_zero_bytes() {
        let mut b = [0xAAu8; 3];
        S24_3Le::write_silence(&mut b);
        assert_eq!(b, [0, 0, 0]);
    }
}
